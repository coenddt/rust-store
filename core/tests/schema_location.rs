//! 01 core 定位模型：批次唯一性（A1 / D13）+ 主从链路（A3）+ 落点定位。
//!
//! 定义文件零落点：落点由 [`Location`] 在注册批次时注入。

use serde_json::json;

use rust_store_core::schema::{Location, Registry};

fn loc(source: &str, database: Option<&str>, schema: Option<&str>) -> Location {
    Location {
        source: source.to_string(),
        database: database.map(String::from),
        schema: schema.map(String::from),
    }
}

fn order_defn() -> serde_json::Value {
    json!({
        "name": "Order", "collection": "orders", "timestamps": false,
        "fields": { "code": { "type": "string" } },
        "relations": {}
    })
}

fn replica_defn() -> serde_json::Value {
    json!({ "name": "Order", "replica": true })
}

// ── A1：同批同名主定义 ⇒ 报错（服务不启动） ──

#[test]
fn a1_duplicate_primary_in_batch_errors() {
    let mut r = Registry::new();
    let items = vec![
        (order_defn(), loc("mongo_main", Some("sales_db"), None)),
        (order_defn(), loc("mongo_main", Some("analytics_db"), None)),
    ];
    let err = r
        .register_batch(&items, None)
        .expect_err("同批两个同名主定义应报错");
    assert!(err.contains("同名主定义重复"), "错误信息异常: {}", err);
    // 零副作用：判决先于落库，未写入任何定义
    assert!(!r.has("Order"));
    assert!(r.list().is_empty());
}

#[test]
fn a1_cross_batch_reload_bumps_version() {
    let mut r = Registry::new();
    r.register_batch(
        &[(order_defn(), loc("mongo_main", Some("sales_db"), None))],
        None,
    )
    .unwrap();
    assert_eq!(r.version_of("Order").unwrap(), 1);
    // 跨批同 name = 更新（version + 1），服务正常
    r.register_batch(
        &[(order_defn(), loc("mongo_main", Some("sales_db"), None))],
        None,
    )
    .unwrap();
    assert_eq!(r.version_of("Order").unwrap(), 2);
    assert_eq!(
        r.list(),
        vec!["Order".to_string(), "OrderDeleted".to_string()]
    );
}

// ── A3：同一份主结构 + 两条链路（主 + replica） ──

#[test]
fn a3_primary_plus_replica_makes_links() {
    let mut r = Registry::new();
    let items = vec![
        (order_defn(), loc("mongo_main", Some("sales_db"), None)),
        (
            replica_defn(),
            loc("mongo_main", Some("analytics_db"), None),
        ),
    ];
    r.register_batch(&items, None).unwrap();

    let links = r.links_of("Order").unwrap();
    assert_eq!(links.len(), 2, "主 + 从 = 两条链路");
    assert_eq!(
        links[0].database.as_deref(),
        Some("sales_db"),
        "links[0] 恒为主"
    );
    assert_eq!(links[1].database.as_deref(), Some("analytics_db"));

    // 从定义不注册新定义（只有主 + 归档附表）
    assert_eq!(
        r.list(),
        vec!["Order".to_string(), "OrderDeleted".to_string()]
    );
    // 主结构落点 = 主链路
    assert_eq!(
        r.primary_location("Order").unwrap().database.as_deref(),
        Some("sales_db")
    );
    assert_eq!(r.get("Order").unwrap().database(), Some("sales_db"));
}

#[test]
fn a3_replica_without_primary_or_existing_errors() {
    let mut r = Registry::new();
    let err = r
        .register_batch(
            &[(
                replica_defn(),
                loc("mongo_main", Some("analytics_db"), None),
            )],
            None,
        )
        .expect_err("无主定义的从定义应报错");
    assert!(err.contains("未找到主 schema"), "错误信息异常: {}", err);
}

#[test]
fn a3_replica_appends_link_to_existing_across_batch() {
    let mut r = Registry::new();
    r.register_batch(
        &[(order_defn(), loc("mongo_main", Some("sales_db"), None))],
        None,
    )
    .unwrap();
    assert_eq!(r.links_of("Order").unwrap().len(), 1);
    // 跨批：仅从定义 ⇒ 给既有主追加链路
    r.register_batch(
        &[(
            replica_defn(),
            loc("mongo_main", Some("analytics_db"), None),
        )],
        None,
    )
    .unwrap();
    assert_eq!(r.links_of("Order").unwrap().len(), 2);
    assert_eq!(r.version_of("Order").unwrap(), 2);
}

// ── 定位四元组 / 冲突检测 ──

#[test]
fn get_by_location_four_tuple_hits_any_link() {
    let mut r = Registry::new();
    r.register_batch(
        &[
            (order_defn(), loc("mongo_main", Some("sales_db"), None)),
            (
                replica_defn(),
                loc("mongo_main", Some("analytics_db"), None),
            ),
        ],
        None,
    )
    .unwrap();
    assert!(r
        .get_by_location("mongo_main", Some("sales_db"), None, "orders")
        .is_ok());
    assert!(r
        .get_by_location("mongo_main", Some("analytics_db"), None, "orders")
        .is_ok());
    assert!(r
        .get_by_location("mongo_main", Some("other_db"), None, "orders")
        .is_err());
}

#[test]
fn location_conflict_across_names_errors() {
    let mut r = Registry::new();
    let items = vec![
        (
            json!({ "name": "A", "collection": "dup", "timestamps": false }),
            loc("s", Some("db"), None),
        ),
        (
            json!({ "name": "B", "collection": "dup", "timestamps": false }),
            loc("s", Some("db"), None),
        ),
    ];
    let err = r
        .register_batch(&items, None)
        .expect_err("跨名占用同一四元组应报错");
    assert!(err.contains("定位冲突"), "错误信息异常: {}", err);
    assert!(!r.has("A") && !r.has("B"), "零副作用");
}

#[test]
fn default_source_is_none_and_pg_schema_layer_kept() {
    let mut r = Registry::new();
    r.register_batch(
        &[
            (
                json!({ "name": "U", "collection": "u", "timestamps": false }),
                Location::default(),
            ),
            (
                json!({ "name": "C", "collection": "c", "timestamps": false }),
                loc("pg1", Some("app_db"), Some("app")),
            ),
        ],
        None,
    )
    .unwrap();
    // source = default ⇒ Schema.source 为 None（保留旧兼容语义）
    assert_eq!(r.get("U").unwrap().source(), "default");
    assert!(r.get("U").unwrap().source.is_none());
    let c = r.get("C").unwrap();
    assert_eq!(c.source(), "pg1");
    assert_eq!(c.database(), Some("app_db"));
    assert_eq!(c.schema(), Some("app"));
}
