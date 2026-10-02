//! RBAC 动态策略引擎测试（判决语义 + 叠加织入 + 生效域 + 解析校验）
//!
//! 语义基线（设计文档 §4 / §11 / 执行文档 01 号 §4.4）：
//! - grant 间 OR（授权并集），引擎间 AND（deny-wins：RBAC 永不放大静态权限面）；
//! - `ctx=None` / `internal` / 豁免清单（`exempt_roles`，默认空）命中直通；
//! - overlay：有匹配 grant 才生效；enforce：受管角色 default deny（豁免清单空时无例外）；
//! - 策略未注入时全部原语直通（既有 159 个测试即零回归基线）。

use serde_json::{json, Value};

use rust_store_core::command::{
    plan_insert, plan_mutation, plan_query, plan_query_with_count, plan_remove, plan_update,
    plan_update_many, Probe,
};
use rust_store_core::permission::{
    can_read_schema, can_write_schema, should_inject_owner_condition, Context, UnconfiguredPolicy,
};
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
    assert!(
        reg.set_rbac(Some(&policy(json!({ "mode": "enforce", "grants": [] }))))
            .is_err(),
        "enforce 无受管角色应 Err"
    );
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
    // 原「super_admin / admin 隐形豁免」已随清单化移除：受管 admin 无例外 default deny
    // 的断言见 enforce_no_exception_default_deny（豁免只能显式 set_exempt_roles）
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
    let plan = plan_query(
        "Post{ title body secret }",
        &params_of(json!({})),
        &reg,
        Some(&v),
    )
    .expect("granted read 应放行");
    let proj = projection_of(&plan);
    let proj = proj.as_object().expect("应有投影");
    assert!(
        proj.contains_key("title") && proj.contains_key("body"),
        "可读字段应保留"
    );
    assert!(
        !proj.contains_key("secret"),
        "readFields 之外应被裁剪: {proj:?}"
    );
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
    assert!(
        doc.get("secret").is_none(),
        "writeFields 之外应被过滤: {doc}"
    );
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
    assert!(
        s.contains("createdBy"),
        "静态 creator owner 条件不得被 RBAC 放大: {s}"
    );
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
    let first = plan_remove(
        "Post",
        &reg,
        Some(&e),
        &json!({ "_id": "p1" }),
        Probe::NotProbed,
    )
    .expect("首入应返回探针");
    assert!(
        first.get("needsProbe").is_some(),
        "remove ownerOnly 应走探针: {first}"
    );
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
    let d = rbac::decide(policy, &[], Some(&both), "Post").expect("应有判决");
    assert!(d.allowed_actions.contains("read"));
    assert!(d.allowed_actions.contains("insert") && d.allowed_actions.contains("remove"));
    assert!(
        d.read_fields.is_none(),
        "任一 read grant 未声明字段 = 不收紧"
    );

    // 单 viewer：只读 + 字段收紧
    let viewer = ctx_of("u2", &["viewer"]);
    let d = rbac::decide(policy, &[], Some(&viewer), "Post").expect("应有判决");
    assert!(d.allowed_actions.contains("read"));
    assert!(!d.allowed_actions.contains("insert"));
    let rf = d.read_fields.as_ref().expect("viewer 应有字段收紧");
    assert!(rf.contains("title") && !rf.contains("secret"));

    // 无 grant model → overlay 不介入
    assert!(rbac::decide(policy, &[], Some(&viewer), "Comment").is_none());

    // row_condition：多 grant 行限制 OR 串接；有全开 grant → None
    assert!(rbac::row_condition(&reg, reg.get("Post").unwrap(), Some(&both), "read").is_none());
    let rc = rbac::row_condition(&reg, reg.get("Post").unwrap(), Some(&viewer), "read");
    // viewer 无 ownerOnly/condition → 无行限制 → None
    assert!(rc.is_none());
}

// ─── 角色清单化语义（设计 §11.3–§11.5；总纲 A3） ─────────────

/// enforce 无例外 default deny（§11.5 第 1 行）：豁免清单为空时无后门
#[test]
fn enforce_no_exception_default_deny() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_rbac(Some(&policy(json!({
        "mode": "enforce",
        "roles": { "editor": {}, "admin": {} },
        "grants": []
    }))))
    .unwrap();
    let a = ctx_of("u1", &["admin"]);
    let err = plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&a))
        .expect_err("豁免清单空 = 无例外 default deny（admin 不再隐形放行）");
    assert!(err.starts_with(RBAC_PREFIX), "{err}");
    // 同 ctx 显式配置豁免清单后 → 直通
    reg.set_exempt_roles(vec!["admin".to_string()]);
    plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&a))
        .unwrap_or_else(|e| panic!("显式豁免后应直通: {e}"));
}

/// 静态引擎豁免清单可配置（§11.5 第 1 行）：admin 不再默认放行，豁免显式注入
#[test]
fn exempt_roles_configurable_static() {
    let read_editor_schema = json!({
        "name": "Post",
        "collection": "posts",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "read": ["editor"]
    });
    let mut reg = registry_with(&[read_editor_schema]);
    let schema = reg.get("Post").unwrap();
    let a = ctx_of("u1", &["admin"]);
    // 默认（豁免清单空）：admin 不再默认放行
    assert!(
        !can_read_schema(reg.role_rules(), schema, Some(&a)),
        "admin 不再默认放行（原隐形豁免已清单化）"
    );
    // 显式配置豁免清单后放行
    reg.set_exempt_roles(vec!["admin".to_string()]);
    let schema = reg.get("Post").unwrap();
    assert!(can_read_schema(reg.role_rules(), schema, Some(&a)));

    // creator-only schema 下：非豁免者（有 userId）注入 owner 条件；豁免者不注入
    let creator_schema = json!({
        "name": "Memo",
        "collection": "memos",
        "timestamps": false,
        "fields": { "body": { "type": "string" } },
        "relations": {},
        "read": ["creator"]
    });
    let mut reg2 = registry_with(&[creator_schema]);
    let memo = reg2.get("Memo").unwrap();
    assert!(
        should_inject_owner_condition(reg2.role_rules(), memo, Some(&a)),
        "creator-only 下非豁免者应注入 owner 条件"
    );
    reg2.set_exempt_roles(vec!["admin".to_string()]);
    let memo = reg2.get("Memo").unwrap();
    assert!(
        !should_inject_owner_condition(reg2.role_rules(), memo, Some(&a)),
        "豁免者不注入 owner 条件"
    );
}

/// 拒写清单可配置（§11.5 第 2 行）：guest 写不再必拒，显式注入后拒绝且读不受影响
#[test]
fn deny_write_roles_configurable() {
    // `write: true` 经 register 归一为未配置（Open 姿态承载），等价「写不做角色白名单限制」
    let write_open_schema = json!({
        "name": "Post",
        "collection": "posts",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "write": true
    });
    let mut reg = registry_with(&[write_open_schema]);
    let schema = reg.get("Post").unwrap();
    let g = ctx_of("u1", &["guest"]);
    // 默认（拒写清单空）：guest 写放行（原 guest 硬编码写拒已清单化移除）
    assert!(
        can_write_schema(reg.role_rules(), schema, Some(&g)),
        "guest 写不再必拒"
    );
    assert!(
        can_read_schema(reg.role_rules(), schema, Some(&g)),
        "read 未配 → Open 姿态放行"
    );
    // 显式注入拒写清单：写拒，读不受影响
    reg.set_deny_write_roles(vec!["guest".to_string()]);
    let schema = reg.get("Post").unwrap();
    assert!(
        !can_write_schema(reg.role_rules(), schema, Some(&g)),
        "拒写清单命中应拒绝"
    );
    assert!(
        can_read_schema(reg.role_rules(), schema, Some(&g)),
        "拒写只影响写路径，读不受影响"
    );
}

/// Open 姿态下 guest 无白名单读放行（§11.4 第 3 行语义差异：原 guest 读拒不再保留）
#[test]
fn unconfigured_open_allows_guest_read() {
    let reg = registry_with(&[post_schema()]); // 无 read 配置
    let schema = reg.get("Post").unwrap();
    let g = ctx_of("u1", &["guest"]);
    assert!(
        can_read_schema(reg.role_rules(), schema, Some(&g)),
        "Open 下 guest 无白名单读放行——原「无白名单时 guest 读拒」已随清单化移除（迁移差异，§11.4）"
    );
}

/// Closed 姿态 fail-secure（§11.5 第 4 行）：未配置模型读写全拒；internal / ctx=None
/// 不受姿态影响（fail-open 归 require_context，§11.6）；豁免直通先于姿态（§11.3）
#[test]
fn unconfigured_closed_fails_secure() {
    let mut reg = registry_with(&[post_schema()]);
    reg.set_unconfigured_policy(UnconfiguredPolicy::Closed);
    let schema = reg.get("Post").unwrap();
    let g = ctx_of("u1", &["guest"]);
    assert!(
        !can_read_schema(reg.role_rules(), schema, Some(&g)),
        "Closed 读拒"
    );
    assert!(
        !can_write_schema(reg.role_rules(), schema, Some(&g)),
        "Closed 写拒"
    );
    // internal / ctx=None 直通不受姿态影响
    let sys = Context::system();
    assert!(
        can_read_schema(reg.role_rules(), schema, Some(&sys)),
        "internal 不受姿态影响"
    );
    assert!(
        can_read_schema(reg.role_rules(), schema, None),
        "ctx=None 不受姿态影响"
    );
    // 豁免直通先于未配置姿态（显式信任声明，姿态管不住它）
    reg.set_exempt_roles(vec!["admin".to_string()]);
    let schema = reg.get("Post").unwrap();
    let a = ctx_of("u1", &["admin"]);
    assert!(
        can_read_schema(reg.role_rules(), schema, Some(&a)),
        "豁免清单命中者在 Closed 下仍放行（§11.3 一切判决环节直通）"
    );
}

/// 拒写清单命中者三写路径全拒（A3/A1 回归：原 guest 写拒三锚点路径）
#[test]
fn deny_write_blocks_all_write_paths() {
    let mut reg = registry_with(&[post_with_rel_schema(), comment_schema()]);
    reg.set_deny_write_roles(vec!["guest".to_string()]);
    let g = ctx_of("u1", &["guest"]);
    const PERM: &str = "ERR_PERMISSION:";

    // insert（mutation 链路）
    let err = plan_mutation(
        "Post",
        &reg,
        Some(&g),
        &json!({ "title": "x" }),
        0,
        &["p_new_1".to_string()],
    )
    .expect_err("拒写清单命中应拒绝 insert");
    assert!(err.starts_with(PERM), "{err}");

    // updateMany（批量路径）
    let err = plan_update_many(
        "Post",
        &reg,
        Some(&g),
        &json!({ "_id": "p1" }),
        &json!({ "title": "y" }),
        0,
    )
    .expect_err("拒写清单命中应拒绝 updateMany");
    assert!(err.starts_with(PERM), "{err}");

    // 单条 update（check_write_perm 探针路径）
    let err = plan_update(
        "Post",
        &reg,
        Some(&g),
        &json!({ "_id": "p1" }),
        &json!({ "title": "y" }),
        &json!({}),
        0,
        Probe::NotProbed,
    )
    .expect_err("拒写清单命中应拒绝单条 update");
    assert!(err.starts_with(PERM), "{err}");
}
