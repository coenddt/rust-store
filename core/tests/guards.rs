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
