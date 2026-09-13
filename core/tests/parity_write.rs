//! Phase 2（Command 序列契约）写路径 parity 对拍测试
//!
//! 读 `fixtures/commands/cases.json`（输入）与 `fixtures/commands/expected.json`
//! （JS 黄金基准）里 kind 为写路径的用例，按 JS 侧的「Host 执行语义」模拟驱动回放：
//!   - insert_many  : `plan_insert_many` → `{command, returns}` 直接对拍
//!   - update       : `plan_update` → 命令 + 模拟执行结果（fx.updatedDoc）回喂
//!     `apply_defaults_and_computes` 得 returns
//!   - update_many  : `plan_update_many` → 命令 + `{modifiedCount}` returns
//!   - remove       : `plan_remove` +（有归档结果时）`plan_archive_docs` → 命令序列
//!   - upsert       : `plan_upsert` → 命令 + 结果回喂
//!   - mutation     : `plan_mutation` 步骤序列 → Host 依次「执行」并回填
//!     `{{step.<N>._id}}` 占位符（insertOne 步结果 = doc 本身；
//!     findOneAndUpdate 步结果 = fx.updatedDoc）
//!
//! 黄金基准为**冻结快照**（原单体 JS 参考实现已随重构退役，快照无源可再生）。
//! 复算校验：`node tools/verify-fixtures.js`。

use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use rust_store_core::command::{
    plan_archive_docs, plan_insert_many, plan_mutation, plan_remove, plan_update, plan_update_many,
    plan_upsert, Probe,
};
use rust_store_core::computes::apply_defaults_and_computes;
use rust_store_core::permission::context_from_value;
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

fn model_of(fx: &Value) -> &str {
    fx.get("model").and_then(|v| v.as_str()).unwrap_or("")
}

fn ctx_of(fx: &Value) -> Option<rust_store_core::permission::Context> {
    match fx.get("context") {
        None | Some(Value::Null) => None,
        Some(v) => context_from_value(v),
    }
}

fn now_of(fx: &Value) -> i64 {
    fx.get("now").and_then(|v| v.as_i64()).unwrap_or(0)
}

/// Host 模拟执行 findOneAndUpdate 的返回文档（fx.updatedDoc；缺省 null）
fn updated_doc_of(fx: &Value) -> Value {
    fx.get("updatedDoc").cloned().unwrap_or(Value::Null)
}

/// 把命令里形如 `{{step.<N>._id}}` 的字符串值替换为第 N 步模拟执行的 `_id`
fn substitute_step_placeholders(value: &mut Value, resolved: &[Value]) {
    match value {
        Value::String(s) => {
            if let Some(id) = parse_step_placeholder(s, resolved) {
                *value = id;
            }
        }
        Value::Array(a) => {
            for v in a.iter_mut() {
                substitute_step_placeholders(v, resolved);
            }
        }
        Value::Object(o) => {
            for v in o.values_mut() {
                substitute_step_placeholders(v, resolved);
            }
        }
        _ => {}
    }
}

fn parse_step_placeholder(s: &str, resolved: &[Value]) -> Option<Value> {
    let inner = s.strip_prefix("{{")?.strip_suffix("}}")?;
    let rest = inner.strip_prefix("step.")?;
    let idx: usize = rest.strip_suffix("._id")?.parse().ok()?;
    resolved.get(idx).cloned()
}

// ─── 各 kind 的回放 ──────────────────────────────────────────

fn run_insert_many(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let docs = fx
        .get("docs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let new_ids: Vec<String> = fx
        .get("newIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let out = plan_insert_many(
        model_of(fx),
        &registry,
        ctx_of(fx).as_ref(),
        &docs,
        now_of(fx),
        &new_ids,
        None,
    )?;
    Ok(out)
}

fn run_update(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let ctx = ctx_of(fx);
    let condition = fx.get("condition").cloned().unwrap_or(Value::Null);
    let data = fx.get("data").cloned().unwrap_or(Value::Null);
    let options = fx.get("options").cloned().unwrap_or(Value::Null);

    let out = plan_update(
        model_of(fx),
        &registry,
        ctx.as_ref(),
        &condition,
        &data,
        &options,
        now_of(fx),
        Probe::NotProbed,
    )?;
    let command = out
        .get("command")
        .cloned()
        .ok_or_else(|| format!("plan_update 未产出命令: {}", out))?;

    // Host 模拟执行：findOneAndUpdate 返回 fx.updatedDoc，回喂补默认值
    let updated = updated_doc_of(fx);
    let returns = if updated.is_null() {
        Value::Null
    } else {
        apply_defaults_and_computes(&updated, registry.get(model_of(fx))?, None)?
    };
    Ok(json!({ "commands": [command], "returns": returns }))
}

fn run_update_many(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let condition = fx.get("condition").cloned().unwrap_or(Value::Null);
    let data = fx.get("data").cloned().unwrap_or(Value::Null);

    let out = plan_update_many(
        model_of(fx),
        &registry,
        ctx_of(fx).as_ref(),
        &condition,
        &data,
        now_of(fx),
    )?;
    let command = out
        .get("command")
        .cloned()
        .ok_or_else(|| format!("plan_update_many 未产出命令: {}", out))?;
    let modified_count = fx
        .get("modifiedCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    Ok(json!({ "command": command, "returns": { "modifiedCount": modified_count } }))
}

fn run_remove(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let condition = fx.get("condition").cloned().unwrap_or(Value::Null);
    let docs = fx
        .get("docs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let out = plan_remove(
        model_of(fx),
        &registry,
        ctx_of(fx).as_ref(),
        &condition,
        Probe::NotProbed,
    )?;
    let find_command = out.get("findCommand").cloned().unwrap_or(Value::Null);
    let delete_command = out
        .get("deleteCommand")
        .cloned()
        .ok_or_else(|| format!("plan_remove 未产出 deleteCommand: {}", out))?;

    let mut commands: Vec<Value> = Vec::new();
    let mut archived_count = 0usize;
    if !find_command.is_null() {
        commands.push(find_command);
        // Host 模拟执行 find → fx.docs；非空时归档
        if !docs.is_empty() {
            let archive = plan_archive_docs(model_of(fx), &registry, &docs, now_of(fx))?;
            commands.push(archive.get("command").cloned().unwrap_or(Value::Null));
            archived_count = docs.len();
        }
    }
    commands.push(delete_command);

    let deleted_count = fx.get("deletedCount").and_then(|v| v.as_i64()).unwrap_or(0);
    Ok(json!({
        "commands": commands,
        "returns": { "deletedCount": deleted_count, "archivedCount": archived_count },
    }))
}

fn run_upsert(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let condition = fx.get("condition").cloned().unwrap_or(Value::Null);
    let data = fx.get("data").cloned().unwrap_or(Value::Null);
    let options = fx.get("options").cloned().unwrap_or(Value::Null);
    let new_id = fx
        .get("newIds")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let out = plan_upsert(
        model_of(fx),
        &registry,
        ctx_of(fx).as_ref(),
        &condition,
        &data,
        &options,
        now_of(fx),
        &new_id,
    )?;
    let command = out
        .get("command")
        .cloned()
        .ok_or_else(|| format!("plan_upsert 未产出命令: {}", out))?;

    let updated = updated_doc_of(fx);
    let returns = if updated.is_null() {
        Value::Null
    } else {
        apply_defaults_and_computes(&updated, registry.get(model_of(fx))?, None)?
    };
    Ok(json!({ "command": command, "returns": returns }))
}

fn run_mutation(fx: &Value) -> Result<Value, String> {
    let registry = build_registry(fx)?;
    let data = fx.get("data").cloned().unwrap_or(Value::Null);
    let new_ids: Vec<String> = fx
        .get("newIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let out = plan_mutation(
        model_of(fx),
        &registry,
        ctx_of(fx).as_ref(),
        &data,
        now_of(fx),
        &new_ids,
    )?;
    let steps = out
        .get("steps")
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("plan_mutation 未产出 steps: {}", out))?;

    // Host 依次「执行」各步：insertOne 结果 = doc 本身；findOneAndUpdate 结果 = fx.updatedDoc；
    // 后续命令里的 `{{step.<N>._id}}` 用第 N 步结果 `_id` 回填
    let mut resolved: Vec<Value> = Vec::with_capacity(steps.len());
    let mut commands: Vec<Value> = Vec::with_capacity(steps.len());
    let mut root_result: Option<Value> = None;
    for step in steps {
        let mut command = step
            .get("command")
            .cloned()
            .ok_or_else(|| format!("步骤缺少 command: {}", step))?;
        substitute_step_placeholders(&mut command, &resolved);

        let result = match command.get("kind").and_then(|v| v.as_str()) {
            Some("insertOne") => command.get("doc").cloned().unwrap_or(Value::Null),
            Some("findOneAndUpdate") => updated_doc_of(fx),
            other => return Err(format!("未支持的 mutation 步骤命令: {:?}", other)),
        };
        if root_result.is_none() {
            root_result = Some(result.clone());
        }
        resolved.push(result.get("_id").cloned().unwrap_or(Value::Null));
        commands.push(command);
    }

    let root = root_result.ok_or_else(|| "mutation 至少应有一个步骤".to_string())?;
    let returns = if root.is_null() {
        Value::Null
    } else {
        apply_defaults_and_computes(&root, registry.get(model_of(fx))?, None)?
    };
    Ok(json!({ "commands": commands, "returns": returns }))
}

fn run_case(fx: &Value) -> Result<Value, String> {
    match fx.get("kind").and_then(|v| v.as_str()).unwrap_or("") {
        "insert_many" => run_insert_many(fx),
        "update" => run_update(fx),
        "update_many" => run_update_many(fx),
        "remove" => run_remove(fx),
        "upsert" => run_upsert(fx),
        "mutation" => run_mutation(fx),
        // 读路径用例由 parity_commands 覆盖，此处跳过
        "query" | "query_with_count" | "resolve_page" | "restore_sort_order" | "insert"
        | "exists" | "count" | "aggregate" => Ok(Value::Null),
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
fn parity_write_with_js_reference() {
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

        let is_write = matches!(
            fx.get("kind").and_then(|v| v.as_str()).unwrap_or(""),
            "insert_many" | "update" | "update_many" | "remove" | "upsert" | "mutation"
        );
        if !is_write {
            continue; // 非 write 用例（黄金基准归 parity_commands）
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
            "parity_write 对拍失败 {} 项:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
