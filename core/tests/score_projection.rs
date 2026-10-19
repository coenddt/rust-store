//! `_score` 投影与协议皮（04 §6）：Mongo 投影放行 / 非向量护栏 / PG 快照 / 保留名。
//!
//! 覆盖验收 A6（`_score` 口径与反向断言：未请求时响应不含该键）。
//! 契约来源：执行文档 `向量检索扩展-04` §4、§6。

use serde_json::{json, Map, Value};

use rust_store_core::command::plan_query;
use rust_store_core::dialect::{translate, Backend};
use rust_store_core::local::pipeline;
use rust_store_core::pipeline::{build_pipeline_projection, build_projection, parse_gql};
use rust_store_core::schema::Registry;

const GQL_SCORE: &str = "Artwork($condition:@c0){ _id, title, _score }";
const GQL_NO_SCORE: &str = "Artwork($condition:@c0){ _id, title }";

fn vec4(a: f64, b: f64, c: f64, d: f64) -> Value {
    json!([a, b, c, d])
}

fn registry() -> Registry {
    let mut r = Registry::new();
    r.register(&json!({
        "name": "Artwork",
        "collection": "artworks",
        "timestamps": false,
        "fields": {
            "title": { "type": "string" },
            "status": { "type": "string" },
            "embedding": { "type": "vector", "dim": 4, "metric": "cosine" }
        },
        "relations": {},
        "indexes": [{
            "keys": { "embedding": "vector" },
            "options": { "indexName": "artworks_vec" }
        }]
    }))
    .expect("schema 注册应成功");
    r.register(&json!({
        "name": "Computed",
        "collection": "computeds",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "computes": { "fnField": { "fn": true, "depends": ["title"] } }
    }))
    .expect("计算列 schema 注册应成功");
    r
}

fn params(condition: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("c0".into(), condition);
    m
}

fn near(limit: i64) -> Value {
    json!({ "$vector": vec4(1.0, 0.0, 0.0, 0.0), "$limit": limit })
}

/// 直接调用 `build_projection`（Mongo find / aggregate 共用）
fn projection_of(gql: &str) -> Option<Value> {
    let reg = registry();
    let ast = parse_gql(gql).expect("GQL 应可解析");
    let schema = reg.get(&ast.model).expect("schema 应存在");
    build_projection(&ast, schema, None, &reg)
}

fn pipeline_projection_of(gql: &str) -> Option<Value> {
    let reg = registry();
    let ast = parse_gql(gql).expect("GQL 应可解析");
    let schema = reg.get(&ast.model).expect("schema 应存在");
    build_pipeline_projection(&ast, schema)
}

fn pipeline_of_gql(gql: &str, condition: Value) -> Vec<Value> {
    let reg = registry();
    let p = plan_query(gql, &params(condition), &reg, None).expect("应规划成功");
    assert_eq!(p.mode.as_str(), "aggregate", "向量检索必须走聚合路径");
    p.commands[0]["pipeline"]
        .as_array()
        .cloned()
        .expect("aggregate 命令须带 pipeline")
}

// ─── §6 Mongo 投影断言 1~4 ───────────────────────────────────

#[test]
fn score_is_not_injected_when_not_requested() {
    assert_eq!(
        projection_of("Artwork { title }"),
        Some(json!({ "_id": 1, "title": 1 })),
        "未请求 `_score` 时不得注入（A6 反向断言）"
    );
}

#[test]
fn score_with_real_field_is_kept() {
    assert_eq!(
        projection_of("Artwork { title, _score }"),
        Some(json!({ "_id": 1, "title": 1, "_score": 1 }))
    );
}

#[test]
fn score_only_projection_is_not_none() {
    assert_eq!(
        projection_of("Artwork { _score }"),
        Some(json!({ "_id": 1, "_score": 1 })),
        "只请求 `_score` 时 has_real_field=false 但 wants_score=true，不得返回 None"
    );
}

#[test]
fn only_compute_projection_is_still_none() {
    assert_eq!(
        projection_of("Computed { fnField }"),
        None,
        "既有行为不变：无真实字段不投影"
    );
}

// ─── §4.2 `$pipeline` 顶层投影 ──────────────────────────────

#[test]
fn pipeline_projection_keeps_score() {
    assert_eq!(
        pipeline_projection_of("Artwork { _id, title, _score }"),
        Some(json!({ "_id": 1, "title": 1, "_score": 1 }))
    );
}

#[test]
fn pipeline_projection_without_score_is_unchanged() {
    assert_eq!(
        pipeline_projection_of("Artwork { _id, title }"),
        Some(json!({ "_id": 1, "title": 1 }))
    );
}

// ─── §4.3 非向量查询请求 `_score` 的护栏 ────────────────────

#[test]
fn score_without_near_is_rejected_with_exact_message() {
    let reg = registry();
    let err = plan_query(
        "Artwork($condition:@c0){ _id, _score }",
        &params(json!({ "status": "published" })),
        &reg,
        None,
    )
    .unwrap_err();
    assert_eq!(err, "字段 _score 仅在向量检索（$near）查询中可用");
}

// ─── §4.5 `_score` 保留名（字面比较，不误伤 `score`） ──────

fn register_err(def: Value) -> String {
    let mut r = Registry::new();
    r.register(&def).unwrap_err()
}

#[test]
fn score_in_fields_is_rejected() {
    let err = register_err(json!({
        "name": "Bad", "collection": "bad", "timestamps": false,
        "fields": { "_score": { "type": "number" } },
        "relations": {}
    }));
    assert!(
        err.starts_with("ERR_NAME_CONFLICT:"),
        "须以 ERR_NAME_CONFLICT: 开头：{err}"
    );
    assert!(err.contains("fields"), "须指明命名空间 fields：{err}");
}

#[test]
fn score_in_relations_is_rejected() {
    let err = register_err(json!({
        "name": "Bad", "collection": "bad", "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {
            "_score": { "model": "Other", "type": "many", "localField": "_id", "foreignField": "badId" }
        }
    }));
    assert!(
        err.starts_with("ERR_NAME_CONFLICT:"),
        "须以 ERR_NAME_CONFLICT: 开头：{err}"
    );
    assert!(err.contains("relations"), "须指明命名空间 relations：{err}");
}

#[test]
fn score_in_computes_is_rejected() {
    let err = register_err(json!({
        "name": "Bad", "collection": "bad", "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
        "computes": { "_score": { "fn": true, "depends": ["title"] } }
    }));
    assert!(
        err.starts_with("ERR_NAME_CONFLICT:"),
        "须以 ERR_NAME_CONFLICT: 开头：{err}"
    );
    assert!(err.contains("computes"), "须指明命名空间 computes：{err}");
}

#[test]
fn plain_score_field_is_still_allowed() {
    let mut r = Registry::new();
    r.register(&json!({
        "name": "Ok", "collection": "ok", "timestamps": false,
        "fields": { "score": { "type": "number" } },
        "relations": {}
    }))
    .expect("合法字段名 score 不得被误拒");
}

// ─── §6 SQL 快照（PG，cosine，limit 20，minScore 0.55） ─────

#[test]
fn pg_sql_snapshot_includes_score_column() {
    let reg = registry();
    let p = plan_query(
        GQL_SCORE,
        &params(json!({
            "embedding": { "$near": {
                "$vector": vec4(1.0, 0.0, 0.0, 0.0),
                "$limit": 20,
                "$minScore": 0.55
            } },
            "status": "published"
        })),
        &reg,
        None,
    )
    .expect("应规划成功");
    let out = translate(Backend::Postgres, &p.commands[0], &reg).expect("PG 翻译应成功");
    let stmt = &out["stmts"][0];
    let text = stmt["text"].as_str().expect("应有 SQL 文本");

    assert!(
        text.contains(r#"(1.0 - (t."embedding" <=> CAST($1 AS vector))) AS "_score""#),
        "`_score` 输出列不符：{text}"
    );
    // `_score` 是向量派生列：`field_column_ref` 宽松兜底会把它当裸列下发
    //（`physical_of` 归一成 `score` → `t."score"` 不存在，PG 42703）。
    // 回归锚点：投影里只允许出现补列表达式，绝不允许 `t."score"` 裸列引用。
    assert!(
        !text.contains(r#"t."score""#),
        "投影不得把派生列 _score 当裸列 t.\"score\" 下发：{text}"
    );
    assert!(text.contains(r#"FROM "artworks" t"#), "根表不符：{text}");
    // 查询向量参数与写侧 V-19 同一约定：占位符显式 CAST、参数为 pgvector 文本
    assert!(
        text.contains(
            r#"WHERE t."status" = $2 AND t."embedding" <=> CAST($1 AS vector) <= $3"#
        ),
        "WHERE 组合不符：{text}"
    );
    assert!(
        text.contains(r#"ORDER BY t."embedding" <=> CAST($1 AS vector) ASC"#),
        "ORDER BY 不符：{text}"
    );
    assert!(text.contains("LIMIT $4"), "LIMIT 参数序号不符：{text}");

    let ps = stmt["params"].as_array().expect("应有参数数组");
    assert_eq!(ps.len(), 4, "参数个数不符：{ps:?}");
    assert_eq!(ps[0], json!("[1.0,0.0,0.0,0.0]"));
    assert_eq!(ps[1], json!("published"));
    assert!(
        (ps[2].as_f64().unwrap() - 0.45).abs() < 1e-12,
        "距离上界须为 1-minScore=0.45，实际 {}",
        ps[2]
    );
    assert_eq!(ps[3], json!(20));
}

#[test]
fn pg_sql_snapshot_omits_score_column_when_not_requested() {
    let reg = registry();
    let p = plan_query(
        GQL_NO_SCORE,
        &params(json!({
            "embedding": { "$near": {
                "$vector": vec4(1.0, 0.0, 0.0, 0.0),
                "$limit": 20,
                "$minScore": 0.55
            } },
            "status": "published"
        })),
        &reg,
        None,
    )
    .expect("应规划成功");
    let out = translate(Backend::Postgres, &p.commands[0], &reg).expect("PG 翻译应成功");
    let text = out["stmts"][0]["text"].as_str().expect("应有 SQL 文本");
    assert!(
        !text.contains(r#"AS "_score""#),
        "未请求 `_score` 时 SELECT 列中不得出现该列（A6 反向断言）：{text}"
    );
}

// ─── §6 端到端（local）：请求则保留、未请求则无该键 ─────────

fn local_rows() -> Vec<Value> {
    vec![
        json!({ "_id": "a", "title": "A", "status": "published", "embedding": vec4(1.0, 0.0, 0.0, 0.0) }),
        json!({ "_id": "b", "title": "B", "status": "published", "embedding": vec4(0.0, 1.0, 0.0, 0.0) }),
    ]
}

#[test]
fn local_aggregate_keeps_score_when_requested() {
    let stages = pipeline_of_gql(GQL_SCORE, json!({ "embedding": { "$near": near(2) } }));
    let out = pipeline::run(&json!({ "artworks": local_rows() }), "artworks", &stages)
        .expect("local 执行应成功");
    assert_eq!(out.len(), 2);
    assert!(
        out.iter().all(|r| r["_score"].as_f64().is_some()),
        "请求了 `_score` 就必须保留：{out:?}"
    );
}

#[test]
fn local_aggregate_omits_score_when_not_requested() {
    let stages = pipeline_of_gql(GQL_NO_SCORE, json!({ "embedding": { "$near": near(2) } }));
    let out = pipeline::run(&json!({ "artworks": local_rows() }), "artworks", &stages)
        .expect("local 执行应成功");
    assert_eq!(out.len(), 2);
    assert!(
        out.iter().all(|r| r.get("_score").is_none()),
        "未请求 `_score` 时响应不得含该键（A6 反向断言）：{out:?}"
    );
}
