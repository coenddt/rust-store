//! 04 落点择优与写路径：A4（读择优）+ A3（写单连接判决）用例。
//!
//! 定义文件零落点：schema 不含 `source`/`database`/`schema`；落点由 [`Location`]
//! 在 `register_batch` 时注入（01 落地的链路模型：主 + 从，主在首位）。

use serde_json::{json, Value};

use rust_store_core::command::{
    plan_insert, plan_mutation, WriteLinkPolicy, ERR_WRITE_CROSS_SOURCE_PREFIX,
};
use rust_store_core::federation::plan_federated;
use rust_store_core::schema::{Location, Registry};

// ── 公共夹具 ─────────────────────────────────────────────────

fn loc(source: &str, database: Option<&str>, schema: Option<&str>) -> Location {
    Location {
        source: source.to_string(),
        database: database.map(String::from),
        schema: schema.map(String::from),
    }
}

fn registry_of(items: &[(Value, Location)]) -> Registry {
    let mut registry = Registry::new();
    registry
        .register_batch(items, None)
        .expect("schema 注册失败");
    registry
}

fn sources_of(plan: &Value) -> Vec<Value> {
    plan.get("sources")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn edges_of(plan: &Value) -> Vec<Value> {
    plan.get("join")
        .and_then(|j| j.get("edges"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn order_defn() -> Value {
    json!({
        "name": "Order", "collection": "orders", "timestamps": false, "idPrefix": "ord",
        "fields": { "code": { "type": "string" } },
        "relations": {}
    })
}

fn replica_defn() -> Value {
    json!({ "name": "Order", "replica": true })
}

/// User 主链路 (db=null) + 从链路 (orders_db)；Order 主链路 (orders_db) + 从链路 (users_db)。
/// 两条链路都落在同一 source，唯 Order 主链路与 User 从链路同 db ⇒ 择优应选 (orders_db, orders_db)。
fn user_order_items() -> Vec<(Value, Location)> {
    vec![
        (
            json!({
                "name": "User", "collection": "users", "timestamps": false, "idPrefix": "usr",
                "fields": { "name": { "type": "string" } },
                "relations": {
                    "orders": { "model": "Order", "type": "many", "localField": "_id", "foreignField": "userId" }
                }
            }),
            loc("mongodb_main", None, None),
        ),
        (
            json!({ "name": "User", "replica": true }),
            loc("mongodb_main", Some("orders_db"), None),
        ),
        (
            json!({
                "name": "Order", "collection": "orders", "timestamps": false, "idPrefix": "ord",
                "fields": { "userId": { "type": "string" }, "code": { "type": "string" } },
                "relations": {}
            }),
            loc("mongodb_main", Some("orders_db"), None),
        ),
        (
            json!({ "name": "Order", "replica": true }),
            loc("mongodb_main", Some("users_db"), None),
        ),
    ]
}

// ── A4：读择优 ───────────────────────────────────────────────

#[test]
fn a4_read_picks_same_db_chain_single_unit() {
    // 两表各有 {同 db 链路, 跨 db 链路} ⇒ 选同 (source, database) ⇒ 下推为单单元
    let registry = registry_of(&user_order_items());
    let ds = json!({ "sources": { "mongodb_main": "mongo" } });
    let plan = plan_federated(
        "User{name, orders{code}}",
        &Default::default(),
        &registry,
        None,
        &ds,
    )
    .expect("联邦规划失败");

    let sources = sources_of(&plan);
    assert_eq!(sources.len(), 1, "应选同 db 链路下推为单单元: {}", plan);
    assert!(edges_of(&plan).is_empty(), "下推后不应有内存 join 边");
    assert_eq!(sources[0]["source"], "mongodb_main");
    assert_eq!(
        sources[0]["database"], "orders_db",
        "选中链路应为 Order 主链路所在 db"
    );
}

#[test]
fn a4_read_without_common_db_minimizes_units() {
    // 无完全同 db 组合 ⇒ 最小化切分组数（此处只能拆 2 组、1 条 join 边）
    let items = vec![
        (
            json!({
                "name": "User", "collection": "users", "timestamps": false, "idPrefix": "usr",
                "fields": { "name": { "type": "string" } },
                "relations": {
                    "orders": { "model": "Order", "type": "many", "localField": "_id", "foreignField": "userId" }
                }
            }),
            loc("mongodb_main", None, None),
        ),
        (
            json!({ "name": "User", "replica": true }),
            loc("mongodb_main", Some("users_db"), None),
        ),
        (
            json!({
                "name": "Order", "collection": "orders", "timestamps": false, "idPrefix": "ord",
                "fields": { "userId": { "type": "string" }, "code": { "type": "string" } },
                "relations": {}
            }),
            loc("mongodb_main", Some("orders_db"), None),
        ),
        (
            json!({ "name": "Order", "replica": true }),
            loc("mongodb_main", Some("archive_db"), None),
        ),
    ];
    let registry = registry_of(&items);
    let ds = json!({ "sources": { "mongodb_main": "mongo" } });
    let plan = plan_federated(
        "User{name, orders{code}}",
        &Default::default(),
        &registry,
        None,
        &ds,
    )
    .expect("联邦规划失败");

    assert_eq!(
        sources_of(&plan).len(),
        2,
        "无同 db 组合应拆 2 组: {}",
        plan
    );
    assert_eq!(edges_of(&plan).len(), 1);
}

// ── A3：写单连接判决 ─────────────────────────────────────────

/// 同连接多落点：主 (mongodb_main, orders_db) + 从 (mongodb_main, archived_db)
fn order_same_source_items() -> Vec<(Value, Location)> {
    vec![
        (order_defn(), loc("mongodb_main", Some("orders_db"), None)),
        (
            replica_defn(),
            loc("mongodb_main", Some("archived_db"), None),
        ),
    ]
}

/// 跨连接多落点：主 (mongodb_main, orders_db) + 从 (pg_main, analytics)
fn order_cross_source_items() -> Vec<(Value, Location)> {
    vec![
        (order_defn(), loc("mongodb_main", Some("orders_db"), None)),
        (replica_defn(), loc("pg_main", Some("analytics"), None)),
    ]
}

#[test]
fn a3_same_connection_write_is_atomic() {
    let registry = registry_of(&order_same_source_items());
    let plan = plan_insert(
        "Order",
        &registry,
        None,
        &json!({ "_id": "o1", "code": "C1" }),
        0,
        "",
        None,
    )
    .expect("同连接写应放行");

    let wl = &plan["writeLinks"];
    assert_eq!(wl["atomic"], true, "同连接应可同事务: {}", plan);
    assert_eq!(wl["source"], "mongodb_main");
    let targets = wl["targets"].as_array().expect("targets 应为数组");
    assert_eq!(targets.len(), 2, "targets 应覆盖 2 落点: {}", plan);
    assert!(targets.iter().all(|t| t["source"] == "mongodb_main"));
}

#[test]
fn a3_same_connection_mutation_attaches_write_links() {
    let registry = registry_of(&order_same_source_items());
    let out = plan_mutation(
        "Order",
        &registry,
        None,
        &json!({ "_id": "o1", "code": "C1" }),
        0,
        &[],
    )
    .expect("同连接 mutation 应放行");

    let wl = &out["writeLinks"];
    assert_eq!(wl["atomic"], true, "{}", out);
    assert_eq!(wl["targets"].as_array().unwrap().len(), 2);
}

#[test]
fn a3_cross_connection_write_rejected_by_default() {
    let registry = registry_of(&order_cross_source_items());
    assert_eq!(registry.write_link_policy(), WriteLinkPolicy::Reject);

    let err = plan_insert(
        "Order",
        &registry,
        None,
        &json!({ "_id": "o1", "code": "C1" }),
        0,
        "",
        None,
    )
    .expect_err("跨连接写默认应拒绝");
    assert!(
        err.starts_with(ERR_WRITE_CROSS_SOURCE_PREFIX),
        "错误前缀不正确: {err}"
    );
}

#[test]
fn a3_cross_connection_primary_only_degrades_not_atomic() {
    let mut registry = registry_of(&order_cross_source_items());
    registry.set_write_link_policy(WriteLinkPolicy::PrimaryOnly);

    let plan = plan_insert(
        "Order",
        &registry,
        None,
        &json!({ "_id": "o1", "code": "C1" }),
        0,
        "",
        None,
    )
    .expect("PrimaryOnly 策略应降级放行");

    assert_eq!(
        plan["writeLinks"]["atomic"], false,
        "从链路未同步 ⇒ 非原子: {}",
        plan
    );
    let codes: Vec<&str> = plan["degraded"]
        .as_array()
        .expect("plan.degraded 应存在")
        .iter()
        .filter_map(|d| d["code"].as_str())
        .collect();
    assert!(codes.contains(&"writePrimaryOnly"), "plan = {plan}");
    assert_eq!(
        plan["writeLinks"]["degraded"][0]["code"], "writePrimaryOnly",
        "writeLinks 内也应带降级声明: {}",
        plan
    );
}

#[test]
fn single_location_write_plan_shape_unchanged() {
    // 单落点 schema ⇒ 不追加 writeLinks / degraded（零 parity 变更）
    let registry = registry_of(&[(order_defn(), loc("mongodb_main", Some("orders_db"), None))]);
    let plan = plan_insert(
        "Order",
        &registry,
        None,
        &json!({ "_id": "o1", "code": "C1" }),
        0,
        "",
        None,
    )
    .expect("单落点写应放行");
    assert!(plan.get("writeLinks").is_none(), "plan = {plan}");
    assert!(plan.get("degraded").is_none(), "plan = {plan}");
}
