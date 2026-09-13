//! 类型系统 —— 类型 → 零值映射（对应 JS `src/types.js`）

use serde_json::{json, Value};

use crate::schema::Schema;

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

// ─── 聚合/`$pipeline` 阶段校验（缺陷 R3） ─────────────────────

/// 写副作用阶段拒绝名单：不允许在查询/聚合中落盘。
const STAGE_FORBIDDEN: [&str; 2] = ["$out", "$merge"];
/// `$match` 内 `$expr` 引用的合法字段（除 schema 声明外的固定键）。
const EXPR_SYS_FIELDS: [&str; 4] = ["_id", "createdBy", "createdAt", "updatedAt"];
/// `$expr` 内的运算符名（字段引用判定用：非运算符的 `$name` 视为字段引用）。
const EXPR_OPS: &[&str] = &[
    "and", "or", "not", "nor", "gt", "gte", "lt", "lte", "eq", "ne", "in", "nin", "exists", "expr",
    "cond", "if", "then", "else", "switch", "case", "default", "sqrt", "pow", "abs", "ceil",
    "floor", "round", "subtract", "add", "multiply", "divide", "mod", "modulo", "literal", "size",
    "arrayElemAt", "arrayToObject", "objectToArray", "isArray", "toString", "toInt", "toDouble",
    "toLong", "concat", "substrBytes", "toLower", "toUpper", "trim", "split", "toArray", "map",
    "reduce", "filter", "let", "sum", "avg", "min", "max", "first", "last", "push", "anyElementTrue",
    "allElementsTrue", "setIsSubset", "setEquals", "setIntersection", "setUnion", "setDifference",
    "in", "type", "mergeObjects", "dateToString", "dateFromString", "toDate", "dateDiff",
];

/// 校验聚合/管道阶段：拒绝 `$out`/`$merge` 写副作用阶段、递归拒绝危险执行算子
/// （`$where` 等）、并校验 `$expr` 未引用 schema 外字段。
pub fn validate_pipeline_stages(schema: &Schema, pipeline: &[Value]) -> Result<(), String> {
    for stage in pipeline {
        match stage {
            Value::Object(m) => {
                for (k, v) in m {
                    if STAGE_FORBIDDEN.contains(&k.as_str()) {
                        return Err(format!(
                            "聚合阶段 {k}（写副作用）被拒绝：不允许在查询/聚合中写落盘"
                        ));
                    }
                    if k == "$match" {
                        validate_expr_fields(schema, v)?;
                    }
                    validate_condition(v)?;
                }
            }
            Value::Array(a) => {
                for v in a {
                    validate_condition(v)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// 递归定位 `$match` 体内的 `$expr` 子树并做字段引用校验。
fn validate_expr_fields(schema: &Schema, v: &Value) -> Result<(), String> {
    match v {
        Value::Object(m) => {
            for (k, sub) in m {
                if k == "$expr" {
                    validate_expr_tree(schema, sub)?;
                } else {
                    validate_expr_fields(schema, sub)?;
                }
            }
        }
        Value::Array(a) => {
            for x in a {
                validate_expr_fields(schema, x)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `$expr` 表达式树：形如 `"$name"` 的叶子（非运算符）视为字段引用，
/// 必须在 schema 中声明（含固定键/关系），否则显式报错。
fn validate_expr_tree(schema: &Schema, v: &Value) -> Result<(), String> {
    match v {
        Value::String(s) if s.starts_with('$') => {
            let f = &s[1..];
            if !EXPR_OPS.contains(&f) && !schema_expr_has_field(schema, f) {
                return Err(format!(
                    "$expr 引用了 schema 未声明的字段 ${f}：请使用 schema 中已定义的字段"
                ));
            }
        }
        Value::Array(a) => {
            for x in a {
                validate_expr_tree(schema, x)?;
            }
        }
        Value::Object(m) => {
            for (_, sub) in m {
                validate_expr_tree(schema, sub)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn schema_expr_has_field(schema: &Schema, f: &str) -> bool {
    EXPR_SYS_FIELDS.contains(&f) || schema.fields.contains_key(f) || schema.relations.contains_key(f)
}
