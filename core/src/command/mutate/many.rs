//! 批量写路径：insertMany / updateMany。

use serde_json::{json, Value};

use crate::command::cmd::{cmd_insert_many, cmd_update_many};
use crate::command::write::build_insert_doc;
use crate::command::{ensure_context, ERR_NO_BATCH_WRITE, ERR_NO_WRITE};
use crate::computes::{apply_defaults_and_computes, FnRegistry};
use crate::permission::{can_write_schema, Context};
use crate::rbac::{ensure_write, merge_row_condition, WriteAction};
use crate::schema::Registry;
use crate::types::{validate_condition, validate_condition_shape};

use super::{
    build_raw_update, build_set_data, has_raw_operators, is_blank_condition, needs_new_id,
    plan_rel_pred_mutation, IdCursor,
};

/// 批量插入（对应 JS `insertMany`）。
///
/// 需要生成 `_id` 的文档按顺序消费 `new_ids`（与 JS `_generateId` 的调用顺序一致）；
/// `new_ids` 不足时报错，Host 应按数据规模多备一些（剩余忽略）。
pub fn plan_insert_many(
    schema_name: &str,
    registry: &Registry,
    ctx: Option<&Context>,
    docs: &[Value],
    now: i64,
    new_ids: &[String],
    fn_registry: Option<&dyn FnRegistry>,
) -> Result<Value, String> {
    ensure_context(registry, ctx)?;
    // JS：非数组/空数组直接返回 []，不产生命令
    if docs.is_empty() {
        return Ok(json!({ "command": Value::Null, "returns": [] }));
    }
    let schema = registry.get(schema_name)?;
    if !can_write_schema(registry.role_rules(), schema, ctx) {
        return Err(ERR_NO_WRITE.to_string());
    }
    // RBAC 表级写判定（deny-wins，叠加于静态白名单之后）
    ensure_write(registry, schema, ctx, WriteAction::Insert)?;

    let mut ids = IdCursor::new(new_ids);
    let mut processed = Vec::with_capacity(docs.len());
    for data in docs {
        // 仅需要生成 `_id` 的文档才消耗游标（对齐 JS `_generateId` 的调用次数）
        let new_id = if needs_new_id(schema, data) {
            ids.next()?
        } else {
            ""
        };
        let doc = build_insert_doc(registry, schema, ctx, data, now, new_id)?;
        processed.push(Value::Object(doc));
    }
    let returns = processed
        .iter()
        .map(|d| apply_defaults_and_computes(d, schema, fn_registry))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({
        "command": cmd_insert_many(schema, &processed),
        "returns": returns,
    }))
}

/// 批量更新（对应 JS `updateMany`）。拒写清单命中 / 无写授权直接拒绝，不走 creator 探针。
pub fn plan_update_many(
    schema_name: &str,
    registry: &Registry,
    ctx: Option<&Context>,
    condition: &Value,
    data: &Value,
    now: i64,
) -> Result<Value, String> {
    ensure_context(registry, ctx)?;
    let schema = registry.get(schema_name)?;
    // 条件拒绝名单（缺陷 D-02）：updateMany 条件命中拒绝名单即显式报错
    validate_condition(condition)?;
    // §11.4（D2）：写路径条件与读路径同码拒绝 U1~U4 形态（数组/对象/点号路径）
    validate_condition_shape(schema, condition, registry.profile())?;
    // R4：无条件批量写一票否决（不落全表）
    if is_blank_condition(condition) {
        return Err(ERR_NO_BATCH_WRITE.to_string());
    }
    if let Some(c) = ctx {
        // 拒写清单（默认空——无拒写；原 guest 硬编码随清单化移除，设计 §11.4）
        let deny_write =
            crate::permission::has_any_role(c, &registry.role_rules().deny_write_roles);
        if deny_write || !can_write_schema(registry.role_rules(), schema, ctx) {
            return Err(ERR_NO_BATCH_WRITE.to_string());
        }
    }
    // RBAC 表级写判定（deny-wins）+ 行级条件合并（updateMany 无探针通道，
    // ownerOnly / condition 并入 filter——匹配不到即 0 行，与读路径语义一致）
    ensure_write(registry, schema, ctx, WriteAction::Update)?;
    let eff_condition =
        merge_row_condition(registry, schema, ctx, "update", Some(condition.clone()))
            .unwrap_or_else(|| json!({}));

    let update_doc = if has_raw_operators(data) {
        build_raw_update(registry, schema, ctx, data, now)
    } else {
        let mut set_data = build_set_data(registry, schema, ctx, data);
        if schema.timestamps {
            set_data.insert("updatedAt".to_string(), json!(now));
        }
        json!({ "$set": Value::Object(set_data) })
    };

    // 关系谓词条件（阶段1 T1-02）：条件含 schema 关系名 → 归一为
    // preCommand（aggregate 取命中 `_id`）+ `_id $in` 改写条件。
    // SQL 侧 preCommand 翻译为 EXISTS（§9.6 下推），主命令 `_id $in` 是标量条件；
    // Mongo 侧两段原生命令直接可执行 —— 修复原「关系谓词被静默忽略为 no-op」。
    if condition
        .as_object()
        .map(|m| m.keys().any(|k| schema.relations.contains_key(k)))
        .unwrap_or(false)
    {
        let pre = plan_rel_pred_mutation(schema, registry, ctx, &eff_condition)?;
        let rewritten = pre.condition;
        let mut command = cmd_update_many(schema, &rewritten, &update_doc);
        if let Some(obj) = command.as_object_mut() {
            obj.insert("preCommand".to_string(), pre.lookup_command);
        }
        return Ok(json!({ "command": command }));
    }

    let command = cmd_update_many(schema, &eff_condition, &update_doc);
    Ok(json!({ "command": command }))
}
