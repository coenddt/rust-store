//! claim / compareAndSet 命令翻译：并发原语的三后端 SQL（行锁 / 子查询原子化）。
//!
//! Host 执行契约（`atomic_commands` 事务序列）：
//!
//! - **MySQL / PostgreSQL**：stmt1 锁定并取回待领 `_id`
//!   （`FOR UPDATE SKIP LOCKED`——MySQL 8+ / PG 9.5+；无此能力的旧版本语法错误
//!   显式抛出，**禁静默降级**）；stmt2 按 ids 写领取标记——SQL 文本含字面 token
//!   [`crate::command::CLAIMED_IDS`]，Host 在同事务内把 token 展开为 ids 占位符
//!   序列后执行，**参数序列 = 原 params（SET 值）+ ids**（占位符按文本出现顺序：
//!   SET 在前、IN 展开在后）。token 未展开必致语法错误 = 显式失败，禁静默。
//!   ids 为空（0 行命中）时 Host 跳过 stmt2（`IN ()` 非法 SQL，不可执行）。
//! - **SQLite**：单写者无重复领取窗口——stmt1 `UPDATE … WHERE _id IN (子查询)`
//!   原子完成领取（命中数由 `changes()` 读出）；stmt2 以**同一子查询文本**取回
//!   被领行（同连接同事务内可复用）。语义边界：stmt2 子查询在 UPDATE 之后求值，
//!   若 filter 引用了 update 修改的列，取行会漏命——filter 不应引用被修改列
//!   （见 README 边界说明），core 不特判（零语义发明）。
//!
//! `compareAndSet`：单条 `UPDATE … SET … WHERE <filter>`，形状与 updateMany
//! 完全一致（复用既有翻译）；matched 语义由 Host 从 rowCount / `changes()` 读出。

use serde_json::{json, Value};

use crate::dialect::filter::build_filter;
use crate::dialect::ir::{RowCol, RowShape, SqlStmt};
use crate::dialect::{field_column_ref, Backend, ColumnRef};
use crate::schema::Schema;

use super::update::translate_update_many;
use super::update::build_assignments;
use super::{col_map, returning_cols, returning_shape, tname, Binder};

/// claim 翻译：按后端产出领取 SQL 序列
pub(super) fn translate_claim(
    backend: Backend,
    schema: &Schema,
    cmd: &Value,
) -> Result<Vec<SqlStmt>, String> {
    let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
    let sort = cmd.get("sort").cloned().unwrap_or_else(|| json!([["_id", 1]]));
    let limit = cmd
        .get("limit")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "claim 命令缺少 limit（或非整数）".to_string())?;
    let update = cmd.get("update").cloned().unwrap_or(json!({}));
    if limit <= 0 {
        return Err(format!(
            "claim 的 limit 必须 > 0，实际 {limit}（禁静默取 1）"
        ));
    }

    let mut binder = Binder::new(backend);
    let assigns = build_assignments(&mut binder, schema, &update, None)?;
    if assigns.is_empty() {
        return Err("claim 的 update 没有提供要更新的字段".to_string());
    }

    match backend {
        Backend::Mysql | Backend::Postgres => {
            // stmt1：SELECT _id 锁定 + 取回（行锁避免重复领取；旧版本无 SKIP LOCKED
            // 能力时语法错误显式抛出，禁静默降级）
            let mut sel = Binder::new(backend);
            let mut seq = sel.seq();
            let wh = build_filter(&filter, backend, "t", &col_map(schema), &mut seq, None)?;
            sel.params.extend(wh.params.clone());
            let lim_ph = sel.bind(json!(limit));
            let where_sql = if wh.text.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", wh.text)
            };
            let text = format!(
                "SELECT t.{} FROM {} t{}{} LIMIT {} FOR UPDATE SKIP LOCKED",
                backend.pcol("_id"),
                tname(backend, schema),
                where_sql,
                order_by_of(backend, schema, &sort)?,
                lim_ph,
            );
            let shape = RowShape {
                columns: vec![RowCol::scalar("_id", &["_id"])],
                present_alias: None,
            };
            let stmt1 = SqlStmt::select(text, sel.params, shape);
            // stmt2：按 ids 写标记；token 由 Host 展开为 ids 占位符序列，
            // 参数序列 = 原 params（SET 值）+ ids（占位符按文本出现顺序）
            let upd_text = format!(
                "UPDATE {} AS t SET {} WHERE t.{} IN ({})",
                tname(backend, schema),
                assigns.join(", "),
                backend.pcol("_id"),
                crate::command::CLAIMED_IDS,
            );
            let stmt2 = SqlStmt::write(upd_text, binder.params);
            Ok(vec![stmt1, stmt2])
        }
        Backend::Sqlite => {
            // 单写者：UPDATE 子查询原子领取 + 同一子查询取行（单语句内参数各自独立游标）
            let mut seq = 0usize;
            let wh = build_filter(&filter, backend, "t", &col_map(schema), &mut seq, None)?;
            let where_sql = if wh.text.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", wh.text)
            };
            let order = order_by_of(backend, schema, &sort)?;
            let subquery = format!(
                "(SELECT t.{} FROM {} t{}{} LIMIT ?)",
                backend.pcol("_id"),
                tname(backend, schema),
                where_sql,
                order,
            );
            // stmt1：UPDATE … WHERE _id IN (子查询)；params = SET 值 + filter + limit
            let upd_text = format!(
                "UPDATE {} AS t SET {} WHERE t.{} IN {}",
                tname(backend, schema),
                assigns.join(", "),
                backend.pcol("_id"),
                subquery,
            );
            let mut upd_params = binder.params;
            upd_params.extend(wh.params.iter().cloned());
            upd_params.push(json!(limit));
            let stmt1 = SqlStmt::write(upd_text, upd_params);
            // stmt2：同一子查询取回被领行（含 `__present` 哨兵列，走既有行还原）
            let cols = returning_cols(schema);
            let select_list = cols
                .iter()
                .map(|c| format!("t.{}", backend.pcol(c)))
                .collect::<Vec<_>>()
                .join(", ");
            let sel_text = format!(
                "SELECT {} FROM {} t WHERE t.{} IN {}",
                select_list,
                tname(backend, schema),
                backend.pcol("_id"),
                subquery,
            );
            let mut sel_params = wh.params;
            sel_params.push(json!(limit));
            let stmt2 = SqlStmt::select(sel_text, sel_params, returning_shape(schema, &cols));
            Ok(vec![stmt1, stmt2])
        }
    }
}

/// compareAndSet 翻译：单条 UPDATE，形状与 updateMany 完全一致（复用既有翻译，
/// 含 `__present` 哨兵维护与 MySQL 无 RETURNING 无关——CAS 不回读）。
/// matched 语义由宿主从 rowCount / `changes()` 读出，core 不产出。
pub(super) fn translate_cas(
    backend: Backend,
    schema: &Schema,
    cmd: &Value,
) -> Result<Vec<SqlStmt>, String> {
    let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
    let update = cmd.get("update").cloned().unwrap_or(json!({}));
    translate_update_many(backend, schema, &filter, &update)
}

/// sort 数对数组 → ORDER BY 片段（含前导空格）。
///
/// claim 的领取顺序**必须确定性下推**（跨 worker 各领各的行），与读路径不同：
/// 字段无法映射为可下推排序列时直接 Err（绝不降级为"未下推排序"——降级会产生
/// 重复领取窗口）。object/array 整值列禁止排序；点号路径走 JSON 标量提取表达式。
fn order_by_of(backend: Backend, schema: &Schema, sort: &Value) -> Result<String, String> {
    let pairs = sort
        .as_array()
        .ok_or_else(|| "claim 的 sort 必须为 [[field, dir], ...] 数组".to_string())?;
    if pairs.is_empty() {
        return Err("claim 的 sort 不能为空（领取顺序必须确定性，禁降级）".to_string());
    }
    let mut parts: Vec<String> = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let arr = pair
            .as_array()
            .filter(|a| a.len() == 2)
            .ok_or_else(|| "claim 的 sort 项必须为 [field, dir] 二元组".to_string())?;
        let field = arr[0]
            .as_str()
            .ok_or_else(|| "claim 的 sort 字段名必须为字符串".to_string())?;
        let dir = if arr[1].as_i64().unwrap_or(1) >= 0 {
            "ASC"
        } else {
            "DESC"
        };
        match field_column_ref(schema, field) {
            Some(ColumnRef::Scalar(c)) => {
                parts.push(format!("t.{} {}", backend.pcol(&c), dir));
            }
            Some(ColumnRef::JsonPath(col, path)) => {
                let base = format!("t.{}", backend.pcol(&col));
                let segs: Vec<&str> = path.iter().map(String::as_str).collect();
                let expr = backend.json_extract_scalar(&base, &segs)?;
                parts.push(format!("{} {}", expr, dir));
            }
            _ => {
                return Err(format!(
                    "claim 的 sort 字段 \"{field}\" 无法映射为可下推排序列（object/array 整值排序禁止；claim 排序必须确定性，禁降级）"
                ));
            }
        }
    }
    Ok(format!(" ORDER BY {}", parts.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Registry;

    fn registry() -> Registry {
        let mut r = Registry::new();
        r.register(&json!({
            "name": "Outbox", "collection": "outbox", "timestamps": false,
            "fields": {
                "status": { "type": "string" },
                "lockedBy": { "type": "string" },
                "lockedAt": { "type": "int" },
                "payload": { "type": "object" }
            },
            "relations": {}
        }))
        .expect("schema 注册失败");
        r
    }

    fn claim_cmd() -> Value {
        json!({
            "kind": "claim", "collection": "outbox",
            "filter": { "status": "pending" },
            "sort": [["_id", 1]],
            "limit": 10,
            "update": { "$set": { "lockedBy": "w1", "lockedAt": 123 } },
            "nowStamp": 123
        })
    }

    fn cas_cmd() -> Value {
        json!({
            "kind": "compareAndSet", "collection": "outbox",
            "filter": { "_id": "l1", "lockedBy": "w1" },
            "update": { "$set": { "lockedAt": 456 } },
            "nowStamp": 456
        })
    }

    fn stmts_of(backend: Backend, cmd: &Value) -> Vec<Value> {
        let r = registry();
        let out = crate::dialect::translate(backend, cmd, &r).expect("translate");
        out["stmts"]
            .as_array()
            .expect("stmts 数组")
            .clone()
    }

    // ── A1 快照：MySQL 两步（SELECT FOR UPDATE SKIP LOCKED + UPDATE IN token） ──

    #[test]
    fn claim_mysql_two_step_snapshot() {
        let stmts = stmts_of(Backend::Mysql, &claim_cmd());
        assert_eq!(stmts.len(), 2, "MySQL 应产出两步 SQL");
        let s1 = stmts[0]["text"].as_str().unwrap();
        assert!(s1.contains("FOR UPDATE SKIP LOCKED"), "stmt1 须行锁: {s1}");
        assert!(s1.contains("ORDER BY t.`_id` ASC"), "缺省排序 FIFO: {s1}");
        assert!(s1.ends_with("LIMIT ? FOR UPDATE SKIP LOCKED"), "{s1}");
        assert_eq!(stmts[0]["isWrite"], json!(false));
        assert_eq!(stmts[0]["params"], json!(["pending", 10]));
        let s2 = stmts[1]["text"].as_str().unwrap();
        assert!(s2.starts_with("UPDATE `outbox` AS t SET"), "{s2}");
        assert!(s2.contains("t.`_id` IN (___claimed_ids___)"), "token 占位: {s2}");
        assert_eq!(stmts[1]["isWrite"], json!(true));
        // SET 列按物理名字典序（locked_at < locked_by）
        assert_eq!(stmts[1]["params"], json!([123, "w1"]));
    }

    #[test]
    fn claim_pg_two_step_snapshot() {
        let stmts = stmts_of(Backend::Postgres, &claim_cmd());
        assert_eq!(stmts.len(), 2);
        let s1 = stmts[0]["text"].as_str().unwrap();
        assert!(s1.contains("FOR UPDATE SKIP LOCKED"), "{s1}");
        // PG：$n 占位（WHERE $1 + LIMIT $2）
        assert!(s1.contains("LIMIT $2 FOR UPDATE SKIP LOCKED"), "{s1}");
        let s2 = stmts[1]["text"].as_str().unwrap();
        assert!(s2.contains("IN (___claimed_ids___)"), "{s2}");
        assert_eq!(stmts[1]["params"], json!([123, "w1"]));
    }

    #[test]
    fn claim_sqlite_subquery_snapshot() {
        let stmts = stmts_of(Backend::Sqlite, &claim_cmd());
        assert_eq!(stmts.len(), 2);
        let s1 = stmts[0]["text"].as_str().unwrap();
        assert!(s1.starts_with("UPDATE"), "SQLite stmt1 先领取: {s1}");
        assert!(
            s1.contains("WHERE t.\"_id\" IN (SELECT t.\"_id\" FROM \"outbox\" t WHERE"),
            "子查询领取: {s1}"
        );
        assert!(s1.contains("ORDER BY t.\"_id\" ASC LIMIT ?"), "{s1}");
        // params = SET 值（物理名字典序） + filter + limit
        assert_eq!(stmts[0]["params"], json!([123, "w1", "pending", 10]));
        let s2 = stmts[1]["text"].as_str().unwrap();
        assert!(s2.starts_with("SELECT"), "stmt2 取回被领行: {s2}");
        assert!(
            s2.contains("WHERE t.\"_id\" IN (SELECT t.\"_id\" FROM \"outbox\" t WHERE"),
            "同一子查询复用: {s2}"
        );
        assert_eq!(stmts[1]["params"], json!(["pending", 10]));
        // 取回行走既有行还原形状（含 __present 哨兵）
        assert_eq!(stmts[1]["rowShape"]["present"], json!("__present"));
    }

    // ── A1 快照：CAS 三后端单条 UPDATE（形状与 updateMany 一致） ──

    #[test]
    fn cas_three_backends_snapshot() {
        for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
            let stmts = stmts_of(backend, &cas_cmd());
            assert_eq!(stmts.len(), 1, "{backend:?} CAS 单条 UPDATE");
            let text = stmts[0]["text"].as_str().unwrap();
            assert!(
                text.starts_with("UPDATE ") && text.contains(" AS t SET "),
                "{backend:?}: {text}"
            );
            assert!(text.contains("WHERE"), "{backend:?}: {text}");
            assert_eq!(stmts[0]["isWrite"], json!(true));
        }
    }

    // ── 错误面：禁降级 / 禁静默 ──

    #[test]
    fn claim_rejects_unmappable_sort_field() {
        let mut cmd = claim_cmd();
        cmd["sort"] = json!([["payload", 1]]); // object 整值列禁止排序
        let r = registry();
        let err = crate::dialect::translate(Backend::Mysql, &cmd, &r).unwrap_err();
        assert!(err.contains("无法映射为可下推排序列"), "{err}");
    }

    #[test]
    fn claim_rejects_nonpositive_limit() {
        let mut cmd = claim_cmd();
        cmd["limit"] = json!(0);
        let r = registry();
        let err = crate::dialect::translate(Backend::Mysql, &cmd, &r).unwrap_err();
        assert!(err.contains("limit 必须 > 0"), "{err}");
    }

    #[test]
    fn claim_rejects_raw_operator_in_update() {
        let mut cmd = claim_cmd();
        cmd["update"] = json!({ "$push": { "tags": "x" } });
        let r = registry();
        let err = crate::dialect::translate(Backend::Mysql, &cmd, &r).unwrap_err();
        assert!(err.contains("不支持的操作符"), "{err}");
    }

    #[test]
    fn unknown_kind_returns_err_not_panic() {
        let r = registry();
        let err = crate::dialect::translate(
            Backend::Mysql,
            &json!({ "kind": "claimX", "collection": "outbox" }),
            &r,
        )
        .unwrap_err();
        assert!(err.contains("未支持的命令 kind"), "{err}");
    }
}
