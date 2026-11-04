//! A2 / A3 修复回归：
//! - A2：creator 权限 ⇒ 注册期自动补 `createdBy`，insert 落库文档带归属键；
//! - A3：schema 显式 `default` 在 insert **落库前**生效，且保留显式 `null`、
//!   不补类型零值、嵌套 object 只补显式声明的 default。

use serde_json::{json, Value};

use rust_store_core::command::plan_insert;
use rust_store_core::permission::{context_from_value, Context};
use rust_store_core::schema::Registry;

fn ctx_of(v: Value) -> Context {
    context_from_value(&v).expect("ctx JSON 应解析为对象")
}

fn registry_with(defns: &[Value]) -> Registry {
    let mut reg = Registry::new();
    for d in defns {
        reg.register(d).expect("schema 注册应成功");
    }
    reg
}

fn insert_doc(reg: &Registry, ctx: Option<&Context>, data: Value) -> Value {
    let plan = plan_insert("Task", reg, ctx, &data, 0, "t_new", None).expect("insert 规划应成功");
    plan["command"]["doc"].clone()
}

/// A2：`write: ["creator"]` ⇒ 注册期自动补 `createdBy` 字段声明
#[test]
fn a2_register_adds_created_by_field() {
    let reg = registry_with(&[json!({
        "name": "Task",
        "collection": "tasks",
        "idPrefix": "t",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "write": ["creator"],
    })]);
    let schema = reg.get("Task").expect("Task 应已注册");
    assert!(
        schema.fields.contains_key("createdBy"),
        "creator 权限未自动补 createdBy 字段"
    );
}

/// A2：insert 落库文档包含 createdBy（取 ctx.userId）
#[test]
fn a2_insert_doc_contains_created_by() {
    let reg = registry_with(&[json!({
        "name": "Task",
        "collection": "tasks",
        "idPrefix": "t",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "write": ["creator"],
    })]);
    let c = ctx_of(json!({ "userId": "u1" }));
    let doc = insert_doc(&reg, Some(&c), json!({ "title": "x" }));
    assert_eq!(doc["createdBy"], "u1", "落库文档应注入归属键: {doc}");
}

/// A2：显式传入的 createdBy 不被覆盖
#[test]
fn a2_explicit_created_by_is_kept() {
    let reg = registry_with(&[json!({
        "name": "Task",
        "collection": "tasks",
        "idPrefix": "t",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "write": ["creator"],
    })]);
    let c = ctx_of(json!({ "userId": "u1" }));
    let doc = insert_doc(&reg, Some(&c), json!({ "title": "x", "createdBy": "u2" }));
    assert_eq!(doc["createdBy"], "u2", "显式归属键应优先: {doc}");
}

fn default_schema() -> Value {
    json!({
        "name": "Task",
        "collection": "tasks",
        "idPrefix": "t",
        "timestamps": false,
        "fields": {
            "title": { "type": "string" },
            "deletedAt": { "type": "int", "default": 0 },
            "count": { "type": "int" },
            "meta": {
                "type": "object",
                "fields": {
                    "a": { "type": "int" },
                    "b": { "type": "int", "default": 1 }
                }
            }
        },
        "relations": {},
    })
}

/// A3：未传 deletedAt ⇒ 落库取 schema 显式 default 0（列表按 deletedAt:0 才能命中）
#[test]
fn a3_insert_applies_declared_default() {
    let reg = registry_with(&[default_schema()]);
    let c = ctx_of(json!({ "userId": "u1" }));
    let doc = insert_doc(&reg, Some(&c), json!({ "title": "x" }));
    assert_eq!(doc["deletedAt"], 0, "显式 default 应落库: {doc}");
}

/// A3：显式传入 5 ⇒ 保留 5（default 不得覆盖显式值）
#[test]
fn a3_explicit_value_overrides_default() {
    let reg = registry_with(&[default_schema()]);
    let doc = insert_doc(&reg, None, json!({ "title": "x", "deletedAt": 5 }));
    assert_eq!(doc["deletedAt"], 5, "{doc}");
}

/// A3：显式 null 必须原样落库（不得被 default 替换）——F-07 / H-01 三态契约
#[test]
fn a3_explicit_null_is_preserved() {
    let reg = registry_with(&[default_schema()]);
    let doc = insert_doc(&reg, None, json!({ "title": "x", "deletedAt": null }));
    assert!(
        doc.get("deletedAt").map(Value::is_null).unwrap_or(false),
        "显式 null 被 default 替换了: {doc}"
    );
}

/// A3：未声明 default 的字段不补类型零值（避免把「未提供」伪装成 0 / ""）
#[test]
fn a3_no_type_zero_defaults_on_insert() {
    let reg = registry_with(&[default_schema()]);
    let doc = insert_doc(&reg, None, json!({ "title": "x" }));
    assert!(
        !doc.as_object().unwrap().contains_key("count"),
        "未声明 default 的 count 不应落 0: {doc}"
    );
}

/// A3：嵌套 object 只补显式声明的 default（`b`），不补类型零值（`a`）
#[test]
fn a3_nested_only_declared_default() {
    let reg = registry_with(&[default_schema()]);
    let doc = insert_doc(&reg, None, json!({ "title": "x", "meta": {} }));
    assert_eq!(doc["meta"], json!({ "b": 1 }), "嵌套默认应收窄: {doc}");
}

/// A3：嵌套显式 null 保留 null，且不递归填子字段
#[test]
fn a3_nested_explicit_null_is_preserved() {
    let reg = registry_with(&[default_schema()]);
    let doc = insert_doc(&reg, None, json!({ "title": "x", "meta": null }));
    assert!(doc["meta"].is_null(), "嵌套显式 null 应保留: {doc}");
}

// ─── ownerKey（owner 行控，A2 同类根因） ─────────────────────

fn owner_schema(owner_key: Value, write_roles: Value) -> Value {
    json!({
        "name": "Task",
        "collection": "tasks",
        "idPrefix": "t",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "ownerKey": owner_key,
        "write": write_roles,
    })
}

/// owner 行控 + 自定义 ownerKey 未声明 ⇒ 注册期补该字段（不再硬编码 createdBy）
#[test]
fn owner_key_register_adds_custom_field() {
    let reg = registry_with(&[owner_schema(json!("creatorId"), json!(["creator"]))]);
    let schema = reg.get("Task").expect("Task 应已注册");
    assert!(
        schema.fields.contains_key("creatorId"),
        "owner 行控未自动补 ownerKey 字段: {:?}",
        schema.fields.keys().collect::<Vec<_>>()
    );
    assert!(
        !schema.fields.contains_key("createdBy"),
        "ownerKey 自定义时不应再补 createdBy"
    );
}

/// owner 行控 + 缺省 ownerKey ⇒ 仍补 createdBy（零破坏）
#[test]
fn owner_key_default_still_adds_created_by() {
    let reg = registry_with(&[owner_schema(Value::Null, json!(["creator"]))]);
    let schema = reg.get("Task").expect("Task 应已注册");
    assert!(schema.fields.contains_key("createdBy"));
}

/// 自定义 ownerKey 的 insert 落库文档注入该键（不注入 createdBy）
#[test]
fn owner_key_insert_doc_uses_custom_field() {
    let reg = registry_with(&[owner_schema(json!("creatorId"), json!(["creator"]))]);
    let c = ctx_of(json!({ "userId": "u1" }));
    let doc = insert_doc(&reg, Some(&c), json!({ "title": "x" }));
    assert_eq!(doc["creatorId"], "u1", "落库文档应注入归属键: {doc}");
    assert!(
        doc.get("createdBy").is_none(),
        "ownerKey 自定义时不应注入 createdBy: {doc}"
    );
}

/// 显式声明 ownerKey 字段 ⇒ 保留声明的类型，不被补成 string
#[test]
fn owner_key_explicit_declaration_keeps_type() {
    let mut defn = owner_schema(json!("creatorId"), json!(["creator"]));
    defn["fields"]["creatorId"] = json!({ "type": "int" });
    let reg = registry_with(&[defn]);
    let schema = reg.get("Task").expect("Task 应已注册");
    assert_eq!(schema.fields["creatorId"].field_type, "int");
}

/// 不使用 owner 行控（无 creator）⇒ 不补字段（行为不变）
#[test]
fn owner_key_without_owner_control_adds_nothing() {
    let reg = registry_with(&[owner_schema(json!("creatorId"), json!(["admin"]))]);
    let schema = reg.get("Task").expect("Task 应已注册");
    assert!(!schema.fields.contains_key("creatorId"));
}

/// ownerKey = `_id`（系统主键）⇒ 不把 _id 补成普通字段
#[test]
fn owner_key_id_is_not_added_as_field() {
    let reg = registry_with(&[owner_schema(json!("_id"), json!(["creator"]))]);
    let schema = reg.get("Task").expect("Task 应已注册");
    assert!(!schema.fields.contains_key("_id"));
}
