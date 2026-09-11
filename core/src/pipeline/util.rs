//! 跨模块共享的小工具

use serde_json::{json, Map, Value};

/// 从 params 中按 `@ref` 取值；ref 为空/缺席返回 None
pub(crate) fn param<'a>(params: &'a Map<String, Value>, r: Option<&String>) -> Option<&'a Value> {
    let r = r?;
    if r.is_empty() {
        return None;
    }
    // 对应 JS `ref.slice(1)`：无条件去掉首字符 '@'
    let key = r.get(1..).unwrap_or("");
    params.get(key)
}

/// JS `x != null`：undefined / null 均视为缺席
pub(crate) fn is_nullish(v: Option<&Value>) -> bool {
    match v {
        None => true,
        Some(Value::Null) => true,
        Some(_) => false,
    }
}

pub(super) fn find_stage_idx(stages: &[Value], key: &str) -> Option<usize> {
    stages
        .iter()
        .position(|s| s.as_object().map(|o| o.contains_key(key)).unwrap_or(false))
}

/// 按序追加 $sort/$skip/$limit
pub(super) fn append_order(
    stages: &mut Vec<Value>,
    sort: Option<&Value>,
    skip_val: Option<&Value>,
    limit_val: Option<&Value>,
) {
    if !is_nullish(sort) {
        stages.push(json!({ "$sort": sort.unwrap().clone() }));
    }
    if !is_nullish(skip_val) {
        stages.push(json!({ "$skip": skip_val.unwrap().clone() }));
    }
    if !is_nullish(limit_val) {
        stages.push(json!({ "$limit": limit_val.unwrap().clone() }));
    }
}
