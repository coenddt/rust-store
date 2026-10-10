//! Schema 触发器测试（A1/A2/A3/A4/A9 —— 01 步验收）
//!
//! 覆盖：
//! - 合法声明注册成功；非法声明注册期显式 Err（未知事件键 / 同给 / 同缺 / op 白名单 /
//!   op=update 缺 condition / onFields 未声明字段 / cascade）
//! - `plan_insert` / `plan_update` 出参 `triggers` 形状；未配置时不出现 `triggers` 键
//! - `before_probe_fields` 收集 `onFields` 与 `{{before.x}}`；探针投影并入
//! - text2query 档拒绝

use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

use rust_store_core::command::{
    before_probe_fields, expand_schedule_triggers, plan_insert, plan_remove, plan_update, Probe,
};
use rust_store_core::permission::Context;
use rust_store_core::schema::{validate_cron, Profile, Registry};

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
        "triggers": { "delete": [ { "fnRef": "f" } ] }
    });
    let err = Registry::new()
        .register(&defn)
        .expect_err("delete（非 EVENTS 成员）应 Err");
    assert!(err.contains("非法"), "{err}");
}

#[test]
fn remove_event_key_is_valid() {
    let defn = json!({
        "name": "T", "collection": "t", "idPrefix": "t",
        "fields": { "a": { "type": "string" } },
        "triggers": { "remove": [ { "fnRef": "f" } ] }
    });
    Registry::new()
        .register(&defn)
        .expect("remove 是合法事件键");
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
    // 无 ctx（内部调用）也必须发触发器探针（before 供给与权限无关）：
    // NotProbed → needsProbe；携 Found 重入 → command + triggers
    let reg = registry_with(&[audit_schema(), task_schema()]);
    let first = plan_update(
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
    let probe_cmd = first
        .get("needsProbe")
        .expect("配 update 触发器（无 ctx）应发探针");
    let proj = probe_cmd
        .get("projection")
        .expect("探针命令应含 projection");
    assert_eq!(proj["status"], json!(1), "onFields 应并入探针投影");
    assert_eq!(proj["_id"], json!(1));
    let plan = plan_update(
        "Task",
        &reg,
        None,
        &json!({ "title": "x" }),
        &json!({ "status": "done" }),
        &json!({}),
        1000,
        Probe::Found(&json!({ "_id": "t1", "status": "open" })),
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

// ─── A1/A2：plan_remove 触发链（remove 事件） ─────────────────

/// 带 remove 触发器的 schema：命令式 op:"remove" + 回调式
fn doc_schema() -> Value {
    json!({
        "name": "Doc",
        "collection": "docs",
        "idPrefix": "d",
        "timestamps": false,
        "fields": {
            "title": { "type": "string" },
            "ownerId": { "type": "string" }
        },
        "relations": {},
        "triggers": {
            "remove": [
                { "name": "cleanAudit", "into": "Audit", "op": "remove",
                  "condition": { "note": "{{before.title}}" } },
                { "name": "onRemoved", "fnRef": "onDocRemoved",
                  "args": { "id": "{{before._id}}" } }
            ]
        }
    })
}

#[test]
fn plan_remove_emits_triggers_step() {
    let reg = registry_with(&[audit_schema(), doc_schema()]);
    let plan = plan_remove("Doc", &reg, None, &json!({ "_id": "d1" }), Probe::NotProbed)
        .expect("plan_remove 应成功");
    let triggers = plan
        .get("triggers")
        .expect("配了 remove 触发器应出现 triggers 键");
    let arr = triggers.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // 命令式 step：op:"remove" → deleteMany，目标为 Audit
    let step0 = &arr[0];
    assert_eq!(step0["name"], "Doc.remove.cleanAudit");
    let cmd = step0.get("command").expect("命令式 step 应含 command");
    assert_eq!(cmd["kind"], "deleteMany");
    assert_eq!(cmd["collection"], "audits");
    assert_eq!(cmd["filter"]["note"], "{{before.title}}");
    // 回调式 step
    let step1 = &arr[1];
    assert_eq!(step1["name"], "Doc.remove.onRemoved");
    let cb = step1.get("callback").expect("回调式 step 应含 callback");
    assert_eq!(cb["fnRef"], "onDocRemoved");
    assert_eq!(cb["args"]["id"], "{{before._id}}");
    // 既有键不受影响
    assert!(plan.get("deleteCommand").is_some());
    assert!(plan.get("archiveCollection").is_some());
}

#[test]
fn remove_op_missing_condition_is_err() {
    let defn = json!({
        "name": "T", "collection": "t", "idPrefix": "t",
        "fields": { "a": { "type": "string" } },
        "triggers": { "remove": [ { "into": "T", "op": "remove" } ] }
    });
    let err = Registry::new()
        .register(&defn)
        .expect_err("op=remove 缺 condition 应 Err");
    assert!(err.contains("必须提供 condition"), "{err}");
}

#[test]
fn remove_op_with_data_is_err() {
    let defn = json!({
        "name": "T", "collection": "t", "idPrefix": "t",
        "fields": { "a": { "type": "string" } },
        "triggers": { "remove": [ {
            "into": "T", "op": "remove",
            "condition": { "a": "x" }, "data": { "a": "y" }
        } ] }
    });
    let err = Registry::new()
        .register(&defn)
        .expect_err("op=remove 带 data 应 Err");
    assert!(err.contains("不接受 data"), "{err}");
}

#[test]
fn plan_remove_without_triggers_has_no_key() {
    let reg = registry_with(&[audit_schema()]);
    let plan = plan_remove(
        "Audit",
        &reg,
        None,
        &json!({ "_id": "a1" }),
        Probe::NotProbed,
    )
    .expect("plan_remove 应成功");
    assert!(
        plan.get("triggers").is_none(),
        "未配置触发器不得出现 triggers 键"
    );
    // 结构与改动前一致
    assert!(plan.get("archiveCollection").is_some());
    assert!(plan.get("deleteCommand").is_some());
}

// ─── A3/A4：schedule 触发器（cron + expand_schedule_triggers） ─

#[test]
fn schedule_event_registers_with_cron() {
    let defn = json!({
        "name": "Job", "collection": "jobs", "idPrefix": "j",
        "fields": { "at": { "type": "number" } },
        "triggers": { "schedule": [
            { "name": "nightly", "cron": "0 2 * * *",
              "fnRef": "onNightly", "args": { "at": "{{now}}" } }
        ] }
    });
    let mut reg = Registry::new();
    reg.register(&defn).expect("合法 schedule 触发器应注册成功");
    let s = reg.get("Job").unwrap();
    let list = s.triggers.get("schedule").expect("应有 schedule 列表");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].cron.as_deref(), Some("0 2 * * *"));
}

#[test]
fn schedule_edge_cron_is_err() {
    // errCases（fixture）之外的边界：步进 0 / 区间倒置 / 周越界
    for (cron, want) in [
        ("*/0 * * * *", "正整数"),
        ("5-1 * * * *", "起点大于终点"),
        ("* * * * 8", "超出范围"),
    ] {
        let defn = json!({
            "name": "T", "collection": "t", "idPrefix": "t",
            "fields": { "a": { "type": "string" } },
            "triggers": { "schedule": [ { "cron": cron, "fnRef": "f" } ] }
        });
        let err = Registry::new()
            .register(&defn)
            .expect_err("非法 cron 应注册期 Err");
        assert!(err.contains(want), "cron \"{cron}\"：{err}");
    }
}

#[test]
fn non_schedule_with_cron_is_err() {
    let defn = json!({
        "name": "T", "collection": "t", "idPrefix": "t",
        "fields": { "a": { "type": "string" } },
        "triggers": { "insert": [ { "cron": "* * * * *", "fnRef": "f" } ] }
    });
    let err = Registry::new()
        .register(&defn)
        .expect_err("非 schedule 带 cron 应 Err");
    assert!(err.contains("仅 schedule 事件可配"), "{err}");
}

#[test]
fn validate_cron_accepts_common_forms() {
    for c in [
        "* * * * *",
        "0 2 * * *",
        "*/10 * * * *",
        "0,30 * * * *",
        "0 0 1 1 0",
        "30 8-18 * * 1-5",
    ] {
        validate_cron(c).unwrap_or_else(|e| panic!("\"{c}\" 应合法：{e}"));
    }
}

#[test]
fn expand_schedule_triggers_output_shape() {
    let job = json!({
        "name": "Job", "collection": "jobs", "idPrefix": "j",
        "fields": { "at": { "type": "number" } },
        "triggers": { "schedule": [
            { "name": "nightly", "cron": "0 2 * * *",
              "into": "Audit", "op": "insert", "data": { "note": "nightly {{now}}" } },
            { "name": "hourly", "cron": "0 * * * *",
              "fnRef": "onHourly", "args": { "at": "{{now}}" } }
        ] }
    });
    let reg = registry_with(&[audit_schema(), job]);
    let list = expand_schedule_triggers(&reg, None).expect("展开应成功");
    assert_eq!(list.len(), 2);
    // [{schema, name, cron, step}] 形状 + 声明顺序稳定
    assert_eq!(list[0]["schema"], "Job");
    assert_eq!(list[0]["name"], "Job.schedule.nightly");
    assert_eq!(list[0]["cron"], "0 2 * * *");
    let step = &list[0]["step"];
    assert_eq!(step["name"], "Job.schedule.nightly");
    assert!(step.get("onFields").is_some(), "step 应含 onFields");
    assert!(step.get("when").is_some(), "step 应含 when");
    let cmd = step.get("command").expect("命令式 step 应含 command");
    assert_eq!(cmd["kind"], "insertOne");
    assert_eq!(cmd["collection"], "audits");
    assert_eq!(list[1]["name"], "Job.schedule.hourly");
    assert_eq!(list[1]["cron"], "0 * * * *");
    let cb = list[1]["step"]
        .get("callback")
        .expect("回调式 step 应含 callback");
    assert_eq!(cb["fnRef"], "onHourly");
    assert_eq!(cb["args"]["at"], "{{now}}");
}

#[test]
fn expand_schedule_triggers_empty_without_schedule() {
    let reg = registry_with(&[audit_schema(), task_schema()]);
    let list = expand_schedule_triggers(&reg, None).expect("展开应成功");
    assert!(list.is_empty(), "无 schedule 声明应为空数组");
}

#[test]
fn text2query_rejects_schedule_expand() {
    let job = json!({
        "name": "Job", "collection": "jobs", "idPrefix": "j",
        "fields": { "at": { "type": "number" } },
        "triggers": { "schedule": [
            { "cron": "0 2 * * *", "fnRef": "onNightly", "args": { "at": "{{now}}" } }
        ] }
    });
    let mut reg = registry_with(&[job]);
    reg.set_profile(Profile::Text2Query);
    let err =
        expand_schedule_triggers(&reg, None).expect_err("text2query 档含 schedule 触发器应 Err");
    assert!(err.starts_with("ERR_TEXT2QUERY:"), "{err}");
}

// ─── fixture golden：schedule 展开 + 非法 cron errCases ───────

#[test]
fn fixture_err_cases_and_schedule_expand() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core 目录应有上级目录")
        .join("fixtures")
        .join("triggers")
        .join("cases.json");
    let fx: Value = serde_json::from_str(&fs::read_to_string(&path).expect("读 fixture 应成功"))
        .expect("解析 fixture 应成功");

    // errCases：逐项注册期 Err 且错误含期望子串（零静默）
    for (i, ec) in fx["errCases"]
        .as_array()
        .expect("errCases 应为数组")
        .iter()
        .enumerate()
    {
        let want = ec["err"].as_str().expect("errCases 项应含 err 子串");
        let err = Registry::new()
            .register(&ec["defn"])
            .expect_err("errCases 项应注册期 Err");
        assert!(
            err.contains(want),
            "errCases[{i}] 期望含 \"{want}\"，实为：{err}"
        );
    }

    // schedule 展开：Order 的 2 条声明（命令式 expireLogs + 回调式 dailyReport）
    let mut reg = Registry::new();
    for s in fx["schemas"].as_array().expect("schemas 应为数组") {
        reg.register(s).expect("fixture schema 注册应成功");
    }
    let list = expand_schedule_triggers(&reg, None).expect("schedule 展开应成功");
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["name"], "Order.schedule.expireLogs");
    assert_eq!(list[0]["cron"], "0 2 * * *");
    let cmd = list[0]["step"]
        .get("command")
        .expect("expireLogs 应为命令式");
    assert_eq!(cmd["kind"], "deleteMany");
    assert_eq!(cmd["collection"], "StockLog");
    assert_eq!(cmd["filter"]["at"]["$lt"], "{{now}}");
    assert_eq!(list[1]["name"], "Order.schedule.dailyReport");
    assert_eq!(list[1]["cron"], "*/10 * * * *");
    let cb = list[1]["step"]
        .get("callback")
        .expect("dailyReport 应为回调式");
    assert_eq!(cb["fnRef"], "onDailyReport");
    assert_eq!(cb["args"]["at"], "{{now}}");
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

// ─── fixture golden（03 步骤 2；03 §4.2 契约） ────────────────

#[test]
fn triggers_fixture_shape() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core 目录应有上级目录")
        .join("fixtures")
        .join("triggers")
        .join("cases.json");
    let fx: Value = serde_json::from_str(
        &fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("读取 {} 失败: {}", path.display(), e)),
    )
    .unwrap_or_else(|e| panic!("解析 fixture 失败: {e}"));

    let mut reg = Registry::new();
    for s in fx["schemas"].as_array().expect("schemas 应为数组") {
        reg.register(s).expect("fixture schema 注册应成功");
    }

    // case 0：plan_insert → triggers 每 step 含 name/onFields/when/(command|callback)
    let c0 = &fx["cases"][0];
    let plan = plan_insert(
        c0["schema"].as_str().unwrap(),
        &reg,
        None,
        &c0["input"],
        c0["now"].as_i64().unwrap_or(0),
        c0["newId"].as_str().unwrap_or(""),
        None,
    )
    .expect("fixture case0 plan_insert 应成功");
    let triggers = plan.get("triggers").expect("case0 应含 triggers 键");
    for step in triggers.as_array().expect("triggers 应为数组") {
        assert!(step.get("name").is_some(), "step 应含 name");
        assert!(step.get("onFields").is_some(), "step 应含 onFields");
        assert!(step.get("when").is_some(), "step 应含 when");
        assert!(
            step.get("command").is_some() || step.get("callback").is_some(),
            "step 应含 command 或 callback"
        );
    }
    assert_eq!(triggers[0]["name"], "Order.insert.decStock");
    assert_eq!(triggers[1]["name"], "Order.insert.stockLog");

    // case 1：plan_update 首次 → needsProbe（projection 含 onFields 的 status）
    let c1 = &fx["cases"][1];
    let first = plan_update(
        c1["schema"].as_str().unwrap(),
        &reg,
        None,
        &c1["condition"],
        &c1["input"],
        &json!({}),
        c1["now"].as_i64().unwrap_or(0),
        Probe::NotProbed,
    )
    .expect("fixture case1 首次 plan_update 应成功");
    let proj = first
        .get("needsProbe")
        .expect("case1 首次应返回 needsProbe")
        .get("projection")
        .expect("探针应含 projection");
    assert_eq!(proj["status"], json!(1), "onFields 应并入探针投影");

    // 携 Found 重入 → command + triggers（fnRef 回调式 step）
    let doc = c1["doc"].clone();
    let plan = plan_update(
        c1["schema"].as_str().unwrap(),
        &reg,
        None,
        &c1["condition"],
        &c1["input"],
        &json!({}),
        c1["now"].as_i64().unwrap_or(0),
        Probe::Found(&doc),
    )
    .expect("fixture case1 重入 plan_update 应成功");
    let triggers = plan.get("triggers").expect("case1 重入应含 triggers 键");
    let arr = triggers.as_array().expect("triggers 应为数组");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "Order.update.onPaid");
    let cb = arr[0].get("callback").expect("onPaid 应为回调式 step");
    assert_eq!(cb["fnRef"], "grantPoints");

    // remove 声明用例：plan_remove → remove_cmd(deleteMany) + remove_cb(callback)
    let rplan = plan_remove(
        c1["schema"].as_str().unwrap(),
        &reg,
        None,
        &c1["condition"],
        Probe::NotProbed,
    )
    .expect("fixture plan_remove 应成功");
    let rtrig = rplan
        .get("triggers")
        .expect("fixture remove 声明应出现 triggers 键");
    let rarr = rtrig.as_array().expect("triggers 应为数组");
    assert_eq!(rarr.len(), 2);
    assert_eq!(rarr[0]["name"], "Order.remove.cleanLogs");
    let rcmd = rarr[0].get("command").expect("cleanLogs 应为命令式 step");
    assert_eq!(rcmd["kind"], "deleteMany");
    assert_eq!(rcmd["collection"], "StockLog");
    assert_eq!(rcmd["filter"]["orderId"], "{{before._id}}");
    assert_eq!(rarr[1]["name"], "Order.remove.onRemoved");
    let rcb = rarr[1].get("callback").expect("onRemoved 应为回调式 step");
    assert_eq!(rcb["fnRef"], "onOrderRemoved");
    assert_eq!(rcb["args"]["id"], "{{before._id}}");
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
