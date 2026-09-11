//! 权限相关方法。

use pyo3::prelude::*;
use serde_json::Value;

use rust_store_core::permission::{
    can_read_schema, can_write_schema, filter_writable_data, get_readable_fields,
    get_readable_relations, get_writable_fields, merge_owner_condition, should_inject_owner_condition,
};

use crate::convert::{ctx_from, err, py_to_json, sorted_set, to_py};
use crate::Registry;

#[pymethods]
impl Registry {
    #[pyo3(signature = (model, ctx=None))]
    fn can_read(&self, model: String, ctx: Option<&Bound<'_, PyAny>>) -> PyResult<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        Ok(can_read_schema(schema, context.as_ref()))
    }

    #[pyo3(signature = (model, ctx=None))]
    fn can_write(&self, model: String, ctx: Option<&Bound<'_, PyAny>>) -> PyResult<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        Ok(can_write_schema(schema, context.as_ref()))
    }

    #[pyo3(signature = (model, ctx=None))]
    fn should_inject_owner(
        &self,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        Ok(should_inject_owner_condition(schema, context.as_ref()))
    }

    /// 非 admin 用户只看自己数据时叠加 owner 条件；无上下文时原样返回
    #[pyo3(signature = (model, ctx=None, condition=None))]
    fn merge_owner_condition(
        &self,
        py: Python<'_>,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
        condition: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        let condition = match condition {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let out =
            merge_owner_condition(schema, context.as_ref(), condition).unwrap_or(Value::Null);
        to_py(py, out)
    }

    #[pyo3(signature = (model, ctx=None))]
    fn readable_fields(
        &self,
        py: Python<'_>,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        to_py(py, sorted_set(get_readable_fields(schema, context.as_ref())))
    }

    #[pyo3(signature = (model, ctx=None))]
    fn readable_relations(
        &self,
        py: Python<'_>,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        to_py(py, sorted_set(get_readable_relations(schema, context.as_ref())))
    }

    #[pyo3(signature = (model, ctx=None))]
    fn writable_fields(
        &self,
        py: Python<'_>,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        to_py(py, sorted_set(get_writable_fields(schema, context.as_ref())))
    }

    #[pyo3(signature = (model, ctx=None, data=None))]
    fn filter_writable_data(
        &self,
        py: Python<'_>,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
        data: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        let data = match data {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        to_py(py, filter_writable_data(schema, context.as_ref(), &data))
    }
}
