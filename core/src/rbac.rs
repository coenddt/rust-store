//! RBAC 动态策略引擎（core 层判决插件；静态白名单引擎见 [`crate::permission`]）
//!
//! 与静态引擎的叠加语义（deny-wins）：RBAC 判决叠加在静态判决**之后**，任一拒绝即
//! 拒绝；字段维度取交集；行条件与静态 owner 条件 `$and`。策略未注入（`registry.rbac()`
//! 为 `None`）时全部原语直通，行为与现状逐位一致——增量收紧，永不放大既有权限面。
//!
//! 两个容易搞反的维度：
//! - grant 之间（同一用户多角色 / 多 grant）：**OR**（任一 grant 授予即放行其
//!   条件命中的行 / 并集字段）——授权是并集；
//! - 引擎之间（静态白名单 × RBAC）：**AND**（deny-wins）——收紧取交集。
//!
//! 行为契约（与 [`crate::permission`] 一致，勿破坏）：
//! - `ctx: None` / `internal` / `super_admin` / `admin` → RBAC 不介入（直通）；
//! - 拒绝一律携带 `ERR_PERMISSION:` 稳定前缀（宿主按前缀映射 PermissionError），
//!   二级标识 `RBAC:` 供日志与测试区分。

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::permission::{get_writable_fields, Context};
use crate::schema::{Registry, Schema};
use crate::types::is_truthy;

/// 策略模式：
/// - [`RbacMode::Overlay`]（默认）：存在匹配 grant 才生效（按 model 增量收紧）；
/// - [`RbacMode::Enforce`]：受管角色（出现在 `roles` 声明中）对**所有** model 生效，
///   无匹配 grant 即拒绝（default deny）；未受管角色不介入。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RbacMode {
    #[default]
    Overlay,
    Enforce,
}

/// 写动作粒度（对齐静态引擎 `check_write_perm` 的 `ERR_NO_WRITE` / `ERR_NO_DELETE` 分派）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteAction {
    Insert,
    Update,
    Remove,
}

impl WriteAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            WriteAction::Insert => "insert",
            WriteAction::Update => "update",
            WriteAction::Remove => "remove",
        }
    }
}

/// action 字符串 → [`WriteAction`]；非法值显式 Err（禁静默回落）
pub fn write_action_from_str(s: &str) -> Result<WriteAction, String> {
    match s {
        "insert" => Ok(WriteAction::Insert),
        "update" => Ok(WriteAction::Update),
        "remove" => Ok(WriteAction::Remove),
        other => Err(format!(
            "RBAC action \"{other}\" 非法（仅支持 read / insert / update / remove）"
        )),
    }
}

#[derive(Debug, Clone)]
pub struct Grant {
    role: String,
    model: String, // 精确 schema 名或 "*"（通配）
    actions: Vec<String>, // 归一化后（"write" 已展开），判定用 contains
    read_fields: Option<HashSet<String>>,
    write_fields: Option<HashSet<String>>,
    owner_only: bool,
    condition: Option<Value>, // 仅等值标量键值（解析期校验）
}

#[derive(Debug, Clone, Default)]
pub struct RbacPolicy {
    mode: RbacMode,
    managed_roles: HashSet<String>,
    grants: Vec<Grant>,
}

/// 单模型判决视图（`decide` 产出）
#[derive(Debug, Clone)]
pub struct ModelDecision {
    pub allowed_actions: HashSet<String>, // read / insert / update / remove
    /// None = 字段维度不收紧（与 [`crate::permission`] 的 Option 语义一致）
    pub read_fields: Option<HashSet<String>>,
    pub write_fields: Option<HashSet<String>>,
}

/// 拒绝哨兵：复用 `ERR_PERMISSION:` 前缀（宿主零改动映射 403），二级标识 `RBAC:`
pub fn deny_msg(action: &str, model: &str) -> String {
    format!("ERR_PERMISSION:RBAC:角色对 [{model}] 无 [{action}] 权限")
}

// ─── 解析（fail-fast，禁静默吞） ─────────────────────────────

impl RbacPolicy {
    /// 由策略 JSON 构建；任何非法形态显式 Err，绝不静默回落
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let obj = v
            .as_object()
            .ok_or_else(|| "RBAC 策略必须是对象".to_string())?;

        let mode = match obj.get("mode") {
            None | Some(Value::Null) => RbacMode::Overlay,
            Some(Value::String(s)) if s == "overlay" => RbacMode::Overlay,
            Some(Value::String(s)) if s == "enforce" => RbacMode::Enforce,
            Some(other) => {
                return Err(format!(
                    "RBAC mode {other} 非法（仅支持 overlay / enforce）"
                ))
            }
        };

        let mut managed_roles = HashSet::new();
        match obj.get("roles") {
            None | Some(Value::Null) => {}
            Some(Value::Object(rm)) => {
                for (name, _) in rm {
                    if name.is_empty() {
                        return Err("RBAC roles 含空角色名".to_string());
                    }
                    managed_roles.insert(name.clone());
                }
            }
            Some(other) => return Err(format!("RBAC roles 必须是对象，实际 {other}")),
        }
        if mode == RbacMode::Enforce && managed_roles.is_empty() {
            return Err("RBAC mode=enforce 时 roles 不能为空（无受管角色 = 策略空转，属配置错误）".to_string());
        }

        let mut grants = Vec::new();
        match obj.get("grants") {
            None | Some(Value::Null) => {}
            Some(Value::Array(gs)) => {
                for (i, g) in gs.iter().enumerate() {
                    grants.push(parse_grant(i, g)?);
                }
            }
            Some(other) => return Err(format!("RBAC grants 必须是数组，实际 {other}")),
        }

        Ok(Self { mode, managed_roles, grants })
    }
}

fn parse_grant(idx: usize, v: &Value) -> Result<Grant, String> {
    let o = v
        .as_object()
        .ok_or_else(|| format!("RBAC grants[{idx}] 必须是对象"))?;
    let get_str = |k: &str| -> Result<String, String> {
        o.get(k)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .ok_or_else(|| format!("RBAC grants[{idx}].{k} 必须是非空字符串"))
    };
    let role = get_str("role")?;
    let model = get_str("model")?;

    let actions_raw = o
        .get("actions")
        .and_then(|x| x.as_array())
        .ok_or_else(|| format!("RBAC grants[{idx}].actions 必须是非空数组"))?;
    if actions_raw.is_empty() {
        return Err(format!("RBAC grants[{idx}].actions 不能为空（不授权请移除该 grant）"));
    }
    let mut actions: Vec<String> = Vec::new();
    for a in actions_raw {
        let s = a
            .as_str()
            .ok_or_else(|| format!("RBAC grants[{idx}].actions 含非字符串项 {a}"))?;
        let expanded: &[&str] = match s {
            "read" => &["read"],
            "insert" => &["insert"],
            "update" => &["update"],
            "remove" => &["remove"],
            "write" => &["insert", "update", "remove"],
            other => {
                return Err(format!(
                    "RBAC grants[{idx}].actions 含非法值 \"{other}\"（仅支持 read/insert/update/remove/write）"
                ))
            }
        };
        for e in expanded {
            if !actions.iter().any(|x| x == e) {
                actions.push((*e).to_string());
            }
        }
    }

    let parse_fields = |k: &str| -> Result<Option<HashSet<String>>, String> {
        match o.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(fs)) => {
                if fs.is_empty() {
                    return Err(format!(
                        "RBAC grants[{idx}].{k} 不能为空数组（全禁请移除该 grant）"
                    ));
                }
                let mut set = HashSet::new();
                for f in fs {
                    let s = f
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| format!("RBAC grants[{idx}].{k} 含非法项 {f}（须非空字符串）"))?;
                    set.insert(s.to_string());
                }
                Ok(Some(set))
            }
            Some(other) => Err(format!("RBAC grants[{idx}].{k} 必须是字符串数组，实际 {other}")),
        }
    };
    let read_fields = parse_fields("readFields")?;
    let write_fields = parse_fields("writeFields")?;

    let owner_only = o.get("ownerOnly").map(is_truthy).unwrap_or(false);

    // 第一批仅等值标量条件：键非空且不得以 "$" 开头（操作符/逻辑组禁用），
    // 值必须是标量——超出形态显式 Err，禁运行时静默失配
    let condition = match o.get("condition") {
        None | Some(Value::Null) => None,
        Some(cm @ Value::Object(_)) => {
            let entries = cm
                .as_object()
                .expect("已按 Object 匹配，此处必为对象");
            for (k, val) in entries {
                if k.is_empty() || k.starts_with('$') {
                    return Err(format!(
                        "RBAC grants[{idx}].condition 键 \"{k}\" 非法（第一批仅支持等值字段条件，键不得以 $ 开头）"
                    ));
                }
                if !val.is_boolean() && !val.is_number() && !val.is_string() && !val.is_null() {
                    return Err(format!(
                        "RBAC grants[{idx}].condition[\"{k}\"] 必须是标量（string/number/bool/null），实际 {val}"
                    ));
                }
            }
            Some(cm.clone())
        }
        Some(other) => {
            return Err(format!(
                "RBAC grants[{idx}].condition 必须是对象（等值条件），实际 {other}"
            ))
        }
    };

    Ok(Grant { role, model, actions, read_fields, write_fields, owner_only, condition })
}

// ─── 判决 ────────────────────────────────────────────────────

/// 有效角色集（对齐 `permission::evaluate`：`roles` 为空回落单 `role`）
fn effective_roles(c: &Context) -> Vec<String> {
    match &c.roles {
        Some(r) if !r.is_empty() => r.clone(),
        _ => vec![c.role.clone().unwrap_or_default()],
    }
}

/// 核心判决：`None` = RBAC 不介入（无 ctx / internal / 豁免角色 / overlay 未覆盖 /
/// enforce 未受管）。enforce 受管但 matched 为空 → `Some`（全 deny），这是
/// default deny 的承载点。
pub fn decide(policy: &RbacPolicy, ctx: Option<&Context>, model: &str) -> Option<ModelDecision> {
    let c = ctx?;
    if c.internal {
        return None;
    }
    let roles = effective_roles(c);
    if roles.iter().any(|r| r == "super_admin" || r == "admin") {
        return None;
    }

    let matched: Vec<&Grant> = policy
        .grants
        .iter()
        .filter(|g| roles.iter().any(|r| r == &g.role) && (g.model == model || g.model == "*"))
        .collect();

    match policy.mode {
        RbacMode::Overlay if matched.is_empty() => return None,
        RbacMode::Enforce => {
            let managed = roles.iter().any(|r| policy.managed_roles.contains(r));
            if !managed {
                return None;
            }
        }
        _ => {}
    }

    // 字段集：grant 间 OR 语义——任一允许该动作的 grant 未声明字段集 = 授予全字段
    // （整体不收紧）；全部声明了才取并集收紧
    let mut allowed_actions = HashSet::new();
    let mut read_all = false;
    let mut read_union: HashSet<String> = HashSet::new();
    let mut write_all = false;
    let mut write_union: HashSet<String> = HashSet::new();
    for g in &matched {
        for a in &g.actions {
            allowed_actions.insert(a.clone());
        }
        if g.actions.iter().any(|a| a == "read") {
            match &g.read_fields {
                None => read_all = true,
                Some(s) => read_union.extend(s.iter().cloned()),
            }
        }
        if g
            .actions
            .iter()
            .any(|a| a == "insert" || a == "update" || a == "remove")
        {
            match &g.write_fields {
                None => write_all = true,
                Some(s) => write_union.extend(s.iter().cloned()),
            }
        }
    }
    let read_fields = if read_all || read_union.is_empty() { None } else { Some(read_union) };
    let write_fields = if write_all || write_union.is_empty() { None } else { Some(write_union) };
    Some(ModelDecision { allowed_actions, read_fields, write_fields })
}

// ─── 叠加原语（registry.rbac() == None 时全部直通） ──────────

/// 表级读判定（叠加在 `can_read_schema` 之后）
pub fn ensure_read(
    registry: &Registry,
    schema: &Schema,
    ctx: Option<&Context>,
) -> Result<(), String> {
    let Some(p) = registry.rbac() else { return Ok(()) };
    match decide(p, ctx, &schema.name) {
        None => Ok(()),
        Some(d) if d.allowed_actions.contains("read") => Ok(()),
        Some(_) => Err(deny_msg("read", &schema.name)),
    }
}

/// 表级写判定（叠加在 `can_write_schema` 之后；upsert 路径裁定按 Insert 判）
pub fn ensure_write(
    registry: &Registry,
    schema: &Schema,
    ctx: Option<&Context>,
    action: WriteAction,
) -> Result<(), String> {
    let Some(p) = registry.rbac() else { return Ok(()) };
    match decide(p, ctx, &schema.name) {
        None => Ok(()),
        Some(d) if d.allowed_actions.contains(action.as_str()) => Ok(()),
        Some(_) => Err(deny_msg(action.as_str(), &schema.name)),
    }
}

/// 探针重入判决（单条 update/remove 的 ownerOnly / condition 命中判定）。
///
/// grant 间 OR 语义：任一允许该动作的 grant 无行限制 → 放行；有行限制且命中
/// doc → 放行；全部不命中 → 拒绝（杜绝静默 0 行，对齐 no-error-masking）。
pub fn ensure_write_on_doc(
    registry: &Registry,
    schema: &Schema,
    ctx: Option<&Context>,
    action: WriteAction,
    doc: &Value,
) -> Result<(), String> {
    let Some(p) = registry.rbac() else { return Ok(()) };
    let Some(c) = ctx else { return Ok(()) };
    if c.internal {
        return Ok(());
    }
    let Some(d) = decide(p, Some(c), &schema.name) else { return Ok(()) };
    if !d.allowed_actions.contains(action.as_str()) {
        return Err(deny_msg(action.as_str(), &schema.name));
    }
    let roles = effective_roles(c);
    let hit = p.grants.iter().any(|g| {
        g.actions.iter().any(|a| a == action.as_str())
            && (g.model == schema.name || g.model == "*")
            && roles.iter().any(|r| r == &g.role)
            && match grant_row_restriction(g, c) {
                None => true,
                Some(cond) => doc_matches_eq(doc, &cond),
            }
    });
    if hit {
        Ok(())
    } else {
        Err(format!(
            "ERR_PERMISSION:RBAC:角色对 [{}] 的 [{}] 限于本人/条件内文档",
            schema.name,
            action.as_str()
        ))
    }
}

/// 行级条件（grant 间 OR 合并）：RBAC 未生效 / 存在无行限制的匹配 grant →
/// `None`（不收紧）；否则各 grant 行限制取 `$or` 串接。
/// `action` ∈ {"read", "update", "remove"}（insert 无行级语义，调用方不传）。
pub fn row_condition(
    registry: &Registry,
    schema: &Schema,
    ctx: Option<&Context>,
    action: &str,
) -> Option<Value> {
    let p = registry.rbac()?;
    let c = ctx?;
    if c.internal {
        return None;
    }
    decide(p, Some(c), &schema.name)?;
    let roles = effective_roles(c);
    let mut conds: Vec<Value> = Vec::new();
    for g in p.grants.iter().filter(|g| {
        g.actions.iter().any(|a| a == action)
            && (g.model == schema.name || g.model == "*")
            && roles.iter().any(|r| r == &g.role)
    }) {
        match grant_row_restriction(g, c) {
            None => return None, // 任一 grant 无行限制 → 无限制
            Some(v) => conds.push(v),
        }
    }
    match conds.len() {
        0 => None,
        1 => Some(conds.pop().unwrap_or(Value::Null)),
        _ => Some(json!({ "$or": conds })),
    }
}

/// 在静态引擎产出的 owner 条件（`merge_owner_condition` 结果）之上叠加 RBAC 行
/// 条件：两引擎间 `$and`（deny-wins）。任一为 `None` → 取另一者；空对象视为无条件。
pub fn merge_row_condition(
    registry: &Registry,
    schema: &Schema,
    ctx: Option<&Context>,
    action: &str,
    base: Option<Value>,
) -> Option<Value> {
    let meaningful = |v: Option<Value>| -> Option<Value> {
        v.filter(|x| x.as_object().map(|o| !o.is_empty()).unwrap_or(false))
    };
    let rbac_cond = row_condition(registry, schema, ctx, action);
    match (meaningful(base), rbac_cond) {
        (None, r) => r,
        (b, None) => b,
        (Some(b), Some(r)) => Some(json!({ "$and": [b, r] })),
    }
}

/// 字段读交集：`base`（静态 `get_readable_fields` 结果）与 RBAC readFields 取交。
/// `_id` 随 base 保留（文档标识豁免，同 `filter_writable_data` 的 D-03 理由）。
pub fn overlay_readable_fields(
    p: Option<&RbacPolicy>,
    model: &str,
    ctx: Option<&Context>,
    base: Option<HashSet<String>>,
) -> Option<HashSet<String>> {
    let Some(p) = p else { return base };
    let Some(d) = decide(p, ctx, model) else { return base };
    let Some(rf) = &d.read_fields else { return base };
    match base {
        None => {
            let mut s: HashSet<String> = rf.clone();
            s.insert("_id".to_string());
            Some(s)
        }
        Some(b) => {
            let keep_id = b.contains("_id");
            let mut s: HashSet<String> = b.intersection(rf).cloned().collect();
            if keep_id {
                s.insert("_id".to_string());
            }
            Some(s)
        }
    }
}

/// 字段写交集（语义同 [`overlay_readable_fields`]，作用于 writeFields）
pub fn overlay_writable_fields(
    p: Option<&RbacPolicy>,
    model: &str,
    ctx: Option<&Context>,
    base: Option<HashSet<String>>,
) -> Option<HashSet<String>> {
    let Some(p) = p else { return base };
    let Some(d) = decide(p, ctx, model) else { return base };
    let Some(wf) = &d.write_fields else { return base };
    match base {
        None => {
            let mut s: HashSet<String> = wf.clone();
            s.insert("_id".to_string());
            Some(s)
        }
        Some(b) => {
            let keep_id = b.contains("_id");
            let mut s: HashSet<String> = b.intersection(wf).cloned().collect();
            if keep_id {
                s.insert("_id".to_string());
            }
            Some(s)
        }
    }
}

/// 写入数据过滤（RBAC 感知版，供写路径调用点整体替换
/// `permission::filter_writable_data`）：可写字段 = 静态 writable ∩ RBAC writeFields。
/// 与 [`crate::permission::filter_writable_data`] 逐行同构（`_id` 豁免、点号按 root），
/// 仅集合来源叠加 RBAC 交集——静态引擎文件零改动。
pub fn filter_writable_data_overlay(
    registry: &Registry,
    schema: &Schema,
    ctx: Option<&Context>,
    data: &Value,
) -> Value {
    let Some(p) = registry.rbac() else {
        return crate::permission::filter_writable_data(schema, ctx, data);
    };
    let base = get_writable_fields(schema, ctx);
    let writable = overlay_writable_fields(Some(p), &schema.name, ctx, base);
    let Some(writable) = writable else {
        return data.clone();
    };
    let Some(obj) = data.as_object() else {
        return data.clone();
    };
    let mut result = Map::new();
    for (key, val) in obj {
        let root = key.split('.').next().unwrap_or(key);
        if key == "_id" || writable.contains(root) {
            result.insert(key.clone(), val.clone());
        }
    }
    Value::Object(result)
}

// ─── 内部工具 ────────────────────────────────────────────────

/// 单 grant 的行限制：`None` = 无限制；`ownerOnly` → `{createdBy: userId}`；
/// `condition` → 原样；两者并存 → `$and`。`ownerOnly` 但 userId 缺失 → 恒假哨兵
/// （禁当 `None`——那是静默放大）。
fn grant_row_restriction(g: &Grant, c: &Context) -> Option<Value> {
    if !g.owner_only && g.condition.is_none() {
        return None;
    }
    let owner = if g.owner_only {
        let uid = c.user_id.clone().unwrap_or_default();
        if uid.is_empty() {
            // 恒假条件：__rbac_impossible__ 不可能在文档出现 → doc_matches_eq 恒 false
            Some(json!({ "__rbac_impossible__": true }))
        } else {
            Some(json!({ "createdBy": uid }))
        }
    } else {
        None
    };
    match (owner, g.condition.clone()) {
        (None, cond) => cond,
        (owner, None) => owner,
        (Some(o), Some(cond)) => Some(json!({ "$and": [o, cond] })),
    }
}

/// 等值条件命中判定（`cond` 为 `{字段: 标量}`，解析期已保证）；哨兵键恒不命中
fn doc_matches_eq(doc: &Value, cond: &Value) -> bool {
    let (Some(obj), Some(cond_map)) = (doc.as_object(), cond.as_object()) else {
        return false;
    };
    cond_map.iter().all(|(k, v)| obj.get(k).map(|dv| dv == v).unwrap_or(false))
}
