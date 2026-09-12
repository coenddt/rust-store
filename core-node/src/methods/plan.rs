//! 查询 / 写路径的 Command 规划方法。

use napi::bindgen_prelude::Env;
use napi::Result;
use napi_derive::napi;
use serde_json::{json, Value};

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
use rust_store_core::permission::context_from_value;
use rust_store_core::pipeline::{build_pipeline, build_projection, parse_gql, token_to_value, tokenize};

use crate::convert::err;
use crate::Registry;

/// 探针状态换算：`probe_found` 缺省 = 未探查；`false` = 探针无结果（拒绝）；
/// `true` = 探针命中（取 `probe_doc`）
fn probe_of(probe_found: Option<bool>, probe_doc: Option<&Value>) -> Probe<'_> {
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
fn with_route_override(mut plan: Value, route_override: &Option<Value>) -> Value {
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
        let pipeline = build_pipeline(&mut ast, &params, &self.core, context.as_ref()).map_err(err)?;
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

    /// 生成插入命令；`now` / `newId` 由 Host 提供（core 无时钟与随机源）
    ///
    /// 参数与 JS store API 一一对应（跨语言 parity 优先于参数个数），保持位置参数。
    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub fn plan_insert(
        &self,
        env: Env,
        model: String,
        data: Value,
        now: i64,
        new_id: String,
        ctx: Option<Value>,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        let bridge = self.bridge(&env);
        let plan = core_plan_insert(
            &model,
            &self.core,
            context.as_ref(),
            &data,
            now,
            &new_id,
            Some(&bridge),
        )
        .map_err(err)?;
        Ok(with_route_override(plan, &route_override))
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
        route_override: Option<Value>,
    ) -> Result<Value> {
        let filter = filter.as_ref().filter(|v| !v.is_null());
        let plan = core_plan_count(&model, &self.core, filter).map_err(err)?;
        Ok(with_route_override(plan, &route_override))
    }

    #[napi]
    pub fn plan_aggregate(
        &self,
        model: String,
        pipeline: Vec<Value>,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let plan = core_plan_aggregate(&model, &self.core, &pipeline).map_err(err)?;
        Ok(with_route_override(plan, &route_override))
    }

    // ─── Phase 2.5：写路径命令规划 ─────────────────────────

    /// 批量插入命令；`newIds` 按需消费（仅无 `_id` 的文档取用）
    ///
    /// 参数与 JS store API 一一对应（跨语言 parity 优先于参数个数），保持位置参数。
    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub fn plan_insert_many(
        &self,
        env: Env,
        model: String,
        docs: Vec<Value>,
        now: i64,
        new_ids: Vec<String>,
        ctx: Option<Value>,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        let bridge = self.bridge(&env);
        let plan = core_plan_insert_many(
            &model,
            &self.core,
            context.as_ref(),
            &docs,
            now,
            &new_ids,
            Some(&bridge),
        )
        .map_err(err)?;
        Ok(with_route_override(plan, &route_override))
    }

    /// 更新一条（findOneAndUpdate + returnDocument AFTER）。
    ///
    /// creator 写权限需探针时返回 `{"needsProbe": cmd}`；Host 执行探针后携
    /// `probeFound`（true/false）与 `probeDoc` 重入即得 `{"command": cmd}`。
    ///
    /// 参数与 JS store API 一一对应（跨语言 parity 优先于参数个数），保持位置参数。
    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub fn plan_update(
        &self,
        model: String,
        condition: Value,
        data: Value,
        options: Option<Value>,
        now: i64,
        ctx: Option<Value>,
        probe_found: Option<bool>,
        probe_doc: Option<Value>,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        let plan = core_plan_update(
            &model,
            &self.core,
            context.as_ref(),
            &condition,
            &data,
            &options.unwrap_or(Value::Null),
            now,
            probe_of(probe_found, probe_doc.as_ref()),
        )
        .map_err(err)?;
        Ok(with_route_override(plan, &route_override))
    }

    /// 批量更新（guest / 无写授权直接拒绝，不走 creator 探针）
    #[napi]
    pub fn plan_update_many(
        &self,
        model: String,
        condition: Value,
        data: Value,
        now: i64,
        ctx: Option<Value>,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        let plan =
            core_plan_update_many(&model, &self.core, context.as_ref(), &condition, &data, now)
                .map_err(err)?;
        Ok(with_route_override(plan, &route_override))
    }

    /// 删除计划：归档表存在时返回 findCommand（Host 取源文档后调 planArchiveDocs）+
    /// deleteCommand。creator 探针语义同 planUpdate。
    #[napi]
    pub fn plan_remove(
        &self,
        model: String,
        condition: Value,
        ctx: Option<Value>,
        probe_found: Option<bool>,
        probe_doc: Option<Value>,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        let plan = core_plan_remove(
            &model,
            &self.core,
            context.as_ref(),
            &condition,
            probe_of(probe_found, probe_doc.as_ref()),
        )
        .map_err(err)?;
        Ok(with_route_override(plan, &route_override))
    }

    /// 归档文档命令：源文档补 `deletedAt` 后批量写入 `<collection>_deleted`
    #[napi]
    pub fn plan_archive_docs(
        &self,
        model: String,
        docs: Vec<Value>,
        now: i64,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let plan = core_plan_archive_docs(&model, &self.core, &docs, now).map_err(err)?;
        Ok(with_route_override(plan, &route_override))
    }

    /// 显式条件 upsert；`newId` 仅在需生成 `_id` 时被使用
    ///
    /// 参数与 JS store API 一一对应（跨语言 parity 优先于参数个数），保持位置参数。
    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub fn plan_upsert(
        &self,
        model: String,
        condition: Value,
        data: Value,
        options: Option<Value>,
        now: i64,
        new_id: String,
        ctx: Option<Value>,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        let plan = core_plan_upsert(
            &model,
            &self.core,
            context.as_ref(),
            &condition,
            &data,
            &options.unwrap_or(Value::Null),
            now,
            &new_id,
        )
        .map_err(err)?;
        Ok(with_route_override(plan, &route_override))
    }

    /// mutation 规划：展开为有序步骤序列 `{steps: [{model, command}]}`，
    /// 父子依赖用 `{{step.<N>._id}}` 占位符表达，由 Host 依次执行并回填
    #[napi]
    pub fn plan_mutation(
        &self,
        model: String,
        data: Value,
        now: i64,
        new_ids: Vec<String>,
        ctx: Option<Value>,
        route_override: Option<Value>,
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        let plan =
            core_plan_mutation(&model, &self.core, context.as_ref(), &data, now, &new_ids)
                .map_err(err)?;
        Ok(with_route_override(plan, &route_override))
    }

    /// 写路径结果回喂：对 findOneAndUpdate 返回文档补默认值 / 同步计算列
    /// （对齐 JS `applyDefaultsAndComputes(result, s)`）
    #[napi]
    pub fn apply_write_defaults(&self, env: Env, model: String, doc: Value) -> Result<Value> {
        let schema = self.core.get(&model).map_err(err)?;
        let bridge = self.bridge(&env);
        core_apply_defaults(&doc, schema, Some(&bridge)).map_err(err)
    }

    #[napi]
    pub fn sorts_by_relation(&self, sort: Option<Value>) -> bool {
        core_sorts_by_relation(sort.as_ref().filter(|v| !v.is_null()))
    }
}
