//! UPDATE / findOneAndUpdate 翻译（含 MySQL 无 `RETURNING` 的写后回读两段式）。

use serde_json::Value;

use crate::dialect::filter::build_filter;
use crate::dialect::ir::SqlStmt;
use crate::dialect::Backend;
use crate::schema::Schema;

use super::upsert::translate_upsert;
use super::{col_map, q, returning_cols, returning_shape, tname, Binder};

/// 由 Mongo update doc 构建 SET 赋值列表。
///
/// 支持的操作符：`$set`（`col = ?`）、`$inc`（`col = COALESCE(col, 0) + ?`）、
/// `$unset`（`col = NULL`）；`$setOnInsert` 由 upsert 分支单独处理，此处忽略。
/// 其余操作符（`$push` / `$addToSet` / `$pull` …）无法安全映射为标量 UPDATE → 报错。
pub(super) fn build_assignments(
    binder: &mut Binder,
    schema: &Schema,
    update: &Value,
    present_qualifier: Option<&str>,
) -> Result<Vec<String>, String> {
    let Some(obj) = update.as_object() else {
        return Err("update 必须是对象".to_string());
    };
    if obj.is_empty() {
        return Err("没有提供要更新的字段".to_string());
    }
    let mut assigns: Vec<String> = Vec::new();
    let mut set_fields: Vec<String> = Vec::new();
    let mut unset_fields: Vec<String> = Vec::new();
    for (op, val) in obj {
        match op.as_str() {
            "$set" | "$inc" | "$unset" => {
                op_assignments(
                    binder,
                    schema,
                    op,
                    val,
                    &mut assigns,
                    &mut set_fields,
                    &mut unset_fields,
                )?;
            }
            // upsert 专用，非 upsert 路径忽略
            "$setOnInsert" => continue,
            _ => {
                return Err(format!(
                    "translate: 不支持的操作符 {}（写路径仅支持 $set/$inc/$unset）",
                    op
                ))
            }
        }
    }
    // 缺失 vs null 三态（F-07）：$set/$inc 置字段「显式存在」、$unset 置「缺失」，
    // 一并维护 `__present` 哨兵，保证后续 `$eq:null`/`$exists` 过滤仍正确。
    if let Some(present) = present_expr(
        binder.backend,
        &set_fields,
        &unset_fields,
        present_qualifier,
    ) {
        assigns.push(present);
    }
    Ok(assigns)
}

/// 构建 `__present` 哨兵维护表达式：先按 `,k,` 令牌移除（$unset）已不存在的字段，
/// 再用后端各自拼接追加（$set/$inc）显式存在的字段。无任何标量增删 → None（不生成）。
///
/// 注意：UPDATE 的 SET 目标**不可以**带表别名限定（SQLite 直接 `near "."` 语法错，
/// MySQL/PG 亦不通用）——LHS 一律裸列名。`present_qualifier` 仅用于限定 RHS 的
/// 现值引用：PG 的 `ON CONFLICT … DO UPDATE` 中裸 `__present` 会与 `EXCLUDED` 歧义，
/// 须限定为目标表（其余场景为 None）。
fn present_expr(
    backend: Backend,
    set_fields: &[String],
    unset_fields: &[String],
    qualifier: Option<&str>,
) -> Option<String> {
    if set_fields.is_empty() && unset_fields.is_empty() {
        return None;
    }
    let col = q(backend, "__present");
    let col_ref = match qualifier {
        Some(tbl) => format!("{}.{}", tbl, col),
        None => col.clone(),
    };
    let mut expr = format!("COALESCE({}, ',')", col_ref);
    for k in unset_fields {
        expr = format!("REPLACE({}, ',{},,', ',')", expr, k);
    }
    for k in set_fields {
        let token = format!(",{},", k);
        expr = match backend {
            Backend::Mysql => format!("CONCAT({}, '{}')", expr, token),
            _ => format!("{} || '{}'", expr, token),
        };
    }
    Some(format!("{} = {}", col, expr))
}

/// 单个更新操作符 → 赋值片段（`$set` 跳过 null；`$inc`/`$unset` 不跳过）。
/// 另把涉及标量字段的增删收集进 `set_fields`/`unset_fields`，供 `__present` 维护。
///
/// C-11-1：`$set`/`$inc` 目标为 schema 声明的 object/array 字段（SQL 侧无列）时
/// → **显式 `Err`**，绝不让写入静默丢失。
fn op_assignments(
    binder: &mut Binder,
    schema: &Schema,
    op: &str,
    val: &Value,
    assigns: &mut Vec<String>,
    set_fields: &mut Vec<String>,
    unset_fields: &mut Vec<String>,
) -> Result<(), String> {
    let Some(fields) = val.as_object() else {
        return Ok(());
    };
    for (k, v) in fields {
        let Some(col) = super::scalar_col(schema, k) else {
            if matches!(op, "$set" | "$inc") {
                return Err(format!(
                    "SQL 后端不支持对 object/array 字段 \"{k}\" 执行 {op}（无对应列；C-11-1：绝不静默丢失写入）"
                ));
            }
            continue;
        };
        let qc = binder.backend.quote_ident(&col);
        match op {
            "$set" => {
                if v.is_null() {
                    continue;
                }
                let ph = binder.bind(v.clone());
                assigns.push(format!("{} = {}", qc, ph));
                if !set_fields.contains(&col) {
                    set_fields.push(col);
                }
            }
            "$inc" => {
                let ph = binder.bind(v.clone());
                assigns.push(format!("{} = COALESCE({}, 0) + {}", qc, qc, ph));
                if !set_fields.contains(&col) {
                    set_fields.push(col);
                }
            }
            "$unset" => {
                assigns.push(format!("{} = NULL", qc));
                if !unset_fields.contains(&col) {
                    unset_fields.push(col);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// `WHERE` 片段（沿用统一 filter 翻译；占位序号接续 SET 参数）
/// 缺陷 D-02：不可翻译条件现在显式报错，绝不静默丢条件
pub(super) fn where_of(
    binder: &mut Binder,
    schema: &Schema,
    filter: &Value,
) -> Result<String, String> {
    let mut seq = binder.seq();
    // 写路径无告警通道（None）：filter 中出现无法表达的语义组合时直接报错（见 filter::Warnings）
    let wh = build_filter(
        filter,
        binder.backend,
        "t",
        &col_map(schema),
        &mut seq,
        None,
    )?;
    binder.params.extend(wh.params);
    Ok(if wh.text.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", wh.text)
    })
}

pub(super) fn translate_update_many(
    backend: Backend,
    schema: &Schema,
    filter: &Value,
    update: &Value,
) -> Result<Vec<SqlStmt>, String> {
    let mut binder = Binder::new(backend);
    let assigns = build_assignments(&mut binder, schema, update, None)?;
    if assigns.is_empty() {
        return Err("没有提供要更新的字段".to_string());
    }
    let where_sql = where_of(&mut binder, schema, filter)?;
    let text = format!(
        "UPDATE {} AS t SET {}{}",
        tname(backend, schema),
        assigns.join(", "),
        where_sql,
    );
    Ok(vec![SqlStmt::write(text, binder.params)])
}

pub(super) fn translate_find_one_and_update(
    backend: Backend,
    schema: &Schema,
    filter: &Value,
    update: &Value,
    options: &Value,
) -> Result<Vec<SqlStmt>, String> {
    let upsert = options
        .get("upsert")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if upsert {
        return translate_upsert(backend, schema, filter, update);
    }

    let mut binder = Binder::new(backend);
    let assigns = build_assignments(&mut binder, schema, update, None)?;
    if assigns.is_empty() {
        return Err("没有提供要更新的字段".to_string());
    }
    let where_sql = where_of(&mut binder, schema, filter)?;
    let update_text = format!(
        "UPDATE {} AS t SET {}{}",
        tname(backend, schema),
        assigns.join(", "),
        where_sql,
    );
    let cols = returning_cols(schema);
    Ok(if backend.supports_returning() {
        vec![write_with_returning(
            backend,
            schema,
            update_text,
            &cols,
            binder.params,
        )]
    } else {
        // MySQL：无 RETURNING → 写后按同一 filter 回读（两段编排由 Host 执行器顺序执行）
        vec![
            SqlStmt::write(update_text, binder.params),
            read_back(backend, schema, filter, &cols)?,
        ]
    })
}

/// 单条「写 + `RETURNING` 回读」语句（支持 RETURNING 的后端）
pub(super) fn write_with_returning(
    backend: Backend,
    schema: &Schema,
    write_text: String,
    cols: &[String],
    params: Vec<Value>,
) -> SqlStmt {
    let ret = cols
        .iter()
        .map(|c| q(backend, c))
        .collect::<Vec<_>>()
        .join(", ");
    let mut stmt = SqlStmt::write(format!("{} RETURNING {}", write_text, ret), params);
    stmt.is_write = true;
    stmt.row_shape = Some(returning_shape(schema, cols));
    stmt.returning = cols.to_vec();
    stmt
}

/// 无 `RETURNING` 后端的回读语句（按同一 filter 重建，独立参数游标）
pub(super) fn read_back(
    backend: Backend,
    schema: &Schema,
    filter: &Value,
    cols: &[String],
) -> Result<SqlStmt, String> {
    let select_list = cols
        .iter()
        .map(|c| format!("t.{}", q(backend, c)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut read_binder = Binder::new(backend);
    let where_sql = where_of(&mut read_binder, schema, filter)?;
    let text = format!(
        "SELECT {} FROM {} t{}",
        select_list,
        tname(backend, schema),
        where_sql,
    );
    let mut stmt = SqlStmt::write(text, read_binder.params);
    stmt.is_write = false;
    stmt.row_shape = Some(returning_shape(schema, cols));
    Ok(stmt)
}
