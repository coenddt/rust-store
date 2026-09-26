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
        Backend::Mysql.json_extract_scalar("`meta`", &["a", "b"]),
        "JSON_UNQUOTE(JSON_EXTRACT(`meta`, '$.a.b'))"
    );
    assert_eq!(
        Backend::Postgres.json_extract_scalar("\"meta\"", &["a", "b"]),
        "(\"meta\" #>> '{a,b}')"
    );
    assert_eq!(
        Backend::Sqlite.json_extract_scalar("\"meta\"", &["a", "b"]),
        "json_extract(\"meta\", '$.a.b')"
    );
}

#[test]
fn json_extract_scalar_single_segment() {
    assert_eq!(
        Backend::Mysql.json_extract_scalar("`meta`", &["title"]),
        "JSON_UNQUOTE(JSON_EXTRACT(`meta`, '$.title'))"
    );
    assert_eq!(
        Backend::Postgres.json_extract_scalar("\"meta\"", &["title"]),
        "(\"meta\" #>> '{title}')"
    );
    assert_eq!(
        Backend::Sqlite.json_extract_scalar("\"meta\"", &["title"]),
        "json_extract(\"meta\", '$.title')"
    );
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
    assert!(text.contains("(t.\"meta\" #>> '{seo,title}')"), "PG U3: {text}");

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
    assert!(text.contains("ORDER BY (t.\"meta\" #>> '{level}') ASC"), "PG U4: {text}");
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
        params.iter().any(|p| p.as_str() == Some("{\"level\":\"a\"}")),
        "object 字段应序列化为 JSON 文本: {params:?}"
    );
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
