//! fnfns / commands 对拍共享辅助
//!
//! [`TestFnRegistry`] 是 `tools/test-fns.js` 测试函数表的 Rust 对应实现，
//! 两侧语义必须完全一致——这是 fnfns 黄金基准的可信度来源。

use serde_json::{json, Value};

use mongo_store_core::computes::FnRegistry;
use mongo_store_core::permission::Context;

/// JS `(doc[key] || 0)` 的对齐：数字取值，其余（缺失/null/false/字符串）→ 0
fn num_or0(doc: &Value, key: &str) -> f64 {
    match doc.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// JS 数字 → JSON（整数值输出为整数，对齐 `JSON.stringify`）
fn num_value(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0 {
        json!(f as i64)
    } else {
        json!(f)
    }
}

pub struct TestFnRegistry;

impl FnRegistry for TestFnRegistry {
    fn call_sync(&self, fn_ref: &str, doc: &Value) -> Result<Value, String> {
        match fn_ref {
            // (doc) => (doc.a || 0) + (doc.b || 0)
            "sum_ab" => Ok(num_value(num_or0(doc, "a") + num_or0(doc, "b"))),
            // (doc) => (doc.qty || 0) * (doc.price || 0)
            "mul_qp" => Ok(num_value(num_or0(doc, "qty") * num_or0(doc, "price"))),
            // (doc) => (doc.v > 10 ? 'big' : 'small')
            "label" => Ok(json!(if num_or0(doc, "v") > 10.0 {
                "big"
            } else {
                "small"
            })),
            // () => null
            "null_fn" => Ok(Value::Null),
            // (doc) => 'hi:' + (doc.name || '')
            "greet" => {
                let name = doc.get("name").and_then(|v| v.as_str()).unwrap_or("");
                Ok(json!(format!("hi:{}", name)))
            }
            other => Err(format!("未注册的同步测试函数: {}", other)),
        }
    }

    fn call_async(
        &self,
        fn_ref: &str,
        items: &mut [Value],
        ctx: Option<&Context>,
    ) -> Result<(), String> {
        match fn_ref {
            // (items) => { for (it of items) it.area = (it.w || 0) * (it.h || 0) }
            "fill_area" => {
                for it in items.iter_mut() {
                    let area = num_or0(it, "w") * num_or0(it, "h");
                    if let Some(o) = it.as_object_mut() {
                        o.insert("area".to_string(), num_value(area));
                    }
                }
                Ok(())
            }
            // (items, ctx) => { t = ctx?.userId ? 'u:'+ctx.userId : 'anon'; it.tag = t }
            "ctx_tag" => {
                let tag = match ctx.and_then(|c| c.user_id.clone()) {
                    Some(uid) => format!("u:{}", uid),
                    None => "anon".to_string(),
                };
                for it in items.iter_mut() {
                    if let Some(o) = it.as_object_mut() {
                        o.insert("tag".to_string(), json!(tag));
                    }
                }
                Ok(())
            }
            // (items) => { for (it of items) it.skuTag = (it.items||[]).map(s=>s.sku).join('|') }
            "tag_skus" => {
                for it in items.iter_mut() {
                    let skus: Vec<String> = it
                        .get("items")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .map(|s| {
                                    s.get("sku")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string()
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(o) = it.as_object_mut() {
                        o.insert("skuTag".to_string(), json!(skus.join("|")));
                    }
                }
                Ok(())
            }
            other => Err(format!("未注册的异步测试函数: {}", other)),
        }
    }
}
