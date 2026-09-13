//! introspection 结果 ↔ 本地覆盖 合并（permission + compute）

use serde_json::{Map, Value};

/// 把 base（introspection 生成的 schemaJSON 数组）与 overlay（本地键控 schemaJSON 数组）
/// 合并：相同 model 名的 def 做字段/关系并集，read/write 与 computes 由 overlay 覆盖。
///
/// overlay 中名字匹配到 base 的 def 才合并；未出现在 base 的新 model 也会保留（纯新增）。
pub fn merge_schema(base: &Value, overlay: &Value) -> Result<Value, String> {
    let base_arr = base.as_array().cloned().unwrap_or_default();
    let overlay_arr = overlay.as_array().cloned().unwrap_or_default();

    // base 按 model 名索引
    let mut out: Vec<Value> = Vec::new();
    for b in &base_arr {
        let name = b
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let over = overlay_arr
            .iter()
            .find(|o| o.get("name").and_then(|v| v.as_str()) == Some(name.as_str()));
        out.push(match over {
            Some(o) => merge_one(b, o)?,
            None => b.clone(),
        });
    }
    // overlay 中 base 没有的新 model
    for o in &overlay_arr {
        let name = o
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let exists = base_arr
            .iter()
            .any(|b| b.get("name").and_then(|v| v.as_str()) == Some(name.as_str()));
        if !exists {
            out.push(o.clone());
        }
    }
    Ok(Value::Array(out))
}

/// 合并单个 schema def：fields/relations 并集，read/write/computes/indexes overlay 覆盖
fn merge_one(base: &Value, overlay: &Value) -> Result<Value, String> {
    let mut out = base.clone();
    let b = base.as_object().cloned().unwrap_or_default();
    let b_fields = b
        .get("fields")
        .and_then(|f| f.as_object())
        .cloned()
        .unwrap_or_default();
    let b_rels = b
        .get("relations")
        .and_then(|f| f.as_object())
        .cloned()
        .unwrap_or_default();

    if let Some(o) = overlay.as_object() {
        // 字段并集
        let mut all_fields = b_fields;
        if let Some(of) = o.get("fields").and_then(|f| f.as_object()) {
            for (k, v) in of {
                all_fields.insert(k.clone(), v.clone());
            }
        }
        if let Some(rel_obj) = out.get_mut("fields") {
            *rel_obj = Value::Object(all_fields);
        }

        // 关系并集
        let mut all_rels = b_rels;
        if let Some(or) = o.get("relations").and_then(|f| f.as_object()) {
            for (k, v) in or {
                all_rels.insert(k.clone(), v.clone());
            }
        }
        if let Some(rel_obj) = out.get_mut("relations") {
            *rel_obj = Value::Object(all_rels);
        }

        // 覆盖标量字段
        for key in ["read", "write", "idPrefix", "collection"] {
            if let Some(v) = o.get(key) {
                if let Some(target) = out.get_mut(key) {
                    *target = v.clone();
                } else {
                    out.as_object_mut()
                        .map(|m| m.insert(key.to_string(), v.clone()));
                }
            }
        }

        // overlay 优先的 computes / indexes
        if let Some(c) = o.get("computes") {
            out.as_object_mut()
                .map(|m| m.insert("computes".to_string(), c.clone()));
        }
        if let Some(i) = o.get("indexes") {
            out.as_object_mut()
                .map(|m| m.insert("indexes".to_string(), i.clone()));
        }
    }
    Ok(out)
}

/// 简化单 def 合并（绑定层可能传单个 schema 而非数组）
pub fn merge_one_schema_json(base: &Value, overlay: &Value) -> Result<Value, String> {
    if base.get("name").is_some() && base.get("fields").is_some() {
        merge_one(base, overlay)
    } else {
        Err("base 不是单个 schema def".to_string())
    }
}

#[allow(dead_code)]
fn _map_helper() -> Map<String, Value> {
    Map::new()
}
