//! 查询路径的 Command 规划方法（GQL → pipeline/projection → 命令序列）。
//!
//! 写路径命令在 [`write`]。

use napi::Result;
use napi_derive::napi;
use serde_json::{json, Value};

use rust_store_core::command::apply_route_override as core_apply_route_override;
use rust_store_core::command::{
    plan_aggregate as core_plan_aggregate, plan_count as core_plan_count,
    plan_exists as core_plan_exists, plan_query as core_plan_query,
    plan_query_one as core_plan_query_one, plan_query_with_count as core_plan_query_with_count,
    resolve_page as core_resolve_page, restore_sort_order as core_restore_sort_order,
    sorts_by_relation as core_sorts_by_relation,
};
use rust_store_core::permission::context_from_value;
use rust_store_core::pipeline::{
    build_pipeline, build_projection, parse_gql, token_to_value, tokenize,
};

use crate::convert::err;
use crate::Registry;

mod write;

/// 多租户路由 override（§6）：`route_override` 键出现才替换计划内命令体的
/// `source` / `namespace`（见 core `apply_route_override`）
pub(super) fn with_route_override(mut plan: Value, route_override: &Option<Value>) -> Value {
    if let Some(ov) = route_override.as_ref().filter(|v| !v.is_null()) {
        core_apply_route_override(&mut plan, ov);
    }
    plan
}

#[napi]
impl Registry {
    // ─── Phase 1：GQL → pipeline / projection ───────────────

    /// 解析 GQL 并构建 pipeline / projection，返回 `{tokens, ast, pipeline, projection}`
    #[napi]
    pub fn build_pipeline(&self, gql: String, params: Value, ctx: Option<Value>) -> Result<Value> {
        let params = Self::params_map(&params);
        let context = ctx.as_ref().and_then(context_from_value);

        let tokens = tokenize(&gql);
        let tokens_value = Value::Array(tokens.iter().map(token_to_value).collect());
        // `build_pipeline` 会原地展平 ast，故 ast 快照须在展平前取
        let mut ast = parse_gql(&gql).map_err(err)?;
        let ast_value = ast.to_value();
        let schema = self.core.get(&ast.model).map_err(err)?;
        let pipeline =
            build_pipeline(&mut ast, &params, &self.core, context.as_ref()).map_err(err)?;
        let projection = build_projection(&ast, schema, context.as_ref()).unwrap_or(Value::Null);

        Ok(json!({
            "tokens": tokens_value,
            "ast": ast_value,
            "pipeline": pipeline,
            "projection": projection,
        }))
    }

    // ─── Phase 2：Command 序列 ─────────────────────────────

    #[napi]
    pub fn plan_query(
        &self,
        gql: String,
        params: Value,
        ctx: Option<Value>,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let params = Self::params_map(&params);
        let context = ctx.as_ref().and_then(context_from_value);
        let plan = core_plan_query(&gql, &params, &self.core, context.as_ref())
            .map(|p| p.to_value())
            .map_err(err)?;
        Ok(with_route_override(plan, &route_override))
    }

    /// queryOne 计划：未显式 `$limit` 时强制下推 `$limit(1)`（`$pipeline` 全权模式不注入）
    #[napi]
    pub fn plan_query_one(
        &self,
        gql: String,
        params: Value,
        ctx: Option<Value>,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let params = Self::params_map(&params);
        let context = ctx.as_ref().and_then(context_from_value);
        let plan = core_plan_query_one(&gql, &params, &self.core, context.as_ref())
            .map(|p| p.to_value())
            .map_err(err)?;
        Ok(with_route_override(plan, &route_override))
    }

    /// 列表 + total；`total` 由 Host 执行 `countCommand` 后回喂，用于算 `hasMore`
    #[napi]
    pub fn plan_query_with_count(
        &self,
        gql: String,
        params: Value,
        ctx: Option<Value>,
        total: Option<f64>,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let params = Self::params_map(&params);
        let context = ctx.as_ref().and_then(context_from_value);
        let plan =
            core_plan_query_with_count(&gql, &params, &self.core, context.as_ref()).map_err(err)?;

        let mut out = plan.to_value().as_object().cloned().unwrap_or_default();
        out.insert(
            "hasMore".to_string(),
            json!(plan.has_more(total.unwrap_or(0.0))),
        );
        Ok(with_route_override(Value::Object(out), &route_override))
    }

    #[napi]
    pub fn resolve_page(&self, gql: String, params: Value) -> Result<Value> {
        let ast = parse_gql(&gql).map_err(err)?;
        let params = Self::params_map(&params);
        Ok(core_resolve_page(&ast, &params).to_value())
    }

    /// 两阶段查询后按阶段一 `_id` 顺序重排，返回 `{items}`
    #[napi]
    pub fn restore_sort_order(
        &self,
        items: Vec<Value>,
        ids: Vec<Value>,
        sort: Option<Value>,
    ) -> Result<Value> {
        let mut items = items;
        let sort_ref = sort.as_ref().filter(|v| !v.is_null());
        core_restore_sort_order(&mut items, &ids, sort_ref);
        Ok(json!({ "items": items }))
    }

    #[napi]
    pub fn plan_exists(
        &self,
        model: String,
        condition: Value,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let plan = core_plan_exists(&model, &self.core, &condition).map_err(err)?;
        Ok(with_route_override(plan, &route_override))
    }

    #[napi]
    pub fn plan_count(
        &self,
        model: String,
        filter: Option<Value>,
        ctx: Option<Value>,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let filter = filter.as_ref().filter(|v| !v.is_null());
        let context = ctx.as_ref().and_then(context_from_value);
        let plan = core_plan_count(&model, &self.core, filter, context.as_ref()).map_err(err)?;
        Ok(with_route_override(plan, &route_override))
    }

    #[napi]
    pub fn plan_aggregate(
        &self,
        model: String,
        pipeline: Vec<Value>,
        ctx: Option<Value>,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        let plan =
            core_plan_aggregate(&model, &self.core, &pipeline, context.as_ref()).map_err(err)?;
        Ok(with_route_override(plan, &route_override))
    }

    #[napi]
    pub fn sorts_by_relation(&self, sort: Option<Value>) -> bool {
        core_sorts_by_relation(sort.as_ref().filter(|v| !v.is_null()))
    }
}
