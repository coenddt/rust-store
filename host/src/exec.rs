//! 命令执行层：Mongo 风格命令 JSON → dialect 翻译 → sqlx 执行 → 行还原。
//!
//! 铁律（对齐 datasource.js:459-470 与 no-error-masking）：
//! `unsupported` 非空时绝不执行残缺 SQL —— 显式报错，禁静默降级。
//!
//! 三后端（SQLite/MySQL/PG）共用同一执行骨架，差异收敛在两处：
//! 参数绑定（各驱动 Arguments 类型）与行解码（按候选类型链逐一尝试，全败显式报错）。
//! PG 特有：params 含显式 null 时把对应 `$n` 内联为 `NULL` 字面量并重排剩余序号 ——
//! sqlx 强类型绑定会给 null 附带具体类型 OID，落入类型不匹配列时 PG 拒绝
//! （asyncpg / node-pg 走未类型化参数无此问题）；字面量 NULL 由 PG 按列类型推导，语义等价。
//! 前提契约：core 参数化保证 `$` 不会出现在字符串字面量内，占位符扫描安全。

use std::sync::RwLock;

use rust_store_core::dialect::{restore_rows_json, translate, Backend};
use rust_store_core::schema::Registry;
use serde_json::{json, Map, Value};
use sqlx::mysql::{MySqlArguments, MySqlConnection, MySqlRow};
use sqlx::postgres::{PgArguments, PgConnection, PgRow};
use sqlx::sqlite::{SqliteArguments, SqliteConnection, SqliteRow};
use sqlx::{Column, MySqlPool, PgPool, Row, SqlitePool, TypeInfo, ValueRef};

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

/// 连接池（三后端）
pub enum Pool {
    Sqlite(SqlitePool),
    Mysql(MySqlPool),
    Postgres(PgPool),
}

/// 从池借出的裸连接（Box 包装：PoolConnection 尺寸大，枚举变体不悬殊）
pub enum PoolConn {
    Sqlite(Box<sqlx::pool::PoolConnection<sqlx::Sqlite>>),
    Mysql(Box<sqlx::pool::PoolConnection<sqlx::MySql>>),
    Postgres(Box<sqlx::pool::PoolConnection<sqlx::Postgres>>),
}

impl Pool {
    pub async fn acquire(&self) -> Result<PoolConn, String> {
        Ok(match self {
            Pool::Sqlite(p) => {
                PoolConn::Sqlite(Box::new(p.acquire().await.map_err(|e| e.to_string())?))
            }
            Pool::Mysql(p) => {
                PoolConn::Mysql(Box::new(p.acquire().await.map_err(|e| e.to_string())?))
            }
            Pool::Postgres(p) => {
                PoolConn::Postgres(Box::new(p.acquire().await.map_err(|e| e.to_string())?))
            }
        })
    }
}

/// 可执行连接（借用视图）
pub enum Conn<'a> {
    Sqlite(&'a mut SqliteConnection),
    Mysql(&'a mut MySqlConnection),
    Postgres(&'a mut PgConnection),
}

impl PoolConn {
    pub fn conn(&mut self) -> Conn<'_> {
        match self {
            PoolConn::Sqlite(c) => Conn::Sqlite(c),
            PoolConn::Mysql(c) => Conn::Mysql(c),
            PoolConn::Postgres(c) => Conn::Postgres(c),
        }
    }
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
        let unsupported = out
            .get("unsupported")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
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
                text: s
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                params: s
                    .get("params")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
                is_write: s.get("isWrite").and_then(|v| v.as_bool()).unwrap_or(false),
                row_shape: s.get("rowShape").cloned().unwrap_or(Value::Null),
                returning: s
                    .get("returning")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
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
    conn: Conn<'_>,
    translated: &Translated,
) -> Result<ExecOutcome, String> {
    match conn {
        Conn::Sqlite(c) => exec_sqlite(c, translated).await,
        Conn::Mysql(c) => exec_mysql(c, translated).await,
        Conn::Postgres(c) => {
            // PG：显式 null 占位符内联为字面量（原因见模块注释）
            let stmts = translated
                .stmts
                .iter()
                .map(inline_pg_nulls)
                .collect::<Result<Vec<_>, String>>()?;
            exec_pg(c, &Translated { stmts }).await
        }
    }
}

/// PG 专用：params 含显式 null 时，把对应 `$n` 占位符内联为 `NULL` 字面量并重排剩余序号。
/// 无 null 的语句原样返回。
fn inline_pg_nulls(stmt: &SqlStmtJson) -> Result<SqlStmtJson, String> {
    if !stmt.params.iter().any(|v| v.is_null()) {
        return Ok(SqlStmtJson {
            text: stmt.text.clone(),
            params: stmt.params.clone(),
            is_write: stmt.is_write,
            row_shape: stmt.row_shape.clone(),
            returning: stmt.returning.clone(),
        });
    }
    let mut out = String::with_capacity(stmt.text.len());
    let mut new_params: Vec<Value> = Vec::with_capacity(stmt.params.len());
    let bytes = stmt.text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                let idx: usize = stmt.text[i + 1..j]
                    .parse()
                    .map_err(|e| format!("PG 占位符解析失败: {e}"))?;
                let v = stmt.params.get(idx - 1).ok_or_else(|| {
                    format!(
                        "PG 占位符 ${idx} 超出 params 范围（{} 个）",
                        stmt.params.len()
                    )
                })?;
                if v.is_null() {
                    out.push_str("NULL");
                } else {
                    new_params.push(v.clone());
                    out.push_str(&format!("${}", new_params.len()));
                }
                i = j;
                continue;
            }
        }
        // 非 ASCII 直接按字节落回（SQL 文本为 ASCII 标识符 + 引号，中文字符不会出现在 dialect 产出中）
        out.push(bytes[i] as char);
        i += 1;
    }
    Ok(SqlStmtJson {
        text: out,
        params: new_params,
        is_write: stmt.is_write,
        row_shape: stmt.row_shape.clone(),
        returning: stmt.returning.clone(),
    })
}

/// 三后端共用执行骨架（结构一致，仅类型名不同；差异已收敛到 bind/decode 函数）
macro_rules! exec_backend {
    ($fn_name:ident, $conn_ty:ty, $row_ty:ty, $bind:ident, $decode:ident) => {
        async fn $fn_name(
            conn: &mut $conn_ty,
            translated: &Translated,
        ) -> Result<ExecOutcome, String> {
            let mut docs: Vec<Value> = Vec::new();
            let mut changes: u64 = 0;
            for stmt in &translated.stmts {
                let mut q = sqlx::query(&stmt.text);
                for p in &stmt.params {
                    q = $bind(q, p);
                }
                if stmt.is_write && stmt.row_shape.is_null() && stmt.returning.is_empty() {
                    let r = q
                        .execute(&mut *conn)
                        .await
                        .map_err(|e| format!("SQL 执行失败: {e}"))?;
                    changes += r.rows_affected();
                    docs.clear();
                } else {
                    let rows: Vec<$row_ty> = q
                        .fetch_all(&mut *conn)
                        .await
                        .map_err(|e| format!("SQL 执行失败: {e}"))?;
                    docs = restore_docs(&stmt.row_shape, &rows, $decode)?;
                    changes += docs.len() as u64;
                }
            }
            Ok(ExecOutcome { docs, changes })
        }
    };
}

exec_backend!(
    exec_sqlite,
    SqliteConnection,
    SqliteRow,
    bind_sqlite,
    decode_sqlite
);
exec_backend!(
    exec_mysql,
    MySqlConnection,
    MySqlRow,
    bind_mysql,
    decode_mysql
);
exec_backend!(exec_pg, PgConnection, PgRow, bind_pg, decode_pg);

/// 取数语句的行处理：有 rowShape → core 还原嵌套文档；否则直接取列
fn restore_docs<R: sqlx::Row>(
    row_shape: &Value,
    rows: &[R],
    decode: impl Fn(&R) -> Result<Value, String>,
) -> Result<Vec<Value>, String> {
    let values: Vec<Value> = rows.iter().map(decode).collect::<Result<_, String>>()?;
    if !row_shape.is_null() {
        let restored = restore_rows_json(row_shape, &json!(values))?;
        Ok(restored
            .as_array()
            .cloned()
            .ok_or_else(|| "restore_rows_json 未返回数组".to_string())?)
    } else {
        Ok(values)
    }
}

macro_rules! bind_impl {
    ($fn_name:ident, $db:ty, $args:ty) => {
        fn $fn_name<'q>(
            q: sqlx::query::Query<'q, $db, $args>,
            v: &'q Value,
        ) -> sqlx::query::Query<'q, $db, $args> {
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
                // Binder 产出的 params 契约上全是标量；对象/数组属上游失守，序列化落文本列
                other => q.bind(serde_json::to_string(other).unwrap_or_default()),
            }
        }
    };
}

bind_impl!(bind_sqlite, sqlx::Sqlite, SqliteArguments<'q>);
bind_impl!(bind_mysql, sqlx::MySql, MySqlArguments);
bind_impl!(bind_pg, sqlx::Postgres, PgArguments);

/// 提取文档数组的 `_id` 列表（两阶段 / preCommand 共用）
pub fn extract_ids(docs: &[Value]) -> Vec<Value> {
    docs.iter()
        .map(|d| d.get("_id").cloned().unwrap_or(Value::Null))
        .collect()
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

/// SQLite 行解码（动态类型：按 sqlite 存储类型取值）
fn decode_sqlite(row: &SqliteRow) -> Result<Value, String> {
    let mut map = Map::new();
    for (i, col) in row.columns().iter().enumerate() {
        let name = col.name();
        let ti = row
            .try_get_raw(i)
            .map_err(|e| format!("读取列 {name} 失败: {e}"))?
            .type_info()
            .name()
            .to_string();
        let v = match ti.as_str() {
            "NULL" => Value::Null,
            "INTEGER" => row
                .try_get::<i64, _>(i)
                .map(Value::from)
                .map_err(|e| e.to_string())?,
            "REAL" => row
                .try_get::<f64, _>(i)
                .map(Value::from)
                .map_err(|e| e.to_string())?,
            "TEXT" => row
                .try_get::<String, _>(i)
                .map(Value::from)
                .map_err(|e| e.to_string())?,
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

/// MySQL / PG 行解码：按候选类型链逐一尝试（i64 → f64 → bool → String → bytes），
/// 全部失败显式报错（禁静默吞列）。NULL 先探先返。
macro_rules! decode_impl {
    ($fn_name:ident, $row_ty:ty, $chain:ident, $label:expr) => {
        fn $fn_name(row: &$row_ty) -> Result<Value, String> {
            let mut map = Map::new();
            for (i, col) in row.columns().iter().enumerate() {
                let name = col.name();
                let is_null = row.try_get_raw(i).map(|v| v.is_null()).unwrap_or(false);
                let v = if is_null {
                    Value::Null
                } else {
                    $chain(row, i).ok_or_else(|| format!($label, name))?
                };
                map.insert(name.to_string(), v);
            }
            Ok(Value::Object(map))
        }
    };
}

decode_impl!(
    decode_mysql,
    MySqlRow,
    chain_mysql,
    "MySQL 列 {0} 解码失败：全部候选类型不匹配"
);
decode_impl!(
    decode_pg,
    PgRow,
    chain_pg,
    "PostgreSQL 列 {0} 解码失败：全部候选类型不匹配"
);

// 候选类型链必须按后端宏生成：sqlx 的 Type/Decode 逐库实现，
// 泛型 <R: Row> 上 try_get::<i64> 无对应 trait 约束
macro_rules! chain_impl {
    ($fn_name:ident, $row_ty:ty) => {
        fn $fn_name(row: &$row_ty, i: usize) -> Option<Value> {
            if let Ok(v) = row.try_get::<i64, _>(i) {
                return Some(Value::from(v));
            }
            if let Ok(v) = row.try_get::<f64, _>(i) {
                return Some(Value::from(v));
            }
            if let Ok(v) = row.try_get::<bool, _>(i) {
                return Some(Value::from(v));
            }
            if let Ok(v) = row.try_get::<String, _>(i) {
                return Some(Value::from(v));
            }
            row.try_get::<Vec<u8>, _>(i)
                .ok()
                .map(|b| Value::String(String::from_utf8_lossy(&b).into_owned()))
        }
    };
}

chain_impl!(chain_mysql, MySqlRow);
chain_impl!(chain_pg, PgRow);
