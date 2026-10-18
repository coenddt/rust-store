//! 向量检索（03 §6）：规划期校验 / `filter` 与 `residual` 拆分 / PG SQL 快照 / local 打分口径。
//!
//! 覆盖验收 A5（规划与执行）、A6（`_score` 口径）、A7（预过滤不占 TopK）。
//! 契约来源：执行文档 `向量检索扩展-03` §4、§6。

use serde_json::{json, Map, Value};

use rust_store_core::command::plan_query;
use rust_store_core::dialect::{translate, Backend};
use rust_store_core::local::{pipeline, vector};
use rust_store_core::schema::Registry;

const GQL: &str = "Artwork($condition:@c0){ _id, title }";
const GQL_ACL: &str = "Artwork($condition:@c0){ _id, title, tenant, ownerId, status }";

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
            "tenant": { "type": "string" },
            "ownerId": { "type": "string" },
            "embedding": { "type": "vector", "dim": 4, "metric": "cosine" }
        },
        "relations": {},
        "indexes": [{
            "keys": { "embedding": "vector" },
            "options": { "indexName": "artworks_vec" }
        }]
    }))
    .expect("schema 注册应成功");
    r
}

fn params(condition: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("c0".into(), condition);
    m
}

fn plan(condition: Value) -> Result<Value, String> {
    let reg = registry();
    let p = plan_query(GQL, &params(condition), &reg, None)?;
    Ok(json!({ "mode": p.mode.as_str(), "commands": p.commands }))
}

/// 规划成功后的 aggregate pipeline 阶段数组
fn pipeline_of(condition: Value) -> Vec<Value> {
    pipeline_of_gql(GQL, condition)
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

fn near(limit: i64) -> Value {
    json!({ "$vector": vec4(1.0, 0.0, 0.0, 0.0), "$limit": limit })
}

// ─── §6 行为断言 1~3：参数校验 ───────────────────────────────

#[test]
fn missing_near_limit_is_rejected() {
    let err = plan(json!({ "embedding": { "$near": { "$vector": vec4(1.0, 0.0, 0.0, 0.0) } } }))
        .unwrap_err();
    assert!(err.contains("必填"), "缺 $near.$limit 须显式报「必填」：{err}");
}

#[test]
fn vector_dimension_mismatch_is_rejected() {
    let err = plan(json!({
        "embedding": { "$near": {
            "$vector": [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8],
            "$limit": 3
        } }
    }))
    .unwrap_err();
    assert!(err.contains("维度"), "维度不符须显式报「维度」：{err}");
}

#[test]
fn zero_query_vector_is_rejected_for_cosine() {
    let err = plan(json!({
        "embedding": { "$near": {
            "$vector": vec4(0.0, 0.0, 0.0, 0.0),
            "$limit": 3
        } }
    }))
    .unwrap_err();
    assert!(err.contains("零向量"), "cosine 零向量须显式报「零向量」：{err}");
}

// ─── §6 行为断言 4~6：互斥与位置 ─────────────────────────────

#[test]
fn root_skip_with_near_is_rejected() {
    let reg = registry();
    let mut p = params(json!({ "embedding": { "$near": near(3) } }));
    p.insert("s0".into(), json!(1));
    let err = plan_query("Artwork($condition:@c0,$skip:@s0){ _id }", &p, &reg, None).unwrap_err();
    assert!(err.contains("$skip"), "根 $skip 与 $near 须互斥：{err}");
}

#[test]
fn root_sort_with_near_is_rejected() {
    let reg = registry();
    let mut p = params(json!({ "embedding": { "$near": near(3) } }));
    p.insert("s0".into(), json!({ "title": 1 }));
    let err = plan_query("Artwork($condition:@c0,$sort:@s0){ _id }", &p, &reg, None).unwrap_err();
    assert!(err.contains("$sort"), "根 $sort 与 $near 须互斥：{err}");
}

#[test]
fn near_inside_or_is_rejected() {
    let err = plan(json!({
        "$or": [
            { "embedding": { "$near": near(3) } },
            { "status": "published" }
        ]
    }))
    .unwrap_err();
    assert!(err.contains("$or"), "$or 内 $near 须显式报错：{err}");
}

// ─── §6 行为断言 7：不可进 filter 的条件下沉 residual ──────

#[test]
fn regex_goes_to_residual_match_after_topk() {
    let stages = pipeline_of(json!({
        "embedding": { "$near": near(3) },
        "title": { "$regex": "^A" }
    }));
    let spec = &stages[0]["$vectorSearch"];
    assert!(
        spec.get("filter").is_none(),
        "$regex 不可进 Atlas filter，须整体下沉 residual：{spec}"
    );
    assert_eq!(spec["path"], json!("embedding"));
    assert_eq!(spec["index"], json!("artworks_vec"));
    assert_eq!(spec["limit"], json!(3));
    assert_eq!(spec["numCandidates"], json!(30));
    assert_eq!(
        stages[1],
        json!({ "$addFields": { "_score": { "$meta": "vectorSearchScore" } } }),
        "`_score` 阶段须紧随 `$vectorSearch`"
    );
    assert_eq!(
        stages[2],
        json!({ "$match": { "title": { "$regex": "^A" } } }),
        "residual 条件须为 TopK 之后的 `$match`"
    );
}

// ─── §6 行为断言 8：预过滤不占 TopK（A7） ────────────────────

#[test]
fn acl_filters_are_prefilter_and_do_not_consume_topk() {
    let stages = pipeline_of_gql(
        GQL_ACL,
        json!({
            "embedding": { "$near": near(5) },
            "tenant": "t1",
            "ownerId": "u1",
            "status": "published"
        }),
    );
    let spec = &stages[0]["$vectorSearch"];
    let flt = spec.get("filter").expect("三条 ACL 行条件须进 $vectorSearch.filter");
    assert_eq!(flt["tenant"], json!("t1"));
    assert_eq!(flt["ownerId"], json!("u1"));
    assert_eq!(flt["status"], json!("published"));
    assert_eq!(spec["limit"], json!(5), "TopK 仍是 5，不因预过滤缩水");

    // 端到端：8 行中 3 行不可读 → 预过滤后只剩 5 行，TopK=5 全部返回且均可读
    let mut rows = Vec::new();
    for (i, ok) in [true, true, true, true, true, false, false, false]
        .into_iter()
        .enumerate()
    {
        rows.push(json!({
            "_id": format!("a{i}"),
            "title": format!("A{i}"),
            "tenant": if ok { "t1" } else { "t2" },
            "ownerId": if ok { "u1" } else { "u2" },
            "status": if ok { "published" } else { "archived" },
            "embedding": vec4(1.0, 0.0, 0.0, 0.0)
        }));
    }
    let out = pipeline::run(&json!({ "artworks": rows }), "artworks", &stages).unwrap();
    assert_eq!(out.len(), 5, "TopK 5 须返回 5 条可读行：{out:?}");
    assert!(out.iter().all(|r| r["tenant"] == json!("t1")
        && r["ownerId"] == json!("u1")
        && r["status"] == json!("published")));
}

// ─── §6 行为断言 9：`_score` 与独立实现误差 ≤1e-6（A6） ──────

/// 独立暴力实现（不调用被测 `score`）
fn brute_cosine(a: &[f64], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    dot / (na * nb)
}

#[test]
fn score_matches_independent_implementation() {
    let rows = vec![
        json!({ "_id": "x", "title": "X", "embedding": vec4(0.6, 0.8, 0.0, 0.0) }),
        json!({ "_id": "y", "title": "Y", "embedding": vec4(0.0, 0.0, 0.3, 0.4) }),
        json!({ "_id": "z", "title": "Z", "embedding": vec4(0.1, 0.2, 0.3, 0.4) }),
    ];
    let stages = pipeline_of(json!({ "embedding": { "$near": near(3) } }));
    let spec = vector::parse_stage(&stages[0]["$vectorSearch"]).expect("阶段应可解析");
    let out = vector::search(rows, &spec).expect("local 检索应成功");
    assert_eq!(out.len(), 3);

    let q = [1.0, 0.0, 0.0, 0.0];
    for row in &out {
        let id = row["_id"].as_str().unwrap();
        let b: Vec<f64> = row["embedding"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        let want = brute_cosine(&q, &b);
        let got = row["_score"].as_f64().unwrap();
        assert!(
            (got - want).abs() <= 1e-6,
            "行 {id} 的 _score={got}，独立实现={want}（误差须 ≤1e-6）"
        );
    }
}

// ─── §6 行为断言 10：local 存量零向量行跳过且不产出 NaN ────

#[test]
fn local_skips_zero_vector_rows_without_nan() {
    let rows = vec![
        json!({ "_id": "a", "title": "A", "embedding": vec4(1.0, 0.0, 0.0, 0.0) }),
        json!({ "_id": "z", "title": "Z", "embedding": vec4(0.0, 0.0, 0.0, 0.0) }),
    ];
    let stages = pipeline_of(json!({ "embedding": { "$near": near(10) } }));
    let spec = vector::parse_stage(&stages[0]["$vectorSearch"]).expect("阶段应可解析");
    let out = vector::search(rows, &spec).expect("local 检索应成功");
    assert_eq!(out.len(), 1, "零向量行须被跳过：{out:?}");
    assert_eq!(out[0]["_id"], json!("a"));
    assert!(
        out.iter()
            .all(|r| r["_score"].as_f64().map(f64::is_finite) == Some(true)),
        "不得返回 NaN/Infinity：{out:?}"
    );
}

// ─── PG SQL 快照（cosine，limit 20，minScore 0.55） ─────────

#[test]
fn pg_sql_snapshot() {
    let reg = registry();
    let p = plan_query(
        GQL,
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
    let cmd = p.commands[0].clone();
    assert_eq!(cmd["kind"], json!("aggregate"));

    let out = translate(Backend::Postgres, &cmd, &reg).expect("PG 翻译应成功");
    let stmt = &out["stmts"][0];
    let text = stmt["text"].as_str().expect("应有 SQL 文本");

    // `_score` 输出列的断言由 04 §6 的同一份快照覆盖（投影放行属 04 §4.1/§4.2）；
    // 本步只验向量阶段驱动的 WHERE / ORDER BY / LIMIT 与参数顺序。
    assert!(text.contains(r#"FROM "artworks" t"#), "根表不符：{text}");
    // filter 落 WHERE（检索前预过滤），minScore 折叠为距离上界
    assert!(
        text.contains(r#"WHERE t."status" = $2 AND t."embedding" <=> $1 <= $3"#),
        "WHERE 组合不符：{text}"
    );
    assert!(
        text.contains(r#"ORDER BY t."embedding" <=> $1 ASC"#),
        "ORDER BY 不符：{text}"
    );
    assert!(text.contains("LIMIT $4"), "LIMIT 参数序号不符：{text}");

    // 参数顺序：queryVector → filter → 距离上界 → limit
    let ps = stmt["params"].as_array().expect("应有参数数组");
    assert_eq!(ps.len(), 4, "参数个数不符：{ps:?}");
    assert_eq!(ps[0], json!([1.0, 0.0, 0.0, 0.0]));
    assert_eq!(ps[1], json!("published"));
    assert!(
        (ps[2].as_f64().unwrap() - 0.45).abs() < 1e-12,
        "距离上界须为 1-minScore=0.45，实际 {}",
        ps[2]
    );
    assert_eq!(ps[3], json!(20));
}
