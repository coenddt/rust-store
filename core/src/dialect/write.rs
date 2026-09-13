//! 写语句翻译：insertOne / insertMany / updateMany / findOneAndUpdate / deleteMany
//!
//! 约束（铁律 1/8）：值全部参数化；标识符只来自 Registry 字段白名单 + `Backend::quote_ident`；
//! 无法安全翻译的组合直接报错，**绝不生成错误 SQL**。
//!
//! MySQL 无 `RETURNING`（`Backend::supports_returning() == false`）：写后回读拆成
//! `UPDATE/INSERT` + `SELECT` 两条语句，由 Host 执行器顺序执行（执行器只做「绑定 + 执行」，
//! 不做任何 SQL 拼装）。

use serde_json::{json, Value};

use crate::schema::{Registry, Schema};

use super::filter::build_filter;
use super::ir::{RowCol, RowShape, SqlStmt};
use super::Backend;

/// 翻译写命令
///
/// 写路径不产出 `warnings`（`unsupported` 机制在 select 侧）——需要告警的翻译在此直接报错，
/// 故不再保留占位参数（评测报告 I-2）。
pub fn translate_write(
    backend: Backend,
    cmd: &Value,
    registry: &Registry,
) -> Result<Vec<SqlStmt>, String> {
    let kind = cmd.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let collection = cmd.get("collection").and_then(|v| v.as_str()).unwrap_or("");
    // 三元组定位：source 缺省 default / namespace 缺省 null（兼容无定位字段的旧命令）
    let source = cmd
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or(crate::datasource::DEFAULT_SOURCE);
    let namespace = cmd.get("namespace").and_then(|v| v.as_str());
    // 结构 schema 按 (source, collection) 定位（override 回落见 `get_for_command`）；
    // 表名限定跟随命令 namespace（§6：定位由命令决定，结构由 Registry 决定）
    let mut schema = registry
        .get_for_command(source, namespace, collection)?
        .clone();
    if let Some(ns) = namespace {
        schema.namespace = Some(ns.to_string());
    }
    let schema = &schema;

    match kind {
        "insertOne" => {
            let doc = cmd.get("doc").cloned().unwrap_or(json!({}));
            Ok(vec![build_insert(backend, schema, &doc)])
        }
        "insertMany" => {
            let docs = cmd
                .get("docs")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            // 多条 → 一条 VALUES (...) 多组
            if docs.is_empty() {
                return Err("insertMany 无文档".to_string());
            }
            let upsert_by_id = cmd
                .get("upsertById")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            build_insert_many(backend, schema, &docs, upsert_by_id)
        }
        "updateMany" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let update = cmd.get("update").cloned().unwrap_or(json!({}));
            translate_update_many(backend, schema, &filter, &update)
        }
        "findOneAndUpdate" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let update = cmd.get("update").cloned().unwrap_or(json!({}));
            let options = cmd.get("options").cloned().unwrap_or(json!({}));
            translate_find_one_and_update(backend, schema, &filter, &update, &options)
        }
        "deleteMany" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let mut seq = 0usize;
            // 写路径无告警通道（None）：filter 中出现无法表达的语义组合时直接报错（见 filter::Warnings）
            let wh = build_filter(&filter, backend, "t", &col_map(schema), &mut seq, None)?;
            let where_sql = if wh.text.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", wh.text)
            };
            let text = format!("DELETE FROM {} AS t{}", tname(backend, schema), where_sql);
            Ok(vec![SqlStmt::write(text, wh.params)])
        }
        _ => Err(format!("translate: 未知写命令 kind = {}", kind)),
    }
}

// ─── 参数绑定 ────────────────────────────────────────────────

/// 参数绑定游标：统一 `?`（MySQL/SQLite）与 `$n`（PostgreSQL）占位符。
///
/// `bind` 返回当前位置的占位符并把值入队；PostgreSQL 的 `$n` 序号与队列长度一致，
/// 从而保证「文本占位符顺序 == params 顺序」。
struct Binder {
    backend: Backend,
    params: Vec<Value>,
}

impl Binder {
    fn new(backend: Backend) -> Self {
        Binder {
            backend,
            params: Vec::new(),
        }
    }

    fn bind(&mut self, v: Value) -> String {
        let ph = self.backend.placeholder(self.params.len());
        self.params.push(v);
        ph
    }

    /// 下一个占位符序号（PostgreSQL 用；MySQL/SQLite 恒 0 起点无影响）
    fn seq(&self) -> usize {
        self.params.len()
    }
}

// ─── 字段/列白名单 ───────────────────────────────────────────

/// 标量字段 → 列名（写侧薄包装；语义唯一出处见 [`super::scalar_column`]）
fn scalar_col(schema: &Schema, field: &str) -> Option<String> {
    super::scalar_column(schema, field)
}

fn col_map(schema: &Schema) -> impl Fn(&str) -> Option<String> + '_ {
    move |field: &str| scalar_col(schema, field)
}

/// 回读列：`_id`（物理主键列）恒首位 + 其余标量字段按字典序（确定性输出，供 parity）。
///
/// `_id` 不要求出现在 `schema.fields`（core 不自动补 `_id`），但物理表恒有该列，
/// 且 `restore_rows` 依赖它做根分组，故强制补上。
fn returning_cols(schema: &Schema) -> Vec<String> {
    let mut cols: Vec<String> = schema
        .fields
        .keys()
        .filter(|f| {
            !matches!(
                schema.fields.get(f.as_str()).map(|d| d.field_type.as_str()),
                Some("object") | Some("array")
            )
        })
        .filter(|f| f.as_str() != "_id")
        .cloned()
        .collect();
    cols.sort();
    cols.insert(0, "_id".to_string());
    cols
}

/// 回读列 → RowShape（标量直接还原到 `[field]`）
fn returning_shape(cols: &[String]) -> RowShape {
    RowShape {
        columns: cols
            .iter()
            .map(|c| RowCol::scalar(c, &[c.as_str()]))
            .collect(),
    }
}

// ─── INSERT ─────────────────────────────────────────────────

/// 单条 insert
fn build_insert(backend: Backend, schema: &Schema, doc: &Value) -> SqlStmt {
    let cols = scalar_cols(schema, doc);
    if cols.is_empty() {
        // 空插入：INSERT 空行
        return match backend {
            Backend::Mysql => SqlStmt::write(
                format!("INSERT INTO {} () VALUES ()", tname(backend, schema)),
                Vec::new(),
            ),
            _ => SqlStmt::write(
                format!("INSERT INTO {} DEFAULT VALUES", tname(backend, schema)),
                Vec::new(),
            ),
        };
    }
    let cols_sql = cols
        .iter()
        .map(|c| q(backend, c))
        .collect::<Vec<_>>()
        .join(", ");
    let mut binder = Binder::new(backend);
    let phs: Vec<String> = cols
        .iter()
        .map(|c| binder.bind(doc.get(c).cloned().unwrap_or(Value::Null)))
        .collect();
    let text = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        tname(backend, schema),
        cols_sql,
        phs.join(", "),
    );
    SqlStmt::write(text, binder.params)
}

/// 多条 insert → 一条 `INSERT ... VALUES (...), (...)`
///
/// `upsert_by_id`（归档幂等）：`_id` 冲突时改为整行覆盖/更新 ——
/// PostgreSQL `ON CONFLICT (_id) DO UPDATE`、MySQL `ON DUPLICATE KEY UPDATE`、
/// SQLite `INSERT OR REPLACE`。
fn build_insert_many(
    backend: Backend,
    schema: &Schema,
    docs: &[Value],
    upsert_by_id: bool,
) -> Result<Vec<SqlStmt>, String> {
    let cols = scalar_cols(schema, &docs[0]);
    if cols.is_empty() {
        return Err("insertMany 无标量可写字段".to_string());
    }
    let cols_sql = cols
        .iter()
        .map(|c| q(backend, c))
        .collect::<Vec<_>>()
        .join(", ");
    let mut binder = Binder::new(backend);
    let mut groups: Vec<String> = Vec::new();
    for doc in docs {
        let phs: Vec<String> = cols
            .iter()
            .map(|c| binder.bind(doc.get(c).cloned().unwrap_or(Value::Null)))
            .collect();
        groups.push(format!("({})", phs.join(", ")));
    }
    let mut text = format!(
        "INSERT INTO {} ({}) VALUES {}",
        tname(backend, schema),
        cols_sql,
        groups.join(", "),
    );
    if upsert_by_id {
        let upd_cols: Vec<&String> = cols.iter().filter(|c| c.as_str() != "_id").collect();
        text = match backend {
            Backend::Sqlite => format!("INSERT OR REPLACE INTO {}", &text["INSERT INTO ".len()..]),
            Backend::Postgres => {
                if upd_cols.is_empty() {
                    format!("{} ON CONFLICT ({}) DO NOTHING", text, q(backend, "_id"))
                } else {
                    let sets = upd_cols
                        .iter()
                        .map(|c| format!("{} = EXCLUDED.{}", q(backend, c), q(backend, c)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "{} ON CONFLICT ({}) DO UPDATE SET {}",
                        text,
                        q(backend, "_id"),
                        sets
                    )
                }
            }
            Backend::Mysql => {
                let sets = if upd_cols.is_empty() {
                    // 无可更新列时的 no-op 赋值，保证语法合法
                    format!("{} = {}", q(backend, "_id"), q(backend, "_id"))
                } else {
                    upd_cols
                        .iter()
                        .map(|c| format!("{} = VALUES({})", q(backend, c), q(backend, c)))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                format!("{} ON DUPLICATE KEY UPDATE {}", text, sets)
            }
        };
    }
    Ok(vec![SqlStmt::write(text, binder.params)])
}

/// 标量可写列（排除 object/array 附属表字段）；`_id` 为保留字段，文档带时恒写首位
fn scalar_cols(schema: &Schema, doc: &Value) -> Vec<String> {
    let mut cols: Vec<String> = schema
        .fields
        .iter()
        .filter(|(_k, f)| !(f.field_type == "object" || f.field_type == "array"))
        .map(|(k, _)| k.clone())
        .filter(|k| doc.get(k).map(|v| !v.is_null()).unwrap_or(false))
        .collect();
    if doc.get("_id").map(|v| !v.is_null()).unwrap_or(false) && !cols.contains(&"_id".to_string()) {
        cols.insert(0, "_id".to_string());
    }
    cols
}

// ─── UPDATE ─────────────────────────────────────────────────

/// 由 Mongo update doc 构建 SET 赋值列表。
///
/// 支持的操作符：`$set`（`col = ?`）、`$inc`（`col = COALESCE(col, 0) + ?`）、
/// `$unset`（`col = NULL`）；`$setOnInsert` 由 upsert 分支单独处理，此处忽略。
/// 其余操作符（`$push` / `$addToSet` / `$pull` …）无法安全映射为标量 UPDATE → 报错。
fn build_assignments(
    binder: &mut Binder,
    schema: &Schema,
    update: &Value,
) -> Result<Vec<String>, String> {
    let Some(obj) = update.as_object() else {
        return Err("update 必须是对象".to_string());
    };
    if obj.is_empty() {
        return Err("没有提供要更新的字段".to_string());
    }
    let mut assigns: Vec<String> = Vec::new();
    for (op, val) in obj {
        match op.as_str() {
            "$set" => {
                let Some(fields) = val.as_object() else {
                    continue;
                };
                for (k, v) in fields {
                    if v.is_null() {
                        continue;
                    }
                    let Some(col) = scalar_col(schema, k) else {
                        continue;
                    };
                    let ph = binder.bind(v.clone());
                    assigns.push(format!("{} = {}", binder.backend.quote_ident(&col), ph));
                }
            }
            "$inc" => {
                let Some(fields) = val.as_object() else {
                    continue;
                };
                for (k, v) in fields {
                    let Some(col) = scalar_col(schema, k) else {
                        continue;
                    };
                    let qc = binder.backend.quote_ident(&col);
                    let ph = binder.bind(v.clone());
                    assigns.push(format!("{} = COALESCE({}, 0) + {}", qc, qc, ph));
                }
            }
            "$unset" => {
                let Some(fields) = val.as_object() else {
                    continue;
                };
                for k in fields.keys() {
                    let Some(col) = scalar_col(schema, k) else {
                        continue;
                    };
                    assigns.push(format!("{} = NULL", binder.backend.quote_ident(&col)));
                }
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
    Ok(assigns)
}

/// `WHERE` 片段（沿用统一 filter 翻译；占位序号接续 SET 参数）
/// 缺陷 D-02：不可翻译条件现在显式报错，绝不静默丢条件
fn where_of(binder: &mut Binder, schema: &Schema, filter: &Value) -> Result<String, String> {
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

fn translate_update_many(
    backend: Backend,
    schema: &Schema,
    filter: &Value,
    update: &Value,
) -> Result<Vec<SqlStmt>, String> {
    let mut binder = Binder::new(backend);
    let assigns = build_assignments(&mut binder, schema, update)?;
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

fn translate_find_one_and_update(
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
    let assigns = build_assignments(&mut binder, schema, update)?;
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
    if backend.supports_returning() {
        let ret = cols
            .iter()
            .map(|c| q(backend, c))
            .collect::<Vec<_>>()
            .join(", ");
        let mut stmt = SqlStmt::write(format!("{} RETURNING {}", update_text, ret), binder.params);
        stmt.is_write = true;
        stmt.row_shape = Some(returning_shape(&cols));
        stmt.returning = cols.clone();
        Ok(vec![stmt])
    } else {
        // MySQL：无 RETURNING → 写后按同一 filter 回读（两段编排由 Host 执行器顺序执行）
        let update_stmt = SqlStmt::write(update_text, binder.params);

        let select_list = cols
            .iter()
            .map(|c| format!("t.{}", q(backend, c)))
            .collect::<Vec<_>>()
            .join(", ");
        let mut read_binder = Binder::new(backend);
        let where_sql_s = where_of(&mut read_binder, schema, filter)?;
        let select_text = format!(
            "SELECT {} FROM {} t{}",
            select_list,
            tname(backend, schema),
            where_sql_s,
        );
        let mut read_stmt = SqlStmt::write(select_text, read_binder.params);
        read_stmt.is_write = false;
        read_stmt.row_shape = Some(returning_shape(&cols));
        Ok(vec![update_stmt, read_stmt])
    }
}

/// upsert：`INSERT ... ON CONFLICT/ON DUPLICATE KEY ...` + 回读
fn translate_upsert(
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
    if let Some(o) = update.get("$setOnInsert").and_then(|v| v.as_object()) {
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
    if let Some(o) = update.get("$set").and_then(|v| v.as_object()) {
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
    let mut stmts: Vec<SqlStmt> = Vec::new();

    if backend.supports_returning() {
        let target_sql = target
            .iter()
            .map(|c| q(backend, c))
            .collect::<Vec<_>>()
            .join(", ");
        let ret = returning
            .iter()
            .map(|c| q(backend, c))
            .collect::<Vec<_>>()
            .join(", ");
        let text = format!(
            "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET {} RETURNING {}",
            tname(backend, schema),
            cols_sql,
            phs.join(", "),
            target_sql,
            assigns.join(", "),
            ret,
        );
        let mut stmt = SqlStmt::write(text, binder.params);
        stmt.is_write = true;
        stmt.row_shape = Some(returning_shape(&returning));
        stmt.returning = returning.clone();
        stmts.push(stmt);
    } else {
        // MySQL：`ON DUPLICATE KEY UPDATE` 后按 filter 回读
        let text = format!(
            "INSERT INTO {} ({}) VALUES ({}) ON DUPLICATE KEY UPDATE {}",
            tname(backend, schema),
            cols_sql,
            phs.join(", "),
            assigns.join(", "),
        );
        stmts.push(SqlStmt::write(text, binder.params));

        let select_list = returning
            .iter()
            .map(|c| format!("t.{}", q(backend, c)))
            .collect::<Vec<_>>()
            .join(", ");
        let mut read_binder = Binder::new(backend);
        let where_sql = where_of(&mut read_binder, schema, filter)?;
        let select_text = format!(
            "SELECT {} FROM {} t{}",
            select_list,
            tname(backend, schema),
            where_sql,
        );
        let mut read_stmt = SqlStmt::write(select_text, read_binder.params);
        read_stmt.is_write = false;
        read_stmt.row_shape = Some(returning_shape(&returning));
        stmts.push(read_stmt);
    }

    Ok(stmts)
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

fn q(backend: Backend, ident: &str) -> String {
    backend.quote_ident(ident)
}

/// 表名 SQL：带 schema.namespace 限定（区别于列/别名的 `q`）
fn tname(backend: Backend, schema: &Schema) -> String {
    backend.qualified_table(schema.ns(), &schema.collection)
}
