//! P1 / P2 / P4 / P10 写路径归属与行级条件回归。
//!
//! - P2：RBAC ownerOnly 的 update / remove 在探针放行后，**执行条件**必须合入
//!   `ownerKey = ctx.userId`（此前仍用原始条件，可删改他人数据）；
//! - P1：`write: ["creator"]` 的 update / updateMany / update 必须做归属校验与条件收窄；
//! - P4：upsert 新文档的归属键取 `ctx.userId`（此前误用 `_id`）；
//! - P10：归属键是系统维护字段——insert 不得冒充、update 不得改写（含点号路径与操作符）。

use serde_json::{json, Value};

use rust_store_core::command::{
    plan_insert, plan_remove, plan_update, plan_update_many, plan_upsert,
};
use rust_store_core::permission::Context;
use rust_store_core::schema::Registry;

const ERR_NO_WRITE: &str = "ERR_PERMISSION:无写入权限";

fn registry_with(defns: &[Value]) -> Registry {
    let mut reg = Registry::new();
    for d in defns {
        reg.register(d).expect("schema 注册应成功");
    }
    reg
}

fn ctx_of(uid: &str, roles: &[&str]) -> Context {
    Context {
        user_id: Some(uid.to_string()),
        roles: Some(roles.iter().map(|r| r.to_string()).collect()),
        ..Default::default()
    }
}

fn post_schema() -> Value {
    json!({
        "name": "Post",
        "collection": "posts",
        "idPrefix": "p",
        "timestamps": false,
        "fields": { "title": { "type": "string" }, "status": { "type": "string" } },
        "relations": {},
    })
}

fn creator_schema() -> Value {
    json!({
        "name": "Post",
        "collection": "posts",
        "idPrefix": "p",
        "timestamps": false,
        "fields": { "title": { "type": "string" }, "status": { "type": "string" } },
        "relations": {},
        "write": ["creator"],
    })
}

fn owner_only_rbac(actions: Value) -> Value {
    json!({
        "roles": { "editor": {} },
        "grants": [ { "role": "editor", "model": "Post", "actions": actions, "ownerOnly": true } ]
    })
}

fn filter_has_owner(filter: &Value, uid: &str) -> bool {
    let owner = json!({ "createdBy": uid });
    if filter == &owner {
        return true;
    }
    filter["$and"]
        .as_array()
        .map(|ands| ands.iter().any(|c| c == &owner))
        .unwrap_or(false)
}

// ─── P2：RBAC ownerOnly 执行条件收窄 ─────────────────────────

#[test]
fn p2_owner_only_update_executes_with_row_condition() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&owner_only_rbac(json!(["update"]))))
        .unwrap();
    let e = ctx_of("u1", &["editor"]);
    let plan = plan_update(
        "Post",
        &reg,
        Some(&e),
        &json!({ "status": "open" }),
        &json!({ "title": "y" }),
        &json!({}),
        0,
        rust_store_core::command::Probe::Found(&json!({ "_id": "p1", "createdBy": "u1" })),
    )
    .expect("ownerOnly 命中本人文档应放行");
    let filter = &plan["command"]["filter"];
    assert!(
        filter_has_owner(filter, "u1"),
        "update 执行条件必须合入 ownerOnly 行条件: {filter}"
    );
    assert!(
        filter.to_string().contains("open"),
        "原条件不得丢失: {filter}"
    );
}

#[test]
fn p2_owner_only_remove_delete_many_includes_row_condition() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&owner_only_rbac(json!(["remove"]))))
        .unwrap();
    let e = ctx_of("u1", &["editor"]);
    let plan = plan_remove(
        "Post",
        &reg,
        Some(&e),
        &json!({ "status": "open" }),
        rust_store_core::command::Probe::Found(&json!({ "_id": "p1", "createdBy": "u1" })),
    )
    .expect("ownerOnly 命中本人文档应放行");
    let delete_filter = &plan["deleteCommand"]["filter"];
    assert!(
        filter_has_owner(delete_filter, "u1"),
        "deleteMany 必须合入 ownerOnly 行条件（否则可删他人数据）: {delete_filter}"
    );
    let find_filter = &plan["findCommand"]["filter"];
    if !find_filter.is_null() {
        assert!(
            filter_has_owner(find_filter, "u1"),
            "归档 find 必须与 delete 同条件: {find_filter}"
        );
    }
}

// ─── P1：write:["creator"] 的写路径归属校验 ──────────────────

#[test]
fn p1_creator_only_update_probes_and_denies_others() {
    let reg = registry_with(&[creator_schema()]);
    let u1 = ctx_of("u1", &[]);
    // 首入必须发探针（此前 can_write_schema 对 Doc::Missing 恒放行，分支不可达）
    let first = plan_update(
        "Post",
        &reg,
        Some(&u1),
        &json!({ "_id": "p1" }),
        &json!({ "title": "y" }),
        &json!({}),
        0,
        rust_store_core::command::Probe::NotProbed,
    )
    .expect("creator 写应返回探针");
    assert!(
        first.get("needsProbe").is_some(),
        "creator-only update 必须探针: {first}"
    );
    // 命中他人文档 → 拒绝（错误码/文案保持 ERR_NO_WRITE）
    let err = plan_update(
        "Post",
        &reg,
        Some(&u1),
        &json!({ "_id": "p1" }),
        &json!({ "title": "y" }),
        &json!({}),
        0,
        rust_store_core::command::Probe::Found(&json!({ "_id": "p1", "createdBy": "u2" })),
    )
    .expect_err("creator-only 更新他人文档应拒绝");
    assert_eq!(err, ERR_NO_WRITE);
}

#[test]
fn p1_creator_only_update_executes_with_owner_condition() {
    let reg = registry_with(&[creator_schema()]);
    let u1 = ctx_of("u1", &[]);
    let plan = plan_update(
        "Post",
        &reg,
        Some(&u1),
        &json!({ "status": "open" }),
        &json!({ "title": "y" }),
        &json!({}),
        0,
        rust_store_core::command::Probe::Found(&json!({ "_id": "p1", "createdBy": "u1" })),
    )
    .expect("creator 命中本人文档应放行");
    let filter = &plan["command"]["filter"];
    assert!(
        filter_has_owner(filter, "u1"),
        "creator-only update 执行条件必须含归属键: {filter}"
    );
}

#[test]
fn p1_creator_only_update_many_merges_owner_condition() {
    let reg = registry_with(&[creator_schema()]);
    let u1 = ctx_of("u1", &[]);
    let plan = plan_update_many(
        "Post",
        &reg,
        Some(&u1),
        &json!({ "status": "open" }),
        &json!({ "title": "y" }),
        0,
    )
    .expect("creator-only updateMany 应规划成功");
    let filter = &plan["command"]["filter"];
    assert!(
        filter_has_owner(filter, "u1"),
        "creator-only updateMany 必须收窄到本人: {filter}"
    );
}

#[test]
fn p1_creator_only_update_many_without_user_id_is_denied() {
    let reg = registry_with(&[creator_schema()]);
    let anon = Context::default();
    let err = plan_update_many(
        "Post",
        &reg,
        Some(&anon),
        &json!({ "status": "open" }),
        &json!({ "title": "y" }),
        0,
    )
    .expect_err("无 userId 的 creator-only 批量写必须拒绝");
    assert_eq!(err, "ERR_PERMISSION:无批量写入权限");
}

#[test]
fn p1_creator_only_remove_denies_others() {
    let reg = registry_with(&[creator_schema()]);
    let u1 = ctx_of("u1", &[]);
    let err = plan_remove(
        "Post",
        &reg,
        Some(&u1),
        &json!({ "_id": "p1" }),
        rust_store_core::command::Probe::Found(&json!({ "_id": "p1", "createdBy": "u2" })),
    )
    .expect_err("creator-only 删除他人文档应拒绝");
    assert_eq!(err, "ERR_PERMISSION:无删除权限");
}

// ─── P4：upsert 归属键取 userId ──────────────────────────────

#[test]
fn p4_upsert_set_on_insert_owner_is_user_id() {
    let reg = registry_with(&[creator_schema()]);
    let u1 = ctx_of("u1", &[]);
    let plan = plan_upsert(
        "Post",
        &reg,
        Some(&u1),
        &json!({ "_id": "p9" }),
        &json!({ "title": "x" }),
        &json!({}),
        0,
        "p9",
    )
    .expect("upsert 应规划成功");
    let update = &plan["command"]["update"];
    assert_eq!(
        update["$setOnInsert"]["createdBy"], "u1",
        "upsert 新文档归属键必须是 userId: {update}"
    );
    let filter = &plan["command"]["filter"];
    assert!(
        filter_has_owner(filter, "u1"),
        "creator-only upsert 必须收窄到本人: {filter}"
    );
}

// ─── F1：upsert 合并字段 default ────────────────────────────

#[test]
fn f1_upsert_set_on_insert_merges_field_defaults() {
    let reg = registry_with(&[json!({
        "name": "Cfg",
        "collection": "cfgs",
        "idPrefix": "c",
        "timestamps": false,
        "fields": {
            "name": { "type": "string" },
            "n": { "type": "int", "default": 5 },
            "s": { "type": "string", "default": "D" }
        },
        "relations": {}
    })]);
    let plan = plan_upsert(
        "Cfg",
        &reg,
        None,
        &json!({ "name": "x" }),
        &json!({ "name": "x" }),
        &json!({}),
        0,
        "c9",
    )
    .expect("upsert 应规划成功");
    let update = &plan["command"]["update"];
    assert_eq!(
        update["$setOnInsert"]["n"], 5,
        "F1：$setOnInsert 必须合并字段 default: {update}"
    );
    assert_eq!(update["$setOnInsert"]["s"], "D", "{update}");
    assert!(
        update["$set"].get("n").is_none(),
        "F1：default 不得进 $set（否则更新会覆盖现值）: {update}"
    );
}

// ─── P10：归属键不可写 ──────────────────────────────────────

#[test]
fn p10_insert_cannot_impersonate_owner() {
    let reg = registry_with(&[creator_schema()]);
    let u1 = ctx_of("u1", &[]);
    let plan = plan_insert(
        "Post",
        &reg,
        Some(&u1),
        &json!({ "_id": "p1", "title": "x", "createdBy": "u2" }),
        0,
        "p1",
        None,
    )
    .expect("insert 应规划成功");
    assert_eq!(
        plan["command"]["doc"]["createdBy"], "u1",
        "显式传入的归属键必须被 ctx.userId 覆盖: {}",
        plan["command"]["doc"]
    );
}

#[test]
fn p10_update_rejects_owner_field() {
    let reg = registry_with(&[post_schema()]);
    let u1 = ctx_of("u1", &[]);
    let err = plan_update(
        "Post",
        &reg,
        Some(&u1),
        &json!({ "_id": "p1" }),
        &json!({ "title": "y", "createdBy": "u2" }),
        &json!({}),
        0,
        rust_store_core::command::Probe::NotProbed,
    )
    .expect_err("update 改写归属键应显式报错");
    assert!(err.contains("归属键"), "{err}");
}

#[test]
fn p10_update_rejects_owner_dotted_path() {
    let reg = registry_with(&[post_schema()]);
    let u1 = ctx_of("u1", &[]);
    let err = plan_update(
        "Post",
        &reg,
        Some(&u1),
        &json!({ "_id": "p1" }),
        &json!({ "createdBy.x": "u2" }),
        &json!({}),
        0,
        rust_store_core::command::Probe::NotProbed,
    )
    .expect_err("点号路径穿透归属键应显式报错");
    assert!(err.contains("归属键"), "{err}");
}

#[test]
fn p10_update_raw_operator_rejects_owner_field() {
    let reg = registry_with(&[post_schema()]);
    let u1 = ctx_of("u1", &[]);
    let err = plan_update(
        "Post",
        &reg,
        Some(&u1),
        &json!({ "_id": "p1" }),
        &json!({ "$set": { "createdBy": "u2" } }),
        &json!({}),
        0,
        rust_store_core::command::Probe::NotProbed,
    )
    .expect_err("$set 改写归属键应显式报错");
    assert!(err.contains("归属键"), "{err}");

    let err = plan_update(
        "Post",
        &reg,
        Some(&u1),
        &json!({ "_id": "p1" }),
        &json!({ "$unset": { "createdBy": "" } }),
        &json!({}),
        0,
        rust_store_core::command::Probe::NotProbed,
    )
    .expect_err("$unset 改写归属键应显式报错");
    assert!(err.contains("归属键"), "{err}");
}

// ─── P10 修正：internal 可信调用方允许显式写归属键 ─────────────

fn internal_ctx(uid: Option<&str>) -> Context {
    Context {
        internal: true,
        user_id: uid.map(|s| s.to_string()),
        ..Default::default()
    }
}

/// 回归：宿主（metadef / 迁移 / 种子）以 internal 上下文显式写 `createdBy` 必须落库。
/// 此前 `get_writable_fields` 无条件剔除归属键，internal 显式值被静默丢弃（KeyError: createdBy）。
#[test]
fn p10_internal_insert_keeps_explicit_owner() {
    let reg = registry_with(&[creator_schema()]);

    // internal 且无 userId：显式值原样保留（不得被字段过滤剔除，也不得因无身份报错）
    let plan = plan_insert(
        "Post",
        &reg,
        Some(&internal_ctx(None)),
        &json!({ "_id": "p1", "title": "x", "createdBy": "u1" }),
        0,
        "p1",
        None,
    )
    .expect("internal insert 应规划成功");
    assert_eq!(
        plan["command"]["doc"]["createdBy"], "u1",
        "internal 显式归属键不得被过滤: {}",
        plan["command"]["doc"]
    );

    // internal 且带 userId：可信调用方的显式值优先（不被 ctx.userId 覆盖）
    let plan = plan_insert(
        "Post",
        &reg,
        Some(&internal_ctx(Some("operator"))),
        &json!({ "_id": "p2", "title": "x", "createdBy": "u1" }),
        0,
        "p2",
        None,
    )
    .expect("internal insert 应规划成功");
    assert_eq!(
        plan["command"]["doc"]["createdBy"], "u1",
        "可信调用方显式指定归属键应保留: {}",
        plan["command"]["doc"]
    );
}

/// internal 走 update 同样可改写归属键（`reject_owner_write` 与字段过滤均须放行）。
#[test]
fn p10_internal_update_can_change_owner() {
    // 归属键显式声明（宿主 metadef 模型即如此：`createdBy: { type: 'string' }`）
    let reg = registry_with(&[json!({
        "name": "Doc",
        "collection": "docs",
        "timestamps": false,
        "fields": {
            "title": { "type": "string" },
            "createdBy": { "type": "string" }
        },
    })]);
    let plan = plan_update(
        "Doc",
        &reg,
        Some(&internal_ctx(None)),
        &json!({ "_id": "d1" }),
        &json!({ "createdBy": "u2" }),
        &json!({}),
        0,
        rust_store_core::command::Probe::NotProbed,
    )
    .expect("internal update 改写归属键应放行");
    assert_eq!(
        plan["command"]["update"]["$set"]["createdBy"], "u2",
        "internal update 的归属键写入不得被过滤: {}",
        plan["command"]["update"]
    );
}

/// 非 internal 的过滤/拒绝不受影响：冒充、点号穿透、`$set` 夹带仍全部拒。
#[test]
fn p10_untrusted_still_cannot_touch_owner() {
    // insert 冒充：creator 权限模型（无 creator 时 insert 不做归属注入）
    let creator = registry_with(&[creator_schema()]);
    let u1 = ctx_of("u1", &[]);

    let plan = plan_insert(
        "Post",
        &creator,
        Some(&u1),
        &json!({ "_id": "p1", "title": "x", "createdBy": "u2" }),
        0,
        "p1",
        None,
    )
    .expect("非 internal insert 应规划成功");
    assert_eq!(plan["command"]["doc"]["createdBy"], "u1");

    // 点号穿透 / `$set` 夹带 / updateMany 夹带：普通模型（无需探针）
    let reg = registry_with(&[post_schema()]);

    // 点号穿透
    let err = plan_update(
        "Post",
        &reg,
        Some(&u1),
        &json!({ "_id": "p1" }),
        &json!({ "createdBy.x": "u2" }),
        &json!({}),
        0,
        rust_store_core::command::Probe::NotProbed,
    )
    .expect_err("点号路径穿透归属键应显式报错");
    assert!(err.contains("归属键"), "{err}");

    // `$set` 里夹带（与合法字段同批）仍被拒
    let err = plan_update(
        "Post",
        &reg,
        Some(&u1),
        &json!({ "_id": "p1" }),
        &json!({ "$set": { "title": "y", "createdBy": "u2" } }),
        &json!({}),
        0,
        rust_store_core::command::Probe::NotProbed,
    )
    .expect_err("$set 夹带归属键应显式报错");
    assert!(err.contains("归属键"), "{err}");

    // updateMany 同样不放过（批量更新与合法字段同批夹带）
    let err = plan_update_many(
        "Post",
        &reg,
        Some(&u1),
        &json!({ "status": "open" }),
        &json!({ "title": "y", "createdBy": "u2" }),
        0,
    )
    .expect_err("updateMany 夹带归属键应显式报错");
    assert!(err.contains("归属键"), "{err}");
}
