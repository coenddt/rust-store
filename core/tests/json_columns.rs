//! JSON 列基础件：`object`/`array` 字段在 SQL 后端**落单列存 JSON 文本**
//! （MySQL `JSON` / PG `jsonb` / SQLite `TEXT`+JSON1）的方言表达式生成。
//!
//! 这是 U3（对象点号路径过滤）/ U4（对象点号路径排序）在 `standard` 档下推的地基；
//! 依据执行文档 §5 步骤 8 与不足清单 #3。契约：core 不写 DDL（铁律 6），
//! 列类型由示例 DDL 与 introspection 对齐，此处只产出表达式。

use rust_store_core::dialect::Backend;

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
