//! local：本地磁盘数据源纯求值转发（宿主提供集合快照 → 返回结果与变更后快照）。

use pyo3::prelude::*;
use serde_json::Value;

use crate::convert::{err, py_to_json, to_py};
use crate::Registry;

#[pymethods]
impl Registry {
    /// 本地磁盘数据源纯求值（宿主提供集合快照 → 返回 `{result, changed, collections}`）
    fn local_eval(
        &self,
        py: Python<'_>,
        collections: &Bound<'_, PyAny>,
        command: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let collections: Value = py_to_json(collections)?;
        let command: Value = py_to_json(command)?;
        let out = rust_store_core::local::eval_command(&collections, &command).map_err(err)?;
        to_py(py, out)
    }
}
