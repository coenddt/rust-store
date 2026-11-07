//! `plan_exists_with_ctx` 的读判据契约（宿主 `exists()` 的合法入口）：
//! 与 `plan_count` 同一套读校验 + 归属/RBAC 行条件；旧无 ctx 入口保持内部语义。

use rust_store_core::command::{plan_count, plan_exists, plan_exists_with_ctx};
use rust_store_core::permission::Context;
use rust_store_core::schema::Registry;
use serde_json::{json, Value};

fn registry_with(defn: Value) -> Registry {
    let mut reg = Registry::default();
    reg.register(&defn).expect("schema 注册应成功");
    reg
}

fn guarded_schema(read: Value) -> Value {
    json!({
        "name": "Doc",
        "collection": "docs",
        "idPrefix": "d",
        "timestamps": false,
        "read": read,
        "write": ["admin"],
        "fields": {
            "title": { "type": "string" },
            "secret": { "type": "string" },
            "createdBy": { "type": "string" }
        },
        "relations": {}
    })
}

fn user_ctx() -> Context {
    let mut ctx = Context {
        user_id: Some("u1".to_string()),
        roles: Some(vec!["user".to_string()]),
        ..Default::default()
    };
    ctx.role = Some("user".to_string());
    ctx
}

fn admin_ctx() -> Context {
    let mut ctx = Context {
        user_id: Some("a1".to_string()),
        roles: Some(vec!["admin".to_string()]),
        ..Default::default()
    };
    ctx.role = Some("admin".to_string());
    ctx
}

#[test]
fn legacy_entry_keeps_internal_semantics() {
    // 旧入口 = ctx=None 的内部调用语义：即使 read: [] 也保持既有 fail-open，
    // 避免无 ctx 的内部路径（迁移 / 种子）被新判据误掐断。
    let reg = registry_with(guarded_schema(json!([])));
    let plan = plan_exists("Doc", &reg, &json!({ "title": "x" })).expect("无 ctx 旧入口应保持可用");
    assert_eq!(plan["kind"], json!("findOne"), "{plan}");
}

#[test]
fn empty_read_list_agrees_with_count_on_5x() {
    // 5.x 的 `read: []` 仍是「空名单 = 不限制」（对称化语义属主干大版本改动），
    // 故本分支只保证 exists 与 count 同判据，不在此单独收紧。
    let reg = registry_with(guarded_schema(json!([])));
    let exists = plan_exists_with_ctx("Doc", &reg, &json!({ "title": "x" }), Some(&admin_ctx()));
    let count = plan_count(
        "Doc",
        &reg,
        Some(&json!({ "title": "x" })),
        Some(&admin_ctx()),
    );
    assert_eq!(
        exists.is_err(),
        count.is_err(),
        "exists 必须与 count 同一套读判据"
    );
}

#[test]
fn denies_role_outside_read_whitelist() {
    let reg = registry_with(guarded_schema(json!(["admin"])));
    let err = plan_exists_with_ctx("Doc", &reg, &json!({ "title": "x" }), Some(&user_ctx()))
        .expect_err("未命中 read 白名单的角色不得 exists");
    assert!(err.contains("ERR_PERMISSION"), "{err}");
}

#[test]
fn obeys_rbac_enforce_default_deny() {
    let mut reg = registry_with(json!({
        "name": "Post",
        "collection": "posts",
        "idPrefix": "p",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {}
    }));
    reg.set_rbac(Some(&json!({
        "mode": "enforce",
        "roles": { "user": {} },
        "grants": []
    })))
    .expect("策略应合法");
    let err = plan_exists_with_ctx("Post", &reg, &json!({}), Some(&user_ctx()))
        .expect_err("RBAC enforce 下未受 grant 的角色不得 exists");
    assert!(err.contains("ERR_PERMISSION"), "{err}");
}

#[test]
fn allows_authorized_and_keeps_return_shape() {
    let reg = registry_with(guarded_schema(json!(["admin"])));
    let plan = plan_exists_with_ctx("Doc", &reg, &json!({ "title": "x" }), Some(&admin_ctx()))
        .expect("admin 应可 exists");
    assert_eq!(plan["kind"], json!("findOne"), "{plan}");
    assert_eq!(plan["projection"], json!({ "_id": 1 }), "{plan}");
    // 返回结构与旧入口一致（kind / collection / projection 三键对齐）
    let legacy = plan_exists("Doc", &reg, &json!({ "title": "x" })).expect("旧入口可用");
    assert_eq!(plan["kind"], legacy["kind"]);
    assert_eq!(plan["collection"], legacy["collection"]);
    assert_eq!(plan["projection"], legacy["projection"]);
}

#[test]
fn injects_owner_condition_for_creator_read() {
    let reg = registry_with(json!({
        "name": "Note",
        "collection": "notes",
        "idPrefix": "n",
        "timestamps": false,
        "read": ["creator"],
        "write": ["creator"],
        "fields": {
            "body": { "type": "string" },
            "createdBy": { "type": "string" }
        },
        "relations": {}
    }));
    let plan = plan_exists_with_ctx("Note", &reg, &json!({ "body": "x" }), Some(&user_ctx()))
        .expect("creator 读应可 exists");
    assert_eq!(
        plan["filter"],
        json!({ "$and": [{ "body": "x" }, { "createdBy": "u1" }] }),
        "存在性只能看见自己读得到的行: {plan}"
    );
}
