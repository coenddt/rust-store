//! 字段默认值填充 + 同步 fn 计算列（仅根文档，不递归）

use serde_json::Value;

use crate::schema::{FieldDef, Schema};
use crate::types::get_default;

use super::cache::ensure_cache;
use super::registry::FnRegistry;

fn none_if_null(v: Value) -> Option<Value> {
    if v.is_null() {
        None
    } else {
        Some(v)
    }
}

/// 取规范化字段定义的默认值（无则 None），语义对齐 Python：null 视为未配置
pub fn field_default(field: &FieldDef) -> Option<Value> {
    if let Some(d) = &field.default {
        return Some(d.clone());
    }
    none_if_null(get_default(&field.field_type))
}

/// 取原始（未规范化）字段定义的默认值，供嵌套 object 子字段使用
pub(super) fn raw_field_default(defn: &Value) -> Option<Value> {
    match defn {
        Value::String(s) => none_if_null(get_default(s)),
        Value::Object(o) => {
            if let Some(d) = o.get("default") {
                if !d.is_null() {
                    return Some(d.clone());
                }
            }
            let t = o.get("type").and_then(|v| v.as_str()).unwrap_or("");
            none_if_null(get_default(t))
        }
        _ => None,
    }
}

pub(super) fn is_object_field(field: &FieldDef) -> bool {
    field.field_type == "object" && field.fields.as_ref().map(|v| !v.is_null()).unwrap_or(false)
}

fn raw_is_object_field(defn: &Value) -> bool {
    let Some(o) = defn.as_object() else {
        return false;
    };
    o.get("type").and_then(|v| v.as_str()) == Some("object")
        && o.get("fields").map(|v| !v.is_null()).unwrap_or(false)
}

/// 递归填充嵌套 object 子字段默认值（对应 JS `_fillNestedDefaults`）
pub fn fill_nested_defaults(obj: &mut Value, fields_def: &Value) {
    let Some(defs) = fields_def.as_object() else {
        return;
    };
    let Some(o) = obj.as_object_mut() else {
        return;
    };
    for (key, defn) in defs {
        let absent = o.get(key).map(|v| v.is_null()).unwrap_or(true);
        if absent {
            if let Some(d) = raw_field_default(defn) {
                o.insert(key.clone(), d);
            }
        }
        if raw_is_object_field(defn) {
            let nested = defn.get("fields").cloned().unwrap_or(Value::Null);
            if let Some(sub) = o.get_mut(key) {
                if sub.is_object() {
                    fill_nested_defaults(sub, &nested);
                }
            }
        }
    }
}

/// 对单条记录应用字段默认值 + 同步 fn 计算列（对应 JS `applyDefaultsAndComputes`，仅根文档，不递归）
pub fn apply_defaults_and_computes(
    doc: &Value,
    schema: &Schema,
    fn_registry: Option<&dyn FnRegistry>,
) -> Result<Value, String> {
    let Some(src) = doc.as_object() else {
        return Ok(doc.clone());
    };
    let mut result = Value::Object(src.clone());

    for (key, field) in &schema.fields {
        {
            let o = result.as_object_mut().unwrap();
            let absent = o.get(key).map(|v| v.is_null()).unwrap_or(true);
            if absent {
                if let Some(defn) = field_default(field) {
                    o.insert(key.clone(), defn);
                }
            }
        }
        if is_object_field(field) {
            if let Some(fd) = field.fields.clone() {
                if let Some(sub) = result.as_object_mut().unwrap().get_mut(key) {
                    if sub.is_object() {
                        fill_nested_defaults(sub, &fd);
                    }
                }
            }
        }
    }

    // 同步 fn 计算列（对应 JS `if (comp.fn) result[key] = comp.fn(result)`）
    let cache = ensure_cache(schema);
    for entry in &cache.fn_list {
        let v = match fn_registry {
            Some(r) => r.call_sync(&entry.fn_ref, &result)?,
            None => return Err(format!("计算列 {} 未注册同步实现", entry.key)),
        };
        result
            .as_object_mut()
            .unwrap()
            .insert(entry.key.clone(), v);
    }

    Ok(result)
}
