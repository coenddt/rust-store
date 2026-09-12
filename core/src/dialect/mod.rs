//! Mongo 方言 → 关系型 SQL 翻译层（纯逻辑，无 IO）
//!
//! 背景：core 按约定继续产出 **Mongo 命令 JSON**（`find`/`aggregate`/`countDocuments`/…）。
//! 本模块提供一条**纯函数**翻译通道，把这些命令落成 MySQL / PostgreSQL / SQLite 各后端的
//! 参数化 SQL 语句（[`ir::SqlStmt`]），并负责把驱动返回的**平铺 JOIN 行**还原为嵌套 Mongo
//! 文档（[`row::restore_rows`]）。
//!
//! 设计边界（对齐「用 MongoDB 方言，其他数据库适配」的要求）：
//! 1. [`translate::translate`] 只做 JSON → SQL 中间表示，完全不触数据库；Host 拿到
//!    `SqlStmt` 后自行绑定驱动。
//! 2. 关系模型采用**严格关系范式**：标量字段落在单表，`object`/`array` 展平为附属表，
//!    关系用 SQL JOIN 解析。
//! 3. 无法安全翻译的组合输出 `_unsupported` 标志 + warning，**绝不生成错误 SQL**。
//! 4. 平铺行 → 嵌套文档的还原与 introspection 行 → schemaJSON 的映射都是纯逻辑，四侧对拍可复现。

pub mod filter;
pub mod introspect;
pub mod ir;
pub mod overlay;
pub mod row;
pub mod select;
pub mod translate;
pub mod write;

// 保持对外路径稳定：`crate::dialect::*` 直接可用（函数与其同名模块共存）
pub use introspect::{introspect_to_schema_json, schema_def_from_rows};
pub use overlay::merge_schema;
pub use row::restore_rows_json;
pub use translate::translate;

/// 支持的数据库后端
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Mysql,
    Postgres,
    Sqlite,
}

impl Backend {
    /// 从字符串解析后端（未知 → Err）
    pub fn parse(s: &str) -> Result<Backend, String> {
        match s.to_lowercase().as_str() {
            "mysql" => Ok(Backend::Mysql),
            "postgres" | "pg" | "postgresql" => Ok(Backend::Postgres),
            "sqlite" => Ok(Backend::Sqlite),
            _ => Err(format!("不支持的后端: {}", s)),
        }
    }

    /// 双重引号标识符
    pub fn quote_ident(&self, ident: &str) -> String {
        match self {
            Backend::Mysql => format!("`{}`", ident.replace('`', "``")),
            _ => format!("\"{}\"", ident.replace('"', "\"\"")),
        }
    }

    /// 表名 → SQL：namespace 可选限定（`Some(ns)` → `"ns"."table"`；`None` → `"table"`）
    ///
    /// namespace 与连接的 search_path / 连接库 / ATTACH 库对应（见
    /// `multi-datasource-routing-plan.md` §二）；空串按 `None` 处理。
    pub fn qualified_table(&self, namespace: Option<&str>, table: &str) -> String {
        match namespace.filter(|s| !s.is_empty()) {
            Some(ns) => format!("{}.{}", self.quote_ident(ns), self.quote_ident(table)),
            None => self.quote_ident(table),
        }
    }

    /// 占位符：SQLite/MySQL 用 `?`，PostgreSQL 用 `$n`（调用方保证按顺序传入 index）
    pub fn placeholder(&self, _index: usize) -> String {
        match self {
            Backend::Postgres => format!("${}", _index + 1),
            _ => "?".to_string(),
        }
    }

    /// 后端标识（同 `Debug`，供绑定层序列化）
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Mysql => "mysql",
            Backend::Postgres => "postgres",
            Backend::Sqlite => "sqlite",
        }
    }

    /// MySQL 无 RETURNING —— 需由 Host 编排 UPDATE + find 两段；PG/SQLite 原生支持
    pub fn supports_returning(&self) -> bool {
        !matches!(self, Backend::Mysql)
    }
}