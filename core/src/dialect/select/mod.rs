//! SELECT 翻译：find / countDocuments / aggregate（含 $lookup → JOIN）

use serde_json::{json, Value};

use crate::schema::{Registry, Schema};

use crate::dialect::filter::build_filter;
use crate::dialect::ir::{RowShape, SqlStmt};
use crate::dialect::Backend;

mod aggregate;
mod find;
mod lookup_join;

use aggregate::translate_aggregate;
use find::translate_find;

/// 翻译 find / countDocuments / aggregate 命令
pub fn translate_select(
    backend: Backend,
    cmd: &Value,
    registry: &Registry,
    warnings: &mut Vec<String>,
    unsupported: &mut Vec<Value>,
) -> Result<Vec<SqlStmt>, String> {
    let kind = cmd.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let collection = cmd.get("collection").and_then(|v| v.as_str()).unwrap_or("");
    let schema = registry.get_by_collection(collection)?;

    match kind {
        "countDocuments" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let mut seq = 0usize;
            let wh = build_filter(&filter, backend, "t", &col_fn(schema), &mut seq);
            let where_sql = if wh.text.is_empty() { String::new() } else { format!(" WHERE {}", wh.text) };
            let text = format!("SELECT COUNT(*) FROM {} t{}", q(backend, &schema.collection), where_sql);
            Ok(vec![SqlStmt::select(text, wh.params, RowShape::empty())])
        }
        "find" | "findOne" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let projection = cmd.get("projection").cloned();
            translate_find(backend, schema, &filter, projection.as_ref())
        }
        "aggregate" => {
            let pipeline = cmd.get("pipeline").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            translate_aggregate(backend, schema, &pipeline, registry, warnings, unsupported)
        }
        _ => Err(format!("translate: 未知命令 kind = {}", kind)),
    }
}

/// 字段 → 列名：标量字段在本表；object/array 附属表字段跳过（标量字段原样）
pub(in crate::dialect::select) fn col_fn(schema: &Schema) -> impl Fn(&str) -> Option<String> {
    let field_types: Vec<(String, String)> = schema
        .fields
        .iter()
        .map(|(k, v)| (k.clone(), v.field_type.clone()))
        .collect();
    move |field: &str| {
        if field.contains('.') {
            let (head, _) = field.split_once('.')?;
            // 点号字段：若 head 是 object 字段则跳过（附属表）；否则按整串处理
            let head_type = field_types.iter().find(|(k, _)| k == head).map(|(_, t)| t.clone());
            if matches!(head_type.as_deref(), Some("object") | Some("array")) {
                return None;
            }
            return Some(field.to_string());
        }
        let t = field_types.iter().find(|(k, _)| k == field).map(|(_, t)| t.clone());
        match t {
            Some(t) if t == "object" || t == "array" => None,
            _ => Some(field.to_string()),
        }
    }
}

/// 投影字段：null / 全 1 → 所有标量字段；否则取值为「非 0」的字段
pub(in crate::dialect::select) fn projection_fields(schema: &Schema, projection: Option<&Value>) -> Vec<String> {
    match projection {
        None | Some(Value::Null) => schema.fields.keys().cloned().collect(),
        Some(p) => p.as_object()
            .map(|o| {
                let on: Vec<String> = o
                    .iter()
                    .filter(|(_, v)| {
                        v.as_i64().map(|n| n != 0).unwrap_or(false)
                            || v.as_bool() == Some(true)
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                if on.is_empty() { schema.fields.keys().cloned().collect() } else { on }
            })
            .unwrap_or_else(|| schema.fields.keys().cloned().collect()),
    }
}

pub(in crate::dialect::select) fn q(backend: Backend, ident: &str) -> String {
    backend.quote_ident(ident)
}
