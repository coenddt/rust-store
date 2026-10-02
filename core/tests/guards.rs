//! 扩展守卫测试：timestamps 值校验 + 条件形状（U1~U4）
//!
//! ① schema 可声明秒级时间戳（`timestamps: 's'`），非法值注册即报错；
//! ② 数组/对象字段过滤（U1/U2）与对象点号路径过滤/排序（U3/U4）standard 档放行
//!    （JSON 列落地）、text2query 档显式 Err（双门禁分离 §4.1 #4）；
//! ③ 关系聚合谓词的子级 `filter` 同受同一门禁约束（§4.1 #4 → 8c-1）。

use serde_json::{json, Map, Value};

use rust_store_core::command::{plan_query, plan_query_with_count, QueryPlan, ERR_TEXT2QUERY};
use rust_store_core::federation::plan_federated;
use rust_store_core::schema::{Profile, Registry};

fn registry_with(defn: Value) -> Registry {
    let mut reg = Registry::new();
    reg.register(&defn).expect("schema 注册应成功");
    reg
}

fn base_schema(timestamps: Value) -> Value {
    json!({
        "name": "Post",
        "collection": "posts",
        "timestamps": timestamps,
        "fields": { "title": { "type": "string" } },
        "relations": {},
    })
}

fn params_of(extra: Value) -> Map<String, Value> {
    extra.as_object().cloned().unwrap_or_default()
}

// ─── timestamps 值校验 ───────────────────────────────────────

#[test]
fn timestamps_accepts_bool_and_units() {
    for v in [
        json!(true),
        json!(false),
        json!("ms"),
        json!("s"),
        Value::Null,
    ] {
        let mut reg = Registry::new();
        reg.register(&base_schema(v.clone()))
            .unwrap_or_else(|e| panic!("timestamps = {} 应可注册: {}", v, e));
    }
}

#[test]
fn timestamps_rejects_invalid_values() {
    for v in [json!("years"), json!(1)] {
        let mut reg = Registry::new();
        let err = reg
            .register(&base_schema(v.clone()))
            .expect_err("非法 timestamps 应报错");
        assert!(err.contains("timestamps 仅支持"), "错误信息异常: {}", err);
    }
}

// ─── queryOne `$limit(1)` 下推 + 权限错误哨兵前缀 ────────────

use rust_store_core::command::{plan_query_one, Mode};
use rust_store_core::permission::Context;

#[test]
fn query_one_pushes_limit_one_when_absent() {
    let reg = registry_with(base_schema(json!(true)));
    let plan =
        plan_query_one("Post{ title }", &params_of(json!({})), &reg, None).expect("应规划成功");
    assert_eq!(plan.mode, Mode::Aggregate, "注入 limit 后应走标准聚合");
    let pipeline = plan.commands[0]["pipeline"].as_array().unwrap();
    assert!(
        pipeline
            .iter()
            .any(|s| s.get("$limit").map(|v| v == &json!(1)).unwrap_or(false)),
        "pipeline 应包含下推的 $limit(1)"
    );
}

#[test]
fn query_one_keeps_user_limit() {
    let reg = registry_with(base_schema(json!(true)));
    let plan = plan_query_one(
        "Post($limit:@l){ title }",
        &params_of(json!({ "l": 7 })),
        &reg,
        None,
    )
    .expect("应规划成功");
    let pipeline = plan.commands[0]["pipeline"].as_array().unwrap();
    assert!(
        pipeline
            .iter()
            .any(|s| s.get("$limit").map(|v| v == &json!(7)).unwrap_or(false)),
        "保留用户 $limit(7)"
    );
    assert!(
        pipeline
            .iter()
            .all(|s| !s.get("$limit").map(|v| v == &json!(1)).unwrap_or(false)),
        "不应注入 $limit(1)"
    );
}

#[test]
fn permission_errors_carry_stable_prefix() {
    use rust_store_core::command::{ERR_NO_WRITE, ERR_PERM_PREFIX};

    // guest 角色写 schema → 权限错误；断言稳定前缀（Host 按前缀映射 PermissionError）
    // 拒写语义已清单化：guest 写拒由显式 set_deny_write_roles 承载（原硬编码随清单化移除）
    let mut reg = registry_with(base_schema(json!(true)));
    reg.set_deny_write_roles(vec!["guest".to_string()]);
    let ctx = Context {
        roles: Some(vec!["guest".to_string()]),
        ..Default::default()
    };
    let err = rust_store_core::command::plan_mutation("Post", &reg, Some(&ctx), &json!({}), 0, &[])
        .expect_err("guest 写入应报错");
    assert!(
        err.starts_with(ERR_PERM_PREFIX),
        "权限错误应携带稳定前缀: {err}"
    );
    assert_eq!(ERR_NO_WRITE, format!("{ERR_PERM_PREFIX}无写入权限"));
}

// ─── DB 独有能力受控放开（§4.1 #5 `$pipeline` 直通）：standard 放行 / text2query Err ──

const PIPELINE_GQL: &str = "Post($pipeline:@p0){ title }";

fn standard_registry() -> Registry {
    registry_with(base_schema(json!(true)))
}

fn t2q_registry() -> Registry {
    let mut reg = registry_with(base_schema(json!(true)));
    reg.set_profile(Profile::Text2Query);
    reg
}

fn pipeline_plan(reg: &Registry, ctx: Option<&Context>) -> Result<QueryPlan, String> {
    plan_query(
        PIPELINE_GQL,
        &params_of(json!({ "p0": [{ "$match": { "title": "a" } }] })),
        reg,
        ctx,
    )
}

#[test]
fn user_pipeline_allowed_in_standard_and_projected() {
    let plan = pipeline_plan(&standard_registry(), None)
        .expect("standard 档 `$pipeline` 直通应放行（Mongo 源 DB 独有能力）");
    let stages = plan.commands[0]["pipeline"]
        .as_array()
        .expect("应为聚合命令（含 pipeline 数组）");
    assert_eq!(
        stages[0],
        json!({ "$match": { "title": "a" } }),
        "用户阶段应原样下发"
    );
    assert_eq!(
        stages.last(),
        Some(&json!({ "$project": { "_id": 1, "title": 1 } })),
        "R9：`$pipeline` 模式仍按 GQL 请求字段施加顶层投影"
    );
    assert!(
        plan.postprocess.is_none(),
        "`$pipeline` 全权模式不做后处理（postprocess = None）"
    );
}

#[test]
fn user_pipeline_rejected_in_text2query() {
    let ctx = Context::system();
    let err = pipeline_plan(&t2q_registry(), Some(&ctx))
        .expect_err("text2query 档 `$pipeline` 直通必须显式报错");
    assert!(
        err.starts_with(ERR_TEXT2QUERY) && err.contains("$pipeline 直通"),
        "应携带档位哨兵前缀并点明命中项: {err}"
    );
}

#[test]
fn user_pipeline_write_stages_rejected_in_both_profiles() {
    for stage in [
        json!({ "$out": "dest" }),
        json!({ "$merge": { "into": "dest" } }),
    ] {
        let params = params_of(json!({ "p0": [stage.clone()] }));

        // standard 档：档位放行 → 落到阶段安全校验（R3，拒绝写副作用）
        let err = plan_query(PIPELINE_GQL, &params, &standard_registry(), None)
            .expect_err("standard 档 `$out`/`$merge` 写副作用阶段必须拒绝");
        assert!(err.contains("写副作用"), "应点明写副作用拒绝: {err}");

        // text2query 档：档位收缩优先（`$pipeline` 直通整体禁用），亦为显式 Err
        let err = plan_query(
            PIPELINE_GQL,
            &params,
            &t2q_registry(),
            Some(&Context::system()),
        )
        .expect_err("text2query 档 `$pipeline` 整体禁用（含写副作用阶段）");
        assert!(err.starts_with(ERR_TEXT2QUERY), "应携带档位哨兵前缀: {err}");
    }
}

#[test]
fn user_pipeline_with_relation_field_is_rejected() {
    // `$pipeline` 全权模式不做关系下推：GQL 里声明关系 → 显式 Err，绝不静默丢弃
    let reg = registry_with(json!({
        "name": "Post",
        "collection": "posts",
        "timestamps": false,
        "fields": { "title": { "type": "string" } },
        "relations": {
            "comments": { "model": "Comment", "type": "many", "localField": "_id", "foreignField": "postId" }
        },
    }));
    // 关系目标 model 未注册亦不透传（先于关系下推报「不支持」）
    let err = plan_query(
        "Post($pipeline:@p0){ title, comments{ _id } }",
        &params_of(json!({ "p0": [{ "$match": {} }] })),
        &reg,
        None,
    )
    .expect_err("`$pipeline` + 关系字段应显式报错");
    assert!(
        err.contains("$pipeline") && err.contains("关系字段"),
        "应点明 `$pipeline` 不支持关系字段: {err}"
    );
}

// ─── require_context fail-secure 开关（默认关闭 = JS parity） ──

use rust_store_core::command::{
    plan_count, plan_exists, plan_insert, plan_insert_many, plan_mutation, plan_remove,
    plan_update, plan_update_many, plan_upsert, Probe, ERR_NO_CONTEXT,
};

const PLAIN_GQL: &str = "Post{ title }";

fn no_ctx_err(label: &str, result: Result<impl Sized + std::fmt::Debug, String>) {
    let err = result.expect_err(&format!("{label} 在 require_context 下应报错"));
    assert!(
        err.starts_with(ERR_NO_CONTEXT),
        "{label} 应报 ERR_NO_CONTEXT: {err}"
    );
}

#[test]
fn missing_context_allowed_by_default() {
    // 默认 fail-open（与 JS parity）：无 ctx 照常规划
    let reg = registry_with(base_schema(json!(true)));
    plan_query(PLAIN_GQL, &params_of(json!({})), &reg, None)
        .unwrap_or_else(|e| panic!("默认无 ctx 应放行: {e}"));
    assert!(!reg.require_context(), "开关默认关闭");
}

#[test]
fn require_context_blocks_read_paths_without_ctx() {
    let mut reg = registry_with(base_schema(json!(true)));
    reg.set_require_context(true);

    no_ctx_err(
        "plan_query",
        plan_query(PLAIN_GQL, &params_of(json!({})), &reg, None),
    );
    no_ctx_err(
        "plan_query_one",
        plan_query_one(PLAIN_GQL, &params_of(json!({})), &reg, None),
    );
    no_ctx_err(
        "plan_query_with_count",
        plan_query_with_count(PLAIN_GQL, &params_of(json!({})), &reg, None),
    );
    no_ctx_err(
        "plan_federated",
        plan_federated(PLAIN_GQL, &params_of(json!({})), &reg, None, &json!({})),
    );
}

#[test]
fn require_context_blocks_write_paths_without_ctx() {
    let mut reg = registry_with(base_schema(json!(true)));
    reg.set_require_context(true);

    no_ctx_err(
        "plan_insert",
        plan_insert("Post", &reg, None, &json!({ "title": "x" }), 0, "p1", None),
    );
    no_ctx_err(
        "plan_insert_many",
        plan_insert_many("Post", &reg, None, &[json!({ "title": "x" })], 0, &[], None),
    );
    no_ctx_err(
        "plan_mutation",
        plan_mutation("Post", &reg, None, &json!({ "title": "x" }), 0, &[]),
    );
    no_ctx_err(
        "plan_update",
        plan_update(
            "Post",
            &reg,
            None,
            &json!({}),
            &json!({ "title": "y" }),
            &json!({}),
            0,
            Probe::NotProbed,
        ),
    );
    no_ctx_err(
        "plan_update_many",
        plan_update_many("Post", &reg, None, &json!({}), &json!({ "title": "y" }), 0),
    );
    no_ctx_err(
        "plan_remove",
        plan_remove("Post", &reg, None, &json!({}), Probe::NotProbed),
    );
    no_ctx_err(
        "plan_upsert",
        plan_upsert(
            "Post",
            &reg,
            None,
            &json!({}),
            &json!({ "title": "z" }),
            &json!({}),
            0,
            "p2",
        ),
    );
}

#[test]
fn system_context_passes_require_context() {
    // 显式系统上下文 = 内部调用：require_context 下照常放行（读 + 写）
    let mut reg = registry_with(base_schema(json!(true)));
    reg.set_require_context(true);
    let sys = Context::system();

    plan_query(PLAIN_GQL, &params_of(json!({})), &reg, Some(&sys))
        .unwrap_or_else(|e| panic!("系统上下文读应放行: {e}"));
    // 显式提供 _id（D-03：无 idPrefix 的 schema 必须显式给 _id；本测试只验证权限守卫）
    plan_insert(
        "Post",
        &reg,
        Some(&sys),
        &json!({ "_id": "post_x", "title": "x" }),
        0,
        "p1",
        None,
    )
    .unwrap_or_else(|e| panic!("系统上下文写应放行: {e}"));
}

#[test]
fn require_context_switch_is_reversible() {
    let mut reg = registry_with(base_schema(json!(true)));
    reg.set_require_context(true);
    assert!(plan_query(PLAIN_GQL, &params_of(json!({})), &reg, None).is_err());
    reg.set_require_context(false);
    plan_query(PLAIN_GQL, &params_of(json!({})), &reg, None).expect("重新关闭后应放行");
}

// ─── U1~U4 全局 Err（D2）+ 空逻辑组 ──────────────────────────

fn shape_registry() -> Registry {
    let mut reg = Registry::new();
    reg.register(&json!({
        "name": "Course",
        "collection": "courses",
        "timestamps": true,
        "fields": {
            "title": { "type": "string" },
            "tags":  { "type": "array" },
            "meta":  { "type": "object", "fields": {
                "level": { "type": "string" },
                "seo":   { "type": "object", "fields": { "title": { "type": "string" } } }
            } }
        },
        "relations": {
            "lessons":  { "model": "Lesson", "type": "many", "localField": "_id", "foreignField": "courseId" },
            "category": { "model": "Lesson", "type": "one",  "localField": "catId", "foreignField": "_id" },
        },
    }))
    .unwrap();
    reg.register(&json!({
        "name": "Lesson",
        "collection": "lessons",
        "timestamps": true,
        "fields": {
            "name":      { "type": "string" },
            "courseId":  { "type": "string" },
            "tags":      { "type": "array" },
            "meta":      { "type": "object", "fields": {
                "level": { "type": "string" },
                "seo":   { "type": "object", "fields": { "title": { "type": "string" } } }
            } }
        },
        "relations": {},
    }))
    .unwrap();
    reg
}

fn cond_err(gql: &str, params: Value) -> String {
    plan_query(gql, &params_of(params), &shape_registry(), None)
        .expect_err("U1~U4 / 空逻辑组必须显式报错")
}

#[test]
fn u1_array_field_filter_allowed_in_standard() {
    // 双门禁分离（§4.1 #4 → 批 8b）：standard 档放行数组字段整值过滤（JSON 列包含/整体等值）
    plan_query(
        "Course($condition:@c0){ _id }",
        &params_of(json!({ "c0": { "tags": "python" } })),
        &shape_registry(),
        None,
    )
    .expect("standard 档数组字段过滤应放行");
}

#[test]
fn u1_array_field_filter_rejected_in_text2query() {
    let mut reg = shape_registry();
    reg.set_profile(Profile::Text2Query);
    let err = plan_query(
        "Course($condition:@c0){ _id }",
        &params_of(json!({ "c0": { "tags": "python" } })),
        &reg,
        Some(&Context::system()),
    )
    .expect_err("text2query 档 U1 必须显式报错");
    assert!(err.starts_with(ERR_TEXT2QUERY), "应携带档位哨兵前缀: {err}");
    assert!(err.contains("U1"), "应报 U1：{err}");
}

#[test]
fn u2_object_deep_equality_allowed_in_standard() {
    // 双门禁分离（§4.1 #4 → 批 8b）：standard 档放行对象字段整值过滤（JSON 列整值等值，U2）
    plan_query(
        "Course($condition:@c0){ _id }",
        &params_of(json!({ "c0": { "meta": { "level": "beginner" } } })),
        &shape_registry(),
        None,
    )
    .expect("standard 档对象字段整值过滤应放行");
}

#[test]
fn u2_object_deep_equality_rejected_in_text2query() {
    let mut reg = shape_registry();
    reg.set_profile(Profile::Text2Query);
    let err = plan_query(
        "Course($condition:@c0){ _id }",
        &params_of(json!({ "c0": { "meta": { "level": "beginner" } } })),
        &reg,
        Some(&Context::system()),
    )
    .expect_err("text2query 档 U2 必须显式报错");
    assert!(err.starts_with(ERR_TEXT2QUERY), "应携带档位哨兵前缀: {err}");
    assert!(err.contains("U2"), "应报 U2：{err}");
}

#[test]
fn u3_object_dotted_filter_allowed_in_standard() {
    // 双门禁分离（§4.1 #4）：standard 档放行对象点号路径过滤（JSON 列落地）
    let params = params_of(json!({ "c0": { "meta.seo.title": "看Python" } }));
    plan_query(
        "Course($condition:@c0){ _id }",
        &params,
        &shape_registry(),
        None,
    )
    .expect("standard 档对象点号路径过滤应放行");
}

#[test]
fn u3_object_dotted_filter_rejected_in_text2query() {
    let mut reg = shape_registry();
    reg.set_profile(Profile::Text2Query);
    let err = plan_query(
        "Course($condition:@c0){ _id }",
        &params_of(json!({ "c0": { "meta.seo.title": "看Python" } })),
        &reg,
        Some(&Context::system()),
    )
    .expect_err("text2query 档 U3 必须显式报错");
    assert!(err.starts_with(ERR_TEXT2QUERY), "应携带档位哨兵前缀: {err}");
    assert!(err.contains("U3"), "应报 U3：{err}");
}

#[test]
fn u4_object_dotted_sort_allowed_in_standard() {
    // 双门禁分离（§4.1 #4）：standard 档放行对象点号路径排序（JSON 列落地）
    let params = params_of(json!({ "s0": { "meta.level": 1 } }));
    plan_query("Course($sort:@s0){ _id }", &params, &shape_registry(), None)
        .expect("standard 档对象点号路径排序应放行");
}

#[test]
fn u4_object_dotted_sort_rejected_in_text2query() {
    let mut reg = shape_registry();
    reg.set_profile(Profile::Text2Query);
    let err = plan_query(
        "Course($sort:@s0){ _id }",
        &params_of(json!({ "s0": { "meta.level": 1 } })),
        &reg,
        Some(&Context::system()),
    )
    .expect_err("text2query 档 U4 必须显式报错");
    assert!(err.starts_with(ERR_TEXT2QUERY), "应携带档位哨兵前缀: {err}");
    assert!(err.contains("U4"), "应报 U4：{err}");
}

#[test]
fn u1_u4_apply_to_relation_level_too() {
    // 关系级数组过滤同受档位门禁约束：text2query 档一律 Err
    let mut reg = shape_registry();
    reg.set_profile(Profile::Text2Query);
    let err = plan_query(
        "Course{ _id, lessons($condition:@c0){ _id } }",
        &params_of(json!({ "c0": { "tags": "rust" } })),
        &reg,
        Some(&Context::system()),
    )
    .expect_err("关系级数组过滤在 text2query 档应报 U1");
    assert!(err.starts_with(ERR_TEXT2QUERY), "应携带档位哨兵前缀: {err}");
    assert!(err.contains("U1"), "关系级数组过滤应报 U1：{err}");
}

#[test]
fn empty_logical_group_is_error() {
    for group in ["$and", "$or"] {
        let err = cond_err(
            "Course($condition:@c0){ _id }",
            json!({ "c0": { group: [] } }),
        );
        assert!(err.contains("逻辑组为空"), "{group} 空组应报错：{err}");
    }
}

#[test]
fn scalar_filter_and_sort_still_plan() {
    let reg = shape_registry();
    plan_query(
        "Course($condition:@c0,$sort:@s0){ _id, title }",
        &params_of(json!({ "c0": { "title": "x" }, "s0": { "title": 1 } })),
        &reg,
        None,
    )
    .unwrap_or_else(|e| panic!("标量域条件/排序应放行: {e}"));

    // 关系路径排序（R10）不是对象点号排序，不应被 U4 拒绝
    plan_query(
        "Course($sort:@s0){ _id }",
        &params_of(json!({ "s0": { "category.name": 1 } })),
        &reg,
        None,
    )
    .unwrap_or_else(|e| panic!("关系路径排序应放行: {e}"));
}

#[test]
fn u1_error_is_identical_across_plan_paths() {
    // text2query 档：单库 / 带计数 / 联邦三条规划路径同一码（core 规划期统一拒绝 → 文案一致）
    let mut reg = shape_registry();
    reg.set_profile(Profile::Text2Query);
    let ctx = Context::system();
    let params = params_of(json!({ "c0": { "tags": "python" } }));
    let gql = "Course($condition:@c0){ _id }";

    let a = plan_query(gql, &params, &reg, Some(&ctx)).expect_err("plan_query 应报错");
    let b = plan_query_with_count(gql, &params, &reg, Some(&ctx))
        .expect_err("plan_query_with_count 应报错");
    let c = plan_federated(gql, &params, &reg, Some(&ctx), &json!({}))
        .expect_err("plan_federated 应报错");
    assert_eq!(a, b, "plan_query 与 query_with_count 文案须一致");
    assert_eq!(a, c, "plan_query 与 federated 文案须一致");
}

// ─── 关系聚合谓词子级 filter 的按档分流（§4.1 #4 → 8c-1） ────

const REL_PRED_GQL: &str = "Course($condition:@c0){ _id }";

/// 关系聚合谓词参数：`lessons` 关系 + 子级 `$filter` + `$count`
fn rel_pred(filter: Value) -> Map<String, Value> {
    params_of(json!({ "c0": { "lessons": { "$filter": filter, "$count": { "$gt": 1 } } } }))
}

#[test]
fn rel_pred_filter_shape_allowed_in_standard() {
    // 双门禁分离（§4.1 #4 → 8c-1）：standard 档关系聚合谓词的子级 filter 放行
    // 数组字段整值（U1）/ 对象字段整值（U2）/ 对象点号路径（U3）
    for filter in [
        json!({ "tags": "rust" }),
        json!({ "meta": { "level": "beginner" } }),
        json!({ "meta.seo.title": "看Rust" }),
    ] {
        plan_query(
            REL_PRED_GQL,
            &rel_pred(filter.clone()),
            &shape_registry(),
            None,
        )
        .unwrap_or_else(|e| panic!("standard 档关系谓词 filter {filter} 应放行: {e}"));
    }
}

#[test]
fn rel_pred_filter_shape_rejected_in_text2query() {
    // text2query 档：同一门禁收缩 → U1/U2/U3 一律显式 Err
    for (filter, code) in [
        (json!({ "tags": "rust" }), "U1"),
        (json!({ "meta": { "level": "beginner" } }), "U2"),
        (json!({ "meta.seo.title": "看Rust" }), "U3"),
    ] {
        let mut reg = shape_registry();
        reg.set_profile(Profile::Text2Query);
        let err = plan_query(
            REL_PRED_GQL,
            &rel_pred(filter),
            &reg,
            Some(&Context::system()),
        )
        .expect_err("text2query 档关系谓词 filter 必须显式报错");
        assert!(
            err.starts_with(ERR_TEXT2QUERY),
            "应携带档位哨兵前缀: {err}"
        );
        assert!(err.contains(code), "应报 {code}：{err}");
    }
}

#[test]
fn rel_pred_filter_array_index_path_rejected_in_both_profiles() {
    // 数组字段索引路径（`tags.0`）：各后端索引语义不一致 → 两档一律 Err
    for profile in [Profile::Standard, Profile::Text2Query] {
        let mut reg = shape_registry();
        reg.set_profile(profile);
        let err = plan_query(
            REL_PRED_GQL,
            &rel_pred(json!({ "tags.0": "rust" })),
            &reg,
            Some(&Context::system()),
        )
        .expect_err("数组索引路径两档均须 Err");
        assert!(err.contains("索引路径"), "应报索引路径：{err}");
    }
}

// ─── 关系聚合谓词子级 filter 内嵌套关系下推（§4.1 #4 → 8c-2） ──

/// 嵌套关系 schema：Course --lessons--> Lesson --parts--> LessonPart（`parts.subs` 自引用造三级）
fn nested_registry() -> Registry {
    let mut reg = Registry::new();
    reg.register(&json!({
        "name": "Course", "collection": "courses", "timestamps": true,
        "fields": { "title": { "type": "string" } },
        "relations": {
            "lessons": { "model": "Lesson", "type": "many", "localField": "_id", "foreignField": "courseId" }
        }
    }))
    .unwrap();
    reg.register(&json!({
        "name": "Lesson", "collection": "lessons", "timestamps": true,
        "fields": { "name": { "type": "string" }, "courseId": { "type": "string" } },
        "relations": {
            "parts": { "model": "LessonPart", "type": "many", "localField": "_id", "foreignField": "lessonId" }
        }
    }))
    .unwrap();
    reg.register(&json!({
        "name": "LessonPart", "collection": "lesson_parts", "timestamps": true,
        "fields": { "lessonId": { "type": "string" }, "price": { "type": "int" } },
        "relations": {
            "subs": { "model": "LessonPart", "type": "many", "localField": "_id", "foreignField": "lessonId" }
        }
    }))
    .unwrap();
    reg
}

/// 嵌套关系参数：`lessons` 关系 filter 内下钻 `parts.*`
fn nested_pred(filter: Value) -> Map<String, Value> {
    params_of(json!({ "c0": { "lessons": { "$filter": filter, "$count": { "$gt": 1 } } } }))
}

#[test]
fn nested_relation_filter_allowed_in_standard() {
    // 8c-2：standard 档放行一层嵌套关系下钻（标量 / 与标量并列 / 逻辑组内）
    for filter in [
        json!({ "parts.price": { "$gt": 10 } }),
        json!({ "name": "L1", "parts.price": { "$gt": 10 } }),
        json!({ "$and": [ { "parts.price": { "$gt": 10 } }, { "name": "L1" } ] }),
    ] {
        plan_query(
            REL_PRED_GQL,
            &nested_pred(filter.clone()),
            &nested_registry(),
            None,
        )
        .unwrap_or_else(|e| panic!("standard 档嵌套关系 filter {filter} 应放行: {e}"));
    }
}

#[test]
fn nested_relation_stage_shape_mongo() {
    // Mongo 侧：内层 `$lookup`（`as` = `__rn0__parts`）先于本层 `$match`，且嵌套条件经前缀化点号路径
    let plan = plan_query(
        REL_PRED_GQL,
        &nested_pred(json!({ "parts.price": { "$gt": 10 } })),
        &nested_registry(),
        None,
    )
    .expect("standard 档嵌套下钻应放行");
    let pipeline = plan.commands[0]["pipeline"].as_array().unwrap();
    let outer = pipeline
        .iter()
        .find_map(|s| s.get("$lookup"))
        .expect("外层关系谓词 $lookup");
    let inner_stages = outer["pipeline"].as_array().unwrap();
    assert_eq!(
        inner_stages[0]["$lookup"]["as"], "__rn0__parts",
        "内层嵌套 $lookup 应先于本层 $match: {inner_stages:?}"
    );
    let match_doc = inner_stages[1]["$match"].clone();
    assert!(
        match_doc.to_string().contains(r#""__rn0__parts.price""#),
        "嵌套条件须前缀化为点号路径: {match_doc}"
    );
}

#[test]
fn nested_relation_filter_rejected_in_text2query() {
    let mut reg = nested_registry();
    reg.set_profile(Profile::Text2Query);
    let err = plan_query(
        REL_PRED_GQL,
        &nested_pred(json!({ "parts.price": { "$gt": 10 } })),
        &reg,
        Some(&Context::system()),
    )
    .expect_err("text2query 档嵌套关系下钻必须显式报错");
    assert!(err.starts_with(ERR_TEXT2QUERY), "应携带档位哨兵前缀: {err}");
    assert!(err.contains("功能收缩"), "应报功能收缩：{err}");
}

#[test]
fn nested_relation_three_level_path_rejected() {
    let err = plan_query(
        REL_PRED_GQL,
        &nested_pred(json!({ "parts.subs.price": { "$gt": 10 } })),
        &nested_registry(),
        None,
    )
    .expect_err("三级关系路径必须显式报错");
    assert!(err.contains("三级关系路径"), "应报三级关系路径：{err}");
}

#[test]
fn nested_relation_inside_or_rejected() {
    let err = plan_query(
        REL_PRED_GQL,
        &nested_pred(json!({ "$or": [ { "parts.price": { "$gt": 10 } } ] })),
        &nested_registry(),
        None,
    )
    .expect_err("$or 内嵌套关系路径必须显式报错");
    assert!(err.contains("$or"), "应报 $or 限制：{err}");
}

#[test]
fn nested_relation_bare_reference_rejected() {
    let err = plan_query(
        REL_PRED_GQL,
        &nested_pred(json!({ "parts": { "$exists": true } })),
        &nested_registry(),
        None,
    )
    .expect_err("纯关系引用必须显式报错（须写成 关系.字段）");
    assert!(err.contains("关系引用"), "应报关系引用：{err}");
}

// ─── 权限 RBAC（R0）：聚合 / 关系侧信道收口（F2/F3/F6/L1/L6/F5/X1） ─

use rust_store_core::command::{finalize_query, ERR_PERMISSION, ERR_PERM_PREFIX};

/// R0 权限用例 schema：可读关系 teacher/enrollments + 不可读关系 lessons、
/// 不可读字段 salary/teacherId、不可读目标模型 Vault、agg 计算列。
fn rbac_registry() -> Registry {
    let mut reg = Registry::new();
    reg.register(&json!({
        "name": "Course",
        "collection": "courses",
        "timestamps": true,
        "fields": {
            "title": { "type": "string" },
            "status": { "type": "string" },
            "salary": { "type": "int", "read": ["admin"] },
            "teacherId": { "type": "string", "read": ["admin"] }
        },
        "relations": {
            "lessons":     { "model": "Lesson", "type": "many", "localField": "_id", "foreignField": "courseId", "read": ["admin"] },
            "enrollments": { "model": "Enrollment", "type": "many", "localField": "_id", "foreignField": "courseId" },
            "teacher":     { "model": "Teacher", "type": "one", "localField": "teacherId", "foreignField": "_id" },
            "vault":       { "model": "Vault", "type": "one", "localField": "_id", "foreignField": "courseId" }
        },
        "computes": {
            "lessonCount": { "type": "int",   "agg": { "$count": "lessons" } },
            "enrollCount": { "type": "int",   "agg": { "$count": "enrollments" } },
            "avgGrade":    { "type": "float", "agg": { "$avg": "enrollments.grade" } }
        }
    }))
    .unwrap();
    reg.register(&json!({
        "name": "Lesson", "collection": "lessons", "timestamps": true,
        "fields": { "courseId": { "type": "string" }, "name": { "type": "string" }, "duration": { "type": "int", "read": ["admin"] } },
        "relations": {}
    }))
    .unwrap();
    reg.register(&json!({
        "name": "Enrollment", "collection": "enrollments", "timestamps": true,
        "fields": { "courseId": { "type": "string" }, "passed": { "type": "boolean" }, "grade": { "type": "float", "read": ["admin"] } },
        "relations": {}
    }))
    .unwrap();
    reg.register(&json!({
        "name": "Teacher", "collection": "teachers", "timestamps": true,
        "fields": { "name": { "type": "string" } },
        "relations": {}
    }))
    .unwrap();
    reg.register(&json!({
        "name": "Vault", "collection": "vaults", "timestamps": true, "read": ["admin"],
        "fields": { "x": { "type": "string" } },
        "relations": {}
    }))
    .unwrap();
    reg
}

/// 非 admin 的普通用户（可读大多数字段，不可读 read=["admin"] 的字段/关系）
fn editor_ctx() -> Context {
    Context {
        user_id: Some("u1".to_string()),
        roles: Some(vec!["editor".to_string()]),
        ..Default::default()
    }
}

/// 断言结果为带 `ERR_PERMISSION:` 前缀的权限错误
fn perm_err(label: &str, r: Result<impl Sized + std::fmt::Debug, String>) -> String {
    let err = r.expect_err(&format!("{label} 应报权限错误"));
    assert!(
        err.starts_with(ERR_PERM_PREFIX),
        "{label} 应为权限错误: {err}"
    );
    err
}

// ── F2：根级 `$group` by / agg 字段读校验 ──

#[test]
fn group_unreadable_by_field_is_error() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    let err = perm_err(
        "$group by salary",
        plan_query(
            "Course($group:@g0){ salary, n }",
            &params_of(json!({ "g0": { "by": ["salary"], "agg": { "n": { "$count": "*" } } } })),
            &reg,
            Some(&ctx),
        ),
    );
    assert_eq!(err, ERR_PERMISSION, "应返回统一权限码 + 文案");
}

#[test]
fn group_unreadable_agg_field_is_error() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    perm_err(
        "$group agg $sum salary",
        plan_query(
            "Course($group:@g0){ total }",
            &params_of(json!({ "g0": { "agg": { "total": { "$sum": "salary" } } } })),
            &reg,
            Some(&ctx),
        ),
    );
}

#[test]
fn group_having_backed_by_unreadable_agg_is_error() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    perm_err(
        "$group $having 背后的不可读 agg 字段",
        plan_query(
            "Course($group:@g0,$having:@h0){ status, total }",
            &params_of(json!({
                "g0": { "by": ["status"], "agg": { "total": { "$sum": "salary" } } },
                "h0": { "total": { "$gt": 1 } }
            })),
            &reg,
            Some(&ctx),
        ),
    );
}

#[test]
fn group_readable_fields_pass() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    plan_query(
        "Course($group:@g0){ status, n }",
        &params_of(json!({ "g0": { "by": ["status"], "agg": { "n": { "$count": "*" } } } })),
        &reg,
        Some(&ctx),
    )
    .expect("可读分组键 / 计数聚合应放行");
}

// ── 分组后 `$sort` 引用 by 键：须改写为分组 `_id` 路径 ──
//
// `$group` 之后 by 键已改名为 `_id` / `_id.<key>`；若 `$sort` 仍用原 by 键名，
// Mongo 视为作用在不存在的字段上（no-op）→ 与 SQL 侧按分组键排序的结果不一致。

#[test]
fn group_sort_by_by_key_is_rewritten_to_id_path() {
    let mut reg = Registry::new();
    reg.register(&json!({
        "name": "Course",
        "collection": "courses",
        "timestamps": false,
        "fields": { "status": { "type": "string" }, "enrolled": { "type": "int" } },
        "relations": {},
    }))
    .unwrap();

    let sort_of = |plan: rust_store_core::command::QueryPlan| -> Value {
        let stages = plan.commands[0]["pipeline"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        stages
            .iter()
            .find_map(|s| s.get("$sort").cloned())
            .expect("应有 $sort 阶段")
    };

    // 单 by 键：`status` → `_id`
    let plan = plan_query(
        "Course($group:@g0,$sort:@s0){ status, n }",
        &params_of(json!({
            "g0": { "by": ["status"], "agg": { "n": { "$count": "*" } } },
            "s0": { "status": 1 }
        })),
        &reg,
        None,
    )
    .expect("分组 by 键排序应放行");
    assert_eq!(
        sort_of(plan),
        json!({ "_id": 1 }),
        "单 by 键须改写为 `_id`（否则 Mongo 端排序为 no-op）"
    );

    // 多 by 键：`status` → `_id.status`；agg 别名保持顶层
    let plan = plan_query(
        "Course($group:@g0,$sort:@s0){ status, enrolled, n }",
        &params_of(json!({
            "g0": { "by": ["status", "enrolled"], "agg": { "n": { "$count": "*" } } },
            "s0": { "status": 1, "n": -1 }
        })),
        &reg,
        None,
    )
    .expect("多 by 键排序应放行");
    assert_eq!(
        sort_of(plan),
        json!({ "_id.status": 1, "n": -1 }),
        "多 by 键须改写为 `_id.<key>`，agg 别名保持顶层"
    );
}

// ── 步骤 9a：`$group.by` object 点号路径按档分流（standard 放行 / text2query Err） ──
//
// ⚠️ DB 独有能力（仅 MongoDB）：Mongo 原生下钻可执行；SQL 侧由 dialect 显式报
// 「无法把 $group 的 by 键映射到本表标量列」。standard 档放行、text2query 档功能收缩。

fn group_dot_registry() -> Registry {
    registry_with(json!({
        "name": "Course", "collection": "courses", "timestamps": false,
        "fields": {
            "status": { "type": "string" },
            "meta": { "type": "object", "fields": { "level": { "type": "string" } } }
        },
        "relations": {}
    }))
}

#[test]
fn group_by_object_dot_path_allowed_in_standard() {
    let reg = group_dot_registry();
    let plan = plan_query(
        "Course($group:@g0){ status, meta.level, n }",
        &params_of(
            json!({ "g0": { "by": ["status", "meta.level"], "agg": { "n": { "$count": "*" } } } }),
        ),
        &reg,
        None,
    )
    .expect("standard 档对象点号路径分组键应放行（Mongo 源可用）");
    let group = plan.commands[0]["pipeline"]
        .as_array()
        .and_then(|s| s.iter().find_map(|x| x.get("$group").cloned()))
        .expect("应有 $group 阶段");
    assert_eq!(
        group["_id"]["meta"]["level"],
        json!("$meta.level"),
        "点号路径须按嵌套结构还原为 Mongo `$meta.level` 引用"
    );
}

#[test]
fn group_by_object_dot_path_rejected_in_text2query() {
    let mut reg = group_dot_registry();
    reg.set_profile(Profile::Text2Query);
    // text2query 档强制携带用户上下文（ensure_profile_ctx），故须给 ctx 才能走到 by 键校验
    let ctx = Context {
        user_id: Some("u1".to_string()),
        roles: Some(vec!["admin".to_string()]),
        ..Default::default()
    };
    let err = plan_query(
        "Course($group:@g0){ status, meta.level, n }",
        &params_of(
            json!({ "g0": { "by": ["status", "meta.level"], "agg": { "n": { "$count": "*" } } } }),
        ),
        &reg,
        Some(&ctx),
    )
    .expect_err("text2query 档对象点号路径分组键应显式 Err");
    assert!(
        err.starts_with("ERR_TEXT2QUERY:") && err.contains("$group.by object 点号路径"),
        "应为档位拒绝哨兵 + 命中项文案: {err}"
    );
}

// ── F6/L6：计算列 agg 的依赖关系 / 子字段读校验 ──

#[test]
fn compute_agg_unreadable_relation_is_error() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    perm_err(
        "计算列 lessonCount 依赖不可读关系",
        plan_query(
            "Course{ _id, lessonCount }",
            &params_of(json!({})),
            &reg,
            Some(&ctx),
        ),
    );
}

#[test]
fn compute_agg_unreadable_child_field_is_error() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    perm_err(
        "计算列 avgGrade 依赖不可读子字段",
        plan_query(
            "Course{ _id, avgGrade }",
            &params_of(json!({})),
            &reg,
            Some(&ctx),
        ),
    );
}

#[test]
fn compute_agg_readable_dependency_passes() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    plan_query(
        "Course{ _id, enrollCount }",
        &params_of(json!({})),
        &reg,
        Some(&ctx),
    )
    .expect("可读关系 / 子字段的计算列应放行");
}

// ── L1/T2/L6：显式请求的关系可读性（关系 read ∧ 目标 model read）──

#[test]
fn explicit_unreadable_relation_is_error() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    perm_err(
        "显式请求不可读关系 lessons",
        plan_query(
            "Course{ _id, lessons{ name } }",
            &params_of(json!({})),
            &reg,
            Some(&ctx),
        ),
    );
}

#[test]
fn explicit_unreadable_target_model_is_error() {
    // 关系 vault 自身无 read 限制（可读），但目标 model Vault read=["admin"] → Err（T2）
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    perm_err(
        "关系目标 model 不可读",
        plan_query(
            "Course{ _id, vault{ x } }",
            &params_of(json!({})),
            &reg,
            Some(&ctx),
        ),
    );
}

#[test]
fn explicit_readable_relation_passes() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    plan_query(
        "Course{ _id, teacher{ name } }",
        &params_of(json!({})),
        &reg,
        Some(&ctx),
    )
    .expect("可读关系应放行");
}

#[test]
fn relation_permission_error_identical_across_plan_paths() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    let params = params_of(json!({}));
    let gql = "Course{ _id, lessons{ name } }";
    let a = plan_query(gql, &params, &reg, Some(&ctx)).expect_err("单库应报错");
    let b = plan_federated(gql, &params, &reg, Some(&ctx), &json!({})).expect_err("联邦应报错");
    assert_eq!(a, b, "单库与联邦关系权限错误须同码同文案");
    assert_eq!(a, ERR_PERMISSION);
}

// ── F3：§9.6 关系聚合谓词 filter / agg / $of 子字段读校验 ──

#[test]
fn rel_predicate_unreadable_filter_field_is_error() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    perm_err(
        "关系谓词 filter 引用不可读子字段",
        plan_query(
            "Course($condition:@c0){ _id }",
            &params_of(
                json!({ "c0": { "enrollments": { "$filter": { "grade": 1 }, "$count": { "$gt": 1 } } } }),
            ),
            &reg,
            Some(&ctx),
        ),
    );
}

#[test]
fn rel_predicate_unreadable_of_field_is_error() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    perm_err(
        "关系谓词 $of 引用不可读子字段",
        plan_query(
            "Course($condition:@c0){ _id }",
            &params_of(
                json!({ "c0": { "enrollments": { "$count": { "$of": "grade", "$gt": 1 } } } }),
            ),
            &reg,
            Some(&ctx),
        ),
    );
}

#[test]
fn rel_predicate_unreadable_relation_is_error() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    perm_err(
        "关系谓词引用不可读关系",
        plan_query(
            "Course($condition:@c0){ _id }",
            &params_of(json!({ "c0": { "lessons": { "$exists": true } } })),
            &reg,
            Some(&ctx),
        ),
    );
}

#[test]
fn rel_predicate_readable_fields_pass() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    plan_query(
        "Course($condition:@c0){ _id }",
        &params_of(json!({ "c0": { "enrollments": { "$filter": { "passed": true }, "$count": { "$gt": 1 } } } })),
        &reg,
        Some(&ctx),
    )
    .expect("可读子字段的关系谓词应放行");
}

// ── F5：join key 不可读时不得出现在输出 ──

#[test]
fn unreadable_join_key_not_in_output() {
    // teacher.localField = teacherId（read=["admin"]）：内部匹配可用，
    // 但输出投影 / 结果必须裁剪掉它。
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    let plan = plan_query(
        "Course{ _id, teacherId, teacher{ name } }",
        &params_of(json!({})),
        &reg,
        Some(&ctx),
    )
    .expect("可读关系 + 不可读 join key 应可规划");

    let post = plan.to_value()["postprocess"].clone();
    let mut items = vec![json!({
        "_id": "c1", "teacherId": "t1",
        "teacher": { "_id": "t1", "name": "n" }
    })];
    finalize_query(&post, &mut items, &reg, None, Some(&ctx)).expect("后处理应成功");

    assert!(
        items[0].get("teacherId").is_none(),
        "不可读 join key 不应出现在输出: {}",
        items[0]
    );
    assert!(items[0].get("teacher").is_some(), "可读关系应保留");
}

// ── X1：同一 ctx 下，权限裁剪在单库 / 联邦两路径结果一致 ──

#[test]
fn permission_prune_parity_single_vs_federation() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    let params = params_of(json!({}));
    let gql = "Course{ _id, title, salary, teacher{ name } }";
    let docs = || {
        vec![json!({
            "_id": "c1", "title": "t", "salary": 100,
            "teacher": { "_id": "t1", "name": "n" }
        })]
    };

    let single = plan_query(gql, &params, &reg, Some(&ctx)).expect("单库应规划成功");
    let mut a = docs();
    finalize_query(
        &single.to_value()["postprocess"].clone(),
        &mut a,
        &reg,
        None,
        Some(&ctx),
    )
    .expect("单库后处理应成功");

    let fed = plan_federated(gql, &params, &reg, Some(&ctx), &json!({})).expect("联邦应规划成功");
    let mut b = docs();
    finalize_query(&fed["postprocess"].clone(), &mut b, &reg, None, Some(&ctx))
        .expect("联邦后处理应成功");

    assert_eq!(a, b, "同一 ctx 下单库与联邦权限裁剪结果须一致");
    assert!(a[0].get("salary").is_none(), "不可读字段应被裁剪: {}", a[0]);
    assert!(a[0].get("teacher").is_some(), "可读关系应保留");
}

// ── §9.6 × query_with_count：关系聚合谓词无法用标量 countDocuments 表达 ──

#[test]
fn query_with_count_rejects_relation_predicate() {
    let reg = rbac_registry();
    let err = plan_query_with_count(
        "Course($condition:@c0){ _id }",
        &params_of(json!({
            "c0": { "enrollments": { "$filter": { "passed": true }, "$count": { "$gt": 1 } } }
        })),
        &reg,
        None,
    )
    .expect_err("关系聚合谓词 + query_with_count 应显式报错");
    assert!(err.contains("关系聚合谓词"), "错误信息异常: {}", err);
}

#[test]
fn query_with_count_nested_relation_predicate_rejected() {
    let reg = rbac_registry();
    let err = plan_query_with_count(
        "Course($condition:@c0){ _id }",
        &params_of(json!({
            "c0": { "$and": [
                { "title": "x" },
                { "enrollments": { "$count": { "$gt": 1 } } }
            ] }
        })),
        &reg,
        None,
    )
    .expect_err("$and 内嵌关系聚合谓词亦应显式报错");
    assert!(err.contains("关系聚合谓词"), "错误信息异常: {}", err);
}

// ── §11.4 写路径静默点收口（D2：绝不静默） ───────────────────

#[test]
fn write_paths_reject_u1_in_text2query() {
    // text2query 档：写路径数组字段条件一律显式报错（U1）
    let mut reg = shape_registry();
    reg.set_profile(Profile::Text2Query);
    let ctx = Context::system();
    let cond = json!({ "tags": "python" });

    let err = plan_update(
        "Course",
        &reg,
        Some(&ctx),
        &cond,
        &json!({ "title": "x" }),
        &json!({}),
        0,
        Probe::NotProbed,
    )
    .expect_err("plan_update 数组字段条件应显式报错");
    assert!(err.contains("U1"), "plan_update 应报 U1: {err}");

    let err = plan_remove("Course", &reg, Some(&ctx), &cond, Probe::NotProbed)
        .expect_err("plan_remove 数组字段条件应显式报错");
    assert!(err.contains("U1"), "plan_remove 应报 U1: {err}");

    let err = plan_update_many(
        "Course",
        &reg,
        Some(&ctx),
        &cond,
        &json!({ "title": "x" }),
        0,
    )
    .expect_err("plan_update_many 数组字段条件应显式报错");
    assert!(err.contains("U1"), "plan_update_many 应报 U1: {err}");

    let err = plan_upsert(
        "Course",
        &reg,
        Some(&ctx),
        &cond,
        &json!({ "title": "x" }),
        &json!({}),
        0,
        "",
    )
    .expect_err("plan_upsert 数组字段条件应显式报错");
    assert!(err.contains("U1"), "plan_upsert 应报 U1: {err}");
}

#[test]
fn write_paths_allow_u1_in_standard() {
    // 双门禁分离（批 8b）：standard 档数组字段条件在写路径放行（U1 落地为 JSON 列谓词）
    let reg = shape_registry();
    let cond = json!({ "tags": "python" });
    plan_update(
        "Course",
        &reg,
        None,
        &cond,
        &json!({ "title": "x" }),
        &json!({}),
        0,
        Probe::NotProbed,
    )
    .expect("standard 档 plan_update 数组字段条件应放行");
    plan_remove("Course", &reg, None, &cond, Probe::NotProbed)
        .expect("standard 档 plan_remove 数组字段条件应放行");
}

#[test]
fn nor_empty_logical_group_is_error() {
    // `$nor: []` 在 Mongo 侧恒真（静默返回全表），与 `$and:[]` / `$or:[]` 同属空逻辑组 → 必须同码拒绝
    let err = cond_err(
        "Course($condition:@c0){ _id }",
        json!({ "c0": { "$nor": [] } }),
    );
    assert!(err.contains("$nor"), "应报 $nor 逻辑组为空: {err}");
}

#[test]
fn query_with_count_rejects_not_wrapped_relation_predicate() {
    let reg = rbac_registry();
    let err = plan_query_with_count(
        "Course($condition:@c0){ _id }",
        &params_of(json!({
            "c0": { "$not": { "enrollments": { "$count": { "$gt": 1 } } } }
        })),
        &reg,
        None,
    )
    .expect_err("$not 包裹的关系聚合谓词 + query_with_count 应显式报错");
    assert!(err.contains("关系聚合谓词"), "错误信息异常: {}", err);
}

#[test]
fn scalar_count_and_exists_reject_relation_predicate() {
    let reg = rbac_registry();
    let cond = json!({ "enrollments": { "$count": { "$gt": 1 } } });

    let err = plan_count("Course", &reg, Some(&cond), None)
        .expect_err("标量 count 不支持关系聚合谓词（否则静默给出错数）");
    assert!(err.contains("关系聚合谓词"), "count 错误信息异常: {err}");

    let err = plan_exists("Course", &reg, &cond)
        .expect_err("标量 exists 不支持关系聚合谓词（否则静默给出错结果）");
    assert!(err.contains("关系聚合谓词"), "exists 错误信息异常: {err}");
}

#[test]
fn unknown_relation_type_is_error_not_silently_skipped() {
    let mut reg = Registry::new();
    reg.register(&json!({
        "name": "Course",
        "collection": "courses",
        "timestamps": true,
        "fields": { "title": { "type": "string" } },
        "relations": {
            "lessons": { "model": "Lesson", "type": "ref", "localField": "_id", "foreignField": "courseId" }
        },
    }))
    .unwrap();
    reg.register(&json!({
        "name": "Lesson",
        "collection": "lessons",
        "timestamps": true,
        "fields": { "courseId": { "type": "string" } },
        "relations": {},
    }))
    .unwrap();

    let err = plan_mutation(
        "Course",
        &reg,
        None,
        &json!({ "_id": "c1", "title": "x", "lessons": { "name": "y" } }),
        0,
        &[],
    )
    .expect_err("未知 rel_type 应显式报错，不得静默跳过");
    assert!(err.contains("rel_type"), "错误信息异常: {err}");
}

#[test]
fn unreadable_relation_write_is_degraded_not_silent() {
    let (reg, ctx) = (rbac_registry(), editor_ctx());
    let plan = plan_mutation(
        "Course",
        &reg,
        Some(&ctx),
        &json!({ "_id": "c1", "title": "x", "lessons": { "name": "y" } }),
        0,
        &[],
    )
    .expect("规划应成功（不可读关系的数据被跳过，但必须显式声明）");

    let degraded = plan
        .get("degraded")
        .and_then(|d| d.as_array())
        .expect("不可读关系应产生 degraded 声明");
    assert!(
        degraded
            .iter()
            .any(|d| d.get("code").and_then(|c| c.as_str()) == Some("relationSkipped")),
        "应包含 relationSkipped 降级事件: {plan}"
    );
}

// ─── 档位收缩 Err 的稳定前缀统一（Host 按前缀映射 profile_blocked） ──

#[test]
fn t2q_shape_errors_carry_stable_prefix_and_feature_tag() {
    // U1~U4 / 8c-2 收缩 Err 与 `$pipeline` 直通 / 深度超限（见各自用例）同一哨兵前缀
    // `ERR_TEXT2QUERY:`，且带 `[feature]` 方括号标签（Host 正则 `\[(.+?)\]` 提取为
    // `profile_blocked` 告警的 feature 字段，缺括号即 feature 留白）——形锁在此。
    let ctx = Context::system();
    let mut t2q_shape = shape_registry();
    t2q_shape.set_profile(Profile::Text2Query);
    let mut t2q_nested = nested_registry();
    t2q_nested.set_profile(Profile::Text2Query);

    for (gql, params, reg, feature) in [
        (
            "Course($condition:@c0){ _id }",
            params_of(json!({ "c0": { "tags": "python" } })),
            &t2q_shape,
            "[U1 数组字段条件]",
        ),
        (
            "Course($condition:@c0){ _id }",
            params_of(json!({ "c0": { "meta": { "level": "beginner" } } })),
            &t2q_shape,
            "[U2 对象字段条件]",
        ),
        (
            "Course($condition:@c0){ _id }",
            params_of(json!({ "c0": { "meta.seo.title": "看Rust" } })),
            &t2q_shape,
            "[U3 对象点号路径条件]",
        ),
        (
            "Course($sort:@s0){ _id }",
            params_of(json!({ "s0": { "meta.level": 1 } })),
            &t2q_shape,
            "[U4 对象点号路径排序]",
        ),
        (
            REL_PRED_GQL,
            nested_pred(json!({ "parts.price": { "$gt": 10 } })),
            &t2q_nested,
            "[8c-2 嵌套关系路径]",
        ),
    ] {
        let err = plan_query(gql, &params, reg, Some(&ctx))
            .expect_err("text2query 档收缩必须显式 Err");
        assert!(
            err.starts_with(ERR_TEXT2QUERY),
            "收缩 Err 应以哨兵前缀开头: {err}"
        );
        assert!(
            err.contains(feature),
            "应含 [{feature}] 门禁项标签（Host 正则提取依赖）: {err}"
        );
        assert!(err.contains("功能收缩"), "应报功能收缩: {err}");
    }
}
