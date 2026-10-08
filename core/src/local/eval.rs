//! 本地磁盘数据源 —— 命令分发（core 规划产出的 Mongo 命令 JSON → 求值）。
//!
//! 语义基准 = **MongoDB 驱动语义**：本层与 `mongo` 路径吃**同一份**命令、必须同结果。
//! 纯逻辑：无 IO / 无时钟 / 无随机 ——「整目录集合快照」由宿主提供并回收（文件读写只在宿主）。
//!
//! 返回契约（执行文档 §4.1）：
//! `{ "result": <驱动等价返回值>, "changed": ["<集合名>", …], "collections": {…} }`
//!   - 读命令（`find` / `aggregate` / `countDocuments` / `findOne`）→ `changed` 恒 `[]`，
//!     `collections` 原样回传；
//!   - 写命令 → `collections` 为变更后快照、`changed` 为被改集合名。
//!
//! 单集合文档数超护栏 [`MAX_LOCAL_COLLECTION_DOCS`] → `Err`（禁静默截断）。

use serde_json::{json, Map, Value};

use crate::local::value::{apply_projection, MAX_LOCAL_COLLECTION_DOCS};
use crate::local::{filter, pipeline, update};

/// 本地磁盘数据源单命令求值（纯逻辑，无 IO / 无时钟 / 无随机）。
///
/// `collections`：`{ "<物理集合名>": [文档, …] }` —— 宿主提供的**整目录快照**；
///                命令 `$lookup.from` 指向的集合必须也在其中（缺集合按空集合处理）。
/// `command`：core 规划产出的 Mongo 命令 JSON（与 mongo 路径**同一份**）。
pub fn eval_command(collections: &Value, command: &Value) -> Result<Value, String> {
    let cmd = command
        .as_object()
        .ok_or_else(|| format!("command 必须是对象，收到 {}", type_name(command)))?;
    let kind = cmd
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "command 缺少 kind".to_string())?;
    let name = cmd
        .get("collection")
        .and_then(Value::as_str)
        .ok_or_else(|| "command 缺少 collection".to_string())?;
    let mut map = collections
        .as_object()
        .cloned()
        .ok_or_else(|| format!("collections 必须是对象 {{集合名: [文档…]}}，收到 {}", type_name(collections)))?;

    // ── 读命令：changed 恒空、collections 原样回传 ──────────────────────────
    match kind {
        "find" => {
            let rows = rows_of(&map, name)?;
            let condition = cmd.get("filter").cloned().unwrap_or(Value::Null);
            let projection = cmd.get("projection").cloned().unwrap_or(Value::Null);
            let mut out = Vec::new();
            for row in &rows {
                if filter::matches(row, &condition)? {
                    out.push(project(row, &projection)?);
                }
            }
            return Ok(envelope(Value::Array(out), Vec::new(), collections));
        }
        "aggregate" => {
            let stages = cmd
                .get("pipeline")
                .and_then(Value::as_array)
                .ok_or_else(|| "aggregate 需要 pipeline 数组".to_string())?;
            let out = pipeline::run(collections, name, stages)?;
            return Ok(envelope(Value::Array(out), Vec::new(), collections));
        }
        "countDocuments" => {
            let rows = rows_of(&map, name)?;
            let condition = cmd.get("filter").cloned().unwrap_or(Value::Null);
            let mut n = 0_usize;
            for row in &rows {
                if filter::matches(row, &condition)? {
                    n += 1;
                }
            }
            return Ok(envelope(json!(n), Vec::new(), collections));
        }
        "findOne" => {
            let rows = rows_of(&map, name)?;
            let condition = cmd.get("filter").cloned().unwrap_or(Value::Null);
            let projection = cmd.get("projection").cloned().unwrap_or(Value::Null);
            let mut found = Value::Null;
            for row in &rows {
                if filter::matches(row, &condition)? {
                    found = project(row, &projection)?;
                    break;
                }
            }
            return Ok(envelope(found, Vec::new(), collections));
        }
        _ => {}
    }

    // ── 写命令：变更后快照 + 非空 changed ──────────────────────────────────
    let mut rows = rows_of(&map, name)?;
    let result = match kind {
        "insertOne" => {
            let doc = cmd
                .get("doc")
                .ok_or_else(|| "insertOne 缺少 doc".to_string())?;
            update::insert_one(&mut rows, doc)?
        }
        "insertMany" => {
            let docs = cmd
                .get("docs")
                .and_then(Value::as_array)
                .ok_or_else(|| "insertMany 需要 docs 数组".to_string())?;
            let upsert_by_id = cmd
                .get("upsertById")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            update::insert_many(&mut rows, docs, upsert_by_id)?
        }
        "updateMany" => {
            let condition = cmd.get("filter").cloned().unwrap_or(Value::Null);
            let spec = cmd
                .get("update")
                .ok_or_else(|| "updateMany 缺少 update".to_string())?;
            update::update_many(&mut rows, &condition, spec)?
        }
        "findOneAndUpdate" => {
            let condition = cmd.get("filter").cloned().unwrap_or(Value::Null);
            let spec = cmd
                .get("update")
                .ok_or_else(|| "findOneAndUpdate 缺少 update".to_string())?;
            let options = cmd.get("options").cloned().unwrap_or(Value::Null);
            update::find_one_and_update(&mut rows, &condition, spec, &options)?
        }
        "deleteMany" => {
            let condition = cmd.get("filter").cloned().unwrap_or(Value::Null);
            update::delete_many(&mut rows, &condition)?
        }
        other => {
            return Err(format!(
                "未支持的命令: {other}（本地求值器拒绝静默；支持 find/aggregate/countDocuments/findOne/insertOne/insertMany/updateMany/findOneAndUpdate/deleteMany）"
            ));
        }
    };

    guard_docs(name, &rows)?;
    map.insert(name.to_string(), Value::Array(rows));
    Ok(envelope(result, vec![name.to_string()], &Value::Object(map)))
}

/// 返回包络：`{ result, changed, collections }`。
fn envelope(result: Value, changed: Vec<String>, collections: &Value) -> Value {
    json!({
        "result": result,
        "changed": changed,
        "collections": collections,
    })
}

/// 取集合文档数组；缺失 → 空数组；非数组 → `Err`（禁静默当空集合）。
fn rows_of(map: &Map<String, Value>, name: &str) -> Result<Vec<Value>, String> {
    match map.get(name) {
        None => Ok(Vec::new()),
        Some(Value::Array(a)) => Ok(a.clone()),
        Some(other) => Err(format!(
            "集合 \"{name}\" 必须是文档数组，收到 {}",
            type_name(other)
        )),
    }
}

/// 投影：`null`（未声明）→ 原样；否则按 [`apply_projection`]。
fn project(doc: &Value, projection: &Value) -> Result<Value, String> {
    if projection.is_null() {
        return Ok(doc.clone());
    }
    apply_projection(doc, projection)
}

/// 文档数护栏（禁静默截断）。
fn guard_docs(name: &str, rows: &[Value]) -> Result<(), String> {
    if rows.len() > MAX_LOCAL_COLLECTION_DOCS {
        return Err(format!(
            "集合 \"{name}\" 文档数 {} 超过护栏 MAX_LOCAL_COLLECTION_DOCS={}（拒绝静默截断）",
            rows.len(),
            MAX_LOCAL_COLLECTION_DOCS
        ));
    }
    Ok(())
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn read_commands_have_empty_changed_and_passthrough() {
        let collections = json!({ "users": [{"_id": "u1", "name": "Ada", "age": 36}] });
        // find
        let out = eval_command(
            &collections,
            &json!({ "kind": "find", "collection": "users", "filter": { "age": { "$gt": 30 } } }),
        )
        .unwrap();
        assert_eq!(out["result"], json!([{"_id": "u1", "name": "Ada", "age": 36}]));
        assert_eq!(out["changed"], json!([]));
        assert_eq!(out["collections"], collections);
        // countDocuments
        let c = eval_command(
            &collections,
            &json!({ "kind": "countDocuments", "collection": "users", "filter": null }),
        )
        .unwrap();
        assert_eq!(c["result"], json!(1));
        assert_eq!(c["changed"], json!([]));
        // findOne 未命中 → null
        let one = eval_command(
            &collections,
            &json!({ "kind": "findOne", "collection": "users", "filter": { "_id": "nope" } }),
        )
        .unwrap();
        assert_eq!(one["result"], Value::Null);
    }

    #[test]
    fn find_projection_applied() {
        let collections = json!({ "users": [{"_id": "u1", "name": "Ada", "age": 36}] });
        let out = eval_command(
            &collections,
            &json!({
                "kind": "find",
                "collection": "users",
                "filter": null,
                "projection": { "name": 1, "_id": 0 }
            }),
        )
        .unwrap();
        assert_eq!(out["result"], json!([{"name": "Ada"}]));
    }

    #[test]
    fn insert_then_query_roundtrip() {
        let empty = json!({});
        let ins = eval_command(
            &empty,
            &json!({ "kind": "insertOne", "collection": "users", "doc": { "_id": "u1", "name": "Ada" } }),
        )
        .unwrap();
        assert_eq!(ins["result"], json!({ "_id": "u1", "name": "Ada" }));
        assert_eq!(ins["changed"], json!(["users"]));
        let snapshot = ins["collections"].clone();
        // 用变更后快照续查
        let q = eval_command(
            &snapshot,
            &json!({ "kind": "find", "collection": "users", "filter": { "_id": "u1" } }),
        )
        .unwrap();
        assert_eq!(q["result"], json!([{ "_id": "u1", "name": "Ada" }]));
    }

    /// 总纲 E4 的关系查询原样命令（aggregate：`$match + $lookup + $unwind + $project`）。
    #[test]
    fn aggregate_relation_golden() {
        let collections = json!({
            "probeUsers": [
                {"_id": "pu1", "name": "Ada"},
                {"_id": "pu2", "name": "Bob"}
            ],
            "probePosts": [
                {"_id": "pp1", "title": "P1", "userId": "pu1"},
                {"_id": "pp2", "title": "P2", "userId": "pu2"}
            ]
        });
        let command = json!({
            "kind": "aggregate",
            "collection": "probePosts",
            "pipeline": [
                {"$match": {}},
                {"$lookup": {
                    "as": "author",
                    "from": "probeUsers",
                    "let": {"rel_userId": {"$ifNull": ["$userId", null]}},
                    "pipeline": [
                        {"$match": {"$expr": {"$eq": ["$_id", "$$rel_userId"]}}},
                        {"$project": {"_id": 1, "name": 1}}
                    ]
                }},
                {"$unwind": {"path": "$author", "preserveNullAndEmptyArrays": true}},
                {"$project": {"_id": 1, "author": 1, "title": 1}}
            ]
        });
        let out = eval_command(&collections, &command).unwrap();
        assert_eq!(
            out["result"],
            json!([
                {"_id": "pp1", "author": {"_id": "pu1", "name": "Ada"}, "title": "P1"},
                {"_id": "pp2", "author": {"_id": "pu2", "name": "Bob"}, "title": "P2"}
            ])
        );
        assert_eq!(out["changed"], json!([]));
        assert_eq!(out["collections"], collections);
    }

    #[test]
    fn write_commands_envelope_and_counts() {
        let mut snapshot = json!({ "posts": [
            {"_id": "p1", "title": "P1", "userId": "u1"},
            {"_id": "p2", "title": "P2", "userId": "u2"}
        ]});
        // insertMany
        let ins = eval_command(
            &snapshot,
            &json!({ "kind": "insertMany", "collection": "posts", "docs": [{ "_id": "p3" }] }),
        )
        .unwrap();
        assert_eq!(ins["result"], json!({ "insertedCount": 1 }));
        assert_eq!(ins["changed"], json!(["posts"]));
        snapshot = ins["collections"].clone();
        // updateMany
        let upd = eval_command(
            &snapshot,
            &json!({
                "kind": "updateMany",
                "collection": "posts",
                "filter": { "userId": "u1" },
                "update": { "$set": { "seen": true } }
            }),
        )
        .unwrap();
        assert_eq!(upd["result"], json!({ "modifiedCount": 1 }));
        snapshot = upd["collections"].clone();
        // deleteMany
        let del = eval_command(
            &snapshot,
            &json!({ "kind": "deleteMany", "collection": "posts", "filter": { "_id": "p3" } }),
        )
        .unwrap();
        assert_eq!(del["result"], json!({ "deletedCount": 1 }));
    }

    #[test]
    fn archive_upsert_by_id_through_eval() {
        let snapshot = json!({ "usersDeleted": [] });
        let out = eval_command(
            &snapshot,
            &json!({
                "kind": "insertMany",
                "collection": "usersDeleted",
                "upsertById": true,
                "docs": [{ "_id": "u1", "deletedAt": 100 }]
            }),
        )
        .unwrap();
        assert_eq!(out["result"], json!({ "insertedCount": 1 }));
        assert_eq!(
            out["collections"],
            json!({ "usersDeleted": [{"_id": "u1", "deletedAt": 100}] })
        );
    }

    #[test]
    fn unknown_command_and_collection_type_error() {
        // 未知命令 → Err
        assert!(eval_command(&json!({}), &json!({ "kind": "drop", "collection": "t" })).is_err());
        // 缺少 kind / collection → Err
        assert!(eval_command(&json!({}), &json!({ "collection": "t" })).is_err());
        assert!(eval_command(&json!({}), &json!({ "kind": "find" })).is_err());
        // 集合非数组 → Err（禁静默当空集合）
        assert!(eval_command(&json!({ "t": 1 }), &json!({ "kind": "find", "collection": "t" })).is_err());
        // collections 非对象 → Err
        assert!(eval_command(&json!(5), &json!({ "kind": "find", "collection": "t" })).is_err());
    }

    #[test]
    fn guard_docs_over_limit_errors() {
        // 预置已达护栏的集合，再插一条 → 超限 Err（不构造超大批次，避免冲突检查放大）
        let docs: Vec<Value> = (0..MAX_LOCAL_COLLECTION_DOCS)
            .map(|i| json!({ "_id": i }))
            .collect();
        let snapshot = json!({ "big": docs });
        let command = json!({ "kind": "insertOne", "collection": "big", "doc": { "_id": "x" } });
        let err = eval_command(&snapshot, &command).unwrap_err();
        assert!(err.contains("MAX_LOCAL_COLLECTION_DOCS"), "err = {err}");
    }
}
