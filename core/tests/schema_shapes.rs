//! 生态真实 schema 形状注册回归网（N4/N5 收紧后的 P0 防护）。
//!
//! 2026-11 N4/N5 把「未知顶层键 / 未知字段键」改为注册期显式报错，但允许清单漏掉了
//! 生态已在使用、core 本应透传的键：`system`（py-store / nodejs-store 内建模型标记，
//! 如 `__workflowRun`）、`description`（store-graphql / store-grpc 文档管道，Schema 与
//! 字段级含嵌套）、`x-*`（协议适配注记 `x-graphql` / `x-grpc` / `x-mcp`）。
//! 漏掉直接导致 `import py_store` 在收集期抛
//! `RuntimeError: schema "__workflowRun" 含未知顶层键 "system"`。
//!
//! 本测试把各仓真实形状原样注册一遍；同时保留负例（拼写错误仍须拒绝），
//! 避免「放宽」退化成「不再校验」。

use serde_json::{json, Value};

use rust_store_core::permission::{Context, RoleRules};
use rust_store_core::schema::Registry;

fn register_all(defs: &[Value]) -> Registry {
    let mut reg = Registry::new();
    for d in defs {
        let name = d
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unnamed>")
            .to_string();
        reg.register(d)
            .unwrap_or_else(|e| panic!("schema {name} 注册被拒: {e}"));
    }
    reg
}

/// py-store / nodejs-store 的内建模型形状（metadef + workflow）。
fn builtin_shapes() -> Vec<Value> {
    vec![
        // py-store workflow.py::_RUN_SCHEMA（node 同形）
        json!({
            "name": "__workflowRun",
            "system": true,
            "collection": "__workflowRun",
            "idPrefix": "wfrun",
            "write": [],
            "fields": {
                "_id": { "type": "string" },
                "workflow": { "type": "string" },
                "status": { "type": "string" },
                "input": { "type": "object" },
                "steps": { "type": "array" },
                "error": { "type": "string" },
                "stepIndex": { "type": "int" },
                "dryRun": { "type": "bool" },
                "now": { "type": "number" }
            }
        }),
        // py-store metadef.py::_def_model（__schemaDef / __workflowDef）
        json!({
            "name": "__schemaDef",
            "system": true,
            "collection": "__schemaDef",
            "idPrefix": "sdef",
            "write": [],
            "fields": {
                "_id": { "type": "string" },
                "tenant": { "type": "string" },
                "env": { "type": "string" },
                "name": { "type": "string" },
                "version": { "type": "int" },
                "defn": { "type": "object" },
                "status": { "type": "string" },
                "createdBy": { "type": "string" }
            }
        }),
        // py-store metadef.py::_FEEDBACK_MODEL（字段名 `type` 合法）
        json!({
            "name": "__feedback",
            "system": true,
            "collection": "__feedback",
            "idPrefix": "fdbk",
            "write": [],
            "fields": {
                "_id": { "type": "string" },
                "type": { "type": "string" },
                "code": { "type": "string" },
                "layer": { "type": "string" },
                "message": { "type": "string" },
                "hint": { "type": "string" },
                "tenant": { "type": "string" },
                "env": { "type": "string" },
                "now": { "type": "number" }
            }
        }),
        // py-store metadef.py::_MIGRATION_MODEL
        json!({
            "name": "__schemaMigration",
            "system": true,
            "collection": "__schemaMigration",
            "idPrefix": "mig",
            "write": [],
            "fields": {
                "_id": { "type": "string" },
                "tenant": { "type": "string" },
                "env": { "type": "string" },
                "backend": { "type": "string" },
                "name": { "type": "string" },
                "hash": { "type": "string" },
                "version": { "type": "int" },
                "appliedAt": { "type": "number" },
                "sqls": { "type": "array" }
            }
        }),
    ]
}

#[test]
fn builtin_models_with_system_flag_register() {
    let reg = register_all(&builtin_shapes());
    assert!(reg.has("__workflowRun"));
    assert!(reg.has("__schemaDef"));
    assert!(reg.has("__feedback"));
    assert!(reg.has("__schemaMigration"));
    // `system` 是正式键：core 解析并保留在 Schema 上（可被宿主 / 工具链读取做分类）
    assert!(
        reg.get("__workflowRun").unwrap().system,
        "内建模型的 system 标记应落到 Schema.system"
    );
}

/// `system` 归档继承 + 类型校验：系统模型的 `<Name>Deleted` 仍标记为系统模型；
/// 非布尔值注册期显式拒绝（禁静默回落为 false）。
#[test]
fn system_flag_is_typed_and_inherited_by_archive() {
    let reg = register_all(&[json!({
        "name": "__sysThing",
        "system": true,
        "timestamps": false,
        "fields": { "_id": { "type": "string" } }
    })]);
    assert!(reg.get("__sysThing").unwrap().system);
    assert!(
        reg.get("__sysThingDeleted").unwrap().system,
        "归档派生应继承 system 标记"
    );
    // 业务模型缺省 false
    let reg2 = register_all(&[json!({ "name": "Biz", "fields": { "a": { "type": "string" } } })]);
    assert!(!reg2.get("Biz").unwrap().system);

    let mut bad = Registry::new();
    let err = bad
        .register(&json!({ "name": "Bad3", "system": "yes", "fields": {} }))
        .expect_err("非布尔 system 应拒绝");
    assert!(err.contains("system 必须为布尔值"), "实际: {err}");
}

/// `system` 只是「内建模型」声明标记：不授予 internal，也不改写读写判定
/// （与同结构、同 `write: []` 的普通模型同判据；空名单 = 不限制，恢复 5.x 旧义）。
#[test]
fn system_flag_does_not_alter_permission_semantics() {
    let reg = register_all(&builtin_shapes());
    let schema = reg.get("__workflowRun").expect("应注册成功");
    let normal = Context {
        user_id: Some("u1".to_string()),
        ..Default::default()
    };
    // 对照：同定义去掉 `system` 标记
    let mut plain_def = builtin_shapes()[0].clone();
    plain_def["name"] = json!("__workflowRunPlain");
    plain_def.as_object_mut().unwrap().remove("system");
    let plain = register_all(&[plain_def]);
    let plain_schema = plain.get("__workflowRunPlain").expect("对照模型应注册成功");
    assert_eq!(
        rust_store_core::permission::can_write_schema(&RoleRules::default(), schema, Some(&normal)),
        rust_store_core::permission::can_write_schema(
            &RoleRules::default(),
            plain_schema,
            Some(&normal)
        ),
        "system 标记不得改变写判定"
    );
    assert_eq!(
        rust_store_core::permission::can_read_schema(&RoleRules::default(), schema, Some(&normal)),
        rust_store_core::permission::can_read_schema(
            &RoleRules::default(),
            plain_schema,
            Some(&normal)
        ),
        "system 标记不得改变读判定"
    );
}

/// 归档模型：真实业务 schema（含 timestamps / archive 开关）+ 自动派生 `<Name>Deleted`。
#[test]
fn archive_model_shape_registers_and_inherits_owner() {
    let reg = register_all(&[json!({
        "name": "Order",
        "collection": "orders",
        "idPrefix": "ord",
        "timestamps": true,
        "archive": true,
        "fields": {
            "_id": { "type": "string" },
            "title": { "type": "string" },
            "amount": { "type": "float" },
            "createdBy": { "type": "string" }
        }
    })]);
    assert!(reg.has("Order"));
    let deleted = reg.get("OrderDeleted").expect("归档模型应自动派生");
    assert!(
        deleted.fields.contains_key("createdBy"),
        "归档模型须继承归属键（N2）"
    );
    assert!(deleted.fields.contains_key("deletedAt"));
}

/// meta-store 定义形状（发布 API / 测试同形）。
#[test]
fn meta_definition_shape_registers() {
    let reg = register_all(&[json!({
        "name": "Order",
        "fields": {
            "_id": { "type": "string" },
            "title": { "type": "string" },
            "price": { "type": "number" }
        }
    })]);
    assert!(reg.has("Order"));
}

/// store-graphql / store-grpc / store-mcp 的注记与文档管道形状：
/// 顶层 `description`、字段级 `description`（含嵌套）、`x-*` 注记。
#[test]
fn adapter_annotation_and_description_shapes_register() {
    let reg = register_all(&[
        // store-graphql smoke（spec/05）
        json!({
            "name": "User",
            "description": "用户表：平台账号主档",
            "x-graphql": { "readonly": true },
            "fields": {
                "_id": { "type": "string", "description": "主键，u 前缀" },
                "name": { "type": "string" },
                "age": { "type": "int" },
                "profile": {
                    "type": "object",
                    "description": "个人资料",
                    "fields": { "bio": { "type": "string" } }
                }
            }
        }),
        // store-grpc（spec/01）
        json!({
            "name": "AuditEvent",
            "x-grpc": { "readonly": true },
            "fields": {
                "_id": { "type": "string" },
                "name": { "type": "string" }
            }
        }),
        // store-mcp（spec/01：模型级 x-mcp.hidden）
        json!({
            "name": "SecretHidden",
            "x-mcp": { "hidden": true },
            "fields": { "s": { "type": "string" } }
        }),
        // store-api rust 用例形状
        json!({
            "name": "user",
            "collection": "user",
            "idPrefix": "u",
            "fields": {
                "name": { "type": "string" },
                "age": { "type": "number" }
            }
        }),
    ]);
    assert!(reg.has("User"));
    assert!(reg.has("AuditEvent"));
    assert!(reg.has("SecretHidden"));
}

/// 负例：放宽后仍须拒绝真正拼错的键，不得退化成「不再校验」。
#[test]
fn typo_keys_are_still_rejected() {
    let mut reg = Registry::new();
    let err = reg
        .register(&json!({
            "name": "Bad",
            "ownerkey": "createdBy",
            "fields": { "a": { "type": "string" } }
        }))
        .expect_err("顶层拼写错误仍须拒绝");
    assert!(err.contains("未知顶层键"), "实际: {err}");

    let mut reg2 = Registry::new();
    let err2 = reg2
        .register(&json!({
            "name": "Bad2",
            "fields": { "a": { "type": "string", "descriptio": "typo" } }
        }))
        .expect_err("字段级拼写错误仍须拒绝");
    assert!(err2.contains("未知属性"), "实际: {err2}");
}
