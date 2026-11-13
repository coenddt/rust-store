//! 「空名单 + `denyRead`」跨分支契约测试（主干与 `release/5.x` 共用同一份断言）。
//!
//! 定稿语义（两线必须一致）：
//! - `read: []` = 未配置 = 不限制（5.x 旧义，按 `unconfigured` 姿态）；
//! - `denyRead: true` = 谁都不许读（显式意图，优先于 `read: []`、`internal`、豁免清单）；
//! - `ctx = None`（系统内部调用）保持 fail-open，不受 `denyRead` 影响；
//! - `denyRead` 只接受布尔，`null` / 字符串 / 数字 / 数组在注册期显式 Err。

use serde_json::{json, Value};

use rust_store_core::command::{plan_count, plan_exists_with_ctx, plan_query};
use rust_store_core::permission::{can_read_schema, Context, UnconfiguredPolicy};
use rust_store_core::schema::Registry;

// ─── fixtures ────────────────────────────────────────────────

/// 无 `read` 键的模型（未配置）。
fn open_schema() -> Value {
    json!({
        "name": "OpenDoc",
        "collection": "open_docs",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {}
    })
}

/// `read: []`（5.x 旧义：空名单 = 不限制）。
fn empty_read_schema() -> Value {
    json!({
        "name": "EmptyRead",
        "collection": "empty_read",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "read": []
    })
}

/// `denyRead: true` 且叠加 `read: []`（并存时以显式拒绝为准）。
fn deny_read_schema() -> Value {
    json!({
        "name": "DenyDoc",
        "collection": "deny_docs",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "read": [],
        "denyRead": true
    })
}

fn registry_with(defns: &[Value]) -> Registry {
    let mut reg = Registry::new();
    for d in defns {
        reg.register(d).expect("schema 注册应成功");
    }
    reg
}

fn ctx_of(roles: &[&str]) -> Context {
    Context {
        user_id: Some("u1".to_string()),
        roles: Some(roles.iter().map(|r| r.to_string()).collect()),
        ..Default::default()
    }
}

fn internal_ctx() -> Context {
    Context {
        user_id: Some("sys".to_string()),
        internal: true,
        ..Default::default()
    }
}

fn params_of(extra: Value) -> serde_json::Map<String, Value> {
    extra.as_object().cloned().unwrap_or_default()
}

// ─── 用例 ────────────────────────────────────────────────────

/// `read: []` 放行所有角色（含 internal 与无 ctx）。
#[test]
fn empty_read_list_allows_every_role() {
    let reg = registry_with(&[empty_read_schema()]);
    let schema = reg.get("EmptyRead").expect("已注册");
    for roles in [&["guest"][..], &["viewer"], &["admin"], &[]] {
        let ctx = ctx_of(roles);
        assert!(
            can_read_schema(reg.role_rules(), schema, Some(&ctx)),
            "read: [] 应放行角色 {roles:?}（5.x 旧义：空名单 = 未配置）"
        );
    }
    assert!(
        can_read_schema(reg.role_rules(), schema, Some(&internal_ctx())),
        "read: [] 应放行 internal"
    );
    assert!(
        can_read_schema(reg.role_rules(), schema, None),
        "read: [] 在 ctx=None 时应放行（fail-open）"
    );
}

/// `read: []` 与键缺失同义：由 `unconfigured` 姿态决定（Open 放行 / Closed 拒绝）。
#[test]
fn empty_read_list_equals_unconfigured() {
    let mut reg = registry_with(&[empty_read_schema(), open_schema()]);
    let ctx = ctx_of(&["guest"]);
    {
        let empty = reg.get("EmptyRead").expect("已注册");
        assert!(can_read_schema(reg.role_rules(), empty, Some(&ctx)));
    }
    reg.set_unconfigured_policy(UnconfiguredPolicy::Closed);
    let empty = reg.get("EmptyRead").expect("已注册");
    let open = reg.get("OpenDoc").expect("已注册");
    assert!(
        !can_read_schema(reg.role_rules(), empty, Some(&ctx)),
        "Closed 姿态下空名单应与未配置同判：拒绝"
    );
    assert!(
        !can_read_schema(reg.role_rules(), open, Some(&ctx)),
        "Closed 姿态下未配置应拒绝"
    );
    assert!(
        can_read_schema(reg.role_rules(), empty, None),
        "ctx=None 不受 unconfigured 姿态影响（内部调用 fail-open）"
    );
}

/// `denyRead: true` 拒绝一切角色（含 internal），`ctx=None` 例外。
#[test]
fn deny_read_rejects_every_role() {
    let reg = registry_with(&[deny_read_schema()]);
    let schema = reg.get("DenyDoc").expect("已注册");
    for roles in [
        &["guest"][..],
        &["viewer"],
        &["admin"],
        &["admin", "owner"],
        &[],
    ] {
        let ctx = ctx_of(roles);
        assert!(
            !can_read_schema(reg.role_rules(), schema, Some(&ctx)),
            "denyRead: true 应拒绝角色 {roles:?}"
        );
    }
    assert!(
        !can_read_schema(reg.role_rules(), schema, Some(&internal_ctx())),
        "denyRead: true 应优先于 internal"
    );
    assert!(
        can_read_schema(reg.role_rules(), schema, None),
        "ctx=None 保持既有 fail-open 契约"
    );
}

/// `read: []` 与 `denyRead: true` 并存 ⇒ 显式拒绝生效。
#[test]
fn deny_read_wins_over_empty_read_list() {
    let reg = registry_with(&[deny_read_schema()]);
    let schema = reg.get("DenyDoc").expect("已注册");
    assert_eq!(schema.read.as_deref(), Some(&[][..]), "前置：read 为空名单");
    assert!(schema.deny_read, "前置：denyRead 已解析");
    assert!(
        !can_read_schema(reg.role_rules(), schema, Some(&ctx_of(&["admin"]))),
        "并存时应以 denyRead 为准（显式意图优先）"
    );
}

/// `denyRead` 类型校验：非布尔一律注册期 Err（`null` 不静默当 false）。
#[test]
fn deny_read_must_be_boolean() {
    for bad in [json!("true"), json!(1), json!([]), json!(null)] {
        let mut defn = open_schema();
        defn["denyRead"] = bad.clone();
        let err = Registry::new()
            .register(&defn)
            .expect_err("非布尔 denyRead 应注册期报错");
        assert!(
            err.contains("denyRead 必须为布尔值"),
            "错误文案应点名 denyRead，实际 {err}"
        );
    }
    // 显式 false 合法且等价于缺省。
    let mut defn = open_schema();
    defn["denyRead"] = json!(false);
    let reg = registry_with(&[defn]);
    let schema = reg.get("OpenDoc").expect("已注册");
    assert!(!schema.deny_read);
}

/// 全部读路径同判：query / count / exists 均被 `denyRead` 拦截。
#[test]
fn deny_read_blocks_query_count_and_exists() {
    let reg = registry_with(&[deny_read_schema(), empty_read_schema()]);
    let admin = ctx_of(&["admin"]);

    let err = plan_query(
        "DenyDoc{ title }",
        &params_of(json!({})),
        &reg,
        Some(&admin),
    )
    .expect_err("query 应被 denyRead 拒绝");
    assert!(err.starts_with("ERR_PERMISSION"), "query: {err}");
    let err =
        plan_count("DenyDoc", &reg, None, Some(&admin)).expect_err("count 应被 denyRead 拒绝");
    assert!(err.starts_with("ERR_PERMISSION"), "count: {err}");
    let err = plan_exists_with_ctx("DenyDoc", &reg, &json!({}), Some(&admin))
        .expect_err("exists 应被 denyRead 拒绝");
    assert!(err.starts_with("ERR_PERMISSION"), "exists: {err}");

    // 对照：`read: []` 三条路径都放行（与上面同命令、仅换模型）。
    plan_query(
        "EmptyRead{ title }",
        &params_of(json!({})),
        &reg,
        Some(&admin),
    )
    .expect("read: [] 的 query 应放行");
    plan_count("EmptyRead", &reg, None, Some(&admin)).expect("read: [] 的 count 应放行");
    plan_exists_with_ctx("EmptyRead", &reg, &json!({}), Some(&admin))
        .expect("read: [] 的 exists 应放行");
}
