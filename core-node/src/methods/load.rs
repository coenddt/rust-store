//! load：目录语义装载的绑定透出（纯逻辑转发；IO 留在宿主 / 脚手架）。
//!
//! 透出 core [`plan_load`](rust_store_core::schema::plan_load) 与带定位批量注册
//! [`Registry::register_batch`](rust_store_core::Registry::register_batch)（D13 批次唯一），
//! 回填 01 分步「待核实 1」（绑定缺带定位注册）。

use napi::Result;
use napi_derive::napi;
use serde_json::{json, Value};

use rust_store_core::datasource::DEFAULT_SOURCE;
use rust_store_core::schema::{plan_load as core_plan_load, LoadConfig, Location};

use crate::convert::err;
use crate::Registry;

/// `{source?, database?, schema?}` → [`Location`]（缺省 source = `default`；空串视为 None）
fn location_from_value(v: &Value) -> Result<Location> {
    let o = v
        .as_object()
        .ok_or_else(|| napi::Error::from_reason("location 必须是对象"))?;
    let pick = |k: &str| {
        o.get(k)
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    Ok(Location {
        source: pick("source").unwrap_or_else(|| DEFAULT_SOURCE.to_string()),
        database: pick("database"),
        schema: pick("schema"),
    })
}

/// [`Location`] → `{source, database, schema}`（跨语言稳定投影）
fn location_to_value(loc: &Location) -> Value {
    json!({ "source": loc.source, "database": loc.database, "schema": loc.schema })
}

#[napi]
impl Registry {
    /// 目录语义纯规划：`store.config.json` + 目录扫描结果 → 定位后的装载项。
    ///
    /// `config` = `{sources, defs}`；`files` = `[{rel, defn}]`（`rel` = 相对 defs-root 的路径）。
    /// 返回 `[{defn, location:{source,database,schema}}]`（主在前、其后从；组间按 name 字典序）。
    #[napi]
    pub fn plan_load(&self, config: Value, files: Value) -> Result<Value> {
        let cfg = LoadConfig::from_json(&config).map_err(err)?;
        let arr = files
            .as_array()
            .ok_or_else(|| napi::Error::from_reason("files 必须是数组"))?;
        let mut pairs: Vec<(String, Value)> = Vec::with_capacity(arr.len());
        for f in arr {
            let o = f
                .as_object()
                .ok_or_else(|| napi::Error::from_reason("files 项必须是 {rel, defn} 对象"))?;
            let rel = o
                .get("rel")
                .and_then(|v| v.as_str())
                .ok_or_else(|| napi::Error::from_reason("files 项缺 rel"))?
                .to_string();
            let defn = o
                .get("defn")
                .cloned()
                .ok_or_else(|| napi::Error::from_reason("files 项缺 defn"))?;
            pairs.push((rel, defn));
        }
        let items = core_plan_load(&cfg, &pairs).map_err(err)?;
        let out: Vec<Value> = items
            .iter()
            .map(|(defn, loc)| json!({ "defn": defn, "location": location_to_value(loc) }))
            .collect();
        Ok(Value::Array(out))
    }

    /// 带定位批量注册（D13 批次唯一）；`items` = `[{defn, location}]`，`ctx` 缺省 = 无上下文。
    #[napi]
    pub fn register_batch(&mut self, items: Value, ctx: Option<Value>) -> Result<()> {
        let arr = items
            .as_array()
            .ok_or_else(|| napi::Error::from_reason("items 必须是数组"))?;
        let mut pairs: Vec<(Value, Location)> = Vec::with_capacity(arr.len());
        for it in arr {
            let o = it
                .as_object()
                .ok_or_else(|| napi::Error::from_reason("items 项必须是 {defn, location} 对象"))?;
            let defn = o
                .get("defn")
                .cloned()
                .ok_or_else(|| napi::Error::from_reason("items 项缺 defn"))?;
            let loc_v = o
                .get("location")
                .ok_or_else(|| napi::Error::from_reason("items 项缺 location"))?;
            pairs.push((defn, location_from_value(loc_v)?));
        }
        let context = ctx
            .as_ref()
            .and_then(rust_store_core::permission::context_from_value);
        self.core
            .register_batch(&pairs, context.as_ref())
            .map_err(err)
    }
}
