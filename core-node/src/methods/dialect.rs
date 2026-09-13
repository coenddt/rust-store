//! dialect：Mongo 命令 → 关系型 SQL 的翻译与结果还原方法。

use napi::Result;
use napi_derive::napi;
use serde_json::Value;

use rust_store_core::dialect::{
    introspect_to_schema_json as core_introspect_to_schema_json, merge_schema as core_merge_schema,
    restore_rows_json as core_restore_rows_json, translate as core_dialect_translate, Backend,
};

use crate::convert::err;
use crate::Registry;

#[napi]
impl Registry {
    // ─── dialect：Mongo 命令 → 关系型 SQL ─────────────

    /// 把一条 Mongo 命令 JSON 翻译为指定后端的 SQL 语句序列（见 `translate::translate`）。
    #[napi]
    pub fn dialect_translate(&self, backend: String, cmd: Value) -> Result<Value> {
        let backend = Backend::parse(&backend).map_err(err)?;
        core_dialect_translate(backend, &cmd, &self.core).map_err(err)
    }

    /// 把 `{rowShape, rows}` 还原为嵌套 Mongo 文档数组（平铺 JOIN 行 → 文档）
    #[napi]
    pub fn restore_rows(&self, shape: Value, rows: Value) -> Result<Value> {
        core_restore_rows_json(&shape, &rows).map_err(err)
    }

    /// 把 introspection 行 JSON → schemaJSON（`{rows}` 内含 tables/columns/fks/indexes）
    #[napi]
    pub fn schema_from_rows(&self, rows: Value, backend: String) -> Result<Value> {
        let backend = Backend::parse(&backend).map_err(err)?;
        core_introspect_to_schema_json(&rows, &backend).map_err(err)
    }

    /// 合并 introspected 基础 schema 与本地 overlay（权限 / 计算列 / 覆盖）
    #[napi]
    pub fn merge_schema(&self, base: Value, overlay: Value) -> Result<Value> {
        core_merge_schema(&base, &overlay).map_err(err)
    }
}
