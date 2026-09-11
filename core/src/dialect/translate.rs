//! 翻译入口：Mongo 命令 → 各后端 SqlStmt 列表

use serde_json::{json, Value};

use crate::schema::Registry;

use super::select::translate_select;
use super::write::translate_write;
use super::Backend;

/// 翻译入口：把一条 Mongo 命令 JSON 译为指定后端的 SQL 语句序列。
///
/// 返回：
/// ```json
/// { "backend": "...", "stmts": [ SqlStmt.to_value() ], "warnings": [ ... ] }
/// ```
pub fn translate(
    backend: Backend,
    cmd: &Value,
    registry: &Registry,
) -> Result<Value, String> {
    let mut warnings: Vec<String> = Vec::new();
    let kind = cmd.get("kind").and_then(|v| v.as_str()).unwrap_or("");

    let stmts = match kind {
        "find" | "findOne" | "countDocuments" | "aggregate" => {
            translate_select(backend, cmd, registry, &mut warnings)?
        }
        "insertOne" | "insertMany" | "deleteMany" | "updateMany" | "findOneAndUpdate" => {
            translate_write(backend, cmd, registry, &mut warnings)?
        }
        _ => return Err(format!("translate: 未支持的命令 kind = {}", kind)),
    };

    Ok(json!({
        "backend": backend.as_str(),
        "stmts": stmts.iter().map(|s| s.to_value()).collect::<Vec<_>>(),
        "warnings": warnings,
    }))
}