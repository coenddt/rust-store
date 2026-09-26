//! 档位（profile）机制测试：standard（默认）/ text2query。
//!
//! 判决唯一在 core：`Profile` 枚举 + `Registry.profile` 读写；
//! 各门禁点按档分流（步骤 3 用例在下方逐项断言）。

use rust_store_core::command::{
    ensure_profile_ctx, ensure_route_override_allowed, forbid_t2q, plan_query, ERR_TEXT2QUERY,
    T2Q_MAX_DEPTH, T2Q_MAX_FEDERATION_ROWS, T2Q_MAX_ROWS,
};
use rust_store_core::federation::plan_federated;
use rust_store_core::permission::Context;
use rust_store_core::schema::{Profile, Registry};

use serde_json::{json, Map, Value};

fn params_of(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn post_registry() -> Registry {
    let mut reg = Registry::new();
    reg.register(&json!({
        "name": "Post",
        "collection": "posts",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
    }))
    .unwrap();
    reg
}

/// A → B → C → D → E 四级关系链（many，逐层 `$lookup`）
fn nested_registry() -> Registry {
    let mut reg = Registry::new();
    let chain = [("A", "as", "b", "B"), ("B", "bs", "c", "C"), ("C", "cs", "d", "D"), ("D", "ds", "e", "E")];
    for (name, coll, rel, model) in chain {
        reg.register(&json!({
            "name": name,
            "collection": coll,
            "timestamps": false,
            "fields": { "name": { "type": "string" }, "aId": { "type": "string" } },
            "relations": {
                rel: { "model": model, "type": "many", "localField": "_id", "foreignField": "aId" }
            },
        }))
        .unwrap();
    }
    reg.register(&json!({
        "name": "E",
        "collection": "es",
        "timestamps": false,
        "fields": { "name": { "type": "string" }, "aId": { "type": "string" } },
        "relations": {},
    }))
    .unwrap();
    reg
}

fn t2q_post() -> (Registry, Context) {
    let mut reg = post_registry();
    reg.set_profile(Profile::Text2Query);
    (reg, Context::system())
}

/// 从命令里取第一个 `$limit` 值（根级 pipeline 或首条 aggregate）
fn first_root_limit(plan: &rust_store_core::command::QueryPlan) -> Option<Value> {
    plan.commands
        .iter()
        .find_map(|c| c.get("pipeline"))
        .and_then(|p| p.as_array())
        .and_then(|a| a.iter().find_map(|s| s.get("$limit").cloned()))
}

#[test]
fn default_profile_is_standard() {
    let reg = Registry::new();
    assert_eq!(reg.profile(), Profile::Standard, "默认档位应为 standard");
    assert_eq!(reg.profile().as_str(), "standard");
}

#[test]
fn set_profile_roundtrips() {
    let mut reg = Registry::new();
    reg.set_profile(Profile::Text2Query);
    assert_eq!(reg.profile(), Profile::Text2Query);
    assert_eq!(reg.profile().as_str(), "text2query");

    reg.set_profile(Profile::Standard);
    assert_eq!(reg.profile(), Profile::Standard);
    assert_eq!(reg.profile().as_str(), "standard");
}

#[test]
fn profile_from_str_known_values() {
    assert_eq!(
        Profile::from_str_or_err("standard").unwrap(),
        Profile::Standard
    );
    assert_eq!(
        Profile::from_str_or_err("text2query").unwrap(),
        Profile::Text2Query
    );
}

#[test]
fn profile_from_str_unknown_is_err() {
    let err = Profile::from_str_or_err("x").expect_err("未知档位必须 Err（禁静默回落）");
    assert!(err.contains("未知 profile"), "错误信息异常: {err}");
    assert!(
        Profile::from_str_or_err("").is_err(),
        "空档位亦须 Err，不得回落 standard"
    );
}

// ─── 档位常量与门禁辅助 ─────────────────────────────────────

#[test]
#[allow(clippy::assertions_on_constants)] // 档位常量值断言（执行文档 §5 步骤 2 要求）
fn t2q_limits_are_stricter_than_standard() {
    use rust_store_core::command::MAX_PAGE_SIZE;
    use rust_store_core::federation::MAX_FEDERATION_ROWS;
    use rust_store_core::pipeline::MAX_DEPTH;

    assert_eq!(T2Q_MAX_ROWS, 1000.0);
    assert_eq!(T2Q_MAX_DEPTH, 3);
    assert_eq!(T2Q_MAX_FEDERATION_ROWS, 10_000);
    assert!(T2Q_MAX_ROWS < MAX_PAGE_SIZE, "text2query 行数上限须严于 standard");
    assert!(T2Q_MAX_DEPTH < MAX_DEPTH, "text2query 深度上限须严于 standard");
    assert!(
        T2Q_MAX_FEDERATION_ROWS < MAX_FEDERATION_ROWS,
        "text2query 联邦上限须严于 standard"
    );
}

#[test]
fn ensure_profile_ctx_forces_context_only_in_t2q() {
    let ctx = Context::system();

    let std_reg = Registry::new();
    ensure_profile_ctx(&std_reg, None).expect("standard 档缺 ctx 应放行");
    ensure_profile_ctx(&std_reg, Some(&ctx)).expect("standard 档带 ctx 应放行");

    let mut t2q_reg = Registry::new();
    t2q_reg.set_profile(Profile::Text2Query);
    let err = ensure_profile_ctx(&t2q_reg, None).expect_err("text2query 档缺 ctx 必须 Err");
    assert!(
        err.starts_with(ERR_TEXT2QUERY),
        "应携带 ERR_TEXT2QUERY 前缀: {err}"
    );
    ensure_profile_ctx(&t2q_reg, Some(&ctx)).expect("text2query 档带 ctx 应放行");
}

#[test]
fn forbid_t2q_blocks_only_in_t2q() {
    let std_reg = Registry::new();
    forbid_t2q(&std_reg, "$pipeline 直通").expect("standard 档不得拦截（放行 DB 独有能力）");

    let mut t2q_reg = Registry::new();
    t2q_reg.set_profile(Profile::Text2Query);
    let err = forbid_t2q(&t2q_reg, "$pipeline 直通").expect_err("text2query 档必须拒绝");
    assert!(
        err.starts_with(ERR_TEXT2QUERY) && err.contains("$pipeline 直通"),
        "应携带前缀并点明命中项: {err}"
    );
}

// ─── 门禁矩阵 #7：单次取数行数（含 $limit） ─────────────────────

#[test]
fn t2q_injects_root_limit_when_absent() {
    let (reg, ctx) = t2q_post();
    let plan = plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&ctx))
        .expect("应规划成功");
    assert_eq!(
        first_root_limit(&plan),
        Some(json!(1000)),
        "text2query 档省略 $limit 应注入上限 T2Q_MAX_ROWS"
    );

    let std = plan_query("Post{ title }", &params_of(json!({})), &post_registry(), None)
        .expect("standard 档应规划成功");
    assert_eq!(
        first_root_limit(&std),
        None,
        "standard 档省略 $limit 不得注入（纯 $match → find 快路径）"
    );
}

#[test]
fn t2q_clamps_root_limit_over_cap() {
    let (reg, ctx) = t2q_post();
    let plan = plan_query(
        "Post($limit:@l){ title }",
        &params_of(json!({ "l": 5000 })),
        &reg,
        Some(&ctx),
    )
    .expect("应规划成功");
    assert_eq!(
        first_root_limit(&plan),
        Some(json!(1000)),
        "text2query 档 $limit(5000) 应夹到 T2Q_MAX_ROWS"
    );

    let std = plan_query(
        "Post($limit:@l){ title }",
        &params_of(json!({ "l": 5000 })),
        &post_registry(),
        None,
    )
    .expect("standard 档应规划成功");
    assert_eq!(
        first_root_limit(&std),
        Some(json!(5000)),
        "standard 档 $limit 不得夹（不封顶）"
    );
}

#[test]
fn t2q_clamps_relation_level_limit() {
    // 取关系 `$lookup` 内层 pipeline 的 `$limit`（可为两阶段：需跨 commands 找）
    fn inner_limit(plan: &rust_store_core::command::QueryPlan) -> Option<Value> {
        plan.to_value()["commands"]
            .as_array()
            .and_then(|cmds| {
                cmds.iter()
                    .filter_map(|c| c.get("pipeline").and_then(|x| x.as_array()))
                    .flatten()
                    .find_map(|s| s.get("$lookup"))
                    .and_then(|lo| lo["pipeline"].as_array())
                    .and_then(|a| a.iter().find_map(|s| s.get("$limit").cloned()))
            })
    }

    let mut reg = nested_registry();
    reg.set_profile(Profile::Text2Query);
    let plan = plan_query(
        "A{ _id, b($limit:@l){ _id } }",
        &params_of(json!({ "l": 5000 })),
        &reg,
        Some(&Context::system()),
    )
    .expect("应规划成功");
    assert_eq!(
        inner_limit(&plan),
        Some(json!(1000)),
        "text2query 档关系级 $limit(5000) 应夹到 T2Q_MAX_ROWS"
    );

    let std = plan_query(
        "A{ _id, b($limit:@l){ _id } }",
        &params_of(json!({ "l": 5000 })),
        &nested_registry(),
        None,
    )
    .expect("standard 档应规划成功");
    assert_eq!(
        inner_limit(&std),
        Some(json!(5000)),
        "standard 档关系级 $limit 不得夹"
    );
}

// ─── 门禁矩阵 #8：关系嵌套深度 ────────────────────────────────

const DEEP_GQL: &str = "A{ _id, b{ _id, c{ _id, d{ _id, e{ _id } } } } }";

#[test]
fn t2q_rejects_deep_nesting_standard_passes() {
    let mut reg = nested_registry();
    reg.set_profile(Profile::Text2Query);
    let err = plan_query(DEEP_GQL, &params_of(json!({})), &reg, Some(&Context::system()))
        .expect_err("text2query 档超深嵌套必须显式报错");
    assert!(
        err.starts_with(ERR_TEXT2QUERY),
        "应携带 ERR_TEXT2QUERY 前缀: {err}"
    );

    plan_query(
        DEEP_GQL,
        &params_of(json!({})),
        &nested_registry(),
        Some(&Context::system()),
    )
    .expect("standard 档（深度上限 10）应收敛规划成功");
}

// ─── 门禁矩阵 #11：用户上下文强制 ────────────────────────────

#[test]
fn t2q_forces_user_context() {
    let mut reg = post_registry();
    reg.set_profile(Profile::Text2Query);
    let err = plan_query("Post{ title }", &params_of(json!({})), &reg, None)
        .expect_err("text2query 档缺 ctx 必须报错");
    assert!(
        err.starts_with(ERR_TEXT2QUERY),
        "应携带 ERR_TEXT2QUERY 前缀: {err}"
    );

    plan_query("Post{ title }", &params_of(json!({})), &post_registry(), None)
        .expect("standard 档缺 ctx 默认放行（fail-open）");
}

// ─── 门禁矩阵 #9：联邦单源行数 ────────────────────────────────

#[test]
fn federation_row_cap_follows_profile() {
    let (t2q, ctx) = t2q_post();
    let plan = plan_federated(
        "Post{ title }",
        &params_of(json!({})),
        &t2q,
        Some(&ctx),
        &json!({}),
    )
    .expect("联邦应规划成功");
    assert_eq!(
        plan["maxRowsPerSource"],
        json!(10_000),
        "text2query 档联邦单源上限应为 T2Q_MAX_FEDERATION_ROWS"
    );

    let std = plan_federated(
        "Post{ title }",
        &params_of(json!({})),
        &post_registry(),
        None,
        &json!({}),
    )
    .expect("联邦应规划成功");
    assert_eq!(
        std["maxRowsPerSource"],
        json!(100_000),
        "standard 档联邦单源上限应保持 MAX_FEDERATION_ROWS"
    );
}

#[test]
fn route_override_gate_follows_profile() {
    let mut reg = post_registry();
    // standard 档：携带与否均放行（route_override 为受信服务端参数，可用）
    assert!(ensure_route_override_allowed(&reg, true).is_ok());
    assert!(ensure_route_override_allowed(&reg, false).is_ok());

    reg.set_profile(Profile::Text2Query);
    // text2query 档：不携带放行；携带即拒（受信来源门禁，CWE-639）
    assert!(ensure_route_override_allowed(&reg, false).is_ok());
    let err = ensure_route_override_allowed(&reg, true).unwrap_err();
    assert!(err.starts_with(ERR_TEXT2QUERY), "应为档位哨兵前缀: {err}");
    assert!(err.contains("route_override"), "文案应指名 route_override: {err}");
}
