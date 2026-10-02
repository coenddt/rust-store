//! 真实 MySQL / PostgreSQL 端到端测试（显式 opt-in：设 RUST_STORE_REAL_DB=1 才运行）。
//! 凭证与 py-store 场景测试一致（e2e/e2e123@127.0.0.1/mongo_store_e2e，可用 MYSQL_URI / PG_URI 覆盖）。
//!
//! 验证点：CRUD 全链路（三后端方言）+ 显式 null（F-07 三态，重点验证 PG 占位符内联）+ 归档事务化。
#![cfg(feature = "real-db")]

use rust_store::Store;
use serde_json::{json, Map, Value};

fn params(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn mysql_uri() -> String {
    std::env::var("MYSQL_URI")
        .unwrap_or_else(|_| "mysql://e2e:e2e123@127.0.0.1:3306/mongo_store_e2e".into())
}

fn pg_uri() -> String {
    std::env::var("PG_URI")
        .unwrap_or_else(|_| "postgres://e2e:e2e123@127.0.0.1:5432/mongo_store_e2e".into())
}

async fn run_e2e(store: &Store, backend_label: &str) {
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

    // insert（含显式 null：F-07 三态 —— PG 侧重点验证占位符内联）
    let doc = store
        .insert("user", &json!({ "name": "alice", "age": 30 }), None)
        .await
        .expect("insert 失败");
    let uid = doc["_id"].as_str().expect("_id").to_string();
    assert!(
        uid.starts_with('u'),
        "{backend_label} idPrefix 应生效: {uid}"
    );

    let doc2 = store
        .insert("user", &json!({ "name": "bob", "age": null }), None)
        .await
        .expect("{backend_label} 显式 null 插入失败（F-07）");
    let uid2 = doc2["_id"].as_str().expect("_id").to_string();

    // GQL 条件查询 + 关系字段还原
    let rows = store
        .query(
            "user($condition: @c0) { name, age }",
            &params(&[("c0", json!({ "age": { "$gte": 18 } }))]),
            None,
        )
        .await
        .expect("query 失败");
    assert_eq!(rows.len(), 1, "{backend_label} 条件命中数");

    // 显式 null 读回三态：age=null 是「显式 null」不是缺失（H-01）
    let one = store
        .query_one(
            "user($condition: @c0) { name, age }",
            &params(&[("c0", json!({ "_id": uid2 }))]),
            None,
        )
        .await
        .expect("query_one 失败")
        .expect("bob 应命中");
    assert!(
        one.get("age").map(|v| v.is_null()).unwrap_or(false),
        "{backend_label} 显式 null 应读回 null 键"
    );

    // update（探针重入 + RETURNING / MySQL 写后回读）
    let updated = store
        .update("user", &json!({ "_id": uid }), &json!({ "age": 31 }), None)
        .await
        .expect("update 失败")
        .expect("应命中");
    assert_eq!(updated["age"].as_f64(), Some(31.0), "{backend_label}");

    // remove：归档 + 删除事务化
    let out = store
        .remove("user", &json!({ "_id": uid }), None)
        .await
        .expect("remove 失败");
    assert_eq!(out["deletedCount"], 1, "{backend_label}");
    assert_eq!(out["archivedCount"], 1, "{backend_label}");

    // 归档幂等（upsertById）：同 id 再删一次不会重复归档（此处已删，deletedCount=0）
    let out2 = store
        .remove("user", &json!({ "_id": uid }), None)
        .await
        .expect("remove 二次失败");
    assert_eq!(out2["deletedCount"], 0, "{backend_label}");

    // 清理（下轮运行幂等）
    store
        .remove("user", &json!({ "_id": uid2 }), None)
        .await
        .ok();
}

#[tokio::test]
async fn mysql_e2e() {
    if std::env::var("RUST_STORE_REAL_DB").ok().as_deref() != Some("1") {
        eprintln!("跳过 MySQL e2e（未设置 RUST_STORE_REAL_DB=1）");
        return;
    }
    let store = Store::connect(&mysql_uri()).await.expect("MySQL 连接失败");
    store
        .execute_ddl(&[
            "DROP TABLE IF EXISTS `user`",
            "DROP TABLE IF EXISTS `user_deleted`",
            "CREATE TABLE `user` (`_id` VARCHAR(64) PRIMARY KEY, name VARCHAR(255), age DOUBLE, createdAt BIGINT, updatedAt BIGINT, `__present` TEXT) ENGINE=InnoDB",
            "CREATE TABLE `user_deleted` (`_id` VARCHAR(64) PRIMARY KEY, name VARCHAR(255), age DOUBLE, createdAt BIGINT, updatedAt BIGINT, `deletedAt` BIGINT, `__present` TEXT) ENGINE=InnoDB",
        ])
        .await
        .expect("MySQL DDL 失败");
    run_e2e(&store, "mysql").await;
}

#[tokio::test]
async fn pg_e2e() {
    if std::env::var("RUST_STORE_REAL_DB").ok().as_deref() != Some("1") {
        eprintln!("跳过 PostgreSQL e2e（未设置 RUST_STORE_REAL_DB=1）");
        return;
    }
    let store = Store::connect(&pg_uri())
        .await
        .expect("PostgreSQL 连接失败");
    store
        .execute_ddl(&[
            "DROP TABLE IF EXISTS \"user\"",
            "DROP TABLE IF EXISTS \"user_deleted\"",
            "CREATE TABLE \"user\" (\"_id\" TEXT PRIMARY KEY, name TEXT, age DOUBLE PRECISION, \"createdAt\" BIGINT, \"updatedAt\" BIGINT, \"__present\" TEXT)",
            "CREATE TABLE \"user_deleted\" (\"_id\" TEXT PRIMARY KEY, name TEXT, age DOUBLE PRECISION, \"createdAt\" BIGINT, \"updatedAt\" BIGINT, \"deletedAt\" BIGINT, \"__present\" TEXT)",
        ])
        .await
        .expect("PostgreSQL DDL 失败");
    run_e2e(&store, "pg").await;
}
