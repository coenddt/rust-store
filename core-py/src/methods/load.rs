//! load：目录语义装载的绑定透出（纯逻辑转发；IO 留在宿主 / 脚手架）。
//!
//! 与 `core-node/src/methods/load.rs` 同构：透出 core `plan_load` 与带定位批量注册
//! `register_batch`（D13 批次唯一），回填 01 分步「待核实 1」。

use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use serde_json::{json, Value};

use rust_store_core::datasource::DEFAULT_SOURCE;
use rust_store_core::schema::{plan_load as core_plan_load, LoadConfig, Location};

use crate::convert::{ctx_from, err, py_to_json, to_py};
use crate::Registry;

/// `{source?, database?, schema?}` → [`Location`]（缺省 source = `default`；空串视为 None）
fn location_from_value(v: &Value) -> PyResult<Location> {
    let o = v
        .as_object()
        .ok_or_else(|| PyTypeError::new_err("location 必须是对象"))?;
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

#[pymethods]
impl Registry {
    /// 目录语义纯规划：`store.config.json` + 目录扫描结果 → 定位后的装载项。
    ///
    /// `config` = `{sources, defs}`；`files` = `[{rel, defn}]`（`rel` = 相对 defs-root 的路径）。
    /// 返回 `[{defn, location:{source,database,schema}}]`（主在前、其后从；组间按 name 字典序）。
    fn plan_load(
        &self,
        py: Python<'_>,
        config: &Bound<'_, PyAny>,
        files: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let cfg = LoadConfig::from_json(&py_to_json(config)?).map_err(err)?;
        let files_json = py_to_json(files)?;
        let arr = files_json
            .as_array()
            .ok_or_else(|| PyTypeError::new_err("files 必须是数组"))?;
        let mut pairs: Vec<(String, Value)> = Vec::with_capacity(arr.len());
        for f in arr {
            let o = f
                .as_object()
                .ok_or_else(|| PyTypeError::new_err("files 项必须是 {rel, defn} 对象"))?;
            let rel = o
                .get("rel")
                .and_then(|v| v.as_str())
                .ok_or_else(|| PyTypeError::new_err("files 项缺 rel"))?
                .to_string();
            let defn = o
                .get("defn")
                .cloned()
                .ok_or_else(|| PyTypeError::new_err("files 项缺 defn"))?;
            pairs.push((rel, defn));
        }
        let items = core_plan_load(&cfg, &pairs).map_err(err)?;
        let out: Vec<Value> = items
            .iter()
            .map(|(defn, loc)| json!({ "defn": defn, "location": location_to_value(loc) }))
            .collect();
        to_py(py, Value::Array(out))
    }

    /// 带定位批量注册（D13 批次唯一）；`items` = `[{defn, location}]`，`ctx=None` = 无上下文。
    fn register_batch(&mut self, items: &Bound<'_, PyAny>, ctx: &Bound<'_, PyAny>) -> PyResult<()> {
        let items_json = py_to_json(items)?;
        let arr = items_json
            .as_array()
            .ok_or_else(|| PyTypeError::new_err("items 必须是数组"))?;
        let mut pairs: Vec<(Value, Location)> = Vec::with_capacity(arr.len());
        for it in arr {
            let o = it
                .as_object()
                .ok_or_else(|| PyTypeError::new_err("items 项必须是 {defn, location} 对象"))?;
            let defn = o
                .get("defn")
                .cloned()
                .ok_or_else(|| PyTypeError::new_err("items 项缺 defn"))?;
            let loc_v = o
                .get("location")
                .ok_or_else(|| PyTypeError::new_err("items 项缺 location"))?;
            pairs.push((defn, location_from_value(loc_v)?));
        }
        let context = ctx_from(Some(ctx))?;
        self.core
            .register_batch(&pairs, context.as_ref())
            .map_err(err)
    }
}
