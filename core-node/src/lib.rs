//! mongo-store 的 Node 绑定（napi-rs）
//!
//! 把 Rust core 的 Command 契约暴露给 JS Host：JSON in / JSON out，不执行任何 IO。
//!
//! 设计边界（对齐方案 A 的切分）：
//! 1. core 不持有 MongoDB 驱动，本绑定层同样只产出「命令序列」与后处理结果；
//! 2. JS 函数无法经 `serde_json::Value` 传递（napi 遇到 Function 会报 `InvalidArg`），
//!    故**同步**计算列回调单独经 [`Registry::set_fn`] 注册，由 Rust 侧同步回调；
//! 3. **异步**计算列（`asyncFn`）无法被 Rust 同步等待，改由两段式承接：
//!    [`Registry::prepare_query`] 返回待执行 `fnRefs`（已做 read 权限过滤），
//!    JS Host 依次 `await` 后调 [`Registry::strip_query`]。
//!
//! 注意：返回类型**必须**写成 `napi::Result<T>`（不能用类型别名）。napi-derive 靠
//! 「路径末段 ident == `Result`」判定是否生成抛异常代码（见 napi-derive
//! `parser/mod.rs::extract_result_ty`）；一旦改成别名，`Err` 会被当作普通返回值
//! 经 `impl ToNapiValue for Error` 序列化成一个 `{code}` 对象返回给 JS，而非抛出。

use std::collections::{HashMap, HashSet};

use napi::bindgen_prelude::{Env, FunctionRef};
use napi::{Error, Result};
use napi_derive::napi;
use serde_json::{json, Map, Value};

use mongo_store_core::command::{
    plan_aggregate as core_plan_aggregate, plan_archive_docs as core_plan_archive_docs,
    plan_count as core_plan_count, plan_exists as core_plan_exists,
    plan_insert as core_plan_insert, plan_insert_many as core_plan_insert_many,
    plan_mutation as core_plan_mutation, plan_query as core_plan_query,
    plan_query_with_count as core_plan_query_with_count, plan_remove as core_plan_remove,
    plan_update as core_plan_update, plan_update_many as core_plan_update_many,
    plan_upsert as core_plan_upsert, prepare_query as core_prepare_query,
    resolve_page as core_resolve_page, restore_sort_order as core_restore_sort_order,
    sorts_by_relation as core_sorts_by_relation, strip_query as core_strip_query, Probe,
};
use mongo_store_core::computes::{
    apply_defaults_and_computes as core_apply_defaults, collect_rel_deps, merge_depends_into_ast,
    process_node, select_async_fns, strip_dep_injected as core_strip_dep_injected, FnRegistry,
    InjectInfo,
};
use mongo_store_core::dialect::{
    introspect_to_schema_json as core_introspect_to_schema_json,
    merge_schema as core_merge_schema, restore_rows_json as core_restore_rows_json,
    translate as core_dialect_translate, Backend,
};
use mongo_store_core::permission::{
    can_read_schema, can_write_schema, context_from_value, filter_writable_data,
    get_readable_fields, get_readable_relations, get_writable_fields, merge_owner_condition,
    should_inject_owner_condition, Context,
};
use mongo_store_core::pipeline::{build_pipeline, build_projection, parse_gql, token_to_value, tokenize};
use mongo_store_core::schema::Registry as CoreRegistry;

fn err(reason: String) -> Error {
    Error::from_reason(reason)
}

/// `Option<HashSet<String>>` → `null` / 排序数组（保证跨语言比较稳定）
fn sorted_set(set: Option<HashSet<String>>) -> Value {
    match set {
        None => Value::Null,
        Some(set) => {
            let mut v: Vec<String> = set.into_iter().collect();
            v.sort();
            Value::Array(v.into_iter().map(Value::String).collect())
        }
    }
}

/// 把 JS 注册的同步计算列回调适配成 core 的 [`FnRegistry`]
struct SyncFnBridge<'a> {
    env: &'a Env,
    fns: &'a HashMap<String, FunctionRef<Value, Value>>,
}

impl FnRegistry for SyncFnBridge<'_> {
    fn call_sync(&self, fn_ref: &str, doc: &Value) -> std::result::Result<Value, String> {
        let f = self
            .fns
            .get(fn_ref)
            .ok_or_else(|| format!("计算列 {} 未注册同步实现", fn_ref))?;
        let func = f.borrow_back(self.env).map_err(|e| e.to_string())?;
        func.call(doc.clone()).map_err(|e| e.to_string())
    }

    fn call_async(
        &self,
        fn_ref: &str,
        _items: &mut [Value],
        _ctx: Option<&Context>,
    ) -> std::result::Result<(), String> {
        // 异步回调由 Host 执行，core 内不会触发该分支
        Err(format!(
            "异步计算列 {} 需由 Host 执行（见 prepareQuery 返回的 fnRefs）",
            fn_ref
        ))
    }
}

#[napi]
pub struct Registry {
    core: CoreRegistry,
    sync_fns: HashMap<String, FunctionRef<Value, Value>>,
}

impl Registry {
    fn bridge<'a>(&'a self, env: &'a Env) -> SyncFnBridge<'a> {
        SyncFnBridge {
            env,
            fns: &self.sync_fns,
        }
    }

    fn params_map(params: &Value) -> Map<String, Value> {
        params.as_object().cloned().unwrap_or_default()
    }
}

#[napi]
impl Registry {
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            core: CoreRegistry::new(),
            sync_fns: HashMap::new(),
        }
    }

    /// 注册 schema（自动派生 `<Name>Deleted` 归档表；`timestamps !== false` 时补时间戳字段）
    #[napi]
    pub fn register(&mut self, defn: Value) -> Result<()> {
        self.core.register(&defn).map_err(err)
    }

    #[napi]
    pub fn has(&self, name: String) -> bool {
        self.core.has(&name)
    }

    #[napi]
    pub fn list(&self) -> Vec<String> {
        self.core.list()
    }

    /// 注册同步计算列回调（schema 里 `fn: true` 的 `fnRef`，缺省为计算列名）
    #[napi]
    pub fn set_fn(&mut self, fn_ref: String, callback: FunctionRef<Value, Value>) {
        self.sync_fns.insert(fn_ref, callback);
    }

    #[napi]
    pub fn clear_fns(&mut self) {
        self.sync_fns.clear();
    }

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
    pub fn plan_query(&self, gql: String, params: Value, ctx: Option<Value>) -> Result<Value> {
        let params = Self::params_map(&params);
        let context = ctx.as_ref().and_then(context_from_value);
        core_plan_query(&gql, &params, &self.core, context.as_ref())
            .map(|p| p.to_value())
            .map_err(err)
    }

    /// 列表 + total；`total` 由 Host 执行 `countCommand` 后回喂，用于算 `hasMore`
    #[napi]
    pub fn plan_query_with_count(
        &self,
        gql: String,
        params: Value,
        ctx: Option<Value>,
        total: Option<f64>,
    ) -> Result<Value> {
        let params = Self::params_map(&params);
        let context = ctx.as_ref().and_then(context_from_value);
        let plan = core_plan_query_with_count(&gql, &params, &self.core, context.as_ref()).map_err(err)?;

        let mut out = plan.to_value().as_object().cloned().unwrap_or_default();
        out.insert(
            "hasMore".to_string(),
            json!(plan.has_more(total.unwrap_or(0.0))),
        );
        Ok(Value::Object(out))
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
    #[napi]
    pub fn plan_insert(
        &self,
        env: Env,
        model: String,
        data: Value,
        now: i64,
        new_id: String,
        ctx: Option<Value>,
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        let bridge = self.bridge(&env);
        core_plan_insert(
            &model,
            &self.core,
            context.as_ref(),
            &data,
            now,
            &new_id,
            Some(&bridge),
        )
        .map_err(err)
    }

    #[napi]
    pub fn plan_exists(&self, model: String, condition: Value) -> Result<Value> {
        core_plan_exists(&model, &self.core, &condition).map_err(err)
    }

    #[napi]
    pub fn plan_count(&self, model: String, filter: Option<Value>) -> Result<Value> {
        let filter = filter.as_ref().filter(|v| !v.is_null());
        core_plan_count(&model, &self.core, filter).map_err(err)
    }

    #[napi]
    pub fn plan_aggregate(&self, model: String, pipeline: Vec<Value>) -> Result<Value> {
        core_plan_aggregate(&model, &self.core, &pipeline).map_err(err)
    }

    // ─── Phase 2.5：写路径命令规划 ─────────────────────────

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

    /// 批量插入命令；`newIds` 按需消费（仅无 `_id` 的文档取用）
    #[napi]
    pub fn plan_insert_many(
        &self,
        env: Env,
        model: String,
        docs: Vec<Value>,
        now: i64,
        new_ids: Vec<String>,
        ctx: Option<Value>,
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        let bridge = self.bridge(&env);
        core_plan_insert_many(
            &model,
            &self.core,
            context.as_ref(),
            &docs,
            now,
            &new_ids,
            Some(&bridge),
        )
        .map_err(err)
    }

    /// 更新一条（findOneAndUpdate + returnDocument AFTER）。
    ///
    /// creator 写权限需探针时返回 `{"needsProbe": cmd}`；Host 执行探针后携
    /// `probeFound`（true/false）与 `probeDoc` 重入即得 `{"command": cmd}`。
    #[napi]
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
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        core_plan_update(
            &model,
            &self.core,
            context.as_ref(),
            &condition,
            &data,
            &options.unwrap_or(Value::Null),
            now,
            Self::probe_of(probe_found, probe_doc.as_ref()),
        )
        .map_err(err)
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
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        core_plan_update_many(&model, &self.core, context.as_ref(), &condition, &data, now)
            .map_err(err)
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
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        core_plan_remove(
            &model,
            &self.core,
            context.as_ref(),
            &condition,
            Self::probe_of(probe_found, probe_doc.as_ref()),
        )
        .map_err(err)
    }

    /// 归档文档命令：源文档补 `deletedAt` 后批量写入 `<collection>_deleted`
    #[napi]
    pub fn plan_archive_docs(&self, model: String, docs: Vec<Value>, now: i64) -> Result<Value> {
        core_plan_archive_docs(&model, &self.core, &docs, now).map_err(err)
    }

    /// 显式条件 upsert；`newId` 仅在需生成 `_id` 时被使用
    #[napi]
    pub fn plan_upsert(
        &self,
        model: String,
        condition: Value,
        data: Value,
        options: Option<Value>,
        now: i64,
        new_id: String,
        ctx: Option<Value>,
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        core_plan_upsert(
            &model,
            &self.core,
            context.as_ref(),
            &condition,
            &data,
            &options.unwrap_or(Value::Null),
            now,
            &new_id,
        )
        .map_err(err)
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
    ) -> Result<Value> {
        let context = ctx.as_ref().and_then(context_from_value);
        core_plan_mutation(&model, &self.core, context.as_ref(), &data, now, &new_ids)
            .map_err(err)
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

    // ─── Phase 3：计算列 / 依赖注入 / 结果回喂 ─────────────

    /// 逐条后处理文档（默认值 → 同步 fn → 递归下钻 → 权限裁剪），返回 `{doc}`
    #[napi]
    pub fn process_node(&self, env: Env, gql: String, doc: Value, ctx: Option<Value>) -> Result<Value> {
        let mut ast = parse_gql(&gql).map_err(err)?;
        let schema = self.core.get(&ast.model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        let bridge = self.bridge(&env);
        let mut doc = doc;

        process_node(
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

    // ─── 权限 ─────────────────────────────────────────────

    #[napi]
    pub fn can_read(&self, model: String, ctx: Option<Value>) -> Result<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(can_read_schema(schema, context.as_ref()))
    }

    #[napi]
    pub fn can_write(&self, model: String, ctx: Option<Value>) -> Result<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(can_write_schema(schema, context.as_ref()))
    }

    #[napi]
    pub fn should_inject_owner(&self, model: String, ctx: Option<Value>) -> Result<bool> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(should_inject_owner_condition(schema, context.as_ref()))
    }

    /// 非 admin 用户只看自己数据时叠加 owner 条件；无上下文时原样返回
    #[napi]
    pub fn merge_owner_condition(
        &self,
        model: String,
        ctx: Option<Value>,
        condition: Option<Value>,
    ) -> Result<Value> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(merge_owner_condition(schema, context.as_ref(), condition).unwrap_or(Value::Null))
    }

    #[napi]
    pub fn readable_fields(&self, model: String, ctx: Option<Value>) -> Result<Value> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(sorted_set(get_readable_fields(schema, context.as_ref())))
    }

    #[napi]
    pub fn readable_relations(&self, model: String, ctx: Option<Value>) -> Result<Value> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(sorted_set(get_readable_relations(schema, context.as_ref())))
    }

    #[napi]
    pub fn writable_fields(&self, model: String, ctx: Option<Value>) -> Result<Value> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(sorted_set(get_writable_fields(schema, context.as_ref())))
    }

    #[napi]
    pub fn filter_writable_data(
        &self,
        model: String,
        ctx: Option<Value>,
        data: Value,
    ) -> Result<Value> {
        let schema = self.core.get(&model).map_err(err)?;
        let context = ctx.as_ref().and_then(context_from_value);
        Ok(filter_writable_data(schema, context.as_ref(), &data))
    }

    // ─── dialect：Mongo 命令 → 关系型 SQL ─────────────

    /// 把一条 Mongo 命令 JSON 翻译为指定后端的 SQL 语句序列（见 `translate::translate`）。
    #[napi]
    pub fn dialect_translate(&self, backend: String, cmd: Value) -> Result<Value> {
        let backend = Backend::parse(&backend).map_err(err)?;
        core_dialect_translate(backend, &cmd, &self.core).map_err(err)
    }

    /// 把 `{rowShape, rows}` 还原为嵌套 Mongo 文档数组（平铺 JOIN 行 → 文档）
    #[napi]
    pub fn restore_rows(&self, shape: Value, rows: Value) -> Result<Value> {
        core_restore_rows_json(&shape, &rows).map_err(err)
    }

    /// 把 introspection 行 JSON → schemaJSON（`{rows}` 内含 tables/columns/fks/indexes）
    #[napi]
    pub fn schema_from_rows(&self, rows: Value, backend: String) -> Result<Value> {
        let backend = Backend::parse(&backend).map_err(err)?;
        core_introspect_to_schema_json(&rows, &backend).map_err(err)
    }

    /// 合并 introspected 基础 schema 与本地 overlay（权限 / 计算列 / 覆盖）
    #[napi]
    pub fn merge_schema(&self, base: Value, overlay: Value) -> Result<Value> {
        core_merge_schema(&base, &overlay).map_err(err)
    }
}
