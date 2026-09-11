//! datasource：多后端路由的解析方法（纯逻辑转发，连接句柄留在 Host）。

use pyo3::prelude::*;
use serde_json::Value;

use crate::convert::{err, py_to_json};
use crate::Registry;

#[pymethods]
impl Registry {
    /// 解析 schema 绑定的数据源 kind（`"mongo"` / `"mysql"` / `"postgres"` / `"sqlite"`）
    ///
    /// `config` 形状：`{ "sources": { "<name>": "<kind>" } }`；`None` → 单源 Mongo。
    #[pyo3(signature = (schema_name, config=None))]
    fn resolve_datasource(
        &self,
        schema_name: &str,
        config: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<String> {
        let config = match config {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let ds = self
            .core
            .resolve_datasource(schema_name, &config)
            .map_err(err)?;
        Ok(ds.as_str().to_string())
    }

    /// schema 声明的数据源名（未声明 → `None`，语义为 `default`）
    fn schema_datasource(&self, schema_name: &str) -> PyResult<Option<String>> {
        self.core.schema_datasource(schema_name).map_err(err)
    }
}
