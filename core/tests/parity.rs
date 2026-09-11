//! Phase 1 parity 对拍测试
//!
//! 读 `fixtures/pipeline/cases.json`（输入）与 `fixtures/expected/cases.json`（JS 黄金基准），
//! 用 Rust core 跑同一批输入，逐条深比较 `tokens` / `ast` / `pipeline` / `projection`。
//! 黄金基准由 `node tools/gen-fixtures.js` 从现有 JS 实现生成。

use std::fs;
use std::path::PathBuf;

use serde_json::Value;

use rust_store_core::permission::context_from_value;
use rust_store_core::pipeline::{build_pipeline, build_projection, parse, token_to_value, tokenize};
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

/// 跑单个用例，返回 (tokens, ast, pipeline, projection)；`Err` 等价 JS 抛异常
fn run_case(fx: &Value) -> Result<(Value, Value, Value, Value), String> {
    let mut registry = Registry::new();
    if let Some(schemas) = fx.get("schemas").and_then(|v| v.as_array()) {
        for s in schemas {
            registry.register(s)?;
        }
    }

    let gql = fx.get("gql").and_then(|v| v.as_str()).unwrap_or("");
    let params = fx
        .get("params")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let ctx = fx.get("context").and_then(context_from_value);

    let tokens = tokenize(gql);
    let tokens_value = Value::Array(tokens.iter().map(token_to_value).collect());

    // `build_pipeline` 会原地展平 ast，故 ast 黄金值须在展平前取
    let mut ast = parse(&tokens)?;
    let ast_value = ast.to_value();

    let schema = registry.get(&ast.model)?;
    let pipeline = build_pipeline(&mut ast, &params, &registry, ctx.as_ref())?;
    let projection = build_projection(&ast, schema, ctx.as_ref()).unwrap_or(Value::Null);

    Ok((tokens_value, ast_value, pipeline, projection))
}

#[test]
fn parity_with_js_reference() {
    let input = load(&fixtures_dir().join("pipeline").join("cases.json"));
    let expected = load(&fixtures_dir().join("expected").join("cases.json"));

    let cases = input.as_array().expect("cases.json 应为数组");
    let goldens = expected.as_array().expect("expected/cases.json 应为数组");
    assert_eq!(cases.len(), goldens.len(), "输入与黄金基准用例数不一致");

    let mut failures: Vec<String> = Vec::new();

    for (fx, golden) in cases.iter().zip(goldens.iter()) {
        let name = fx.get("name").and_then(|v| v.as_str()).unwrap_or("<unnamed>");
        let gname = golden
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unnamed>");
        assert_eq!(name, gname, "用例顺序不一致");

        let expect_error = fx
            .get("expect_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let result = run_case(fx);

        if expect_error {
            if result.is_ok() {
                failures.push(format!("[{}] 期望报错但成功返回", name));
            }
            continue;
        }

        let (tokens, ast, pipeline, projection) = match result {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("[{}] Rust 报错: {}", name, e));
                continue;
            }
        };

        for (label, actual) in [
            ("tokens", &tokens),
            ("ast", &ast),
            ("pipeline", &pipeline),
            ("projection", &projection),
        ] {
            let want = golden.get(label).cloned().unwrap_or(Value::Null);
            if actual != &want {
                failures.push(format!(
                    "[{}] {} 不一致\n  want: {}\n  got : {}",
                    name,
                    label,
                    serde_json::to_string(&want).unwrap(),
                    serde_json::to_string(actual).unwrap(),
                ));
            }
        }
    }

    if !failures.is_empty() {
        panic!(
            "parity 对拍失败 {} 项:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
