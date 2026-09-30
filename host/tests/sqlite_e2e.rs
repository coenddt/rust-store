//! 端到端集成测试：真实 SQLite（内存库）全链路
//! register → DDL → insert → GQL 查询 → update（探针重入）→ remove（归档编排）

use rust_store::Store;
use rust_store_core::permission::Context;
use serde_json::{json, Map, Value};

fn params(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

async fn fresh_store() -> Store {
    let store = Store::connect_sqlite("sqlite::memory:").await.expect("连接失败");
    store
        .register(&json!({
            "name": "user",
            "collection": "user",
            "idPrefix": "u",
            "fields": {
                "name": { "type": "string" },
                "age": { "type": "number" }
            }
        }))
        .expect("注册失败");
    // SQL 后端每表必建 __present 哨兵列（存「显式存在字段集合」；见 nodejs-store/src/ddl.js:10
    // 与 core dialect/write/insert.rs::present_value —— dialect 层物理契约）
    let pool = store.sqlite_pool().expect("SQLite 源");
    sqlx::query("CREATE TABLE \"user\" (_id TEXT PRIMARY KEY, name TEXT, age REAL, createdAt INTEGER, updatedAt INTEGER, \"__present\" TEXT)")
        .execute(pool)
        .await
        .expect("建表失败");
    sqlx::query("CREATE TABLE \"user_deleted\" (_id TEXT PRIMARY KEY, name TEXT, age REAL, createdAt INTEGER, updatedAt INTEGER, deletedAt INTEGER, \"__present\" TEXT)")
        .execute(pool)
        .await
        .expect("建归档表失败");
    store
}

#[tokio::test]
async fn insert_query_update_remove_e2e() {
    let store = fresh_store().await;

    // insert
    let doc = store
        .insert("user", &json!({ "name": "alice", "age": 30 }), None)
        .await
        .expect("insert 失败");
    assert_eq!(doc["name"], "alice");
    let uid = doc["_id"].as_str().expect("插入结果应有 _id").to_string();
    assert!(uid.starts_with('u'), "idPrefix 应生效: {uid}");
    assert!(doc["createdAt"].is_i64(), "timestamps 应生效");

    // query（GQL 条件 + 命名参数）
    let rows = store
        .query(
            "user($condition: @c0) { name, age }",
            &params(&[("c0", json!({ "age": { "$gte": 18 } }))]),
            None,
        )
        .await
        .expect("query 失败");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "alice");
    assert_eq!(rows[0]["age"].as_f64(), Some(30.0));

    // query_one：命中
    let one = store
        .query_one("user($condition: @c0) { name, age }", &params(&[("c0", json!({ "_id": uid.clone() }))]), None)
        .await
        .expect("query_one 失败");
    assert!(one.is_some());

    // update（探针重入路径）
    let updated = store
        .update("user", &json!({ "_id": uid.clone() }), &json!({ "age": 31 }), None)
        .await
        .expect("update 失败");
    assert_eq!(updated.expect("update 应命中")["age"].as_f64(), Some(31.0));

    // 权限：无权限 ctx 拒绝写（core ERR_NO_WRITE，宿主原样透传）
    let guest = Context {
        user_id: Some("guest".into()),
        roles: Some(vec!["guest".into()]),
        role: None,
        internal: false,
    };
    let denied = store
        .update("user", &json!({ "_id": uid }), &json!({ "age": 1 }), Some(&guest))
        .await;
    assert!(denied.is_err(), "guest 写入应被拒绝");
    assert!(denied.unwrap_err().starts_with("ERR_PERMISSION"));

    // remove：归档 + 删除
    let out = store
        .remove("user", &json!({ "_id": uid.clone() }), None)
        .await
        .expect("remove 失败");
    assert_eq!(out["deletedCount"], 1, "删除计数");
    assert_eq!(out["archivedCount"], 1, "归档计数");

    // 删除后查不到
    let after = store
        .query_one("user($condition: @c0) { name, age }", &params(&[("c0", json!({ "_id": uid }))]), None)
        .await
        .expect("query 失败");
    assert!(after.is_none());

    // 归档表有记录（直接 SQL 验证）
    let archived: (String,) = sqlx::query_as("SELECT name FROM \"user_deleted\" LIMIT 1")
        .fetch_one(store.sqlite_pool().expect("SQLite 源"))
        .await
        .expect("归档表查询失败");
    assert_eq!(archived.0, "alice");
}
