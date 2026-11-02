//! `accessKeys` 运行时判决（读路径行条件 + 写路径子集校验的公共解析）。
//!
//! 声明解析见 [`crate::schema::access_keys`]；本模块只消费已校验的
//! [`crate::schema::AccessKeyDef`] 与 [`crate::permission::Context`] 清单。
//!
//! 语义：
//! - 读：`{字段: {"$in": 清单}}`，与 owner / RBAC 条件 `$and` 叠加；
//!   单值字段 = 标量成员判定，`array` 字段 = 交集命中（Mongo 原生 `$in` 语义）；
//! - 写：单值目标 ∈ 清单、`array` 目标 ⊆ 清单（见写校验函数）；
//! - ctx 缺键且未 bypass → `ERR_PERMISSION:ACCESS:`（fail closed，禁静默放行）；
//! - `清单: []` 是结论（读 0 行 / 写全拒），缺键是错误，两者严格区分。

use serde_json::{json, Value};

use crate::permission::Context;
use crate::schema::{AccessKeyDef, Schema};

/// accessKeys 拒绝错误统一前缀（Host 依此前缀映射 PermissionError）
pub const ERR_ACCESS_PREFIX: &str = "ERR_PERMISSION:ACCESS:";

/// 角色是否命中该键的 `bypassRoles`（`internal` 恒豁免）。
pub fn is_bypassed(def: &AccessKeyDef, ctx: &Context) -> bool {
    if ctx.internal {
        return true;
    }
    let mut roles: Vec<&str> = Vec::new();
    if let Some(rs) = &ctx.roles {
        roles.extend(rs.iter().map(String::as_str));
    }
    if let Some(r) = &ctx.role {
        roles.push(r.as_str());
    }
    roles
        .iter()
        .any(|r| def.bypass_roles.iter().any(|b| b == r))
}

/// 取 ctx 清单；缺键 / 形态非法 → 显式 Err（fail closed）。
pub fn resolve_list<'a>(
    ctx: &'a Context,
    ctx_key: &str,
    field: &str,
) -> Result<&'a Vec<Value>, String> {
    if let Some(err) = ctx.access_list_errors.get(ctx_key) {
        return Err(format!(
            "{ERR_ACCESS_PREFIX}ctx 键 \"{ctx_key}\" 的{err}（字段 \"{field}\" 的行级权限清单非法）"
        ));
    }
    match ctx.access_lists.get(ctx_key) {
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(format!(
            "{ERR_ACCESS_PREFIX}ctx 键 \"{ctx_key}\" 的清单必须为数组"
        )),
        None => Err(format!(
            "{ERR_ACCESS_PREFIX}ctx 缺少字段 \"{field}\" 的行级权限清单键 \"{ctx_key}\"\
             （角色未命中 bypassRoles，拒绝静默放行）"
        )),
    }
}

/// 读/写行条件：按 action 取 `read` / `write` 引用键，逐键生成 `$in` 条件并 `$and`。
///
/// - `ctx: None` → `None`（沿用「无 ctx = 系统内部调用跳过权限」契约）；
/// - `read` 用于查询；`update` / `remove` 用于批量写行收紧（使用 `write` 键）；
/// - `write` 缺省 = 该键只读控，写路径不生成条件。
pub fn row_condition(
    schema: &Schema,
    ctx: Option<&Context>,
    action: &str,
) -> Result<Option<Value>, String> {
    let Some(c) = ctx else {
        return Ok(None);
    };
    if !matches!(action, "read" | "update" | "remove") {
        return Ok(None);
    }
    let mut conds: Vec<Value> = Vec::new();
    for def in &schema.access_keys {
        if is_bypassed(def, c) {
            continue;
        }
        let key = if action == "read" {
            Some(def.read.as_str())
        } else {
            def.write.as_deref()
        };
        let Some(key) = key else {
            continue;
        };
        let list = resolve_list(c, key, &def.field)?;
        let mut obj = serde_json::Map::new();
        obj.insert(def.field.clone(), json!({ "$in": list.clone() }));
        conds.push(Value::Object(obj));
    }
    Ok(match conds.len() {
        0 => None,
        1 => Some(conds.remove(0)),
        _ => Some(json!({ "$and": conds })),
    })
}
