//! datasource：多后端路由的解析方法（纯逻辑转发，连接句柄留在 Host）。

use napi::Result;
use napi_derive::napi;
use serde_json::Value;

use crate::convert::err;
use crate::Registry;

#[napi]
impl Registry {
    /// 解析 schema 绑定的数据源 kind（`"mongo"` / `"mysql"` / `"postgres"` / `"sqlite"`）
    ///
    /// `config` 形状：`{ "sources": { "<name>": "<kind>" } }`；`null` → 单源 Mongo。
    #[napi]
    pub fn resolve_datasource(&self, schema_name: String, config: Option<Value>) -> Result<String> {
        let config = config.unwrap_or(Value::Null);
        let ds = self
            .core
            .resolve_datasource(&schema_name, &config)
            .map_err(err)?;
        Ok(ds.as_str().to_string())
    }

    /// schema 声明的数据源名（未声明 → `null`，语义为 `default`）
    #[napi]
    pub fn schema_datasource(&self, schema_name: String) -> Result<Option<String>> {
        self.core.schema_datasource(&schema_name).map_err(err)
    }
}
