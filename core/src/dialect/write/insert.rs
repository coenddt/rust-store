//! INSERT 翻译（单条 / 多条，含 `_id` 冲突覆盖语义）。

use serde_json::Value;

use crate::schema::Schema;

use crate::dialect::ir::SqlStmt;
use crate::dialect::Backend;

use super::{bind_value, q, tname, Binder};

/// 单条 insert
pub(super) fn build_insert(backend: Backend, schema: &Schema, doc: &Value) -> SqlStmt {
    let mut cols = writable_cols(schema, doc);
    let present = present_value(schema, doc);
    cols.push("__present".to_string());
    if cols.len() == 1 {
        // 唯一列即 __present（无任何标量值）→ INSERT 空行 + 存在集合
        return match backend {
            Backend::Mysql => SqlStmt::write(
                format!(
                    "INSERT INTO {} (__present) VALUES (?)",
                    tname(backend, schema)
                ),
                vec![present],
            ),
            _ => SqlStmt::write(
                format!(
                    "INSERT INTO {} (__present) VALUES (?)",
                    tname(backend, schema)
                ),
                vec![present],
            ),
        };
    }
    let mut binder = Binder::new(backend);
    let phs: Vec<String> = cols
        .iter()
        .map(|c| {
            let p = present.clone();
            let v = if c == "__present" {
                p
            } else {
                // object/array 字段 → JSON 文本；其余标量原样
                bind_value(schema, c, &doc.get(c).cloned().unwrap_or(Value::Null))
            };
            binder.bind(v)
        })
        .collect();
    let mut text = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        tname(backend, schema),
        quote_join(backend, &cols),
        phs.join(", "),
    );
    // 阶段2：数据库自增主键 —— doc 无 `_id` 列时由数据库赋值；支持 RETURNING 的后端
    // 单语句回读自增值（宿主据此把 `_id` 回填进 insert 结果）。MySQL 无 RETURNING，
    // 由宿主执行器 lastrowid 回读。
    if !cols.iter().any(|c| c == "_id")
        && schema.id_is_autoincrement()
        && backend.supports_returning()
    {
        text.push_str(&format!(" RETURNING {}", q(backend, "_id")));
    }
    SqlStmt::write(text, binder.params)
}

pub(super) fn build_insert_many(
    backend: Backend,
    schema: &Schema,
    docs: &[Value],
    upsert_by_id: bool,
) -> Result<Vec<SqlStmt>, String> {
    let mut cols = writable_cols_union(schema, docs);
    cols.push("__present".to_string());
    if cols.len() == 1 {
        return Err("insertMany 无标量可写字段".to_string());
    }
    let mut binder = Binder::new(backend);
    let groups: Vec<String> = docs
        .iter()
        .map(|doc| {
            let present = present_value(schema, doc);
            let phs: Vec<String> = cols
                .iter()
                .map(|c| {
                    let p = present.clone();
                    let v = if c == "__present" {
                        p
                    } else {
                        bind_value(schema, c, &doc.get(c).cloned().unwrap_or(Value::Null))
                    };
                    binder.bind(v)
                })
                .collect();
            format!("({})", phs.join(", "))
        })
        .collect();
    let text = format!(
        "INSERT INTO {} ({}) VALUES {}",
        tname(backend, schema),
        quote_join(backend, &cols),
        groups.join(", "),
    );
    let text = if upsert_by_id {
        with_conflict_override(backend, text, &cols)
    } else {
        text
    };
    Ok(vec![SqlStmt::write(text, binder.params)])
}

/// 该文档「显式存在的字段集合」→ `,field1,field2,`（缺失 vs null 三态哨兵值）。
/// 集合 = doc 中已声明字段键（含 object/array JSON 列）+ `_id`（不论类型）；
/// 显式 `null` 也计入（存在但为 null），未声明键与缺失字段不计入。
fn present_value(schema: &Schema, doc: &Value) -> Value {
    let mut keys: Vec<String> = Vec::new();
    if let Some(m) = doc.as_object() {
        for (k, _) in m.iter() {
            // `_id` 恒为列；其余仅当 schema 已声明（标量 / object / array 均落列）
            let is_column = k == "_id" || schema.fields.contains_key(k);
            if is_column {
                keys.push(k.clone());
            }
        }
    }
    Value::String(format!(",{},", keys.join(",")))
}

/// 给 INSERT 追加 `_id` 冲突覆盖子句（归档幂等）
fn with_conflict_override(backend: Backend, text: String, cols: &[String]) -> String {
    let upd_cols: Vec<&String> = cols.iter().filter(|c| c.as_str() != "_id").collect();
    match backend {
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
    }
}

fn quote_join(backend: Backend, cols: &[String]) -> String {
    cols.iter()
        .map(|c| q(backend, c))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 可写列（标量 + object/array JSON 列；已声明字段中 doc 有非 null 值者）；
/// `_id` 为保留字段，文档带时恒写首位
fn writable_cols(schema: &Schema, doc: &Value) -> Vec<String> {
    let mut cols: Vec<String> = schema
        .fields
        .keys()
        .filter(|k| doc.get(k.as_str()).map(|v| !v.is_null()).unwrap_or(false))
        .cloned()
        .collect();
    if doc.get("_id").map(|v| !v.is_null()).unwrap_or(false) && !cols.contains(&"_id".to_string()) {
        cols.insert(0, "_id".to_string());
    }
    cols
}

/// 多文档插入的列集：**所有文档列键的并集**（schema 字段顺序 + `_id` 置首）。
///
/// 缺陷修复（评测 C-10-2）：此前 `build_insert_many` 仅以 `docs[0]` 推导列集，
/// 异构文档中「仅后续文档才有的字段」会被静默丢弃（INSERT 列集不含该列）。
/// Mongo `insertMany` 允许文档间字段不同（稀疏），故列集必须取并集，
/// 缺失字段在绑定阶段落 `NULL`。
fn writable_cols_union(schema: &Schema, docs: &[Value]) -> Vec<String> {
    let has_value = |k: &str| {
        docs.iter()
            .any(|d| d.get(k).map(|v| !v.is_null()).unwrap_or(false))
    };
    let mut cols: Vec<String> = schema
        .fields
        .keys()
        .filter(|k| has_value(k.as_str()))
        .cloned()
        .collect();
    if has_value("_id") && !cols.contains(&"_id".to_string()) {
        cols.insert(0, "_id".to_string());
    }
    cols
}
