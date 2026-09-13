//! 写路径命令规划方法（insert / update / remove / archive / upsert / mutation）。

use pyo3::prelude::*;
use serde_json::Value;

use rust_store_core::command::{
    plan_archive_docs as core_plan_archive_docs, plan_insert as core_plan_insert,
    plan_insert_many as core_plan_insert_many, plan_mutation as core_plan_mutation,
    plan_remove as core_plan_remove, plan_update as core_plan_update,
    plan_update_many as core_plan_update_many, plan_upsert as core_plan_upsert, Probe,
};
use rust_store_core::computes::apply_defaults_and_computes as core_apply_defaults;

use super::with_route_override;
use crate::convert::{ctx_from, err, py_to_json, to_py, value_list};
use crate::fns::PyFnBridge;
use crate::Registry;

/// 探针状态换算：`probe_found` 缺省 = 未探查；`False` = 探针无结果（拒绝）；
/// `True` = 探针命中（取 `probe_doc`）
fn probe_of<'a>(probe_found: Option<bool>, probe_doc: Option<&'a Value>) -> Probe<'a> {
    match probe_found {
        None => Probe::NotProbed,
        Some(false) => Probe::NoResult,
        Some(true) => match probe_doc {
            Some(doc) => Probe::Found(doc),
            None => Probe::NoResult,
        },
    }
}

#[pymethods]
impl Registry {
    // ─── Phase 2.5：写路径命令规划 ─────────────────────────

    /// 生成插入命令；`now` / `new_id` 由 Host 提供（core 无时钟与随机源）
    ///
    /// 参数与 JS store API 一一对应（跨语言 parity 优先于参数个数），保持位置参数。
    #[pyo3(signature = (model, data=None, now=0, new_id="", ctx=None, route_override=None))]
    #[allow(clippy::too_many_arguments)]
    fn plan_insert(
        &self,
        py: Python<'_>,
        model: String,
        data: Option<&Bound<'_, PyAny>>,
        now: i64,
        new_id: &str,
        ctx: Option<&Bound<'_, PyAny>>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let context = ctx_from(ctx)?;
        let data = match data {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let bridge = PyFnBridge {
            py,
            fns: &self.sync_fns,
        };
        let out = core_plan_insert(
            &model,
            &self.core,
            context.as_ref(),
            &data,
            now,
            new_id,
            Some(&bridge),
        )
        .map_err(err)?;
        to_py(py, with_route_override(out, ro.as_ref()))
    }

    /// 批量插入命令；`new_ids` 按需消费（仅无 `_id` 的文档取用）
    ///
    /// 参数与 JS store API 一一对应（跨语言 parity 优先于参数个数），保持位置参数。
    #[pyo3(signature = (model, docs=None, now=0, new_ids=None, ctx=None, route_override=None))]
    #[allow(clippy::too_many_arguments)]
    fn plan_insert_many(
        &self,
        py: Python<'_>,
        model: String,
        docs: Option<&Bound<'_, PyAny>>,
        now: i64,
        new_ids: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let docs = value_list(docs)?;
        let new_ids: Vec<String> = value_list(new_ids)?
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            })
            .collect();
        let context = ctx_from(ctx)?;
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let bridge = PyFnBridge {
            py,
            fns: &self.sync_fns,
        };
        let out = core_plan_insert_many(
            &model,
            &self.core,
            context.as_ref(),
            &docs,
            now,
            &new_ids,
            Some(&bridge),
        )
        .map_err(err)?;
        to_py(py, with_route_override(out, ro.as_ref()))
    }

    /// 更新一条（findOneAndUpdate + returnDocument AFTER）。
    ///
    /// creator 写权限需探针时返回 `{"needsProbe": cmd}`；Host 执行探针后携
    /// `probe_found`（True/False）与 `probe_doc` 重入即得 `{"command": cmd}`。
    ///
    /// 参数与 JS store API 一一对应（跨语言 parity 优先于参数个数），保持位置参数。
    #[pyo3(signature = (model, condition=None, data=None, options=None, now=0, ctx=None, probe_found=None, probe_doc=None, route_override=None))]
    #[allow(clippy::too_many_arguments)]
    fn plan_update(
        &self,
        py: Python<'_>,
        model: String,
        condition: Option<&Bound<'_, PyAny>>,
        data: Option<&Bound<'_, PyAny>>,
        options: Option<&Bound<'_, PyAny>>,
        now: i64,
        ctx: Option<&Bound<'_, PyAny>>,
        probe_found: Option<bool>,
        probe_doc: Option<&Bound<'_, PyAny>>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let condition = match condition {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let data = match data {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let options = match options {
            Some(v) if !v.is_none() => py_to_json(v)?,
            _ => Value::Null,
        };
        let probe_doc = match probe_doc {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let context = ctx_from(ctx)?;
        let out = core_plan_update(
            &model,
            &self.core,
            context.as_ref(),
            &condition,
            &data,
            &options,
            now,
            probe_of(probe_found, probe_doc.as_ref()),
        )
        .map_err(err)?;
        to_py(py, with_route_override(out, ro.as_ref()))
    }

    /// 批量更新（guest / 无写授权直接拒绝，不走 creator 探针）
    ///
    /// 参数与 JS store API 一一对应（跨语言 parity 优先于参数个数），保持位置参数。
    #[pyo3(signature = (model, condition=None, data=None, now=0, ctx=None, route_override=None))]
    #[allow(clippy::too_many_arguments)]
    fn plan_update_many(
        &self,
        py: Python<'_>,
        model: String,
        condition: Option<&Bound<'_, PyAny>>,
        data: Option<&Bound<'_, PyAny>>,
        now: i64,
        ctx: Option<&Bound<'_, PyAny>>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let condition = match condition {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let data = match data {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let context = ctx_from(ctx)?;
        let out =
            core_plan_update_many(&model, &self.core, context.as_ref(), &condition, &data, now)
                .map_err(err)?;
        to_py(py, with_route_override(out, ro.as_ref()))
    }

    /// 删除计划：归档表存在时返回 findCommand（Host 取源文档后调 planArchiveDocs）+
    /// deleteCommand。creator 探针语义同 planUpdate。
    ///
    /// 参数与 JS store API 一一对应（跨语言 parity 优先于参数个数），保持位置参数。
    #[pyo3(signature = (model, condition=None, ctx=None, probe_found=None, probe_doc=None, route_override=None))]
    #[allow(clippy::too_many_arguments)]
    fn plan_remove(
        &self,
        py: Python<'_>,
        model: String,
        condition: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
        probe_found: Option<bool>,
        probe_doc: Option<&Bound<'_, PyAny>>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let condition = match condition {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let probe_doc = match probe_doc {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let context = ctx_from(ctx)?;
        let out = core_plan_remove(
            &model,
            &self.core,
            context.as_ref(),
            &condition,
            probe_of(probe_found, probe_doc.as_ref()),
        )
        .map_err(err)?;
        to_py(py, with_route_override(out, ro.as_ref()))
    }

    /// 归档文档命令：源文档补 `deletedAt` 后批量写入 `<collection>_deleted`
    #[pyo3(signature = (model, docs=None, now=0, route_override=None))]
    fn plan_archive_docs(
        &self,
        py: Python<'_>,
        model: String,
        docs: Option<&Bound<'_, PyAny>>,
        now: i64,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let docs = value_list(docs)?;
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let out = core_plan_archive_docs(&model, &self.core, &docs, now).map_err(err)?;
        to_py(py, with_route_override(out, ro.as_ref()))
    }

    /// 显式条件 upsert；`new_id` 仅在需生成 `_id` 时被使用
    ///
    /// 参数与 JS store API 一一对应（跨语言 parity 优先于参数个数），保持位置参数。
    #[pyo3(signature = (model, condition=None, data=None, options=None, now=0, new_id="", ctx=None, route_override=None))]
    #[allow(clippy::too_many_arguments)]
    fn plan_upsert(
        &self,
        py: Python<'_>,
        model: String,
        condition: Option<&Bound<'_, PyAny>>,
        data: Option<&Bound<'_, PyAny>>,
        options: Option<&Bound<'_, PyAny>>,
        now: i64,
        new_id: &str,
        ctx: Option<&Bound<'_, PyAny>>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let condition = match condition {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let data = match data {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let options = match options {
            Some(v) if !v.is_none() => py_to_json(v)?,
            _ => Value::Null,
        };
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let context = ctx_from(ctx)?;
        let out = core_plan_upsert(
            &model,
            &self.core,
            context.as_ref(),
            &condition,
            &data,
            &options,
            now,
            new_id,
        )
        .map_err(err)?;
        to_py(py, with_route_override(out, ro.as_ref()))
    }

    /// mutation 规划：展开为有序步骤序列 `{steps: [{model, command}]}`，
    /// 父子依赖用 `{{step.<N>._id}}` 占位符表达，由 Host 依次执行并回填
    ///
    /// 参数与 JS store API 一一对应（跨语言 parity 优先于参数个数），保持位置参数。
    #[pyo3(signature = (model, data=None, now=0, new_ids=None, ctx=None, route_override=None))]
    #[allow(clippy::too_many_arguments)]
    fn plan_mutation(
        &self,
        py: Python<'_>,
        model: String,
        data: Option<&Bound<'_, PyAny>>,
        now: i64,
        new_ids: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let data = match data {
            Some(v) => py_to_json(v)?,
            None => Value::Null,
        };
        let new_ids: Vec<String> = value_list(new_ids)?
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            })
            .collect();
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let context = ctx_from(ctx)?;
        let out = core_plan_mutation(&model, &self.core, context.as_ref(), &data, now, &new_ids)
            .map_err(err)?;
        to_py(py, with_route_override(out, ro.as_ref()))
    }

    /// 写路径结果回喂：对 findOneAndUpdate 返回文档补默认值 / 同步计算列
    /// （对齐 JS `applyDefaultsAndComputes(result, s)`）
    fn apply_write_defaults(
        &self,
        py: Python<'_>,
        model: String,
        doc: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let schema = self.core.get(&model).map_err(err)?;
        let doc = py_to_json(doc)?;
        let bridge = PyFnBridge {
            py,
            fns: &self.sync_fns,
        };
        let out = core_apply_defaults(&doc, schema, Some(&bridge)).map_err(err)?;
        to_py(py, out)
    }
}
