//! X1：SQL 列序必须与 schema 声明序一致。
//!
//! 此前 `writable_cols` / `writable_cols_union` 取 `schema.fields.keys()`
//! （HashMap 随机迭代序），同一 schema 跨进程、跨运行列序都可能不同，
//! 导致 SQL 文本不可复现（缓存键、审计比对、golden 全部失效）。
//!
//! 本测试走公开 `translate` 面，不依赖 crate 内部函数：注册 12 字段 schema，
//! 翻译一条 insertOne，断言发射的列序严格等于声明序。

use serde_json::{json, Value};

use rust_store_core::dialect::{translate, Backend};
use rust_store_core::schema::Registry;

fn registry_with_wide_schema() -> Registry {
    let mut reg = Registry::new();
    reg.register(&json!({
        "name": "Wide",
        "collection": "wide",
        "idPrefix": "w",
        "timestamps": false,
        "fields": {
            "f01": { "type": "string" }, "f02": { "type": "string" },
            "f03": { "type": "string" }, "f04": { "type": "string" },
            "f05": { "type": "string" }, "f06": { "type": "string" },
            "f07": { "type": "string" }, "f08": { "type": "string" },
            "f09": { "type": "string" }, "f10": { "type": "string" },
            "f11": { "type": "string" }, "f12": { "type": "string" }
        },
        "relations": {},
    }))
    .expect("注册应成功");
    reg
}

fn expected_field_names() -> Vec<String> {
    (1..=12).map(|i| format!("f{i:02}")).collect()
}

#[test]
fn insert_sql_column_order_follows_declaration() {
    let reg = registry_with_wide_schema();
    let mut doc = serde_json::Map::new();
    doc.insert("_id".to_string(), json!("w1"));
    for k in expected_field_names() {
        doc.insert(k, json!("v"));
    }
    let cmd = json!({ "kind": "insertOne", "collection": "wide", "doc": Value::Object(doc) });

    for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
        let out = translate(backend, &cmd, &reg).expect("translate 应成功");
        let text = out["stmts"][0]["text"].as_str().unwrap_or("");
        // 去掉外层 `INSERT INTO <table> (` 与 `) VALUES ...`
        let start = text.find('(').expect("SQL 应含列清单") + 1;
        let end = text[start..].find(')').expect("列清单应闭合") + start;
        let cols: Vec<String> = text[start..end]
            .split(',')
            .map(|s| s.trim().trim_matches(['`', '"']).to_string())
            .collect();
        let mut expected = vec!["_id".to_string()];
        expected.extend(expected_field_names());
        expected.push("__present".to_string());
        assert_eq!(
            cols, expected,
            "[{backend:?}] SQL 列序必须按声明序（X1），实际 SQL: {text}"
        );
    }
}

#[test]
fn insert_many_sql_column_order_follows_declaration() {
    let reg = registry_with_wide_schema();
    let full: serde_json::Map<String, Value> = expected_field_names()
        .into_iter()
        .map(|k| (k, json!("v")))
        .collect();
    // 第二篇只带后半段字段：并集列序仍须按声明序，不得因文档顺序改变
    let mut partial = serde_json::Map::new();
    for k in ["f07", "f08", "f09", "f10", "f11", "f12"] {
        partial.insert(k.to_string(), json!("v"));
    }
    let cmd = json!({
        "kind": "insertMany",
        "collection": "wide",
        "docs": [ Value::Object(full), Value::Object(partial) ]
    });

    let out = translate(Backend::Postgres, &cmd, &reg).expect("translate 应成功");
    let text = out["stmts"][0]["text"].as_str().unwrap_or("");
    let start = text.find('(').expect("SQL 应含列清单") + 1;
    let end = text[start..].find(')').expect("列清单应闭合") + start;
    let cols: Vec<String> = text[start..end]
        .split(',')
        .map(|s| s.trim().trim_matches(['`', '"']).to_string())
        .collect();
    let mut expected = expected_field_names();
    expected.push("__present".to_string());
    assert_eq!(
        cols, expected,
        "insertMany 并集列序必须按声明序（X1），实际 SQL: {text}"
    );
}
