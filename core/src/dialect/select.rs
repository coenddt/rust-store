//! SELECT 翻译：find / countDocuments / aggregate（含 $lookup → JOIN）

use serde_json::{json, Value};

use crate::schema::{Registry, Schema};

use super::filter::{build_filter, WhereClause};
use super::ir::{RowCol, RowShape, SqlStmt};
use super::Backend;

/// 翻译 find / countDocuments / aggregate 命令
pub fn translate_select(
    backend: Backend,
    cmd: &Value,
    registry: &Registry,
    warnings: &mut Vec<String>,
) -> Result<Vec<SqlStmt>, String> {
    let kind = cmd.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let collection = cmd.get("collection").and_then(|v| v.as_str()).unwrap_or("");
    let schema = registry.get_by_collection(collection)?;

    match kind {
        "countDocuments" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let mut seq = 0usize;
            let wh = build_filter(&filter, backend, "t", &col_fn(schema), &mut seq);
            let where_sql = if wh.text.is_empty() { String::new() } else { format!(" WHERE {}", wh.text) };
            let text = format!("SELECT COUNT(*) FROM {} t{}", q(backend, &schema.collection), where_sql);
            Ok(vec![SqlStmt::select(text, wh.params, RowShape::empty())])
        }
        "find" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let projection = cmd.get("projection").cloned();
            translate_find(backend, schema, &filter, projection.as_ref())
        }
        "aggregate" => {
            let pipeline = cmd.get("pipeline").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            translate_aggregate(backend, schema, &pipeline, registry, warnings)
        }
        _ => Err(format!("translate: 未知命令 kind = {}", kind)),
    }
}

/// 字段 → 列名：标量字段在本表；object/array 附属表字段跳过（标量字段原样）
fn col_fn(schema: &Schema) -> impl Fn(&str) -> Option<String> {
    let field_types: Vec<(String, String)> = schema
        .fields
        .iter()
        .map(|(k, v)| (k.clone(), v.field_type.clone()))
        .collect();
    move |field: &str| {
        if field.contains('.') {
            let (head, _) = field.split_once('.')?;
            // 点号字段：若 head 是 object 字段则跳过（附属表）；否则按整串处理
            let head_type = field_types.iter().find(|(k, _)| k == head).map(|(_, t)| t.clone());
            if matches!(head_type.as_deref(), Some("object") | Some("array")) {
                return None;
            }
            return Some(field.to_string());
        }
        let t = field_types.iter().find(|(k, _)| k == field).map(|(_, t)| t.clone());
        match t {
            Some(t) if t == "object" || t == "array" => None,
            _ => Some(field.to_string()),
        }
    }
}

// find：根表标量 + 可选 projection
fn translate_find(
    backend: Backend,
    schema: &Schema,
    filter: &Value,
    projection: Option<&Value>,
) -> Result<Vec<SqlStmt>, String> {
    let mut seq = 0usize;
    let wh = build_filter(filter, backend, "t", &col_fn(schema), &mut seq);
    let selected = projection_fields(schema, projection);
    let mut cols_sql = Vec::new();
    let mut columns = Vec::new();
    for f in &selected {
        if let Some(c) = col_fn(schema)(f) {
            cols_sql.push(format!("t.{}", q(backend, &c)));
            columns.push(RowCol::scalar(&c, &[f.as_str()]));
        }
    }
    let select_list = if cols_sql.is_empty() { q(backend, "_id") } else { cols_sql.join(", ") };
    let from = q(backend, &schema.collection);
    let where_sql = if wh.text.is_empty() { String::new() } else { format!(" WHERE {}", wh.text) };
    Ok(vec![SqlStmt::select(
        format!("SELECT {} FROM {} t{}", select_list, from, where_sql),
        wh.params,
        RowShape { columns },
    )])
}

/// 投影字段：null / 全 1 → 所有标量字段；否则取值为「非 0」的字段
fn projection_fields(schema: &Schema, projection: Option<&Value>) -> Vec<String> {
    match projection {
        None | Some(Value::Null) => schema.fields.keys().cloned().collect(),
        Some(p) => p.as_object()
            .map(|o| {
                let on: Vec<String> = o
                    .iter()
                    .filter(|(_, v)| {
                        v.as_i64().map(|n| n != 0).unwrap_or(false)
                            || v.as_bool() == Some(true)
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                if on.is_empty() { schema.fields.keys().cloned().collect() } else { on }
            })
            .unwrap_or_else(|| schema.fields.keys().cloned().collect()),
    }
}

/// aggregate：$match / $lookup(→LEFT JOIN) / $sort / $skip / $limit / $project
fn translate_aggregate(
    backend: Backend,
    schema: &Schema,
    pipeline: &[Value],
    registry: &Registry,
    warnings: &mut Vec<String>,
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
        } else if let Some(_r) = stage.get("$lookup") {
            unreachable!()
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
    let mut from_sql = format!("{} t", q(backend, &schema.collection));
    let mut all_params: Vec<Value> = Vec::new();
    for (i, j) in joins.iter().enumerate() {
        let r = format!("r{}", i);
        from_sql.push_str(&format!(
            " LEFT JOIN {} {} ON {}.{} = t.{}",
            q(backend, &j.from),
            r,
            r,
            q(backend, &j.foreign_col),
            q(backend, &j.local_col),
        ));
        // 关系表自身 schema
        if let Ok(rel_schema) = registry.get_by_collection(&j.from) {
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
                    param_seq += 1;
                    limit_params.push(json!(root_offset));
                }
            }
            _ => {
                if root_offset > 0 {
                    limit_sql = format!(" LIMIT ? OFFSET ?");
                    limit_params.push(json!(lim));
                    limit_params.push(json!(root_offset));
                } else {
                    limit_sql = format!(" LIMIT ?");
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
                limit_sql = format!(" LIMIT -1 OFFSET ?");
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

    // 忽略 LATERAL/窗口降级（milestone）：每父子 limit 输出警告交由调用方
}

#[derive(Debug, Clone)]
struct Join {
    rel_name: String,
    alias: String,
    from: String,
    local_col: String,
    foreign_col: String,
}

/// 从 schema.relations 解析 $lookup JOIN 键
fn resolve_join(schema: &Schema, registry: &Registry, alias: &str, from: &str) -> Option<Join> {
    let make = |name: &str, d: &crate::schema::RelationDef| Join {
        rel_name: name.to_string(),
        alias: alias.to_string(),
        from: from.to_string(),
        local_col: d.local_field.clone(),
        foreign_col: d.foreign_field.clone(),
    };
    // 优先按关系名匹配
    if let Some((name, d)) = schema.relations.iter().find(|(n, _)| n.as_str() == alias) {
        return Some(make(name, d));
    }
    // 按 model/collection 匹配
    schema.relations.iter().find_map(|(name, d)| {
        let ok = d.model == from
            || registry.get(&d.model).ok().map(|s| s.collection == from).unwrap_or(false);
        if ok {
            Some(make(name, d))
        } else {
            None
        }
    })
}

fn q(backend: Backend, ident: &str) -> String {
    backend.quote_ident(ident)
}