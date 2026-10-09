//! JSON 列基础件：`object`/`array` 字段在 SQL 后端**落单列存 JSON 文本**
//! （MySQL `JSON` / PG `jsonb` / SQLite `TEXT`+JSON1）的方言表达式生成。
//!
//! 这是 U3（对象点号路径过滤）/ U4（对象点号路径排序）在 `standard` 档下推的地基；
//! 依据执行文档 §5 步骤 8 与不足清单 #3。契约：core 不写 DDL（铁律 6），
//! 列类型由示例 DDL 与 introspection 对齐，此处只产出表达式。

use serde_json::json;

use rust_store_core::dialect::{restore_rows_json, translate, Backend};
use rust_store_core::schema::Registry;

fn reg() -> Registry {
    let mut r = Registry::new();
    r.register(&json!({
        "name": "Course",
        "collection": "courses",
        "timestamps": false,
        "fields": {
            "title": { "type": "string" },
            "tags": { "type": "array" },
            "meta": { "type": "object", "fields": {
                "level": { "type": "string" },
                "seo": { "type": "object", "fields": { "title": { "type": "string" } } }
            } }
        },
        "relations": {}
    }))
    .unwrap();
    r
}

#[test]
fn json_type_name_per_backend() {
    assert_eq!(Backend::Mysql.json_type_name(), "JSON");
    assert_eq!(Backend::Postgres.json_type_name(), "jsonb");
    assert_eq!(Backend::Sqlite.json_type_name(), "TEXT");
}

#[test]
fn json_extract_scalar_multi_segment() {
    assert_eq!(
        Backend::Mysql
            .json_extract_scalar("`meta`", &["a", "b"])
            .unwrap(),
        "JSON_UNQUOTE(JSON_EXTRACT(`meta`, '$.a.b'))"
    );
    assert_eq!(
        Backend::Postgres
            .json_extract_scalar("\"meta\"", &["a", "b"])
            .unwrap(),
        "(\"meta\" #>> '{a,b}')"
    );
    assert_eq!(
        Backend::Sqlite
            .json_extract_scalar("\"meta\"", &["a", "b"])
            .unwrap(),
        "json_extract(\"meta\", '$.a.b')"
    );
}

#[test]
fn json_extract_scalar_single_segment() {
    assert_eq!(
        Backend::Mysql.json_extract_scalar("`meta`", &["title"]).unwrap(),
        "JSON_UNQUOTE(JSON_EXTRACT(`meta`, '$.title'))"
    );
    assert_eq!(
        Backend::Postgres
            .json_extract_scalar("\"meta\"", &["title"])
            .unwrap(),
        "(\"meta\" #>> '{title}')"
    );
    assert_eq!(
        Backend::Sqlite
            .json_extract_scalar("\"meta\"", &["title"])
            .unwrap(),
        "json_extract(\"meta\", '$.title')"
    );
}

/// N1 注入防护：段含 `'` / `\` / 空格 / `$` 等非法字符 → 三后端一律显式 `Err`（禁静默改写）
#[test]
fn json_extract_scalar_rejects_illegal_segment() {
    for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
        for bad in ["a'b", "a\\b", "a b", "$x", "a{b", "a,b", ""] {
            let err = backend
                .json_extract_scalar("`meta`", &[bad])
                .expect_err("非法路径段应显式 Err（禁静默改写）");
            assert!(
                err.starts_with("ERR_GQL_PARSE:"),
                "错误前缀应为 ERR_GQL_PARSE:（实际 {err}）"
            );
        }
    }
}

/// U3：对象点号路径过滤 → 各方言 JSON 标量提取下推（standard 档放开）
#[test]
fn u3_object_dotted_filter_pushes_json_extract() {
    let cmd = json!({
        "kind": "find", "collection": "courses",
        "filter": { "meta.seo.title": "x" },
        "projection": { "_id": 1, "title": 1 }
    });
    let sqlite = translate(Backend::Sqlite, &cmd, &reg()).unwrap();
    let text = sqlite["stmts"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("json_extract(t.\"meta\", '$.seo.title')"),
        "SQLite U3: {text}"
    );
    assert!(sqlite["unsupported"].as_array().unwrap().is_empty());

    let pg = translate(Backend::Postgres, &cmd, &reg()).unwrap();
    let text = pg["stmts"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("(t.\"meta\" #>> '{seo,title}')"),
        "PG U3: {text}"
    );

    let my = translate(Backend::Mysql, &cmd, &reg()).unwrap();
    let text = my["stmts"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("JSON_UNQUOTE(JSON_EXTRACT(t.`meta`, '$.seo.title'))"),
        "MySQL U3: {text}"
    );
}

/// U4：对象点号路径排序 → 各方言 JSON 标量提取进 ORDER BY（standard 档放开）
#[test]
fn u4_object_dotted_sort_pushes_json_extract() {
    let cmd = json!({
        "kind": "aggregate", "collection": "courses",
        "pipeline": [ { "$sort": { "meta.level": 1 } }, { "$project": { "_id": 1 } } ]
    });
    let sqlite = translate(Backend::Sqlite, &cmd, &reg()).unwrap();
    let text = sqlite["stmts"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("ORDER BY json_extract(t.\"meta\", '$.level') ASC"),
        "SQLite U4: {text}"
    );
    assert!(sqlite["unsupported"].as_array().unwrap().is_empty());

    let pg = translate(Backend::Postgres, &cmd, &reg()).unwrap();
    let text = pg["stmts"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("ORDER BY (t.\"meta\" #>> '{level}') ASC"),
        "PG U4: {text}"
    );
}

/// 写入：object 字段序列化为 JSON 文本参数（跨后端落 JSON 列）
#[test]
fn insert_object_field_serialized_as_json_text() {
    let cmd = json!({
        "kind": "insertOne", "collection": "courses",
        "doc": { "_id": "c1", "title": "t", "meta": { "level": "a" } }
    });
    let out = translate(Backend::Sqlite, &cmd, &reg()).unwrap();
    let params = out["stmts"][0]["params"].as_array().unwrap();
    assert!(
        params
            .iter()
            .any(|p| p.as_str() == Some("{\"level\":\"a\"}")),
        "object 字段应序列化为 JSON 文本: {params:?}"
    );
}

/// U1：数组字段过滤 → 各方言「数组包含元素」谓词下推（standard 档放开）
#[test]
fn u1_array_contains_pushes_native_predicate() {
    let cmd = json!({
        "kind": "find", "collection": "courses",
        "filter": { "tags": "python" },
        "projection": { "_id": 1 }
    });
    // MySQL：JSON_CONTAINS + 值的 JSON 字面量参数
    let my = translate(Backend::Mysql, &cmd, &reg()).unwrap();
    let text = my["stmts"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("JSON_CONTAINS(t.`tags`, CAST(? AS JSON))"),
        "MySQL U1: {text}"
    );
    let params = my["stmts"][0]["params"].as_array().unwrap();
    assert!(
        params.iter().any(|p| p.as_str() == Some("\"python\"")),
        "MySQL U1 参数应为值的 JSON 字面量: {params:?}"
    );

    // PG：@> + 单元素数组参数
    let pg = translate(Backend::Postgres, &cmd, &reg()).unwrap();
    let text = pg["stmts"][0]["text"].as_str().unwrap();
    assert!(text.contains("t.\"tags\" @> $1::jsonb"), "PG U1: {text}");
    let params = pg["stmts"][0]["params"].as_array().unwrap();
    assert!(
        params.iter().any(|p| p.as_str() == Some("[\"python\"]")),
        "PG U1 参数应为单元素数组: {params:?}"
    );

    // SQLite：json_each 子查询 + 标量原值参数
    let sq = translate(Backend::Sqlite, &cmd, &reg()).unwrap();
    let text = sq["stmts"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("EXISTS (SELECT 1 FROM json_each(t.\"tags\") WHERE json_each.value = ?)"),
        "SQLite U1: {text}"
    );
    let params = sq["stmts"][0]["params"].as_array().unwrap();
    assert!(
        params.iter().any(|p| p.as_str() == Some("python")),
        "SQLite U1 参数应为标量原值: {params:?}"
    );
}

/// U1：数组整体等值（值本身是数组）→ 各方言 JSON 整值等值下推
#[test]
fn u1_array_whole_equality_pushes_json_value_eq() {
    let cmd = json!({
        "kind": "find", "collection": "courses",
        "filter": { "tags": ["a", "b"] },
        "projection": { "_id": 1 }
    });
    let my = translate(Backend::Mysql, &cmd, &reg()).unwrap();
    let text = my["stmts"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("t.`tags` = CAST(? AS JSON)"),
        "MySQL U1 整体等值: {text}"
    );

    let pg = translate(Backend::Postgres, &cmd, &reg()).unwrap();
    let text = pg["stmts"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("t.\"tags\" = $1::jsonb"),
        "PG U1 整体等值: {text}"
    );

    let sq = translate(Backend::Sqlite, &cmd, &reg()).unwrap();
    let text = sq["stmts"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("json(t.\"tags\") = json(?)"),
        "SQLite U1 整体等值: {text}"
    );
}

/// U2：对象字段整值等值 → 各方言 JSON 整值等值下推，且**必须告警**（跨后端键序差异，禁静默）
#[test]
fn u2_object_equality_pushes_json_value_eq_and_warns() {
    let cmd = json!({
        "kind": "find", "collection": "courses",
        "filter": { "meta": { "level": "a" } },
        "projection": { "_id": 1 }
    });
    let sq = translate(Backend::Sqlite, &cmd, &reg()).unwrap();
    let text = sq["stmts"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("json(t.\"meta\") = json(?)"),
        "SQLite U2: {text}"
    );
    let params = sq["stmts"][0]["params"].as_array().unwrap();
    assert!(
        params
            .iter()
            .any(|p| p.as_str() == Some("{\"level\":\"a\"}")),
        "U2 参数应为对象 JSON 文本: {params:?}"
    );
    let warnings = sq["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("键序")),
        "U2 必须产出跨后端键序差异告警（禁静默）: {warnings:?}"
    );

    // PG / MySQL 同样下推整值等值
    let pg = translate(Backend::Postgres, &cmd, &reg()).unwrap();
    let text = pg["stmts"][0]["text"].as_str().unwrap();
    assert!(text.contains("t.\"meta\" = $1::jsonb"), "PG U2: {text}");
}

/// U1：数组字段 `$exists` 走 `__present` 哨兵（与标量字段同源）
#[test]
fn u1_array_exists_uses_present_sentinel() {
    let cmd = json!({
        "kind": "find", "collection": "courses",
        "filter": { "tags": { "$exists": false } },
        "projection": { "_id": 1 }
    });
    let sq = translate(Backend::Sqlite, &cmd, &reg()).unwrap();
    let text = sq["stmts"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("NOT (COALESCE(t.__present, ',') LIKE '%,tags,%')"),
        "U1 $exists 应走 __present 哨兵: {text}"
    );
}

/// 无法语义等价翻译的取值组合 → 显式 Err（禁静默丢弃条件）
#[test]
fn json_field_unsupported_operator_is_error() {
    let cmd = json!({
        "kind": "find", "collection": "courses",
        "filter": { "tags": { "$gt": "a" } },
        "projection": { "_id": 1 }
    });
    let err = translate(Backend::Sqlite, &cmd, &reg()).expect_err("U1 $gt 应显式报错");
    assert!(err.contains("$gt"), "错误信息应含操作符: {err}");
}

/// 读取：JSON 列（文本）还原为嵌套对象（跨后端对齐 Mongo）
#[test]
fn json_column_restores_nested_object() {
    let cmd = json!({
        "kind": "find", "collection": "courses", "filter": {},
        "projection": { "_id": 1, "title": 1, "meta": 1 }
    });
    let out = translate(Backend::Sqlite, &cmd, &reg()).unwrap();
    let shape = &out["stmts"][0]["rowShape"];
    let rows = json!([
        { "_id": "c1", "title": "t",
          "meta": "{\"level\":\"a\",\"seo\":{\"title\":\"z\"}}",
          "__present": ",title,meta," }
    ]);
    let restored = restore_rows_json(shape, &rows).unwrap();
    assert_eq!(
        restored,
        json!([{ "_id": "c1", "title": "t",
                 "meta": { "level": "a", "seo": { "title": "z" } } }])
    );
}
