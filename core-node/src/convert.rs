//! 绑定层通用转换：napi 错误构造与「跨语言稳定」的 JSON 归一化。
//!
//! 只放与 core 语义无关的样板，保持 core-node / core-py 两侧逐函数对应。

use std::collections::HashSet;

use napi::Error;
use rust_store_core::CoreError;
use serde_json::Value;

/// core 错误 → napi `Error`（评测报告 rust m-3：结构化错误映射）。
///
/// core 的 `String` 错误在 FFI 边界经 [`CoreError`] 归类——哨兵前缀匹配收口在
/// `core::error::classify`（core 内唯一匹配点），绑定层只消费枚举。
/// napi `Error` 无错误码语义，三个变体统一 message 透传；文案保留**完整原文**
/// （含 `ERR_PERMISSION:` 等哨兵前缀），JS Host 既有的前缀映射（403 归类）不受
/// 影响。如需向 JS 透出结构化 `e.code`，在此处按 [`CoreError::code`] 扩展。
pub(crate) fn err(reason: String) -> Error {
    Error::from_reason(CoreError::from(reason).message().to_owned())
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
