//! 权限相关方法。

use pyo3::prelude::*;
use serde_json::Value;

use rust_store_core::permission::{
    can_read_schema, can_write_schema, filter_writable_data, get_readable_computes,
    get_readable_fields, get_readable_relations, get_writable_fields, merge_owner_condition,
    should_inject_owner_condition,
};

use crate::convert::{ctx_from, err, py_to_json, sorted_set, to_py};
use crate::Registry;

#[pymethods]
impl Registry {
    #[pyo3(signature = (model, ctx=None))]
    fn can_read(&self, model: String, ctx: Option<&Bound<'_, PyAny>>) -> PyResult<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        Ok(can_read_schema(
            self.core.role_rules(),
            schema,
            context.as_ref(),
        ))
    }

    #[pyo3(signature = (model, ctx=None))]
    fn can_write(&self, model: String, ctx: Option<&Bound<'_, PyAny>>) -> PyResult<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        Ok(can_write_schema(
            self.core.role_rules(),
            schema,
            context.as_ref(),
        ))
    }

    #[pyo3(signature = (model, ctx=None))]
    fn should_inject_owner(&self, model: String, ctx: Option<&Bound<'_, PyAny>>) -> PyResult<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        Ok(should_inject_owner_condition(
            self.core.role_rules(),
            schema,
            context.as_ref(),
        ))
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
            merge_owner_condition(self.core.role_rules(), schema, context.as_ref(), condition)
                .unwrap_or(Value::Null);
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
        to_py(
            py,
            sorted_set(get_readable_fields(
                self.core.role_rules(),
                schema,
                context.as_ref(),
            )),
        )
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
        to_py(
            py,
            sorted_set(get_readable_relations(
                self.core.role_rules(),
                schema,
                context.as_ref(),
            )),
        )
    }

    /// 可读计算列（read 白名单判决与 fields/relations 同构；`ctx=None` → None 不裁剪）
    #[pyo3(signature = (model, ctx=None))]
    fn readable_computes(
        &self,
        py: Python<'_>,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        to_py(
            py,
            sorted_set(get_readable_computes(
                self.core.role_rules(),
                schema,
                context.as_ref(),
            )),
        )
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
        to_py(
            py,
            sorted_set(get_writable_fields(
                self.core.role_rules(),
                schema,
                context.as_ref(),
            )),
        )
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
        to_py(
            py,
            filter_writable_data(self.core.role_rules(), schema, context.as_ref(), &data),
        )
    }

    // ─── RBAC 查询面（判决唯一在 core；本层零判决逻辑） ────────

    /// RBAC 动作判决：`action ∈ {read, insert, update, remove}`。
    /// 策略未注入 / RBAC 不介入（无 ctx / internal / 豁免角色 / overlay 未覆盖 /
    /// enforce 未受管）→ true（与 plan 链路的实际拦截结果一致）。
    #[pyo3(signature = (model, action, ctx=None))]
    fn rbac_can(
        &self,
        model: String,
        action: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        Ok(match action.as_str() {
            "read" => {
                rust_store_core::rbac::ensure_read(&self.core, schema, context.as_ref()).is_ok()
            }
            a => {
                let wa = rust_store_core::rbac::write_action_from_str(a).map_err(err)?;
                rust_store_core::rbac::ensure_write(&self.core, schema, context.as_ref(), wa)
                    .is_ok()
            }
        })
    }

    /// RBAC 叠加后的可读字段集（静态 ∩ readFields）；`ctx=None` → None 不裁剪
    #[pyo3(signature = (model, ctx=None))]
    fn rbac_readable_fields(
        &self,
        py: Python<'_>,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        let base = get_readable_fields(self.core.role_rules(), schema, context.as_ref());
        to_py(
            py,
            sorted_set(rust_store_core::rbac::overlay_readable_fields(
                &self.core,
                &model,
                context.as_ref(),
                base,
            )),
        )
    }

    /// RBAC 叠加后的可写字段集（静态 ∩ writeFields）；`ctx=None` → None 不裁剪
    #[pyo3(signature = (model, ctx=None))]
    fn rbac_writable_fields(
        &self,
        py: Python<'_>,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        let base = get_writable_fields(self.core.role_rules(), schema, context.as_ref());
        to_py(
            py,
            sorted_set(rust_store_core::rbac::overlay_writable_fields(
                &self.core,
                &model,
                context.as_ref(),
                base,
            )),
        )
    }

    /// RBAC 行级条件（ownerOnly / condition 的 OR 合并体）；None = 无行级收紧。
    /// `action ∈ {read, update, remove}`（insert 无行级语义）。
    #[pyo3(signature = (model, action, ctx=None))]
    fn rbac_row_condition(
        &self,
        py: Python<'_>,
        model: String,
        action: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        if !matches!(action.as_str(), "read" | "update" | "remove") {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "RBAC row_condition 的 action \"{action}\" 非法（仅支持 read / update / remove）"
            )));
        }
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        // None（无行级收紧）→ null（对齐 merge_owner_condition 绑定的既有形态）
        let cond =
            rust_store_core::rbac::row_condition(&self.core, schema, context.as_ref(), &action)
                .unwrap_or(Value::Null);
        to_py(py, cond)
    }
}
