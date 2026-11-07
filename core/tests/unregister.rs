//! `Registry::unregister` 的契约（宿主 `start()` 失败回滚 / 动态卸载）：
//! 派生归档一并清理、未知 name 幂等、版本不保留、策略视图只读；
//! 并固化 `register_batch` 失败零副作用（core 内收口的原子性依据）。

use rust_store_core::schema::{Location, Registry};
use serde_json::json;

fn defn(name: &str, collection: &str) -> serde_json::Value {
    json!({
        "name": name,
        "collection": collection,
        "idPrefix": "x",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {}
    })
}

#[test]
fn unregister_removes_schema_and_its_derived_archive() {
    let mut reg = Registry::default();
    reg.register(&defn("Foo", "foos")).expect("注册失败");
    assert!(reg.has("Foo"));
    assert!(reg.has("FooDeleted"), "注册期应自动派生归档表");

    reg.unregister("Foo").expect("注销失败");

    assert!(!reg.has("Foo"));
    assert!(!reg.has("FooDeleted"), "派生归档表必须随主项清理");
    assert!(reg.list().is_empty(), "{:?}", reg.list());
}

#[test]
fn unregister_unknown_is_idempotent_and_keeps_others() {
    let mut reg = Registry::default();
    reg.register(&defn("Foo", "foos")).expect("注册失败");
    let before = reg.list();

    reg.unregister("Nope").expect("未知 name 应幂等成功");
    reg.unregister("Nope").expect("重复注销仍应成功");

    assert_eq!(reg.list(), before, "未知名注销不得改动既有注册项");
}

#[test]
fn unregister_then_reregister_resets_version_and_order() {
    let mut reg = Registry::default();
    reg.register(&defn("A", "as")).expect("注册失败");
    reg.register(&defn("B", "bs")).expect("注册失败");
    reg.register(&defn("A", "as")).expect("重载失败");
    assert_eq!(reg.version_of("A"), Ok(2), "同批外重载应递增版本");

    reg.unregister("A").expect("注销失败");
    reg.register(&defn("A", "as")).expect("重注册失败");

    assert_eq!(reg.version_of("A"), Ok(1), "注销后重注册从 1 重新计数");
    assert_eq!(reg.version_of("B"), Ok(1), "其他 schema 版本不受影响");
    assert_eq!(
        reg.list(),
        vec![
            "B".to_string(),
            "BDeleted".to_string(),
            "A".to_string(),
            "ADeleted".to_string()
        ],
        "注销项从声明序摘除，重注册追加到末尾"
    );
}

#[test]
fn unregister_does_not_remove_independently_registered_archive_name() {
    let mut reg = Registry::default();
    reg.register(&defn("Doc", "docs")).expect("注册失败");
    reg.unregister("Doc").expect("注销失败");
    // 独立注册一个与派生归档同名的项（此时它不是任何主项的派生对象）
    reg.register(&defn("DocDeleted", "docs_deleted"))
        .expect("注册失败");

    reg.unregister("Doc").expect("主项已不存在 ⇒ 幂等");

    assert!(
        reg.has("DocDeleted"),
        "未命中主项时不得按名字后缀误删独立注册项"
    );
}

#[test]
fn unregister_on_policy_view_is_rejected() {
    let mut reg = Registry::default();
    reg.register(&defn("Foo", "foos")).expect("注册失败");
    let overrides = rust_store_core::schema::PolicyOverrides::default();
    let mut view = reg.with_policy(&overrides);

    let err = view.unregister("Foo").expect_err("策略视图必须只读");
    assert!(err.contains("ERR_POLICY_VIEW_READONLY"), "{err}");
}

#[test]
fn register_batch_failure_has_zero_side_effects() {
    let mut reg = Registry::default();
    let items = vec![
        (defn("Good", "dup"), Location::default()),
        // 与 Good 同 collection + 同 database/schema ⇒ 四元组定位冲突（构造期即失败）
        (defn("Bad", "dup"), Location::default()),
    ];

    let err = reg.register_batch(&items, None).expect_err("同定位应报错");
    assert!(err.contains("定位冲突"), "{err}");
    assert!(!reg.has("Good"), "批量失败不得残留前面已构造的项");
    assert!(!reg.has("GoodDeleted"));
    assert!(reg.list().is_empty(), "{:?}", reg.list());
}
