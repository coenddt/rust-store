//! federation：跨库联邦计划与结果合并（纯逻辑转发，各源执行留在 Host）。

use pyo3::prelude::*;

use rust_store_core::federation::{
    merge_federated as core_merge_federated, plan_federated as core_plan_federated,
};

use crate::convert::{ctx_from, err, params_from, py_to_json, to_py, value_list};
use crate::Registry;

#[pymethods]
impl Registry {
    /// 生成联邦计划：按 `Schema.datasource` 把一条 GQL 拆成
    /// 「各源命令序列 + 内存 join 边」；Host 逐源执行命令后调 `merge_federated`
    ///
    /// 返回 `{v, kind:"federated", root, sources, join, postprocess, degraded}`；
    /// 单源（无跨源关系）时 `sources` 仅根单元、`join.edges` 为空。
    #[pyo3(signature = (gql, params=None, ctx=None))]
    fn plan_federated(
        &self,
        py: Python<'_>,
        gql: String,
        params: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let params = params_from(params)?;
        let context = ctx_from(ctx)?;
        let out = core_plan_federated(&gql, &params, &self.core, context.as_ref()).map_err(err)?;
        to_py(py, out)
    }

    /// 合并各源结果 → 嵌套文档数组；`results` 必须与 `plan["sources"]` **同序同长**
    fn merge_federated(
        &self,
        py: Python<'_>,
        plan: &Bound<'_, PyAny>,
        results: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let plan = py_to_json(plan)?;
        let results = value_list(Some(results))?;
        let out = core_merge_federated(&plan, &results).map_err(err)?;
        to_py(py, out)
    }
}
