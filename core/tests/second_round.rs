//! 第二轮自检：read 判据单点 / 归属键交叉矩阵 / 默认值收敛 / 列序与 `__present`。
//!
//! 本文件只放「反例与组合边界」，对应的既有正例见 defaults_semantics.rs /
//! write_ownership.rs / column_order.rs / access_keys.rs。

use serde_json::{json, Value};

use rust_store_core::command::{plan_count, plan_insert, plan_query, plan_upsert};
use rust_store_core::dialect::{restore_rows_json, translate, Backend};
use rust_store_core::permission::{Context, UnconfiguredPolicy};
use rust_store_core::schema::Registry;

fn registry_with(defn: Value) -> Registry {
    let mut reg = Registry::new();
    reg.register(&defn).expect("schema 注册应成功");
    reg
}

fn guarded_schema(read: Value) -> Value {
    json!({
        "name": "Doc",
        "collection": "docs",
        "idPrefix": "d",
        "timestamps": false,
        "read": read,
        "write": ["admin"],
        "fields": {
            "title": { "type": "string" },
            "secret": { "type": "string" },
            "createdBy": { "type": "string" }
        },
        "relations": {}
    })
}

fn user_ctx() -> Context {
    let mut ctx = Context {
        user_id: Some("u1".to_string()),
        roles: Some(vec!["user".to_string()]),
        ..Default::default()
    };
    ctx.role = Some("user".to_string());
    ctx
}

fn admin_ctx() -> Context {
    let mut ctx = Context {
        user_id: Some("a1".to_string()),
        roles: Some(vec!["admin".to_string()]),
        ..Default::default()
    };
    ctx.role = Some("admin".to_string());
    ctx
}

// ─── B1：count 必须与 query 共用同一套表级读判据 ───────────────

#[test]
fn b1_count_denies_unlisted_role_like_query() {
    let reg = registry_with(guarded_schema(json!(["admin"])));
    let ctx = user_ctx();
    // 同一模型同一上下文：query 已拒绝（既有行为）
    let q = plan_query("Doc{ title }", &Default::default(), &reg, Some(&ctx));
    assert!(q.is_err(), "query 应拒绝未命中 read 白名单的角色: {q:?}");
    // count 必须同样拒绝，否则 read 判据不单点（可数出全表行数）
    let c = plan_count("Doc", &reg, None, Some(&ctx));
    assert!(
        c.is_err(),
        "count 必须与 query 同判据拒绝未授权读，实际: {c:?}"
    );
}

#[test]
fn b1_count_denies_explicit_empty_read_list() {
    let reg = registry_with(guarded_schema(json!([])));
    let ctx = admin_ctx();
    let c = plan_count("Doc", &reg, None, Some(&ctx));
    assert!(
        c.is_err(),
        "read: [] 表示显式不允许任何角色，count 不得放行: {c:?}"
    );
}

#[test]
fn b1_count_still_allows_admin_and_internal() {
    let reg = registry_with(guarded_schema(json!(["admin"])));
    let admin = plan_count("Doc", &reg, None, Some(&admin_ctx()));
    assert!(admin.is_ok(), "admin 应可 count: {admin:?}");
    let sys = plan_count("Doc", &reg, None, Some(&Context::system()));
    assert!(sys.is_ok(), "internal 应可 count: {sys:?}");
    let anon = plan_count("Doc", &reg, None, None);
    assert!(anon.is_ok(), "无 ctx 为内部调用，保持 fail-open: {anon:?}");
}

// ─── B2：显式空名单（read/write 对称）不能被 count 绕过 ───────

#[test]
fn b2_explicit_empty_read_is_not_bypassed_by_unconfigured_policy() {
    let mut reg = registry_with(guarded_schema(json!([])));
    reg.set_unconfigured_policy(UnconfiguredPolicy::Open);
    let err = plan_count("Doc", &reg, None, Some(&user_ctx()))
        .expect_err("read: [] 是显式拒绝，与 unconfigured 姿态无关");
    assert!(err.contains("ERR_PERMISSION"), "{err}");
}

// ─── B3：RBAC 表级读同样要覆盖 count ─────────────────────────

#[test]
fn b3_count_obeys_rbac_enforce_default_deny() {
    let mut reg = registry_with(json!({
        "name": "Post",
        "collection": "posts",
        "idPrefix": "p",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {}
    }));
    reg.set_rbac(Some(&json!({
        "mode": "enforce",
        "roles": { "user": {} },
        "grants": []
    })))
    .expect("策略应合法");
    let c = plan_count("Post", &reg, None, Some(&user_ctx()));
    assert!(
        c.is_err(),
        "RBAC enforce 下未受 grant 的角色不得 count: {c:?}"
    );
}

// ─── C：insert / upsert 的归属键与默认值 ─────────────────────

fn owner_schema() -> Value {
    json!({
        "name": "Note",
        "collection": "notes",
        "idPrefix": "n",
        "timestamps": false,
        "read": ["creator"],
        "write": ["creator"],
        "fields": {
            "body": { "type": "string" },
            "createdBy": { "type": "string" }
        },
        "relations": {}
    })
}

#[test]
fn c1_insert_without_user_id_is_rejected() {
    let reg = registry_with(owner_schema());
    let ctx = Context {
        roles: Some(vec!["user".to_string()]),
        ..Default::default()
    };
    let err = plan_insert(
        "Note",
        &reg,
        Some(&ctx),
        &json!({ "body": "x" }),
        0,
        "n1",
        None,
    )
    .expect_err("creator 写入缺 userId 必须显式拒绝");
    assert!(err.contains("ERR_PERMISSION"), "{err}");
}

#[test]
fn c2_upsert_cannot_impersonate_owner() {
    let reg = registry_with(owner_schema());
    let ctx = user_ctx();
    let plan = plan_upsert(
        "Note",
        &reg,
        Some(&ctx),
        &json!({ "_id": "n9" }),
        &json!({ "body": "x", "createdBy": "victim" }),
        &json!({}),
        0,
        "n9",
    )
    .expect("upsert 应规划成功");
    let update = &plan["command"]["update"];
    assert_eq!(
        update["$setOnInsert"]["createdBy"], "u1",
        "upsert 新文档归属键必须是当前用户，不得采用显式传入值: {update}"
    );
    assert!(
        update["$set"].get("createdBy").is_none(),
        "归属键不得进入 $set: {update}"
    );
    let filter = &plan["command"]["filter"];
    assert!(
        filter.to_string().contains("u1"),
        "creator-only upsert 必须收窄到本人: {filter}"
    );
}

#[test]
fn c3_upsert_set_on_insert_keeps_nested_declared_defaults() {
    // 嵌套 object 的显式 default：insert 落库会补（递归），upsert 的 $setOnInsert
    // 必须同样补，否则同一语义在两个入口产生不同文档形状。
    let reg = registry_with(json!({
        "name": "Nest",
        "collection": "nests",
        "idPrefix": "ns",
        "timestamps": false,
        "fields": {
            "meta": {
                "type": "object",
                "fields": { "level": { "type": "number", "default": 7 } }
            }
        },
        "relations": {}
    }));
    let insert = plan_insert("Nest", &reg, None, &json!({ "meta": {} }), 0, "ns1", None)
        .expect("insert 应规划成功");
    assert_eq!(
        insert["command"]["doc"]["meta"]["level"], 7,
        "insert 落库应补嵌套 default: {insert}"
    );

    let upsert = plan_upsert(
        "Nest",
        &reg,
        None,
        &json!({ "_id": "ns2" }),
        &json!({ "meta": {} }),
        &json!({}),
        0,
        "ns2",
    )
    .expect("upsert 应规划成功");
    let update = &upsert["command"]["update"];
    assert_eq!(
        update["$set"]["meta"]["level"], 7,
        "upsert 显式 object 应按 insert 语义递归补 default（落 $set，避免与 $setOnInsert 路径冲突）: {update}"
    );
    assert!(
        update["$setOnInsert"].get("meta").is_none(),
        "同一路径不得同时出现在 $set / $setOnInsert（Mongo 冲突）: {update}"
    );
}

// ─── D：三后端列序 / `__present` 三态 ───────────────────────

fn present_schema() -> Value {
    json!({
        "name": "P",
        "collection": "ps",
        "idPrefix": "p",
        "timestamps": false,
        "fields": {
            "a": { "type": "string" },
            "b": { "type": "string" },
            "c": { "type": "string" }
        },
        "relations": {}
    })
}

#[test]
fn d1_insert_present_marks_explicit_null_but_not_missing_on_all_backends() {
    let reg = registry_with(present_schema());
    // b 显式 null（必须占列并进 present）；c 缺失（不得占列）
    let cmd = json!({
        "kind": "insertOne",
        "collection": "ps",
        "doc": { "_id": "p1", "a": "x", "b": null }
    });
    let mut seen: Vec<String> = Vec::new();
    for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
        let out = translate(backend, &cmd, &reg).expect("translate 应成功");
        let stmt = &out["stmts"][0];
        let text = stmt["text"].as_str().unwrap_or("");
        let params = stmt["params"].as_array().expect("params 必为数组");
        // 列序：_id → a → __present（声明序 + 哨兵末位）。显式 null 的 b 不占列
        // 但由 `__present` 标记「存在」——SQL 列默认 NULL，与 Mongo 的 null 语义等价。
        let start = text.find('(').expect("含列清单") + 1;
        let end = text[start..].find(')').expect("列清单闭合") + start;
        let cols: Vec<String> = text[start..end]
            .split(',')
            .map(|s| s.trim().trim_matches(['`', '"']).to_string())
            .collect();
        assert_eq!(
            cols,
            vec!["_id", "a", "__present"],
            "{backend:?} 列序不一致: {text}"
        );
        let present = params.last().and_then(|v| v.as_str()).unwrap_or("");
        assert_eq!(
            present, ",_id,a,b,",
            "{backend:?} 的 __present 必须标记显式键（含显式 null），且不含缺失字段 c"
        );
        seen.push(present.to_string());
    }
    assert_eq!(seen.len(), 3);
}

#[test]
fn d2_update_unset_and_set_null_both_maintain_present_sentinel() {
    let reg = registry_with(present_schema());
    let cmd = json!({
        "kind": "updateMany",
        "collection": "ps",
        "filter": { "_id": "p1" },
        "update": { "$set": { "a": null }, "$unset": { "b": "" } }
    });
    for backend in [Backend::Mysql, Backend::Postgres, Backend::Sqlite] {
        let out = translate(backend, &cmd, &reg).expect("translate 应成功");
        let text = out["stmts"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("__present"),
            "{backend:?} 的 update 必须维护 __present 哨兵（否则 null/缺失不可还原）: {text}"
        );
        // 显式 null 落 `a = ?` 并作为参数绑定；unset 的 b 以常量令牌内联进哨兵
        let params = out["stmts"][0]["params"].to_string();
        assert!(
            text.contains(&format!(
                "= {}",
                if matches!(backend, Backend::Mysql) {
                    "?"
                } else {
                    "$1"
                }
            )) || params.contains("null"),
            "{backend:?} 的 $set:null 必须真正落列并绑定 NULL: text={text} params={params}"
        );
        assert!(
            text.contains(",b,"),
            "{backend:?} 的 __present 必须移除 unset 键 b: {text}"
        );
    }
}

#[test]
fn d4_plan_update_keeps_explicit_null_in_set() {
    let reg = registry_with(present_schema());
    let plan = rust_store_core::command::plan_update(
        "P",
        &reg,
        None,
        &json!({ "_id": "p1" }),
        &json!({ "a": null }),
        &json!({}),
        0,
        rust_store_core::command::Probe::NotProbed,
    )
    .expect("显式 null 是合法更新数据，不得报「没有提供要更新的字段」");
    assert_eq!(
        plan["command"]["update"]["$set"]["a"],
        Value::Null,
        "规划层不得静默丢弃显式 null: {plan}"
    );
}

#[test]
fn d3_restore_distinguishes_explicit_null_from_missing() {
    let shape = json!({
        "columns": [
            { "alias": "a", "path": ["a"] },
            { "alias": "b", "path": ["b"] }
        ],
        "present": "__present"
    });
    // 显式 null：行值为 null 且哨兵含 ,a,
    let with_null = restore_rows_json(
        &shape,
        &json!([{ "a": null, "b": null, "__present": ",a," }]),
    )
    .expect("还原应成功");
    assert_eq!(with_null, json!([{ "a": null }]), "显式 null 必须保留键");
    // 缺失：哨兵不含 ,a,
    let missing = restore_rows_json(
        &shape,
        &json!([{ "a": null, "b": "v", "__present": ",b," }]),
    )
    .expect("还原应成功");
    assert_eq!(missing, json!([{ "b": "v" }]), "缺失字段不得还原成 null");
}
