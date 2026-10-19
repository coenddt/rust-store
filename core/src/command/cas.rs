//! compareAndSet 命令规划：比较并写原语（抢占 / 乐观锁 / leader 选举底座）。
//!
//! 语义：对 filter 命中的行写入 update（典型用法是把「版本等值」放进 filter、
//! 把「版本 +1」放进 update——比较与写在同一条 UPDATE 内原子完成）。
//! matched（命中行数）语义由 Host 从驱动 rowCount / `changes()` 读出（三方言
//! 口径差异与对齐手段见宿主门面文档），core 只产出命令，不做 IO。
//!
//! 单命令也走 `atomic_commands` mode：宿主统一在事务包裹内执行——leader 选举
//! 场景「判 matched + 写续约时间」必须同事务（见文档二 leader_elector 语义）。
//!
//! 计划形状（Host 契约）：
//!
//! ```json
//! { "mode": "atomic_commands", "schema": "Leader",
//!   "commands": [
//!     { "kind": "compareAndSet", "source": "...", "database": null,
//!       "schema": null, "collection": "leader",
//!       "filter": {...}, "update": {...}, "nowStamp": 0 }
//!   ] }
//! ```
//!
//! MongoDB 无 CAS 方言路径：Host 的 Mongo 执行器对 `kind: "compareAndSet"`
//! 显式拒绝（宿主侧抛 PushdownUnsupportedError），core 不产出该路径。

use serde_json::{json, Value};

use crate::command::cmd::cmd_compare_and_set;
use crate::command::mutate::{build_raw_update, build_set_data, has_raw_operators};
use crate::command::write_links::attach_write_links;
use crate::command::{ensure_context, forbid_t2q, ERR_NO_BATCH_WRITE, MODE_ATOMIC_COMMANDS};
use crate::permission::{can_write_schema, Context};
use crate::rbac::{ensure_write, merge_row_condition, WriteAction};
use crate::schema::Registry;
use crate::types::{validate_condition, validate_condition_shape};

use super::claim::ensure_fields_declared;

/// compareAndSet 计划：对 filter 命中的行原子写入 update。
///
/// `now` 由 Host 注入（core 无时钟）；filter 为空对象时放行（语义 = 匹配全部行，
/// 调用方自担风险；宿主门面文档须警示），与既有 filter 翻译的空对象行为一致。
pub fn plan_compare_and_set(
    schema_name: &str,
    registry: &Registry,
    ctx: Option<&Context>,
    filter: &Value,
    update: &Value,
    now: i64,
) -> Result<Value, String> {
    ensure_context(registry, ctx)?;
    forbid_t2q(registry, "compareAndSet")?;
    let schema = registry.get(schema_name)?;
    validate_condition(filter)?;
    validate_condition_shape(schema, filter, registry.profile())?;
    if update.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        return Err("compareAndSet 的 update 必须为非空对象（无条件写入属编程错误）".to_string());
    }
    // 拒写清单 + 静态写权限（updateMany 同款：不走 creator 探针）
    if let Some(c) = ctx {
        let deny_write =
            crate::permission::has_any_role(c, &registry.role_rules().deny_write_roles);
        if deny_write || !can_write_schema(registry.role_rules(), schema, ctx) {
            return Err(ERR_NO_BATCH_WRITE.to_string());
        }
    }
    ensure_write(registry, schema, ctx, WriteAction::Update)?;
    let eff_condition = merge_row_condition(registry, schema, ctx, "update", Some(filter.clone()))
        .unwrap_or_else(|| json!({}));

    let update_doc = if has_raw_operators(update) {
        build_raw_update(registry, schema, ctx, update, now)
    } else {
        let mut set_data = build_set_data(registry, schema, ctx, update);
        if schema.timestamps {
            set_data.insert("updatedAt".to_string(), json!(now));
        }
        json!({ "$set": Value::Object(set_data) })
    };
    ensure_fields_declared(schema, &update_doc)?;

    let cmd = cmd_compare_and_set(schema, &eff_condition, &update_doc, now);
    let plan = json!({
        "mode": MODE_ATOMIC_COMMANDS,
        "schema": schema_name,
        "commands": [cmd],
    });
    attach_write_links(plan, registry, &[schema_name], registry.write_link_policy())
}
