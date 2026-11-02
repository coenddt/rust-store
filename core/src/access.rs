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

use serde_json::{json, Map, Value};

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

// ─── 写路径 ──────────────────────────────────────────────────

/// 该 schema 本次写是否受 accessKeys 管控（`ctx=None` / `internal` / 全部 bypass /
/// `write=None` → `false`）。
///
/// 缺清单键 / 清单非数组 → 显式 `Err`（fail closed）；`清单: []` 亦返回 `true`
/// （写全拒由各 `validate_*` 给出「清单为空」错误）。
pub fn write_restricted(schema: &Schema, ctx: Option<&Context>) -> Result<bool, String> {
    let Some(c) = ctx.filter(|c| !c.internal) else {
        return Ok(false);
    };
    Ok(!active_write_keys(schema, c)?.is_empty())
}

/// 收集本次写实际受控的 `(定义, ctx 键, 清单)`；缺键 / 非法形态 → `Err`。
fn active_write_keys<'a>(
    schema: &'a Schema,
    c: &'a Context,
) -> Result<Vec<(&'a AccessKeyDef, &'a str, &'a Vec<Value>)>, String> {
    let mut out = Vec::new();
    for def in &schema.access_keys {
        if is_bypassed(def, c) {
            continue;
        }
        let Some(key) = def.write.as_deref() else {
            continue;
        };
        let list = resolve_list(c, key, &def.field)?;
        out.push((def, key, list));
    }
    Ok(out)
}

fn is_array_field(schema: &Schema, field: &str) -> bool {
    schema
        .fields
        .get(field)
        .map(|f| f.field_type == "array")
        .unwrap_or(false)
}

fn empty_list_err(key: &str, field: &str) -> String {
    format!("{ERR_ACCESS_PREFIX}清单 \"{key}\" 为空，拒绝写入字段 \"{field}\"")
}

/// payload 目标值校验：单值目标 ∈ 清单；`array` 目标 ⊆ 清单。
///
/// 仅校验 `data` 顶层出现的受控字段；点号路径改写（`field.xxx`）一律拒绝，
/// 防止绕过值校验。
pub fn validate_write_values(
    schema: &Schema,
    ctx: Option<&Context>,
    data: &Map<String, Value>,
) -> Result<(), String> {
    let Some(c) = ctx.filter(|c| !c.internal) else {
        return Ok(());
    };
    for (def, key, list) in active_write_keys(schema, c)? {
        let field = def.field.as_str();
        if let Some(path) = data.keys().find(|k| k.starts_with(&format!("{field}."))) {
            return Err(format!(
                "{ERR_ACCESS_PREFIX}不允许通过点号路径 \"{path}\" 改写受 accessKeys 管控的字段 \"{field}\""
            ));
        }
        let Some(target) = data.get(field) else {
            continue;
        };
        if list.is_empty() {
            return Err(empty_list_err(key, field));
        }
        if is_array_field(schema, field) {
            let Some(items) = target.as_array() else {
                return Err(format!(
                    "{ERR_ACCESS_PREFIX}字段 \"{field}\" 为数组，目标值必须为数组（实际 {target}）"
                ));
            };
            for item in items {
                if !list.contains(item) {
                    return Err(format!(
                        "{ERR_ACCESS_PREFIX}字段 \"{field}\" 的目标数组含清单 \"{key}\" 之外的元素 {item}"
                    ));
                }
            }
        } else if !list.contains(target) {
            return Err(format!(
                "{ERR_ACCESS_PREFIX}字段 \"{field}\" 的目标值 {target} 不在清单 \"{key}\" 中"
            ));
        }
    }
    Ok(())
}

/// 探针文档校验：对**被操作文档当前值**做单值 ∈ 清单 / `array` ⊆ 清单（子集，非交集）。
pub fn validate_doc_values(
    schema: &Schema,
    ctx: Option<&Context>,
    doc: &Value,
) -> Result<(), String> {
    let Some(c) = ctx.filter(|c| !c.internal) else {
        return Ok(());
    };
    for (def, key, list) in active_write_keys(schema, c)? {
        let field = def.field.as_str();
        if list.is_empty() {
            return Err(empty_list_err(key, field));
        }
        if is_array_field(schema, field) {
            match doc.get(field) {
                None | Some(Value::Null) => {}
                Some(Value::Array(items)) => {
                    for item in items {
                        if !list.contains(item) {
                            return Err(format!(
                                "{ERR_ACCESS_PREFIX}文档字段 \"{field}\" 含清单 \"{key}\" 之外的元素 {item}"
                            ));
                        }
                    }
                }
                Some(other) => {
                    return Err(format!(
                        "{ERR_ACCESS_PREFIX}文档字段 \"{field}\" 非数组，无法做子集判定（实际 {other}）"
                    ))
                }
            }
        } else {
            match doc.get(field) {
                None | Some(Value::Null) => {
                    return Err(format!(
                        "{ERR_ACCESS_PREFIX}文档字段 \"{field}\" 缺失，不在清单 \"{key}\" 中"
                    ))
                }
                Some(v) if !list.contains(v) => {
                    return Err(format!(
                        "{ERR_ACCESS_PREFIX}文档字段 \"{field}\" 的值 {v} 不在清单 \"{key}\" 中"
                    ))
                }
                Some(_) => {}
            }
        }
    }
    Ok(())
}

/// 原生操作符（`$inc` / `$unset` / `$push` / `$pull` / `$addToSet` 等）不得改写
/// 受控字段；`$set` 放行（其值已由 [`validate_write_values`] 校验）。
pub fn validate_raw_operators(
    schema: &Schema,
    ctx: Option<&Context>,
    update: &Value,
) -> Result<(), String> {
    let Some(c) = ctx.filter(|c| !c.internal) else {
        return Ok(());
    };
    let pairs = active_write_keys(schema, c)?;
    if pairs.is_empty() {
        return Ok(());
    }
    let Some(obj) = update.as_object() else {
        return Ok(());
    };
    for (op, val) in obj {
        if !op.starts_with('$') || op == "$set" {
            continue;
        }
        let Some(fields) = val.as_object() else {
            continue;
        };
        for name in fields.keys() {
            let root = name.split('.').next().unwrap_or(name);
            if let Some((def, key, _)) = pairs.iter().find(|(d, _, _)| d.field == root) {
                if def.write.as_deref() == Some(*key) {
                    return Err(format!(
                        "{ERR_ACCESS_PREFIX}操作符 {op} 不允许改写受 accessKeys 管控的字段 \"{root}\""
                    ));
                }
            }
        }
    }
    Ok(())
}

/// 探针需额外投影的受控字段名（缺键即 `Err`，保证探针前已完成 fail closed）。
pub fn probe_fields(schema: &Schema, ctx: Option<&Context>) -> Result<Vec<String>, String> {
    let Some(c) = ctx.filter(|c| !c.internal) else {
        return Ok(Vec::new());
    };
    Ok(active_write_keys(schema, c)?
        .into_iter()
        .map(|(def, _, _)| def.field.clone())
        .collect())
}
