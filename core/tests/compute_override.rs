//! `fields ∩ computes` 同名（计算列覆盖字段）：
//! 注册期放行 + 执行期 fn / asyncFn / agg 三条路径的覆盖顺序与 SQL 无重复列。
//!
//! 语义（与 nodejs-store 对齐）：computed 值拥有同名键的**输出所有权**，
//! `fields` 仍保留该键的类型声明（写入校验 / DDL / `$inc` 等原生增量更新）。
//! 其余 5 种命名组合（组内归一撞名、fields∩relations、relations∩computes）仍须拦截。

use serde_json::{json, Map, Value};

use rust_store_core::command::{finalize_query, plan_query};
use rust_store_core::computes::{process_node, FnRegistry};
use rust_store_core::dialect::{translate, Backend};
use rust_store_core::local::pipeline;
use rust_store_core::permission::Context;
use rust_store_core::schema::Registry;

/// 实时计数 = 3（模拟「DB 原值 7、实时计数 3」的覆盖场景）
struct LiveCounter;

impl FnRegistry for LiveCounter {
    fn call_sync(&self, fn_ref: &str, _doc: &Value) -> Result<Value, String> {
        match fn_ref {
            "live_favorite" => Ok(json!(3)),
            other => Err(format!("未注册的同步测试函数: {other}")),
        }
    }

    fn call_async(
        &self,
        fn_ref: &str,
        items: &mut [Value],
        _ctx: Option<&Context>,
    ) -> Result<(), String> {
        match fn_ref {
            "live_favorite" => {
                for it in items.iter_mut() {
                    if let Some(o) = it.as_object_mut() {
                        o.insert("favoriteCount".to_string(), json!(3));
                    }
                }
                Ok(())
            }
            other => Err(format!("未注册的异步测试函数: {other}")),
        }
    }
}

/// 同名 fn / asyncFn 两种形态：`fields.favoriteCount` 与 `computes.favoriteCount` 同名
fn listing_registry(async_fn: bool) -> Registry {
    let computes = if async_fn {
        json!({
            "favoriteCount": {
                "type": "int", "asyncFn": true, "fnRef": "live_favorite", "depends": []
            }
        })
    } else {
        json!({
            "favoriteCount": {
                "type": "int", "fn": true, "fnRef": "live_favorite", "depends": []
            }
        })
    };
    let mut r = Registry::new();
    r.register(&json!({
        "name": "Listing",
        "collection": "listings",
        "timestamps": false,
        "fields": {
            "_id": { "type": "string" },
            "title": { "type": "string" },
            "favoriteCount": { "type": "int" }
        },
        "relations": {},
        "computes": computes
    }))
    .expect("fields ∩ computes 同名应放行（计算列覆盖字段）");
    r
}

/// 同名 agg 计算列（关系整行计数）
fn shop_registry() -> Registry {
    let mut r = Registry::new();
    r.register(&json!({
        "name": "Order",
        "collection": "orders",
        "timestamps": false,
        "fields": {
            "amount": { "type": "number" },
            "shopId": { "type": "string" }
        },
        "relations": {}
    }))
    .expect("Order 注册成功");
    r.register(&json!({
        "name": "Shop",
        "collection": "shops",
        "timestamps": false,
        "fields": {
            "_id": { "type": "string" },
            "name": { "type": "string" },
            "orderCount": { "type": "int" }
        },
        "relations": {
            "orders": {
                "model": "Order", "type": "many",
                "localField": "_id", "foreignField": "shopId"
            }
        },
        "computes": {
            "orderCount": { "type": "int", "agg": { "$count": "orders" } }
        }
    }))
    .expect("fields ∩ computes 同名（agg）应放行");
    r
}

fn reg_err(def: Value) -> String {
    let mut r = Registry::new();
    r.register(&def).unwrap_err()
}

// ─── 注册期：6 种命名组合 ─────────────────────────────────────

/// fields 内部归一撞名 ⇒ 仍报错
#[test]
fn fields_internal_conflict_still_errs() {
    let err = reg_err(json!({
        "name": "Bad", "collection": "bad", "timestamps": false,
        "fields": {
            "orderTotal": { "type": "number" },
            "order_total": { "type": "number" }
        },
        "relations": {}
    }));
    assert!(err.starts_with("ERR_NAME_CONFLICT:"), "{err}");
}

/// relations 内部归一撞名 ⇒ 仍报错
#[test]
fn relations_internal_conflict_still_errs() {
    let err = reg_err(json!({
        "name": "Bad", "collection": "bad", "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {
            "orderTotal": {
                "model": "Order", "type": "many",
                "localField": "_id", "foreignField": "badId"
            },
            "order_total": {
                "model": "Order", "type": "many",
                "localField": "_id", "foreignField": "badId"
            }
        }
    }));
    assert!(err.starts_with("ERR_NAME_CONFLICT:"), "{err}");
}

/// computes 内部归一撞名 ⇒ 仍报错
#[test]
fn computes_internal_conflict_still_errs() {
    let err = reg_err(json!({
        "name": "Bad", "collection": "bad", "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "computes": {
            "orderTotal": { "fn": true, "depends": [] },
            "order_total": { "fn": true, "depends": [] }
        }
    }));
    assert!(err.starts_with("ERR_NAME_CONFLICT:"), "{err}");
}

/// fields ∩ relations ⇒ 仍报错
#[test]
fn fields_relations_conflict_still_errs() {
    let err = reg_err(json!({
        "name": "Bad", "collection": "bad", "timestamps": false,
        "fields": { "orderTotal": { "type": "number" } },
        "relations": {
            "order_total": {
                "model": "Order", "type": "many",
                "localField": "_id", "foreignField": "badId"
            }
        }
    }));
    assert!(err.starts_with("ERR_NAME_CONFLICT:"), "{err}");
    assert!(
        err.contains("orderTotal") && err.contains("order_total"),
        "{err}"
    );
}

/// relations ∩ computes ⇒ 仍报错
#[test]
fn relations_computes_conflict_still_errs() {
    let err = reg_err(json!({
        "name": "Bad", "collection": "bad", "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {
            "orderTotal": {
                "model": "Order", "type": "many",
                "localField": "_id", "foreignField": "badId"
            }
        },
        "computes": {
            "order_total": { "fn": true, "depends": [] }
        }
    }));
    assert!(err.starts_with("ERR_NAME_CONFLICT:"), "{err}");
    assert!(
        err.contains("orderTotal") && err.contains("order_total"),
        "{err}"
    );
}

/// fields ∩ computes 同名 ⇒ 放行；两处声明均保留（职责不同层）
#[test]
fn fields_computes_same_name_is_allowed() {
    let r = listing_registry(false);
    let s = r.get("Listing").expect("可取");
    assert!(
        s.fields.contains_key("favoriteCount"),
        "fields 保留同名键类型声明（写入校验 / DDL / $inc 依据）"
    );
    assert_eq!(s.fields["favoriteCount"].field_type, "int");
    assert!(
        s.compute("favoriteCount").is_some(),
        "computes 保留同名计算列（查询期输出所有权）"
    );
}

// ─── 执行期：fn / asyncFn 覆盖顺序 ───────────────────────────

/// 同步 fn：`process_node` 中补默认值之后求值并直接写入同名键 ⇒ 覆盖 DB 原值
#[test]
fn sync_fn_overrides_same_name_field() {
    let reg = listing_registry(false);
    let schema = reg.get("Listing").expect("可取");
    let mut fields: Vec<String> = vec!["_id".to_string(), "favoriteCount".to_string()];
    let mut relations = Vec::new();
    let mut doc = json!({ "_id": "l1", "title": "t", "favoriteCount": 7 });

    process_node(
        &mut doc,
        &mut fields,
        &mut relations,
        schema,
        None,
        &reg,
        Some(&LiveCounter),
    )
    .expect("后处理应成功");

    assert_eq!(doc["favoriteCount"], json!(3), "查询期计算列覆盖 DB 原值");
    assert_eq!(doc["_id"], json!("l1"));
    assert!(doc.get("title").is_none(), "未请求字段被裁剪");
}

/// asyncFn：`finalize_query` 在同步段之后执行宿主回调 ⇒ 覆盖同名键
#[test]
fn async_fn_overrides_same_name_field() {
    let reg = listing_registry(true);
    let plan =
        plan_query("Listing { _id, favoriteCount }", &Map::new(), &reg, None).expect("规划成功");
    let post = plan
        .postprocess
        .clone()
        .expect("非分组查询应有 postprocess");

    let mut items = vec![json!({ "_id": "l1", "favoriteCount": 7 })];
    finalize_query(&post, &mut items, &reg, Some(&LiveCounter), None).expect("后处理应成功");

    assert_eq!(items[0]["favoriteCount"], json!(3), "asyncFn 覆盖同名键");
}

// ─── 执行期：agg（Mongo 阶段顺序 + 本地执行） ────────────────

/// Mongo：`$addFields` 在最终 `$project` 之前，同名字段由计算列覆盖后仍被投影保留
#[test]
fn mongo_agg_add_fields_precedes_final_project() {
    let reg = shop_registry();
    let plan = plan_query("Shop { _id, orderCount }", &Map::new(), &reg, None).expect("规划成功");
    assert_eq!(plan.mode.as_str(), "aggregate");
    let stages = plan.commands[0]["pipeline"]
        .as_array()
        .expect("聚合命令应带 pipeline");

    let add_idx = stages
        .iter()
        .position(|s| s.get("$addFields").is_some())
        .expect("agg 计算列应发射 $addFields");
    let proj_idx = stages
        .iter()
        .rposition(|s| s.get("$project").is_some())
        .expect("应有最终 $project");
    assert!(
        add_idx < proj_idx,
        "计算列 $addFields 必须先于最终 $project：{stages:?}"
    );

    let add = stages[add_idx]["$addFields"]
        .as_object()
        .expect("$addFields 对象");
    assert!(
        add.contains_key("orderCount"),
        "同名键由 $addFields 覆盖写入：{add:?}"
    );
    let proj = stages[proj_idx]["$project"]
        .as_object()
        .expect("$project 对象");
    assert_eq!(
        proj.get("orderCount"),
        Some(&json!(1)),
        "投影保留同名计算列：{proj:?}"
    );
}

/// 本地执行同一 pipeline：DB 存储值 7 被 agg 结果 2 覆盖
#[test]
fn local_agg_overwrites_stored_field() {
    let reg = shop_registry();
    let plan = plan_query("Shop { _id, orderCount }", &Map::new(), &reg, None).expect("规划成功");
    let stages = plan.commands[0]["pipeline"]
        .as_array()
        .expect("聚合命令应带 pipeline")
        .clone();

    let collections = json!({
        "shops": [{ "_id": "s1", "name": "S", "orderCount": 7 }],
        "orders": [
            { "_id": "o1", "shopId": "s1", "amount": 10 },
            { "_id": "o2", "shopId": "s1", "amount": 20 },
            { "_id": "o3", "shopId": "s2", "amount": 30 }
        ]
    });
    let out = pipeline::run(&collections, "shops", &stages).expect("本地执行应成功");

    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["orderCount"], json!(2), "计算列覆盖 DB 存储值");
}

// ─── SQL 方言：同名键只输出派生列，不得重复基础列 ────────────

/// agg 同名键：根表基础列被排除，只以派生表别名出现一次
#[test]
fn sql_aggregate_emits_compute_alias_once() {
    let reg = shop_registry();
    let plan = plan_query("Shop { _id, orderCount }", &Map::new(), &reg, None).expect("规划成功");
    let out = translate(Backend::Sqlite, &plan.commands[0], &reg).expect("SQL 翻译应成功");
    let text = out["stmts"][0]["text"].as_str().expect("应有 SQL 文本");

    assert_eq!(
        text.matches(r#""orderCount""#).count(),
        1,
        "同名键只允许以派生别名出现一次：{text}"
    );
    assert!(
        !text.contains(r#"t."order_count""#),
        "不得再取同名字段的基础列（计算列拥有输出所有权）：{text}"
    );
}

/// fn/asyncFn 同名键：find 快路径不下推计算列（由 Host 后处理覆盖）
#[test]
fn sql_find_does_not_select_compute_name_as_base_column() {
    let reg = listing_registry(false);
    let plan =
        plan_query("Listing { _id, favoriteCount }", &Map::new(), &reg, None).expect("规划成功");
    let out = translate(Backend::Sqlite, &plan.commands[0], &reg).expect("SQL 翻译应成功");
    let text = out["stmts"][0]["text"].as_str().expect("应有 SQL 文本");

    assert!(
        !text.contains(r#"t."favorite_count""#),
        "计算列不是真实列，不得下推基础列：{text}"
    );
}
