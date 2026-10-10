//! N 系列 01 步：复现「待核实」项（N6a 联邦无 $condition 越权 / N6d 触发器写越权）
//!
//! 断言口径：断言**正确行为**（应注入行条件）。本文件先于修复运行——预期为红（复现成立），
//! 修复步骤 4/5 落地后转绿，作为长期回归用例。

use serde_json::{json, Map, Value};

use rust_store_core::command::{plan_query, plan_update, Probe};
use rust_store_core::federation::plan_federated;
use rust_store_core::permission::Context;
use rust_store_core::schema::Registry;

/// ownerOnly schema：`read: ["creator"]`（非 creator 用户仅可见自己创建的行）
fn owner_only_schema(name: &str, coll: &str, prefix: &str) -> Value {
    json!({
        "name": name,
        "collection": coll,
        "idPrefix": prefix,
        "timestamps": false,
        "read": ["creator"],
        "fields": { "name": { "type": "string" } },
        "relations": {}
    })
}

fn non_creator_ctx() -> Context {
    Context {
        user_id: Some("u1".to_string()),
        roles: Some(vec!["member".to_string()]),
        role: None,
        internal: false,
    }
}

/// N6a：联邦查询**未显式携带 `$condition`** 时，owner/RBAC 行条件也应注入（防越权读全表）。
/// 复现成功 = 当前 inject 缺失 → 本断言红。
#[test]
fn n6a_federation_without_condition_injects_owner() {
    let mut reg = Registry::new();
    reg.register(&owner_only_schema("Doc", "docs", "d"))
        .expect("Doc 注册成功");

    let plan = plan_federated(
        "Doc { name }",
        &Map::new(),
        &reg,
        Some(&non_creator_ctx()),
        &Value::Null,
    )
    .expect("plan_federated 应成功");

    let cmds = serde_json::to_string(&plan["sources"][0]["commands"]).expect("序列化 commands");
    assert!(
        cmds.contains("createdBy"),
        "N6a：联邦无 $condition 时应注入 owner 行条件（createdBy），实际 commands={cmds}"
    );
}

/// N6d：触发器写目标（op=update）应带行级探针（owner/RBAC 行条件并入 filter），
/// 否则可命中他人行。复现成功 = 当前 filter 无 createdBy → 本断言红。
#[test]
fn n6d_trigger_update_injects_row_condition() {
    let mut reg = Registry::new();
    reg.register(&owner_only_schema("Target", "targets", "g"))
        .expect("Target 注册成功");
    reg.register(&json!({
        "name": "Source",
        "collection": "sources",
        "idPrefix": "s",
        "timestamps": false,
        "fields": { "title": { "type": "string" }, "status": { "type": "string" } },
        "relations": {},
        "triggers": {
            "update": [
                {
                    "name": "sync",
                    "onFields": ["status"],
                    "into": "Target",
                    "op": "update",
                    "condition": { "name": "x" },
                    "data": { "name": "y" }
                }
            ]
        }
    }))
    .expect("Source 注册成功");

    let plan = plan_update(
        "Source",
        &reg,
        Some(&non_creator_ctx()),
        &json!({ "title": "a" }),
        &json!({ "status": "done" }),
        &json!({}),
        1000,
        Probe::Found(&json!({ "_id": "s1", "status": "open" })),
    )
    .expect("plan_update 应成功");

    let triggers = plan.get("triggers").expect("应含 triggers");
    let filter = serde_json::to_string(&triggers[0]["command"]["filter"]).expect("序列化 filter");
    assert!(
        filter.contains("createdBy"),
        "N6d：触发器 update 目标应注入行级条件（createdBy），实际 filter={filter}"
    );
}

/// N6d：触发器写目标（op=remove）同样应带行级探针。
#[test]
fn n6d_trigger_remove_injects_row_condition() {
    let mut reg = Registry::new();
    reg.register(&owner_only_schema("Target", "targets", "g"))
        .expect("Target 注册成功");
    reg.register(&json!({
        "name": "Source",
        "collection": "sources",
        "idPrefix": "s",
        "timestamps": false,
        "fields": { "title": { "type": "string" }, "status": { "type": "string" } },
        "relations": {},
        "triggers": {
            "remove": [
                {
                    "name": "cleanup",
                    "into": "Target",
                    "op": "remove",
                    "condition": { "name": "x" }
                }
            ]
        }
    }))
    .expect("Source 注册成功");

    let plan = rust_store_core::command::plan_remove(
        "Source",
        &reg,
        Some(&non_creator_ctx()),
        &json!({ "title": "a" }),
        Probe::Found(&json!({ "_id": "s1", "status": "open" })),
    )
    .expect("plan_remove 应成功");

    let triggers = plan.get("triggers").expect("应含 triggers");
    let filter = serde_json::to_string(&triggers[0]["command"]["filter"]).expect("序列化 filter");
    assert!(
        filter.contains("createdBy"),
        "N6d：触发器 remove 目标应注入行级条件（createdBy），实际 filter={filter}"
    );
}

// ─── 步骤 6：A2 五路径行级权限一致性矩阵 ─────────────────────

fn post_with_author_schema() -> Value {
    json!({
        "name": "Post",
        "collection": "posts",
        "timestamps": false,
        "fields": { "title": { "type": "string" }, "authorId": { "type": "string" } },
        "relations": {
            "author": { "model": "Author", "type": "one", "localField": "authorId", "foreignField": "_id" }
        }
    })
}

/// 取 aggregate 形态命令中 `as == rel` 的 `$lookup` 内层 `$match`
fn lookup_inner_match(cmd: &Value, rel: &str) -> Option<Value> {
    cmd.get("pipeline")?.as_array()?.iter().find_map(|s| {
        let lk = s.get("$lookup")?;
        if lk.get("as").and_then(|v| v.as_str()) != Some(rel) {
            return None;
        }
        lk.get("pipeline")?
            .as_array()?
            .iter()
            .find_map(|x| x.get("$match").cloned())
    })
}

/// A2：同一 ownerOnly 行限（`read: ["creator"]`）+ 同一 ctx（u1/member）下，
/// 「根查询 / 关系子查询($lookup) / $group 关系路径 / 联邦查询 / 触发器写」五条路径
/// 都必须注入同一 owner 行条件（`createdBy == u1`），任一缺失即行级权限旁路。
#[test]
fn a2_five_paths_inject_same_owner_row_condition() {
    let mut reg = Registry::new();
    reg.register(&owner_only_schema("Doc", "docs", "d"))
        .expect("Doc 注册成功");
    reg.register(&owner_only_schema("Author", "authors", "a"))
        .expect("Author 注册成功");
    reg.register(&post_with_author_schema())
        .expect("Post 注册成功");
    reg.register(&json!({
        "name": "Source",
        "collection": "sources",
        "idPrefix": "s",
        "timestamps": false,
        "fields": { "title": { "type": "string" }, "status": { "type": "string" } },
        "relations": {},
        "triggers": {
            "update": [
                {
                    "name": "sync",
                    "onFields": ["status"],
                    "into": "Doc",
                    "op": "update",
                    "condition": { "name": "x" },
                    "data": { "name": "y" }
                }
            ]
        }
    }))
    .expect("Source 注册成功");
    let ctx = non_creator_ctx();
    let params = Map::new();

    // ① 根查询
    let root = plan_query("Doc{ name }", &params, &reg, Some(&ctx)).expect("根查询应放行");
    let root_cmd = serde_json::to_string(&root.commands[0]).expect("序列化");
    assert!(
        root_cmd.contains("createdBy"),
        "① 根查询应注入 owner 行条件: {root_cmd}"
    );

    // ② 关系子查询（$lookup 内层 $match）
    let rel = plan_query("Post{ title, author{ name } }", &params, &reg, Some(&ctx))
        .expect("关系查询应放行");
    let rel_match = lookup_inner_match(&rel.commands[0], "author").expect("应有 author 的 $lookup");
    assert!(
        rel_match.to_string().contains("createdBy"),
        "② 关系子查询应注入 author 行条件: {rel_match}"
    );

    // ③ $group 关系路径（与 ② 同源注入）
    let mut gparams = Map::new();
    gparams.insert(
        "g0".to_string(),
        json!({ "by": ["author.name"], "agg": { "n": { "$count": "*" } } }),
    );
    let grp = plan_query(
        "Post($group:@g0){ author.name, n }",
        &gparams,
        &reg,
        Some(&ctx),
    )
    .expect("$group 关系路径应放行");
    let grp_match = lookup_inner_match(&grp.commands[0], "author").expect("应有 author 的 $lookup");
    assert!(
        grp_match.to_string().contains("createdBy"),
        "③ $group 关系路径应注入行条件: {grp_match}"
    );

    // ④ 联邦查询（未显式携带 $condition）
    let fed = plan_federated("Doc { name }", &params, &reg, Some(&ctx), &Value::Null)
        .expect("联邦查询应成功");
    let fed_cmd = serde_json::to_string(&fed["sources"][0]["commands"]).expect("序列化");
    assert!(
        fed_cmd.contains("createdBy"),
        "④ 联邦查询应注入 root 行条件: {fed_cmd}"
    );

    // ⑤ 触发器写（update 目标 filter）
    let upd = plan_update(
        "Source",
        &reg,
        Some(&ctx),
        &json!({ "title": "a" }),
        &json!({ "status": "done" }),
        &json!({}),
        1000,
        Probe::Found(&json!({ "_id": "s1", "status": "open" })),
    )
    .expect("触发器计划应成功");
    let trig = serde_json::to_string(&upd["triggers"][0]["command"]["filter"]).expect("序列化");
    assert!(
        trig.contains("createdBy"),
        "⑤ 触发器写应注入目标行条件: {trig}"
    );
}
