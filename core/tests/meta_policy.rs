//! 定义层门禁单测（分步 03，A4）：Open 全放行 / Closed 无 ctx 拒绝且定义不变 /
//! Closed internal 放行 / Closed 白名单角色放行。

use serde_json::json;

use rust_store_core::permission::{can_register, context_from_value, Context, MetaPolicy};
use rust_store_core::schema::Registry;

fn defn(name: &str) -> serde_json::Value {
    json!({ "name": name, "fields": { "title": { "type": "string" } } })
}

#[test]
fn open_policy_allows_all() {
    let mut reg = Registry::new();
    // 默认 Open（未调 set_meta_policy）→ 无 ctx 亦放行（既有 parity）
    reg.register(&defn("A")).expect("Open 应放行");
    assert!(reg.has("A"));
}

#[test]
fn closed_rejects_without_ctx_and_definition_unchanged() {
    let mut reg = Registry::new();
    reg.set_meta_policy(true, vec![]);
    let err = reg.register(&defn("B")).unwrap_err();
    assert!(err.starts_with("ERR_PERMISSION:"), "err={err}");
    // 拒绝即返回：零副作用，定义（含归档派生）不得写入
    assert!(!reg.has("B"), "拒绝时定义不得写入");
    assert!(!reg.has("BDeleted"), "拒绝时归档表不得派生");
}

#[test]
fn closed_allows_internal_ctx() {
    let mut reg = Registry::new();
    reg.set_meta_policy(true, vec![]);
    let ctx = Context::system();
    reg.register_with_ctx(&defn("C"), Some(&ctx))
        .expect("internal 应放行");
    assert!(reg.has("C"));
    assert!(reg.has("CDeleted"), "归档表随内部注册派生");
}

#[test]
fn closed_allows_whitelisted_role_only() {
    let mut reg = Registry::new();
    reg.set_meta_policy(true, vec!["meta_admin".to_string()]);
    let outsider = context_from_value(&json!({ "roles": ["guest"] })).unwrap();
    assert!(reg.register_with_ctx(&defn("D"), Some(&outsider)).is_err());
    assert!(!reg.has("D"));
    let insider = context_from_value(&json!({ "roles": ["meta_admin"] })).unwrap();
    reg.register_with_ctx(&defn("D"), Some(&insider))
        .expect("白名单角色应放行");
    assert!(reg.has("D"));
}

#[test]
fn closed_empty_roles_only_internal() {
    let mut reg = Registry::new();
    reg.set_meta_policy(true, vec![]);
    // 非 internal 的任何角色都不放行（白名单为空）
    let role = context_from_value(&json!({ "roles": ["admin"] })).unwrap();
    assert!(reg.register_with_ctx(&defn("E"), Some(&role)).is_err());
}

#[test]
fn can_register_unit() {
    let open = MetaPolicy::default();
    assert!(can_register(&open, None));
    let closed = MetaPolicy { closed: true, roles: vec![] };
    assert!(!can_register(&closed, None));
    assert!(can_register(&closed, Some(&Context::system())));
}

#[test]
fn registry_can_register_readonly() {
    let mut reg = Registry::new();
    // Open（默认）：无 ctx 放行
    assert!(reg.can_register(None));

    reg.set_meta_policy(true, vec![]);
    // Closed：无 ctx 拒
    assert!(!reg.can_register(None));
    // Closed：internal 放行
    let sys = Context::system();
    assert!(reg.can_register(Some(&sys)));
    // Closed：白名单角色放行
    reg.set_meta_policy(true, vec!["meta_admin".to_string()]);
    let insider = context_from_value(&json!({ "roles": ["meta_admin"] })).unwrap();
    assert!(reg.can_register(Some(&insider)));
    // 只读性：判决调用不改变注册表
    assert!(!reg.has("X"));
}
