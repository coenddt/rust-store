//! 查询路径的 Command 规划方法（GQL → pipeline/projection → 命令序列）。
//!
//! 写路径命令在 [`write`]。

use pyo3::prelude::*;
use serde_json::{json, Map, Value};

use rust_store_core::command::apply_route_override as core_apply_route_override;
use rust_store_core::command::{
    plan_aggregate as core_plan_aggregate, plan_count as core_plan_count,
    plan_exists as core_plan_exists, plan_query as core_plan_query,
    plan_query_one as core_plan_query_one, plan_query_with_count as core_plan_query_with_count,
    resolve_page as core_resolve_page, restore_sort_order as core_restore_sort_order,
    sorts_by_relation as core_sorts_by_relation,
};
use rust_store_core::pipeline::{
    build_pipeline, build_projection, parse_gql, token_to_value, tokenize,
};

use crate::convert::{ctx_from, err, params_from, py_to_json, to_py};
use crate::Registry;

mod write;

/// 多租户路由 override（§6）：`route_override` 键出现才替换计划内命令体的
/// `source` / `namespace`（见 core `apply_route_override`）
pub(super) fn with_route_override(mut plan: Value, route_override: Option<&Value>) -> Value {
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
        let projection = build_projection(&ast, schema, context.as_ref()).unwrap_or(Value::Null);

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

    #[pyo3(signature = (model, filter=None, ctx=None, route_override=None))]
    fn plan_count(
        &self,
        py: Python<'_>,
        model: String,
        filter: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let context = ctx_from(ctx)?;
        let filter = match filter {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let ro = match route_override {
            Some(v) if !v.is_none() => Some(py_to_json(v)?),
            _ => None,
        };
        let out = core_plan_count(&model, &self.core, filter.as_ref(), context.as_ref())
            .map_err(err)?;
        to_py(py, with_route_override(out, ro.as_ref()))
    }

    #[pyo3(signature = (model, pipeline=None, ctx=None, route_override=None))]
    fn plan_aggregate(
        &self,
        py: Python<'_>,
        model: String,
        pipeline: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
        route_override: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let context = ctx_from(ctx)?;
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
        let out = core_plan_aggregate(&model, &self.core, &pipeline, context.as_ref())
            .map_err(err)?;
        to_py(py, with_route_override(out, ro.as_ref()))
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
