//! 计算列 / 依赖注入 / 结果回喂方法。

use pyo3::prelude::*;
use serde_json::{json, Value};

use rust_store_core::command::{prepare_query as core_prepare_query, strip_query as core_strip_query};
use rust_store_core::computes::{
    collect_rel_deps, merge_depends_into_ast, process_node as core_process_node, select_async_fns,
    strip_dep_injected as core_strip_dep_injected, InjectInfo,
};
use rust_store_core::pipeline::parse_gql;

use crate::convert::{ctx_from, err, py_to_json, to_py};
use crate::fns::PyFnBridge;
use crate::Registry;

#[pymethods]
impl Registry {
    // ─── Phase 3：计算列 / 依赖注入 / 结果回喂 ─────────────

    /// 逐条后处理文档（默认值 → 同步 fn → 递归下钻 → 权限裁剪），返回 `{doc}`
    #[pyo3(signature = (gql, doc, ctx=None))]
    fn process_node(
        &self,
        py: Python<'_>,
        gql: String,
        doc: &Bound<'_, PyAny>,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let mut ast = parse_gql(&gql).map_err(err)?;
        let schema = self.core.get(&ast.model).map_err(err)?;
        let context = ctx_from(ctx)?;
        let bridge = PyFnBridge {
            py,
            fns: &self.sync_fns,
        };
        let mut doc = py_to_json(doc)?;

        core_process_node(
            &mut doc,
            &mut ast.fields,
            &mut ast.relations,
            schema,
            context.as_ref(),
            &self.core,
            Some(&bridge),
        )
        .map_err(err)?;
        to_py(py, json!({ "doc": doc }))
    }

    /// 需由 Host 异步执行的计算列 `fn_ref` 列表（已按 `comp.read` 权限过滤）
    #[pyo3(signature = (model, ctx=None))]
    fn async_fn_refs(
        &self,
        model: String,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Vec<String>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx_from(ctx)?;
        Ok(select_async_fns(schema, context.as_ref())
            .into_iter()
            .map(|e| e.fn_ref)
            .collect())
    }

    /// 收集关系依赖并注入 AST，返回 `{relDeps, ast, injectInfo}`
    fn inject_depends(&self, py: Python<'_>, gql: String) -> PyResult<Py<PyAny>> {
        let mut ast = parse_gql(&gql).map_err(err)?;
        let schema = self.core.get(&ast.model).map_err(err)?;

        let deps = collect_rel_deps(schema).map_err(err)?;
        let deps_value = Value::Array(deps.iter().map(|d| d.to_value()).collect());
        let info = merge_depends_into_ast(&mut ast.relations, schema).map_err(err)?;

        to_py(
            py,
            json!({
                "relDeps": deps_value,
                "ast": ast.to_value(),
                "injectInfo": info.to_value(),
            }),
        )
    }

    /// 剥离 asyncFn 依赖注入的字段，返回 `{items}`
    fn strip_dep_injected(
        &self,
        py: Python<'_>,
        inject_info: &Bound<'_, PyAny>,
        items: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let info = InjectInfo::from_value(&py_to_json(inject_info)?);
        let mut items = match py_to_json(items)? {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        core_strip_dep_injected(&mut items, &info);
        to_py(py, json!({ "items": items }))
    }

    /// finalize 第一阶段：逐条 `process_node`，返回 `{items, fnRefs}`
    #[pyo3(signature = (postprocess, items, ctx=None))]
    fn prepare_query(
        &self,
        py: Python<'_>,
        postprocess: &Bound<'_, PyAny>,
        items: &Bound<'_, PyAny>,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let context = ctx_from(ctx)?;
        let bridge = PyFnBridge {
            py,
            fns: &self.sync_fns,
        };
        let mut items = match py_to_json(items)? {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        let postprocess = py_to_json(postprocess)?;

        let fn_refs = core_prepare_query(
            &postprocess,
            &mut items,
            &self.core,
            Some(&bridge),
            context.as_ref(),
        )
        .map_err(err)?;

        to_py(py, json!({ "items": items, "fnRefs": fn_refs }))
    }

    /// finalize 第三阶段：剥离 asyncFn 依赖注入的字段，返回 `{items}`
    fn strip_query(
        &self,
        py: Python<'_>,
        postprocess: &Bound<'_, PyAny>,
        items: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let postprocess = py_to_json(postprocess)?;
        let mut items = match py_to_json(items)? {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        core_strip_query(&postprocess, &mut items);
        to_py(py, json!({ "items": items }))
    }
}
