//! 写路径命令规划方法（insert / update / remove / archive / upsert / mutation）。

use napi::bindgen_prelude::Env;
use napi::Result;
use napi_derive::napi;
use serde_json::Value;

use rust_store_core::command::{
    plan_archive_docs as core_plan_archive_docs, plan_insert as core_plan_insert,
    plan_insert_many as core_plan_insert_many, plan_mutation as core_plan_mutation,
    plan_remove as core_plan_remove, plan_update as core_plan_update,
    plan_update_many as core_plan_update_many, plan_upsert as core_plan_upsert, Probe,
};
use rust_store_core::computes::apply_defaults_and_computes as core_apply_defaults;
use rust_store_core::permission::context_from_value;

use super::with_route_override;
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

#[napi]
impl Registry {
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
        let plan = core_plan_mutation(&model, &self.core, context.as_ref(), &data, now, &new_ids)
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
}
