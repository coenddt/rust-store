//! Phase 5（跨库联邦）对拍测试
//!
//! 读 `fixtures/federation/cases.json`（输入）与 `fixtures/federation/expected.json`
//! （黄金基准），按 `kind` 分派：
//!   - `plan`  : `plan_federated` 的结构性摘要（`sources` 的
//!     key/source/model/mode、`join.edges` 全字段、`degraded` 的 code 列表、
//!     `postprocess` 是否存在）。Command 序列本身由 A5/A6 端到端用例覆盖，
//!     这里只钉「拆源结果」，避免把整段 pipeline 钉死在 fixture。
//!   - `merge` : `merge_federated` 的合并结果（逐字深比较）。
//!
//! 另外两条契约在代码里直接校验（不适合放进 JSON fixture）：
//!   - 单源/跨源用例的 `postprocess` 必须与单库 `plan_query` **完全同形状**
//!     （由用例里的 `parity_with_query` 开启）；
//!   - 单源结果行数超过 `MAX_FEDERATION_ROWS` 必须报错（拒绝静默全表拉取）。

use std::fs;
use std::path::PathBuf;

use serde_json::{json, Map, Value};

use rust_store_core::command::plan_query;
use rust_store_core::federation::{merge_federated, plan_federated, MAX_FEDERATION_ROWS};
use rust_store_core::permission::{context_from_value, Context};
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

fn ctx_of(fx: &Value) -> Option<Context> {
    match fx.get("context") {
        None | Some(Value::Null) => None,
        Some(v) => context_from_value(v),
    }
}

/// 把联邦计划投影成「结构性摘要」：只保契约关心的字段
fn project_plan(plan: &Value) -> Value {
    let sources: Vec<Value> = plan
        .get("sources")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|s| {
                    json!({
                        "key": s.get("key").cloned().unwrap_or(Value::Null),
                        "source": s.get("source").cloned().unwrap_or(Value::Null),
                        "namespace": s.get("namespace").cloned().unwrap_or(Value::Null),
                        "model": s.get("model").cloned().unwrap_or(Value::Null),
                        "mode": s.get("mode").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let edges = plan
        .get("join")
        .and_then(|j| j.get("edges"))
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default();

    let degraded: Vec<Value> = plan
        .get("degraded")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|d| d.get("code").cloned()).collect())
        .unwrap_or_default();

    json!({
        "v": plan.get("v").cloned().unwrap_or(Value::Null),
        "kind": plan.get("kind").cloned().unwrap_or(Value::Null),
        "root": plan.get("root").cloned().unwrap_or(Value::Null),
        "sources": sources,
        "edges": edges,
        "degraded": degraded,
        "hasPostprocess": !plan.get("postprocess").map(|p| p.is_null()).unwrap_or(true),
    })
}

fn run_plan(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let params = params_map(fx);
    let ctx = ctx_of(fx);
    let plan = plan_federated(
        gql_of(fx),
        &params,
        &registry,
        ctx.as_ref(),
        fx.get("dsConfig").unwrap_or(&Value::Null),
    )?;

    // 契约：postprocess 必须与单库 plan_query 同形状（联邦只在「取数 AST」上拆源，
    // 后处理 AST 始终是含全部关系的完整快照）
    if fx
        .get("parity_with_query")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let single = plan_query(gql_of(fx), &params, &registry, ctx.as_ref())?;
        let want = single.postprocess.clone().unwrap_or(Value::Null);
        let got = plan.get("postprocess").cloned().unwrap_or(Value::Null);
        if want != got {
            return Err(format!(
                "postprocess 与单库 plan_query 不一致\n  want: {}\n  got : {}",
                serde_json::to_string(&want).unwrap(),
                serde_json::to_string(&got).unwrap(),
            ));
        }
    }

    Ok(project_plan(&plan))
}

fn run_merge(fx: &Value) -> Result<Value, String> {
    let plan = fx.get("plan").cloned().unwrap_or(Value::Null);
    let results = fx
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    merge_federated(&plan, &results)
}

fn run_case(fx: &Value) -> Result<Value, String> {
    match fx.get("kind").and_then(|v| v.as_str()).unwrap_or("") {
        "plan" => run_plan(fx),
        "merge" => run_merge(fx),
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
fn parity_federation_with_reference() {
    let input = load(&fixtures_dir().join("federation").join("cases.json"));
    let expected = load(&fixtures_dir().join("federation").join("expected.json"));

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
            "parity_federation 对拍失败 {} 项:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}

/// 单源结果行数超上限必须显式报错（拒绝静默全表拉取）
#[test]
fn merge_rejects_oversized_source() {
    let plan = json!({
        "sources": [
            { "key": "0", "source": "mongodb_main", "model": "User" },
            { "key": "1", "source": "mysql_main", "model": "Order" }
        ],
        "join": {
            "type": "hash",
            "edges": [
                { "parent": "User", "rel": "orders", "local": "_id", "foreign": "userId", "cardinality": "many", "path": [], "key": "1" }
            ]
        }
    });

    let oversized: Vec<Value> = (0..=MAX_FEDERATION_ROWS)
        .map(|i| json!({ "_id": format!("o{}", i), "userId": "u1" }))
        .collect();
    let results = vec![json!([{ "_id": "u1" }]), Value::Array(oversized)];

    let err = merge_federated(&plan, &results).expect_err("超上限应报错");
    assert!(
        err.contains("超过联邦内存 join 上限"),
        "错误信息异常: {}",
        err
    );
}
