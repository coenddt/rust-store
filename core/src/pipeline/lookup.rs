//! $lookup / $addFields 阶段构建

use serde_json::{json, Map, Value};

use crate::permission::{get_readable_computes, Context};
use crate::schema::{Registry, RelationDef, Schema};
use crate::types::is_truthy;

use super::ast::RelAst;
use super::util::{append_order, is_nullish, non_nullish, param};
use super::{MAX_DEPTH, MAX_PAGINATED_DEPTH};

/// 外键匹配表达式：数组字段用 $in，否则 $eq
fn rel_match_expr(foreign_key: &str, let_var: &str, is_array_field: bool) -> Value {
    if is_array_field {
        json!({ "$expr": { "$in": [format!("${}", foreign_key), format!("$${}", let_var)] } })
    } else {
        json!({ "$expr": { "$eq": [format!("${}", foreign_key), format!("$${}", let_var)] } })
    }
}

/// let 变量守卫：数组字段用 $isArray，否则 $ifNull
fn rel_let_expr(local_key: &str, is_array_field: bool) -> Value {
    if is_array_field {
        json!({ "$cond": [{ "$isArray": format!("${}", local_key) }, format!("${}", local_key), []] })
    } else {
        json!({ "$ifNull": [format!("${}", local_key), null] })
    }
}

fn is_array_local_field(schema: &Schema, local_key: &str) -> bool {
    schema
        .fields
        .get(local_key)
        .map(|f| f.field_type == "array")
        .unwrap_or(false)
}

/// 构建空 $lookup（递归保护降级用）
pub fn build_empty_lookup(
    rel_name: &str,
    rel_def: &RelationDef,
    rel_schema: &Schema,
    source_schema: &Schema,
) -> Value {
    let local_key = &rel_def.local_field;
    let let_var = format!("rel_{}", local_key);
    let is_array = is_array_local_field(source_schema, local_key);
    let match_expr = rel_match_expr(&rel_def.foreign_field, &let_var, is_array);

    let mut let_map = Map::new();
    let_map.insert(let_var, rel_let_expr(local_key, is_array));

    let mut lookup = Map::new();
    lookup.insert(
        "from".to_string(),
        Value::String(rel_schema.collection.clone()),
    );
    lookup.insert("let".to_string(), Value::Object(let_map));
    lookup.insert("pipeline".to_string(), json!([{ "$match": match_expr }]));
    lookup.insert("as".to_string(), Value::String(rel_name.to_string()));

    json!({ "$lookup": Value::Object(lookup) })
}

/// 构建嵌套关系的 $project（请求字段 + fn 计算列 depends）
fn build_rel_projection(rel_ast: &RelAst, rel_schema: &Schema) -> Option<Value> {
    if rel_ast.fields.is_empty() {
        return None;
    }
    let mut proj = Map::new();
    proj.insert("_id".to_string(), json!(1));
    for f in &rel_ast.fields {
        proj.insert(f.clone(), json!(1));
    }
    // 补充 fn 计算列的 depends 字段
    for f in &rel_ast.fields {
        if let Some(comp) = rel_schema.compute(f) {
            if comp.has_fn {
                for dep in &comp.depends {
                    proj.entry(dep.clone()).or_insert(json!(1));
                }
            }
        }
    }
    Some(Value::Object(proj))
}

/// 构建所有嵌套关系 lookup 阶段（含 one 关系的 $unwind）
fn ns_lookup_stages(
    rel_ast: &RelAst,
    rel_schema: &Schema,
    params: &Map<String, Value>,
    depth: usize,
    next_paginated: usize,
    registry: &Registry,
) -> Result<Vec<Value>, String> {
    let mut stages = Vec::new();
    for (n_name, n_ast) in &rel_ast.relations {
        let n_def = rel_schema.relations.get(n_name).ok_or_else(|| {
            format!("关系 \"{}\" 未在 schema \"{}\" 中定义", n_name, rel_schema.name)
        })?;
        let n_schema = registry.get(&n_def.model)?;
        stages.push(build_lookup(
            n_name,
            n_ast,
            params,
            n_def,
            n_schema,
            rel_schema,
            depth + 1,
            next_paginated,
            registry,
        )?);
        if n_def.rel_type == "one" {
            stages.push(json!({
                "$unwind": { "path": format!("${}", n_name), "preserveNullAndEmptyArrays": true }
            }));
        }
    }
    Ok(stages)
}

/// `$lookup` 翻译入口：签名与 JS 参考实现逐一对应（跨语言 parity 优先于参数个数）。
#[allow(clippy::too_many_arguments)]
pub fn build_lookup(
    rel_name: &str,
    rel_ast: &RelAst,
    params: &Map<String, Value>,
    rel_def: &RelationDef,
    rel_schema: &Schema,
    source_schema: &Schema,
    depth: usize,
    paginated: usize,
    registry: &Registry,
) -> Result<Value, String> {
    let local_key = rel_def.local_field.clone();
    let foreign_key = rel_def.foreign_field.clone();
    let let_var = format!("rel_{}", local_key);

    let condition = param(params, rel_ast.params.get("condition")).cloned();
    let sort = param(params, rel_ast.params.get("sort")).cloned();
    let skip_val = param(params, rel_ast.params.get("skip")).cloned();
    let limit_val = param(params, rel_ast.params.get("limit")).cloned();

    // ── 递归保护（分两套深度限制） ──
    let has_paginated = !is_nullish(skip_val.as_ref()) || !is_nullish(limit_val.as_ref());
    let next_paginated = if has_paginated { paginated + 1 } else { paginated };
    if depth >= MAX_DEPTH || (has_paginated && paginated >= MAX_PAGINATED_DEPTH) {
        // 返回空 $lookup（只做外键匹配，不继续嵌套），pipeline 不崩溃
        return Ok(build_empty_lookup(rel_name, rel_def, rel_schema, source_schema));
    }

    let is_array = is_array_local_field(source_schema, &local_key);
    let mut stages: Vec<Value> = Vec::new();

    // $match: 外键关联 + 附加条件
    let match_expr = rel_match_expr(&foreign_key, &let_var, is_array);
    if let Some(cond) = non_nullish(condition.as_ref()) {
        stages.push(json!({ "$match": { "$and": [match_expr, cond] } }));
    } else {
        stages.push(json!({ "$match": match_expr }));
    }

    // sort / skip / limit（优先执行，避免全量数据流入后续嵌套 $lookup）
    // 当 sort 依赖嵌套关联字段时，嵌套 $lookup 必须优先于 sort
    let sorts_by_nested = sort
        .as_ref()
        .and_then(|s| s.as_object())
        .map(|o| o.keys().any(|k| k.contains('.')))
        .unwrap_or(false);
    if !sorts_by_nested {
        append_order(&mut stages, sort.as_ref(), skip_val.as_ref(), limit_val.as_ref());
    }

    // 嵌套 relations
    stages.extend(ns_lookup_stages(
        rel_ast,
        rel_schema,
        params,
        depth,
        next_paginated,
        registry,
    )?);

    // sort / skip / limit（兜底：仅在嵌套 $lookup 未提前执行时追加）
    if sorts_by_nested {
        append_order(&mut stages, sort.as_ref(), skip_val.as_ref(), limit_val.as_ref());
    }

    // $project: 只返回请求的字段 + 计算列 fn 的 depends
    if let Some(rel_proj) = build_rel_projection(rel_ast, rel_schema) {
        stages.push(json!({ "$project": rel_proj }));
    }

    let mut let_map = Map::new();
    let_map.insert(let_var, rel_let_expr(&local_key, is_array));

    let mut lookup = Map::new();
    lookup.insert(
        "from".to_string(),
        Value::String(rel_schema.collection.clone()),
    );
    lookup.insert("let".to_string(), Value::Object(let_map));
    lookup.insert("pipeline".to_string(), Value::Array(stages));
    lookup.insert("as".to_string(), Value::String(rel_name.to_string()));

    Ok(json!({ "$lookup": Value::Object(lookup) }))
}

/// 构建 compute 的独立 $lookup 阶段
pub fn build_compute_lookup_stages(schema: &Schema) -> Vec<Value> {
    let mut stages = Vec::new();
    for (key, comp) in &schema.computes {
        let Some(lookup) = &comp.lookup else { continue };
        let Some(lo) = lookup.as_object() else { continue };
        let Some(from) = lo.get("from").filter(|v| is_truthy(v)) else {
            continue;
        };
        let mut inner = Map::new();
        inner.insert("from".to_string(), from.clone());
        inner.insert(
            "let".to_string(),
            lo.get("let").cloned().unwrap_or_else(|| json!({})),
        );
        inner.insert(
            "pipeline".to_string(),
            lo.get("pipeline").cloned().unwrap_or_else(|| json!([])),
        );
        inner.insert(
            "as".to_string(),
            lo.get("as")
                .cloned()
                .unwrap_or_else(|| Value::String(format!("_{}", key))),
        );
        stages.push(json!({ "$lookup": Value::Object(inner) }));
    }
    stages
}

/// 构建 $addFields 阶段（lookup 类型计算列）
pub fn build_add_fields(schema: &Schema, ctx: Option<&Context>) -> Option<Value> {
    let readable_computes = if ctx.is_some() {
        get_readable_computes(schema, ctx)
    } else {
        None
    };

    let mut add_fields = Map::new();
    for (key, comp) in &schema.computes {
        // 权限裁剪：跳过不可读的计算列
        if let Some(set) = &readable_computes {
            if !set.contains(key) {
                continue;
            }
        }
        let Some(lookup) = &comp.lookup else { continue };
        let from_truthy = lookup
            .as_object()
            .and_then(|o| o.get("from"))
            .map(is_truthy)
            .unwrap_or(false);
        if from_truthy {
            // 独立 $lookup 模式：使用 addFields 表达式提取结果
            if let Some(af) = lookup.as_object().and_then(|o| o.get("addFields")) {
                if is_truthy(af) {
                    add_fields.insert(key.clone(), af.clone());
                }
            }
        } else {
            // 简单表达式模式
            add_fields.insert(key.clone(), lookup.clone());
        }
    }
    if add_fields.is_empty() {
        None
    } else {
        Some(json!({ "$addFields": Value::Object(add_fields) }))
    }
}
