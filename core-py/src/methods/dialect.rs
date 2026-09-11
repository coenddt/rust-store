//! dialect：Mongo 命令 → 关系型 SQL 的方法。

use pyo3::prelude::*;
use serde_json::Value;

use rust_store_core::dialect::{
    introspect_to_schema_json as core_introspect_to_schema_json,
    merge_schema as core_merge_schema, restore_rows_json as core_restore_rows_json,
    translate as core_dialect_translate, Backend,
};

use crate::convert::{err, py_to_json, to_py};
use crate::Registry;

#[pymethods]
impl Registry {
    #[pyo3(signature = (backend, cmd))]
    fn dialect_translate(
        &self,
        py: Python<'_>,
        backend: String,
        cmd: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let backend = Backend::parse(&backend).map_err(err)?;
        let cmd = match cmd {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        to_py(py, core_dialect_translate(backend, &cmd, &self.core).map_err(err)?)
    }

    #[pyo3(signature = (shape, rows))]
    fn restore_rows(
        &self,
        py: Python<'_>,
        shape: Option<&Bound<'_, PyAny>>,
        rows: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let shape = match shape {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let rows = match rows {
            Some(v) => py_to_json(v)?,
            None => Value::Array(Vec::new()),
        };
        to_py(py, core_restore_rows_json(&shape, &rows).map_err(err)?)
    }

    #[pyo3(signature = (rows, backend))]
    fn schema_from_rows(
        &self,
        py: Python<'_>,
        rows: Option<&Bound<'_, PyAny>>,
        backend: String,
    ) -> PyResult<Py<PyAny>> {
        let backend = Backend::parse(&backend).map_err(err)?;
        let rows = match rows {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        to_py(py, core_introspect_to_schema_json(&rows, &backend).map_err(err)?)
    }

    #[pyo3(signature = (base, overlay))]
    fn merge_schema(
        &self,
        py: Python<'_>,
        base: Option<&Bound<'_, PyAny>>,
        overlay: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let base = match base {
            Some(v) => py_to_json(v)?,
            None => Value::Array(Vec::new()),
        };
        let overlay = match overlay {
            Some(v) => py_to_json(v)?,
            None => Value::Array(Vec::new()),
        };
        to_py(py, core_merge_schema(&base, &overlay).map_err(err)?)
    }
}
