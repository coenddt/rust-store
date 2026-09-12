//! federation：跨库联邦计划与结果合并（纯逻辑转发，各源执行留在 Host）。

use pyo3::prelude::*;

use rust_store_core::federation::{
    merge_federated as core_merge_federated, plan_federated as core_plan_federated,
};

use crate::convert::{ctx_from, err, params_from, py_to_json, to_py, value_list};
use crate::Registry;

/// Option 参数 → JSON（None → `Value::Null`）
fn py_to_json_opt(v: Option<&Bound<'_, PyAny>>) -> PyResult<serde_json::Value> {
    match v {
        None => Ok(serde_json::Value::Null),
        Some(v) => py_to_json(v),
    }
}

#[pymethods]
impl Registry {
    /// 生成联邦计划：按 schema 的 `(datasource, namespace)` 与数据源 kind 把一条 GQL
    /// 拆成「各源命令序列 + 内存 join 边」；Host 逐源执行命令后调 `merge_federated`
    ///
    /// `ds_config`：`{ "sources": { name: kind } }`（与 `init` 的数据源声明一致；
    /// `None` = 单源 Mongo）。SQL 同源跨 namespace 仍下推（qualified JOIN），
    /// Mongo 跨 db 剥离为内存 join。
    #[pyo3(signature = (gql, params=None, ctx=None, ds_config=None))]
    fn plan_federated(
        &self,
        py: Python<'_>,
        gql: String,
        params: Option<&Bound<'_, PyAny>>,
        ctx: Option<&Bound<'_, PyAny>>,
        ds_config: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let params = params_from(params)?;
        let context = ctx_from(ctx)?;
        let ds_cfg = py_to_json_opt(ds_config)?;
        let out =
            core_plan_federated(&gql, &params, &self.core, context.as_ref(), &ds_cfg).map_err(err)?;
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
