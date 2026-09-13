//! $lookup / $addFields 阶段构建

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::command::ERR_PERMISSION;
use crate::permission::{
    can_read_schema, get_readable_computes, is_field_readable, is_relation_readable,
    merge_owner_condition, Context,
};
use crate::schema::{Registry, RelationDef, Schema};
use crate::types::{validate_condition, validate_condition_shape, validate_sort_shape};

use super::ast::RelAst;
use super::util::{append_order, is_nullish, non_nullish, param};
use super::{MAX_DEPTH, MAX_PAGINATED_DEPTH};

/// 外键匹配表达式：数组字段用 $in，否则 $eq
pub(crate) fn rel_match_expr(foreign_key: &str, let_var: &str, is_array_field: bool) -> Value {
    if is_array_field {
        json!({ "$expr": { "$in": [format!("${}", foreign_key), format!("$${}", let_var)] } })
    } else {
        json!({ "$expr": { "$eq": [format!("${}", foreign_key), format!("$${}", let_var)] } })
    }
}

/// let 变量守卫：数组字段用 $isArray，否则 $ifNull
pub(crate) fn rel_let_expr(local_key: &str, is_array_field: bool) -> Value {
    if is_array_field {
        json!({ "$cond": [{ "$isArray": format!("${}", local_key) }, format!("${}", local_key), []] })
    } else {
        json!({ "$ifNull": [format!("${}", local_key), null] })
    }
}

pub(crate) fn is_array_local_field(schema: &Schema, local_key: &str) -> bool {
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
    // 保留嵌套关系名（其值已由嵌套 $lookup + $unwind 落为本层 object/数组），
    // 否则最终 $project 会把已解析的嵌套关系投影丢弃（C-05：lessons 内 parent）
    for (n_name, _) in &rel_ast.relations {
        proj.insert(n_name.clone(), json!(1));
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
    ctx: Option<&Context>,
) -> Result<Vec<Value>, String> {
    let mut stages = Vec::new();
    for (n_name, n_ast) in &rel_ast.relations {
        let n_def = rel_schema.relations.get(n_name).ok_or_else(|| {
            format!(
                "关系 \"{}\" 未在 schema \"{}\" 中定义",
                n_name, rel_schema.name
            )
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
            ctx,
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
    ctx: Option<&Context>,
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
    let next_paginated = if has_paginated {
        paginated + 1
    } else {
        paginated
    };
    if depth >= MAX_DEPTH || (has_paginated && paginated >= MAX_PAGINATED_DEPTH) {
        // 返回空 $lookup（只做外键匹配，不继续嵌套），pipeline 不崩溃
        return Ok(build_empty_lookup(
            rel_name,
            rel_def,
            rel_schema,
            source_schema,
        ));
    }

    let is_array = is_array_local_field(source_schema, &local_key);
    let mut stages: Vec<Value> = Vec::new();

    // $match: 外键关联 + 附加条件 + 关系目标 owner 注入（R1：关系 read=creator 时
    // 只挂属于当前用户的行，防关系越权）
    let match_expr = rel_match_expr(&foreign_key, &let_var, is_array);
    let mut ands: Vec<Value> = vec![match_expr];
    if let Some(cond) = non_nullish(condition.as_ref()) {
        // 关系附加条件同样过拒绝名单（缺陷 D-02）
        validate_condition(cond)?;
        // U1~U4（D2）：关系级过滤同样只允许标量域，数组/对象字段与对象点号路径统一报错
        validate_condition_shape(rel_schema, cond)?;
        ands.push(cond.clone());
    }
    if let Some(owner) = merge_owner_condition(rel_schema, ctx, None) {
        ands.push(owner);
    }
    let match_doc = if ands.len() == 1 {
        ands.remove(0)
    } else {
        json!({ "$and": ands })
    };
    stages.push(json!({ "$match": match_doc }));

    // sort / skip / limit（优先执行，避免全量数据流入后续嵌套 $lookup）
    // 当 sort 依赖嵌套关联字段时，嵌套 $lookup 必须优先于 sort
    if let Some(srt) = non_nullish(sort.as_ref()) {
        // U4（D2）：关系级排序的对象点号路径统一报错（关系路径排序 R10 不受影响）
        validate_sort_shape(rel_schema, srt)?;
    }
    let sorts_by_nested = sort
        .as_ref()
        .and_then(|s| s.as_object())
        .map(|o| o.keys().any(|k| k.contains('.')))
        .unwrap_or(false);
    if !sorts_by_nested {
        append_order(
            &mut stages,
            sort.as_ref(),
            skip_val.as_ref(),
            limit_val.as_ref(),
        );
    }

    // 嵌套 relations
    stages.extend(ns_lookup_stages(
        rel_ast,
        rel_schema,
        params,
        depth,
        next_paginated,
        registry,
        ctx,
    )?);

    // sort / skip / limit（兜底：仅在嵌套 $lookup 未提前执行时追加）
    if sorts_by_nested {
        append_order(
            &mut stages,
            sort.as_ref(),
            skip_val.as_ref(),
            limit_val.as_ref(),
        );
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

/// 构建归一聚合计算列（§9.2(2)）阶段：`$lookup`（收集子行）+ `$addFields`（聚合标量）。
///
/// - Mongo 原生执行该形态；
/// - SQL 方言把同一形态翻译为「派生表 `LEFT JOIN (… GROUP BY fk)` + 标量列」
///   （见 `dialect/select/aggregate.rs`），**不出现 SQL 无法翻译的表达式**；
/// - 仅发射「被请求」且可读的计算列（权限裁剪 + 避免无谓 `$lookup`）；
/// - 子 pipeline 注入关系目标 owner 条件（越权防护），与前缀关系 `$lookup` 一致。
pub fn build_agg_stages(
    schema: &Schema,
    registry: &Registry,
    ctx: Option<&Context>,
    requested: &HashSet<String>,
) -> Result<Vec<Value>, String> {
    let readable = if ctx.is_some() {
        get_readable_computes(schema, ctx)
    } else {
        None
    };

    let mut stages: Vec<Value> = Vec::new();
    let mut add_fields = Map::new();
    for (key, comp) in &schema.computes {
        if !requested.contains(key) {
            continue;
        }
        // 权限裁剪：不可读的计算列不发射（不下发底层取数）
        if let Some(set) = &readable {
            if !set.contains(key) {
                continue;
            }
        }
        let Some(agg) = &comp.agg else { continue };
        let (op, path) = agg
            .as_object()
            .and_then(|o| o.iter().next())
            .ok_or_else(|| {
                format!("计算列 \"{key}\" 的 agg 定义非法（须为 {{\"$op\": \"<关系路径>\"}}）")
            })?;
        let path = path.as_str().unwrap_or("");
        let (rel_name, field) = match path.split_once('.') {
            Some((r, f)) => (r.to_string(), Some(f.to_string())),
            None => (path.to_string(), None),
        };
        let rel_def = schema.relations.get(&rel_name).ok_or_else(|| {
            format!("计算列 \"{key}\" 的 agg 引用的关系 \"{rel_name}\" 未在 schema 中定义")
        })?;
        let rel_schema = registry.get(&rel_def.model)?;

        // F6 / L6 / R0 决策 #3：计算列（含 agg 形态）的**依赖关系 / 子字段**不可读 → Err，
        // 不因「只是派生值」而放行（可读计算列但其依赖不可读仍拒绝）。
        if ctx.is_some() {
            let field_unreadable = field
                .as_deref()
                .map(|f| !is_field_readable(rel_schema, ctx, f))
                .unwrap_or(false);
            if !is_relation_readable(schema, ctx, &rel_name)
                || !can_read_schema(rel_schema, ctx)
                || field_unreadable
            {
                return Err(ERR_PERMISSION.to_string());
            }
        }
        let as_name = format!("_{}", key);

        // $lookup：外键匹配 + 目标 owner 注入（子行越权防护）
        let let_var = "aggv";
        let is_array = is_array_local_field(schema, &rel_def.local_field);
        let mut let_map = Map::new();
        let_map.insert(
            let_var.to_string(),
            rel_let_expr(&rel_def.local_field, is_array),
        );
        let mut ands: Vec<Value> = vec![rel_match_expr(&rel_def.foreign_field, let_var, is_array)];
        if let Some(owner) = merge_owner_condition(rel_schema, ctx, None) {
            ands.push(owner);
        }
        let match_doc = if ands.len() == 1 {
            ands.remove(0)
        } else {
            json!({ "$and": ands })
        };
        let mut inner = Map::new();
        inner.insert(
            "from".to_string(),
            Value::String(rel_schema.collection.clone()),
        );
        inner.insert("let".to_string(), Value::Object(let_map));
        inner.insert("pipeline".to_string(), json!([{ "$match": match_doc }]));
        inner.insert("as".to_string(), Value::String(as_name.clone()));
        stages.push(json!({ "$lookup": Value::Object(inner) }));

        // $addFields：聚合标量（空集语义 §9.7：$count → 0；$sum/$avg/$min/$max → null）
        let arr = json!({ "$ifNull": [format!("${}", as_name), []] });
        let field_ref = format!("${}.{}", as_name, field.clone().unwrap_or_default());
        match op.as_str() {
            "$count" => {
                add_fields.insert(key.clone(), json!({ "$size": arr }));
            }
            "$sum" => {
                // Mongo 对空数组的 $sum 为 0 → 显式守卫为 null（对齐 SQL LEFT JOIN 空集）
                add_fields.insert(
                    key.clone(),
                    json!({ "$cond": [{ "$eq": [{ "$size": arr }, 0] }, null, { "$sum": field_ref }] }),
                );
            }
            "$avg" => {
                add_fields.insert(key.clone(), json!({ "$avg": field_ref }));
            }
            "$min" => {
                add_fields.insert(key.clone(), json!({ "$min": field_ref }));
            }
            "$max" => {
                add_fields.insert(key.clone(), json!({ "$max": field_ref }));
            }
            other => {
                return Err(format!(
                    "计算列 \"{key}\" 的 agg 算子 {other} 不在白名单（$count/$sum/$avg/$min/$max）"
                ));
            }
        }
    }

    if !add_fields.is_empty() {
        stages.push(json!({ "$addFields": Value::Object(add_fields) }));
    }
    Ok(stages)
}
