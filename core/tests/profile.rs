//! 档位（profile）机制测试：standard（默认）/ text2query。
//!
//! 判决唯一在 core：`Profile` 枚举 + `Registry.profile` 读写；
//! 各门禁点按档分流（步骤 3 用例在下方逐项断言）。

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
