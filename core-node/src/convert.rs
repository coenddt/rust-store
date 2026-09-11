//! 绑定层通用转换：napi 错误构造与「跨语言稳定」的 JSON 归一化。
//!
//! 只放与 core 语义无关的样板，保持 core-node / core-py 两侧逐函数对应。

use std::collections::HashSet;

use napi::Error;
use serde_json::Value;

pub(crate) fn err(reason: String) -> Error {
    Error::from_reason(reason)
}

/// `Option<HashSet<String>>` → `null` / 排序数组（保证跨语言比较稳定）
pub(crate) fn sorted_set(set: Option<HashSet<String>>) -> Value {
    match set {
        None => Value::Null,
        Some(set) => {
            let mut v: Vec<String> = set.into_iter().collect();
            v.sort();
            Value::Array(v.into_iter().map(Value::String).collect())
        }
    }
}
