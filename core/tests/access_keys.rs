//! `accessKeys` 声明式行级权限：注册期校验 + 读路径行条件（01 号执行文档 §4.7）。
//!
//! 语义基线：
//! - 读路径按 `read` 引用键生成 `{字段: {"$in": 清单}}`，与 owner / RBAC 条件 `$and` 叠加；
//! - 单值字段 = 标量成员判定，`array` 字段 = 交集命中（Mongo 原生 `$in` 语义）；
//! - ctx 缺键且未 bypass → `ERR_PERMISSION:ACCESS:`（fail closed）；
//! - `清单: []` 是结论（读 0 行），与缺键严格区分；
//! - `ctx: None` / `internal` / 命中 `bypassRoles` → 直通（不注入该键条件）。

use serde_json::{json, Value};

use rust_store_core::command::{
    plan_insert, plan_query, plan_remove, plan_update, Probe, QueryPlan,
};
use rust_store_core::permission::{context_from_value, Context};
use rust_store_core::schema::Registry;

const ACCESS_PREFIX: &str = "ERR_PERMISSION:ACCESS:";

// ─── fixtures ────────────────────────────────────────────────

/// 单值字段（`satCode`）绑定读/写两个 ctx 键
fn sat_schema() -> Value {
    json!({
        "name": "Sat",
        "collection": "sats",
        "timestamps": false,
        "fields": {
            "title": { "type": "string" },
            "satCode": { "type": "string" }
        },
        "relations": {},
        "accessKeys": {
            "satCode": { "read": "satsRead", "write": "satsWrite" }
        },
    })
}

/// 数组字段（`bidders`）绑定读键
fn bid_schema() -> Value {
    json!({
        "name": "Bid",
        "collection": "bids",
        "timestamps": false,
        "fields": {
            "title": { "type": "string" },
            "bidders": { "type": "array" }
        },
        "relations": {},
        "accessKeys": {
            "bidders": { "read": "biddersRead" }
        },
    })
}

/// 两个绑定键（验证 `$and` 组合）
fn multi_schema() -> Value {
    json!({
        "name": "Multi",
        "collection": "multis",
        "timestamps": false,
        "fields": {
            "title": { "type": "string" },
            "satCode": { "type": "string" },
            "regionCode": { "type": "string" }
        },
        "relations": {},
        "accessKeys": {
            "satCode": { "read": "satsRead" },
            "regionCode": { "read": "regionsRead" }
        },
    })
}

/// 静态 owner 条件（`read: ["creator"]`）+ accessKeys（验证两条件 `$and` 叠加）
fn owner_schema() -> Value {
    json!({
        "name": "Post",
        "collection": "posts",
        "timestamps": false,
        "fields": {
            "title": { "type": "string" },
            "satCode": { "type": "string" }
        },
        "relations": {},
        "read": ["creator"],
        "accessKeys": {
            "satCode": { "read": "satsRead" }
        },
    })
}

/// 带 bypassRoles 的单键 schema
fn bypass_schema() -> Value {
    json!({
        "name": "Sat",
        "collection": "sats",
        "timestamps": false,
        "fields": {
            "title": { "type": "string" },
            "satCode": { "type": "string" }
        },
        "relations": {},
        "accessKeys": {
            "satCode": { "read": "satsRead", "bypassRoles": ["admin"] }
        },
    })
}

fn registry_with(defns: &[Value]) -> Registry {
    let mut reg = Registry::new();
    for d in defns {
        reg.register(d).expect("schema 注册应成功");
    }
    reg
}

fn params_of(extra: Value) -> serde_json::Map<String, Value> {
    extra.as_object().cloned().unwrap_or_default()
}

fn ctx_of(v: Value) -> Context {
    context_from_value(&v).expect("ctx JSON 应解析为对象")
}

fn plan_sat(reg: &Registry, ctx: Option<&Context>) -> QueryPlan {
    plan_query("Sat{ title }", &params_of(json!({})), reg, ctx).expect("规划应成功")
}

// ─── 注册期校验（fail-fast） ─────────────────────────────────

#[test]
fn parse_accepts_valid_declaration() {
    let reg = registry_with(&[sat_schema()]);
    let schema = reg.get("Sat").expect("Sat 应已注册");
    assert_eq!(schema.access_keys.len(), 1, "应解析出 1 个绑定键");
    let def = &schema.access_keys[0];
    assert_eq!(def.field, "satCode");
    assert_eq!(def.read, "satsRead");
    assert_eq!(def.write.as_deref(), Some("satsWrite"));
    assert!(def.bypass_roles.is_empty(), "未声明 bypassRoles 应为空");
}

#[test]
fn parse_absent_access_keys_is_empty() {
    let reg = registry_with(&[json!({
        "name": "Plain",
        "collection": "plains",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {},
    })]);
    assert!(
        reg.get("Plain").unwrap().access_keys.is_empty(),
        "未声明 accessKeys 应为空表"
    );
}

#[test]
fn parse_rejects_unknown_field() {
    let mut reg = Registry::new();
    let e = reg
        .register(&json!({
            "name": "Bad",
            "collection": "bads",
            "timestamps": false,
            "fields": { "title": { "type": "string" } },
            "relations": {},
            "accessKeys": { "ghost": { "read": "ghostRead" } }
        }))
        .expect_err("绑定未声明字段应 Err");
    assert!(e.contains("ghost") && e.contains("fields"), "{e}");
}

#[test]
fn parse_rejects_missing_or_empty_read() {
    let mut reg = Registry::new();
    let base = |ak: Value| {
        json!({
            "name": "Bad",
            "collection": "bads",
            "timestamps": false,
            "fields": { "satCode": { "type": "string" } },
            "relations": {},
            "accessKeys": ak
        })
    };
    assert!(
        reg.register(&base(json!({ "satCode": { } }))).is_err(),
        "缺少 read 应 Err"
    );
    assert!(
        reg.register(&base(json!({ "satCode": { "read": "" } })))
            .is_err(),
        "read 空串应 Err"
    );
    assert!(
        reg.register(&base(json!({ "satCode": { "read": 1 } })))
            .is_err(),
        "read 非字符串应 Err"
    );
}

#[test]
fn parse_rejects_bad_write_and_bypass_roles() {
    let mut reg = Registry::new();
    let base = |ak: Value| {
        json!({
            "name": "Bad",
            "collection": "bads",
            "timestamps": false,
            "fields": { "satCode": { "type": "string" } },
            "relations": {},
            "accessKeys": ak
        })
    };
    assert!(
        reg.register(&base(json!({ "satCode": { "read": "r", "write": 1 } })))
            .is_err(),
        "write 非字符串应 Err"
    );
    assert!(
        reg.register(&base(
            json!({ "satCode": { "read": "r", "bypassRoles": "admin" } })
        ))
        .is_err(),
        "bypassRoles 非数组应 Err"
    );
    assert!(
        reg.register(&base(
            json!({ "satCode": { "read": "r", "bypassRoles": [""] } })
        ))
        .is_err(),
        "bypassRoles 空串元素应 Err"
    );
    assert!(
        reg.register(&base(
            json!({ "satCode": { "read": "r", "bypassRoles": [1] } })
        ))
        .is_err(),
        "bypassRoles 非字符串元素应 Err"
    );
    // write: null 合法（只读控）
    reg.register(&base(json!({ "satCode": { "read": "r", "write": null } })))
        .expect("write=null 应合法");
}

#[test]
fn parse_rejects_non_object_access_keys() {
    let mut reg = Registry::new();
    let e = reg
        .register(&json!({
            "name": "Bad",
            "collection": "bads",
            "timestamps": false,
            "fields": { "satCode": { "type": "string" } },
            "relations": {},
            "accessKeys": ["satCode"]
        }))
        .expect_err("accessKeys 非对象应 Err");
    assert!(e.contains("accessKeys"), "{e}");
}

// ─── 读路径行条件 ───────────────────────────────────────────

#[test]
fn read_injects_in_condition() {
    let reg = registry_with(&[sat_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "satsRead": ["C1", "C2"] }));
    let plan = plan_sat(&reg, Some(&c));
    let s = plan.commands[0].to_string();
    assert!(s.contains("satCode"), "应注入 satCode 行条件: {s}");
    assert!(s.contains("$in"), "应为 $in 条件: {s}");
    assert!(s.contains("C1"), "清单元素应原样落入条件: {s}");
}

#[test]
fn empty_list_reads_nothing() {
    let reg = registry_with(&[sat_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "satsRead": [] }));
    let plan = plan_sat(&reg, Some(&c));
    let s = plan.commands[0].to_string();
    assert!(
        s.contains("$in\":[]"),
        "空清单应生成 $in: []（读 0 行，而非缺键报错）: {s}"
    );
}

#[test]
fn array_field_read_uses_in_intersection() {
    let reg = registry_with(&[bid_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "biddersRead": ["u9", "u8"] }));
    let plan =
        plan_query("Bid{ title }", &params_of(json!({})), &reg, Some(&c)).expect("规划应成功");
    let s = plan.commands[0].to_string();
    assert!(s.contains("bidders"), "数组字段同样以 $in 表达交集: {s}");
    assert!(s.contains("u9"), "{s}");
}

#[test]
fn multiple_keys_combine_with_and() {
    let reg = registry_with(&[multi_schema()]);
    let c = ctx_of(json!({
        "userId": "u1",
        "satsRead": ["C1"],
        "regionsRead": ["R1"]
    }));
    let plan =
        plan_query("Multi{ title }", &params_of(json!({})), &reg, Some(&c)).expect("规划应成功");
    let s = plan.commands[0].to_string();
    assert!(s.contains("$and"), "多键应 $and 组合: {s}");
    assert!(s.contains("satCode") && s.contains("regionCode"), "{s}");
}

#[test]
fn owner_condition_merges_with_access_condition() {
    let reg = registry_with(&[owner_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "roles": ["reader"], "satsRead": ["C1"] }));
    let plan = plan_query("Post{ title }", &params_of(json!({})), &reg, Some(&c))
        .expect("两条件叠加应放行规划");
    let s = plan.commands[0].to_string();
    assert!(s.contains("createdBy"), "静态 owner 条件应保留: {s}");
    assert!(s.contains("satCode"), "accessKeys 行条件应叠加: {s}");
    assert!(s.contains("$and"), "两条件应 $and 组合: {s}");
}

#[test]
fn missing_ctx_key_fails_closed() {
    let reg = registry_with(&[sat_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "roles": ["reader"] }));
    let err = plan_query("Sat{ title }", &params_of(json!({})), &reg, Some(&c))
        .expect_err("ctx 缺清单键应拒绝（禁静默放行全表）");
    assert!(err.starts_with(ACCESS_PREFIX), "{err}");
    assert!(err.contains("satsRead"), "错误应指明缺失的 ctx 键: {err}");
}

#[test]
fn invalid_ctx_list_shape_fails_closed() {
    let reg = registry_with(&[sat_schema()]);
    // 非数组 → 解析期记错误表，引用时显式报错
    let c = ctx_of(json!({ "userId": "u1", "satsRead": "C1" }));
    let err = plan_query("Sat{ title }", &params_of(json!({})), &reg, Some(&c))
        .expect_err("清单非数组应拒绝");
    assert!(err.starts_with(ACCESS_PREFIX), "{err}");
    assert!(err.contains("数组"), "{err}");

    // 数组含对象元素 → 同样拒绝
    let c2 = ctx_of(json!({ "userId": "u1", "satsRead": [{ "x": 1 }] }));
    let err2 = plan_query("Sat{ title }", &params_of(json!({})), &reg, Some(&c2))
        .expect_err("清单元素非标量应拒绝");
    assert!(err2.starts_with(ACCESS_PREFIX), "{err2}");
}

#[test]
fn numeric_list_elements_accepted() {
    let reg = registry_with(&[multi_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "satsRead": [1, 2], "regionsRead": ["R1"] }));
    let plan = plan_query("Multi{ title }", &params_of(json!({})), &reg, Some(&c))
        .expect("数字清单元素应合法");
    let s = plan.commands[0].to_string();
    assert!(s.contains("satCode"), "{s}");
}

// ─── 直通语义（bypass / internal / ctx=None） ───────────────

#[test]
fn bypass_role_skips_key() {
    let reg = registry_with(&[bypass_schema()]);
    // admin 命中 bypassRoles 且 ctx 无清单键 → 仍放行
    let admin = ctx_of(json!({ "userId": "u1", "roles": ["admin"] }));
    let plan = plan_sat(&reg, Some(&admin));
    let s = plan.commands[0].to_string();
    assert!(!s.contains("satCode"), "bypass 角色不应注入该键条件: {s}");

    // 非 bypass 角色且缺键 → fail closed
    let user = ctx_of(json!({ "userId": "u2", "roles": ["viewer"] }));
    let err = plan_query("Sat{ title }", &params_of(json!({})), &reg, Some(&user))
        .expect_err("非 bypass 角色缺键应拒绝");
    assert!(err.starts_with(ACCESS_PREFIX), "{err}");
}

#[test]
fn internal_ctx_skips_key() {
    let reg = registry_with(&[sat_schema()]);
    let sys = Context::system();
    let plan = plan_sat(&reg, Some(&sys));
    let s = plan.commands[0].to_string();
    assert!(!s.contains("satCode"), "internal 应跳过 accessKeys: {s}");
}

#[test]
fn ctx_none_is_transparent() {
    let reg = registry_with(&[sat_schema()]);
    let plan = plan_sat(&reg, None);
    let s = plan.commands[0].to_string();
    assert!(
        !s.contains("satCode"),
        "ctx=None 沿用系统内部调用契约（不注入）: {s}"
    );
}

// ─── 写路径：payload 子集校验 ───────────────────────────────

/// 数组字段 + 写键
fn bid_write_schema() -> Value {
    json!({
        "name": "Bid",
        "collection": "bids",
        "timestamps": false,
        "fields": {
            "title": { "type": "string" },
            "bidders": { "type": "array" }
        },
        "relations": {},
        "accessKeys": {
            "bidders": { "read": "biddersRead", "write": "biddersWrite" }
        },
    })
}

/// 写键 + bypassRoles
fn bypass_write_schema() -> Value {
    json!({
        "name": "Sat",
        "collection": "sats",
        "timestamps": false,
        "fields": {
            "title": { "type": "string" },
            "satCode": { "type": "string" }
        },
        "relations": {},
        "accessKeys": {
            "satCode": { "read": "satsRead", "write": "satsWrite", "bypassRoles": ["admin"] }
        },
    })
}

/// 字段级写白名单（satCode 仅 admin 可写）
fn field_whitelist_schema() -> Value {
    json!({
        "name": "Sat",
        "collection": "sats",
        "timestamps": false,
        "idPrefix": "s",
        "fields": {
            "title": { "type": "string" },
            "satCode": { "type": "string", "write": ["admin"] }
        },
        "relations": {},
        "accessKeys": {
            "satCode": { "read": "satsRead", "write": "satsWrite" }
        },
    })
}

fn insert_sat(reg: &Registry, c: &Context, data: Value) -> Result<Value, String> {
    plan_insert("Sat", reg, Some(c), &data, 0, "s1", None)
}

#[test]
fn insert_single_value_out_of_list_denied() {
    let reg = registry_with(&[sat_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "satsWrite": ["C1"] }));
    let err = insert_sat(
        &reg,
        &c,
        json!({ "_id": "s1", "title": "x", "satCode": "C2" }),
    )
    .expect_err("单值目标越权应拒绝");
    assert!(err.starts_with(ACCESS_PREFIX), "{err}");
    assert!(err.contains("satCode"), "{err}");
}

#[test]
fn insert_array_target_with_outside_element_denied() {
    let reg = registry_with(&[bid_write_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "biddersWrite": ["u1", "u2"] }));
    let err = plan_insert(
        "Bid",
        &reg,
        Some(&c),
        &json!({ "_id": "b1", "title": "x", "bidders": ["u1", "u3"] }),
        0,
        "b1",
        None,
    )
    .expect_err("array 目标含清单外元素应拒绝");
    assert!(err.starts_with(ACCESS_PREFIX), "{err}");
    assert!(err.contains("u3"), "{err}");
}

#[test]
fn insert_array_target_subset_ok() {
    let reg = registry_with(&[bid_write_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "biddersWrite": ["u1", "u2"] }));
    let plan = plan_insert(
        "Bid",
        &reg,
        Some(&c),
        &json!({ "_id": "b1", "title": "x", "bidders": ["u1"] }),
        0,
        "b1",
        None,
    )
    .expect("array 目标为清单子集应放行");
    let s = plan["command"].to_string();
    assert!(s.contains("u1"), "{s}");
}

#[test]
fn insert_missing_write_key_fails_closed() {
    let reg = registry_with(&[sat_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "satsRead": ["C1"] }));
    let err = insert_sat(
        &reg,
        &c,
        json!({ "_id": "s1", "title": "x", "satCode": "C1" }),
    )
    .expect_err("ctx 缺 write 清单键应拒绝");
    assert!(err.starts_with(ACCESS_PREFIX), "{err}");
    assert!(err.contains("satsWrite"), "{err}");
}

#[test]
fn insert_empty_write_list_denied() {
    let reg = registry_with(&[sat_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "satsWrite": [] }));
    let err = insert_sat(
        &reg,
        &c,
        json!({ "_id": "s1", "title": "x", "satCode": "C1" }),
    )
    .expect_err("空写清单应拒绝");
    assert!(err.starts_with(ACCESS_PREFIX), "{err}");
    assert!(err.contains("为空"), "{err}");
}

#[test]
fn insert_bypass_role_ok() {
    let reg = registry_with(&[bypass_write_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "roles": ["admin"] }));
    insert_sat(
        &reg,
        &c,
        json!({ "_id": "s1", "title": "x", "satCode": "ANY" }),
    )
    .expect("bypassRoles 命中应跳过该键写校验");
}

#[test]
fn field_whitelist_filtered_access_field_not_validated() {
    let reg = registry_with(&[field_whitelist_schema()]);
    // satCode 仅 admin 可写 → member 写入被字段白名单剔除；空写清单不应误报
    let c = ctx_of(json!({ "userId": "u1", "roles": ["member"], "satsWrite": [] }));
    let plan = insert_sat(&reg, &c, json!({ "title": "x", "satCode": "C2" }))
        .expect("被字段白名单过滤掉的字段不应校验");
    assert!(
        !plan["command"].to_string().contains("satCode"),
        "satCode 不应落库: {plan}"
    );
}

// ─── 写路径：探针文档子集判定 ───────────────────────────────

#[test]
fn update_probe_doc_value_out_of_list_denied() {
    let reg = registry_with(&[sat_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "satsWrite": ["C1"] }));
    let cond = json!({ "_id": "s1" });
    let data = json!({ "title": "y" });

    // 首入：access 受控且有条件 → 必须探针，投影含受控字段
    let first = plan_update(
        "Sat",
        &reg,
        Some(&c),
        &cond,
        &data,
        &json!({}),
        0,
        Probe::NotProbed,
    )
    .expect("首入应返回探针");
    let probe = &first["needsProbe"];
    let proj = probe["projection"].as_object().expect("探针应有投影");
    assert!(
        proj.contains_key("satCode"),
        "探针投影应含受控字段: {probe}"
    );

    // 重入：文档当前值越权 → 拒绝
    let err = plan_update(
        "Sat",
        &reg,
        Some(&c),
        &cond,
        &data,
        &json!({}),
        0,
        Probe::Found(&json!({ "_id": "s1", "satCode": "C2" })),
    )
    .expect_err("文档当前值越权应拒绝");
    assert!(err.starts_with(ACCESS_PREFIX), "{err}");

    // 重入：文档当前值合法 → 放行
    plan_update(
        "Sat",
        &reg,
        Some(&c),
        &cond,
        &data,
        &json!({}),
        0,
        Probe::Found(&json!({ "_id": "s1", "satCode": "C1" })),
    )
    .expect("文档当前值在清单内应放行");
}

#[test]
fn remove_probe_doc_value_out_of_list_denied() {
    let reg = registry_with(&[sat_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "satsWrite": ["C1"] }));
    let cond = json!({ "_id": "s1" });
    let err = plan_remove(
        "Sat",
        &reg,
        Some(&c),
        &cond,
        Probe::Found(&json!({ "_id": "s1", "satCode": "C2" })),
    )
    .expect_err("删除越权文档应拒绝");
    assert!(err.starts_with(ACCESS_PREFIX), "{err}");
    plan_remove(
        "Sat",
        &reg,
        Some(&c),
        &cond,
        Probe::Found(&json!({ "_id": "s1", "satCode": "C1" })),
    )
    .expect("删除清单内文档应放行");
}

/// 受控更新 + 合法当前值（使探针重入走到写数据构造）
fn found_sat() -> Value {
    json!({ "_id": "s1", "satCode": "C1" })
}

#[test]
fn raw_unset_access_field_denied_and_set_ok() {
    let reg = registry_with(&[sat_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "satsWrite": ["C1"] }));
    let cond = json!({ "_id": "s1" });
    let err = plan_update(
        "Sat",
        &reg,
        Some(&c),
        &cond,
        &json!({ "$unset": { "satCode": "" } }),
        &json!({}),
        0,
        Probe::Found(&found_sat()),
    )
    .expect_err("$unset 受控字段应拒绝");
    assert!(err.starts_with(ACCESS_PREFIX), "{err}");

    let err_set = plan_update(
        "Sat",
        &reg,
        Some(&c),
        &cond,
        &json!({ "$set": { "satCode": "C3" } }),
        &json!({}),
        0,
        Probe::Found(&found_sat()),
    )
    .expect_err("$set 越权值应拒绝");
    assert!(err_set.starts_with(ACCESS_PREFIX), "{err_set}");

    plan_update(
        "Sat",
        &reg,
        Some(&c),
        &cond,
        &json!({ "$set": { "satCode": "C1" } }),
        &json!({}),
        0,
        Probe::Found(&found_sat()),
    )
    .expect("$set 清单内值应放行");
}

#[test]
fn update_without_condition_denied_when_restricted() {
    let reg = registry_with(&[sat_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "satsWrite": ["C1"] }));
    let err = plan_update(
        "Sat",
        &reg,
        Some(&c),
        &json!({}),
        &json!({ "title": "y" }),
        &json!({}),
        0,
        Probe::NotProbed,
    )
    .expect_err("无条件下写受控模型应拒绝");
    assert!(err.starts_with(ACCESS_PREFIX), "{err}");
    assert!(err.contains("条件为空"), "{err}");
}

// ─── A1：写探针三态（未命中 ≠ 越权） ───────────────────────

const NOT_FOUND_PREFIX: &str = "ERR_NOT_FOUND:";

#[test]
fn a1_probe_three_states_update() {
    let reg = registry_with(&[sat_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "satsWrite": ["C1"] }));
    let cond = json!({ "_id": "s1" });
    let data = json!({ "title": "y" });
    let call =
        |probe: Probe<'_>| plan_update("Sat", &reg, Some(&c), &cond, &data, &json!({}), 0, probe);

    // ① NotProbed：仅返回探针，不判定（不得凭空报错）
    let first = call(Probe::NotProbed).expect("首入应返回探针");
    assert!(
        first.get("needsProbe").is_some(),
        "应含 needsProbe: {first}"
    );

    // ② NotFound：行不存在 → ERR_NOT_FOUND，**不得**映射成权限拒绝
    let err = call(Probe::NotFound).expect_err("探针未命中应显式报不存在");
    assert!(err.starts_with(NOT_FOUND_PREFIX), "{err}");
    assert!(
        !err.starts_with(ACCESS_PREFIX) && !err.starts_with("ERR_PERMISSION:"),
        "行不存在被错误地映射成权限错误: {err}"
    );

    // ③ Found 但当前值越权：仍是原权限文案（accessKeys 前缀），不是 not_found
    let denied =
        call(Probe::Found(&json!({ "_id": "s1", "satCode": "C2" }))).expect_err("越权应拒绝");
    assert!(denied.starts_with(ACCESS_PREFIX), "{denied}");
    assert!(!denied.starts_with(NOT_FOUND_PREFIX), "{denied}");

    // ④ Found 且有权：成功
    call(Probe::Found(&json!({ "_id": "s1", "satCode": "C1" }))).expect("有权应放行");
}

#[test]
fn a1_probe_three_states_remove() {
    let reg = registry_with(&[sat_schema()]);
    let c = ctx_of(json!({ "userId": "u1", "satsWrite": ["C1"] }));
    let cond = json!({ "_id": "s1" });
    let call = |probe: Probe<'_>| plan_remove("Sat", &reg, Some(&c), &cond, probe);

    let first = call(Probe::NotProbed).expect("首入应返回探针");
    assert!(
        first.get("needsProbe").is_some(),
        "应含 needsProbe: {first}"
    );

    let err = call(Probe::NotFound).expect_err("探针未命中应显式报不存在");
    assert!(err.starts_with(NOT_FOUND_PREFIX), "{err}");
    assert!(
        !err.starts_with(ACCESS_PREFIX) && !err.starts_with("ERR_PERMISSION:"),
        "行不存在被错误地映射成权限错误: {err}"
    );

    let denied =
        call(Probe::Found(&json!({ "_id": "s1", "satCode": "C2" }))).expect_err("越权应拒绝");
    assert!(denied.starts_with(ACCESS_PREFIX), "{denied}");
    assert!(!denied.starts_with(NOT_FOUND_PREFIX), "{denied}");

    call(Probe::Found(&json!({ "_id": "s1", "satCode": "C1" }))).expect("有权应放行");
}
