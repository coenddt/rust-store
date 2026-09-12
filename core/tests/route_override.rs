//! 多租户路由 override（`apply_route_override`）单元测试
//!
//! 语义（`multi-datasource-routing-plan.md` §6）：
//!   - 只改命令体（带 `kind` + `collection`），计划元数据（sources/edges 等）不动；
//!   - `source` / `namespace` 键出现才替换；`namespace` 可显式置 null；
//!   - 空 override / 非对象 → 原样返回。

use serde_json::{json, Value};

use rust_store_core::command::apply_route_override;

#[test]
fn source_and_namespace_replaced_on_commands_only() {
    let mut plan = json!({
        "mode": "single",
        "commands": [
            { "kind": "find", "source": "default", "namespace": null, "collection": "u", "filter": {} },
            { "kind": "countDocuments", "source": "default", "namespace": null, "collection": "u", "filter": {} }
        ],
        "countCommand": { "kind": "findOne", "source": "pg", "namespace": "app", "collection": "u" },
        "postprocess": { "steps": [{ "kind": "not-a-command" }] }
    });
    apply_route_override(
        &mut plan,
        &json!({ "source": "pg_cluster", "namespace": "tenant_42" }),
    );
    for c in plan["commands"].as_array().unwrap() {
        assert_eq!(c["source"], "pg_cluster");
        assert_eq!(c["namespace"], "tenant_42");
    }
    assert_eq!(plan["countCommand"]["source"], "pg_cluster");
    assert_eq!(plan["countCommand"]["namespace"], "tenant_42");
    // 非命令对象（有 kind 无 collection）不受影响
    assert_eq!(plan["postprocess"]["steps"][0]["kind"], "not-a-command");
}

#[test]
fn mutation_steps_and_probe_commands_overridden() {
    let mut plan = json!({
        "steps": [
            { "model": "Order", "command": { "kind": "insertOne", "source": "default", "namespace": null, "collection": "orders", "doc": {} } },
            { "model": "Item", "command": { "kind": "findOneAndUpdate", "source": "default", "namespace": null, "collection": "items", "filter": {}, "update": {} } }
        ],
        "needsProbe": { "kind": "findOne", "source": "default", "namespace": null, "collection": "orders", "filter": {} }
    });
    apply_route_override(&mut plan, &json!({ "namespace": "tenant_7" }));
    // 只带 namespace：source 保留 schema 原声明
    assert_eq!(plan["steps"][0]["command"]["source"], "default");
    assert_eq!(plan["steps"][0]["command"]["namespace"], "tenant_7");
    assert_eq!(plan["steps"][1]["command"]["namespace"], "tenant_7");
    assert_eq!(plan["needsProbe"]["namespace"], "tenant_7");
}

#[test]
fn namespace_null_reset_and_empty_override_noop() {
    let mut plan = json!({
        "commands": [
            { "kind": "find", "source": "pg", "namespace": "app", "collection": "u", "filter": {} }
        ]
    });
    // namespace 显式置 null = 回到连接默认
    apply_route_override(&mut plan, &json!({ "namespace": null }));
    assert_eq!(plan["commands"][0]["namespace"], Value::Null);
    assert_eq!(plan["commands"][0]["source"], "pg");

    // 空 override / 非对象 → 原样
    let before = plan.clone();
    apply_route_override(&mut plan, &json!({}));
    apply_route_override(&mut plan, &json!(null));
    apply_route_override(&mut plan, &json!("x"));
    assert_eq!(plan, before);
}
