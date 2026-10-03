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

/// schema 白名单缺失/为空时的默认姿态（设计 §11.2）。
/// Open = 现状放行语义（guest 读拒已随清单化移除）；Closed = fail-secure（未配置模型读写全拒）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnconfiguredPolicy {
    #[default]
    Open,
    Closed,
}

impl UnconfiguredPolicy {
    /// 字符串 → 姿态；未知值 **Err**（禁静默回落 Open，对齐 `Profile::from_str_or_err`）
    pub fn from_str_or_err(s: &str) -> Result<Self, String> {
        match s {
            "open" => Ok(UnconfiguredPolicy::Open),
            "closed" => Ok(UnconfiguredPolicy::Closed),
            other => Err(format!(
                "未知 unconfigured_policy: {other}（仅 open / closed）"
            )),
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            UnconfiguredPolicy::Open => "open",
            UnconfiguredPolicy::Closed => "closed",
        }
    }
}

/// 角色清单与未配置姿态（判决的全部用户配置输入，无任何隐藏项——设计 §11.3）。
/// 默认值即「无豁免 / 无拒写 / Open」：清单化后判决 = 白名单 ∧ RBAC 策略 ∧ 本规则。
#[derive(Debug, Clone, Default)]
pub struct RoleRules {
    /// 豁免角色清单：命中者在一切判决环节（静态 + RBAC）直接放行。默认空。
    pub exempt_roles: Vec<String>,
    /// 拒写角色清单：命中者一切写路径拒绝（读不受影响）。默认空。
    pub deny_write_roles: Vec<String>,
    /// 未配置姿态。默认 Open。
    pub unconfigured: UnconfiguredPolicy,
}

/// 定义层（register / 覆盖）门禁策略（分步 03）。
///
/// - `closed = false`（缺省）：**Open** —— 全放行，保持既有 parity（harness / 测试零变更）；
/// - `closed = true`：**Closed** —— 仅 `internal` 或 `roles` 白名单命中者可注册/覆盖。
#[derive(Debug, Clone, Default)]
pub struct MetaPolicy {
    /// 是否启用门禁（false = Open，缺省）。
    pub closed: bool,
    /// Closed 时的授权角色白名单（空 = 仅 internal 可注册）。
    pub roles: Vec<String>,
}

/// 定义层判决：Open 全放行；Closed 仅 `internal` 或白名单角色（有效角色集，与清单语义一致）。
///
/// `ctx = None` 在 Closed 下**拒绝**（fail-secure）；拒绝由调用点（`register_with_ctx`）
/// 转为显式 `ERR_PERMISSION:` 错误，不静默放行。
pub fn can_register(policy: &MetaPolicy, ctx: Option<&Context>) -> bool {
    if !policy.closed {
        return true;
    }
    match ctx {
        None => false,
        Some(c) => c.internal || has_any(c, &policy.roles),
    }
}

/// 评估当前用户是否满足指定角色白名单。
///
/// 判决输入全部来自 `rules`（用户配置，默认 []/[]/Open），无任何内置角色硬编码：
/// - 豁免：有效角色 ∩ `rules.exempt_roles` ≠ ∅ → true（默认空 = 无豁免）。豁免检查
///   **先于**未配置姿态——设计 §11.3「一切判决环节直通」：`set_exempt_roles` 与
///   `Closed` 并存时豁免仍生效（豁免是显式信任声明，姿态管不住它）；
/// - 未配置（role_list 缺失/空）：`Open` 放行（guest 读拒已随清单化移除——迁移差异，
///   设计 §11.4）；`Closed` 对非内部用户全拒（fail-secure）；ctx=None 与 internal
///   的直通不受姿态影响（无 ctx 维度归 `require_context` 管，设计 §11.6）。
pub fn evaluate(
    rules: &RoleRules,
    ctx: Option<&Context>,
    role_list: Option<&[String]>,
    doc: Doc,
) -> bool {
    let Some(c) = ctx else { return true };
    if c.internal {
        return true;
    }
    // 豁免清单（默认空——无隐形豁免；清单化语义见设计 §11.3）
    if has_any(c, &rules.exempt_roles) {
        return true;
    }
    let empty = role_list.map(|r| r.is_empty()).unwrap_or(true);
    if empty {
        // 未配置姿态显式化（原 empty 分支的 guest 读拒随清单化移除）
        return rules.unconfigured == UnconfiguredPolicy::Open;
    }

    // ① 角色匹配
    let effective = effective(c);
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
pub fn get_readable_fields(
    rules: &RoleRules,
    schema: &Schema,
    ctx: Option<&Context>,
) -> Option<HashSet<String>> {
    let c = ctx?;
    let mut allowed = HashSet::new();
    for (key, field) in &schema.fields {
        match &field.read {
            Some(rl) => {
                if evaluate(rules, Some(c), Some(rl), Doc::Missing) {
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

pub fn get_readable_computes(
    rules: &RoleRules,
    schema: &Schema,
    ctx: Option<&Context>,
) -> Option<HashSet<String>> {
    let c = ctx?;
    let mut allowed = HashSet::new();
    for (key, comp) in &schema.computes {
        match &comp.read {
            Some(rl) => {
                if evaluate(rules, Some(c), Some(rl), Doc::Missing) {
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

pub fn get_readable_relations(
    rules: &RoleRules,
    schema: &Schema,
    ctx: Option<&Context>,
) -> Option<HashSet<String>> {
    let c = ctx?;
    let mut allowed = HashSet::new();
    for (key, rel) in &schema.relations {
        match &rel.read {
            Some(rl) => {
                if evaluate(rules, Some(c), Some(rl), Doc::Missing) {
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

/// 单个字段读权限判定（点号路径按 root 字段判定）。
///
/// - `ctx = None` → 放行（fail-open，与默认姿态一致）；
/// - 字段未在 schema `fields` 中声明 → 放行（无 `field.read` 配置可依）；
/// - 字段在 schema 中且配了 `read` 白名单 → 按 [`get_readable_fields`] 判定。
pub fn is_field_readable(
    rules: &RoleRules,
    schema: &Schema,
    ctx: Option<&Context>,
    field: &str,
) -> bool {
    let root = field.split('.').next().unwrap_or(field);
    if !schema.fields.contains_key(root) {
        return true;
    }
    match get_readable_fields(rules, schema, ctx) {
        None => true,
        Some(allowed) => allowed.contains(root),
    }
}

/// 单个关系读权限判定（R6/L1）。`ctx = None` → 放行（fail-open）。
pub fn is_relation_readable(
    rules: &RoleRules,
    schema: &Schema,
    ctx: Option<&Context>,
    rel: &str,
) -> bool {
    match get_readable_relations(rules, schema, ctx) {
        None => true,
        Some(allowed) => allowed.contains(rel),
    }
}

// ─── Schema 级检查 ────────────────────────────────────────────

pub fn can_read_schema(rules: &RoleRules, schema: &Schema, ctx: Option<&Context>) -> bool {
    evaluate(rules, ctx, schema.read.as_deref(), Doc::Missing)
}

/// 拒写清单命中者一切写路径拒绝（读不受影响，默认空——无拒写）；
/// R2 语义保留：write 显式配置为空白名单（`[]`）= 未声明任何可写角色 → 拒绝一切写，
/// 豁免清单命中者与 internal 例外。
pub fn can_write_schema(rules: &RoleRules, schema: &Schema, ctx: Option<&Context>) -> bool {
    if let Some(c) = ctx {
        if has_any(c, &rules.deny_write_roles) {
            return false;
        }
        if !c.internal && matches!(schema.write, Some(ref w) if w.is_empty()) {
            if !has_any(c, &rules.exempt_roles) {
                return false;
            }
        }
    }
    evaluate(rules, ctx, schema.write.as_deref(), Doc::Missing)
}

fn has_role(ctx: &Context, role: &str) -> bool {
    ctx.roles
        .as_ref()
        .map(|r| r.iter().any(|x| x == role))
        .unwrap_or(false)
}

/// 有效角色集：`roles` 非空用之，否则回落单 `role`——对齐 `rbac::effective_roles`
/// （rbac.rs）与白名单匹配语义。清单命中一律用本函数（不再存在
/// 「roles 为空则清单失明」的隐形差异）。
fn effective(c: &Context) -> Vec<String> {
    match &c.roles {
        Some(r) if !r.is_empty() => r.clone(),
        _ => vec![c.role.clone().unwrap_or_default()],
    }
}

/// 有效角色集与清单是否有交集
fn has_any(c: &Context, list: &[String]) -> bool {
    effective(c).iter().any(|r| list.iter().any(|x| x == r))
}

/// [`has_any`] 的 crate 内公开包装：command 层写路径复用，避免各调用点手写 effective 归一
pub(crate) fn has_any_role(c: &Context, list: &[String]) -> bool {
    has_any(c, list)
}

// ─── 所有者条件注入 ──────────────────────────────────────────

pub fn should_inject_owner_condition(
    rules: &RoleRules,
    schema: &Schema,
    ctx: Option<&Context>,
) -> bool {
    let Some(c) = ctx else { return false };
    // JS `!ctx.userId`：空串同样视为缺席
    if c.user_id.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
        return false;
    }
    if c.internal {
        return false;
    }
    if has_any(c, &rules.exempt_roles) {
        return false;
    }
    let effective = effective(c);
    let read = schema.read.clone().unwrap_or_default();
    let real_roles: Vec<&String> = read.iter().filter(|r| *r != "creator").collect();
    if real_roles.iter().any(|r| effective.iter().any(|x| x == *r)) {
        return false;
    }
    read.iter().any(|r| r == "creator")
}

/// 非 admin 用户只看自己的数据：`creator` 专属读权限时注入 `createdBy` 条件
pub fn merge_owner_condition(
    rules: &RoleRules,
    schema: &Schema,
    ctx: Option<&Context>,
    condition: Option<Value>,
) -> Option<Value> {
    if !should_inject_owner_condition(rules, schema, ctx) {
        return condition;
    }
    let owner = json!({ "createdBy": ctx?.user_id });
    match condition.filter(is_truthy) {
        None => Some(owner),
        Some(c) => Some(json!({ "$and": [c, owner] })),
    }
}

// ─── 字段级过滤（写） ─────────────────────────────────────────

pub fn get_writable_fields(
    rules: &RoleRules,
    schema: &Schema,
    ctx: Option<&Context>,
) -> Option<HashSet<String>> {
    let c = ctx?;
    let schema_writable = can_write_schema(rules, schema, ctx);
    let mut allowed = HashSet::new();
    for (key, field) in &schema.fields {
        match &field.write {
            Some(wl) => {
                if evaluate(rules, Some(c), Some(wl), Doc::Missing) {
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
pub fn filter_writable_data(
    rules: &RoleRules,
    schema: &Schema,
    ctx: Option<&Context>,
    data: &Value,
) -> Value {
    let Some(writable) = get_writable_fields(rules, schema, ctx) else {
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
