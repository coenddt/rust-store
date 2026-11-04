//! 字段默认值填充 + 同步 fn 计算列（仅根文档，不递归）

use serde_json::{Map, Value};

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

/// 默认值填充模式（「缺失」的定义与「补哪些默认」随路径而变，禁混用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultFill {
    /// returns 回喂路径：`null` 视为缺失，补类型零值 + 显式 default（与 JS parity）。
    Return,
    /// insert 落库路径（A3）：**仅在键缺失**时补，且**只补 schema 显式声明的 default**。
    ///
    /// - 显式传入的 `null` 原样落库（F-07 / H-01 契约：空串 / NULL / 缺失三态可区分）；
    /// - 未声明 default 的字段不补类型零值，避免把「未提供」伪装成 `""` / `0` 落库。
    Insert,
}

impl DefaultFill {
    /// 该模式下 key 是否算「缺失」。
    fn is_missing(self, obj: &Map<String, Value>, key: &str) -> bool {
        match self {
            DefaultFill::Return => obj.get(key).map(|v| v.is_null()).unwrap_or(true),
            DefaultFill::Insert => !obj.contains_key(key),
        }
    }
}

/// 取嵌套 object 子字段**显式声明**的 default（类型简写无 default ⇒ None）。
///
/// insert 落库路径专用：不按类型派生零值，避免给嵌套对象塞入 `""` / `0` 噪声。
fn raw_declared_default(defn: &Value) -> Option<Value> {
    match defn {
        Value::Object(o) => o.get("default").filter(|d| !d.is_null()).cloned(),
        _ => None,
    }
}

/// 递归填充嵌套 object 子字段默认值（对应 JS `_fillNestedDefaults`）
pub fn fill_nested_defaults(obj: &mut Value, fields_def: &Value) {
    fill_nested_defaults_mode(obj, fields_def, DefaultFill::Return);
}

/// [`fill_nested_defaults`] 的模式化实现（`Return` = 原语义，`Insert` = A3 收窄语义）。
fn fill_nested_defaults_mode(obj: &mut Value, fields_def: &Value, fill: DefaultFill) {
    let Some(defs) = fields_def.as_object() else {
        return;
    };
    let Some(o) = obj.as_object_mut() else {
        return;
    };
    for (key, defn) in defs {
        let absent = fill.is_missing(o, key);
        if absent {
            let d = match fill {
                DefaultFill::Return => raw_field_default(defn),
                DefaultFill::Insert => raw_declared_default(defn),
            };
            if let Some(d) = d {
                o.insert(key.clone(), d);
            }
        }
        if raw_is_object_field(defn) {
            let nested = defn.get("fields").cloned().unwrap_or(Value::Null);
            if let Some(sub) = o.get_mut(key) {
                if sub.is_object() {
                    fill_nested_defaults_mode(sub, &nested, fill);
                }
            }
        }
    }
}

/// 合并字段默认值（仅根文档，不递归计算列）。
///
/// `DefaultFill::Return`：补类型零值 + 显式 default，`null` 视为缺失（returns 回喂路径，
/// 与 JS parity）；`DefaultFill::Insert`：仅键缺失时补、只补显式 default（insert 落库路径，
/// A3 —— 保留显式 null，未声明默认的字段仍按 NULL 落库）。
///
/// 抽取自 [`apply_defaults_and_computes`]，保证「落库」与「返回值」同源，禁另造规则。
pub fn apply_field_defaults(doc: &Value, schema: &Schema, fill: DefaultFill) -> Value {
    let Some(src) = doc.as_object() else {
        return doc.clone();
    };
    let mut obj = src.clone();

    for (key, field) in &schema.fields {
        let absent = fill.is_missing(&obj, key);
        if absent {
            // 阶段2：autoincrement 主键的缺失 ≠ 类型零值 —— 类型默认（int→0）会把
            // 「数据库待赋值」伪装成 `_id: 0`，宿主据此误判已回填 → 显式留空，
            // 由 SQL RETURNING / lastrowid 回读后由宿主回填真实自增值
            if !(key == "_id" && schema.id_is_autoincrement()) {
                let defn = match fill {
                    DefaultFill::Return => field_default(field),
                    DefaultFill::Insert => field.default.clone().filter(|v| !v.is_null()),
                };
                if let Some(defn) = defn {
                    obj.insert(key.clone(), defn);
                }
            }
        }
        if is_object_field(field) {
            if let Some(fd) = field.fields.clone() {
                if let Some(sub) = obj.get_mut(key) {
                    if sub.is_object() {
                        fill_nested_defaults_mode(sub, &fd, fill);
                    }
                }
            }
        }
    }

    Value::Object(obj)
}

/// 对单条记录应用字段默认值 + 同步 fn 计算列（对应 JS `applyDefaultsAndComputes`，仅根文档，不递归）
pub fn apply_defaults_and_computes(
    doc: &Value,
    schema: &Schema,
    fn_registry: Option<&dyn FnRegistry>,
) -> Result<Value, String> {
    let mut result = apply_field_defaults(doc, schema, DefaultFill::Return);

    // 同步 fn 计算列（对应 JS `if (comp.fn) result[key] = comp.fn(result)`）
    let cache = ensure_cache(schema);
    for entry in &cache.fn_list {
        let v = match fn_registry {
            Some(r) => r.call_sync(&entry.fn_ref, &result)?,
            None => return Err(format!("计算列 {} 未注册同步实现", entry.key)),
        };
        let Some(o) = result.as_object_mut() else {
            return Err("apply_defaults_and_computes：结果不是对象".to_string());
        };
        o.insert(entry.key.clone(), v);
    }

    Ok(result)
}
