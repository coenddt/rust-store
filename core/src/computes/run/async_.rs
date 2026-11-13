//! asyncFn 计算列的选取与批量执行（同步侧只做声明收集，执行交由 Host / 批量入口）。

use serde_json::Value;

use crate::permission::{evaluate, Context, Doc, RoleRules};
use crate::schema::Schema;

use super::super::cache::{ensure_cache, ComputeEntry};
use super::super::registry::FnRegistry;

/// 计算需执行的 asyncFn 计算列（带权限裁剪），但不执行——供 Host 异步桥接自行调用
///
/// `requested` 是本次查询显式请求的字段列表（查询 AST 的 `fields`）：只有 `requested`
/// 包含计算列名时该 asyncFn 才入选。原实现无此过滤——任何查询都会执行模型全部
/// asyncFn（IO 型批量，白付开销），且 Host 执行后把值写进 items、发生在
/// `process_node` 裁剪之后无人剥离，未请求字段会泄漏进返回结果（GQL 投影语义被破坏）。
///
/// 无 ctx 时请求命中的全部执行（与 JS 一致：权限过滤仅在 ctx 存在时生效）；
/// 有 ctx 时 `comp.read` 校验不过的 asyncFn 被跳过。
pub fn select_async_fns(
    rules: &RoleRules,
    schema: &Schema,
    ctx: Option<&Context>,
    requested: &[String],
) -> Vec<ComputeEntry> {
    let cache = ensure_cache(schema);
    if cache.async_fn_list.is_empty() {
        return Vec::new();
    }

    let mut selected: Vec<ComputeEntry> = Vec::new();
    for entry in &cache.async_fn_list {
        if !requested.iter().any(|f| f == &entry.key) {
            continue;
        }
        match ctx {
            None => selected.push(entry.clone()),
            Some(c) => {
                let rl = schema
                    .compute(&entry.key)
                    .and_then(|comp| comp.read.as_ref());
                match rl {
                    Some(roles) => {
                        if evaluate(
                            rules,
                            Some(c),
                            Some(roles),
                            Doc::Missing,
                            schema.owner_key(),
                        ) {
                            selected.push(entry.clone());
                        }
                    }
                    None => selected.push(entry.clone()),
                }
            }
        }
    }
    selected
}

/// 执行 asyncFn 计算列（批量，带权限裁剪），对应 JS `_runAsyncFns`
///
/// `requested` 语义同 [`select_async_fns`]：只执行本次查询显式请求的计算列。
/// 无 ctx 时请求命中的全部执行（与 JS 一致：权限过滤仅在 ctx 存在时生效）；
/// 有 ctx 时 `comp.read` 校验不过的 asyncFn 被跳过。
pub fn run_async_fns(
    items: &mut [Value],
    rules: &RoleRules,
    schema: &Schema,
    ctx: Option<&Context>,
    fn_registry: Option<&dyn FnRegistry>,
    requested: &[String],
) -> Result<(), String> {
    if items.is_empty() {
        return Ok(());
    }

    for entry in select_async_fns(rules, schema, ctx, requested) {
        match fn_registry {
            Some(r) => r.call_async(&entry.fn_ref, items, ctx)?,
            None => return Err(format!("计算列 {} 未注册异步实现", entry.key)),
        }
    }
    Ok(())
}
