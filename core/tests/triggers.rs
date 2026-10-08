//! Schema 触发器测试（A1/A2/A3/A4/A9 —— 01 步验收）
//!
//! 覆盖：
//! - 合法声明注册成功；非法声明注册期显式 Err（未知事件键 / 同给 / 同缺 / op 白名单 /
//!   op=update 缺 condition / onFields 未声明字段 / cascade）
//! - `plan_insert` / `plan_update` 出参 `triggers` 形状；未配置时不出现 `triggers` 键
//! - `before_probe_fields` 收集 `onFields` 与 `{{before.x}}`；探针投影并入
//! - text2query 档拒绝

use serde_json::{json, Value};

use rust_store_core::command::{before_probe_fields, plan_insert, plan_update, Probe};
use rust_store_core::permission::Context;
use rust_store_core::schema::{Profile, Registry};

// ─── fixtures ────────────────────────────────────────────────

fn audit_schema() -> Value {
    json!({
        "name": "Audit",
        "collection": "audits",
        "idPrefix": "a",
        "timestamps": false,
        "fields": { "note": { "type": "string" } },
        "relations": {},
    })
}

/// 带触发器的 schema：insert 命令式 + update 回调式（字段级）
fn task_schema() -> Value {
    json!({
        "name": "Task",
        "collection": "tasks",
        "idPrefix": "t",
        "timestamps": false,
        "fields": {
            "title": { "type": "string" },
            "status": { "type": "string" }
        },
        "relations": {},
        "triggers": {
            "insert": [
                { "into": "Audit", "op": "insert", "data": { "note": "created {{root._id}}" } }
            ],
            "update": [
                { "name": "onStatus", "onFields": ["status"],
                  "when": { "status": "done" },
                  "fnRef": "onTaskDone", "args": { "id": "{{root._id}}", "old": "{{before.status}}" } }
            ]
        }
    })
}

fn registry_with(defns: &[Value]) -> Registry {
    let mut reg = Registry::new();
    for d in defns {
        reg.register(d).expect("schema 注册应成功");
    }
    reg
}

// ─── A1：注册期校验 ──────────────────────────────────────────

#[test]
fn valid_triggers_register_ok() {
    let mut reg = Registry::new();
    reg.register(&task_schema())
        .expect("合法触发器声明应注册成功");
    reg.register(&audit_schema()).expect("目标 schema 注册成功");
    let s = reg.get("Task").unwrap();
    assert_eq!(s.triggers.len(), 2);
    assert_eq!(s.triggers["insert"].len(), 1);
    assert_eq!(s.triggers["update"].len(), 1);
}

#[test]
fn unknown_event_key_is_err() {
    let defn = json!({
        "name": "T", "collection": "t", "idPrefix": "t",
        "fields": { "a": { "type": "string" } },
        "triggers": { "remove": [ { "fnRef": "f" } ] }
    });
    let err = Registry::new()
        .register(&defn)
        .expect_err("remove 事件应 Err");
    assert!(err.contains("非法"), "{err}");
}

#[test]
fn command_and_callback_both_given_is_err() {
    let defn = json!({
        "name": "T", "collection": "t", "idPrefix": "t",
        "fields": { "a": { "type": "string" } },
        "triggers": { "insert": [ { "into": "T", "op": "insert", "data": {}, "fnRef": "f" } ] }
    });
    let err = Registry::new().register(&defn).expect_err("同给应 Err");
    assert!(err.contains("二选一"), "{err}");
}

#[test]
fn neither_command_nor_callback_is_err() {
    let defn = json!({
        "name": "T", "collection": "t", "idPrefix": "t",
        "fields": { "a": { "type": "string" } },
        "triggers": { "insert": [ { "name": "x" } ] }
    });
    let err = Registry::new().register(&defn).expect_err("同缺应 Err");
    assert!(err.contains("二选一"), "{err}");
}

#[test]
fn op_upsert_is_err() {
    let defn = json!({
        "name": "T", "collection": "t", "idPrefix": "t",
        "fields": { "a": { "type": "string" } },
        "triggers": { "insert": [ { "into": "T", "op": "upsert", "data": {} } ] }
    });
    let err = Registry::new()
        .register(&defn)
        .expect_err("op=upsert 应 Err");
    assert!(err.contains("白名单"), "{err}");
}

#[test]
fn update_op_missing_condition_is_err() {
    let defn = json!({
        "name": "T", "collection": "t", "idPrefix": "t",
        "fields": { "a": { "type": "string" } },
        "triggers": { "insert": [ { "into": "T", "op": "update", "data": { "a": "x" } } ] }
    });
    let err = Registry::new()
        .register(&defn)
        .expect_err("op=update 缺 condition 应 Err");
    assert!(err.contains("condition"), "{err}");
}

#[test]
fn on_fields_undeclared_field_is_err() {
    let defn = json!({
        "name": "T", "collection": "t", "idPrefix": "t",
        "fields": { "a": { "type": "string" } },
        "triggers": { "update": [ { "onFields": ["nope"], "fnRef": "f" } ] }
    });
    let err = Registry::new()
        .register(&defn)
        .expect_err("onFields 未声明字段应 Err");
    assert!(err.contains("未在 schema 中声明"), "{err}");
}

#[test]
fn cascade_key_is_err() {
    let defn = json!({
        "name": "T", "collection": "t", "idPrefix": "t",
        "fields": { "a": { "type": "string" } },
        "triggers": { "insert": [ { "fnRef": "f", "cascade": true } ] }
    });
    let err = Registry::new().register(&defn).expect_err("cascade 应 Err");
    assert!(err.contains("cascade"), "{err}");
}

#[test]
fn command_data_undeclared_target_field_is_expand_err() {
    let mut reg = registry_with(&[audit_schema(), task_schema()]);
    // 目标 schema 的 data 引用未声明字段 → 展开期 Err
    let defn = json!({
        "name": "Bad", "collection": "bad", "idPrefix": "b",
        "fields": { "a": { "type": "string" } },
        "triggers": { "insert": [ { "into": "Audit", "op": "insert", "data": { "ghost": "x" } } ] }
    });
    reg.register(&defn).expect("注册成功（字段键校验在展开期）");
    let err = plan_insert("Bad", &reg, None, &json!({ "a": "x" }), 1000, "b1", None)
        .expect_err("data 未声明字段应展开期 Err");
    assert!(err.contains("未在目标 schema"), "{err}");
}

// ─── A2：plan_insert 出参 ────────────────────────────────────

#[test]
fn plan_insert_emits_triggers_step() {
    let reg = registry_with(&[audit_schema(), task_schema()]);
    let plan = plan_insert(
        "Task",
        &reg,
        None,
        &json!({ "title": "x" }),
        1000,
        "t1",
        None,
    )
    .expect("plan_insert 应成功");
    let triggers = plan
        .get("triggers")
        .expect("配了 insert 触发器应出现 triggers 键");
    let arr = triggers.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let step = &arr[0];
    assert_eq!(step["name"], "Task.insert[0]");
    assert_eq!(step["onFields"], json!([]));
    assert_eq!(step["when"], Value::Null);
    let cmd = step.get("command").expect("命令式 step 应含 command");
    assert_eq!(cmd["kind"], "insertOne");
    assert_eq!(cmd["collection"], "audits");
    assert_eq!(cmd["doc"]["note"], "created {{root._id}}");
    // 既有键不受影响
    assert!(plan.get("command").is_some());
    assert!(plan.get("returns").is_some());
}

#[test]
fn plan_insert_without_triggers_has_no_key() {
    let reg = registry_with(&[audit_schema()]);
    let plan = plan_insert(
        "Audit",
        &reg,
        None,
        &json!({ "note": "x" }),
        1000,
        "a1",
        None,
    )
    .expect("plan_insert 应成功");
    assert!(
        plan.get("triggers").is_none(),
        "未配置触发器不得出现 triggers 键"
    );
}

// ─── A3/A4：plan_update 字段级 + 探针投影 ────────────────────

fn creator_ctx() -> Context {
    Context {
        user_id: Some("u1".to_string()),
        roles: Some(vec!["user".to_string()]),
        ..Default::default()
    }
}

/// creator-only 写的 Task：creator 命中需探针 → 探针投影并入 onFields
fn task_creator_only_schema() -> Value {
    let mut v = task_schema();
    v.as_object_mut()
        .unwrap()
        .insert("write".to_string(), json!(["creator"]));
    v
}

#[test]
fn plan_update_needs_probe_projection_includes_on_fields() {
    let reg = registry_with(&[audit_schema(), task_creator_only_schema()]);
    let plan = plan_update(
        "Task",
        &reg,
        Some(&creator_ctx()),
        &json!({ "title": "x" }),
        &json!({ "status": "done" }),
        &json!({}),
        1000,
        Probe::NotProbed,
    )
    .expect("plan_update 应返回探针");
    let cmd = plan.get("needsProbe").expect("creator 写应需探针");
    let proj = cmd.get("projection").expect("探针命令应含 projection");
    assert_eq!(proj["status"], json!(1), "onFields 应并入探针投影（A3/A4）");
    assert_eq!(proj["_id"], json!(1));
    assert_eq!(proj["createdBy"], json!(1));
}

#[test]
fn plan_update_emits_triggers_step() {
    // 静态全放行（不配 creator）→ 直接出 command + triggers
    let reg = registry_with(&[audit_schema(), task_schema()]);
    let plan = plan_update(
        "Task",
        &reg,
        None,
        &json!({ "title": "x" }),
        &json!({ "status": "done" }),
        &json!({}),
        1000,
        Probe::NotProbed,
    )
    .expect("plan_update 应成功");
    let triggers = plan
        .get("triggers")
        .expect("配了 update 触发器应出现 triggers 键");
    let arr = triggers.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let step = &arr[0];
    assert_eq!(step["name"], "Task.update.onStatus");
    assert_eq!(step["onFields"], json!(["status"]));
    assert_eq!(step["when"], json!({ "status": "done" }));
    let cb = step.get("callback").expect("回调式 step 应含 callback");
    assert_eq!(cb["fnRef"], "onTaskDone");
    assert_eq!(cb["args"]["old"], "{{before.status}}");
    assert!(plan.get("triggers").is_some());
}

#[test]
fn plan_update_without_triggers_has_no_key() {
    let reg = registry_with(&[audit_schema()]);
    let plan = plan_update(
        "Audit",
        &reg,
        None,
        &json!({ "note": "x" }),
        &json!({ "note": "y" }),
        &json!({}),
        1000,
        Probe::NotProbed,
    )
    .expect("plan_update 应成功");
    assert!(
        plan.get("triggers").is_none(),
        "未配置触发器不得出现 triggers 键"
    );
}

// ─── before_probe_fields ─────────────────────────────────────

#[test]
fn before_probe_fields_collects_on_fields_and_before_refs() {
    let reg = registry_with(&[audit_schema(), task_schema()]);
    let s = reg.get("Task").unwrap();
    let list = s.triggers.get("update").unwrap();
    let fields = before_probe_fields(list);
    // onFields(status) ∪ {{before.status}} 引用 → 去重后仅 status
    assert_eq!(fields, vec!["status".to_string()]);
}

#[test]
fn before_probe_fields_collects_nested_before_refs() {
    let defn = json!({
        "name": "T", "collection": "t", "idPrefix": "t",
        "fields": { "a": { "type": "number" }, "b": { "type": "number" } },
        "triggers": { "update": [ {
            "when": { "sum": "{{before.a}}" },
            "fnRef": "f", "args": { "diff": ["{{before.a}}", "{{before.b}}"] }
        } ] }
    });
    let mut reg = Registry::new();
    reg.register(&defn).expect("注册成功");
    let s = reg.get("T").unwrap();
    let mut fields = before_probe_fields(s.triggers.get("update").unwrap());
    fields.sort();
    assert_eq!(fields, vec!["a".to_string(), "b".to_string()]);
}

// ─── A9：text2query 门禁 ─────────────────────────────────────

#[test]
fn text2query_rejects_triggered_insert_and_update() {
    let mut reg = registry_with(&[audit_schema(), task_schema()]);
    reg.set_profile(Profile::Text2Query);
    let ctx = creator_ctx();
    let err = plan_insert(
        "Task",
        &reg,
        Some(&ctx),
        &json!({ "title": "x" }),
        1000,
        "t1",
        None,
    )
    .expect_err("text2query 档含触发器 insert 应 Err");
    assert!(err.starts_with("ERR_TEXT2QUERY:"), "{err}");
    // creator-only：先探针（NotProbed → needsProbe），携 Found 重入走到触发器门禁
    let probe_plan = plan_update(
        "Task",
        &reg,
        Some(&ctx),
        &json!({ "title": "x" }),
        &json!({ "status": "done" }),
        &json!({}),
        1000,
        Probe::NotProbed,
    )
    .expect("text2query 档 creator 写应先返回探针");
    assert!(probe_plan.get("needsProbe").is_some());
    let err = plan_update(
        "Task",
        &reg,
        Some(&ctx),
        &json!({ "title": "x" }),
        &json!({ "status": "done" }),
        &json!({}),
        1000,
        Probe::Found(&json!({ "_id": "t1", "createdBy": "u1", "status": "done" })),
    )
    .expect_err("text2query 档含触发器 update 应 Err");
    assert!(err.starts_with("ERR_TEXT2QUERY:"), "{err}");
}

#[test]
fn text2query_unaffected_without_triggers() {
    let mut reg = registry_with(&[audit_schema()]);
    reg.set_profile(Profile::Text2Query);
    let ctx = creator_ctx();
    plan_insert(
        "Audit",
        &reg,
        Some(&ctx),
        &json!({ "note": "x" }),
        1000,
        "a1",
        None,
    )
    .expect("无触发器的 schema 在 text2query 档不受影响");
}
