//! 向量字段（VR-01）：schema 属性保留 / 注册期校验 / 写入维度校验 / 读回标记。
//!
//! 覆盖总纲验收 A1（schema 校验）与 A3 的 core 侧（写入序列化 + 读回为 JSON 数组）。
//! 契约来源：执行文档 `向量检索扩展-01` §4。

use serde_json::{json, Value};

use rust_store_core::dialect::{translate, Backend};
use rust_store_core::schema::Registry;

/// 注册一个含 vector 字段的 schema（默认 1024 维 cosine）
fn reg_with(field: Value) -> Result<Registry, String> {
    let mut r = Registry::new();
    r.register(&json!({
        "name": "Doc",
        "collection": "docs",
        "timestamps": false,
        "fields": { "embedding": field, "title": { "type": "string" } },
        "relations": {}
    }))?;
    Ok(r)
}

fn reg() -> Registry {
    reg_with(json!({ "type": "vector", "dim": 1024 })).unwrap()
}

fn insert(backend: Backend, schema: &Registry, doc: Value) -> Result<Value, String> {
    translate(
        backend,
        &json!({ "kind": "insertOne", "collection": "docs", "doc": doc }),
        schema,
    )
}

fn first_stmt(v: &Value) -> &Value {
    &v["stmts"][0]
}

// ─── A1：dim 合法性 ───────────────────────────────────────────

#[test]
fn dim_missing_is_rejected() {
    let err = reg_with(json!({ "type": "vector" })).unwrap_err();
    assert!(err.contains("embedding"), "错误文案须含字段名：{err}");
    assert!(err.contains("dim"), "错误文案须说明缺 dim：{err}");
}

#[test]
fn dim_zero_negative_float_string_are_rejected() {
    for bad in [json!(0), json!(-1), json!(1024.5), json!("1024")] {
        let err = reg_with(json!({ "type": "vector", "dim": bad })).unwrap_err();
        assert!(err.contains("embedding"), "错误文案须含字段名：{err}");
        assert!(err.contains("dim"), "错误文案须说明 dim 非法：{err}");
    }
}

#[test]
fn dim_upper_bound() {
    let err = reg_with(json!({ "type": "vector", "dim": 16001 })).unwrap_err();
    assert!(err.contains("超过上限"), "超过上限须显式报错：{err}");
    // 16000 是上限本身（可存），必须通过
    reg_with(json!({ "type": "vector", "dim": 16000 })).unwrap();
}

// ─── A1：metric 合法性 ───────────────────────────────────────

#[test]
fn metric_values() {
    for bad in [json!("COSINE"), json!("euclidean"), json!(1)] {
        let err = reg_with(json!({ "type": "vector", "dim": 8, "metric": bad })).unwrap_err();
        assert!(err.contains("embedding"), "错误文案须含字段名：{err}");
        assert!(err.contains("metric"), "错误文案须说明 metric 非法：{err}");
    }
    for ok in ["cosine", "l2", "ip"] {
        reg_with(json!({ "type": "vector", "dim": 8, "metric": ok })).unwrap();
    }
}

#[test]
fn metric_defaults_to_cosine() {
    let r = reg();
    let f = r.get("Doc").unwrap().fields.get("embedding").unwrap();
    assert_eq!(f.dim, Some(1024));
    assert_eq!(f.metric_or_default(), "cosine");
}

#[test]
fn non_vector_rejects_dim_and_metric() {
    let err = reg_with(json!({ "type": "string", "dim": 8 })).unwrap_err();
    assert!(err.contains("不接受 dim"), "{err}");
    let err = reg_with(json!({ "type": "string", "metric": "l2" })).unwrap_err();
    assert!(err.contains("不接受 metric"), "{err}");
}

// ─── A3：写入维度校验 ───────────────────────────────────────

#[test]
fn write_dim_mismatch_is_rejected() {
    let r = reg();
    let err = insert(Backend::Sqlite, &r, json!({ "embedding": [0.1, 0.2, 0.3] })).unwrap_err();
    assert!(err.contains("期望 1024，实际 3"), "维度文案须含期望与实际：{err}");
}

#[test]
fn write_non_array_and_non_numeric_are_rejected() {
    let r = reg();
    let err = insert(Backend::Sqlite, &r, json!({ "embedding": "not-an-array" })).unwrap_err();
    assert!(err.contains("必须是数组"), "{err}");

    let mut bad = vec![json!(0.1); 1023];
    bad.push(json!("x"));
    let err = insert(Backend::Sqlite, &r, json!({ "embedding": bad })).unwrap_err();
    assert!(err.contains("必须全部为数值"), "{err}");
}

#[test]
fn write_serializes_vector_as_json_text() {
    let r = reg_with(json!({ "type": "vector", "dim": 2 })).unwrap();
    let out = insert(Backend::Sqlite, &r, json!({ "embedding": [0.1, 0.2] })).unwrap();
    let stmt = first_stmt(&out);
    assert_eq!(
        stmt["params"],
        json!(["[0.1,0.2]", ",embedding,"]),
        "vector 须落 JSON 文本参数（第二个参数为 __present 哨兵）"
    );
}

#[test]
fn pg_binds_vector_with_cast() {
    let r = reg_with(json!({ "type": "vector", "dim": 2 })).unwrap();
    let out = insert(Backend::Postgres, &r, json!({ "embedding": [0.1, 0.2] })).unwrap();
    let text = first_stmt(&out)["text"].as_str().unwrap().to_string();
    assert!(text.contains("CAST($1 AS vector)"), "PG 须显式 CAST：{text}");
}

#[test]
fn non_pg_has_no_cast() {
    let r = reg_with(json!({ "type": "vector", "dim": 2 })).unwrap();
    for backend in [Backend::Mysql, Backend::Sqlite] {
        let out = insert(backend, &r, json!({ "embedding": [0.1, 0.2] })).unwrap();
        let text = first_stmt(&out)["text"].as_str().unwrap().to_string();
        assert!(!text.contains("CAST("), "非 PG 不得加 CAST：{text}");
    }
}

// ─── A3：读回标记 ───────────────────────────────────────────

#[test]
fn returning_shape_marks_vector_as_json() {
    let r = reg();
    let out = translate(
        Backend::Sqlite,
        &json!({ "kind": "find", "collection": "docs", "filter": {} }),
        &r,
    )
    .unwrap();
    let cols = out["stmts"][0]["rowShape"]["columns"]
        .as_array()
        .expect("find 须带 rowShape")
        .clone();
    let col = cols
        .iter()
        .find(|c| c["path"] == json!(["embedding"]))
        .expect("rowShape 须含 embedding 列");
    assert_eq!(col["json"], json!(true), "vector 须标记为 JSON 列：{col}");
}

// ─── $inc 拦截 ─────────────────────────────────────────────

#[test]
fn inc_on_vector_is_rejected() {
    let r = reg();
    let err = translate(
        Backend::Sqlite,
        &json!({
            "kind": "updateMany",
            "collection": "docs",
            "filter": {},
            "update": { "$inc": { "embedding": 1 } }
        }),
        &r,
    )
    .unwrap_err();
    assert!(err.contains("embedding"), "错误文案须含字段名：{err}");
    assert!(err.contains("$inc"), "错误文案须说明 $inc：{err}");
    assert!(err.contains("vector"), "错误文案须说明字段类型：{err}");
}
