//! 命令构造 + 数值工具
//!
//! 所有命令构造器收 [`Schema`]（而非裸 collection 名）：`source` / `database` / `schema` /
//! `collection` 定位四元组由 schema 一次性填入，从类型上消除「漏填定位」的可能
//! （见 `multi-datasource-routing-plan.md`）。命令的定位字段是 Host 路由的唯一依据。

use serde_json::{json, Map, Number, Value};

use crate::schema::Schema;

/// 命令体的定位字段（所有 `cmd_*` 共用）
fn with_loc(mut m: Map<String, Value>, schema: &Schema) -> Map<String, Value> {
    m.insert("source".to_string(), json!(schema.source()));
    m.insert(
        "database".to_string(),
        schema.database().map(|s| json!(s)).unwrap_or(Value::Null),
    );
    m.insert(
        "schema".to_string(),
        schema.schema().map(|s| json!(s)).unwrap_or(Value::Null),
    );
    m
}

pub fn cmd_find(schema: &Schema, filter: &Value, projection: Option<&Value>) -> Value {
    with_loc(
        json!({
            "kind": "find",
            "collection": schema.collection,
            "filter": filter,
            "projection": projection.cloned().unwrap_or(Value::Null),
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
        schema,
    )
    .into()
}

pub fn cmd_aggregate(schema: &Schema, pipeline: &[Value]) -> Value {
    with_loc(
        json!({
            "kind": "aggregate",
            "collection": schema.collection,
            "pipeline": pipeline,
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
        schema,
    )
    .into()
}

pub fn cmd_count_documents(schema: &Schema, filter: &Value) -> Value {
    with_loc(
        json!({
            "kind": "countDocuments",
            "collection": schema.collection,
            "filter": filter,
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
        schema,
    )
    .into()
}

pub fn cmd_find_one(schema: &Schema, filter: &Value, projection: Option<&Value>) -> Value {
    with_loc(
        json!({
            "kind": "findOne",
            "collection": schema.collection,
            "filter": filter,
            "projection": projection.cloned().unwrap_or(Value::Null),
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
        schema,
    )
    .into()
}

pub fn cmd_insert_one(schema: &Schema, doc: &Value) -> Value {
    with_loc(
        json!({
            "kind": "insertOne",
            "collection": schema.collection,
            "doc": doc,
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
        schema,
    )
    .into()
}

pub fn cmd_insert_many(schema: &Schema, docs: &[Value]) -> Value {
    with_loc(
        json!({
            "kind": "insertMany",
            "collection": schema.collection,
            "docs": docs,
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
        schema,
    )
    .into()
}

pub fn cmd_find_one_and_update(
    schema: &Schema,
    filter: &Value,
    update: &Value,
    options: &Value,
) -> Value {
    with_loc(
        json!({
            "kind": "findOneAndUpdate",
            "collection": schema.collection,
            "filter": filter,
            "update": update,
            "options": options,
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
        schema,
    )
    .into()
}

pub fn cmd_update_many(schema: &Schema, filter: &Value, update: &Value) -> Value {
    with_loc(
        json!({
            "kind": "updateMany",
            "collection": schema.collection,
            "filter": filter,
            "update": update,
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
        schema,
    )
    .into()
}

pub fn cmd_delete_many(schema: &Schema, filter: &Value) -> Value {
    with_loc(
        json!({
            "kind": "deleteMany",
            "collection": schema.collection,
            "filter": filter,
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
        schema,
    )
    .into()
}

// ─── 多租户路由 override（方案 §6） ──────────────────────────

/// 把计划内**所有命令体**的定位字段替换为 `routeOverride` 声明。
///
/// - 只作用于命令体（带 `kind` + `collection` 的对象）：计划里的 `sources[]` /
///   `edges` / `postprocess` 等元数据不受影响；
/// - `override.source` / `override.database` / `override.schema` **键出现才替换**
///   （`database` / `schema` 可显式置 null = 连接默认）；缺省回落 schema 原声明；
/// - 权限 / 计算列 / 字段校验仍按结构 schema（与定位正交，语义不变）；
/// - source 存在性由执行层 `getConnection` 兜底，database/schema/表不存在由驱动报错
///   （core 无 IO，不做存在性预检，铁律 1）。
pub fn apply_route_override(plan: &mut Value, route_override: &Value) {
    let ov = match route_override.as_object() {
        Some(o) if !o.is_empty() => o,
        _ => return,
    };
    let src = ov
        .get("source")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let db_present = ov.contains_key("database");
    let db = ov.get("database");
    let sc_present = ov.contains_key("schema");
    let sc = ov.get("schema");
    override_walk(plan, src, db_present, db, sc_present, sc);
}

fn override_walk(
    v: &mut Value,
    src: Option<&str>,
    db_present: bool,
    db: Option<&Value>,
    sc_present: bool,
    sc: Option<&Value>,
) {
    match v {
        Value::Object(map) => {
            let is_cmd = map.contains_key("kind") && map.contains_key("collection");
            if is_cmd {
                if let Some(s) = src {
                    map.insert("source".to_string(), json!(s));
                }
                if db_present {
                    map.insert("database".to_string(), db.cloned().unwrap_or(Value::Null));
                }
                if sc_present {
                    map.insert("schema".to_string(), sc.cloned().unwrap_or(Value::Null));
                }
            }
            for (_, child) in map.iter_mut() {
                override_walk(child, src, db_present, db, sc_present, sc);
            }
        }
        Value::Array(items) => {
            for it in items.iter_mut() {
                override_walk(it, src, db_present, db, sc_present, sc);
            }
        }
        _ => {}
    }
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
