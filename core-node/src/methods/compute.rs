//! 计算列 / 依赖注入 / 结果回喂方法。

use napi::bindgen_prelude::Env;
use napi::Result;
use napi_derive::napi;
use serde_json::{json, Value};

use rust_store_core::command::{
    prepare_query as core_prepare_query, strip_query as core_strip_query,
};
use rust_store_core::computes::{
    collect_rel_deps, merge_depends_into_ast, process_node as core_process_node, select_async_fns,
    strip_dep_injected as core_strip_dep_injected, InjectInfo,
};
use rust_store_core::permission::context_from_value;
use rust_store_core::pipeline::parse_gql;

use crate::convert::err;
use crate::Registry;

#[napi]
impl Registry {
    // ─── Phase 3：计算列 / 依赖注入 / 结果回喂 ─────────────

    /// 逐条后处理文档（默认值 → 同步 fn → 递归下钻 → 权限裁剪），返回 `{doc}`
    #[napi]
    pub fn process_node(
        &self,
        env: Env,
        gql: String,
        doc: Value,
        ctx: Option<Value>,
    ) -> Result<Value> {
        let mut ast = parse_gql(&gql).map_err(err)?;
        let schema = self.core.get(&ast.model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        let bridge = self.bridge(&env);
        let mut doc = doc;

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
        Ok(json!({ "doc": doc }))
    }

    /// 需由 Host 异步执行的计算列 `fnRef` 列表（已按 `comp.read` 权限过滤）
    #[napi]
    pub fn async_fn_refs(&self, model: String, ctx: Option<Value>) -> Result<Vec<String>> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(select_async_fns(schema, context.as_ref())
            .into_iter()
            .map(|e| e.fn_ref)
            .collect())
    }

    /// 收集关系依赖并注入 AST，返回 `{relDeps, ast, injectInfo}`
    #[napi]
    pub fn inject_depends(&self, gql: String) -> Result<Value> {
        let mut ast = parse_gql(&gql).map_err(err)?;
        let schema = self.core.get(&ast.model).map_err(err)?;

        let deps = collect_rel_deps(schema).map_err(err)?;
        let deps_value = Value::Array(deps.iter().map(|d| d.to_value()).collect());
        let info = merge_depends_into_ast(&mut ast.relations, schema).map_err(err)?;

        Ok(json!({
            "relDeps": deps_value,
            "ast": ast.to_value(),
            "injectInfo": info.to_value(),
        }))
    }

    /// 剥离 asyncFn 依赖注入的字段，返回 `{items}`
    #[napi]
    pub fn strip_dep_injected(&self, inject_info: Value, items: Vec<Value>) -> Result<Value> {
        let info = InjectInfo::from_value(&inject_info);
        let mut items = items;
        core_strip_dep_injected(&mut items, &info);
        Ok(json!({ "items": items }))
    }

    /// finalize 第一阶段：逐条 `process_node`，返回 `{items, fnRefs}`
    #[napi]
    pub fn prepare_query(
        &self,
        env: Env,
        postprocess: Value,
        items: Vec<Value>,
        ctx: Option<Value>,
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        let bridge = self.bridge(&env);
        let mut items = items;

        let fn_refs = core_prepare_query(
            &postprocess,
            &mut items,
            &self.core,
            Some(&bridge),
            context.as_ref(),
        )
        .map_err(err)?;

        Ok(json!({ "items": items, "fnRefs": fn_refs }))
    }

    /// finalize 第三阶段：剥离 asyncFn 依赖注入的字段，返回 `{items}`
    #[napi]
    pub fn strip_query(&self, postprocess: Value, items: Vec<Value>) -> Result<Value> {
        let mut items = items;
        core_strip_query(&postprocess, &mut items);
        Ok(json!({ "items": items }))
    }
}
