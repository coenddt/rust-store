//! 扩展守卫测试：timestamps 值校验 + 用户 `$pipeline` 禁用开关
//!
//! 对应宿主接入诉求（AI 查询宿主）：
//!   ① schema 可声明秒级时间戳（`timestamps: 's'`），非法值注册即报错；
//!   ② Registry 级关闭用户 `$pipeline` 直通后，三条规划路径（单库 / 带计数 / 联邦）
//!      均显式报错，重新打开即恢复。

use serde_json::{json, Map, Value};

use rust_store_core::command::{plan_query, plan_query_with_count};
use rust_store_core::federation::plan_federated;
use rust_store_core::schema::Registry;

fn registry_with(defn: Value) -> Registry {
    let mut reg = Registry::new();
    reg.register(&defn).expect("schema 注册应成功");
    reg
}

fn base_schema(timestamps: Value) -> Value {
    json!({
        "name": "Post",
        "collection": "posts",
        "timestamps": timestamps,
        "fields": { "title": { "type": "string" } },
        "relations": {},
    })
}

fn params_of(extra: Value) -> Map<String, Value> {
    extra.as_object().cloned().unwrap_or_default()
}

// ─── timestamps 值校验 ───────────────────────────────────────

#[test]
fn timestamps_accepts_bool_and_units() {
    for v in [json!(true), json!(false), json!("ms"), json!("s"), Value::Null] {
        let mut reg = Registry::new();
        reg.register(&base_schema(v.clone()))
            .unwrap_or_else(|e| panic!("timestamps = {} 应可注册: {}", v, e));
    }
}

#[test]
fn timestamps_rejects_invalid_values() {
    for v in [json!("years"), json!(1)] {
        let mut reg = Registry::new();
        let err = reg
            .register(&base_schema(v.clone()))
            .expect_err("非法 timestamps 应报错");
        assert!(err.contains("timestamps 仅支持"), "错误信息异常: {}", err);
    }
}

// ─── 用户 $pipeline 开关 ─────────────────────────────────────

const PIPELINE_GQL: &str = "Post($pipeline:@p){ title }";

fn pipeline_params() -> Map<String, Value> {
    params_of(json!({ "p": [ { "$match": {} } ] }))
}

#[test]
fn user_pipeline_allowed_by_default() {
    let reg = registry_with(base_schema(json!(true)));
    plan_query(PIPELINE_GQL, &pipeline_params(), &reg, None)
        .unwrap_or_else(|e| panic!("默认应放行用户 $pipeline: {}", e));
}

#[test]
fn user_pipeline_disabled_blocks_all_plan_paths() {
    let mut reg = registry_with(base_schema(json!(true)));
    reg.set_allow_user_pipeline(false);

    let err = plan_query(PIPELINE_GQL, &pipeline_params(), &reg, None)
        .expect_err("plan_query 应报错");
    assert!(err.contains("已被禁用"), "错误信息异常: {}", err);

    let err = plan_query_with_count(PIPELINE_GQL, &pipeline_params(), &reg, None)
        .expect_err("plan_query_with_count 应报错");
    assert!(err.contains("已被禁用"), "错误信息异常: {}", err);

    let err = plan_federated(PIPELINE_GQL, &pipeline_params(), &reg, None, &json!({}))
        .expect_err("plan_federated 应报错");
    assert!(err.contains("已被禁用"), "错误信息异常: {}", err);
}

#[test]
fn user_pipeline_switch_is_reversible() {
    let mut reg = registry_with(base_schema(json!(true)));
    reg.set_allow_user_pipeline(false);
    assert!(plan_query(PIPELINE_GQL, &pipeline_params(), &reg, None).is_err());
    reg.set_allow_user_pipeline(true);
    plan_query(PIPELINE_GQL, &pipeline_params(), &reg, None).expect("重新打开后应放行");
}

// ─── queryOne `$limit(1)` 下推 + 权限错误哨兵前缀 ────────────

use rust_store_core::command::{plan_query_one, Mode};
use rust_store_core::permission::Context;

#[test]
fn query_one_pushes_limit_one_when_absent() {
    let reg = registry_with(base_schema(json!(true)));
    let plan =
        plan_query_one("Post{ title }", &params_of(json!({})), &reg, None).expect("应规划成功");
    assert_eq!(plan.mode, Mode::Aggregate, "注入 limit 后应走标准聚合");
    let pipeline = plan.commands[0]["pipeline"].as_array().unwrap();
    assert!(
        pipeline
            .iter()
            .any(|s| s.get("$limit").map(|v| v == &json!(1)).unwrap_or(false)),
        "pipeline 应包含下推的 $limit(1)"
    );
}

#[test]
fn query_one_keeps_user_limit() {
    let reg = registry_with(base_schema(json!(true)));
    let plan = plan_query_one(
        "Post($limit:@l){ title }",
        &params_of(json!({ "l": 7 })),
        &reg,
        None,
    )
    .expect("应规划成功");
    let pipeline = plan.commands[0]["pipeline"].as_array().unwrap();
    assert!(
        pipeline
            .iter()
            .any(|s| s.get("$limit").map(|v| v == &json!(7)).unwrap_or(false)),
        "保留用户 $limit(7)"
    );
    assert!(
        pipeline
            .iter()
            .all(|s| !s.get("$limit").map(|v| v == &json!(1)).unwrap_or(false)),
        "不应注入 $limit(1)"
    );
}

#[test]
fn query_one_skips_injection_for_custom_pipeline() {
    let reg = registry_with(base_schema(json!(true)));
    let plan = plan_query_one(PIPELINE_GQL, &pipeline_params(), &reg, None).expect("应规划成功");
    assert_eq!(plan.mode, Mode::CustomPipeline);
    let pipeline = plan.commands[0]["pipeline"].as_array().unwrap();
    assert!(
        pipeline.iter().all(|s| s.get("$limit").is_none()),
        "$pipeline 全权模式不应注入 $limit"
    );
}

#[test]
fn permission_errors_carry_stable_prefix() {
    use rust_store_core::command::{ERR_NO_WRITE, ERR_PERM_PREFIX};

    // guest 角色写 schema → 权限错误；断言稳定前缀（Host 按前缀映射 PermissionError）
    let reg = registry_with(base_schema(json!(true)));
    let ctx = Context {
        roles: Some(vec!["guest".to_string()]),
        ..Default::default()
    };
    let err = rust_store_core::command::plan_mutation("Post", &reg, Some(&ctx), &json!({}), 0, &[])
        .expect_err("guest 写入应报错");
    assert!(
        err.starts_with(ERR_PERM_PREFIX),
        "权限错误应携带稳定前缀: {err}"
    );
    assert_eq!(ERR_NO_WRITE, format!("{ERR_PERM_PREFIX}无写入权限"));
}

// ─── require_context fail-secure 开关（默认关闭 = JS parity） ──

use rust_store_core::command::{
    plan_insert, plan_insert_many, plan_mutation, plan_remove, plan_update, plan_update_many,
    plan_upsert, Probe, ERR_NO_CONTEXT,
};

const PLAIN_GQL: &str = "Post{ title }";

fn no_ctx_err(label: &str, result: Result<impl Sized + std::fmt::Debug, String>) {
    let err = result.expect_err(&format!("{label} 在 require_context 下应报错"));
    assert!(err.starts_with(ERR_NO_CONTEXT), "{label} 应报 ERR_NO_CONTEXT: {err}");
}

#[test]
fn missing_context_allowed_by_default() {
    // 默认 fail-open（与 JS parity）：无 ctx 照常规划
    let reg = registry_with(base_schema(json!(true)));
    plan_query(PLAIN_GQL, &params_of(json!({})), &reg, None)
        .unwrap_or_else(|e| panic!("默认无 ctx 应放行: {e}"));
    assert!(!reg.require_context(), "开关默认关闭");
}

#[test]
fn require_context_blocks_read_paths_without_ctx() {
    let mut reg = registry_with(base_schema(json!(true)));
    reg.set_require_context(true);

    no_ctx_err("plan_query", plan_query(PLAIN_GQL, &params_of(json!({})), &reg, None));
    no_ctx_err("plan_query_one", plan_query_one(PLAIN_GQL, &params_of(json!({})), &reg, None));
    no_ctx_err(
        "plan_query_with_count",
        plan_query_with_count(PLAIN_GQL, &params_of(json!({})), &reg, None),
    );
    no_ctx_err(
        "plan_federated",
        plan_federated(PLAIN_GQL, &params_of(json!({})), &reg, None, &json!({})),
    );
}

#[test]
fn require_context_blocks_write_paths_without_ctx() {
    let mut reg = registry_with(base_schema(json!(true)));
    reg.set_require_context(true);

    no_ctx_err(
        "plan_insert",
        plan_insert("Post", &reg, None, &json!({ "title": "x" }), 0, "p1", None),
    );
    no_ctx_err(
        "plan_insert_many",
        plan_insert_many("Post", &reg, None, &[json!({ "title": "x" })], 0, &[], None),
    );
    no_ctx_err(
        "plan_mutation",
        plan_mutation("Post", &reg, None, &json!({ "title": "x" }), 0, &[]),
    );
    no_ctx_err(
        "plan_update",
        plan_update("Post", &reg, None, &json!({}), &json!({ "title": "y" }), &json!({}), 0, Probe::NotProbed),
    );
    no_ctx_err(
        "plan_update_many",
        plan_update_many("Post", &reg, None, &json!({}), &json!({ "title": "y" }), 0),
    );
    no_ctx_err(
        "plan_remove",
        plan_remove("Post", &reg, None, &json!({}), Probe::NotProbed),
    );
    no_ctx_err(
        "plan_upsert",
        plan_upsert("Post", &reg, None, &json!({}), &json!({ "title": "z" }), &json!({}), 0, "p2"),
    );
}

#[test]
fn system_context_passes_require_context() {
    // 显式系统上下文 = 内部调用：require_context 下照常放行（读 + 写）
    let mut reg = registry_with(base_schema(json!(true)));
    reg.set_require_context(true);
    let sys = Context::system();

    plan_query(PLAIN_GQL, &params_of(json!({})), &reg, Some(&sys))
        .unwrap_or_else(|e| panic!("系统上下文读应放行: {e}"));
    plan_insert("Post", &reg, Some(&sys), &json!({ "title": "x" }), 0, "p1", None)
        .unwrap_or_else(|e| panic!("系统上下文写应放行: {e}"));
}

#[test]
fn require_context_switch_is_reversible() {
    let mut reg = registry_with(base_schema(json!(true)));
    reg.set_require_context(true);
    assert!(plan_query(PLAIN_GQL, &params_of(json!({})), &reg, None).is_err());
    reg.set_require_context(false);
    plan_query(PLAIN_GQL, &params_of(json!({})), &reg, None).expect("重新关闭后应放行");
}
