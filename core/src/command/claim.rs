//! claim 命令规划：并发领取原语（outbox / 任务分派的底层原语）。
//!
//! 语义：在 filter 命中的行中按 sort 取前 limit 行，**原子**写入领取标记（update），
//! 并返回被领取的完整文档。原子性由 SQL 侧行锁（`FOR UPDATE SKIP LOCKED`）或
//! 单写者（SQLite）在方言翻译层保证（见 `dialect::write::claim`）；core 只产出
//! 命令序列，不做 IO。
//!
//! 计划形状（Host 契约）：
//!
//! ```json
//! { "mode": "atomic_commands", "schema": "Outbox",
//!   "commands": [
//!     { "kind": "claim", "source": "...", "database": null, "schema": null,
//!       "collection": "outbox", "filter": {...}, "sort": [["_id", 1]],
//!       "limit": 10, "update": {...}, "nowStamp": 0 },
//!     { "kind": "find", "source": "...", "database": null, "schema": null,
//!       "collection": "outbox", "filter": { "_id": { "$in": "___claimed_ids___" } },
//!       "projection": null, "placeholder": "___claimed_ids___" }
//!   ] }
//! ```
//!
//! `atomic_commands`：全部 commands 必须在**同一连接同一事务**内顺序执行；前一条
//! `claim` 的结果集 `_id` 数组填充后一条的 [`CLAIMED_IDS`] 占位符（整值替换，Host
//! 契约同 `{{phase1.ids}}`）。ids 为空（0 行命中）时 Host 跳过后续命令、直接返回
//! 空集。mode 为新增值，不与既有 `two_phase` 语义重叠。
//!
//! MongoDB 无 claim 方言路径：Host 的 Mongo 执行器对 `kind: "claim"` 显式拒绝
//! （宿主侧抛 PushdownUnsupportedError），core 不产出该路径。

use serde_json::{json, Value};

use crate::command::cmd::{cmd_claim, cmd_find};
use crate::command::mutate::{build_raw_update, build_set_data, has_raw_operators};
use crate::command::write_links::attach_write_links;
use crate::command::{
    ensure_context, forbid_t2q, CLAIMED_IDS, ERR_NO_BATCH_WRITE, MODE_ATOMIC_COMMANDS,
};
use crate::permission::{can_write_schema, Context};
use crate::rbac::{ensure_write, merge_row_condition, WriteAction};
use crate::schema::{Registry, Schema};
use crate::types::{validate_condition, validate_condition_shape};

/// claim 计划：领取 filter 命中的前 limit 行并写入领取标记。
///
/// `now` 由 Host 注入（core 无时钟与随机源）；`sort` 缺省 `[["_id", 1]]`（FIFO）；
/// `limit <= 0` 显式拒绝（禁静默取 1——领取语义下静默改值会掩盖调用方错误）。
#[allow(clippy::too_many_arguments)]
pub fn plan_claim(
    schema_name: &str,
    registry: &Registry,
    ctx: Option<&Context>,
    filter: &Value,
    update: &Value,
    sort: Option<&Value>,
    limit: i64,
    now: i64,
) -> Result<Value, String> {
    ensure_context(registry, ctx)?;
    // 并发原语属 DB 独有能力：text2query 档收缩（同 triggers / $pipeline 直通先例）
    forbid_t2q(registry, "claim")?;
    let schema = registry.get(schema_name)?;
    validate_condition(filter)?;
    validate_condition_shape(schema, filter, registry.profile())?;
    if limit <= 0 {
        return Err(format!(
            "claim 的 limit 必须 > 0，实际 {limit}（禁静默取 1）"
        ));
    }
    if update.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        return Err("claim 的 update 必须为非空对象（领取必须写入标记）".to_string());
    }
    // 拒写清单 + 静态写权限（updateMany 同款：不走 creator 探针）
    if let Some(c) = ctx {
        let deny_write =
            crate::permission::has_any_role(c, &registry.role_rules().deny_write_roles);
        if deny_write || !can_write_schema(registry.role_rules(), schema, ctx) {
            return Err(ERR_NO_BATCH_WRITE.to_string());
        }
    }
    // RBAC 表级写判定（deny-wins）+ 行级条件合并（与 updateMany 语义一致：
    // ownerOnly / condition 并入 filter，匹配不到即 0 行）
    ensure_write(registry, schema, ctx, WriteAction::Update)?;
    let eff_condition =
        merge_row_condition(registry, schema, ctx, "update", Some(filter.clone()))
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

    let sort_v = sort.cloned().unwrap_or_else(|| json!([["_id", 1]]));
    let claim_cmd = cmd_claim(schema, &eff_condition, &sort_v, limit, &update_doc, now);
    let mut find_cmd = cmd_find(
        schema,
        &json!({ "_id": { "$in": CLAIMED_IDS } }),
        None,
    );
    if let Some(o) = find_cmd.as_object_mut() {
        o.insert("placeholder".to_string(), json!(CLAIMED_IDS));
    }
    let plan = json!({
        "mode": MODE_ATOMIC_COMMANDS,
        "schema": schema_name,
        "commands": [claim_cmd, find_cmd],
    });
    attach_write_links(plan, registry, &[schema_name], registry.write_link_policy())
}

/// update_doc 中的目标字段必须为 schema 已声明字段（豁免 `_id` 与 timestamps 自动列）。
///
/// claim 是宿主编程原语：未知列名属编程错误，规划期显式拒绝（不静默透传、
/// 不依赖驱动层报错——D2 绝不静默）。
pub(super) fn ensure_fields_declared(schema: &Schema, update_doc: &Value) -> Result<(), String> {
    let exempt = |f: &str| {
        f == "_id" || (schema.timestamps && (f == "createdAt" || f == "updatedAt"))
    };
    let Some(ops) = update_doc.as_object() else {
        return Ok(());
    };
    for (op, body) in ops {
        // 仅校验已知写操作符的字段体；其余（非法操作符 / 混合形态）由方言翻译层显式报错
        if !matches!(op.as_str(), "$set" | "$inc" | "$unset") {
            continue;
        }
        let Some(fields) = body.as_object() else {
            continue;
        };
        for k in fields.keys() {
            if !exempt(k) && !schema.fields.contains_key(k) {
                return Err(format!(
                    "并发原语（claim/compareAndSet）的 update 字段 \"{k}\" 未在 schema 声明（D2：规划期拒绝，不落库报错）"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::plan_compare_and_set;
    use crate::schema::Registry;
    use serde_json::json;

    fn registry() -> Registry {
        let mut r = Registry::new();
        r.register(&json!({
            "name": "Outbox", "collection": "outbox", "timestamps": false,
            "fields": {
                "status": { "type": "string" },
                "lockedBy": { "type": "string" },
                "lockedAt": { "type": "int" }
            },
            "relations": {}
        }))
        .expect("schema 注册失败");
        r
    }

    // ── 计划形状（atomic_commands 契约） ──

    #[test]
    fn plan_claim_shape() {
        let r = registry();
        let plan = plan_claim(
            "Outbox",
            &r,
            None,
            &json!({ "status": "pending" }),
            &json!({ "lockedBy": "w1", "lockedAt": 1 }),
            None,
            10,
            123,
        )
        .expect("plan");
        assert_eq!(plan["mode"], json!(MODE_ATOMIC_COMMANDS));
        assert_eq!(plan["schema"], json!("Outbox"));
        let cmds = plan["commands"].as_array().unwrap();
        assert_eq!(cmds.len(), 2);
        // 命令一：claim（定位四元组 + filter + 缺省 sort + limit + update + nowStamp）
        assert_eq!(cmds[0]["kind"], json!("claim"));
        assert_eq!(cmds[0]["collection"], json!("outbox"));
        assert_eq!(cmds[0]["sort"], json!([["_id", 1]]), "缺省排序 FIFO");
        assert_eq!(cmds[0]["limit"], json!(10));
        assert_eq!(cmds[0]["nowStamp"], json!(123));
        assert!(cmds[0]["source"].is_string());
        // 命令二：find 取回被领行（占位符整值替换，契约同 {{phase1.ids}}）
        assert_eq!(cmds[1]["kind"], json!("find"));
        assert_eq!(
            cmds[1]["filter"]["_id"]["$in"],
            json!(CLAIMED_IDS),
            "占位符逐字 ___claimed_ids___"
        );
        assert_eq!(cmds[1]["placeholder"], json!(CLAIMED_IDS));
    }

    #[test]
    fn plan_cas_shape() {
        let r = registry();
        let plan = plan_compare_and_set(
            "Outbox",
            &r,
            None,
            &json!({ "_id": "l1", "lockedBy": "w1" }),
            &json!({ "lockedAt": 2 }),
            456,
        )
        .expect("plan");
        assert_eq!(plan["mode"], json!(MODE_ATOMIC_COMMANDS));
        let cmds = plan["commands"].as_array().unwrap();
        assert_eq!(cmds.len(), 1, "CAS 单命令也走 atomic_commands");
        assert_eq!(cmds[0]["kind"], json!("compareAndSet"));
        assert_eq!(cmds[0]["nowStamp"], json!(456));
    }

    // ── 错误面（步骤 5：文案前缀断言） ──

    #[test]
    fn plan_claim_unknown_schema_errs() {
        let r = registry();
        let err = plan_claim(
            "NoSuch",
            &r,
            None,
            &json!({}),
            &json!({ "lockedBy": "w1" }),
            None,
            1,
            0,
        )
        .unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn plan_claim_nonpositive_limit_errs() {
        let r = registry();
        for limit in [0i64, -3] {
            let err = plan_claim(
                "Outbox",
                &r,
                None,
                &json!({}),
                &json!({ "lockedBy": "w1" }),
                None,
                limit,
                0,
            )
            .unwrap_err();
            assert!(err.contains("limit 必须 > 0"), "{err}");
        }
    }

    #[test]
    fn plan_claim_empty_update_errs() {
        let r = registry();
        let err = plan_claim(
            "Outbox",
            &r,
            None,
            &json!({}),
            &json!({}),
            None,
            1,
            0,
        )
        .unwrap_err();
        assert!(err.contains("非空对象"), "{err}");
    }

    #[test]
    fn plan_claim_undeclared_update_field_errs() {
        let r = registry();
        let err = plan_claim(
            "Outbox",
            &r,
            None,
            &json!({}),
            &json!({ "noSuchField": 1 }),
            None,
            1,
            0,
        )
        .unwrap_err();
        assert!(err.contains("未在 schema 声明"), "{err}");
    }

    #[test]
    fn plan_cas_undeclared_update_field_errs() {
        let r = registry();
        let err = plan_compare_and_set(
            "Outbox",
            &r,
            None,
            &json!({}),
            &json!({ "noSuchField": 1 }),
            0,
        )
        .unwrap_err();
        assert!(err.contains("未在 schema 声明"), "{err}");
    }
}
