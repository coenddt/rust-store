//! `aggregate` 命令翻译：$match / $lookup(→LEFT JOIN) / $sort / $skip / $limit / $project

use serde_json::{json, Value};

use crate::schema::{Registry, Schema};

use crate::dialect::filter::{build_filter, WhereClause};
use crate::dialect::ir::{RowCol, RowShape, SqlStmt};
use crate::dialect::Backend;

use super::lookup_join::{resolve_join, Join};
use super::{col_fn, projection_fields, q, tname};

/// `$lookup` 子 pipeline 是否含每父 top-N（子 `$limit` / `$skip`）——
/// 需窗口函数 / LATERAL 才能下推，本里程碑未实现，须标记 `_unsupported`。
fn has_child_limit(pipeline: &[Value]) -> bool {
    pipeline
        .iter()
        .any(|s| s.get("$limit").is_some() || s.get("$skip").is_some())
}

/// aggregate：$match / $lookup(→LEFT JOIN) / $sort / $skip / $limit / $project
pub(super) fn translate_aggregate(
    backend: Backend,
    schema: &Schema,
    pipeline: &[Value],
    registry: &Registry,
    warnings: &mut Vec<String>,
    unsupported: &mut Vec<Value>,
) -> Result<Vec<SqlStmt>, String> {
    let mut param_seq = 0usize;
    let mut root_wheres: Vec<WhereClause> = Vec::new();
    let mut root_order: Vec<String> = Vec::new();
    let mut root_limit: Option<i64> = None;
    let mut root_offset: i64 = 0;
    let mut joins: Vec<Join> = Vec::new();
    let mut project_on: Option<Vec<String>> = None;

    for stage in pipeline {
        if let Some(m) = stage.get("$match") {
            let wh = build_filter(m, backend, "t", &col_fn(schema), &mut param_seq);
            if !wh.text.is_empty() {
                root_wheres.push(wh);
            }
        } else if let Some(lo) = stage.get("$lookup") {
            let alias = lo.get("as").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let from = lo.get("from").and_then(|v| v.as_str()).unwrap_or("").to_string();
            // 每父 top-N（子 $sort/$skip/$limit）需窗口函数 / LATERAL 才能下推。
            // 若只发 LEFT JOIN 会静默返回\"未截断\"的子集 —— 违反「绝不静默产生错误结果」，
            // 故不下推该 JOIN，改为标记 `_unsupported:"childLimit"`，由 Host 决定兜底策略。
            if lo
                .get("pipeline")
                .and_then(|v| v.as_array())
                .map(|p| has_child_limit(p))
                .unwrap_or(false)
            {
                warnings.push(format!(
                    "$lookup 关系 {} 含子 limit/skip（每父 top-N），需窗口函数或 LATERAL，暂未下推（_unsupported:childLimit）",
                    alias
                ));
                unsupported.push(json!({
                    "code": "childLimit",
                    "as": alias,
                    "reason": "$lookup 子 pipeline 的 $limit/$skip 需窗口函数或 LATERAL，暂未下推",
                }));
                continue;
            }
            match resolve_join(schema, registry, &alias, &from) {
                Some(j) => joins.push(j),
                None => warnings.push(format!("$lookup 关系 {} 无法匹配 schema，跳过 JOIN", alias)),
            }
        } else if let Some(s) = stage.get("$sort") {
            if let Some(o) = s.as_object() {
                for (k, dir) in o {
                    if let Some(c) = col_fn(schema)(k) {
                        let d = dir.as_i64().unwrap_or(1);
                        root_order.push(format!("t.{} {}", q(backend, &c), if d >= 0 { "ASC" } else { "DESC" }));
                    }
                }
            }
        } else if let Some(l) = stage.get("$skip") {
            root_offset = l.as_i64().unwrap_or(0);
        } else if let Some(l) = stage.get("$limit") {
            root_limit = l.as_i64();
        } else if let Some(p) = stage.get("$project") {
            project_on = Some(projection_fields(schema, Some(p)));
        } else if stage.get("$lookup").is_some() {
            // 理论不可达：$lookup 已在上方分支处理（含 childLimit 标记）。防御性收口——
            // 本模块契约是「绝不 panic / 绝不生成错误 SQL」，收拢到 Err 而非 unreachable!()
            return Err("SELECT 翻译不支持 $lookup 阶段（应经 JOIN 下推）".to_string());
        }
        // $unwind / $addFields / $count … 忽略或告警
    }

    // ── 拼装 SELECT ──
    let selected = project_on.unwrap_or_else(|| schema.fields.keys().cloned().collect());
    let mut cols_sql = Vec::new();
    let mut columns: Vec<RowCol> = Vec::new();

    // 根表恒选 `_id`（标量）—— 作为平铺行还原时的文档分组键，保证 $lookup 聚合可分组
    cols_sql.push(format!("t.{}", q(backend, "_id")));
    columns.push(RowCol::scalar("_id", &["_id"]));

    // 根表选列
    let root_selected: Vec<String> = selected
        .iter()
        .filter(|f| schema.relations.iter().all(|(rn, _)| rn != *f))
        .cloned()
        .collect();
    for f in &root_selected {
        if let Some(c) = col_fn(schema)(f) {
            cols_sql.push(format!("t.{}", q(backend, &c)));
            columns.push(RowCol::scalar(&c, &[f.as_str()]));
        }
    }
    if cols_sql.is_empty() && !selected.is_empty() {
        cols_sql.push(format!("t.{}", q(backend, "_id")));
        columns.push(RowCol::scalar("_id", &["_id"]));
    }

    // 每个 JOIN：LEFT JOIN，并展开关系表的标量字段（含关系自身的 _id 便于聚合）
    let mut from_sql = format!("{} t", tname(backend, schema));
    let mut all_params: Vec<Value> = Vec::new();
    for (i, j) in joins.iter().enumerate() {
        let r = format!("r{}", i);
        // 关系目标 schema：namespace 限定与字段展开都依赖它；定位失败 = 关系悬空 → 跳过并告警
        let rel_schema = match registry.get(&j.model) {
            Ok(s) => s,
            Err(e) => {
                warnings.push(format!("$lookup 关系 {} 目标不可定位，跳过 JOIN: {}", j.alias, e));
                continue;
            }
        };
        from_sql.push_str(&format!(
            " LEFT JOIN {} {} ON {}.{} = t.{}",
            tname(backend, rel_schema),
            r,
            r,
            q(backend, &j.foreign_col),
            q(backend, &j.local_col),
        ));
        let rel_cols = projection_fields(rel_schema, None);
        for rf in rel_cols {
            if rel_schema.fields.get(&rf).map(|f| f.field_type == "object" || f.field_type == "array").unwrap_or(false) {
                continue;
            }
            let alias_col = format!("{}_{}_{}", j.alias, i, rf);
            cols_sql.push(format!("{}.{} AS {}", r, q(backend, &rf), q(backend, &alias_col)));
            // 聚合数组：路径 rel_name.rf
            columns.push(RowCol {
                alias: alias_col,
                json_path: vec![j.rel_name.clone(), rf.clone()],
                is_array: true,
                sub_shape: None,
            });
        }
    }

    let where_sql = if root_wheres.is_empty() { String::new() } else {
        let text = root_wheres.iter().map(|w| w.text.clone()).collect::<Vec<_>>().join(" AND ");
        for w in &root_wheres {
            all_params.extend(w.params.clone());
        }
        format!(" WHERE {}", text)
    };

    let order_sql = if root_order.is_empty() { String::new() } else { format!(" ORDER BY {}", root_order.join(", ")) };

    // LIMIT/OFFSET 需参数
    let mut limit_params: Vec<Value> = Vec::new();
    let mut limit_sql = String::new();
    if let Some(lim) = root_limit {
        match backend {
            Backend::Postgres => {
                limit_sql = format!(" LIMIT ${}", param_seq + 1);
                param_seq += 1;
                limit_params.push(json!(lim));
                if root_offset > 0 {
                    limit_sql.push_str(&format!(" OFFSET ${}", param_seq + 1));
                    limit_params.push(json!(root_offset));
                }
            }
            _ => {
                if root_offset > 0 {
                    limit_sql = " LIMIT ? OFFSET ?".to_string();
                    limit_params.push(json!(lim));
                    limit_params.push(json!(root_offset));
                } else {
                    limit_sql = " LIMIT ?".to_string();
                    limit_params.push(json!(lim));
                }
            }
        }
    } else if root_offset > 0 {
        // 仅 offset
        match backend {
            Backend::Postgres => {
                limit_sql = format!(" LIMIT -1 OFFSET ${}", param_seq + 1);
                limit_params.push(json!(root_offset));
            }
            _ => {
                limit_sql = " LIMIT -1 OFFSET ?".to_string();
                limit_params.push(json!(root_offset));
            }
        }
    }
    all_params.extend(limit_params);

    let select_list = if cols_sql.is_empty() { q(backend, "_id") } else { cols_sql.join(", ") };
    let text = format!(
        "SELECT {} FROM {}{}{}{}",
        select_list,
        from_sql,
        where_sql,
        order_sql,
        limit_sql,
    );
    Ok(vec![SqlStmt::select(text, all_params, RowShape { columns })])
}
