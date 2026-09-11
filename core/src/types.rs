//! 类型系统 —— 类型 → 零值映射（对应 JS `src/types.js`）

use serde_json::{json, Value};

/// 获取指定类型的零值；`date`/`any`/未知类型返回 null
pub fn get_default(field_type: &str) -> Value {
    match field_type {
        "string" => json!(""),
        "int" | "long" | "float" | "double" => json!(0),
        "boolean" => json!(false),
        // array/object 每次返回新实例，避免多个文档共享同一引用
        "array" => json!([]),
        "object" => json!({}),
        _ => Value::Null,
    }
}

pub fn is_numeric(field_type: &str) -> bool {
    matches!(field_type, "int" | "long" | "float" | "double")
}

pub fn is_primitive(field_type: &str) -> bool {
    matches!(
        field_type,
        "string" | "int" | "long" | "float" | "double" | "boolean"
    )
}

/// JS 真值语义：null / false / 0 / "" / 缺席 为假，其余为真
pub fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// 读取字符串数组；非数组（含 null/缺席）返回 None
pub fn str_list(v: Option<&Value>) -> Option<Vec<String>> {
    match v {
        Some(Value::Array(a)) => Some(
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect(),
        ),
        _ => None,
    }
}
