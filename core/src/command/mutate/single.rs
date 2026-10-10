//! 单条写路径：update / remove（归档）/ archiveDocs。

use serde_json::{json, Value};

use crate::command::cmd::{cmd_delete_many, cmd_find, cmd_find_one_and_update, cmd_insert_many};
use crate::command::write::{check_write_perm, Probe};
use crate::command::write_links::attach_write_links;
use crate::command::{ensure_context, ERR_NO_BATCH_WRITE, ERR_NO_DELETE, ERR_NO_WRITE};
use crate::permission::Context;
use crate::rbac::WriteAction;
use crate::schema::Registry;
use crate::types::{validate_condition, validate_condition_shape};

use super::{
    build_raw_update, build_set_data, find_one_and_update_options, has_raw_operators,
    is_blank_condition, object_of,
};

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
    // 条件拒绝名单（缺陷 D-02）：update/remove 的条件绝不静默携带服务端执行操作符
    validate_condition(condition)?;
    // §11.4（D2）：写路径条件与读路径同码拒绝 U1~U4 形态（数组/对象/点号路径）
    validate_condition_shape(schema, condition, registry.profile())?;
    // 触发器探针字段：onFields ∪ {{before.*}} 引用（update 事件；Host 据探针结果
    // 判定字段级命中并替换 before 占位符 —— A3/A4）
    let extra = crate::command::triggers::before_probe_fields(
        schema
            .triggers
            .get("update")
            .map(|v| v.as_slice())
            .unwrap_or(&[]),
    );
    if let Some(cmd) = check_write_perm(
        registry,
        schema,
        ctx,
        condition,
        ERR_NO_WRITE,
        probe,
        WriteAction::Update,
        &extra,
    )? {
        return Ok(json!({ "needsProbe": cmd }));
    }

    let update_doc = if has_raw_operators(data) {
        build_raw_update(registry, schema, ctx, data, now)
    } else {
        let mut set_data = build_set_data(registry, schema, ctx, data);
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
    // 触发链展开（update 事件；text2query 档下含触发器即 Err —— A9）
    let triggers = crate::command::triggers::expand_triggers(registry, ctx, schema_name, "update")?;
    let mut plan = json!({ "command": command });
    // 未配置触发器时不出现 triggers 键（A2 零回归）
    if !triggers.is_empty() {
        plan.as_object_mut()
            .expect("plan_update：plan 必为对象")
            .insert("triggers".to_string(), json!(triggers));
    }
    // 写链路附接（仅 `{command}` 分支；`{needsProbe}` 分支尚未执行写，不接线）
    attach_write_links(plan, registry, &[schema_name], registry.write_link_policy())
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
    // 条件拒绝名单（缺陷 D-02）：remove 条件命中拒绝名单即显式报错
    validate_condition(condition)?;
    // §11.4（D2）：写路径条件与读路径同码拒绝 U1~U4 形态（数组/对象/点号路径）
    validate_condition_shape(schema, condition, registry.profile())?;
    // R4/B-12：空条件（{} / null / 空逻辑组）批量删除一票否决，绝不落全表
    if is_blank_condition(condition) {
        return Err(ERR_NO_BATCH_WRITE.to_string());
    }
    if let Some(cmd) = check_write_perm(
        registry,
        schema,
        ctx,
        condition,
        ERR_NO_DELETE,
        probe,
        WriteAction::Remove,
        &[],
    )? {
        return Ok(json!({ "needsProbe": cmd }));
    }

    // 触发链展开（remove 事件；text2query 档下含触发器即 Err —— A9）。
    // before 值来源 = 归档 findCommand 的完整文档（Host 端替换 `{{before.*}}`），
    // 故 findCommand 投影保持全字段（None）——归档需完整文档，extra 字段天然包含，
    // 不收窄投影（收窄会丢归档字段）。
    let remove_triggers =
        crate::command::triggers::expand_triggers(registry, ctx, schema_name, "remove")?;
    // 关系谓词条件（阶段1 T1-04）：归一为 preCommand（aggregate 取命中 `_id`）+ `_id $in`。
    // 归档 find 也用改写后条件（`_id $in` 标量条件，Mongo/SQL 双侧直接可执行）。
    let has_rel_pred = condition
        .as_object()
        .map(|m| m.keys().any(|k| schema.relations.contains_key(k)))
        .unwrap_or(false);
    let eff_condition = if has_rel_pred {
        let pre = super::plan_rel_pred_mutation(schema, registry, ctx, condition)?;
        let mut delete_command = cmd_delete_many(schema, &pre.condition);
        if let Some(obj) = delete_command.as_object_mut() {
            obj.insert("preCommand".to_string(), pre.lookup_command);
        }
        let mut plan = json!({
            "archiveCollection": if registry.has(&archive_name_of(schema_name)) { json!(registry.get(&archive_name_of(schema_name))?.collection) } else { Value::Null },
            "findCommand": cmd_find(schema, &pre.condition, None),
            "deleteCommand": delete_command,
        });
        // 未配置触发器时不出现 triggers 键（A2 零回归）
        if !remove_triggers.is_empty() {
            plan.as_object_mut()
                .expect("plan_remove：plan 必为对象")
                .insert("triggers".to_string(), json!(remove_triggers));
        }
        return attach_write_links(plan, registry, &[schema_name], registry.write_link_policy());
    } else {
        condition
    };

    let archive_name = format!("{}Deleted", schema_name);
    let (archive_collection, find_command) = if registry.has(&archive_name) {
        let arch = registry.get(&archive_name)?;
        (
            json!(arch.collection),
            Some(cmd_find(schema, eff_condition, None)),
        )
    } else {
        (Value::Null, None)
    };
    let delete_command = cmd_delete_many(schema, eff_condition);
    let mut plan = json!({
        "archiveCollection": archive_collection,
        "findCommand": find_command,
        "deleteCommand": delete_command,
    });
    // 未配置触发器时不出现 triggers 键（A2 零回归）
    if !remove_triggers.is_empty() {
        plan.as_object_mut()
            .expect("plan_remove：plan 必为对象")
            .insert("triggers".to_string(), json!(remove_triggers));
    }
    attach_write_links(plan, registry, &[schema_name], registry.write_link_policy())
}

/// 归档表 schema 名（`<schema>Deleted`；plan_remove 主体与关系谓词分支共用）
fn archive_name_of(schema_name: &str) -> String {
    format!("{}Deleted", schema_name)
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
