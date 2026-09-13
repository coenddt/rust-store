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
    warnings: crate::dialect::filter::Warnings<'_>,
) -> Result<Vec<SqlStmt>, String> {
    let mut seq = 0usize;
    let wh = build_filter(filter, backend, "t", &col_fn(schema), &mut seq, warnings)?;
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
    // 缺失 vs null 三态（F-07/H-01）：额外查出 `__present` 哨兵列，供行还原时区分
    // 「显式 null（有键）」与「缺失（无键）」。`__present` 不是 schema 字段，不入用户投影，
    // 只经 present_alias 交给 restore_rows 消费。
    cols_sql.push("t.__present AS __present".to_string());
    let shape = RowShape { columns, present_alias: Some("__present".to_string()) };
    let select_list = if cols_sql.is_empty() {
        q(backend, "_id")
    } else {
        cols_sql.join(", ")
    };
    let from = tname(backend, schema);
    let where_sql = if wh.text.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", wh.text)
    };
    Ok(vec![SqlStmt::select(
        format!("SELECT {} FROM {} t{}", select_list, from, where_sql),
        wh.params,
        shape,
    )])
}
