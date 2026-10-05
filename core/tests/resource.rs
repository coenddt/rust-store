use rust_store_core::resource::{compose_url, content_path, is_external_url, ref_kind};
use serde_json::json;

#[test]
fn content_path_shards_by_prefix() {
    assert_eq!(content_path("ab12cd").unwrap(), "objects/ab/ab12cd");
    assert!(content_path("a").is_err());
}

#[test]
fn external_detection() {
    assert!(is_external_url("https://x/y.png"));
    assert!(is_external_url("DATA:image/png;base64,AAA"));
    assert!(!is_external_url("ab12cd"));
    assert_eq!(ref_kind("ab12cd"), "internal");
}

#[test]
fn compose_internal_and_external() {
    let cfg = json!({ "baseUrl": "https://cdn.example.com/", "pathTemplate": "/{contentPath}" });
    assert_eq!(
        compose_url("ab12cd", &cfg).unwrap(),
        "https://cdn.example.com/objects/ab/ab12cd"
    );
    assert_eq!(compose_url("https://o/x", &cfg).unwrap(), "https://o/x");
}

#[test]
fn compose_with_query_and_vars() {
    let cfg = json!({
        "baseUrl": "https://cdn",
        "pathTemplate": "/{key}",
        "query": { "x-oss-process": "image/resize,w_{w}" },
        "vars": { "w": 200 }
    });
    assert_eq!(
        compose_url("ab12cd", &cfg).unwrap(),
        "https://cdn/ab12cd?x-oss-process=image%2Fresize%2Cw_200"
    );
}
