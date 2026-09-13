//! `aggregate` 命令翻译：$match / $lookup(→LEFT JOIN) / $sort / $skip / $limit / $project
//!
//! 关系下推（D10 / P1）：`$lookup` → `LEFT JOIN`，**嵌套关系逐层 JOIN**，
//! **每父 top-N 用窗口函数 `ROW_NUMBER() OVER (PARTITION BY fk ORDER BY …)`** 派生表下推；
//! 关系子 pipeline 的 `$condition` / owner 注入作为 `ON … AND …`（或派生表内层 `WHERE`）下推。

use serde_json::{json, Value};

use crate::schema::{Registry, Schema};

use crate::dialect::filter::{
    build_filter, build_filter_with_relations, RelPredResolver, Warnings, WhereClause,
};
use crate::dialect::ir::{RowCol, RowShape, SqlStmt};
use crate::dialect::{field_is_bool, Backend};
use crate::pipeline::REL_PRED_PREFIX;

use super::group_agg::{self, GroupSpec};
use super::lookup_join::{resolve_join, Join};
use super::relation_agg;
use super::{col_fn, limit_offset_sql, projection_fields, q, tname};

/// §9.6 关系聚合谓词代理键解析器：`__rp….0` → `EXISTS` / `NOT EXISTS`
struct PredResolver<'a> {
    preds: &'a [relation_agg::SqlRelPredicate],
    backend: Backend,
    schema: &'a Schema,
    registry: &'a Registry,
}

impl RelPredResolver for PredResolver<'_> {
    fn resolve(
        &self,
        field: &str,
        cond: &Value,
        param_seq: &mut usize,
        warnings: Warnings<'_>,
    ) -> Result<Option<WhereClause>, String> {
        if !field.starts_with(REL_PRED_PREFIX) {
            return Ok(None);
        }
        let as_name = field.rsplit_once('.').map(|(a, _)| a).unwrap_or(field);
        let Some(p) = self.preds.iter().find(|p| p.as_name == as_name) else {
            return Err(format!(
                "关系聚合谓词代理字段 \"{field}\" 没有对应的 $lookup"
            ));
        };
        let exists = cond
            .as_object()
            .and_then(|o| o.get("$exists"))
            .and_then(|v| v.as_bool())
            .ok_or_else(|| {
                format!("关系聚合谓词代理字段 \"{field}\" 的条件非法（须为 {{\"$exists\": bool}}）")
            })?;
        Ok(Some(relation_agg::exists_clause(
            self.backend,
            "t",
            self.schema,
            self.registry,
            p,
            !exists,
            param_seq,
            warnings,
        )?))
    }
}

/// 从关系子 pipeline 的 `$match` 中提取「除 join 键外」的附加条件
/// （关系 `$condition` + 目标 owner 注入）。join 键条件形如
/// `{$expr: {$eq: ["$fk", "$$letVar"]}}`（含 `$$` let 变量）→ 跳过。
pub(super) fn lookup_extra_condition(pipeline: &[Value]) -> Option<Value> {
    let mut extras: Vec<Value> = Vec::new();
    for stage in pipeline {
        let Some(m) = stage.get("$match") else {
            continue;
        };
        let parts: Vec<&Value> = match m.get("$and").and_then(|a| a.as_array()) {
            Some(a) => a.iter().collect(),
            None => vec![m],
        };
        for p in parts {
            // join 键条件：$expr 且含 let 变量引用（$$xxx）
            let is_join_key = p.get("$expr").is_some() && p.to_string().contains("$$");
            if !is_join_key {
                extras.push(p.clone());
            }
        }
    }
    match extras.len() {
        0 => None,
        1 => Some(extras.remove(0)),
        _ => Some(json!({ "$and": extras })),
    }
}

/// 归一聚合计算列（§9.2(2)）的 `$lookup` 描述：`$lookup`(`as` = `_<key>`) + `$addFields`
/// 由 [`translate_aggregate`] 翻译为「派生表 `LEFT JOIN (… GROUP BY fk)` + 标量列」，
/// 与 Mongo 端 `$lookup` + `$addFields` 逐行对齐（§9.7 空集语义）。
#[derive(Debug, Clone)]
struct ComputeAgg {
    /// 计算列名（同时是还原路径）
    key: String,
    /// 聚合关系名（root schema.relations 的键）
    rel_name: String,
    /// 关系目标 model 名（定位目标 schema 的唯一依据）
    model: String,
    /// 关系本地键（根表标量列）
    local_col: String,
    /// 关系外键（子表标量列，派生表 `GROUP BY` 键）
    foreign_col: String,
    /// 白名单算子 `$count/$sum/$avg/$min/$max`
    op: String,
    /// `$sum/$avg/$min/$max` 的子字段；`$count` 为 None
    field: Option<String>,
    /// 子 pipeline `$match` 中除 join 键外的附加条件（目标 owner 注入等）
    extra: Option<Value>,
}

/// 识别归一聚合计算列的 `$lookup`：`as` = `_<key>` 且该计算列声明了 `agg`（§9.2(2)）。
/// 返回 `None` = 普通关系 `$lookup`（走 [`collect_lookup`] 的关系 JOIN 解析）。
fn resolve_compute_agg(schema: &Schema, lo: &Value) -> Option<ComputeAgg> {
    let as_name = lo.get("as").and_then(|v| v.as_str())?;
    let key = as_name.strip_prefix('_')?;
    let agg = schema.compute(key)?.agg.clone()?;
    let (op, path) = agg
        .as_object()
        .and_then(|o| o.iter().next())
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))?;
    let (rel_name, field) = match path.split_once('.') {
        Some((r, f)) => (r.to_string(), Some(f.to_string())),
        None => (path, None),
    };
    let rel_def = schema.relations.get(&rel_name)?;
    let pipeline: Vec<Value> = lo
        .get("pipeline")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Some(ComputeAgg {
        key: key.to_string(),
        rel_name,
        model: rel_def.model.clone(),
        local_col: rel_def.local_field.clone(),
        foreign_col: rel_def.foreign_field.clone(),
        op,
        field,
        extra: lookup_extra_condition(&pipeline),
    })
}

/// 递归收集 `$lookup` 及**其子 pipeline 内的嵌套 `$lookup`** 为扁平 JOIN 列表。
///
/// - `schema` = 关系**父表** schema（根表或上一层关系表）；
/// - `parent` = 父 JOIN 下标（`None` = 根表）；
/// - `parent_path` / `parent_ones` = 逐级关系名路径与基数（决定平铺列还原层级）。
// 递归收集 JOIN 需携带别名 / 列 / joins 出参等上下文，拆结构体反而降低可读性
#[allow(clippy::too_many_arguments)]
fn collect_lookup(
    schema: &Schema,
    registry: &Registry,
    lo: &Value,
    parent: Option<usize>,
    parent_path: &[String],
    parent_ones: &[bool],
    joins: &mut Vec<Join>,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let alias = lo
        .get("as")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let from = lo
        .get("from")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let Some(mut j) = resolve_join(schema, registry, &alias, &from) else {
        warnings.push(format!("$lookup 关系 {} 无法匹配 schema，跳过 JOIN", alias));
        return Ok(());
    };
    let child_pipeline: Vec<Value> = lo
        .get("pipeline")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    j.parent = parent;
    j.path = {
        let mut p = parent_path.to_vec();
        p.push(j.rel_name.clone());
        p
    };
    j.ones = {
        let mut o = parent_ones.to_vec();
        o.push(j.one);
        o
    };
    // 关系子 pipeline 的 `$match` 含「join 键」+「关系 $condition」+「目标 owner 注入」；
    // 后者须下推（`ON … AND …` 或派生表内层 WHERE），绝不静默丢弃（丢弃 = 越权返回他人行）。
    j.extra = lookup_extra_condition(&child_pipeline);
    // 每父 top-N（子 `$sort` / `$skip` / `$limit`）→ 窗口函数下推
    j.child_sort = child_pipeline.iter().find_map(|s| s.get("$sort").cloned());
    j.child_skip = child_pipeline
        .iter()
        .find_map(|s| s.get("$skip").and_then(|v| v.as_i64()));
    j.child_limit = child_pipeline
        .iter()
        .find_map(|s| s.get("$limit").and_then(|v| v.as_i64()));

    let idx = joins.len();
    let path = j.path.clone();
    let ones = j.ones.clone();
    let model = j.model.clone();
    joins.push(j);

    // 递归展开嵌套关系（其 local_field 位于本层关系表上）
    let rel_schema = registry.get(&model)?.clone();
    for stage in &child_pipeline {
        if let Some(nested) = stage.get("$lookup") {
            collect_lookup(
                &rel_schema,
                registry,
                nested,
                Some(idx),
                &path,
                &ones,
                joins,
                warnings,
            )?;
        }
    }
    Ok(())
}

/// 子关系排序 SQL（窗口函数 `ORDER BY`）：键必须是本层标量列；无法映射 → Err（绝不静默）。
fn child_order_sql(backend: Backend, rel_schema: &Schema, j: &Join) -> Result<String, String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = &j.child_sort {
        if let Some(o) = s.as_object() {
            for (k, dir) in o {
                let Some(c) = crate::dialect::scalar_column(rel_schema, k) else {
                    return Err(format!(
                        "$lookup 关系 {} 的子 $sort 字段 {} 无法映射到本层标量列，窗口函数下推不支持",
                        j.alias, k
                    ));
                };
                let d = dir.as_i64().unwrap_or(1);
                parts.push(format!(
                    "c.{} {}",
                    q(backend, &c),
                    if d >= 0 { "ASC" } else { "DESC" }
                ));
            }
        }
    }
    if parts.is_empty() {
        // 无显式子排序：以 `_id` 升序保证确定性（对齐 §9.7 tie-break 精神）
        parts.push(format!("c.{} ASC", q(backend, "_id")));
    }
    Ok(parts.join(", "))
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
    // 根表 `$match`：先收集原始 filter，待 JOIN 的 `ON` 附加条件生成后再按 SQL 文本顺序翻译
    // （JOIN `ON` 在 `WHERE` 之前；`?` 位置占位（SQLite/MySQL）要求参数数组与文本顺序一致）
    let mut root_matches: Vec<Value> = Vec::new();
    let mut root_order: Vec<String> = Vec::new();
    let mut root_limit: Option<i64> = None;
    let mut root_offset: i64 = 0;
    let mut joins: Vec<Join> = Vec::new();
    let mut compute_aggs: Vec<ComputeAgg> = Vec::new();
    // §9.6 关系聚合谓词（跨表条件过滤）：由 `__rp…` 前缀的 `$lookup` 识别
    let mut rel_preds: Vec<relation_agg::SqlRelPredicate> = Vec::new();
    let mut project_on: Option<Vec<String>> = None;

    // 根级 `$group`（§9.2(1)）：命中后进入分组聚合路径（GROUP BY / HAVING / 分组后排序分页）
    let mut group: Option<GroupSpec> = None;
    let mut group_having: Option<Value> = None;
    let mut group_order: Vec<(String, i64)> = Vec::new();
    let mut group_skip: i64 = 0;
    let mut group_limit: Option<i64> = None;
    let mut group_project: Option<Value> = None;

    for stage in pipeline {
        if let Some(g) = stage.get("$group") {
            group = Some(group_agg::parse_group(g)?);
        } else if let Some(m) = stage.get("$match") {
            // `$group` 之后的 `$match` = `$having`（§9.3 固定序），之前的 = WHERE
            if group.is_some() {
                group_having = Some(m.clone());
            } else {
                root_matches.push(m.clone());
            }
        } else if let Some(lo) = stage.get("$lookup") {
            let as_name = lo.get("as").and_then(|v| v.as_str()).unwrap_or("");
            if as_name.starts_with(REL_PRED_PREFIX) {
                // §9.6 关系聚合谓词 → EXISTS / NOT EXISTS（由外层 `$match` 代理键引用）
                rel_preds.push(relation_agg::parse(lo, schema, registry, backend)?);
            } else {
                // 归一聚合计算列 `$lookup`（`as` = `_<key>`）→ 派生表 LEFT JOIN；否则关系 JOIN
                match resolve_compute_agg(schema, lo) {
                    Some(ca) => compute_aggs.push(ca),
                    None => {
                        collect_lookup(schema, registry, lo, None, &[], &[], &mut joins, warnings)?
                    }
                }
            }
        } else if let Some(s) = stage.get("$sort") {
            if let Some(o) = s.as_object() {
                for (k, dir) in o {
                    // 分组后排序：键域 = by 键 ∪ agg 别名，在分组聚合路径内解析
                    if group.is_some() {
                        group_order.push((k.clone(), dir.as_i64().unwrap_or(1)));
                        continue;
                    }
                    // 排序键 → 有效列表达式（缺陷修复 M-10-1）：
                    // - 无点号标量字段 → `t.<col>`
                    // - 关系点号路径（`rel.field`）且已存在同名 JOIN → `r{i}.<col>`
                    // - 其余（未知字段 / object·array 字段 / 关系名本身 / 无对应 $lookup）
                    //   → **不生成 SQL**，告警 + 标记 unsupported 交由 Host 兜底排序。
                    let resolved: Option<String> = match k.split_once('.') {
                        Some((head, rest)) => {
                            joins.iter().position(|j| j.rel_name == head).and_then(|i| {
                                registry
                                    .get(&joins[i].model)
                                    .ok()
                                    .and_then(|rel| crate::dialect::scalar_column(rel, rest))
                                    .map(|c| format!("r{}.{}", i, q(backend, &c)))
                            })
                        }
                        None => {
                            if schema.relations.iter().any(|(n, _)| n.as_str() == k) {
                                // 关系名本身（排序关系数组）不是标量列 → 不下推
                                None
                            } else {
                                col_fn(schema)(k).map(|c| format!("t.{}", q(backend, &c)))
                            }
                        }
                    };
                    match resolved {
                        Some(expr) => {
                            let d = dir.as_i64().unwrap_or(1);
                            root_order.push(format!(
                                "{} {}",
                                expr,
                                if d >= 0 { "ASC" } else { "DESC" }
                            ));
                        }
                        None => {
                            warnings.push(format!(
                                "$sort 字段 {k} 无法映射到有效列（未知字段 / object·array 字段 / 无对应 $lookup），未下推排序"
                            ));
                            unsupported.push(json!({
                                "code": "sortField",
                                "field": k,
                                "reason": "$sort 字段无法映射到有效列，需 Host 侧兜底排序",
                            }));
                        }
                    }
                }
            }
        } else if let Some(l) = stage.get("$skip") {
            if group.is_some() {
                group_skip = l.as_i64().unwrap_or(0);
            } else {
                root_offset = l.as_i64().unwrap_or(0);
            }
        } else if let Some(l) = stage.get("$limit") {
            if group.is_some() {
                group_limit = l.as_i64();
            } else {
                root_limit = l.as_i64();
            }
        } else if let Some(p) = stage.get("$project") {
            if group.is_some() {
                group_project = Some(p.clone());
            } else {
                // D2：显式投影 object/array 字段在 SQL 侧无列 → 显式 Err，绝不静默丢弃该列
                super::check_projection_supported(schema, Some(p))?;
                project_on = Some(projection_fields(schema, Some(p)));
            }
        } else if stage.get("$facet").is_some() || stage.get("$replaceRoot").is_some() {
            // 全表单组空集护栏（§9.7）：Mongo 用 `$facet` + `$replaceRoot` 把空输入补成 1 行，
            // SQL 无 `GROUP BY` 时天然对空输入返回 1 行 → 该护栏对 SQL 是 no-op。
            // 仅当确在分组路径内才忽略；否则维持显式 Err（绝不静默忽略未知阶段）。
            if group.is_none() {
                let stage_name = if stage.get("$facet").is_some() {
                    "$facet"
                } else {
                    "$replaceRoot"
                };
                return Err(format!("SQL 后端暂不支持的聚合阶段 {stage_name}"));
            }
        } else if let Some(u) = stage.get("$unwind") {
            // one 关系：`$lookup` + `$unwind` 已由 LEFT JOIN（至多一条匹配）+ one 塑形等价实现，
            // 此处为 no-op。仅当该 path 确实是**本层**已解析的 one 关系 JOIN 产物时才安全忽略；
            // 其余 $unwind（非关系、或对应 many 关系）维持显式 Err，绝不静默忽略。
            let path = u.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let rel = path.strip_prefix('$').unwrap_or(path);
            if !joins
                .iter()
                .any(|j| j.path.len() == 1 && j.path[0] == rel && j.one)
            {
                return Err(format!(
                    "SQL 后端不支持的 $unwind 路径 {path}（非 one 关系 $lookup 产物）"
                ));
            }
        } else if let Some(af) = stage.get("$addFields") {
            // 归一聚合计算列（§9.2(2)）：`$addFields` 的键均已由对应 `$lookup` 翻译为
            // 派生表 LEFT JOIN 的标量列 → 此处为 no-op。仅当键全为已解析的计算列才安全忽略；
            // 其余（无关计算列的 $addFields）维持显式 Err，绝不静默忽略。
            let keys: Vec<String> = af
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            let all_resolved = !keys.is_empty()
                && keys
                    .iter()
                    .all(|k| compute_aggs.iter().any(|ca| &ca.key == k));
            if !all_resolved {
                return Err(
                    "SQL 后端暂不支持的聚合阶段 $addFields：拒绝静默忽略后返回未聚合的原始行"
                        .to_string(),
                );
            }
        } else {
            // S1 / 缺陷 G-02：未实现阶段（$count / $addFields 之外的聚合阶段）。
            // 此前静默忽略 → 返回「未聚合的原始行」＝语义失真。
            // 硬化契约：SQL 无法翻译即显式报错，绝不静默忽略。
            // （`$group` / `$having` 已由分组聚合路径支持，见 group_agg；`$facet`/`$replaceRoot`
            //   仅在分组路径内作空集护栏 no-op。）
            let stage_name = stage
                .as_object()
                .and_then(|m| m.keys().next())
                .cloned()
                .unwrap_or_else(|| "?".to_string());
            return Err(format!(
                "SQL 后端暂不支持的聚合阶段 {stage_name}：拒绝静默忽略后返回未聚合的原始行"
            ));
        }
    }

    // ── 根级 `$group`：分组聚合路径（GROUP BY / HAVING / 分组后 ORDER BY / LIMIT） ──
    if let Some(spec) = group {
        let stmt = group_agg::translate_group(
            backend,
            schema,
            &spec,
            &root_matches,
            group_having.as_ref(),
            &group_order,
            group_skip,
            group_limit,
            group_project.as_ref(),
            &mut param_seq,
            warnings,
        )?;
        return Ok(vec![stmt]);
    }

    // ── 拼装 SELECT ──
    let selected = project_on.unwrap_or_else(|| schema.fields.keys().cloned().collect());
    let mut cols_sql = Vec::new();
    let mut columns: Vec<RowCol> = Vec::new();

    // 根表恒选 `_id`（标量）—— 作为平铺行还原时的文档分组键，保证 $lookup 聚合可分组
    cols_sql.push(format!("t.{}", q(backend, "_id")));
    columns.push(RowCol::scalar("_id", &["_id"]));

    // 根表选列（排除关系名与归一聚合计算列 —— 后者不是真实列，由派生表 LEFT JOIN 物化）
    let root_selected: Vec<String> = selected
        .iter()
        .filter(|f| schema.relations.iter().all(|(rn, _)| rn != *f))
        .filter(|f| !compute_aggs.iter().any(|ca| &ca.key == *f))
        .cloned()
        .collect();
    for f in &root_selected {
        if let Some(c) = col_fn(schema)(f) {
            cols_sql.push(format!("t.{}", q(backend, &c)));
            // §9.7 布尔归一：schema `boolean` 字段的列值 0/1 → JSON bool
            columns.push(RowCol::scalar_bool(
                &c,
                &[f.as_str()],
                field_is_bool(schema, f),
            ));
        }
    }
    if cols_sql.is_empty() && !selected.is_empty() {
        cols_sql.push(format!("t.{}", q(backend, "_id")));
        columns.push(RowCol::scalar("_id", &["_id"]));
    }

    // 每个 JOIN：LEFT JOIN，并展开关系表的标量字段（含关系自身的 _id 便于聚合）
    let mut from_sql = format!("{} t", tname(backend, schema));
    let mut all_params: Vec<Value> = Vec::new();
    // JOIN 附加条件参数（文本顺序在根表 `WHERE` 之前，故须先于根表 `$match` 参数入数组）
    let mut join_params: Vec<Value> = Vec::new();
    for (i, j) in joins.iter().enumerate() {
        let r = format!("r{}", i);
        let parent_alias = match j.parent {
            Some(p) => format!("r{}", p),
            None => "t".to_string(),
        };
        // 关系目标 schema：namespace 限定与字段展开都依赖它；定位失败 = 关系悬空 → 跳过并告警
        let rel_schema = match registry.get(&j.model) {
            Ok(s) => s,
            Err(e) => {
                warnings.push(format!(
                    "$lookup 关系 {} 目标不可定位，跳过 JOIN: {}",
                    j.alias, e
                ));
                continue;
            }
        };
        let has_topn = j.child_limit.is_some() || j.child_skip.is_some();

        // 关系 `$condition` + 目标 owner 注入 → 下推；绝不静默丢弃（E-12）
        let mut extra_sql = String::new();
        if let Some(extra) = &j.extra {
            // 有窗口时条件须在**派生表内层**（先过滤后编号，对齐 Mongo `$match`→`$sort`→`$limit`）；
            // 否则置于 JOIN `ON … AND …`
            let alias_for_extra = if has_topn { "c" } else { r.as_str() };
            let wh = build_filter(
                extra,
                backend,
                alias_for_extra,
                &col_fn(rel_schema),
                &mut param_seq,
                Some(&mut *warnings),
            )?;
            if !wh.text.is_empty() {
                extra_sql = wh.text;
                join_params.extend(wh.params);
            }
        }

        // 子表引用：有每父 top-N → 窗口函数派生表；否则直连表名
        let child_ref = if has_topn {
            let order = child_order_sql(backend, rel_schema, j)?;
            let mut inner = format!(
                "SELECT c.*, ROW_NUMBER() OVER (PARTITION BY c.{} ORDER BY {}) AS {} FROM {} c",
                q(backend, &j.foreign_col),
                order,
                q(backend, "__rn"),
                tname(backend, rel_schema),
            );
            if !extra_sql.is_empty() {
                inner.push_str(&format!(" WHERE {}", extra_sql));
                extra_sql.clear();
            }
            let mut conds: Vec<String> = Vec::new();
            let skip = j.child_skip.unwrap_or(0);
            if skip > 0 {
                conds.push(format!("w.{} > {}", q(backend, "__rn"), skip));
            }
            if let Some(lm) = j.child_limit {
                conds.push(format!("w.{} <= {}", q(backend, "__rn"), skip + lm));
            }
            if conds.is_empty() {
                format!("({})", inner)
            } else {
                format!(
                    "(SELECT * FROM ({}) w WHERE {})",
                    inner,
                    conds.join(" AND ")
                )
            }
        } else {
            tname(backend, rel_schema)
        };
        let on_extra = if extra_sql.is_empty() {
            String::new()
        } else {
            format!(" AND {}", extra_sql)
        };
        from_sql.push_str(&format!(
            " LEFT JOIN {} {} ON {}.{} = {}.{}{}",
            child_ref,
            r,
            r,
            q(backend, &j.foreign_col),
            parent_alias,
            q(backend, &j.local_col),
            on_extra,
        ));

        let rel_cols = projection_fields(rel_schema, None);
        for rf in rel_cols {
            if rel_schema
                .fields
                .get(&rf)
                .map(|f| f.field_type == "object" || f.field_type == "array")
                .unwrap_or(false)
            {
                continue;
            }
            let alias_col = format!("{}_{}_{}", j.alias, i, rf);
            cols_sql.push(format!(
                "{}.{} AS {}",
                r,
                q(backend, &rf),
                q(backend, &alias_col)
            ));
            // 关系列：json_path = 关系路径 + 字段名；ones = 每级基数（one→对象/null）
            let mut json_path = j.path.clone();
            json_path.push(rf.clone());
            columns.push(RowCol {
                alias: alias_col,
                json_path,
                is_array: true,
                one: *j.ones.first().unwrap_or(&false),
                ones: j.ones.clone(),
                sub_shape: None,
                always: false,
                // §9.7 布尔归一：关系表的 `boolean` 字段同样 0/1 → bool
                is_bool: field_is_bool(rel_schema, &rf),
            });
        }
    }

    // ── 归一聚合计算列（§9.2(2)）→ 派生表 LEFT JOIN + 标量列 ──
    // `LEFT JOIN (SELECT fk, AGG(…) AS v FROM 子表 [WHERE extra] GROUP BY fk) aN ON aN.fk = t.local`
    // 空集语义（§9.7）：`$count` → `COALESCE(agg, 0)`；`$sum/$avg/$min/$max` → NULL（显式 null，
    // 由 `RowCol::computed` 的 `always` 强制写入）。与 Mongo `$lookup` + `$addFields` 逐行对齐。
    for (i, ca) in compute_aggs.iter().enumerate() {
        let rel_schema = match registry.get(&ca.model) {
            Ok(s) => s,
            Err(e) => {
                return Err(format!(
                    "计算列 \"{}\" 的聚合关系 \"{}\" 目标不可定位: {}",
                    ca.key, ca.rel_name, e
                ))
            }
        };
        let alias = format!("a{}", i);
        // 子表附加条件（目标 owner 注入等）→ 派生表内层 WHERE（先过滤后聚合）
        let mut extra_sql = String::new();
        if let Some(extra) = &ca.extra {
            let wh = build_filter(
                extra,
                backend,
                "c",
                &col_fn(rel_schema),
                &mut param_seq,
                Some(&mut *warnings),
            )?;
            if !wh.text.is_empty() {
                extra_sql = wh.text;
                join_params.extend(wh.params);
            }
        }
        let fk = q(backend, &ca.foreign_col);
        let agg_expr = match ca.op.as_str() {
            "$count" => "COUNT(*)".to_string(),
            op @ ("$sum" | "$avg" | "$min" | "$max") => {
                let f = ca
                    .field
                    .as_deref()
                    .ok_or_else(|| format!("计算列 \"{}\" 的 {op} 必须引用关系字段", ca.key))?;
                let col = crate::dialect::scalar_column(rel_schema, f).ok_or_else(|| {
                    format!(
                        "计算列 \"{}\" 的聚合字段 {} 无法映射到关系 {} 的标量列",
                        ca.key, f, ca.rel_name
                    )
                })?;
                let fn_name = match op {
                    "$sum" => "SUM",
                    "$avg" => "AVG",
                    "$min" => "MIN",
                    _ => "MAX",
                };
                let arg_sql = format!("c.{}", q(backend, &col));
                if op == "$avg" {
                    // §9.7「数值归 double」：先 CAST 到双精度，消除 MySQL `AVG(int)` 的 4 位小数截断
                    format!("AVG(CAST({arg_sql} AS {}))", backend.double_type())
                } else {
                    format!("{}({})", fn_name, arg_sql)
                }
            }
            other => {
                return Err(format!(
                    "计算列 \"{}\" 的 agg 算子 {other} 不在白名单（$count/$sum/$avg/$min/$max）",
                    ca.key
                ))
            }
        };
        let mut inner = format!(
            "SELECT c.{} AS {}, {} AS {} FROM {} c",
            fk,
            q(backend, "fk"),
            agg_expr,
            q(backend, "v"),
            tname(backend, rel_schema),
        );
        if !extra_sql.is_empty() {
            inner.push_str(&format!(" WHERE {}", extra_sql));
        }
        inner.push_str(&format!(" GROUP BY c.{}", fk));
        from_sql.push_str(&format!(
            " LEFT JOIN ({}) {} ON {}.{} = t.{}",
            inner,
            alias,
            alias,
            q(backend, "fk"),
            q(backend, &ca.local_col),
        ));
        let out = if ca.op == "$count" {
            format!("COALESCE({}.{}, 0)", alias, q(backend, "v"))
        } else {
            format!("{}.{}", alias, q(backend, "v"))
        };
        cols_sql.push(format!("{} AS {}", out, q(backend, &ca.key)));
        columns.push(RowCol::computed(&ca.key, &[ca.key.as_str()]));
    }

    // 根表 `$match` → WHERE（文本顺序在 JOIN `ON` 之后，故参数接在 join_params 之后）
    // §9.6 关系聚合谓词代理键（`__rp….0`）→ EXISTS / NOT EXISTS
    let resolver = PredResolver {
        preds: &rel_preds,
        backend,
        schema,
        registry,
    };
    let mut root_wheres: Vec<WhereClause> = Vec::new();
    for m in &root_matches {
        let wh = build_filter_with_relations(
            m,
            backend,
            "t",
            &col_fn(schema),
            Some(&resolver),
            &mut param_seq,
            Some(&mut *warnings),
        )?;
        if !wh.text.is_empty() {
            root_wheres.push(wh);
        }
    }
    all_params.extend(join_params);
    let where_sql = if root_wheres.is_empty() {
        String::new()
    } else {
        let text = root_wheres
            .iter()
            .map(|w| w.text.clone())
            .collect::<Vec<_>>()
            .join(" AND ");
        for w in &root_wheres {
            all_params.extend(w.params.clone());
        }
        format!(" WHERE {}", text)
    };

    let order_sql = if root_order.is_empty() {
        String::new()
    } else {
        format!(" ORDER BY {}", root_order.join(", "))
    };

    // LIMIT/OFFSET 需参数（按后端生成合法惯用法，见 limit_offset_sql）
    let (limit_sql, limit_params) =
        limit_offset_sql(backend, root_limit, root_offset, &mut param_seq);
    all_params.extend(limit_params);

    let select_list = if cols_sql.is_empty() {
        q(backend, "_id")
    } else if !cols_sql.iter().any(|c| c.contains("__present")) {
        // 缺失 vs null 三态（F-07）：根表标量随行携带 `__present` 哨兵列，供还原时区分
        // 「显式 null（有键）」与「缺失（无键）」。仅需查一次（根表列），不入用户投影。
        cols_sql.push("t.__present AS __present".to_string());
        cols_sql.join(", ")
    } else {
        cols_sql.join(", ")
    };
    let text = format!(
        "SELECT {} FROM {}{}{}{}",
        select_list, from_sql, where_sql, order_sql, limit_sql,
    );
    Ok(vec![SqlStmt::select(
        text,
        all_params,
        RowShape {
            columns,
            present_alias: Some("__present".to_string()),
        },
    )])
}
