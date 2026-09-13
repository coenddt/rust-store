//! 三仓共享核心缺陷回归测试
//!
//! - D-01（C）：GQL 关系带参 + 子选择集同时出现时解析失败
//! - D-02（C）：`$where` 等服务端执行类载荷导致条件被静默丢弃
//! - D-03（M）：跨绑定序列化边界 —— 无 idPrefix 且无 `_id` 时静默产出无主文档

use serde_json::{json, Value};

use rust_store_core::command::{plan_exists, plan_insert};
use rust_store_core::dialect::filter::build_filter;
use rust_store_core::dialect::select::translate_select;
use rust_store_core::dialect::Backend;
use rust_store_core::pipeline::parse_gql;
use rust_store_core::schema::Registry;
use rust_store_core::types::validate_condition;

fn registry_of(defs: &[Value]) -> Registry {
    let mut reg = Registry::new();
    for d in defs {
        reg.register(d).expect("schema 注册应成功");
    }
    reg
}

// ─── D-01：GQL 关系带参 + 子选择集 ───────────────────────────

#[test]
fn d01_relation_with_params_and_sub_selection_parses() {
    // 缺陷场景：`Rel($limit:@l){f}` 此前 `{` 被误判为下一个字段导致解析失败
    let ast = parse_gql("Order($limit:@l){code, items($filter:@f){qty}}")
        .expect("关系带参+子选择集应可解析");
    assert_eq!(ast.model, "Order");
    assert_eq!(ast.params.get("limit").map(String::as_str), Some("@l"));
    assert_eq!(ast.fields, vec!["code".to_string()]);

    let (name, rel) = ast
        .relations
        .iter()
        .find(|(n, _)| n == "items")
        .expect("应有 items 关系");
    assert_eq!(name, "items");
    assert_eq!(rel.fields, vec!["qty".to_string()]);
    assert_eq!(rel.params.get("filter").map(String::as_str), Some("@f"));
}

#[test]
fn d01_relation_with_params_but_no_body_still_parses() {
    // 关系带参但无子选择集：既有形态不回归
    let ast = parse_gql("Post{rel($limit:@l)}").expect("关系仅带参应可解析");
    let (_, rel) = ast.relations.iter().find(|(n, _)| n == "rel").unwrap();
    assert!(rel.fields.is_empty());
    assert_eq!(rel.params.get("limit").map(String::as_str), Some("@l"));
}

// ─── D-02：$where 等载荷显式拒绝，绝不静默丢条件 ─────────────

#[test]
fn d02_validate_condition_rejects_server_exec_ops() {
    for op in ["$where", "$function", "$accumulator"] {
        let cond = json!({ op: "return true" });
        assert!(validate_condition(&cond).is_err(), "{op} 应被拒绝名单拦截");
        // 嵌套形态同样拒绝
        let nested = json!({ "status": "x", "$or": [ { op: "1" } ] });
        assert!(validate_condition(&nested).is_err(), "嵌套 {op} 应被拦截");
    }
}

#[test]
fn d02_build_filter_errors_on_untranslatable_ops() {
    let col = |f: &str| {
        if f == "title" {
            Some(f.to_string())
        } else {
            None
        }
    };
    for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
        for cond in [
            json!({ "$where": "1" }),
            json!({ "$text": { "$search": "x" } }),
        ] {
            let mut seq = 0usize;
            let err = build_filter(&cond, backend, "t", &col, &mut seq, None)
                .expect_err("SQL 侧不可翻译的条件必须显式报错");
            assert!(!err.is_empty());
        }
        // 正常条件不受影响
        let mut seq = 0usize;
        let wh = build_filter(&json!({ "title": "x" }), backend, "t", &col, &mut seq, None)
            .expect("正常条件应可翻译");
        assert!(!wh.text.is_empty());
    }
}

#[test]
fn d02_plan_paths_reject_where_payload() {
    let reg = registry_of(&[json!({
        "name": "Post", "collection": "posts", "timestamps": false,
        "fields": { "title": { "type": "string" }, "status": { "type": "string" } },
        "relations": {},
    })]);

    // exists 规划路径
    let err = plan_exists("Post", &reg, &json!({ "$where": "return true" }))
        .expect_err("$where 载荷应在 exists 规划层被拒绝");
    assert!(err.contains("$where"), "错误信息应指明操作符: {err}");

    // translate 路径（countDocuments → SQL 翻译兜底）
    let cmd = json!({
        "kind": "countDocuments", "collection": "posts",
        "filter": { "$where": "return true" },
    });
    let mut warnings = Vec::new();
    let mut unsupported = Vec::new();
    let err = translate_select(Backend::Mysql, &cmd, &reg, &mut warnings, &mut unsupported)
        .expect_err("countDocuments 的 $where 载荷应显式报错而非静默丢条件");
    assert!(!err.is_empty());
}

// ─── D-03：无 idPrefix 且无 _id → 显式报错 ───────────────────

fn plain_schema(id_prefix: Option<&str>) -> Value {
    let mut def = json!({
        "name": "Post", "collection": "posts", "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
    });
    if let Some(p) = id_prefix {
        def.as_object_mut()
            .unwrap()
            .insert("idPrefix".into(), json!(p));
    }
    def
}

#[test]
fn d03_insert_without_id_prefix_and_without_id_errors() {
    let reg = registry_of(&[plain_schema(None)]);
    let err = plan_insert(
        "Post",
        &reg,
        None,
        &json!({ "title": "x" }),
        0,
        "gen-0",
        None,
    )
    .expect_err("无 idPrefix 且无 _id 必须显式报错，不得静默产出无主文档");
    assert!(err.contains("idPrefix"), "错误应指明 idPrefix 要求: {err}");
}

#[test]
fn d03_insert_with_explicit_id_is_preserved() {
    let reg = registry_of(&[plain_schema(None)]);
    let out = plan_insert(
        "Post",
        &reg,
        None,
        &json!({ "_id": "custom-1", "title": "x" }),
        0,
        "gen-0",
        None,
    )
    .expect("显式 _id 应放行");
    let doc = &out["returns"];
    assert_eq!(doc["_id"], json!("custom-1"), "显式 _id 不得被覆盖");
}

#[test]
fn d03_insert_with_id_prefix_auto_generates_id() {
    let reg = registry_of(&[plain_schema(Some("post"))]);
    let out = plan_insert(
        "Post",
        &reg,
        None,
        &json!({ "title": "x" }),
        0,
        "post-9",
        None,
    )
    .expect("配置 idPrefix 后应自动生成 _id");
    assert_eq!(out["returns"]["_id"], json!("post-9"));
}

// ─── M-8-3：$regex/$options 合并处理，绝不静默丢弃 ─────────────

#[test]
fn d09_regex_options_case_insensitive_by_backend() {
    let col = |f: &str| {
        if f == "title" {
            Some(f.to_string())
        } else {
            None
        }
    };
    let cond = json!({ "title": { "$regex": "abc", "$options": "i" } });

    // PostgreSQL：'i' → ~*（大小写不敏感），可表达、无告警
    let mut seq = 0usize;
    let mut w = Vec::new();
    let wh = build_filter(&cond, Backend::Postgres, "t", &col, &mut seq, Some(&mut w))
        .expect("PG 应可翻译");
    assert_eq!(wh.text, "t.\"title\" ~* $1", "PG 应为 ~*: {}", wh.text);
    assert!(w.is_empty(), "PG 'i' 可表达，不应有告警: {w:?}");

    // MySQL：'i' → REGEXP_LIKE(col, ?, 'i')（MySQL 8 REGEXP 默认区分大小写，不能靠 collation）
    let mut seq = 0usize;
    let mut w = Vec::new();
    let wh = build_filter(&cond, Backend::Mysql, "t", &col, &mut seq, Some(&mut w))
        .expect("MySQL 应可翻译");
    assert_eq!(
        wh.text, "REGEXP_LIKE(t.`title`, ?, 'i')",
        "MySQL 应为 REGEXP_LIKE: {}",
        wh.text
    );
    assert!(w.is_empty(), "MySQL 'i' 可表达，不应有告警: {w:?}");

    // SQLite：无 flags 支持 → 条件保留 + 告警（语义降级必须明示，绝不静默）
    let mut seq = 0usize;
    let mut w = Vec::new();
    let wh = build_filter(&cond, Backend::Sqlite, "t", &col, &mut seq, Some(&mut w))
        .expect("SQLite 应保留 REGEXP 条件");
    assert_eq!(
        wh.text, "t.\"title\" REGEXP ?",
        "SQLite 条件不得丢弃: {}",
        wh.text
    );
    assert_eq!(w.len(), 1, "SQLite + 'i' 应恰一条告警: {w:?}");
    assert!(w[0].contains("i"), "告警应指明无法表达的 flags: {w:?}");

    // 无 $options：既有语义不回归（PG ~ / MySQL REGEXP，且无告警）
    let plain = json!({ "title": { "$regex": "abc" } });
    let mut seq = 0usize;
    let mut w = Vec::new();
    let wh =
        build_filter(&plain, Backend::Postgres, "t", &col, &mut seq, Some(&mut w)).expect("pg");
    assert_eq!(
        wh.text, "t.\"title\" ~ $1",
        "PG 无 flags 应保持 ~: {}",
        wh.text
    );
    let mut seq = 0usize;
    let wh =
        build_filter(&plain, Backend::Mysql, "t", &col, &mut seq, Some(&mut w)).expect("mysql");
    assert_eq!(
        wh.text, "t.`title` REGEXP ?",
        "MySQL 无 flags 应保持 REGEXP: {}",
        wh.text
    );
    assert!(w.is_empty(), "无 $options 不应有告警: {w:?}");
}

#[test]
fn d09_options_without_regex_is_not_silently_dropped() {
    let col = |f: &str| {
        if f == "title" {
            Some(f.to_string())
        } else {
            None
        }
    };
    let cond = json!({ "title": { "$options": "i" } });

    // 有告警通道：告警 + 不生成条件（无 $regex 可承载该修饰符）
    let mut seq = 0usize;
    let mut w = Vec::new();
    let wh = build_filter(&cond, Backend::Postgres, "t", &col, &mut seq, Some(&mut w))
        .expect("有通道时应告警而非报错");
    assert!(
        wh.text.is_empty(),
        "无 $regex 承载，不应生成条件: {}",
        wh.text
    );
    assert_eq!(w.len(), 1, "应恰一条告警: {w:?}");
    assert!(w[0].contains("$options"), "告警应指明 $options: {w:?}");

    // 无告警通道（写路径语义）：显式报错，绝不静默忽略
    let mut seq = 0usize;
    let err = build_filter(&cond, Backend::Postgres, "t", &col, &mut seq, None)
        .expect_err("无告警通道时必须显式报错");
    assert!(err.contains("$options"), "错误应指明 $options: {err}");

    // $not 内嵌同样生效：孤立 $options 走告警通道而非静默
    let nested = json!({ "title": { "$not": { "$options": "i" } } });
    let mut seq = 0usize;
    let mut w = Vec::new();
    let wh = build_filter(
        &nested,
        Backend::Postgres,
        "t",
        &col,
        &mut seq,
        Some(&mut w),
    )
    .expect("嵌套 $not 内的孤立 $options 应告警");
    assert!(
        wh.text.is_empty(),
        "$not 内无 $regex 承载，不应生成条件: {}",
        wh.text
    );
    assert_eq!(w.len(), 1, "$not 内孤立 $options 应告警: {w:?}");
}
