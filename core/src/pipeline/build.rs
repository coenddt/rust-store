//! 整条 pipeline 的编排（含 object 字段展平）

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::permission::Context;
use crate::schema::{Registry, Schema};
use crate::types::{is_truthy, validate_condition, validate_condition_shape, validate_sort_shape};

use super::ast::{Ast, RelAst};
use super::group;
use super::lookup::{build_agg_stages, build_lookup};
use super::relation_filter;
use super::util::{append_order, non_nullish, param};

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
    let root_group = param(params, ast.params.get("group")).cloned();
    let root_having = param(params, ast.params.get("having")).cloned();
    let root_sort = param(params, ast.params.get("sort")).cloned();
    let root_skip = param(params, ast.params.get("skip")).cloned();
    let root_limit = param(params, ast.params.get("limit")).cloned();

    // U1~U4（D2）：数组/对象字段过滤、对象点号路径过滤/排序 —— 所有后端（含 Mongo）
    // 规划期统一显式报错，绝不静默（判定依据 = 根 schema）。
    if let Some(cond) = non_nullish(root_condition.as_ref()) {
        validate_condition_shape(schema, cond)?;
        // 拒绝名单（$where 等）提前校验；关系谓词解析（§9.6）内部亦复用
        validate_condition(cond)?;
    }

    // §9.6 关系聚合谓词（跨表条件过滤 / semi-join）：判定依据 = schema ——
    // `$condition` 键命中 `schema.relations` 的关系名 → 谓词；命中 array/object 字段 → U1/U2 Err。
    // 无关系谓词 → `None`，既有标量路径逐字节不变。
    let rel_plan = match non_nullish(root_condition.as_ref()) {
        Some(cond) => relation_filter::plan(schema, registry, ctx, cond)?,
        None => None,
    };

    // ── 根级 `$group`：成组聚合终结路径（§9.2(1)） ──
    // 固定执行序（§9.3）：$condition(WHERE) → $group → $having → $sort/$skip/$limit → 投影。
    // 该路径独立成组构建（不与关系下推 / 计算列聚合混用），阶段数组由 group 模块产出。
    if let Some(group_v) = non_nullish(root_group.as_ref()) {
        if rel_plan.is_some() {
            return Err(
                "$group 查询暂不支持关系聚合谓词（§9.6 属 $condition 段，与分组终结路径暂未互通）"
                    .to_string(),
            );
        }
        let spec = group::parse(schema, group_v)?;
        // F2：by 键 / agg 引用字段必须过 field.read（含 $having 背后的引用字段）
        group::validate_read_permission(schema, ctx, &spec)?;
        let stages = group::build_stages(
            &spec,
            &ast.fields,
            ast.relations.is_empty(),
            non_nullish(root_condition.as_ref()),
            non_nullish(root_having.as_ref()),
            non_nullish(root_sort.as_ref()),
            non_nullish(root_skip.as_ref()),
            non_nullish(root_limit.as_ref()),
        )?;
        return Ok(Value::Array(stages));
    }
    if non_nullish(root_having.as_ref()).is_some() {
        return Err("$having 需要与 $group 同时使用（无 $group 时没有分组结果可过滤）".to_string());
    }

    if let Some(srt) = non_nullish(root_sort.as_ref()) {
        validate_sort_shape(schema, srt)?;
    }

    // ── 标准 GQL 模式 ──
    // 展平 object 子字段花括号语法 → dot-notation
    flatten_object_fields(ast, schema);

    if let Some(plan) = &rel_plan {
        // 关系谓词 `$lookup` 必须位于改写后的 `$match` 之前（semi-join：不扇出、父行形状不变）
        stages.extend(plan.lookups.iter().cloned());
        stages.push(json!({ "$match": plan.condition.clone() }));
    } else if let Some(cond) = non_nullish(root_condition.as_ref()) {
        // 条件拒绝名单校验（缺陷 D-02：$where 等载荷显式报错，绝不静默传递）已在上方统一完成
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

    // 归一聚合计算列（§9.2(2)）：仅在被请求时发射 `$lookup` + `$addFields`，
    // 避免无谓 join；SQL 方言把该形态翻译为派生表 LEFT JOIN（见 dialect/select/aggregate.rs）。
    let requested_computes: HashSet<String> = schema
        .computes
        .iter()
        .map(|(k, _)| k.clone())
        .filter(|k| {
            ast.fields
                .iter()
                .any(|f| f == k || f.starts_with(&format!("{}.", k)))
        })
        .collect();
    stages.extend(build_agg_stages(
        schema,
        registry,
        ctx,
        &requested_computes,
    )?);

    Ok(Value::Array(stages))
}
