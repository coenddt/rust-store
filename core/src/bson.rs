//! BSON 跨 FFI 线格式契约
//!
//! core 不持有驱动，Host 用原生驱动执行 Command 后把**原始文档**回喂 core 做后处理。
//! 但 `serde_json::Value` 无法表达 `ObjectId` / `Date` / `Decimal128` / `Long`，
//! 因此双方约定一套 **Extended JSON（canonical 子集）** 线格式：
//!
//! | BSON 类型   | 线格式                          |
//! |-------------|---------------------------------|
//! | ObjectId    | `{"$oid": "<24位小写hex>"}`     |
//! | Date        | `{"$date": <毫秒整数>}`          |
//! | Decimal128  | `{"$numberDecimal": "<字符串>"}` |
//! | Long/BigInt | `{"$numberLong": "<字符串>"}`    |
//! | Binary      | `{"$binary": {"base64": "..", "subType": ".."}}` |
//!
//! 约定：
//!   1. Host → core 方向必须调 [`canonicalize`] 归一化（ObjectId 小写、`$date` 统一为毫秒数值）；
//!   2. core → Host 方向由本模块的构造函数产出，保证形状固定；
//!   3. 其余 JSON 类型原样透传，键序不作为契约（serde_json 默认按字典序）。

use serde_json::{json, Map, Value};

pub const KEY_OID: &str = "$oid";
pub const KEY_DATE: &str = "$date";
pub const KEY_DECIMAL: &str = "$numberDecimal";
pub const KEY_LONG: &str = "$numberLong";
pub const KEY_BINARY: &str = "$binary";

/// ObjectId：入参为 24 位 hex，统一转小写
pub fn object_id(hex: &str) -> Value {
    json!({ KEY_OID: hex.to_ascii_lowercase() })
}

/// 日期：毫秒时间戳（与 JS `Date.now()` 同单位）
pub fn date(ms: i64) -> Value {
    json!({ KEY_DATE: ms })
}

/// Decimal128：字符串形式保留精度
pub fn decimal128(s: &str) -> Value {
    json!({ KEY_DECIMAL: s })
}

/// 64 位整数：字符串形式避免 JSON number 精度丢失
pub fn number_long(v: i64) -> Value {
    json!({ KEY_LONG: v.to_string() })
}

/// 取 ObjectId 的 hex 字符串
pub fn as_object_id(v: &Value) -> Option<&str> {
    v.as_object()?.get(KEY_OID)?.as_str()
}

/// 取日期的毫秒值
pub fn as_date_ms(v: &Value) -> Option<i64> {
    v.as_object()?.get(KEY_DATE)?.as_i64()
}

/// 判断是否为扩展类型对象（`$oid` / `$date` / ... 之一）
pub fn is_extended(v: &Value) -> bool {
    match v.as_object() {
        None => false,
        Some(o) if o.len() != 1 => false,
        Some(o) => {
            let k = o.keys().next().map(String::as_str).unwrap_or("");
            matches!(k, KEY_OID | KEY_DATE | KEY_DECIMAL | KEY_LONG | KEY_BINARY)
        }
    }
}

/// 递归归一化扩展类型（幂等），用于 Host 回喂文档前统一形状
///
/// 归一化规则：
///   - `{"$oid": hex}` → hex 转小写；
///   - `{"$date": n}` → n 为整数则保留；为字符串（ISO）时保留原样，由 Host 保证已是毫秒；
///   - 其余扩展类型透传。
pub fn canonicalize(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(canonicalize).collect()),
        Value::Object(o) => {
            if is_extended(v) {
                let (k, inner) = o.iter().next().expect("is_extended 保证非空");
                return match (k.as_str(), inner) {
                    (KEY_OID, Value::String(s)) => object_id(s),
                    _ => {
                        let mut m = Map::new();
                        m.insert(k.clone(), inner.clone());
                        Value::Object(m)
                    }
                };
            }
            Value::Object(o.iter().map(|(k, x)| (k.clone(), canonicalize(x))).collect())
        }
        other => other.clone(),
    }
}

/// 文档主键的可比较字符串形式，对应 JS `String(id)`
///
/// 两阶段查询需按阶段一返回的 `_id` 顺序还原排序，而 ObjectId 在 JS 里
/// `String(id)` 即其 hex 值。
pub fn id_key(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Object(_) => as_object_id(v).map(String::from).unwrap_or_else(|| dump(v)),
        Value::Array(_) => dump(v),
    }
}

fn dump(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}
