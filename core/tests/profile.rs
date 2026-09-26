//! 档位（profile）机制测试：standard（默认）/ text2query。
//!
//! 判决唯一在 core：`Profile` 枚举 + `Registry.profile` 读写；
//! 各门禁点按档分流（步骤 3 用例在下方逐项断言）。

use rust_store_core::command::{
    ensure_profile_ctx, forbid_t2q, ERR_TEXT2QUERY, T2Q_MAX_DEPTH, T2Q_MAX_FEDERATION_ROWS,
    T2Q_MAX_ROWS,
};
use rust_store_core::permission::Context;
use rust_store_core::schema::{Profile, Registry};

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
