//! asyncFn 计算列按「请求字段」执行（行为修复回归）
//!
//! 原行为（BUG）：任何查询都执行模型全部 asyncFn——IO 型批量计算列在未请求时
//! 白付开销；且 Host 回调写入发生在 `process_node` 裁剪之后、无人剥离，
//! 未请求的计算列值泄漏进返回结果（GQL 投影语义被破坏）。
//!
//! 修复后：只有显式请求的计算列才执行、才写入；`depends` 注入字段仍由
//! `strip_query` 剥离（该机制不受本修复影响）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use rust_store_core::command::{finalize_query, plan_query, prepare_query};
use rust_store_core::computes::FnRegistry;
use rust_store_core::permission::Context;
use rust_store_core::schema::Registry;

/// 统计 asyncFn 回调次数（`favoriteCount` → fnRef `live_favorite`）
#[derive(Clone, Default)]
struct CountingAsync {
    calls: Arc<AtomicUsize>,
}

impl FnRegistry for CountingAsync {
    fn call_sync(&self, _fn_ref: &str, _doc: &Value) -> Result<Value, String> {
        Ok(Value::Null)
    }

    fn call_async(
        &self,
        fn_ref: &str,
        items: &mut [Value],
        _ctx: Option<&Context>,
    ) -> Result<(), String> {
        match fn_ref {
            "live_favorite" => {
                self.calls.fetch_add(1, Ordering::SeqCst);
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

/// Listing：`favoriteCount` asyncFn，`depends` 可选（测 strip 机制用）
fn listing_registry(depends: &[&str]) -> Registry {
    let mut r = Registry::new();
    r.register(&json!({
        "name": "Listing",
        "collection": "listings",
        "timestamps": false,
        "fields": {
            "_id": { "type": "string" },
            "title": { "type": "string" }
        },
        "relations": {},
        "computes": {
            "favoriteCount": {
                "type": "int", "asyncFn": true, "fnRef": "live_favorite",
                "depends": depends
            }
        }
    }))
    .expect("Listing schema 应注册成功");
    r
}

fn plan_post(gql: &str, reg: &Registry) -> Value {
    let plan = plan_query(gql, &Map::new(), reg, None).expect("规划成功");
    plan.postprocess
        .clone()
        .expect("非分组查询应有 postprocess")
}

/// 未请求 asyncFn 计算列：不执行（回调次数 0）、不泄漏进返回
#[test]
fn not_requested_async_fn_is_not_executed_and_not_leaked() {
    let reg = listing_registry(&[]);
    let fns = CountingAsync::default();
    let post = plan_post("Listing { _id, title }", &reg);

    let mut items = vec![json!({ "_id": "l1", "title": "t" })];
    finalize_query(&post, &mut items, &reg, Some(&fns), None).expect("后处理应成功");

    assert_eq!(fns.calls.load(Ordering::SeqCst), 0, "未请求不应执行 asyncFn");
    assert!(
        items[0].get("favoriteCount").is_none(),
        "未请求的计算列不应泄漏进返回: {}",
        items[0]
    );
    assert_eq!(items[0]["title"], json!("t"), "请求字段照常保留");
}

/// 显式请求 asyncFn 计算列：照常执行并写入
#[test]
fn requested_async_fn_executes_and_writes() {
    let reg = listing_registry(&[]);
    let fns = CountingAsync::default();
    let post = plan_post("Listing { _id, favoriteCount }", &reg);

    let mut items = vec![json!({ "_id": "l1" })];
    finalize_query(&post, &mut items, &reg, Some(&fns), None).expect("后处理应成功");

    assert_eq!(fns.calls.load(Ordering::SeqCst), 1, "请求后应执行 asyncFn");
    assert_eq!(items[0]["favoriteCount"], json!(3));
}

/// prepare_query 两段式（Host 桥）：fnRefs 同样按请求字段过滤
#[test]
fn prepare_query_fn_refs_filter_by_requested_fields() {
    let reg = listing_registry(&[]);
    let fns = CountingAsync::default();

    // 未请求 → fnRefs 为空
    let post = plan_post("Listing { _id, title }", &reg);
    let mut items = vec![json!({ "_id": "l1", "title": "t" })];
    let refs = prepare_query(&post, &mut items, &reg, Some(&fns), None)
        .expect("prepare_query 应成功");
    assert!(refs.is_empty(), "未请求时 fnRefs 应为空，得到 {refs:?}");

    // 显式请求 → fnRefs 含 fnRef
    let post = plan_post("Listing { _id, favoriteCount }", &reg);
    let mut items = vec![json!({ "_id": "l1" })];
    let refs = prepare_query(&post, &mut items, &reg, Some(&fns), None)
        .expect("prepare_query 应成功");
    assert_eq!(refs, vec!["live_favorite".to_string()]);
}

/// depends 注入字段仍被 strip 剥离（strip 机制不受字段过滤影响）
#[test]
fn depends_injected_fields_still_stripped() {
    let reg = listing_registry(&["title"]);
    let fns = CountingAsync::default();
    let post = plan_post("Listing { _id, favoriteCount }", &reg);

    let mut items = vec![json!({ "_id": "l1", "title": "t" })];
    finalize_query(&post, &mut items, &reg, Some(&fns), None).expect("后处理应成功");

    assert_eq!(items[0]["favoriteCount"], json!(3), "计算列照常写入");
    assert!(
        items[0].get("title").is_none(),
        "depends 注入的字段应被剥离: {}",
        items[0]
    );
}
