//! 整条 pipeline 的编排（含 object 字段展平、自定义 pipeline 分支）

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::permission::Context;
use crate::schema::{Registry, Schema};
use crate::types::{is_truthy, validate_condition};

use super::ast::{Ast, RelAst};
use super::lookup::{build_add_fields, build_compute_lookup_stages, build_lookup};
use super::util::{append_order, find_stage_idx, non_nullish, param};

/// 归一化 AST：将 type=object 的花括号子字段展平为点号字段（原地修改）
pub fn flatten_object_fields(ast: &mut Ast, schema: &Schema) {
    flatten_object_fields_impl(&mut ast.fields, &mut ast.relations, schema);
}

/// 共享实现：根 AST 与关系节点（`RelAst`）复用同一套展平规则
pub(crate) fn flatten_object_fields_impl(
    fields: &mut Vec<String>,
    relations: &mut Vec<(String, RelAst)>,
    schema: &Schema,
) {
    let mut removed: Vec<String> = Vec::new();
    for (rel_name, rel_ast) in relations.iter() {
        let is_object_field = schema
            .fields
            .get(rel_name)
            .map(|f| f.field_type == "object" && f.fields.as_ref().map(is_truthy).unwrap_or(false))
            .unwrap_or(false);
        if !is_object_field {
            continue;
        }
        fields.extend(
            rel_ast
                .fields
                .iter()
                .map(|sub| format!("{}.{}", rel_name, sub)),
        );
        removed.push(rel_name.clone());
    }
    relations.retain(|(n, _)| !removed.contains(n));
}

/// 若 stages 已含该类 stage 则覆盖，否则追加（自定义 pipeline 模式）
fn override_or_append(stages: &mut Vec<Value>, stage_key: &str, value: Value) {
    let stage = Value::Object(Map::from_iter([(stage_key.to_string(), value)]));
    match find_stage_idx(stages, stage_key) {
        Some(idx) => stages[idx] = stage,
        None => stages.push(stage),
    }
}

/// 自定义 pipeline 模式：延展用户 pipeline 并用根参数覆盖/追加排序分页
fn custom_pipeline_branch(
    stages: &mut Vec<Value>,
    root_pipeline: &Value,
    root_condition: Option<&Value>,
    root_sort: Option<&Value>,
    root_skip: Option<&Value>,
    root_limit: Option<&Value>,
) -> Result<Value, String> {
    if let Some(arr) = root_pipeline.as_array() {
        stages.extend(arr.iter().cloned());
    }
    if let Some(v) = non_nullish(root_condition) {
        // 根 $condition 覆盖同样不允许携带拒绝名单操作符（缺陷 D-02）
        validate_condition(v)?;
        // 缺陷修复：$condition 与 $sort/$skip/$limit 同为根参数，按「覆盖/追加」
        // 语义应用到 pipeline（与 JS 参考实现对拍一致），否则用户 $match 保留、
        // 根条件被静默丢弃
        override_or_append(stages, "$match", v.clone());
    }
    if let Some(v) = non_nullish(root_sort) {
        override_or_append(stages, "$sort", v.clone());
    }
    if let Some(v) = non_nullish(root_skip) {
        override_or_append(stages, "$skip", v.clone());
    }
    if let Some(v) = non_nullish(root_limit) {
        override_or_append(stages, "$limit", v.clone());
    }
    Ok(Value::Array(std::mem::take(stages)))
}

/// 标准 GQL：逐层展开根 relations 为 $lookup（one 关系附加 $unwind）
fn root_lookup_stages(
    ast: &Ast,
    schema: &Schema,
    params: &Map<String, Value>,
    stages: &mut Vec<Value>,
    registry: &Registry,
    ctx: Option<&Context>,
) -> Result<(), String> {
    for (rel_name, rel_ast) in &ast.relations {
        let rel_def = schema.relations.get(rel_name).ok_or_else(|| {
            format!(
                "关系 \"{}\" 未在 schema \"{}\" 中定义",
                rel_name, schema.name
            )
        })?;
        let rel_schema = registry.get(&rel_def.model)?;
        stages.push(build_lookup(
            rel_name, rel_ast, params, rel_def, rel_schema, schema, 0, 0, registry, ctx,
        )?);
        if rel_def.rel_type == "one" {
            stages.push(json!({
                "$unwind": { "path": format!("${}", rel_name), "preserveNullAndEmptyArrays": true }
            }));
        }
    }
    Ok(())
}

/// 从 AST 构建 aggregate pipeline
pub fn build_pipeline(
    ast: &mut Ast,
    params: &Map<String, Value>,
    registry: &Registry,
    ctx: Option<&Context>,
) -> Result<Value, String> {
    let schema = registry.get(&ast.model)?;
    let mut stages: Vec<Value> = Vec::new();

    let root_condition = param(params, ast.params.get("condition")).cloned();
    let root_sort = param(params, ast.params.get("sort")).cloned();
    let root_skip = param(params, ast.params.get("skip")).cloned();
    let root_limit = param(params, ast.params.get("limit")).cloned();
    let root_pipeline = param(params, ast.params.get("pipeline")).cloned();

    if let Some(pipe) = non_nullish(root_pipeline.as_ref()).filter(|v| v.is_array()) {
        return custom_pipeline_branch(
            &mut stages,
            pipe,
            root_condition.as_ref(),
            root_sort.as_ref(),
            root_skip.as_ref(),
            root_limit.as_ref(),
        );
    }

    // ── 标准 GQL 模式 ──
    // 展平 object 子字段花括号语法 → dot-notation
    flatten_object_fields(ast, schema);

    if let Some(cond) = non_nullish(root_condition.as_ref()) {
        // 条件拒绝名单校验（缺陷 D-02：$where 等载荷显式报错，绝不静默传递）
        validate_condition(cond)?;
        stages.push(json!({ "$match": cond }));
    }

    // $lookup: 逐层展开 relations
    root_lookup_stages(ast, schema, params, &mut stages, registry, ctx)?;

    // sort / skip / limit（根级别）
    append_order(
        &mut stages,
        root_sort.as_ref(),
        root_skip.as_ref(),
        root_limit.as_ref(),
    );

    // $lookup: compute 独立的 $lookup 阶段（在 $addFields 之前）。
    // 仅在字段被请求时发射 lookup 计算列，避免无谓 join 与 SQL 不可翻译的 $addFields。
    let requested_computes: HashSet<String> = schema
        .computes
        .iter()
        .map(|(k, _)| k.clone())
        .filter(|k| ast.fields.iter().any(|f| f == k || f.starts_with(&format!("{}.", k))))
        .collect();
    stages.extend(build_compute_lookup_stages(schema, &requested_computes));

    // $addFields: lookup 计算列（放在最后，确保所有 $lookup 字段已就绪）
    if let Some(add_fields) = build_add_fields(schema, ctx, &requested_computes) {
        stages.push(add_fields);
    }

    Ok(Value::Array(stages))
}
