//! 02 步骤3：链路声明装配（`Registry::apply_link_decls`）。
//!
//! 覆盖：命中按来源写回 `role`/`priority`、缺失按声明序追加、归档随附表继承、
//! 未声明 entry 原样（零回归）、未注册 schema ⇒ `Err` 且零副作用。

use serde_json::{json, Value};

use rust_store_core::schema::{LoadConfig, Location, Registry, Role};

fn loc(source: &str, database: Option<&str>, schema: Option<&str>) -> Location {
    Location {
        source: source.to_string(),
        database: database.map(String::from),
        schema: schema.map(String::from),
        role: None,
        priority: None,
    }
}

fn order_defn() -> Value {
    json!({
        "name": "Order", "collection": "orders", "timestamps": false,
        "fields": { "code": { "type": "string" } },
        "relations": {}
    })
}

fn replica_defn() -> Value {
    json!({ "name": "Order", "replica": true })
}

fn item_defn() -> Value {
    json!({ "name": "Item", "collection": "items", "timestamps": false })
}

/// 三连接配置 + 指定 `links`（`defs` 占位，装配不依赖目录扫描）。
fn cfg(links: Value) -> LoadConfig {
    LoadConfig::from_json(&json!({
        "sources": {
            "mongoMain": { "kind": "mongodb", "databases": ["sales_db"] },
            "mongoAlt": { "kind": "mongodb", "databases": ["analytics_db"] },
            "mongoCache": { "kind": "mongodb", "databases": ["cache_db"] }
        },
        "defs": ["schema"],
        "links": links
    }))
    .expect("配置应可解析")
}

// ── 命中写回 + 缺失追加（声明序） ──

#[test]
fn apply_link_decls_attaches_and_appends() {
    let mut r = Registry::new();
    r.register_batch(
        &[
            (order_defn(), loc("mongoMain", Some("sales_db"), None)),
            (replica_defn(), loc("mongoAlt", Some("analytics_db"), None)),
        ],
        None,
    )
    .unwrap();
    // 装配前：两条链路均无 role/priority
    assert_eq!(r.links_of("Order").unwrap().len(), 2);
    assert!(r.links_of("Order").unwrap().iter().all(|l| l.role.is_none()));

    let c = cfg(json!({
        "Order": [
            // 缺失链路（声明序在前）⇒ 追加
            { "source": "mongoCache", "database": "cache_db", "role": "cache", "priority": 5 },
            // 命中既有主链路（来源同址）⇒ 写回
            { "source": "mongoMain", "database": "sales_db", "role": "primary", "priority": 1 }
        ]
    }));
    r.apply_link_decls(&c).expect("装配应成功");

    let links = r.links_of("Order").unwrap();
    assert_eq!(links.len(), 3, "命中 2 条 + 追加 1 条");
    assert_eq!(links[0].source, "mongoMain");
    assert_eq!(links[0].database.as_deref(), Some("sales_db"));
    assert_eq!(links[0].role, Some(Role::Primary));
    assert_eq!(links[0].priority, Some(1));
    // 未声明的既有从链路：原样（role/priority 仍为空）
    assert_eq!(links[1].database.as_deref(), Some("analytics_db"));
    assert_eq!(links[1].role, None);
    assert_eq!(links[1].priority, None);
    // 追加链路按声明序落在尾部
    assert_eq!(links[2].source, "mongoCache");
    assert_eq!(links[2].database.as_deref(), Some("cache_db"));
    assert_eq!(links[2].role, Some(Role::Cache));
    assert_eq!(links[2].priority, Some(5));

    // 归档随附表继承主的重算链路
    assert_eq!(r.links_of("OrderDeleted").unwrap(), links);

    // 非重注册：version 不变
    assert_eq!(r.version_of("Order").unwrap(), 1);
}

// ── 未声明 entry 原样（零回归） ──

#[test]
fn apply_link_decls_leaves_undeclared_entries_untouched() {
    let mut r = Registry::new();
    r.register_batch(
        &[
            (order_defn(), loc("mongoMain", Some("sales_db"), None)),
            (item_defn(), loc("mongoAlt", Some("analytics_db"), None)),
        ],
        None,
    )
    .unwrap();

    let c = cfg(json!({
        "Order": [
            { "source": "mongoMain", "database": "sales_db", "role": "primary", "priority": 2 }
        ]
    }));
    r.apply_link_decls(&c).unwrap();

    // 声明的 Order 已附着
    assert_eq!(r.links_of("Order").unwrap()[0].role, Some(Role::Primary));
    assert_eq!(r.links_of("Order").unwrap()[0].priority, Some(2));

    // 未声明的 Item：链路原样
    let item = r.links_of("Item").unwrap();
    assert_eq!(item.len(), 1);
    assert_eq!(item[0].database.as_deref(), Some("analytics_db"));
    assert_eq!(item[0].role, None);
    assert_eq!(item[0].priority, None);
    // ItemDeleted 同样原样
    assert_eq!(r.links_of("ItemDeleted").unwrap()[0].role, None);
}

// ── 未注册 schema ⇒ Err，且零副作用 ──

#[test]
fn apply_link_decls_rejects_unknown_schema() {
    let mut r = Registry::new();
    r.register_batch(
        &[(order_defn(), loc("mongoMain", Some("sales_db"), None))],
        None,
    )
    .unwrap();

    let c = cfg(json!({
        "Ghost": [
            { "source": "mongoMain", "database": "sales_db", "role": "primary" }
        ]
    }));
    let err = r
        .apply_link_decls(&c)
        .expect_err("引用未注册 schema 应报错");
    assert!(err.contains("未注册 schema"), "错误信息异常: {err}");

    // 零副作用：Order 链路未被改动；Ghost 未注册
    assert_eq!(r.links_of("Order").unwrap()[0].role, None);
    assert!(!r.has("Ghost"));
}

// ── 空 links ⇒ no-op ──

#[test]
fn apply_link_decls_empty_is_noop() {
    let mut r = Registry::new();
    r.register_batch(
        &[(order_defn(), loc("mongoMain", Some("sales_db"), None))],
        None,
    )
    .unwrap();
    let c = LoadConfig::from_json(&json!({
        "sources": { "mongoMain": { "kind": "mongodb", "databases": ["sales_db"] } },
        "defs": ["schema"]
    }))
    .unwrap();
    r.apply_link_decls(&c).unwrap();
    assert_eq!(r.links_of("Order").unwrap()[0].role, None);
}
