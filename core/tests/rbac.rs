//! RBAC 动态策略引擎测试（判决语义 + 叠加织入 + 生效域 + 解析校验）
//!
//! 语义基线（设计文档 §4 / 执行文档 01 号 §4.4）：
//! - grant 间 OR（授权并集），引擎间 AND（deny-wins：RBAC 永不放大静态权限面）；
//! - `ctx=None` / `internal` / `super_admin` / `admin` 直通；
//! - overlay：有匹配 grant 才生效；enforce：受管角色 default deny；
//! - 策略未注入时全部原语直通（既有 159 个测试即零回归基线）。

use serde_json::{json, Value};

use rust_store_core::command::{
    plan_insert, plan_mutation, plan_query, plan_query_with_count, plan_remove, plan_update,
    plan_update_many, Probe,
};
use rust_store_core::permission::Context;
use rust_store_core::rbac;
use rust_store_core::schema::Registry;

const RBAC_PREFIX: &str = "ERR_PERMISSION:RBAC:";

// ─── fixtures ────────────────────────────────────────────────

fn post_schema() -> Value {
    json!({
        "name": "Post",
        "collection": "posts",
        "timestamps": false,
        "fields": {
            "title": { "type": "string" },
            "body": { "type": "string" },
            "secret": { "type": "string" }
        },
        "relations": {},
    })
}

fn comment_schema() -> Value {
    json!({
        "name": "Comment",
        "collection": "comments",
        "timestamps": false,
        "fields": { "body": { "type": "string" } },
        "relations": {},
    })
}

fn post_with_rel_schema() -> Value {
    json!({
        "name": "Post",
        "collection": "posts",
        "idPrefix": "p",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {
            "comments": { "model": "Comment", "type": "many", "localField": "_id", "foreignField": "postId" }
        },
    })
}

/// 提取计划首命令的投影对象：find 形态在 `projection` 键，aggregate 形态在
/// pipeline 的 `$project` 阶段（无 condition 的公开查询走 aggregate 快路径）
fn projection_of(plan: &rust_store_core::command::QueryPlan) -> Value {
    let cmd = &plan.commands[0];
    if let Some(p) = cmd.get("projection") {
        if !p.is_null() {
            return p.clone();
        }
    }
    if let Some(pipeline) = cmd.get("pipeline").and_then(|v| v.as_array()) {
        for s in pipeline {
            if let Some(p) = s.get("$project") {
                return p.clone();
            }
        }
    }
    Value::Null
}

fn registry_with(defns: &[Value]) -> Registry {
    let mut reg = Registry::new();
    for d in defns {
        reg.register(d).expect("schema 注册应成功");
    }
    reg
}

fn ctx_of(uid: &str, roles: &[&str]) -> Context {
    Context {
        user_id: Some(uid.to_string()),
        roles: Some(roles.iter().map(|r| r.to_string()).collect()),
        ..Default::default()
    }
}

fn policy(v: Value) -> Value {
    v
}

fn params_of(extra: Value) -> serde_json::Map<String, Value> {
    extra.as_object().cloned().unwrap_or_default()
}

// ─── 解析校验（fail-fast） ───────────────────────────────────

#[test]
fn parse_rejects_unknown_action() {
    let mut reg = registry_with(&[post_schema()]);
    let err = reg
        .set_rbac(Some(&policy(json!({
            "grants": [ { "role": "r1", "model": "Post", "actions": ["drop"] } ]
        }))))
        .expect_err("未知 action 应 Err");
    assert!(err.contains("非法值"), "{err}");
}

#[test]
fn parse_rejects_empty_role_and_model() {
    let mut reg = registry_with(&[post_schema()]);
    assert!(reg
        .set_rbac(Some(&policy(json!({
            "grants": [ { "role": "", "model": "Post", "actions": ["read"] } ]
        }))))
        .is_err());
    assert!(reg
        .set_rbac(Some(&policy(json!({
            "grants": [ { "role": "r1", "model": "", "actions": ["read"] } ]
        }))))
        .is_err());
}

#[test]
fn parse_rejects_operator_condition_and_non_scalar() {
    let mut reg = registry_with(&[post_schema()]);
    // 操作符键（$gt）拒绝
    assert!(reg
        .set_rbac(Some(&policy(json!({
            "grants": [ { "role": "r1", "model": "Post", "actions": ["read"], "condition": { "$gt": 1 } } ]
        }))))
        .is_err());
    // 非标量值拒绝
    assert!(reg
        .set_rbac(Some(&policy(json!({
            "grants": [ { "role": "r1", "model": "Post", "actions": ["read"], "condition": { "status": { "ne": "x" } } } ]
        }))))
        .is_err());
}

#[test]
fn parse_rejects_enforce_without_roles_and_accepts_write_shorthand() {
    let mut reg = registry_with(&[post_schema()]);
    assert!(reg
        .set_rbac(Some(&policy(json!({ "mode": "enforce", "grants": [] }))))
        .is_err(), "enforce 无受管角色应 Err");
    // "write" 速记合法（展开为 insert/update/remove）
    reg.set_rbac(Some(&policy(json!({
        "roles": { "r1": {} },
        "grants": [ { "role": "r1", "model": "Post", "actions": ["read", "write"] } ]
    }))))
    .expect("合法策略应注入成功");
}

// ─── 生效域（overlay / enforce） ─────────────────────────────

#[test]
fn overlay_without_grant_is_transparent() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "roles": { "editor": {} },
        "grants": [ { "role": "editor", "model": "Comment", "actions": ["read"] } ]
    }))))
    .unwrap();
    // Post 无 grant → RBAC 不介入，正常规划（公开 schema 无静态限制）
    let c = ctx_of("u1", &["editor"]);
    plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&c))
        .unwrap_or_else(|e| panic!("overlay 未覆盖 model 应直通: {e}"));
}

#[test]
fn overlay_grant_allows_and_denies_by_action() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "roles": { "viewer": {} },
        "grants": [ { "role": "viewer", "model": "Post", "actions": ["read"] } ]
    }))))
    .unwrap();
    let v = ctx_of("u1", &["viewer"]);
    plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&v))
        .unwrap_or_else(|e| panic!("granted read 应放行: {e}"));
    let err = plan_insert(
        "Post",
        &reg,
        Some(&v),
        &json!({ "_id": "p1", "title": "x" }),
        0,
        "p1",
        None,
    )
    .expect_err("未授予 insert 应拒绝");
    assert!(err.starts_with(RBAC_PREFIX), "{err}");
}

#[test]
fn wildcard_grant_matches_any_model() {
    let mut reg = registry_with(&[post_schema(), comment_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "roles": { "viewer": {} },
        "grants": [ { "role": "viewer", "model": "*", "actions": ["read"] } ]
    }))))
    .unwrap();
    let v = ctx_of("u1", &["viewer"]);
    plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&v))
        .unwrap_or_else(|e| panic!("通配 read 应放行 Post: {e}"));
    plan_query("Comment{ body }", &params_of(json!({})), &reg, Some(&v))
        .unwrap_or_else(|e| panic!("通配 read 应放行 Comment: {e}"));
}

#[test]
fn enforce_default_denies_unconfigured_models() {
    let mut reg = registry_with(&[post_schema(), comment_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "mode": "enforce",
        "roles": { "editor": {} },
        "grants": [ { "role": "editor", "model": "Post", "actions": ["read"] } ]
    }))))
    .unwrap();
    let e = ctx_of("u1", &["editor"]);
    plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&e))
        .unwrap_or_else(|err| panic!("enforce 已配置 model 应放行: {err}"));
    let err = plan_query("Comment{ body }", &params_of(json!({})), &reg, Some(&e))
        .expect_err("enforce 受管角色未配置 model 应 default deny");
    assert!(err.starts_with(RBAC_PREFIX), "{err}");
}

#[test]
fn enforce_unmanaged_roles_transparent() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "mode": "enforce",
        "roles": { "editor": {} },
        "grants": [ { "role": "editor", "model": "Post", "actions": ["read"] } ]
    }))))
    .unwrap();
    // visitor 不在 roles 声明中 → RBAC 不介入
    let v = ctx_of("u1", &["visitor"]);
    plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&v))
        .unwrap_or_else(|e| panic!("enforce 未受管角色应直通: {e}"));
}

// ─── 直通语义（ctx=None / internal / 豁免角色） ──────────────

#[test]
fn bypass_paths_are_transparent() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "mode": "enforce",
        "roles": { "editor": {} },
        "grants": []
    }))))
    .unwrap();
    // ctx=None（fail-open 契约）
    plan_query("Post{ title }", &params_of(json!({})), &reg, None)
        .unwrap_or_else(|e| panic!("ctx=None 应直通: {e}"));
    // internal
    let sys = Context::system();
    plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&sys))
        .unwrap_or_else(|e| panic!("internal 应直通: {e}"));
    // super_admin / admin 豁免
    for role in ["super_admin", "admin"] {
        let a = ctx_of("boss", &[role]);
        plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&a))
            .unwrap_or_else(|e| panic!("{role} 应豁免: {e}"));
    }
}

#[test]
fn clearing_policy_restores_passthrough() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "mode": "enforce",
        "roles": { "editor": {} },
        "grants": []
    }))))
    .unwrap();
    let e = ctx_of("u1", &["editor"]);
    assert!(plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&e)).is_err());
    reg.set_rbac(None).expect("清除应成功");
    plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&e))
        .unwrap_or_else(|err| panic!("清除策略后应直通: {err}"));
}

// ─── 字段交集（读投影 / 写过滤） ─────────────────────────────

#[test]
fn readfields_intersect_prunes_projection() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "roles": { "viewer": {} },
        "grants": [ { "role": "viewer", "model": "Post", "actions": ["read"], "readFields": ["title", "body"] } ]
    }))))
    .unwrap();
    let v = ctx_of("u1", &["viewer"]);
    let plan = plan_query("Post{ title body secret }", &params_of(json!({})), &reg, Some(&v))
        .expect("granted read 应放行");
    let proj = projection_of(&plan);
    let proj = proj.as_object().expect("应有投影");
    assert!(proj.contains_key("title") && proj.contains_key("body"), "可读字段应保留");
    assert!(!proj.contains_key("secret"), "readFields 之外应被裁剪: {proj:?}");
    assert!(proj.contains_key("_id"), "_id 豁免应保留");
}

#[test]
fn writefields_intersect_filters_insert_data() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "roles": { "writer": {} },
        "grants": [ { "role": "writer", "model": "Post", "actions": ["insert"], "writeFields": ["title"] } ]
    }))))
    .unwrap();
    let w = ctx_of("u1", &["writer"]);
    let plan = plan_insert(
        "Post",
        &reg,
        Some(&w),
        &json!({ "_id": "p1", "title": "t", "secret": "s" }),
        0,
        "p1",
        None,
    )
    .expect("granted insert 应放行");
    let doc = &plan["command"]["doc"];
    assert_eq!(doc["title"], json!("t"), "可写字段应保留");
    assert!(doc.get("secret").is_none(), "writeFields 之外应被过滤: {doc}");
}

// ─── 行级（ownerOnly / condition / 两引擎叠加） ──────────────

#[test]
fn owner_only_read_injects_row_condition() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "roles": { "reader": {} },
        "grants": [ { "role": "reader", "model": "Post", "actions": ["read"], "ownerOnly": true } ]
    }))))
    .unwrap();
    let r = ctx_of("u1", &["reader"]);
    let plan = plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&r))
        .expect("ownerOnly read 应放行规划");
    let s = plan.commands[0].to_string();
    assert!(s.contains("createdBy"), "应注入 createdBy 行条件: {s}");
    assert!(s.contains("u1"), "行条件应绑定当前 userId: {s}");
}

#[test]
fn static_condition_merges_with_rbac_row_condition() {
    // 静态 read=[creator] + RBAC read 全开 → owner 条件仍注入（deny-wins：RBAC 不放大静态限制）
    let static_schema = json!({
        "name": "Post",
        "collection": "posts",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "read": ["creator"]
    });
    let mut reg = registry_with(&[static_schema]);
    reg.set_rbac(Some(&policy(json!({
        "roles": { "reader": {} },
        "grants": [ { "role": "reader", "model": "Post", "actions": ["read"] } ]
    }))))
    .unwrap();
    let r = ctx_of("u1", &["reader"]);
    let plan = plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&r))
        .expect("两引擎都过应放行");
    let s = plan.commands[0].to_string();
    assert!(s.contains("createdBy"), "静态 creator owner 条件不得被 RBAC 放大: {s}");
}

#[test]
fn count_injects_owner_row_condition() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "roles": { "reader": {} },
        "grants": [ { "role": "reader", "model": "Post", "actions": ["read"], "ownerOnly": true } ]
    }))))
    .unwrap();
    let r = ctx_of("u1", &["reader"]);
    let plan = plan_query_with_count("Post{ title }", &params_of(json!({})), &reg, Some(&r))
        .expect("count 应放行规划");
    let s = plan.count_command.to_string();
    assert!(s.contains("createdBy"), "count 应注入行条件: {s}");
}

#[test]
fn condition_based_row_filter_in_read() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "roles": { "reader": {} },
        "grants": [ { "role": "reader", "model": "Post", "actions": ["read"], "condition": { "status": "published" } } ]
    }))))
    .unwrap();
    let r = ctx_of("u1", &["reader"]);
    let plan = plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&r))
        .expect("condition read 应放行规划");
    let s = plan.commands[0].to_string();
    assert!(s.contains("published"), "静态行条件应注入: {s}");
}

// ─── 行级写（单条探针 / 批量合并） ───────────────────────────

#[test]
fn owner_only_update_probes_and_judges_doc() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "roles": { "editor": {} },
        "grants": [ { "role": "editor", "model": "Post", "actions": ["update"], "ownerOnly": true } ]
    }))))
    .unwrap();
    let e = ctx_of("u1", &["editor"]);
    let cond = json!({ "_id": "p1" });

    // 首入：返回探针命令（静态白名单放行但 RBAC 行级受限）
    let first = plan_update(
        "Post",
        &reg,
        Some(&e),
        &cond,
        &json!({ "title": "y" }),
        &json!({}),
        0,
        Probe::NotProbed,
    )
    .expect("首入应返回探针");
    let probe_cmd = first.get("needsProbe").expect("应含 needsProbe");
    let proj = probe_cmd["projection"].as_object().expect("探针应有投影");
    assert!(proj.contains_key("createdBy"), "探针投影应含 createdBy");

    // 重入：命中他人文档 → 拒绝
    let other = plan_update(
        "Post",
        &reg,
        Some(&e),
        &cond,
        &json!({ "title": "y" }),
        &json!({}),
        0,
        Probe::Found(&json!({ "_id": "p1", "createdBy": "someone_else" })),
    )
    .expect_err("ownerOnly 更新他人文档应拒绝");
    assert!(other.starts_with(RBAC_PREFIX), "{other}");

    // 重入：命中本人文档 → 放行
    plan_update(
        "Post",
        &reg,
        Some(&e),
        &cond,
        &json!({ "title": "y" }),
        &json!({}),
        0,
        Probe::Found(&json!({ "_id": "p1", "createdBy": "u1" })),
    )
    .expect("ownerOnly 更新本人文档应放行");
}

#[test]
fn owner_only_remove_probes_like_static_creator() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "roles": { "editor": {} },
        "grants": [ { "role": "editor", "model": "Post", "actions": ["remove"], "ownerOnly": true } ]
    }))))
    .unwrap();
    let e = ctx_of("u1", &["editor"]);
    let first = plan_remove("Post", &reg, Some(&e), &json!({ "_id": "p1" }), Probe::NotProbed)
        .expect("首入应返回探针");
    assert!(first.get("needsProbe").is_some(), "remove ownerOnly 应走探针: {first}");
    let err = plan_remove(
        "Post",
        &reg,
        Some(&e),
        &json!({ "_id": "p1" }),
        Probe::Found(&json!({ "_id": "p1", "createdBy": "someone_else" })),
    )
    .expect_err("ownerOnly 删除他人文档应拒绝");
    assert!(err.starts_with(RBAC_PREFIX), "{err}");
}

#[test]
fn owner_only_update_many_merges_condition() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "roles": { "editor": {} },
        "grants": [ { "role": "editor", "model": "Post", "actions": ["update"], "ownerOnly": true } ]
    }))))
    .unwrap();
    let e = ctx_of("u1", &["editor"]);
    let plan = plan_update_many(
        "Post",
        &reg,
        Some(&e),
        &json!({ "status": "draft" }),
        &json!({ "title": "y" }),
        0,
    )
    .expect("updateMany 行级应合并条件");
    let filter = &plan["command"]["filter"];
    // 合并形态为 $and 包装（与静态 merge_owner_condition 同构）：原条件与 RBAC 行条件并列
    let ands = filter["$and"].as_array().expect("应 $and 合并: {filter}");
    assert!(
        ands.iter().any(|c| c == &json!({ "createdBy": "u1" })),
        "应含 createdBy 条件: {filter}"
    );
    assert!(
        ands.iter().any(|c| c == &json!({ "status": "draft" })),
        "原条件应保留: {filter}"
    );
}

// ─── mutation 链路（子模型递归受限） ─────────────────────────

#[test]
fn mutation_child_model_denied_by_rbac() {
    let mut reg = registry_with(&[post_with_rel_schema(), comment_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "mode": "enforce",
        "roles": { "writer": {} },
        "grants": [
            { "role": "writer", "model": "Post", "actions": ["insert"] },
            { "role": "writer", "model": "Comment", "actions": [] }
        ]
    }))))
    .expect_err("Comment 空动作 grant 不允许——应解析失败");

    // 合法形态：不给 Comment 配 grant（enforce 下无匹配 = default deny）
    let mut reg = registry_with(&[post_with_rel_schema(), comment_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "mode": "enforce",
        "roles": { "writer": {} },
        "grants": [ { "role": "writer", "model": "Post", "actions": ["insert"] } ]
    }))))
    .unwrap();
    let w = ctx_of("u1", &["writer"]);
    let err = plan_mutation(
        "Post",
        &reg,
        Some(&w),
        &json!({ "title": "x", "comments": [{ "body": "c" }] }),
        0,
        &["p_new_1".to_string(), "p_new_2".to_string()],
    )
    .expect_err("enforce 下 Comment 未授权应拒绝");
    assert!(err.starts_with(RBAC_PREFIX), "{err}");
}

// ─── decide 单元（原语层） ───────────────────────────────────

#[test]
fn decide_unit_semantics() {
    let mut reg = registry_with(&[post_schema()]);
    let p = json!({
        "roles": { "editor": {}, "viewer": {} },
        "grants": [
            { "role": "editor", "model": "Post", "actions": ["read", "write"], "writeFields": ["title"] },
            { "role": "viewer", "model": "Post", "actions": ["read"], "readFields": ["title", "body"] }
        ]
    });
    reg.set_rbac(Some(&p)).unwrap();
    let policy = reg.rbac().expect("策略应已注入");

    // 双角色：动作并集 + 字段全开（editor 无 readFields 声明 = 全字段）
    let both = ctx_of("u1", &["editor", "viewer"]);
    let d = rbac::decide(policy, Some(&both), "Post").expect("应有判决");
    assert!(d.allowed_actions.contains("read"));
    assert!(d.allowed_actions.contains("insert") && d.allowed_actions.contains("remove"));
    assert!(d.read_fields.is_none(), "任一 read grant 未声明字段 = 不收紧");

    // 单 viewer：只读 + 字段收紧
    let viewer = ctx_of("u2", &["viewer"]);
    let d = rbac::decide(policy, Some(&viewer), "Post").expect("应有判决");
    assert!(d.allowed_actions.contains("read"));
    assert!(!d.allowed_actions.contains("insert"));
    let rf = d.read_fields.as_ref().expect("viewer 应有字段收紧");
    assert!(rf.contains("title") && !rf.contains("secret"));

    // 无 grant model → overlay 不介入
    assert!(rbac::decide(policy, Some(&viewer), "Comment").is_none());

    // row_condition：多 grant 行限制 OR 串接；有全开 grant → None
    assert!(rbac::row_condition(&reg, reg.get("Post").unwrap(), Some(&both), "read").is_none());
    let rc = rbac::row_condition(&reg, reg.get("Post").unwrap(), Some(&viewer), "read");
    // viewer 无 ownerOnly/condition → 无行限制 → None
    assert!(rc.is_none());
}
