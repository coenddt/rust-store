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
use crate::dialect::{field_is_bool, field_is_json, physical_of, Backend, ColumnRef};
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
                    backend.pcol(&c),
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

/// VR-03：PostgreSQL 原生向量检索的翻译态。
///
/// `param_idx` 是 `queryVector` 的**占位序号（0 基）**：距离表达式 / `_score` 表达式 /
/// 距离上界三处复用同一序号，保证 `queryVector` 只绑定一次。
#[derive(Debug, Clone)]
struct VectorSql {
    col: String,
    param_idx: usize,
    limit: i64,
    min_score: Option<f64>,
    metric: String,
    vector: Vec<f64>,
    /// 可进 `$vectorSearch.filter` 的预过滤条件（PG 侧必须落 WHERE）
    filter: Option<Value>,
}

/// 解析 `$vectorSearch` 阶段（与 `local::vector` 共用同一份二次校验）。
fn parse_vector_search(schema: &Schema, arg: &Value) -> Result<VectorSql, String> {
    let spec = crate::pipeline::vector::parse_stage(Some(schema), arg)?;
    let col = crate::dialect::scalar_column(schema, &spec.field).ok_or_else(|| {
        format!(
            "vector 字段 {} 无法映射到 SQL 列（schema \"{}\"）",
            spec.field, schema.name
        )
    })?;
    Ok(VectorSql {
        col,
        param_idx: 0,
        limit: spec.limit,
        min_score: spec.min_score,
        metric: spec.metric,
        vector: spec.vector,
        filter: spec.filter,
    })
}

/// pgvector 距离表达式：cosine `<=>` / l2 `<->` / ip `<#>`
///
/// 查询向量占位符与写侧 `bind_col`（V-19）同一约定：PG 显式 `CAST($n AS vector)`，
/// 让驱动按文本参数送入、由数据库解析（仅 PG 走原生向量路径，其余后端保持裸占位符）。
fn vector_distance(backend: Backend, v: &VectorSql) -> String {
    let op = match v.metric.as_str() {
        "l2" => "<->",
        "ip" => "<#>",
        _ => "<=>",
    };
    let ph = backend.placeholder(v.param_idx);
    let ph = if backend == Backend::Postgres {
        format!("CAST({ph} AS vector)")
    } else {
        ph
    };
    format!("t.{} {} {}", backend.pcol(&v.col), op, ph)
}

/// 识别 build.rs 生成的 minScore 后置 `$match`：`{"_score": {"$gte": n}}`。
///
/// 其余形态返回 `Ok(None)` 交给常规 filter 翻译；一旦出现 `_score` 但不是该形态
/// （或不是数值）→ 显式 `Err`，绝不静默丢弃。
fn fold_score_min(m: &Value) -> Result<Option<f64>, String> {
    let Some(obj) = m.as_object() else {
        return Ok(None);
    };
    if !obj.contains_key("_score") {
        return Ok(None);
    }
    if obj.len() != 1 {
        return Err("向量评分条件必须是单键 {\"_score\": {\"$gte\": n}}".to_string());
    }
    let inner = obj
        .get("_score")
        .and_then(Value::as_object)
        .ok_or_else(|| "向量评分条件 _score 必须是 {\"$gte\": n}".to_string())?;
    if inner.len() != 1 {
        return Err("向量评分条件 _score 仅支持 $gte".to_string());
    }
    inner
        .get("$gte")
        .and_then(Value::as_f64)
        .map(Some)
        .ok_or_else(|| "向量评分条件 _score.$gte 必须是数值".to_string())
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

    // VR-03：PostgreSQL 原生向量检索（`$vectorSearch` 必须是首阶段）
    let mut vector: Option<VectorSql> = None;

    for stage in pipeline {
        if let Some(vs) = stage.get("$vectorSearch") {
            // 仅 PostgreSQL（pgvector）可下推；其余后端应已由宿主走 vector_scan 降级，
            // 走到这里即 Err —— 拒绝静默全表扫描或静默忽略向量条件。
            if backend != Backend::Postgres {
                return Err(format!(
                    "SQL 后端 {:?} 不支持原生向量检索：应由宿主 vector_scan 降级（拒绝静默全表扫描或忽略条件）",
                    backend
                ));
            }
            if vector.is_some() {
                return Err("$vectorSearch 只允许出现一次（一次查询只做一次向量检索）".to_string());
            }
            // `$vectorSearch` 决定 TopK，必须先于一切过滤 / 关系 / 分页阶段落地
            if group.is_some()
                || project_on.is_some()
                || !joins.is_empty()
                || !compute_aggs.is_empty()
                || !root_matches.is_empty()
                || !root_order.is_empty()
                || root_limit.is_some()
            {
                return Err(
                    "$vectorSearch 必须是聚合管道首阶段（之前不得有 $match/$lookup/$sort/$limit/$project/$group）"
                        .to_string(),
                );
            }
            let v = parse_vector_search(schema, vs)?;
            // filter 是「检索前」的行过滤（ACL / owner / 业务条件）：PG 侧必须落 WHERE，
            // 否则该条件被静默丢弃。TopK 仍由向量阶段决定。
            if let Some(f) = &v.filter {
                root_matches.push(f.clone());
            }
            // `$near.$limit` = TopK；其后的 `$limit` 阶段（如 text2query 封顶）会覆盖此值
            root_limit = Some(v.limit);
            vector = Some(VectorSql { filter: None, ..v });
        } else if let Some(g) = stage.get("$group") {
            group = Some(group_agg::parse_group(g)?);
        } else if let Some(m) = stage.get("$match") {
            // `$group` 之后的 `$match` = `$having`（§9.3 固定序），之前的 = WHERE
            if group.is_some() {
                group_having = Some(m.clone());
            } else if let Some(ms) = fold_score_min(m)? {
                // VR-03：build.rs 生成的 minScore 后置 `$match` → 折叠为距离上界，
                // 不产生 `_score` 常规列条件（`_score` 不是 schema 字段）。
                let v = vector.as_mut().ok_or_else(|| {
                    "评分条件 _score 只能与向量检索（$near / $vectorSearch）并用".to_string()
                })?;
                if v.min_score.is_some() {
                    return Err("$near.$minScore 只允许出现一次".to_string());
                }
                v.min_score = Some(ms);
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
                    // - 对象点号路径（`meta.level`，U4）→ JSON 标量提取表达式
                    // - 关系点号路径（`rel.field`）且已存在同名 JOIN → `r{i}.<col>`
                    // - 其余（未知字段 / object·array 整值 / 关系名本身 / 无对应 $lookup）
                    //   → **不生成 SQL**，告警 + 标记 unsupported 交由 Host 兜底排序。
                    let json_expr = |col: &str, path: &[String]| -> Result<String, String> {
                        let base = format!("t.{}", backend.pcol(col));
                        let segs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
                        backend.json_extract_scalar(&base, &segs)
                    };
                    let resolved: Option<String> = match k.split_once('.') {
                        Some((head, rest)) => {
                            if let Some(i) = joins.iter().position(|j| j.rel_name == head) {
                                registry
                                    .get(&joins[i].model)
                                    .ok()
                                    .and_then(|rel| crate::dialect::scalar_column(rel, rest))
                                    .map(|c| format!("r{}.{}", i, backend.pcol(&c)))
                            } else {
                                // 对象点号路径（U4）：standard 档 JSON 提取；整值 object 排序不支持
                                match crate::dialect::field_column_ref(schema, k) {
                                    Some(ColumnRef::JsonPath(col, path)) => {
                                        Some(json_expr(&col, &path)?)
                                    }
                                    _ => None,
                                }
                            }
                        }
                        None => {
                            if schema.relations.iter().any(|(n, _)| n.as_str() == k) {
                                // 关系名本身（排序关系数组）不是标量列 → 不下推
                                None
                            } else {
                                match col_fn(schema)(k) {
                                    Some(ColumnRef::Scalar(c)) => {
                                        Some(format!("t.{}", backend.pcol(&c)))
                                    }
                                    // 整值 object/array 排序无意义 → 不下推
                                    Some(ColumnRef::Json(..)) => None,
                                    Some(ColumnRef::JsonPath(col, path)) => {
                                        Some(json_expr(&col, &path)?)
                                    }
                                    None => None,
                                }
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
            // VR-03 / VR-04：`_score: {$meta:"vectorSearchScore"}` 由向量列表达式提供 → no-op
            let is_score_only = af
                .as_object()
                .map(|o| {
                    o.len() == 1
                        && o.get("_score")
                            .and_then(|v| v.get("$meta"))
                            .and_then(Value::as_str)
                            == Some("vectorSearchScore")
                })
                .unwrap_or(false);
            if is_score_only && vector.is_some() {
                continue;
            }
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
                // 04：inline-array agg 计算列（路径首段 = 本模型数组字段）不进 compute_aggs
                // （其 $addFields 无对应 $lookup）→ SQL 无翻译。命中时给专属报错，
                // 指导改用 Mongo/本地后端；其余维持泛化报错。
                for k in &keys {
                    if compute_aggs.iter().any(|ca| &ca.key == k) {
                        continue;
                    }
                    let inline_hit = schema
                        .computes
                        .iter()
                        .find(|(ck, _)| ck == k)
                        .and_then(|(_, comp)| comp.agg.as_ref())
                        .and_then(|agg| {
                            agg.as_object()
                                .and_then(|o| o.values().next())
                                .and_then(Value::as_str)
                                .map(|path| path.split('.').next().unwrap_or(path).to_string())
                        })
                        .map(|head| {
                            schema.relations.get(&head).is_none()
                                && schema
                                    .fields
                                    .get(&head)
                                    .map(|f| f.field_type == "array")
                                    .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    if inline_hit {
                        return Err(format!(
                            "SQL 后端暂不支持 inline-array agg 计算列 \"{k}\"（文档内数组聚合无 SQL 翻译，仅 Mongo/本地后端支持）"
                        ));
                    }
                }
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
        if vector.is_some() {
            return Err(
                "向量检索（$vectorSearch）不能与 $group 并用（分组终结前无法保证 TopK 语义）"
                    .to_string(),
            );
        }
        let stmt = group_agg::translate_group(
            backend,
            schema,
            registry,
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
    //
    // `_score` 必须一并剔除：它是向量派生列，不在 schema.fields 中，但
    // `field_column_ref` 对未声明字段走宽松兜底（按裸列名处理，为 `_id` 等物理列服务），
    // 不会返回 None → 若不显式剔除，会经 `physical_of` 命名归一产出 `t."score"`
    //（`_score` 不在保留名白名单，被 canonical/to_snake 剥掉下划线）→ PG 42703
    // `column t.score does not exist`。该列由下方向量分支（VR-03/A6）事后补列。
    let root_selected: Vec<String> = selected
        .iter()
        .filter(|f| schema.relations.iter().all(|(rn, _)| rn != *f))
        .filter(|f| !compute_aggs.iter().any(|ca| &ca.key == *f))
        .filter(|f| f.as_str() != "_score")
        .cloned()
        .collect();
    for f in &root_selected {
        match col_fn(schema)(f) {
            // 标量列
            Some(ColumnRef::Scalar(c)) => {
                cols_sql.push(format!("t.{}", backend.pcol(&c)));
                if field_is_json(schema, f) {
                    // vector：列存 JSON 数组文本（PG 为原生 vector），读回按 JSON 解析
                    columns.push(RowCol::json(&physical_of(&c), &[f.as_str()]));
                } else {
                    // §9.7 布尔归一：schema `boolean` 字段的列值 0/1 → JSON bool
                    columns.push(RowCol::scalar_bool(
                        &physical_of(&c),
                        &[f.as_str()],
                        field_is_bool(schema, f),
                    ));
                }
            }
            // object/array JSON 列：整列取出，还原时解析 JSON 文本
            Some(ColumnRef::Json(c, _)) => {
                cols_sql.push(format!("t.{}", backend.pcol(&c)));
                columns.push(RowCol::json(&physical_of(&c), &[f.as_str()]));
            }
            // 对象点号路径投影：取该路径的标量值（还原为嵌套对象）
            Some(ColumnRef::JsonPath(c, path)) => {
                let base = format!("t.{}", backend.pcol(&c));
                let segs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
                let alias = f.replace('.', "_");
                let expr = backend.json_extract_scalar(&base, &segs)?;
                cols_sql.push(format!("{} AS {}", expr, q(backend, &alias)));
                columns.push(RowCol::scalar(&alias, &f.split('.').collect::<Vec<_>>()));
            }
            None => {}
        }
    }
    // `_score` 输出列（VR-03 / A6）：cosine `1-d`；l2 `1/(1+d)`；ip `-d`
    // `_score` 不在 schema.fields 中，上面循环的 `col_fn` 恒为 None → 必须按 `selected` 事后补列；
    // 未请求时不产出该键（A6 反向断言）。
    if let Some(v) = &vector {
        if selected.iter().any(|f| f == "_score") {
            let d = format!("({})", vector_distance(backend, v));
            let score_expr = match v.metric.as_str() {
                "l2" => format!("(1.0 / (1.0 + {}))", d),
                "ip" => format!("(-{})", d),
                _ => format!("(1.0 - {})", d),
            };
            cols_sql.push(format!("{} AS {}", score_expr, q(backend, "_score")));
            columns.push(RowCol::computed("_score", &["_score"]));
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
        // 关系目标 schema：database/schema 限定与字段展开都依赖它；定位失败 = 关系悬空 → 跳过并告警
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
                backend.pcol(&j.foreign_col),
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
            backend.pcol(&j.foreign_col),
            parent_alias,
            backend.pcol(&j.local_col),
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
                backend.pcol(&rf),
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
                // vector 落 JSON 文本（PG 原生 vector），读回按 JSON 解析
                is_json: field_is_json(rel_schema, &rf),
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
        let fk = backend.pcol(&ca.foreign_col);
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
                let arg_sql = format!("c.{}", backend.pcol(&col));
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
            backend.pcol(&ca.local_col),
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
    // VR-03：向量参数序号必须在 JOIN 参数之后、根 `$match` 参数之前确定
    // （`queryVector` 只绑定一次；距离 / 上界 / `_score` 三处复用同一序号）
    if let Some(v) = vector.as_mut() {
        v.param_idx = param_seq;
        param_seq += 1;
    }
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

    // VR-03 / VR-05：minScore → 距离上界（PG 原生距离运算符可直接走 HNSW）
    // cosine score = 1-d       → d <= 1-minScore
    // l2     score = 1/(1+d)   → d <= 1/minScore-1（minScore<=0 时得分恒 >0，无过滤效果）
    // ip     score = -d        → d <= -minScore
    if let Some(v) = &vector {
        let dist = vector_distance(backend, v);
        if let Some(ms) = v.min_score {
            let bound = match v.metric.as_str() {
                "l2" if ms > 0.0 => Some(1.0 / ms - 1.0),
                "l2" => None,
                "ip" => Some(-ms),
                _ => Some(1.0 - ms),
            };
            if let Some(b) = bound {
                let ph = backend.placeholder(param_seq);
                param_seq += 1;
                root_wheres.push(WhereClause {
                    text: format!("{} <= {}", dist, ph),
                    params: vec![json!(b)],
                });
            }
        }
        root_order.push(format!("{} ASC", dist));
    }
    all_params.extend(join_params);
    // 向量参数位于 JOIN 参数之后、根 `$match` 参数之前（与 `param_idx` 编号一致）
    if let Some(v) = &vector {
        // 查询向量与写侧 V-19 同一约定：core 序列化为 pgvector 文本输入格式
        // `"[1,0.2]"` —— asyncpg 拒绝数组参数（expected str, got list），node-pg 会把
        // 数组编码成 PG array 字面量 `{1,0.2}`，pgvector 均不接受。
        let text =
            serde_json::to_string(&v.vector).map_err(|e| format!("查询向量序列化失败：{e}"))?;
        all_params.push(json!(text));
    }
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
