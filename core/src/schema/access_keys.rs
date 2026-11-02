//! `accessKeys` 声明解析与注册期校验。
//!
//! 声明形态（schema 顶层）：
//! ```json
//! { "accessKeys": { "satCode": { "read": "satsRead", "write": "satsWrite",
//!                               "bypassRoles": ["admin"] } } }
//! ```
//! 判决逻辑在 [`crate::access`]；本模块只做**声明校验**与结构化。

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::definition::FieldDef;

/// 单个行级权限绑定键
#[derive(Debug, Clone, PartialEq)]
pub struct AccessKeyDef {
    /// 行控绑定字段（schema 已声明字段）
    pub field: String,
    /// 读路径引用的 ctx 清单键
    pub read: String,
    /// 写路径引用的 ctx 清单键；`None` = 写路径不控
    pub write: Option<String>,
    /// 命中即跳过该键行控的角色清单
    pub bypass_roles: Vec<String>,
}

/// 解析并校验 `accessKeys`；缺失 / `null` → 空表。
///
/// 任一非法形态显式 `Err`（禁静默回落），schema 不进入注册表。
pub fn parse_access_keys(
    obj: &Map<String, Value>,
    fields: &HashMap<String, FieldDef>,
) -> Result<Vec<AccessKeyDef>, String> {
    let schema_name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("<未命名>");
    let Some(v) = obj.get("accessKeys") else {
        return Ok(Vec::new());
    };
    if v.is_null() {
        return Ok(Vec::new());
    }
    let Some(map) = v.as_object() else {
        return Err(format!(
            "schema \"{schema_name}\" 的 accessKeys 必须为对象，实际 {v}"
        ));
    };

    let mut out = Vec::with_capacity(map.len());
    for (field, def) in map {
        if !fields.contains_key(field) {
            return Err(format!(
                "schema \"{schema_name}\" 的 accessKeys 键 \"{field}\" 未在 fields 中声明\
                 （行级权限只能绑定已声明字段）"
            ));
        }
        let Some(o) = def.as_object() else {
            return Err(format!(
                "schema \"{schema_name}\" 的 accessKeys.\"{field}\" 必须为对象，实际 {def}"
            ));
        };
        let read = match o.get("read") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(other) => {
                return Err(format!(
                    "schema \"{schema_name}\" 的 accessKeys.\"{field}\".read 必须为非空字符串，实际 {other}"
                ))
            }
            None => {
                return Err(format!(
                    "schema \"{schema_name}\" 的 accessKeys.\"{field}\" 缺少必填键 read"
                ))
            }
        };
        let write = match o.get("write") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(other) => {
                return Err(format!(
                    "schema \"{schema_name}\" 的 accessKeys.\"{field}\".write 必须为非空字符串或 null，实际 {other}"
                ))
            }
        };
        let bypass_roles = match o.get("bypassRoles") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut roles = Vec::with_capacity(items.len());
                for item in items {
                    match item.as_str() {
                        Some(s) if !s.is_empty() => roles.push(s.to_string()),
                        _ => {
                            return Err(format!(
                                "schema \"{schema_name}\" 的 accessKeys.\"{field}\".bypassRoles \
                                 元素必须为非空字符串，实际 {item}"
                            ))
                        }
                    }
                }
                roles
            }
            Some(other) => {
                return Err(format!(
                    "schema \"{schema_name}\" 的 accessKeys.\"{field}\".bypassRoles 必须为字符串数组，实际 {other}"
                ))
            }
        };
        out.push(AccessKeyDef {
            field: field.clone(),
            read,
            write,
            bypass_roles,
        });
    }
    Ok(out)
}
