//! 命令构造 + 数值工具

use serde_json::{json, Number, Value};

pub fn cmd_find(collection: &str, filter: &Value, projection: Option<&Value>) -> Value {
    json!({
        "kind": "find",
        "collection": collection,
        "filter": filter,
        "projection": projection.cloned().unwrap_or(Value::Null),
    })
}

pub fn cmd_aggregate(collection: &str, pipeline: &[Value]) -> Value {
    json!({
        "kind": "aggregate",
        "collection": collection,
        "pipeline": pipeline,
    })
}

pub fn cmd_count_documents(collection: &str, filter: &Value) -> Value {
    json!({
        "kind": "countDocuments",
        "collection": collection,
        "filter": filter,
    })
}

pub fn cmd_find_one(collection: &str, filter: &Value, projection: Option<&Value>) -> Value {
    json!({
        "kind": "findOne",
        "collection": collection,
        "filter": filter,
        "projection": projection.cloned().unwrap_or(Value::Null),
    })
}

pub fn cmd_insert_one(collection: &str, doc: &Value) -> Value {
    json!({
        "kind": "insertOne",
        "collection": collection,
        "doc": doc,
    })
}

pub fn cmd_insert_many(collection: &str, docs: &[Value]) -> Value {
    json!({
        "kind": "insertMany",
        "collection": collection,
        "docs": docs,
    })
}

pub fn cmd_find_one_and_update(collection: &str, filter: &Value, update: &Value, options: &Value) -> Value {
    json!({
        "kind": "findOneAndUpdate",
        "collection": collection,
        "filter": filter,
        "update": update,
        "options": options,
    })
}

pub fn cmd_update_many(collection: &str, filter: &Value, update: &Value) -> Value {
    json!({
        "kind": "updateMany",
        "collection": collection,
        "filter": filter,
        "update": update,
    })
}

pub fn cmd_delete_many(collection: &str, filter: &Value) -> Value {
    json!({
        "kind": "deleteMany",
        "collection": collection,
        "filter": filter,
    })
}

// ─── 数值工具（对齐 JS `_toNumber` / Number()） ──────────────

/// 对齐 JS `Number()` + `Math.trunc`：无法转换时返回 0
pub(super) fn to_number(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f.is_nan() {
                0.0
            } else {
                f.trunc()
            }
        }
        Some(Value::Bool(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                return 0.0;
            }
            if is_int_str(t) {
                t.parse::<i64>().map(|i| i as f64).unwrap_or(0.0)
            } else {
                match t.parse::<f64>() {
                    Ok(f) if !f.is_nan() => f,
                    _ => 0.0,
                }
            }
        }
        _ => 0.0,
    }
}

fn is_int_str(s: &str) -> bool {
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit())
}

/// 整数形式的 f64 → JSON 整数，保证与 JS `JSON.stringify` 逐字节一致
pub(super) fn num_value(f: f64) -> Value {
    if !f.is_finite() {
        return json!(0);
    }
    if f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0 {
        json!(f as i64)
    } else {
        Value::Number(Number::from_f64(f).unwrap_or_else(|| Number::from(0)))
    }
}
