//! 单条写路径：update / remove（归档）/ archiveDocs。

use serde_json::{json, Value};

use crate::command::cmd::{
    cmd_delete_many, cmd_find, cmd_find_one_and_update, cmd_insert_many,
};
use crate::command::write::{check_write_perm, Probe};
use crate::command::{ensure_context, ERR_NO_DELETE, ERR_NO_WRITE};
use crate::permission::Context;
use crate::schema::Registry;

use super::{build_raw_update, build_set_data, find_one_and_update_options, has_raw_operators, object_of};

/// 更新一条（对应 JS `update`，`findOneAndUpdate` + returnDocument AFTER）。
///
/// creator 写权限需探针时返回 `{"needsProbe": cmd}`；Host 执行探针后携
/// [`Probe::Found`] / [`Probe::NoResult`] 重入即得 `{"command": cmd}`。
/// `options` 为用户透传选项（如 upsert / arrayFilters）。
///
/// 签名与 JS 参考实现逐一对应（跨语言 parity 优先于参数个数），保持位置参数。
#[allow(clippy::too_many_arguments)]
pub fn plan_update(
    schema_name: &str,
    registry: &Registry,
    ctx: Option<&Context>,
    condition: &Value,
    data: &Value,
    options: &Value,
    now: i64,
    probe: Probe,
) -> Result<Value, String> {
    ensure_context(registry, ctx)?;
    let schema = registry.get(schema_name)?;
    if let Some(cmd) = check_write_perm(schema, ctx, condition, ERR_NO_WRITE, probe)? {
        return Ok(json!({ "needsProbe": cmd }));
    }

    let update_doc = if has_raw_operators(data) {
        build_raw_update(schema, ctx, data, now)
    } else {
        let mut set_data = build_set_data(schema, ctx, data);
        // JS：空字段检查在追加时间戳之前
        if set_data.is_empty() {
            return Err("没有提供要更新的字段".to_string());
        }
        if schema.timestamps {
            set_data.insert("updatedAt".to_string(), json!(now));
        }
        json!({ "$set": Value::Object(set_data) })
    };

    let command = cmd_find_one_and_update(
        schema,
        condition,
        &update_doc,
        &find_one_and_update_options(options),
    );
    Ok(json!({ "command": command }))
}

/// 删除计划（对应 JS `remove`）：
/// 归档表存在时返回 find 命令（Host 取源文档后调 [`plan_archive_docs`]）+ deleteMany 命令。
pub fn plan_remove(
    schema_name: &str,
    registry: &Registry,
    ctx: Option<&Context>,
    condition: &Value,
    probe: Probe,
) -> Result<Value, String> {
    ensure_context(registry, ctx)?;
    let schema = registry.get(schema_name)?;
    if let Some(cmd) = check_write_perm(schema, ctx, condition, ERR_NO_DELETE, probe)? {
        return Ok(json!({ "needsProbe": cmd }));
    }

    let archive_name = format!("{}Deleted", schema_name);
    let (archive_collection, find_command) = if registry.has(&archive_name) {
        let arch = registry.get(&archive_name)?;
        (
            json!(arch.collection),
            Some(cmd_find(schema, condition, None)),
        )
    } else {
        (Value::Null, None)
    };
    let delete_command = cmd_delete_many(schema, condition);
    Ok(json!({
        "archiveCollection": archive_collection,
        "findCommand": find_command,
        "deleteCommand": delete_command,
    }))
}

/// 归档文档命令：源文档补 `deletedAt` 后批量写入 `<collection>_deleted`
/// （对应 JS `remove` 内的归档段；Host 仅在 find 有结果时调用）。
pub fn plan_archive_docs(
    schema_name: &str,
    registry: &Registry,
    docs: &[Value],
    now: i64,
) -> Result<Value, String> {
    let archive_name = format!("{}Deleted", schema_name);
    let arch = registry.get(&archive_name)?;
    let archived: Vec<Value> = docs
        .iter()
        .map(|d| {
            let mut o = object_of(d);
            o.insert("deletedAt".to_string(), json!(now));
            Value::Object(o)
        })
        .collect();
    // upsert-by-_id：归档幂等 —— 「归档成功但删除失败」的重试不再因 _id 冲突
    // 整批失败（SQL: ON CONFLICT DO UPDATE / ON DUPLICATE KEY / INSERT OR REPLACE；
    // Mongo: 逐条 replaceOne upsert，见两端 exec 的 insertMany 分支）
    let mut command = cmd_insert_many(arch, &archived);
    if let Some(obj) = command.as_object_mut() {
        obj.insert("upsertById".to_string(), json!(true));
    }
    Ok(json!({ "command": command }))
}
