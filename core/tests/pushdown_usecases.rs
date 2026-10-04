//! P5 用例：联邦下推 + 多租户 routeOverride（对应方案 §九 断言 B5-B8）
//!
//! - B5: PG 同连接跨 schema 的 GQL 关联 → 单取数单元（下推），SQL 为
//!   qualified 表名 JOIN（`"app_a"."orders" JOIN "app_b"."order_items"`），**非**内存联邦；
//! - B6: Mongo 同 source 跨 db 关联 → 走内存联邦（sources 拆分 + join 边，
//!   `$lookup` 物理不能跨 db，不误下推）；同 source 同 db 仍 `$lookup` 下推；
//! - B7: mutation 跨源父子文档：各步命令的 `source`/`database`/`schema` =
//!   各自 schema 的落点（逐命令取值，非根源统一）；
//! - B8: 同一条 GQL，不同 `route_override` 落不同租户落点（多租户路由）。
//!
//! 定义文件零落点：用例内 schema 不含 `source`/`database`/`schema`；落点由 [`Location`]
//! 在注册时注入。

use serde_json::{json, Value};

use rust_store_core::command::{apply_route_override, plan_mutation, plan_query};
use rust_store_core::dialect::translate;
use rust_store_core::dialect::Backend;
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

/// 注册一批 (定义, 落点)
fn registry_of(items: &[(Value, Location)]) -> Registry {
    let mut registry = Registry::new();
    registry
        .register_batch(items, None)
        .expect("schema 注册失败");
    registry
}

/// PG 双 schema：Order(app_a.orders) → OrderItem(app_b.order_items)，many 关联
fn pg_cross_schema_items() -> Vec<(Value, Location)> {
    vec![
        (
            json!({
                "name": "Order", "collection": "orders", "timestamps": false,
                "idPrefix": "ord",
                "fields": { "code": { "type": "string" } },
                "relations": {
                    "items": { "model": "OrderItem", "type": "many", "localField": "_id", "foreignField": "orderId" }
                }
            }),
            loc("pg", None, Some("app_a")),
        ),
        (
            json!({
                "name": "OrderItem", "collection": "order_items", "timestamps": false,
                "idPrefix": "itm",
                "fields": { "orderId": { "type": "string" }, "sku": { "type": "string" } },
                "relations": {}
            }),
            loc("pg", None, Some("app_b")),
        ),
    ]
}

/// Mongo 同 source 跨 db：User(默认库) → Order(orders_db)，many 关联
fn mongo_cross_db_items() -> Vec<(Value, Location)> {
    vec![
        (
            json!({
                "name": "User", "collection": "users", "timestamps": false,
                "idPrefix": "usr",
                "fields": { "name": { "type": "string" } },
                "relations": {
                    "orders": { "model": "Order", "type": "many", "localField": "_id", "foreignField": "userId" }
                }
            }),
            loc("mongodb_main", None, None),
        ),
        (
            json!({
                "name": "Order", "collection": "orders", "timestamps": false,
                "idPrefix": "ord",
                "fields": { "userId": { "type": "string" }, "code": { "type": "string" } },
                "relations": {}
            }),
            loc("mongodb_main", Some("orders_db"), None),
        ),
    ]
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

// ── B5：PG 同连接跨 schema → 下推为 qualified JOIN ────────────

#[test]
fn b5_pg_cross_schema_pushdown_single_unit_qualified_join() {
    let registry = registry_of(&pg_cross_schema_items());
    let ds = json!({ "sources": { "pg": "postgres" } });
    let plan = plan_federated(
        "Order{code, items{sku}}",
        &Default::default(),
        &registry,
        None,
        &ds,
    )
    .expect("联邦规划失败");

    // 同 source（SQL）跨 schema → 物理下推：单取数单元，无 join 边
    let sources = sources_of(&plan);
    assert_eq!(
        sources.len(),
        1,
        "SQL 跨 schema 应下推为单单元，实际: {}",
        plan
    );
    assert_eq!(sources[0]["source"], "pg");
    assert_eq!(sources[0]["schema"], "app_a");
    assert!(edges_of(&plan).is_empty(), "下推后不应有内存 join 边");

    // 翻译为 SQL：根表与关联表都带 schema 限定
    let agg = sources[0]["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "aggregate")
        .cloned()
        .expect("应产出 aggregate 命令");
    assert_eq!(agg["source"], "pg");
    assert_eq!(agg["schema"], "app_a");

    let out = translate(Backend::Postgres, &agg, &registry).expect("翻译失败");
    assert!(out["unsupported"].as_array().unwrap().is_empty());
    let text = out["stmts"][0]["text"].as_str().expect("应有 SQL 文本");
    assert!(
        text.contains("\"app_a\".\"orders\""),
        "根表应带 schema 限定: {}",
        text
    );
    assert!(
        text.contains("JOIN \"app_b\".\"order_items\""),
        "关联表应跨 schema JOIN 下推（非内存联邦）: {}",
        text
    );
}

// ── B6：Mongo 跨 db → 内存联邦；同 db → $lookup 下推 ─────────

#[test]
fn b6_mongo_cross_db_strips_to_memory_federation() {
    let registry = registry_of(&mongo_cross_db_items());
    let ds = json!({ "sources": { "mongodb_main": "mongo" } });
    let plan = plan_federated(
        "User{name, orders{code}}",
        &Default::default(),
        &registry,
        None,
        &ds,
    )
    .expect("联邦规划失败");

    // Mongo 跨 db：$lookup 不能跨库 → 拆为两个取数单元 + 一条 join 边
    let sources = sources_of(&plan);
    assert_eq!(sources.len(), 2, "Mongo 跨 db 应拆源，实际: {}", plan);
    let edges = edges_of(&plan);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["rel"], "orders");
    assert_eq!(edges[0]["cardinality"], "many");

    // 各单元定位正确：根 = 默认库（database null），子 = orders_db
    assert_eq!(sources[0]["source"], "mongodb_main");
    assert_eq!(sources[0]["database"], Value::Null);
    assert_eq!(sources[1]["source"], "mongodb_main");
    assert_eq!(sources[1]["database"], "orders_db");
}

#[test]
fn b6_mongo_same_db_still_pushdowns_lookup() {
    // 同 source 同 db（database 均缺省 null）→ $lookup 下推，单单元
    let mut items = mongo_cross_db_items();
    items[1].1 = loc("mongodb_main", None, None);
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

    let sources = sources_of(&plan);
    assert_eq!(sources.len(), 1, "Mongo 同库应 $lookup 下推: {}", plan);
    assert!(edges_of(&plan).is_empty());
}

// ── B7：mutation 跨源父子 → 各步命令按各自 schema 取定位 ──────

#[test]
fn b7_mutation_cross_source_steps_carry_schema_location() {
    let registry = registry_of(&mongo_cross_db_items());
    let out = plan_mutation(
        "User",
        &registry,
        None,
        &json!({ "name": "A", "orders": [ { "code": "C1" } ] }),
        0,
        &["new-id-0".to_string(), "new-id-1".to_string()],
    )
    .expect("mutation 规划失败");

    let steps = out["steps"].as_array().expect("应有 steps");
    assert_eq!(steps.len(), 2);

    // 父步骤（User）：mongodb_main 默认库
    assert_eq!(steps[0]["model"], "User");
    assert_eq!(steps[0]["command"]["kind"], "insertOne");
    assert_eq!(steps[0]["command"]["source"], "mongodb_main");
    assert_eq!(steps[0]["command"]["database"], Value::Null);

    // 子步骤（Order）：同 source 但跨 db → database = 子 schema 落点
    assert_eq!(steps[1]["model"], "Order");
    assert_eq!(steps[1]["command"]["kind"], "insertOne");
    assert_eq!(steps[1]["command"]["source"], "mongodb_main");
    assert_eq!(steps[1]["command"]["database"], "orders_db");
    // 外键占位符指向父步骤
    assert_eq!(
        steps[1]["command"]["doc"]["userId"],
        json!("{{step.0._id}}")
    );
}

#[test]
fn b7_mutation_pg_cross_schema_steps_carry_schema() {
    let registry = registry_of(&pg_cross_schema_items());
    let out = plan_mutation(
        "Order",
        &registry,
        None,
        &json!({ "code": "C1", "items": [ { "sku": "S1" } ] }),
        0,
        &["new-id-0".to_string(), "new-id-1".to_string()],
    )
    .expect("mutation 规划失败");

    let steps = out["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["command"]["source"], "pg");
    assert_eq!(steps[0]["command"]["schema"], "app_a");
    assert_eq!(steps[1]["command"]["source"], "pg");
    assert_eq!(steps[1]["command"]["schema"], "app_b");
}

// ── B8：同一 GQL × 不同 route_override → 不同租户落点 ──

#[test]
fn b8_route_override_same_gql_different_tenants() {
    let registry = registry_of(&[(
        json!({
            "name": "User", "collection": "users", "timestamps": false,
            "fields": { "name": { "type": "string" } },
            "relations": {}
        }),
        loc("pg", Some("app_db"), None),
    )]);

    let plan_t42 = plan_query("User{...}", &Default::default(), &registry, None)
        .map(|p| {
            let mut v = p.to_value();
            apply_route_override(&mut v, &json!({ "schema": "tenant_42" }));
            v
        })
        .expect("租户42规划失败");
    let plan_t7 = plan_query("User{...}", &Default::default(), &registry, None)
        .map(|p| {
            let mut v = p.to_value();
            apply_route_override(&mut v, &json!({ "schema": "tenant_7" }));
            v
        })
        .expect("租户7规划失败");

    for (plan, tenant) in [(&plan_t42, "tenant_42"), (&plan_t7, "tenant_7")] {
        for c in plan["commands"].as_array().expect("应有命令") {
            assert_eq!(c["source"], "pg", "只 override schema 时 source 保留声明");
            assert_eq!(c["database"], "app_db", "未 override 的 database 保留声明");
            assert_eq!(c["schema"], tenant);
        }
    }
    // 同一 GQL 不同租户 → schema 落不同值
    assert_ne!(
        plan_t42["commands"][0]["schema"],
        plan_t7["commands"][0]["schema"]
    );

    // source + schema 同时 override（跨连接租户）
    let plan_x = plan_query("User{...}", &Default::default(), &registry, None)
        .map(|p| {
            let mut v = p.to_value();
            apply_route_override(
                &mut v,
                &json!({ "source": "pg_cluster", "schema": "tenant_9" }),
            );
            v
        })
        .expect("跨连接租户规划失败");
    for c in plan_x["commands"].as_array().unwrap() {
        assert_eq!(c["source"], "pg_cluster");
        assert_eq!(c["schema"], "tenant_9");
    }

    // override 不污染 postprocess（权限/后处理语义不变）
    let plan_plain = plan_query("User{...}", &Default::default(), &registry, None).unwrap();
    assert_eq!(
        plan_t42.get("postprocess"),
        plan_plain.to_value().get("postprocess")
    );
}
