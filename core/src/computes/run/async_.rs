//! asyncFn 计算列的选取与批量执行（同步侧只做声明收集，执行交由 Host / 批量入口）。

use serde_json::Value;

use crate::permission::{evaluate, Context, Doc};
use crate::schema::Schema;

use super::super::cache::{ensure_cache, ComputeEntry};
use super::super::registry::FnRegistry;

/// 计算需执行的 asyncFn 计算列（带权限裁剪），但不执行——供 Host 异步桥接自行调用
///
/// 无 ctx 时全部执行（与 JS 一致：权限过滤仅在 ctx 存在时生效）；
/// 有 ctx 时 `comp.read` 校验不过的 asyncFn 被跳过。
pub fn select_async_fns(schema: &Schema, ctx: Option<&Context>) -> Vec<ComputeEntry> {
    let cache = ensure_cache(schema);
    if cache.async_fn_list.is_empty() {
        return Vec::new();
    }

    let mut selected: Vec<ComputeEntry> = Vec::new();
    match ctx {
        None => selected.extend(cache.async_fn_list.iter().cloned()),
        Some(c) => {
            for entry in &cache.async_fn_list {
                let rl = schema
                    .compute(&entry.key)
                    .and_then(|comp| comp.read.as_ref());
                match rl {
                    Some(roles) => {
                        if evaluate(Some(c), Some(roles), Doc::Missing) {
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
/// 无 ctx 时全部执行（与 JS 一致：权限过滤仅在 ctx 存在时生效）；
/// 有 ctx 时 `comp.read` 校验不过的 asyncFn 被跳过。
pub fn run_async_fns(
    items: &mut [Value],
    schema: &Schema,
    ctx: Option<&Context>,
    fn_registry: Option<&dyn FnRegistry>,
) -> Result<(), String> {
    if items.is_empty() {
        return Ok(());
    }

    for entry in select_async_fns(schema, ctx) {
        match fn_registry {
            Some(r) => r.call_async(&entry.fn_ref, items, ctx)?,
            None => return Err(format!("计算列 {} 未注册异步实现", entry.key)),
        }
    }
    Ok(())
}
