//! 查询 / 写路径的 Command 规划方法。

use pyo3::prelude::*;
use serde_json::{json, Map, Value};

use rust_store_core::command::apply_route_override as core_apply_route_override;
use rust_store_core::command::{
    plan_aggregate as core_plan_aggregate, plan_archive_docs as core_plan_archive_docs,
    plan_count as core_plan_count, plan_exists as core_plan_exists,
    plan_insert as core_plan_insert, plan_insert_many as core_plan_insert_many,
    plan_mutation as core_plan_mutation, plan_query as core_plan_query,
    plan_query_one as core_plan_query_one,
    plan_query_with_count as core_plan_query_with_count, plan_remove as core_plan_remove,
    plan_update as core_plan_update, plan_update_many as core_plan_update_many,
    plan_upsert as core_plan_upsert, resolve_page as core_resolve_page,
    restore_sort_order as core_restore_sort_order, sorts_by_relation as core_sorts_by_relation,
    Probe,
};
use rust_store_core::computes::apply_defaults_and_computes as core_apply_defaults;
use rust_store_core::pipeline::{build_pipeline, build_projection, parse_gql, token_to_value, tokenize};

use crate::convert::{ctx_from, err, params_from, py_to_json, to_py, value_list};
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

/// 多租户路由 override（§6）：`route_override` 键出现才替换计划内命令体的
/// `source` / `namespace`（见 core `apply_route_override`）
fn with_route_override(mut plan: Value, route_override: Option<&Value>) -> Value {
    if let Some(ov) = route_override.filter(|v| !v.is_null()) {
        core_apply_route_override(&mut plan, ov);
    }
    plan
}

#[pymethods]
impl Registry {
    // ─── Phase 1：GQL → pipeline / projection ───────────────

    /// 解析 GQL 并构建 pipeline / projection，返回 `{tokens, ast, pipeline, projection}`
    #[pyo3(signature = (gql, params=None, ctx=None))]
    fn build_pipeline(
        &self,
        py: Python<'_>,
        gql: String,
        params: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let params = params_from(params)?;
        let context = ctx_from(ctx)?;

        let tokens = tokenize(&gql);
        let tokens_value = Value::Array(tokens.iter().map(token_to_value).collect());
        // `build_pipeline` 会原地展平 ast，故 ast 快照须在展平前取
        let mut ast = parse_gql(&gql).map_err(err)?;
        let ast_value = ast.to_value();
        let schema = self.core.get(&ast.model).map_err(err)?;
        let pipeline =
            build_pipeline(&mut ast, &params, &self.core, context.as_ref()).map_err(err)?;
        let projection =
            build_projection(&ast, schema, context.as_ref()).unwrap_or(Value::Null);

        to_py(
            py,
            json!({
                "tokens": tokens_value,
                "ast": ast_value,
                "pipeline": pipeline,
                "projection": projection,
            }),
        )
    }

    // ─── Phase 2：Command 序列 ─────────────────────────────

    #[pyo3(signature = (gql, params=None, ctx=None, route_override=None))]
    fn plan_query(
        &self,
        py: Python<'_>,
        gql: String,
        params: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let params = params_from(params)?;
        let context = ctx_from(ctx)?;
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let plan = core_plan_query(&gql, &params, &self.core, context.as_ref())
            .map(|p| p.to_value())
            .map_err(err)?;
        to_py(py, with_route_override(plan, ro.as_ref()))
    }

    /// queryOne 计划：未显式 `$limit` 时强制下推 `$limit(1)`（`$pipeline` 全权模式不注入）
    #[pyo3(signature = (gql, params=None, ctx=None, route_override=None))]
    fn plan_query_one(
        &self,
        py: Python<'_>,
        gql: String,
        params: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let params = params_from(params)?;
        let context = ctx_from(ctx)?;
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let plan = core_plan_query_one(&gql, &params, &self.core, context.as_ref())
            .map(|p| p.to_value())
            .map_err(err)?;
        to_py(py, with_route_override(plan, ro.as_ref()))
    }

    /// 列表 + total；`total` 由 Host 执行 `countCommand` 后回喂，用于算 `hasMore`
    #[pyo3(signature = (gql, params=None, ctx=None, total=None, route_override=None))]
    fn plan_query_with_count(
        &self,
        py: Python<'_>,
        gql: String,
        params: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
        total: Option<f64>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let params = params_from(params)?;
        let context = ctx_from(ctx)?;
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let plan =
            core_plan_query_with_count(&gql, &params, &self.core, context.as_ref()).map_err(err)?;

        let mut out = plan.to_value().as_object().cloned().unwrap_or_default();
        out.insert(
            "hasMore".to_string(),
            json!(plan.has_more(total.unwrap_or(0.0))),
        );
        to_py(py, with_route_override(Value::Object(out), ro.as_ref()))
    }

    #[pyo3(signature = (gql, params=None))]
    fn resolve_page(
        &self,
        py: Python<'_>,
        gql: String,
        params: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let ast = parse_gql(&gql).map_err(err)?;
        let params = params_from(params)?;
        to_py(py, core_resolve_page(&ast, &params).to_value())
    }

    /// 两阶段查询后按阶段一 `_id` 顺序重排，返回 `{items}`
    #[pyo3(signature = (items, ids, sort=None))]
    fn restore_sort_order(
        &self,
        py: Python<'_>,
        items: &Bound<'_, PyAny>,
        ids: &Bound<'_, PyAny>,
        sort: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let mut items = match py_to_json(items)? {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        let ids = match py_to_json(ids)? {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        let sort_v = match sort {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        core_restore_sort_order(&mut items, &ids, sort_v.as_ref());
        to_py(py, json!({ "items": items }))
    }

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

    #[pyo3(signature = (model, condition=None, route_override=None))]
    fn plan_exists(
        &self,
        py: Python<'_>,
        model: String,
        condition: Option<&Bound<'_, PyAny>>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let condition = match condition {
            Some(v) if !v.is_none() => py_to_json(v)?,
            _ => Value::Object(Map::new()),
        };
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let out = core_plan_exists(&model, &self.core, &condition).map_err(err)?;
        to_py(py, with_route_override(out, ro.as_ref()))
    }

    #[pyo3(signature = (model, filter=None, route_override=None))]
    fn plan_count(
        &self,
        py: Python<'_>,
        model: String,
        filter: Option<&Bound<'_, PyAny>>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let filter = match filter {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let out = core_plan_count(&model, &self.core, filter.as_ref()).map_err(err)?;
        to_py(py, with_route_override(out, ro.as_ref()))
    }

    #[pyo3(signature = (model, pipeline=None, route_override=None))]
    fn plan_aggregate(
        &self,
        py: Python<'_>,
        model: String,
        pipeline: Option<&Bound<'_, PyAny>>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let pipeline = match pipeline {
            Some(v) => match py_to_json(v)? {
                Value::Array(a) => a,
                _ => Vec::new(),
            },
            None => Vec::new(),
        };
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let out = core_plan_aggregate(&model, &self.core, &pipeline).map_err(err)?;
        to_py(py, with_route_override(out, ro.as_ref()))
    }

    // ─── Phase 2.5：写路径命令规划 ─────────────────────────

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
        let out = core_plan_update_many(&model, &self.core, context.as_ref(), &condition, &data, now)
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
        let out =
            core_plan_mutation(&model, &self.core, context.as_ref(), &data, now, &new_ids)
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

    #[pyo3(signature = (sort=None))]
    fn sorts_by_relation(&self, sort: Option<&Bound<'_, PyAny>>) -> PyResult<bool> {
        let sort_v = match sort {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        Ok(core_sorts_by_relation(sort_v.as_ref()))
    }
}
