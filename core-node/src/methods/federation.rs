//! federation：跨库联邦计划与结果合并（纯逻辑转发，各源执行留在 Host）。

use napi::Result;
use napi_derive::napi;
use serde_json::Value;

use rust_store_core::federation::{
    merge_federated as core_merge_federated, plan_federated as core_plan_federated,
};
use rust_store_core::permission::context_from_value;

use crate::convert::err;
use crate::Registry;

#[napi]
impl Registry {
    /// 生成联邦计划：按 `Schema.datasource` 把一条 GQL 拆成
    /// 「各源命令序列 + 内存 join 边」；Host 逐源执行命令后调 `mergeFederated`
    ///
    /// 返回 `{v, kind:"federated", root, sources, join, postprocess, degraded}`；
    /// 单源（无跨源关系）时 `sources` 仅根单元、`join.edges` 为空。
    #[napi]
    pub fn plan_federated(
        &self,
        gql: String,
        params: Value,
        ctx: Option<Value>,
    ) -> Result<Value> {
        let params = Self::params_map(&params);
        let context = ctx.as_ref().and_then(context_from_value);
        core_plan_federated(&gql, &params, &self.core, context.as_ref()).map_err(err)
    }

    /// 合并各源结果 → 嵌套文档数组；`results` 必须与 `plan.sources` **同序同长**
    #[napi]
    pub fn merge_federated(&self, plan: Value, results: Vec<Value>) -> Result<Value> {
        core_merge_federated(&plan, &results).map_err(err)
    }
}
