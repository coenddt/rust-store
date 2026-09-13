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

// ─── 条件拒绝名单（缺陷 D-02） ───────────────────────────────

/// 服务端执行类操作符拒绝名单：这些键会触发数据库端 JS/表达式执行
/// （SQL 侧会被静默丢条件返回全量，Mongo 侧会服务端执行），任何路径
/// 都不允许进入规划，命中即显式报错。
const FORBIDDEN_COND_OPS: [&str; 3] = ["$where", "$function", "$accumulator"];

/// 校验查询/写入条件：递归遍历条件树，命中拒绝名单即显式报错。
///
/// 覆盖顶层逻辑组（`$and`/`$or`/`$nor`）、字段级操作符对象与嵌套数组内的
/// 任意深度 —— 防止 `$where` 等载荷借嵌套结构绕过校验。
pub fn validate_condition(filter: &Value) -> Result<(), String> {
    match filter {
        Value::Object(map) => {
            for (k, v) in map {
                if FORBIDDEN_COND_OPS.contains(&k.as_str()) {
                    return Err(format!(
                        "条件包含被拒绝的操作符 {k}（服务端执行类，命中拒绝名单）：请改用结构化条件"
                    ));
                }
                validate_condition(v)?;
            }
            Ok(())
        }
        Value::Array(arr) => {
            for v in arr {
                validate_condition(v)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
