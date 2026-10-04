//! `find` 命令翻译：根表标量 + 可选 projection

use serde_json::Value;

use crate::schema::Schema;

use crate::dialect::filter::build_filter;
use crate::dialect::ir::{RowCol, RowShape, SqlStmt};
use crate::dialect::{field_is_bool, physical_of, Backend, ColumnRef};

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
    // D2：显式投影 object/array 字段在 SQL 侧无列 → 显式 Err，绝不静默丢弃该列
    super::check_projection_supported(schema, projection)?;
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
        // 计算列不是真实列（fn/asyncFn 在 Host 尾部求值、agg 由派生表物化），跳过不下推
        if schema.compute(f).is_some() {
            continue;
        }
        match col_fn(schema)(f) {
            // 标量列
            Some(ColumnRef::Scalar(c)) => {
                cols_sql.push(format!("t.{}", backend.pcol(&c)));
                // §9.7 布尔归一：schema `boolean` 字段的列值 0/1 → JSON bool
                columns.push(RowCol::scalar_bool(
                    &physical_of(&c),
                    &[f.as_str()],
                    field_is_bool(schema, f),
                ));
            }
            // object/array JSON 列：整列取出，还原时解析 JSON 文本
            Some(ColumnRef::Json(c, _)) => {
                cols_sql.push(format!("t.{}", backend.pcol(&c)));
                columns.push(RowCol::json(&physical_of(&c), &[f.as_str()]));
            }
            // 对象点号路径投影：取该路径的标量值（还原为嵌套对象）
            Some(ColumnRef::JsonPath(c, path)) => {
                let base = format!("t.{}", backend.pcol(&c));
                let segs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
                let alias = f.replace('.', "_");
                cols_sql.push(format!(
                    "{} AS {}",
                    backend.json_extract_scalar(&base, &segs),
                    q(backend, &alias)
                ));
                columns.push(RowCol::scalar(&alias, &f.split('.').collect::<Vec<_>>()));
            }
            None => {}
        }
    }
    // 缺失 vs null 三态（F-07/H-01）：额外查出 `__present` 哨兵列，供行还原时区分
    // 「显式 null（有键）」与「缺失（无键）」。`__present` 不是 schema 字段，不入用户投影，
    // 只经 present_alias 交给 restore_rows 消费。
    cols_sql.push("t.__present AS __present".to_string());
    let shape = RowShape {
        columns,
        present_alias: Some("__present".to_string()),
    };
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
