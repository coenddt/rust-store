//! 6.0.0+fix2 upsert 三项缺陷回归：
//! - 缺陷 1（P0）：`$setOnInsert` 不得用 schema default 覆盖 filter 等值键建立的基线；
//! - 缺陷 2：upsert / 原生操作符 update 必须与 insert 同码校验 object 字段类型；
//! - 缺陷 3：schema 声明 `_id.default` 时不得把 `_id` 注入 `$setOnInsert`
//!   （upsert 命中已有文档会触发 MongoDB ImmutableField）。
//!
//! `merge_insert_defaults` 的三个调用点：`plan_upsert` 在本文件覆盖；
//! `build_upsert_update` / `upsert_one_update` 仅 crate 内可见，由
//! `core/src/command/mutate/upsert.rs` 的单元测试覆盖。

use serde_json::{json, Value};

use rust_store_core::command::{plan_update, plan_upsert, Probe};
use rust_store_core::permission::{context_from_value, Context};
use rust_store_core::schema::Registry;

fn registry_with(defns: &[Value]) -> Registry {
    let mut reg = Registry::new();
    for d in defns {
        reg.register(d).expect("schema 注册应成功");
    }
    reg
}

fn ctx_of(v: Value) -> Context {
    context_from_value(&v).expect("ctx JSON 应解析为对象")
}

/// 缺陷 1 fixture：四个字段均声明非空默认值 `""`，filter 等值键与默认值可区分。
fn goods_schema() -> Value {
    json!({
        "name": "GoodsItem",
        "collection": "goods_items",
        "idPrefix": "g",
        "timestamps": false,
        "fields": {
            "userId": { "type": "string", "default": "" },
            "auctionItemId": { "type": "string", "default": "" },
            "moduleType": { "type": "string", "default": "" },
            "resourceId": { "type": "string", "default": "" },
            "code": { "type": "string", "default": "" },
            "title": { "type": "string" }
        },
        "relations": {}
    })
}

fn upsert_update(reg: &Registry, condition: Value, data: Value) -> Value {
    let plan = plan_upsert(
        "GoodsItem",
        reg,
        None,
        &condition,
        &data,
        &json!({}),
        0,
        "g_new",
    )
    .expect("upsert 规划应成功");
    plan["command"]["update"].clone()
}

/// 缺陷 1：filter 直接等值键（`{k: v}`）不得被 schema default 覆盖。
///
/// 修复前：`$setOnInsert` 出现 `userId: "" / auctionItemId: "" / ...`，
/// MongoDB 用 `$setOnInsert` 覆盖 filter 建立的基线，upsert 幂等失效。
#[test]
fn bug1_upsert_skips_filter_equality_keys() {
    let reg = registry_with(&[goods_schema()]);
    let update = upsert_update(
        &reg,
        json!({
            "userId": "u1",
            "auctionItemId": "a1",
            "moduleType": "m1",
            "resourceId": "r1"
        }),
        json!({ "title": "x" }),
    );
    let on_insert = &update["$setOnInsert"];
    for key in ["userId", "auctionItemId", "moduleType", "resourceId"] {
        assert!(
            on_insert.get(key).is_none(),
            "缺陷 1：filter 等值键 {key} 不得补 default（会覆盖基线）: {update}"
        );
    }
}

/// 缺陷 1：filter 显式 `{k: {"$eq": v}}` 同样构成等值基线，不得补 default。
#[test]
fn bug1_upsert_skips_explicit_eq_operator() {
    let reg = registry_with(&[goods_schema()]);
    let update = upsert_update(
        &reg,
        json!({ "code": { "$eq": "c1" } }),
        json!({ "title": "x" }),
    );
    assert!(
        update["$setOnInsert"].get("code").is_none(),
        "缺陷 1：显式 $eq 等值键不得补 default: {update}"
    );
}

/// 缺陷 1 边界：`$in` 不构成单值基线，schema default 仍须正常写入 `$setOnInsert`。
#[test]
fn bug1_non_equality_operator_keeps_default() {
    let reg = registry_with(&[goods_schema()]);
    let update = upsert_update(
        &reg,
        json!({ "code": { "$in": ["c1", "c2"] } }),
        json!({ "title": "x" }),
    );
    assert_eq!(
        update["$setOnInsert"]["code"], "",
        "缺陷 1：$in 不是单值基线，default 仍应写入: {update}"
    );
}

/// 缺陷 1 回归保护：filter 不含该键时 default 仍正常写入 `$setOnInsert`。
#[test]
fn bug1_absent_filter_key_keeps_default() {
    let reg = registry_with(&[goods_schema()]);
    let update = upsert_update(&reg, json!({ "_id": "g1" }), json!({ "title": "x" }));
    assert_eq!(
        update["$setOnInsert"]["userId"], "",
        "缺陷 1：filter 不含 userId 时 default 必须保留: {update}"
    );
}

/// 缺陷 1：creator 行控会把 filter 包进 `$and`，其内部等值键同样是基线。
#[test]
fn bug1_upsert_skips_equality_keys_under_and() {
    let reg = registry_with(&[json!({
        "name": "GoodsItem",
        "collection": "goods_items",
        "idPrefix": "g",
        "timestamps": false,
        "write": ["creator"],
        "fields": {
            "userId": { "type": "string", "default": "" },
            "title": { "type": "string" }
        },
        "relations": {}
    })]);
    let user = ctx_of(json!({ "userId": "u1" }));
    let plan = plan_upsert(
        "GoodsItem",
        &reg,
        Some(&user),
        &json!({ "userId": "u1" }),
        &json!({ "title": "x" }),
        &json!({}),
        0,
        "g_new",
    )
    .expect("upsert 规划应成功");
    let update = &plan["command"]["update"];
    assert!(
        update["$setOnInsert"].get("userId").is_none(),
        "缺陷 1：$and 内等值键不得补 default: {update}"
    );
}

/// 缺陷 2 fixture：`meta` 声明为 object（带 fields）。
fn object_schema() -> Value {
    json!({
        "name": "GoodsItem",
        "collection": "goods_items",
        "idPrefix": "g",
        "timestamps": false,
        "fields": {
            "code": { "type": "string" },
            "title": { "type": "string" },
            "meta": {
                "type": "object",
                "fields": { "a": { "type": "int" } }
            }
        },
        "relations": {}
    })
}

/// 缺陷 2：upsert 路径写入非对象值必须显式报错（与 insert 同码）。
#[test]
fn bug2_upsert_rejects_non_object_for_object_field() {
    let reg = registry_with(&[object_schema()]);
    let err = plan_upsert(
        "GoodsItem",
        &reg,
        None,
        &json!({ "code": "c1" }),
        &json!({ "title": "x", "meta": "notdict" }),
        &json!({}),
        0,
        "g_new",
    )
    .expect_err("缺陷 2：upsert 写入非对象值必须报错");
    assert!(err.contains("声明为 object"), "{err}");
}

/// 缺陷 2：原生操作符 update（`$set`）写入非对象值必须显式报错。
#[test]
fn bug2_raw_update_rejects_non_object_for_object_field() {
    let reg = registry_with(&[object_schema()]);
    let err = plan_update(
        "GoodsItem",
        &reg,
        None,
        &json!({ "_id": "g1" }),
        &json!({ "$set": { "meta": "notdict" } }),
        &json!({}),
        0,
        Probe::NotProbed,
    )
    .expect_err("缺陷 2：原生 $set 写入非对象值必须报错");
    assert!(err.contains("声明为 object"), "{err}");
}

/// 缺陷 2：internal 上下文不豁免 object 类型校验。
#[test]
fn bug2_internal_context_is_not_exempt() {
    let reg = registry_with(&[object_schema()]);
    let sys = Context::system();
    let err = plan_upsert(
        "GoodsItem",
        &reg,
        Some(&sys),
        &json!({ "code": "c1" }),
        &json!({ "title": "x", "meta": "notdict" }),
        &json!({}),
        0,
        "g_new",
    )
    .expect_err("缺陷 2：internal 上下文同样不得绕过 object 校验");
    assert!(err.contains("声明为 object"), "{err}");
}

/// 缺陷 3 fixture：schema 显式声明 `_id.default: ""`（无 idPrefix）。
fn id_default_schema() -> Value {
    json!({
        "name": "GoodsItem",
        "collection": "goods_items",
        "idPrefix": "",
        "timestamps": false,
        "fields": {
            "_id": { "type": "string", "default": "" },
            "code": { "type": "string" },
            "title": { "type": "string" }
        },
        "relations": {}
    })
}

/// 缺陷 3：`_id.default` 不得进入 `$setOnInsert`。
///
/// 修复前：`$setOnInsert: {"_id": ""}`；upsert 命中已有文档的更新分支时
/// MongoDB 直接报 ImmutableField（update on the path '_id'）。
#[test]
fn bug3_upsert_never_puts_id_into_set_on_insert_by_default() {
    let reg = registry_with(&[id_default_schema()]);
    let update = upsert_update(&reg, json!({ "code": "c1" }), json!({ "title": "x" }));
    let has_id = update
        .get("$setOnInsert")
        .and_then(|v| v.get("_id"))
        .is_some();
    assert!(
        !has_id,
        "缺陷 3：schema 的 _id.default 不得注入 $setOnInsert: {update}"
    );
}

/// 缺陷 3 回归保护：显式传入的 `_id` 仍必须进 `$setOnInsert`（新建分支需要主键）。
#[test]
fn bug3_explicit_id_still_goes_to_set_on_insert() {
    let reg = registry_with(&[id_default_schema()]);
    let update = upsert_update(
        &reg,
        json!({ "code": "c1" }),
        json!({ "_id": "g-explicit", "title": "x" }),
    );
    assert_eq!(
        update["$setOnInsert"]["_id"], "g-explicit",
        "缺陷 3：显式 _id 必须保留: {update}"
    );
}
