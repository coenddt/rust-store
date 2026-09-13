//! 批量写路径：insertMany / updateMany。

use serde_json::{json, Value};

use crate::command::cmd::{cmd_insert_many, cmd_update_many};
use crate::command::write::build_insert_doc;
use crate::command::{ensure_context, ERR_NO_BATCH_WRITE, ERR_NO_WRITE};
use crate::computes::{apply_defaults_and_computes, FnRegistry};
use crate::permission::{can_write_schema, Context};
use crate::schema::Registry;
use crate::types::validate_condition;

use super::{build_raw_update, build_set_data, has_raw_operators, needs_new_id, IdCursor};

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
    if !can_write_schema(schema, ctx) {
        return Err(ERR_NO_WRITE.to_string());
    }

    let mut ids = IdCursor::new(new_ids);
    let mut processed = Vec::with_capacity(docs.len());
    for data in docs {
        // 仅需要生成 `_id` 的文档才消耗游标（对齐 JS `_generateId` 的调用次数）
        let new_id = if needs_new_id(schema, data) {
            ids.next()?
        } else {
            ""
        };
        let doc = build_insert_doc(schema, ctx, data, now, new_id)?;
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

/// 批量更新（对应 JS `updateMany`）。guest / 无写授权直接拒绝，不走 creator 探针。
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
    if let Some(c) = ctx {
        let guest = c
            .roles
            .clone()
            .unwrap_or_default()
            .iter()
            .any(|r| r == "guest");
        if guest || !can_write_schema(schema, ctx) {
            return Err(ERR_NO_BATCH_WRITE.to_string());
        }
    }

    let update_doc = if has_raw_operators(data) {
        build_raw_update(schema, ctx, data, now)
    } else {
        let mut set_data = build_set_data(schema, ctx, data);
        if schema.timestamps {
            set_data.insert("updatedAt".to_string(), json!(now));
        }
        json!({ "$set": Value::Object(set_data) })
    };

    let command = cmd_update_many(schema, condition, &update_doc);
    Ok(json!({ "command": command }))
}
