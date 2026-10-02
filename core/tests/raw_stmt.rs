//! 原生 SQL 语句编译（`compile_raw_stmt`）单测：fixture 表驱动 + 边界补充。
//!
//! fixture 为三侧同构契约（Rust/Py/Js），本文件是权威消费方；双宿主测试本地硬编码
//! 对齐用例，不跨仓读此文件（见执行文档 01 §C2）。

use serde_json::{json, Value};

use rust_store_core::dialect::{compile_raw_stmt, Backend};

const FIXTURE: &str = include_str!("../../fixtures/host/raw_stmts.json");

/// fixture 全矩阵表驱动（R1–R9）
#[test]
fn fixture_table_driven() {
    let doc: Value = serde_json::from_str(FIXTURE).expect("fixture 应为合法 JSON");
    let cases = doc["cases"].as_array().expect("cases 应为数组");
    assert!(
        cases.len() >= 14,
        "覆盖矩阵 ≥14 例，实际 {} 例",
        cases.len()
    );
    for case in cases {
        let name = case["name"].as_str().expect("case 缺 name");
        let backend = Backend::parse(case["backend"].as_str().expect("case 缺 backend"))
            .unwrap_or_else(|e| panic!("[{}] backend 解析失败: {}", name, e));
        let is_write = case["isWrite"].as_bool();
        let result = compile_raw_stmt(
            backend,
            case["sql"].as_str().expect("case 缺 sql"),
            case["params"].clone(),
            is_write,
        );
        match case["error"].as_str() {
            // R1/R6/R7 错误文案断言（contains 子串）
            Some(expected_err) => {
                let err = result.expect_err(&format!("[{}] 应报错", name));
                assert!(
                    err.contains(expected_err),
                    "[{}] 错误文案不含 {:?}：{}",
                    name,
                    expected_err,
                    err
                );
            }
            None => {
                let stmt = result.unwrap_or_else(|e| panic!("[{}] 应编译成功: {}", name, e));
                let expected = &case["expected"];
                assert_eq!(
                    stmt.sql,
                    expected["sql"].as_str().expect("expected 缺 sql"),
                    "[{}] sql 不符",
                    name
                );
                assert_eq!(
                    Value::from(stmt.params),
                    expected["params"],
                    "[{}] params 不符",
                    name
                );
                assert_eq!(
                    stmt.is_write,
                    expected["isWrite"].as_bool().expect("expected 缺 isWrite"),
                    "[{}] isWrite 不符",
                    name
                );
            }
        }
    }
}

/// R4：单引号翻倍转义（`''`）后仍在字符串内，内部冒号不编译
#[test]
fn doubled_single_quote_keeps_colon_literal() {
    let stmt = compile_raw_stmt(
        Backend::Mysql,
        "SELECT * FROM t WHERE note = 'it''s :notparam' AND id = :id",
        json!({ "id": 7 }),
        None,
    )
    .expect("应编译成功");
    assert_eq!(
        stmt.sql,
        "SELECT * FROM t WHERE note = 'it''s :notparam' AND id = ?"
    );
    assert_eq!(stmt.params, vec![json!(7)]);
}

/// R9：backend 非法由 `Backend::parse` Err 透传（编译函数不重复校验）
#[test]
fn invalid_backend_rejected_by_parse() {
    assert!(Backend::parse("oracle").is_err());
}
