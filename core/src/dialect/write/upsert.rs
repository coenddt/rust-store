//! upsert 翻译（`findOneAndUpdate` + `upsert: true` 路径）。
//!
//! 支持 `RETURNING` 的后端产出单条 `INSERT ... ON CONFLICT ... DO UPDATE ... RETURNING`；
//! MySQL 无 `RETURNING` → `ON DUPLICATE KEY UPDATE` + 按同一 filter 回读两段式。

use serde_json::{json, Value};

use crate::dialect::ir::SqlStmt;
use crate::dialect::Backend;
use crate::schema::Schema;

use super::update::{build_assignments, read_back, write_with_returning};
use super::{q, returning_cols, scalar_col, tname, Binder};

/// upsert：`INSERT ... ON CONFLICT/ON DUPLICATE KEY ...` + 回读
pub(super) fn translate_upsert(
    backend: Backend,
    schema: &Schema,
    filter: &Value,
    update: &Value,
) -> Result<Vec<SqlStmt>, String> {
    let targets = upsert_target_pairs(schema, filter)?;
    let target: Vec<String> = targets.iter().map(|(c, _)| c.clone()).collect();

    // INSERT 列 = 冲突目标等值列 ∪ `$setOnInsert` ∪ `$set`（标量；已出现的列保留先者取值）
    let mut cols: Vec<String> = Vec::new();
    let mut vals: Vec<Value> = Vec::new();
    // 冲突目标：Mongo upsert 会用 filter 等值填充新文档，关系型 INSERT 需显式带上这些列
    for (col, val) in &targets {
        if cols.iter().any(|c| c == col) {
            continue;
        }
        cols.push(col.clone());
        vals.push(val.clone());
    }
    for key in ["$setOnInsert", "$set"] {
        let Some(o) = update.get(key).and_then(|v| v.as_object()) else {
            continue;
        };
        for (k, v) in o {
            let Some(col) = scalar_col(schema, k) else {
                continue;
            };
            if v.is_null() || cols.iter().any(|c| c == &col) {
                continue;
            }
            cols.push(col);
            vals.push(v.clone());
        }
    }
    if cols.is_empty() {
        return Err("upsert 无可写标量字段".to_string());
    }

    let mut binder = Binder::new(backend);
    let cols_sql = cols
        .iter()
        .map(|c| q(backend, c))
        .collect::<Vec<_>>()
        .join(", ");
    let phs: Vec<String> = vals.iter().map(|v| binder.bind(v.clone())).collect();

    // 冲突时的 SET 赋值（仅 `$set`；`$inc`/`$unset` 亦允许）
    let set_update = json!({ "$set": update.get("$set").cloned().unwrap_or(json!({})) });
    let assigns = build_assignments(&mut binder, schema, &set_update)?;
    if assigns.is_empty() {
        return Err("upsert 无冲突更新字段".to_string());
    }

    let returning = returning_cols(schema);
    let insert_text = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        tname(backend, schema),
        cols_sql,
        phs.join(", "),
    );

    Ok(if backend.supports_returning() {
        let target_sql = target
            .iter()
            .map(|c| q(backend, c))
            .collect::<Vec<_>>()
            .join(", ");
        let text = format!(
            "{} ON CONFLICT ({}) DO UPDATE SET {}",
            insert_text,
            target_sql,
            assigns.join(", "),
        );
        vec![write_with_returning(
            backend,
            text,
            &returning,
            binder.params,
        )]
    } else {
        // MySQL：`ON DUPLICATE KEY UPDATE` 后按 filter 回读
        let text = format!(
            "{} ON DUPLICATE KEY UPDATE {}",
            insert_text,
            assigns.join(", "),
        );
        vec![
            SqlStmt::write(text, binder.params),
            read_back(backend, schema, filter, &returning)?,
        ]
    })
}

/// upsert 冲突目标列及其等值条件值：取自 filter 的唯一条件。
///
/// - 顶层字段（非 `$` 开头）→ 该字段
/// - `$or` → 首个分支的字段集合（core 的 upsert 条件按「_id / unique 索引」顺序生成）
///
/// 仅接受等值条件（非操作符对象）：Mongo upsert 会用 filter 等值填充新文档，
/// 关系型 INSERT 需把这些列/值显式带上；无法确定唯一目标 → 报错（避免生成语义错误的 SQL）。
fn upsert_target_pairs(schema: &Schema, filter: &Value) -> Result<Vec<(String, Value)>, String> {
    let cond = match filter.as_object() {
        Some(o) => {
            if let Some(first) = o
                .get("$or")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
            {
                first.as_object().cloned().unwrap_or_default()
            } else {
                o.clone()
            }
        }
        None => return Err("upsert 需要条件对象".to_string()),
    };
    let mut pairs: Vec<(String, Value)> = Vec::new();
    for (k, v) in cond.iter() {
        if k.starts_with('$') {
            continue;
        }
        let Some(col) = scalar_col(schema, k) else {
            continue;
        };
        if v.is_object() {
            return Err(format!("upsert 条件 {} 需为等值（唯一键）条件", k));
        }
        if !pairs.iter().any(|(c, _)| c == &col) {
            pairs.push((col, v.clone()));
        }
    }
    if pairs.is_empty() {
        return Err("upsert 需要唯一键条件（_id 或 unique 索引字段）".to_string());
    }
    Ok(pairs)
}
