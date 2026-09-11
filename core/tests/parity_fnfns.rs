//! Phase 3（fnRef 回调桥）parity 对拍测试
//!
//! 读 `fixtures/fnfns/cases.json`（输入）与 `fixtures/fnfns/expected.json`
//! （JS 黄金基准**冻结快照**，原 JS 实现已退役、快照无源可再生），用 Rust core 按
//! `kind` 分派跑同一批输入，逐条深比较：
//!   - process_node : `computes::process_node`（同步 fn 计算列内联执行）
//!   - async_fns    : `computes::run_async_fns`（批量异步 + 权限过滤）
//!   - postprocess  : `command::finalize_query`（processNode → asyncFns → strip 注入）
//!   - insert       : `command::plan_insert`（fn 只作用于 returns）
//!
//! fn 执行体经 [`common::TestFnRegistry`] 注入（与 JS 侧 tools/test-fns.js 同语义）。

mod common;

use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use common::TestFnRegistry;

use rust_store_core::command::{finalize_query, plan_insert};
use rust_store_core::computes::{merge_depends_into_ast, process_node, run_async_fns};
use rust_store_core::permission::context_from_value;
use rust_store_core::pipeline::parse_gql;
use rust_store_core::schema::Registry;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core 目录应有上级目录")
        .join("fixtures")
}

fn load(path: &PathBuf) -> Value {
    let text =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("读取 {} 失败: {}", path.display(), e));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("解析 {} 失败: {}", path.display(), e))
}

fn build_registry(fx: &Value) -> Result<Registry, String> {
    let mut registry = Registry::new();
    if let Some(schemas) = fx.get("schemas").and_then(|v| v.as_array()) {
        for s in schemas {
            registry.register(s)?;
        }
    }
    Ok(registry)
}

fn ctx_of(fx: &Value) -> Option<rust_store_core::permission::Context> {
    match fx.get("context") {
        None | Some(Value::Null) => None,
        Some(v) => context_from_value(v),
    }
}

fn run_process_node(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let mut ast = parse_gql(fx.get("gql").and_then(|v| v.as_str()).unwrap_or(""))?;
    let schema = registry.get(&ast.model)?;
    let ctx = ctx_of(fx);
    let mut doc = fx.get("doc").cloned().unwrap_or(Value::Null);

    process_node(
        &mut doc,
        &mut ast.fields,
        &mut ast.relations,
        schema,
        ctx.as_ref(),
        &registry,
        Some(&TestFnRegistry),
    )?;
    Ok(json!({ "doc": doc }))
}

fn run_async_fns_case(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let model = fx.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let schema = registry.get(model)?;
    let ctx = ctx_of(fx);
    let mut items: Vec<Value> = fx
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    run_async_fns(&mut items, schema, ctx.as_ref(), Some(&TestFnRegistry))?;
    Ok(json!({ "items": items }))
}

fn run_postprocess(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let mut ast = parse_gql(fx.get("gql").and_then(|v| v.as_str()).unwrap_or(""))?;
    let schema = registry.get(&ast.model)?;
    let ctx = ctx_of(fx);

    // 对齐 crud.query：先注入 asyncFn 依赖，再取 postprocess 信息
    let inject = merge_depends_into_ast(&mut ast.relations, schema)?;
    let pp = json!({
        "ast": ast.to_value(),
        "inject": if inject.is_empty() { Value::Null } else { inject.to_value() },
    });

    let mut items: Vec<Value> = fx
        .get("docs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    finalize_query(&pp, &mut items, &registry, Some(&TestFnRegistry), ctx.as_ref())?;
    Ok(json!({ "items": items }))
}

fn run_insert(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let ctx = ctx_of(fx);
    let model = fx.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let data = fx.get("data").cloned().unwrap_or(Value::Null);
    let now = fx.get("now").and_then(|v| v.as_i64()).unwrap_or(0);
    // Host 提供 newId（core 无随机源），对齐生成器打桩后的 `_generateId` 结果
    let new_id = fx.get("newId").and_then(|v| v.as_str()).unwrap_or("");

    let out = plan_insert(
        model,
        &registry,
        ctx.as_ref(),
        &data,
        now,
        new_id,
        Some(&TestFnRegistry),
    )?;
    let command = out.get("command").cloned().unwrap_or(Value::Null);
    let returns = out.get("returns").cloned().unwrap_or(Value::Null);
    let new_id = command
        .get("doc")
        .and_then(|d| d.get("_id"))
        .cloned()
        .unwrap_or(Value::Null);

    Ok(json!({ "command": command, "returns": returns, "newId": new_id }))
}

fn run_case(fx: &Value) -> Result<Value, String> {
    match fx.get("kind").and_then(|v| v.as_str()).unwrap_or("") {
        "process_node" => run_process_node(fx),
        "async_fns" => run_async_fns_case(fx),
        "postprocess" => run_postprocess(fx),
        "insert" => run_insert(fx),
        other => Err(format!("未知用例类型: {}", other)),
    }
}

/// 黄金基准形如 `{name, kind, result}`；报错用例则为 `{name, kind, error: true}`
fn golden_result(golden: &Value) -> Value {
    if golden.get("error").and_then(|v| v.as_bool()).unwrap_or(false) {
        json!({ "error": true })
    } else {
        golden.get("result").cloned().unwrap_or(Value::Null)
    }
}

#[test]
fn parity_fnfns_with_js_reference() {
    let input = load(&fixtures_dir().join("fnfns").join("cases.json"));
    let expected = load(&fixtures_dir().join("fnfns").join("expected.json"));

    let cases = input.as_array().expect("cases.json 应为数组");
    let goldens = expected.as_array().expect("expected.json 应为数组");
    assert_eq!(cases.len(), goldens.len(), "输入与黄金基准用例数不一致");

    let mut failures: Vec<String> = Vec::new();

    for (fx, golden) in cases.iter().zip(goldens.iter()) {
        let name = fx.get("name").and_then(|v| v.as_str()).unwrap_or("<unnamed>");
        let gname = golden
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unnamed>");
        assert_eq!(name, gname, "用例顺序不一致");

        let actual = match run_case(fx) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("[{}] Rust 报错: {}", name, e));
                continue;
            }
        };
        let want = golden_result(golden);

        if actual != want {
            failures.push(format!(
                "[{}] 结果不一致\n  want: {}\n  got : {}",
                name,
                serde_json::to_string(&want).unwrap(),
                serde_json::to_string(&actual).unwrap(),
            ));
        }
    }

    if !failures.is_empty() {
        panic!(
            "parity_fnfns 对拍失败 {} 项:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
