//! 写语句翻译：insertOne / insertMany（标量列；object/array 附属表属于后续里程碑）

use serde_json::{json, Value};

use crate::schema::{Registry, Schema};

use super::ir::SqlStmt;
use super::Backend;

/// 翻译写命令
pub fn translate_write(
    backend: Backend,
    cmd: &Value,
    registry: &Registry,
    _warnings: &mut Vec<String>,
) -> Result<Vec<SqlStmt>, String> {
    let kind = cmd.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let collection = cmd.get("collection").and_then(|v| v.as_str()).unwrap_or("");
    let schema = registry.get_by_collection(collection)?;

    match kind {
        "insertOne" => {
            let doc = cmd.get("doc").cloned().unwrap_or(json!({}));
            Ok(vec![build_insert(backend, schema, &doc, false)])
        }
        "insertMany" => {
            let docs = cmd.get("docs").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            // 多条 → 一条 VALUES (...) 多组
            if docs.is_empty() {
                return Err("insertMany 无文档".to_string());
            }
            // 提取统一列（取第一条的非 object/array 标量键）
            let cols = scalar_cols(schema, &docs[0]);
            if cols.is_empty() {
                return Err("insertMany 无标量可写字段".to_string());
            }
            let mut text = String::from("INSERT INTO ");
            text.push_str(&q(backend, &schema.collection));
            let cols_sql = cols.iter().map(|c| q(backend, c)).collect::<Vec<_>>().join(", ");
            text.push_str(&format!(" ({}) VALUES ", cols_sql));
            let mut params: Vec<Value> = Vec::new();
            let mut groups = Vec::new();
            for doc in docs {
                let vals: Vec<Value> = cols
                    .iter()
                    .map(|c| {
                        let v = doc.get(c).cloned().unwrap_or(Value::Null);
                        params.push(v.clone());
                        v
                    })
                    .collect();
                let _ = vals;
                let phs: Vec<String> = if matches!(backend, Backend::Postgres) {
                    // 占位符序号 = 累计 param 位置索引
                    let start = params.len() - cols.len();
                    (0..cols.len()).map(|k| format!("${}", start + k + 1)).collect()
                } else {
                    cols.iter().map(|_| "?".to_string()).collect()
                };
                groups.push(format!("({})", phs.join(", ")));
            }
            text.push_str(&groups.join(", "));
            Ok(vec![SqlStmt::write(text, params)])
        }
        // findOneAndUpdate / updateMany / deleteMany 属于后续里程碑，先明确拒绝避免错误 SQL
        "findOneAndUpdate" | "updateMany" => {
            Err(format!("translate: 写命令 {} 暂不支持（里程碑范围）", kind))
        }
        "deleteMany" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let mut seq = 0usize;
            let wh = super::filter::build_filter(&filter, backend, "t", &|_f: &str| Some(_f.to_string()), &mut seq);
            let where_sql = if wh.text.is_empty() { String::new() } else { format!(" WHERE {}", wh.text) };
            let text = format!("DELETE FROM {} t{}", q(backend, &schema.collection), where_sql);
            Ok(vec![SqlStmt::write(text, wh.params)])
        }
        _ => Err(format!("translate: 未知写命令 kind = {}", kind)),
    }
}

/// 单条 insert
fn build_insert(backend: Backend, schema: &Schema, doc: &Value, _multi: bool) -> SqlStmt {
    let cols = scalar_cols(schema, doc);
    let text = {
        if cols.is_empty() {
            // 空插入：INSERT 空行
            return match backend {
                Backend::Mysql => SqlStmt::write("INSERT INTO {} () VALUES ()".replace("{}", &q(backend, &schema.collection)), Vec::new()),
                _ => SqlStmt::write(format!("INSERT INTO {} DEFAULT VALUES", q(backend, &schema.collection)), Vec::new()),
            };
        }
        let cols_sql = cols.iter().map(|c| q(backend, c)).collect::<Vec<_>>().join(", ");
        let mut text = format!("INSERT INTO {} ({}) VALUES (", q(backend, &schema.collection), cols_sql);
        let phs: Vec<String> = (0..cols.len())
            .map(|i| backend.placeholder(i))
            .collect();
        text.push_str(&phs.join(", "));
        text.push(')');
        text
    };
    let params: Vec<Value> = cols.iter().map(|c| doc.get(c).cloned().unwrap_or(Value::Null)).collect();
    SqlStmt::write(text, params)
}

/// 标量可写列（排除 object/array 附属表字段）；`_id` 为保留字段，文档带时恒写首位
fn scalar_cols(schema: &Schema, doc: &Value) -> Vec<String> {
    let mut cols: Vec<String> = schema
        .fields
        .iter()
        .filter(|(k, f)| !(f.field_type == "object" || f.field_type == "array"))
        .map(|(k, _)| k.clone())
        .filter(|k| doc.get(k).map(|v| !v.is_null()).unwrap_or(false))
        .collect();
    if doc.get("_id").map(|v| !v.is_null()).unwrap_or(false) && !cols.contains(&"_id".to_string()) {
        cols.insert(0, "_id".to_string());
    }
    cols
}

fn q(backend: Backend, ident: &str) -> String {
    backend.quote_ident(ident)
}