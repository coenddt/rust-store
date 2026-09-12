//! `find` 命令翻译：根表标量 + 可选 projection

use serde_json::Value;

use crate::schema::Schema;

use crate::dialect::filter::build_filter;
use crate::dialect::ir::{RowCol, RowShape, SqlStmt};
use crate::dialect::Backend;

use super::{col_fn, projection_fields, q, tname};

// find：根表标量 + 可选 projection
pub(super) fn translate_find(
    backend: Backend,
    schema: &Schema,
    filter: &Value,
    projection: Option<&Value>,
) -> Result<Vec<SqlStmt>, String> {
    let mut seq = 0usize;
    let wh = build_filter(filter, backend, "t", &col_fn(schema), &mut seq);
    let mut selected = projection_fields(schema, projection);
    // Mongo find 默认返回 `_id`（仅当显式 `_id: 0` 时排除）
    let id_excluded = projection
        .and_then(|p| p.as_object())
        .and_then(|o| o.get("_id"))
        .map(|v| v.as_i64() == Some(0) || v.as_bool() == Some(false))
        .unwrap_or(false);
    if !id_excluded && !selected.iter().any(|f| f == "_id") {
        selected.insert(0, "_id".to_string());
    }
    let mut cols_sql = Vec::new();
    let mut columns = Vec::new();
    for f in &selected {
        if let Some(c) = col_fn(schema)(f) {
            cols_sql.push(format!("t.{}", q(backend, &c)));
            columns.push(RowCol::scalar(&c, &[f.as_str()]));
        }
    }
    let select_list = if cols_sql.is_empty() { q(backend, "_id") } else { cols_sql.join(", ") };
    let from = tname(backend, schema);
    let where_sql = if wh.text.is_empty() { String::new() } else { format!(" WHERE {}", wh.text) };
    Ok(vec![SqlStmt::select(
        format!("SELECT {} FROM {} t{}", select_list, from, where_sql),
        wh.params,
        RowShape { columns },
    )])
}
