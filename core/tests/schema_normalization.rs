//! N 系列：schema 注册规范化回归。
//!
//! - N1：`ownerKey:"_id"` / 非 string 归属字段在注册期拒绝（见 write_fixes / guards）；
//! - N2：归档模型继承因 creator 权限**补出**的归属字段；
//! - N4：字段定义（含嵌套 `fields`）未知键显式拒绝，不再静默丢弃；
//! - N5：`indexes` 结构 / 索引键引用 / 顶层未知键显式拒绝。

use serde_json::json;

use rust_store_core::schema::Registry;

fn reg_err(defn: serde_json::Value) -> String {
    let mut reg = Registry::new();
    reg.register(&defn).expect_err("注册应失败")
}

#[test]
fn n2_archive_inherits_auto_added_owner_field() {
    let mut reg = Registry::new();
    reg.register(&json!({
        "name": "Post",
        "collection": "posts",
        "idPrefix": "p",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "read": ["creator"],
        "write": ["creator"],
    }))
    .expect("注册应成功");
    let arch = reg.get("PostDeleted").expect("归档模型应派生");
    assert!(
        arch.fields.contains_key("createdBy"),
        "N2：归档模型必须继承补出的归属字段，实际字段: {:?}",
        arch.fields.keys().collect::<Vec<_>>()
    );
}

/// V12：跨批次重注册同名 schema → 允许覆盖，`version` 递增，字段随之替换。
#[test]
fn v12_re_register_same_name_across_batches_replaces_and_bumps_version() {
    let mut reg = Registry::new();
    reg.register(&json!({
        "name": "Hot",
        "collection": "hot",
        "timestamps": false,
        "fields": { "a": { "type": "string" } },
        "relations": {},
    }))
    .expect("首次注册应成功");
    assert_eq!(reg.version_of("Hot").expect("版本应可读"), 1);

    reg.register(&json!({
        "name": "Hot",
        "collection": "hot",
        "timestamps": false,
        "fields": { "b": { "type": "string" } },
        "relations": {},
    }))
    .expect("重注册应允许覆盖");
    assert_eq!(
        reg.version_of("Hot").expect("版本应可读"),
        2,
        "版本必须递增"
    );
    let s = reg.get("Hot").expect("应可读");
    assert!(s.fields.contains_key("b"), "新定义字段应生效");
    assert!(!s.fields.contains_key("a"), "旧定义字段应被替换");
}

#[test]
fn n4_unknown_field_key_is_rejected() {
    let err = reg_err(json!({
        "name": "Post",
        "collection": "posts",
        "fields": { "title": { "type": "string", "comment": "备注" } },
        "relations": {},
    }));
    assert!(err.contains("未知属性 \"comment\""), "{err}");
}

#[test]
fn n4_unknown_nested_field_key_is_rejected() {
    let err = reg_err(json!({
        "name": "Post",
        "collection": "posts",
        "fields": {
            "meta": {
                "type": "object",
                "fields": { "z": { "type": "string", "of": "string" } }
            }
        },
        "relations": {},
    }));
    assert!(err.contains("未知属性 \"of\""), "{err}");
    assert!(err.contains("meta.z"), "错误应定位到嵌套字段: {err}");
}

#[test]
fn n5_indexes_must_be_array() {
    let err = reg_err(json!({
        "name": "Post",
        "collection": "posts",
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "indexes": { "keys": { "title": 1 } },
    }));
    assert!(err.contains("indexes 必须为数组"), "{err}");
}

#[test]
fn n5_index_keys_must_reference_declared_fields() {
    let err = reg_err(json!({
        "name": "Post",
        "collection": "posts",
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "indexes": [ { "keys": { "typo": 1 } } ],
    }));
    assert!(err.contains("未在字段/关系中声明"), "{err}");
}

#[test]
fn n5_unknown_top_level_key_is_rejected() {
    let err = reg_err(json!({
        "name": "Post",
        "collection": "posts",
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "ownerkey": "createdBy",
    }));
    assert!(err.contains("未知顶层键 \"ownerkey\""), "{err}");
}

/// 合法定义（含索引引用关系）不受影响
#[test]
fn n5_valid_index_and_relation_pass() {
    let mut reg = Registry::new();
    reg.register(&json!({
        "name": "Post",
        "collection": "posts",
        "idPrefix": "p",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": { "author": { "model": "User", "local": "authorId", "foreign": "_id" } },
        "indexes": [ { "keys": { "title": 1, "author": 1 } } ],
    }))
    .expect("合法定义应注册成功");
    assert!(reg.get("Post").unwrap().fields.contains_key("title"));
}
