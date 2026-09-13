//! Phase 2（Command 序列契约）parity 对拍测试
//!
//! 读 `fixtures/commands/cases.json`（输入）与 `fixtures/commands/expected.json`
//! （JS 黄金基准：用 mock 驱动记录真实 driver 调用后归一化），用 Rust core 按 `kind`
//! 分派跑同一批输入，逐条深比较：
//!   - query              : `plan_query` 的命令序列 + mode + postprocess
//!   - query_with_count   : `plan_query_with_count`（另含 countCommand / page / hasMore）
//!   - resolve_page       : `resolve_page`
//!   - restore_sort_order : `restore_sort_order`
//!   - insert             : `plan_insert`（`now` / `newId` 由 Host 提供）
//!   - exists             : `plan_exists`
//!   - count              : `plan_count`
//!   - aggregate          : `plan_aggregate`
//!
//! 黄金基准为**冻结快照**（原单体 JS 参考实现已随重构退役，快照无源可再生）。
//! 复算校验：`node tools/verify-fixtures.js`。

use std::fs;
use std::path::PathBuf;

use serde_json::{json, Map, Value};

use rust_store_core::command::{
    plan_aggregate, plan_count, plan_exists, plan_insert, plan_query, plan_query_with_count,
    resolve_page, restore_sort_order,
};
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

fn params_map(fx: &Value) -> Map<String, Value> {
    fx.get("params")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

fn gql_of(fx: &Value) -> &str {
    fx.get("gql").and_then(|v| v.as_str()).unwrap_or("")
}

fn model_of(fx: &Value) -> &str {
    fx.get("model").and_then(|v| v.as_str()).unwrap_or("")
}

fn ctx_of(fx: &Value) -> Option<rust_store_core::permission::Context> {
    match fx.get("context") {
        None | Some(Value::Null) => None,
        Some(v) => context_from_value(v),
    }
}

fn run_query(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let params = params_map(fx);
    let ctx = ctx_of(fx);
    let plan = plan_query(gql_of(fx), &params, &registry, ctx.as_ref())?;
    Ok(plan.to_value())
}

fn run_query_with_count(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let params = params_map(fx);
    let ctx = ctx_of(fx);
    let plan = plan_query_with_count(gql_of(fx), &params, &registry, ctx.as_ref())?;

    let total = fx.get("total").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let mut out = plan.to_value().as_object().cloned().unwrap_or_default();
    out.insert("hasMore".to_string(), json!(plan.has_more(total)));
    Ok(Value::Object(out))
}

fn run_resolve_page(fx: &Value) -> Result<Value, String> {
    let ast = parse_gql(gql_of(fx))?;
    Ok(resolve_page(&ast, &params_map(fx)).to_value())
}

fn run_restore_sort_order(fx: &Value) -> Result<Value, String> {
    let mut items = fx
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let ids = fx
        .get("ids")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let sort = fx.get("sort").cloned().unwrap_or(Value::Null);
    let sort_ref = if sort.is_null() { None } else { Some(&sort) };

    restore_sort_order(&mut items, &ids, sort_ref);
    Ok(json!({ "items": items }))
}

/// `newId` 由 Host 提供（core 无随机源），对齐生成器里打桩后的 `_generateId` 结果
fn run_insert(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let ctx = ctx_of(fx);
    let data = fx.get("data").cloned().unwrap_or(Value::Null);
    let now = fx.get("now").and_then(|v| v.as_i64()).unwrap_or(0);
    let new_id = fx.get("newId").and_then(|v| v.as_str()).unwrap_or("");

    let out = plan_insert(
        model_of(fx),
        &registry,
        ctx.as_ref(),
        &data,
        now,
        new_id,
        None,
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

fn run_exists(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let condition = fx.get("condition").cloned().unwrap_or_else(|| json!({}));
    let command = plan_exists(model_of(fx), &registry, &condition)?;
    let found = fx.get("foundDoc").map(|v| !v.is_null()).unwrap_or(false);
    let collection = command.get("collection").cloned().unwrap_or(Value::Null);
    Ok(json!({
        "command": command,
        "found": found,
        "collections": [collection],
    }))
}

fn run_count(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let filter = fx.get("filter").filter(|v| !v.is_null()).cloned();
    let command = plan_count(model_of(fx), &registry, filter.as_ref(), None)?;
    Ok(json!({ "command": command }))
}

fn run_aggregate(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let pipeline = fx
        .get("pipeline")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let command = plan_aggregate(model_of(fx), &registry, &pipeline, None)?;
    Ok(json!({ "command": command }))
}

fn run_case(fx: &Value) -> Result<Value, String> {
    match fx.get("kind").and_then(|v| v.as_str()).unwrap_or("") {
        "query" => run_query(fx),
        "query_with_count" => run_query_with_count(fx),
        "resolve_page" => run_resolve_page(fx),
        "restore_sort_order" => run_restore_sort_order(fx),
        "insert" => run_insert(fx),
        "exists" => run_exists(fx),
        "count" => run_count(fx),
        "aggregate" => run_aggregate(fx),
        // 写路径用例归 parity_write 覆盖，此处跳过
        "insert_many" | "update" | "update_many" | "remove" | "upsert" | "mutation" => {
            Ok(Value::Null)
        }
        other => Err(format!("未知用例类型: {}", other)),
    }
}

/// 黄金基准形如 `{name, kind, result}`；报错用例则为 `{name, kind, error: true}`
fn golden_result(golden: &Value) -> Value {
    if golden
        .get("error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        json!({ "error": true })
    } else {
        golden.get("result").cloned().unwrap_or(Value::Null)
    }
}

#[test]
fn parity_commands_with_js_reference() {
    let input = load(&fixtures_dir().join("commands").join("cases.json"));
    let expected = load(&fixtures_dir().join("commands").join("expected.json"));

    let cases = input.as_array().expect("cases.json 应为数组");
    let goldens = expected.as_array().expect("expected.json 应为数组");
    assert_eq!(cases.len(), goldens.len(), "输入与黄金基准用例数不一致");

    let mut failures: Vec<String> = Vec::new();

    for (fx, golden) in cases.iter().zip(goldens.iter()) {
        let name = fx
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unnamed>");
        let gname = golden
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unnamed>");
        assert_eq!(name, gname, "用例顺序不一致");

        // 写路径用例归 parity_write 覆盖，此处整体跳过
        if matches!(
            fx.get("kind").and_then(|v| v.as_str()).unwrap_or(""),
            "insert_many" | "update" | "update_many" | "remove" | "upsert" | "mutation"
        ) {
            continue;
        }

        let expect_error = fx
            .get("expect_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let actual = match run_case(fx) {
            Ok(v) => {
                if expect_error {
                    failures.push(format!("[{}] 期望报错，但 Rust 未报错: {}", name, v));
                    continue;
                }
                v
            }
            Err(e) => {
                if expect_error {
                    json!({ "error": true })
                } else {
                    failures.push(format!("[{}] Rust 报错: {}", name, e));
                    continue;
                }
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
            "parity_commands 对拍失败 {} 项:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
