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
/// {
///   "backend": "...",
///   "stmts": [ SqlStmt.to_value() ],
///   "warnings": [ ... ],
///   "unsupported": [ { "code": "childLimit", "as": "...", "reason": "..." } ]
/// }
/// ```
/// `unsupported` 非空表示存在无法安全下推的组合（如 `$lookup` 子 `$limit` 每父 top-N）；
/// 此时 `stmts` 不含该段，Host 必须兜底（拒绝或降级重查），**绝不返回错误结果**。
pub fn translate(
    backend: Backend,
    cmd: &Value,
    registry: &Registry,
) -> Result<Value, String> {
    let mut warnings: Vec<String> = Vec::new();
    let mut unsupported: Vec<Value> = Vec::new();
    let kind = cmd.get("kind").and_then(|v| v.as_str()).unwrap_or("");

    let stmts = match kind {
        "find" | "findOne" | "countDocuments" | "aggregate" => {
            translate_select(backend, cmd, registry, &mut warnings, &mut unsupported)?
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
        "unsupported": unsupported,
    }))
}