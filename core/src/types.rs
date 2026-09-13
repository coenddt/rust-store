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

/// 聚合算子白名单（§9.5 批次一）：计算列 `agg`、根级 `$group.agg`、§9.6
/// 关系聚合谓词共用同一集合。单点定义，避免多处字面量数组漂移（新增算子时漏改）。
pub(crate) const AGG_OPS: [&str; 5] = ["$count", "$sum", "$avg", "$min", "$max"];

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
                // 空逻辑组（`$and:[]` / `$or:[]` / `$nor:[]`）会让 WHERE 退化为空条件
                // → 全表/全量，与「绝不静默」冲突（A-20 / G-06）→ 规划期显式报错，所有后端一致。
                if matches!(k.as_str(), "$and" | "$or" | "$nor") {
                    if let Value::Array(arr) = v {
                        if arr.is_empty() {
                            return Err(format!(
                                "{k} 逻辑组为空：拒绝生成无条件的全表查询（请在组内提供至少一个条件）"
                            ));
                        }
                    }
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

// ─── U1~U4 形态拒绝（落 D2，跨后端归一） ─────────────────────

/// 取条件键的**根字段类型**（`a.b.c` → 查 `a`）；未声明返回 `None`。
fn root_field_type<'a>(schema: &'a Schema, key: &str) -> Option<&'a str> {
    let root = key.split('.').next().unwrap_or(key);
    schema.fields.get(root).map(|f| f.field_type.as_str())
}

/// U1~U4（`$condition` 侧）：数组字段过滤 / 对象字段过滤 / 对象点号路径过滤 ——
/// 在**所有后端（含 Mongo）**规划期统一显式报错（D2：绝不静默）。
///
/// 判定依据 = schema：数组 / 对象字段在 SQL 侧无可翻译列，Mongo 侧虽能执行，
/// 但会造成跨后端结果不一致 → 一律拒绝。**关系名**（§9.6 关系聚合谓词）不在本检查
/// 范围，交由关系节点处理。
pub fn validate_condition_shape(schema: &Schema, filter: &Value) -> Result<(), String> {
    let Value::Object(map) = filter else {
        return Ok(());
    };
    for (k, v) in map {
        if matches!(k.as_str(), "$and" | "$or" | "$nor") {
            if let Value::Array(arr) = v {
                for it in arr {
                    validate_condition_shape(schema, it)?;
                }
            }
            continue;
        }
        if k.starts_with('$') || schema.relations.contains_key(k) {
            continue;
        }
        let Some(ft) = root_field_type(schema, k) else {
            continue;
        };
        let dotted = k.contains('.');
        match (ft, dotted) {
            ("array", _) => {
                return Err(format!(
                    "数组字段 \"{k}\" 不支持过滤条件（U1/D2：所有后端含 Mongo 统一显式拒绝）"
                ));
            }
            ("object", false) => {
                return Err(format!(
                    "对象字段 \"{k}\" 不支持过滤条件（U2/D2：所有后端含 Mongo 统一显式拒绝）"
                ));
            }
            ("object", true) => {
                return Err(format!(
                    "对象点号路径 \"{k}\" 不支持过滤条件（U3/D2：所有后端含 Mongo 统一显式拒绝）"
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// §9.6 关系聚合谓词探测：`$condition` 中是否出现**关系名**作键。
///
/// 覆盖 `$and` / `$or` / `$nor` 嵌套与 `$not` 否定形态（anti-join）。
/// 标量查询路径（`countDocuments` / 标量 `count` / `exists`）无法表达关系谓词，
/// 命中即须显式 `Err`，否则 Mongo 侧会把它当成「字段等于该对象」而**静默给出错数**（D2）。
pub fn has_relation_predicate(schema: &Schema, v: &Value) -> bool {
    let Some(m) = v.as_object() else {
        return false;
    };
    m.iter().any(|(k, val)| {
        if matches!(k.as_str(), "$and" | "$or" | "$nor") {
            return val
                .as_array()
                .map(|a| a.iter().any(|x| has_relation_predicate(schema, x)))
                .unwrap_or(false);
        }
        // `{"$not": { "<rel>": { … } }}` → anti-join（见 `pipeline/relation_filter.rs`）
        if k == "$not" {
            return has_relation_predicate(schema, val);
        }
        if schema.relations.contains_key(k.as_str()) {
            return true;
        }
        // 运算对象（`{"field": {"$gt": …}}`）不下钻：关系名只可能作顶层键
        false
    })
}

/// U4（`$sort` 侧）：对象点号路径排序 —— 所有后端（含 Mongo）规划期统一显式报错。
///
/// 仅当点号键的根字段是 schema 声明的 object 字段时拒绝；关系路径排序
/// （`bidders.amount`，R10）不在本检查范围。
pub fn validate_sort_shape(schema: &Schema, sort: &Value) -> Result<(), String> {
    let Some(map) = sort.as_object() else {
        return Ok(());
    };
    for k in map.keys() {
        if !k.contains('.') || schema.relations.contains_key(k) {
            continue;
        }
        if root_field_type(schema, k) == Some("object") {
            return Err(format!(
                "对象点号路径 \"{k}\" 不支持排序（U4/D2：所有后端含 Mongo 统一显式拒绝）"
            ));
        }
    }
    Ok(())
}

// ─── 条件形状校验（U1~U4 / D2） ─────────────────────
