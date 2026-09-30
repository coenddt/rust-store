//! 命令执行层：Mongo 风格命令 JSON → dialect 翻译 → sqlx 执行 → 行还原。
//!
//! 铁律（对齐 datasource.js:459-470 与 no-error-masking）：
//! `unsupported` 非空时绝不执行残缺 SQL —— 显式报错，禁静默降级。

use std::sync::RwLock;


use rust_store_core::dialect::{restore_rows_json, translate, Backend};
use rust_store_core::schema::Registry;
use serde_json::{json, Map, Value};
use sqlx::sqlite::SqliteRow;
use sqlx::{Column, Row, SqliteConnection, TypeInfo, ValueRef};

/// 关系谓词归一占位（core mutate/mod.rs:61 的 json! 字面量，非 pub const，宿主按字面量对齐）
pub const REL_PRED_IDS: &str = "__REL_PRED_IDS__";

/// 持读锁完成一次同步 core 调用（plan / translate / finalize 都是纯函数）
pub fn with_registry<T>(
    registry: &RwLock<Registry>,
    f: impl FnOnce(&Registry) -> Result<T, String>,
) -> Result<T, String> {
    let reg = registry
        .read()
        .map_err(|_| "registry 读锁中毒（注册线程 panic）".to_string())?;
    f(&reg)
}

/// translate 结果（取自 `translate` 返回的 JSON）
pub struct Translated {
    pub stmts: Vec<SqlStmtJson>,
}

pub struct SqlStmtJson {
    pub text: String,
    pub params: Vec<Value>,
    pub is_write: bool,
    pub row_shape: Value,
    pub returning: Vec<String>,
}

/// 翻译一条 Mongo 风格命令。`unsupported` 非空 → 显式报错，绝不执行。
pub fn translate_command(
    backend: Backend,
    cmd: &Value,
    registry: &RwLock<Registry>,
) -> Result<Translated, String> {
    with_registry(registry, |reg| {
        let out = translate(backend, cmd, reg)?;
        let unsupported = out.get("unsupported").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if !unsupported.is_empty() {
            return Err(format!(
                "命令包含无法安全下推的组合（unsupported = {}）：拒绝执行，请改写查询或换用 MongoDB 源",
                serde_json::to_string(&unsupported).unwrap_or_default()
            ));
        }
        let stmts = out
            .get("stmts")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "translate 结果缺少 stmts".to_string())?
            .iter()
            .map(|s| SqlStmtJson {
                text: s.get("text").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                params: s.get("params").and_then(|v| v.as_array()).cloned().unwrap_or_default(),
                is_write: s.get("isWrite").and_then(|v| v.as_bool()).unwrap_or(false),
                row_shape: s.get("rowShape").cloned().unwrap_or(Value::Null),
                returning: s
                    .get("returning")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
            })
            .collect();
        Ok(Translated { stmts })
    })
}

/// 执行结果：`docs` = 还原后的文档数组（SELECT / RETURNING 行）；`changes` = 写语句累计影响行数
pub struct ExecOutcome {
    pub docs: Vec<Value>,
    pub changes: u64,
}

/// 在给定连接上执行一组翻译后的语句。
/// 取数规则：最后一条产出行的语句为主结果（SELECT 带 rowShape 还原；写语句带 RETURNING 直取列）。
pub async fn exec_translated(
    conn: &mut SqliteConnection,
    translated: &Translated,
) -> Result<ExecOutcome, String> {
    let mut docs: Vec<Value> = Vec::new();
    let mut changes: u64 = 0;

    for stmt in &translated.stmts {
        let mut q = sqlx::query(&stmt.text);
        for p in &stmt.params {
            q = bind_value(q, p);
        }
        if stmt.is_write && stmt.row_shape.is_null() && stmt.returning.is_empty() {
            let r = q.execute(&mut *conn).await.map_err(|e| format!("SQL 执行失败: {e}"))?;
            changes += r.rows_affected();
            docs.clear();
        } else {
            let rows: Vec<SqliteRow> = q.fetch_all(&mut *conn).await.map_err(|e| format!("SQL 执行失败: {e}"))?;
            let values: Vec<Value> = rows.iter().map(row_to_value).collect::<Result<_, String>>()?;
            if !stmt.row_shape.is_null() {
                let restored = restore_rows_json(&stmt.row_shape, &json!(values))?;
                docs = restored
                    .as_array()
                    .cloned()
                    .ok_or_else(|| "restore_rows_json 未返回数组".to_string())?;
            } else {
                docs = values;
            }
            changes += docs.len() as u64;
        }
    }

    Ok(ExecOutcome { docs, changes })
}

/// 递归替换运行期占位符（@c0 类命名参数 core 规划期已消费，宿主只处理这两类字符串）：
/// `{{phase1.ids}}` → 两阶段第一阶段取回的 `_id` 数组；`__REL_PRED_IDS__` → 关系谓词命中 id 数组。
pub fn substitute_ids(cmd: &mut Value, placeholder: &str, ids: &[Value]) {
    match cmd {
        Value::String(s) if s == placeholder => {
            *cmd = json!(ids);
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                substitute_ids(v, placeholder, ids);
            }
        }
        Value::Object(map) => {
            for v in map.values_mut() {
                substitute_ids(v, placeholder, ids);
            }
        }
        _ => {}
    }
}

/// 提取文档数组的 `_id` 列表（两阶段 / preCommand 共用）
pub fn extract_ids(docs: &[Value]) -> Vec<Value> {
    docs.iter().map(|d| d.get("_id").cloned().unwrap_or(Value::Null)).collect()
}

fn bind_value<'q>(
    q: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    v: &'q Value,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    match v {
        Value::Null => q.bind(Option::<String>::None),
        Value::Bool(b) => q.bind(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                q.bind(i)
            } else {
                q.bind(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => q.bind(s.as_str()),
        // Binder 产出的 params 契约上全是标量；对象/数组属上游失守，序列化落 TEXT 并由调用方报错可见
        other => q.bind(serde_json::to_string(other).unwrap_or_default()),
    }
}

fn row_to_value(row: &SqliteRow) -> Result<Value, String> {
    let mut map = Map::new();
    for (i, col) in row.columns().iter().enumerate() {
        let name = col.name();
        let ti = row
            .try_get_raw(i)
            .map_err(|e| format!("读取列 {name} 失败: {e}"))?
            .type_info()
            .name()
            .to_string();        let v = match ti.as_str() {
            "NULL" => Value::Null,
            "INTEGER" => row.try_get::<i64, _>(i).map(Value::from).map_err(|e| e.to_string())?,
            "REAL" => row.try_get::<f64, _>(i).map(Value::from).map_err(|e| e.to_string())?,
            "TEXT" => row.try_get::<String, _>(i).map(Value::from).map_err(|e| e.to_string())?,
            "BLOB" => {
                let b: Vec<u8> = row.try_get(i).map_err(|e| e.to_string())?;
                Value::String(String::from_utf8_lossy(&b).into_owned())
            }
            other => return Err(format!("未支持的 SQLite 列类型 {other}（列 {name}）")),
        };
        map.insert(name.to_string(), v);
    }
    Ok(Value::Object(map))
}
