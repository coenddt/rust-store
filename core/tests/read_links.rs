//! 02 步骤4：读链路解析（`Registry::resolve_read_links`）。
//!
//! 覆盖：`role`/`priority` 乱序 ⇒ 排序（role 层 → priority 升序 → 声明序）；
//! 未声明 ⇒ 原序（零回归）；传 `unreachable` ⇒ 剔除命中且产 `readLinkUnreachable`
//! 降级声明；全跳过 ⇒ `ERR_READ_NO_REACHABLE_LINK`。

use serde_json::{json, Value};

use rust_store_core::schema::{LoadConfig, Location, Registry};

fn loc(source: &str, database: Option<&str>) -> Location {
    Location {
        source: source.to_string(),
        database: database.map(String::from),
        schema: None,
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

/// 三连接配置 + 指定 `links`（`defs` 占位，装配不依赖目录扫描）。
fn cfg(links: Value) -> LoadConfig {
    LoadConfig::from_json(&json!({
        "sources": {
            "s_primary": { "kind": "mongodb", "databases": ["db1"] },
            "s_replica": { "kind": "mongodb", "databases": ["db2"] },
            "s_cache": { "kind": "mongodb", "databases": ["db3"] }
        },
        "defs": ["schema"],
        "links": links
    }))
    .expect("配置应可解析")
}

/// 主 + 2 从，共三条链路（`links_of` 原序 = [s_primary, s_replica, s_cache]）。
fn registry_with_three_links() -> Registry {
    let mut r = Registry::new();
    r.register_batch(
        &[
            (order_defn(), loc("s_primary", Some("db1"))),
            (replica_defn(), loc("s_replica", Some("db2"))),
            (replica_defn(), loc("s_cache", Some("db3"))),
        ],
        None,
    )
    .unwrap();
    r
}

fn sources_of(r: &Registry, name: &str, unreachable: Option<&[String]>) -> Vec<String> {
    r.resolve_read_links(name, unreachable)
        .unwrap()
        .links
        .iter()
        .map(|l| l.source.clone())
        .collect()
}

// ── 排序：role 层优先（声明序刻意打乱） ──

#[test]
fn resolve_read_links_sorts_by_role_then_priority() {
    let mut r = registry_with_three_links();
    let c = cfg(json!({
        "Order": [
            // 声明序 = cache → replica → primary（与期望结果相反）
            { "source": "s_cache", "database": "db3", "role": "cache", "priority": 1 },
            { "source": "s_replica", "database": "db2", "role": "replica", "priority": 0 },
            { "source": "s_primary", "database": "db1", "role": "primary", "priority": 9 }
        ]
    }));
    r.apply_link_decls(&c).unwrap();

    let rl = r.resolve_read_links("Order", None).unwrap();
    assert!(rl.degraded.is_empty(), "无 unreachable ⇒ 无降级声明");
    let sources: Vec<&str> = rl.links.iter().map(|l| l.source.as_str()).collect();
    assert_eq!(
        sources,
        vec!["s_primary", "s_replica", "s_cache"],
        "role 层序：primary → replica → cache（与 priority 无关）"
    );
}

// ── 同 role 内 priority 升序；未声明 priority 视作最大 ──

#[test]
fn resolve_read_links_priority_ascending_and_unset_last() {
    let mut r = registry_with_three_links();
    let c = cfg(json!({
        "Order": [
            // 未声明 priority ⇒ 视作最大，排同级末位
            { "source": "s_replica", "database": "db2", "role": "replica" },
            { "source": "s_cache", "database": "db3", "role": "replica", "priority": -2 },
            { "source": "s_primary", "database": "db1", "role": "primary", "priority": 100 }
        ]
    }));
    r.apply_link_decls(&c).unwrap();

    assert_eq!(
        sources_of(&r, "Order", None),
        vec!["s_primary", "s_cache", "s_replica"],
        "role 层优先（primary 的 priority=100 不影响排首）；replica 内 priority 升序（-2 < 未声明=MAX）"
    );
}

// ── 零回归：无 links 声明 ⇒ 原序 ──

#[test]
fn resolve_read_links_no_decl_keeps_original_order() {
    let r = registry_with_three_links();
    let rl = r.resolve_read_links("Order", None).unwrap();
    assert!(rl.degraded.is_empty());
    let sources: Vec<&str> = rl.links.iter().map(|l| l.source.as_str()).collect();
    assert_eq!(
        sources,
        vec!["s_primary", "s_replica", "s_cache"],
        "全无 role/priority ⇒ 键全等 ⇒ 稳定排序保声明序"
    );
}

// ── 跳过不可达：剔除命中 + 产降级声明 ──

#[test]
fn resolve_read_links_skips_unreachable_and_declares() {
    let r = registry_with_three_links();
    let unreachable = vec!["s_replica".to_string()];
    let rl = r.resolve_read_links("Order", Some(&unreachable)).unwrap();

    let sources: Vec<&str> = rl.links.iter().map(|l| l.source.as_str()).collect();
    assert_eq!(sources, vec!["s_primary", "s_cache"], "命中 source 被剔除");

    assert_eq!(rl.degraded.len(), 1, "每条被跳过的链路一条声明");
    let d = &rl.degraded[0];
    assert_eq!(d["code"], "readLinkUnreachable");
    assert_eq!(d["layer"], "schema");
    assert_eq!(d["schema"], "Order");
    assert_eq!(d["source"], "s_replica");
    assert!(
        d["message"].as_str().unwrap().contains("s_replica"),
        "message 应含被跳过的 source: {d}"
    );
    assert!(!d["hint"].as_str().unwrap().is_empty(), "hint 非空");
}

// ── 全跳过 ⇒ ERR_READ_NO_REACHABLE_LINK（绝不回落主链路） ──

#[test]
fn resolve_read_links_all_unreachable_err() {
    let r = registry_with_three_links();
    let unreachable = vec![
        "s_primary".to_string(),
        "s_replica".to_string(),
        "s_cache".to_string(),
    ];
    let err = r
        .resolve_read_links("Order", Some(&unreachable))
        .expect_err("全部链路不可达应 Err");
    assert!(
        err.starts_with("ERR_READ_NO_REACHABLE_LINK"),
        "错误前缀异常: {err}"
    );
    assert!(err.contains("Order"), "错误信息应含 schema 名: {err}");
}
