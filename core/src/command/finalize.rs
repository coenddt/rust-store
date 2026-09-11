//! 结果回喂：后处理 / asyncFn 桥 / 剥离注入

use serde_json::Value;

use crate::computes::{
    process_node, run_async_fns, select_async_fns, strip_dep_injected, FnRegistry, InjectInfo,
};
use crate::permission::Context;
use crate::pipeline::Ast;
use crate::schema::Registry;

/// 从 `postprocess` 还原查询 AST（`{ast, inject}` 形状）
fn postprocess_ast(postprocess: &Value) -> Result<Ast, String> {
    let ast_v = postprocess
        .get("ast")
        .ok_or_else(|| "postprocess 缺少 ast".to_string())?;
    Ast::from_value(ast_v)
}

/// finalize 第一阶段：逐条 `process_node`（补默认值 / 同步 fn / 递归下钻 / 权限裁剪），
/// 并返回**待 Host 执行的 asyncFn 回调标识**（`fnRef`，已做 read 权限过滤）。
///
/// 与 [`finalize_query`] 的差异：asyncFn 是宿主语言原生闭包，无法跨 FFI 执行，
/// 因此不在 core 内调用，而是把 `fnRef` 列表交给 Host，由其依次
/// `await jsFn(items, ctx)` 后再调 [`strip_query`]。
pub fn prepare_query(
    postprocess: &Value,
    items: &mut [Value],
    registry: &Registry,
    fn_registry: Option<&dyn FnRegistry>,
    ctx: Option<&Context>,
) -> Result<Vec<String>, String> {
    if postprocess.is_null() {
        return Ok(Vec::new());
    }
    let mut ast = postprocess_ast(postprocess)?;
    let schema = registry.get(&ast.model)?;

    // ① 逐条 process_node（同步 fn 计算列内联执行）
    for item in items.iter_mut() {
        process_node(
            item,
            &mut ast.fields,
            &mut ast.relations,
            schema,
            ctx,
            registry,
            fn_registry,
        )?;
    }

    // 返回 Host 需异步执行的回调标识
    Ok(select_async_fns(schema, ctx)
        .into_iter()
        .map(|e| e.fn_ref)
        .collect())
}

/// finalize 第三阶段：剥离 asyncFn 依赖注入的字段
pub fn strip_query(postprocess: &Value, items: &mut [Value]) {
    if postprocess.is_null() {
        return;
    }
    if let Some(inj_v) = postprocess.get("inject") {
        if !inj_v.is_null() {
            let info = InjectInfo::from_value(inj_v);
            strip_dep_injected(items, &info);
        }
    }
}

/// 读路径尾部后处理（对应 JS `_postprocess` 三段时序）
///
/// Host 执行完命令序列拿到原始文档后回喂 core：
///   1. 逐条 `process_node`（补默认值 → 同步 fn → 递归下钻 → 权限裁剪）
///   2. 批量执行 asyncFn 计算列（权限过滤）
///   3. 剥离 asyncFn 依赖注入的字段
///
/// `postprocess` 即 [`QueryPlan::to_value`] 里的 `postprocess` 字段
/// （`{ast, inject}` 形状；`null` 表示用户 `$pipeline` 直通，不做处理）。
///
/// 这是「core 内直接执行 asyncFn」的入口（Rust Host / 测试用）；
/// 跨 FFI 的绑定应改用 [`prepare_query`] + [`strip_query`] 两段式。
pub fn finalize_query(
    postprocess: &Value,
    items: &mut [Value],
    registry: &Registry,
    fn_registry: Option<&dyn FnRegistry>,
    ctx: Option<&Context>,
) -> Result<(), String> {
    if postprocess.is_null() {
        return Ok(());
    }

    // ① 逐条 process_node
    prepare_query(postprocess, items, registry, fn_registry, ctx)?;

    // ② 批量 asyncFn（权限过滤）
    let ast = postprocess_ast(postprocess)?;
    let schema = registry.get(&ast.model)?;
    run_async_fns(items, schema, ctx, fn_registry)?;

    // ③ 剥离依赖注入的字段
    strip_query(postprocess, items);
    Ok(())
}
