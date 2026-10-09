//! N 系列 01 步：复现「待核实」项（N6a 联邦无 $condition 越权 / N6d 触发器写越权）
//!
//! 断言口径：断言**正确行为**（应注入行条件）。本文件先于修复运行——预期为红（复现成立），
//! 修复步骤 4/5 落地后转绿，作为长期回归用例。

use serde_json::{json, Map, Value};

use rust_store_core::command::{plan_update, Probe};
use rust_store_core::federation::plan_federated;
use rust_store_core::permission::Context;
use rust_store_core::schema::Registry;

/// ownerOnly schema：`read: ["creator"]`（非 creator 用户仅可见自己创建的行）
fn owner_only_schema(name: &str, coll: &str, prefix: &str) -> Value {
    json!({
        "name": name,
        "collection": coll,
        "idPrefix": prefix,
        "timestamps": false,
        "read": ["creator"],
        "fields": { "name": { "type": "string" } },
        "relations": {}
    })
}

fn non_creator_ctx() -> Context {
    Context {
        user_id: Some("u1".to_string()),
        roles: Some(vec!["member".to_string()]),
        role: None,
        internal: false,
    }
}

/// N6a：联邦查询**未显式携带 `$condition`** 时，owner/RBAC 行条件也应注入（防越权读全表）。
/// 复现成功 = 当前 inject 缺失 → 本断言红。
#[test]
fn n6a_federation_without_condition_injects_owner() {
    let mut reg = Registry::new();
    reg.register(&owner_only_schema("Doc", "docs", "d"))
        .expect("Doc 注册成功");

    let plan = plan_federated(
        "Doc { name }",
        &Map::new(),
        &reg,
        Some(&non_creator_ctx()),
        &Value::Null,
    )
    .expect("plan_federated 应成功");

    let cmds = serde_json::to_string(&plan["sources"][0]["commands"]).expect("序列化 commands");
    assert!(
        cmds.contains("createdBy"),
        "N6a：联邦无 $condition 时应注入 owner 行条件（createdBy），实际 commands={cmds}"
    );
}

/// N6d：触发器写目标（op=update）应带行级探针（owner/RBAC 行条件并入 filter），
/// 否则可命中他人行。复现成功 = 当前 filter 无 createdBy → 本断言红。
#[test]
fn n6d_trigger_update_injects_row_condition() {
    let mut reg = Registry::new();
    reg.register(&owner_only_schema("Target", "targets", "g"))
        .expect("Target 注册成功");
    reg.register(&json!({
        "name": "Source",
        "collection": "sources",
        "idPrefix": "s",
        "timestamps": false,
        "fields": { "title": { "type": "string" }, "status": { "type": "string" } },
        "relations": {},
        "triggers": {
            "update": [
                {
                    "name": "sync",
                    "onFields": ["status"],
                    "into": "Target",
                    "op": "update",
                    "condition": { "name": "x" },
                    "data": { "name": "y" }
                }
            ]
        }
    }))
    .expect("Source 注册成功");

    let plan = plan_update(
        "Source",
        &reg,
        Some(&non_creator_ctx()),
        &json!({ "title": "a" }),
        &json!({ "status": "done" }),
        &json!({}),
        1000,
        Probe::Found(&json!({ "_id": "s1", "status": "open" })),
    )
    .expect("plan_update 应成功");

    let triggers = plan.get("triggers").expect("应含 triggers");
    let filter = serde_json::to_string(&triggers[0]["command"]["filter"]).expect("序列化 filter");
    assert!(
        filter.contains("createdBy"),
        "N6d：触发器 update 目标应注入行级条件（createdBy），实际 filter={filter}"
    );
}

/// N6d：触发器写目标（op=remove）同样应带行级探针。
#[test]
fn n6d_trigger_remove_injects_row_condition() {
    let mut reg = Registry::new();
    reg.register(&owner_only_schema("Target", "targets", "g"))
        .expect("Target 注册成功");
    reg.register(&json!({
        "name": "Source",
        "collection": "sources",
        "idPrefix": "s",
        "timestamps": false,
        "fields": { "title": { "type": "string" }, "status": { "type": "string" } },
        "relations": {},
        "triggers": {
            "remove": [
                {
                    "name": "cleanup",
                    "into": "Target",
                    "op": "remove",
                    "condition": { "name": "x" }
                }
            ]
        }
    }))
    .expect("Source 注册成功");

    let plan = rust_store_core::command::plan_remove(
        "Source",
        &reg,
        Some(&non_creator_ctx()),
        &json!({ "title": "a" }),
        Probe::Found(&json!({ "_id": "s1", "status": "open" })),
    )
    .expect("plan_remove 应成功");

    let triggers = plan.get("triggers").expect("应含 triggers");
    let filter = serde_json::to_string(&triggers[0]["command"]["filter"]).expect("序列化 filter");
    assert!(
        filter.contains("createdBy"),
        "N6d：触发器 remove 目标应注入行级条件（createdBy），实际 filter={filter}"
    );
}
