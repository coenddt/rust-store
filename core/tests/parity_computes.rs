//! Phase 2 parity 对拍测试
//!
//! 读 `fixtures/computes/cases.json`（输入）与 `fixtures/computes/expected.json`（JS 黄金基准），
//! 用 Rust core 按 `kind` 分派跑同一批输入，逐条深比较：
//!   - process_node   : `computes::process_node` 后处理后的文档
//!   - inject_depends : `collect_rel_deps` / `merge_depends_into_ast` / `strip_dep_injected`
//!   - permission     : `permission::*` 的读写过滤与 owner 条件注入
//! 黄金基准为**冻结快照**（原单体 JS 参考实现已随重构退役，快照无源可再生）。
//! 复算校验：`node tools/verify-fixtures.js`。

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use rust_store_core::computes::{
    collect_rel_deps, merge_depends_into_ast, process_node, strip_dep_injected,
};
use rust_store_core::permission::{
    can_read_schema, can_write_schema, context_from_value, filter_writable_data,
    get_readable_fields, get_readable_relations, get_writable_fields, merge_owner_condition,
    should_inject_owner_condition,
};
use rust_store_core::pipeline::{parse, tokenize};
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

/// `Set` → 排序数组；`None` → `null`（对齐生成器里的 `sortedArray`）
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

fn run_process_node(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let gql = fx.get("gql").and_then(|v| v.as_str()).unwrap_or("");
    let ctx = fx.get("context").and_then(context_from_value);

    let mut ast = parse(&tokenize(gql))?;
    let schema = registry.get(&ast.model)?;
    let mut doc = fx.get("doc").cloned().unwrap_or(Value::Null);

    process_node(
        &mut doc,
        &mut ast.fields,
        &mut ast.relations,
        schema,
        ctx.as_ref(),
        &registry,
        None,
    )?;
    Ok(doc)
}

fn run_inject_depends(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let gql = fx.get("gql").and_then(|v| v.as_str()).unwrap_or("");

    let mut ast = parse(&tokenize(gql))?;
    let schema = registry.get(&ast.model)?;

    let rel_deps = collect_rel_deps(schema)?;
    let rel_deps_value = Value::Array(rel_deps.iter().map(|d| d.to_value()).collect());

    let info = merge_depends_into_ast(&mut ast.relations, schema)?;

    // `items` 缺席（用例无 items）→ stripped 为 null
    let mut stripped = Value::Null;
    if let Some(arr) = fx.get("items").and_then(|v| v.as_array()) {
        let mut items = Value::Array(arr.clone());
        strip_dep_injected(items.as_array_mut().unwrap(), &info);
        stripped = items;
    }

    Ok(json!({
        "relDeps": rel_deps_value,
        "ast": ast.to_value(),
        "injectInfo": info.to_value(),
        "stripped": stripped,
    }))
}

fn run_permission(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let model = fx.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let schema = registry.get(model)?;
    let ctx = fx.get("context").and_then(context_from_value);

    let condition = fx.get("condition").cloned();
    let merged = merge_owner_condition(schema, ctx.as_ref(), condition);
    let data = fx.get("data").cloned().unwrap_or(Value::Null);

    Ok(json!({
        "canRead": can_read_schema(schema, ctx.as_ref()),
        "canWrite": can_write_schema(schema, ctx.as_ref()),
        "shouldInjectOwner": should_inject_owner_condition(schema, ctx.as_ref()),
        "condition": merged.unwrap_or(Value::Null),
        "readableFields": sorted_set(get_readable_fields(schema, ctx.as_ref())),
        "readableRelations": sorted_set(get_readable_relations(schema, ctx.as_ref())),
        "writableFields": sorted_set(get_writable_fields(schema, ctx.as_ref())),
        "filteredData": filter_writable_data(schema, ctx.as_ref(), &data),
    }))
}

fn run_case(fx: &Value) -> Result<Value, String> {
    match fx.get("kind").and_then(|v| v.as_str()).unwrap_or("") {
        "process_node" => run_process_node(fx),
        "inject_depends" => run_inject_depends(fx),
        "permission" => run_permission(fx),
        other => Err(format!("未知用例类型: {}", other)),
    }
}

/// 去掉黄金基准里的 `name` / `kind` 元字段，得到纯结果
fn strip_meta(golden: &Value) -> Value {
    let mut v = golden.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("name");
        o.remove("kind");
    }
    v
}

#[test]
fn parity_computes_with_js_reference() {
    let input = load(&fixtures_dir().join("computes").join("cases.json"));
    let expected = load(&fixtures_dir().join("computes").join("expected.json"));

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
        let want = strip_meta(golden);

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
            "parity_computes 对拍失败 {} 项:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
