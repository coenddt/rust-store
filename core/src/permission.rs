//! 权限引擎（对应 JS `src/permission.js`）
//!
//! 与 JS 版的差异：`AsyncLocalStorage` 隐式上下文改为**显式 `Context` 入参**。
//!
//! ## 信任模型（fail-open 契约）
//!
//! `ctx: None` = 「系统内部调用，跳过权限检查」——与 JS 原版 `if (ctx)` 语义对齐，
//! **默认 fail-open**（调用方忘传 ctx 不会报错而是放行）。对安全敏感的宿主应：
//!
//! 1. 开启 [`crate::schema::Registry::set_require_context(true)`]：此后 `ctx` 缺失
//!    在所有 plan 入口显式报错（fail-secure opt-in，默认关闭以保持三端 parity）；
//! 2. 内部调用（索引创建、归档回填等）显式传 [`Context::system`]，
//!    与「忘传 ctx」在语义上彻底分离。

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::schema::Schema;
use crate::types::{is_truthy, str_list};

#[derive(Debug, Clone, Default)]
pub struct Context {
    pub user_id: Option<String>,
    pub roles: Option<Vec<String>>,
    pub role: Option<String>,
    pub internal: bool,
}

impl Context {
    /// 系统内部调用上下文：`internal: true` → 权限引擎全放行、不注入 owner 条件。
    ///
    /// 供 Host 的内部路径（索引创建、归档回填、后台任务等）显式表达「这是系统调用」，
    /// 与 `None`（调用方未传上下文，`require_context` 开启时会报错）区分开。
    pub fn system() -> Self {
        Self {
            user_id: None,
            roles: None,
            role: None,
            internal: true,
        }
    }
}

/// 从 fixture/请求的 JSON 上下文构建 `Context`；null/非对象 → None（等价 JS 的 undefined）
pub fn context_from_value(v: &Value) -> Option<Context> {
    if v.is_null() {
        return None;
    }
    let o = v.as_object()?;
    Some(Context {
        user_id: o.get("userId").and_then(|x| x.as_str()).map(String::from),
        roles: str_list(o.get("roles")),
        role: o.get("role").and_then(|x| x.as_str()).map(String::from),
        internal: o.get("internal").map(is_truthy).unwrap_or(false),
    })
}

/// doc 语义（用于 creator 伪角色）：
/// `Missing` → 新插入（无已有文档，creator 自动通过）
/// `NoResult` → 查询无结果（creator 不通过）
/// `Doc` → 已有文档
#[derive(Debug, Clone, Copy)]
pub enum Doc<'a> {
    Missing,
    NoResult,
    Doc(&'a Value),
}

/// 评估当前用户是否满足指定角色白名单
pub fn evaluate(ctx: Option<&Context>, role_list: Option<&[String]>, doc: Doc) -> bool {
    let empty = role_list.map(|r| r.is_empty()).unwrap_or(true);
    if empty {
        // 无角色白名单 = schema/字段无权限配置 → 按角色取默认行为
        return match ctx {
            None => true,
            Some(c) => {
                if c.internal {
                    true
                } else {
                    !c.roles
                        .clone()
                        .unwrap_or_default()
                        .iter()
                        .any(|r| r == "guest")
                }
            }
        };
    }

    let Some(c) = ctx else { return true };
    if c.internal {
        return true;
    }

    let roles = c.roles.clone().unwrap_or_default();
    // super_admin / admin 自动放行
    if roles.iter().any(|r| r == "super_admin" || r == "admin") {
        return true;
    }

    // ① 角色匹配
    let effective = if roles.is_empty() {
        vec![c.role.clone().unwrap_or_default()]
    } else {
        roles
    };
    // `empty` 已早退，此处 role_list 必为 Some 且非空
    let Some(list) = role_list else { return true };
    if list.iter().any(|r| effective.iter().any(|x| x == r)) {
        return true;
    }

    // ② 创作者匹配
    match_creator(c, doc, list)
}

fn match_creator(ctx: &Context, doc: Doc, role_list: &[String]) -> bool {
    if !role_list.iter().any(|r| r == "creator") {
        return false;
    }
    match doc {
        Doc::Missing => true,
        Doc::NoResult => false,
        Doc::Doc(d) => {
            let uid = ctx.user_id.clone().unwrap_or_default();
            if uid.is_empty() {
                return false;
            }
            d.get("createdBy")
                .and_then(|v| v.as_str())
                .map(|cb| cb == uid)
                .unwrap_or(false)
        }
    }
}

// ─── 字段级过滤（读） ─────────────────────────────────────────

/// ctx 为 None 时返回 None（= 不做裁剪）
pub fn get_readable_fields(schema: &Schema, ctx: Option<&Context>) -> Option<HashSet<String>> {
    let c = ctx?;
    let mut allowed = HashSet::new();
    for (key, field) in &schema.fields {
        match &field.read {
            Some(rl) => {
                if evaluate(Some(c), Some(rl), Doc::Missing) {
                    allowed.insert(key.clone());
                }
            }
            None => {
                allowed.insert(key.clone());
            }
        }
    }
    Some(allowed)
}

pub fn get_readable_computes(schema: &Schema, ctx: Option<&Context>) -> Option<HashSet<String>> {
    let c = ctx?;
    let mut allowed = HashSet::new();
    for (key, comp) in &schema.computes {
        match &comp.read {
            Some(rl) => {
                if evaluate(Some(c), Some(rl), Doc::Missing) {
                    allowed.insert(key.clone());
                }
            }
            None => {
                allowed.insert(key.clone());
            }
        }
    }
    Some(allowed)
}

pub fn get_readable_relations(schema: &Schema, ctx: Option<&Context>) -> Option<HashSet<String>> {
    let c = ctx?;
    let mut allowed = HashSet::new();
    for (key, rel) in &schema.relations {
        match &rel.read {
            Some(rl) => {
                if evaluate(Some(c), Some(rl), Doc::Missing) {
                    allowed.insert(key.clone());
                }
            }
            None => {
                allowed.insert(key.clone());
            }
        }
    }
    Some(allowed)
}

// ─── Schema 级检查 ────────────────────────────────────────────

pub fn can_read_schema(schema: &Schema, ctx: Option<&Context>) -> bool {
    evaluate(ctx, schema.read.as_deref(), Doc::Missing)
}

/// 游客无论 schema.write 如何配置，均无写入权限
pub fn can_write_schema(schema: &Schema, ctx: Option<&Context>) -> bool {
    if let Some(c) = ctx {
        if has_role(c, "guest") {
            return false;
        }
    }
    evaluate(ctx, schema.write.as_deref(), Doc::Missing)
}

fn has_role(ctx: &Context, role: &str) -> bool {
    ctx.roles
        .as_ref()
        .map(|r| r.iter().any(|x| x == role))
        .unwrap_or(false)
}

// ─── 所有者条件注入 ──────────────────────────────────────────

pub fn should_inject_owner_condition(schema: &Schema, ctx: Option<&Context>) -> bool {
    let Some(c) = ctx else { return false };
    // JS `!ctx.userId`：空串同样视为缺席
    if c.user_id.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
        return false;
    }
    if c.internal {
        return false;
    }
    let roles = c.roles.clone().unwrap_or_default();
    if roles.iter().any(|r| r == "super_admin" || r == "admin") {
        return false;
    }
    let effective = if roles.is_empty() {
        vec![c.role.clone().unwrap_or_default()]
    } else {
        roles
    };
    let read = schema.read.clone().unwrap_or_default();
    let real_roles: Vec<&String> = read.iter().filter(|r| *r != "creator").collect();
    if real_roles.iter().any(|r| effective.iter().any(|x| x == *r)) {
        return false;
    }
    read.iter().any(|r| r == "creator")
}

/// 非 admin 用户只看自己的数据：`creator` 专属读权限时注入 `createdBy` 条件
pub fn merge_owner_condition(
    schema: &Schema,
    ctx: Option<&Context>,
    condition: Option<Value>,
) -> Option<Value> {
    if !should_inject_owner_condition(schema, ctx) {
        return condition;
    }
    let owner = json!({ "createdBy": ctx?.user_id });
    match condition.filter(is_truthy) {
        None => Some(owner),
        Some(c) => Some(json!({ "$and": [c, owner] })),
    }
}

// ─── 字段级过滤（写） ─────────────────────────────────────────

pub fn get_writable_fields(schema: &Schema, ctx: Option<&Context>) -> Option<HashSet<String>> {
    let c = ctx?;
    let schema_writable = can_write_schema(schema, ctx);
    let mut allowed = HashSet::new();
    for (key, field) in &schema.fields {
        match &field.write {
            Some(wl) => {
                if evaluate(Some(c), Some(wl), Doc::Missing) {
                    allowed.insert(key.clone());
                }
            }
            None => {
                if schema_writable {
                    allowed.insert(key.clone());
                }
            }
        }
    }
    Some(allowed)
}

/// 过滤写入数据：只保留当前用户可写的字段（点号路径按 root 字段检查）
pub fn filter_writable_data(schema: &Schema, ctx: Option<&Context>, data: &Value) -> Value {
    let Some(writable) = get_writable_fields(schema, ctx) else {
        return data.clone();
    };
    let Some(obj) = data.as_object() else {
        return data.clone();
    };
    let mut result = Map::new();
    for (key, val) in obj {
        let root = key.split('.').next().unwrap_or(key);
        // `_id` 是文档标识而非普通数据字段，豁免写权限过滤（D-03：否则
        // 「显式提供 _id」路径在有权限过滤的上下文中永远不可达）
        if key == "_id" || writable.contains(root) {
            result.insert(key.clone(), val.clone());
        }
    }
    Value::Object(result)
}
