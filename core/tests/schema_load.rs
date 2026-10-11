//! 06 目录语义装载：A2 目录即落点 / A1 同名主重复 / A3 主从识别 / A11 PG 落点携带 schema。
//!
//! 纯逻辑（零 IO）：输入 = `store.config.json` JSON + 目录扫描结果（相对 defs-root 的路径）。

use serde_json::{json, Value};

use rust_store_core::schema::{depth_layered, locate, plan_load, LoadConfig, Role};

fn cfg() -> LoadConfig {
    LoadConfig::from_json(&json!({
        "sources": {
            "mongoMain": { "kind": "mongodb", "databases": ["sales_db"] },
            "pgMain": { "kind": "pg", "databases": ["analytics_db"] }
        },
        "defs": ["schema"]
    }))
    .expect("配置应可解析")
}

fn f(rel: &str, defn: Value) -> (String, Value) {
    (rel.to_string(), defn)
}

fn loc_of(
    items: &[(Value, rust_store_core::schema::Location)],
    name: &str,
) -> (String, Option<String>, Option<String>) {
    let hit = items
        .iter()
        .find(|(d, _)| {
            d.get("name").and_then(|v| v.as_str()) == Some(name) && d.get("replica").is_none()
        })
        .expect("应存在主定义");
    (
        hit.1.source.clone(),
        hit.1.database.clone(),
        hit.1.schema.clone(),
    )
}

// ── depth_layered：PG=2，其余=1；kind 非法 ⇒ Err ──

#[test]
fn depth_layered_by_kind() {
    assert_eq!(depth_layered("pg").unwrap(), 2);
    assert_eq!(depth_layered("postgres").unwrap(), 2);
    assert_eq!(depth_layered("mongodb").unwrap(), 1);
    assert_eq!(depth_layered("mongo").unwrap(), 1);
    assert_eq!(depth_layered("mysql").unwrap(), 1);
    assert_eq!(depth_layered("sqlite").unwrap(), 1);
    assert!(depth_layered("oracle").is_err());
}

// ── kind: "memory" 属合法白名单（与 local 同族，深度 = 1） ──

#[test]
fn memory_kind_accepted() {
    assert_eq!(depth_layered("memory").unwrap(), 1);
    let c = LoadConfig::from_json(&json!({
        "sources": { "mem": { "kind": "memory", "databases": ["sales_db"] } },
        "defs": ["schema"]
    }))
    .expect("kind: memory 应可装载");
    let (decl, depth) = c.source_of_db("sales_db").unwrap();
    assert_eq!(decl.kind, "memory");
    assert_eq!(depth, 1);
}

// ── locate：PG L2 = schema；无子目录 ⇒ None；非 PG ⇒ None（L2+ 打平） ──

#[test]
fn locate_pg_reads_l2_others_flatten() {
    let pg = locate("app/Customer.json", "pgMain", "analytics_db", 2).unwrap();
    assert_eq!(pg.schema.as_deref(), Some("app"));
    // PG 直接放文件（无 L2 子目录）⇒ schema = None（回落连接默认 search_path）
    let pg_root = locate("Customer.json", "pgMain", "analytics_db", 2).unwrap();
    assert_eq!(pg_root.schema, None);
    // PG L3 自由目录打平：schema 仍取首段
    let pg_deep = locate("app/report/Monthly.json", "pgMain", "analytics_db", 2).unwrap();
    assert_eq!(pg_deep.schema.as_deref(), Some("app"));
    // 非 PG：L2+ 打平 ⇒ schema 恒 None
    let mongo = locate("inventory/Item.json", "mongoMain", "sales_db", 1).unwrap();
    assert_eq!(mongo.schema, None);
    assert_eq!(mongo.database.as_deref(), Some("sales_db"));
}

// ── LoadConfig：非法即 Err（禁静默兜底） ──

#[test]
fn load_config_rejects_invalid() {
    assert!(LoadConfig::from_json(&json!({ "defs": ["schema"] })).is_err()); // 缺 sources
    assert!(LoadConfig::from_json(&json!({
        "sources": { "x": { "kind": "oracle", "databases": ["d"] } }, "defs": ["schema"]
    }))
    .is_err()); // kind 非法
    assert!(LoadConfig::from_json(&json!({
        "sources": { "x": { "kind": "mongodb", "databases": [] } }, "defs": ["schema"]
    }))
    .is_err()); // databases 空
    assert!(LoadConfig::from_json(&json!({
        "sources": { "x": { "kind": "mongodb", "databases": ["d"] } }, "defs": []
    }))
    .is_err()); // defs 空
}

#[test]
fn source_of_db_conflict_and_undeclared() {
    let c = LoadConfig::from_json(&json!({
        "sources": {
            "a": { "kind": "mongodb", "databases": ["shared"] },
            "b": { "kind": "mysql", "databases": ["shared"] }
        },
        "defs": ["schema"]
    }))
    .unwrap();
    assert!(c.source_of_db("shared").is_err(), "同一 db 挂两连接应报错");
    assert!(c.source_of_db("nope").is_err(), "未声明库应报错");
}

// ── links：链路声明解析与六条校验（A6） ──

#[test]
fn links_default_empty_ok() {
    let c = cfg();
    assert!(c.links.is_empty(), "缺省 links ⇒ 空且不报错（零回归）");
}

#[test]
fn links_parse_roles_priority_and_default_db() {
    let c = LoadConfig::from_json(&json!({
        "sources": {
            "mongoMain": { "kind": "mongodb", "databases": ["sales_db"] },
            "pgMain": { "kind": "pg", "databases": ["analytics_db"] }
        },
        "defs": ["schema"],
        "links": {
            "Order": [
                { "source": "mongoMain", "role": "primary", "priority": -1, "database": "sales_db" },
                { "source": "pgMain", "role": "replica", "schema": "app" }
            ]
        }
    }))
    .expect("links 应可解析");
    assert_eq!(c.links.len(), 1);
    let (name, decls) = &c.links[0];
    assert_eq!(name, "Order");
    assert_eq!(decls.len(), 2, "声明序保持");
    assert_eq!(decls[0].role, Some(Role::Primary));
    assert_eq!(decls[0].priority, Some(-1));
    assert_eq!(decls[0].database.as_deref(), Some("sales_db"));
    assert_eq!(decls[1].role, Some(Role::Replica));
    // database 缺省：源恰 1 库 ⇒ 取唯一库
    assert_eq!(decls[1].database.as_deref(), Some("analytics_db"));
    assert_eq!(decls[1].schema.as_deref(), Some("app"));
}

#[test]
fn links_reject_undeclared_source() {
    let err = LoadConfig::from_json(&json!({
        "sources": { "mongoMain": { "kind": "mongodb", "databases": ["sales_db"] } },
        "defs": ["schema"],
        "links": { "Order": [ { "source": "ghost", "role": "primary" } ] }
    }))
    .expect_err("source 未在 sources 声明应报错");
    assert!(err.contains("未在 sources 中声明"), "错误信息异常: {err}");
}

#[test]
fn links_reject_primary_count() {
    // 0 条
    let e0 = LoadConfig::from_json(&json!({
        "sources": { "m": { "kind": "mongodb", "databases": ["d"] } },
        "defs": ["schema"],
        "links": { "Order": [ { "source": "m", "role": "replica" } ] }
    }))
    .expect_err("0 条 primary 应报错");
    assert!(
        e0.contains("必须恰好一条 role=primary"),
        "错误信息异常: {e0}"
    );
    // 2 条
    let e2 = LoadConfig::from_json(&json!({
        "sources": {
            "m1": { "kind": "mongodb", "databases": ["d1"] },
            "m2": { "kind": "mongodb", "databases": ["d2"] }
        },
        "defs": ["schema"],
        "links": { "Order": [
            { "source": "m1", "role": "primary" },
            { "source": "m2", "role": "primary" }
        ] }
    }))
    .expect_err("2 条 primary 应报错");
    assert!(e2.contains("实际 2 条"), "错误信息异常: {e2}");
}

#[test]
fn links_reject_multi_db_without_database() {
    let err = LoadConfig::from_json(&json!({
        "sources": { "m": { "kind": "mongodb", "databases": ["d1", "d2"] } },
        "defs": ["schema"],
        "links": { "Order": [ { "source": "m", "role": "primary" } ] }
    }))
    .expect_err("多库未给 database 应报错");
    assert!(err.contains("维护多库"), "错误信息异常: {err}");
}

#[test]
fn links_reject_bad_role_and_priority() {
    let err_role = LoadConfig::from_json(&json!({
        "sources": { "m": { "kind": "mongodb", "databases": ["d"] } },
        "defs": ["schema"],
        "links": { "Order": [ { "source": "m", "role": "hot" } ] }
    }))
    .expect_err("非法 role 应报错");
    assert!(err_role.contains("未知链路角色"), "错误信息异常: {err_role}");

    let err_pri = LoadConfig::from_json(&json!({
        "sources": { "m": { "kind": "mongodb", "databases": ["d"] } },
        "defs": ["schema"],
        "links": { "Order": [ { "source": "m", "role": "primary", "priority": 1.5 } ] }
    }))
    .expect_err("非整数 priority 应报错");
    assert!(
        err_pri.contains("priority 须为整数"),
        "错误信息异常: {err_pri}"
    );
}

// ── A2：目录即落点（L1=database；PG L2=schema；L3+ 打平） ──

#[test]
fn a2_directory_is_location() {
    let files = vec![
        f("sales_db/Order.json", json!({ "name": "Order" })),
        f("sales_db/inventory/Item.json", json!({ "name": "Item" })),
        f(
            "analytics_db/app/Customer.json",
            json!({ "name": "Customer" }),
        ),
        f(
            "analytics_db/app/report/Monthly.json",
            json!({ "name": "Monthly" }),
        ),
    ];
    let items = plan_load(&cfg(), &files).unwrap();
    assert_eq!(
        loc_of(&items, "Order"),
        ("mongoMain".into(), Some("sales_db".into()), None)
    );
    assert_eq!(
        loc_of(&items, "Item"),
        ("mongoMain".into(), Some("sales_db".into()), None)
    );
    assert_eq!(
        loc_of(&items, "Customer"),
        (
            "pgMain".into(),
            Some("analytics_db".into()),
            Some("app".into())
        )
    );
    assert_eq!(
        loc_of(&items, "Monthly"),
        (
            "pgMain".into(),
            Some("analytics_db".into()),
            Some("app".into())
        )
    );
}

// ── A1：同名主 ≥2 ⇒ Err ──

#[test]
fn a1_duplicate_primary() {
    let files = vec![
        f("sales_db/Order.json", json!({ "name": "Order" })),
        f("sales_db/inventory/Order.json", json!({ "name": "Order" })),
    ];
    let err = plan_load(&cfg(), &files).expect_err("同名主 ≥2 应报错");
    assert!(err.contains("ERR:LOAD 主定义重复"), "错误信息异常: {err}");
}

// ── A3：主从识别（主在前、其后从；全 replica ⇒ 主缺失） ──

#[test]
fn a3_primary_then_replica() {
    let files = vec![
        f(
            "analytics_db/Order.json",
            json!({ "name": "Order", "replica": true }),
        ),
        f(
            "sales_db/Order.json",
            json!({ "name": "Order", "collection": "order" }),
        ),
    ];
    let items = plan_load(&cfg(), &files).unwrap();
    assert_eq!(items.len(), 2);
    assert!(items[0].0.get("replica").is_none(), "主应在前");
    assert_eq!(items[0].1.database.as_deref(), Some("sales_db"));
    assert_eq!(
        items[1].0.get("replica").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(items[1].1.database.as_deref(), Some("analytics_db"));
}

#[test]
fn a3_missing_primary_errors() {
    let files = vec![f(
        "sales_db/Order.json",
        json!({ "name": "Order", "replica": true }),
    )];
    let err = plan_load(&cfg(), &files).expect_err("全 replica 应报主缺失");
    assert!(err.contains("ERR:LOAD 主定义缺失"), "错误信息异常: {err}");
}

#[test]
fn undeclared_db_errors() {
    let files = vec![f("other_db/Order.json", json!({ "name": "Order" }))];
    let err = plan_load(&cfg(), &files).expect_err("库目录未声明应报错");
    assert!(err.contains("ERR:LOAD 库目录未声明"), "错误信息异常: {err}");
}
