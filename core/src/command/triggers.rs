//! 触发链规划：由 `Schema.triggers` 展开为命令 / 回调步骤序列。
//!
//! **规划期不持有 root 值**（root 是 Host 主写结果）——命令 filter/data / args / when
//! 内的占位符 `{{root.*}}` / `{{before.*}}` / `{{now}}` 原样保留，由 Host 执行前替换
//! （与 mutation `{{step.N._id}}` 同范式，见 `command/mod.rs`）。

use serde_json::{json, Map, Value};

use crate::command::cmd::{cmd_delete_many, cmd_insert_one, cmd_update_many};
use crate::permission::{can_write_schema, merge_owner_condition, Context};
use crate::rbac::{ensure_write, merge_row_condition, WriteAction};
use crate::schema::{Registry, TriggerBody, TriggerDef};

use super::{forbid_t2q, ERR_NO_WRITE};

/// 某 schema + 事件是否有触发器
pub fn has_triggers(registry: &Registry, schema_name: &str, event: &str) -> bool {
    registry
        .get(schema_name)
        .ok()
        .and_then(|s| s.triggers.get(event))
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// 探针需额外投影的字段：全部 `onFields` ∪ `when`/`data`/`args` 中出现的 `{{before.<f>}}`
pub fn before_probe_fields(triggers: &[TriggerDef]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |f: &str| {
        if !f.is_empty() && !out.contains(&f.to_string()) {
            out.push(f.to_string());
        }
    };
    for td in triggers {
        for f in &td.on_fields {
            push(f);
        }
        collect_before_refs(td.when.as_ref(), &mut push);
        if let TriggerBody::Callback { args, .. } = &td.body {
            collect_before_refs(Some(args), &mut push);
        }
        if let TriggerBody::Command {
            data, condition, ..
        } = &td.body
        {
            collect_before_refs(Some(data), &mut push);
            collect_before_refs(condition.as_ref(), &mut push);
        }
    }
    out
}

/// 递归收集字符串中的 `{{before.<f>}}` 引用
fn collect_before_refs(v: Option<&Value>, push: &mut impl FnMut(&str)) {
    match v {
        Some(Value::String(s)) => {
            let mut rest = s.as_str();
            while let Some(p) = rest.find("{{before.") {
                let after = &rest[p + "{{before.".len()..];
                if let Some(end) = after.find("}}") {
                    let field = after[..end].trim();
                    let key = field.split(['.', '[']).next().unwrap_or(field);
                    push(key);
                    rest = &after[end + 2..];
                } else {
                    break;
                }
            }
        }
        Some(Value::Array(a)) => a.iter().for_each(|x| collect_before_refs(Some(x), push)),
        Some(Value::Object(o)) => o.values().for_each(|x| collect_before_refs(Some(x), push)),
        _ => {}
    }
}

/// 展开某 schema + 事件的触发链（只展开一层）。
///
/// 产出 step 列表；每个 step：
///   `{ "name", "onFields", "when", "command"|"callback" }`
pub fn expand_triggers(
    registry: &Registry,
    ctx: Option<&Context>,
    owner_schema: &str,
    event: &str,
) -> Result<Vec<Value>, String> {
    let schema = registry.get(owner_schema)?;
    let Some(list) = schema.triggers.get(event) else {
        return Ok(Vec::new());
    };
    // text2query 档：写法副作用不得由 AI 问数触发（A9）
    if !list.is_empty() {
        forbid_t2q(registry, "triggers")?;
    }
    let mut out = Vec::new();
    for (i, td) in list.iter().enumerate() {
        out.push(build_trigger_step(
            registry,
            ctx,
            owner_schema,
            event,
            i,
            td,
        )?);
    }
    Ok(out)
}

/// 枚举全 registry 的 `schedule` 触发器（宿主定时任务插件消费）。
///
/// 产出 `[{ "schema", "name", "cron", "step" }]`；`step` 与写链触发步骤同构
/// （`{ name, onFields, when, command|callback }`），占位符仅 `{{now}}`
/// （root/before 已在注册期拒，见 `schema::triggers::parse_triggers`）。
/// 按 `registry.list()` 顺序枚举、每 schema 按声明顺序，输出稳定可对拍。
pub fn expand_schedule_triggers(
    registry: &Registry,
    ctx: Option<&Context>,
) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    let mut t2q_checked = false;
    for owner in registry.list() {
        let schema = registry.get(&owner)?;
        let Some(list) = schema.triggers.get("schedule") else {
            continue;
        };
        if list.is_empty() {
            continue;
        }
        // text2query 档：写法副作用不得由 AI 问数触发（A9），幂等防重
        if !t2q_checked {
            forbid_t2q(registry, "triggers")?;
            t2q_checked = true;
        }
        for (i, td) in list.iter().enumerate() {
            let step = build_trigger_step(registry, ctx, &owner, "schedule", i, td)?;
            let name = step
                .get("name")
                .cloned()
                .unwrap_or(Value::Null);
            out.push(json!({
                "schema": owner,
                "name": name,
                "cron": td.cron,
                "step": step,
            }));
        }
    }
    Ok(out)
}

fn build_trigger_step(
    registry: &Registry,
    ctx: Option<&Context>,
    owner: &str,
    event: &str,
    idx: usize,
    td: &TriggerDef,
) -> Result<Value, String> {
    let name = if td.name.is_empty() {
        format!("{owner}.{event}[{idx}]")
    } else {
        format!("{owner}.{event}.{}", td.name)
    };
    let base = |body: Value| -> Value {
        let mut m = Map::new();
        m.insert("name".into(), json!(name));
        m.insert("onFields".into(), json!(td.on_fields));
        m.insert("when".into(), td.when.clone().unwrap_or(Value::Null));
        if let Value::Object(o) = body {
            for (k, v) in o {
                m.insert(k, v);
            }
        }
        Value::Object(m)
    };
    match &td.body {
        TriggerBody::Command {
            into,
            op,
            condition,
            data,
        } => {
            let target = registry.get(into)?;
            if !can_write_schema(registry.role_rules(), target, ctx) {
                return Err(ERR_NO_WRITE.to_string());
            }
            ensure_write(
                registry,
                target,
                ctx,
                match op.as_str() {
                    "insert" => WriteAction::Insert,
                    "remove" => WriteAction::Remove,
                    _ => WriteAction::Update,
                },
            )?;
            // 字段声明校验（值可能含占位符 → 只校验键，不校验类型）；
            // `_id` 豁免 —— 主键由 Host 显式供给（idPrefix/占位符），与普通 insert
            // 文档显式携带 `_id` 同一契约（schema.fields 不声明 `_id`，见 registry.rs）
            let tfields: Vec<&String> = target.fields.keys().collect();
            if let Value::Object(d) = data {
                for k in d.keys() {
                    if k != "_id" && !k.starts_with('$') && !tfields.contains(&k) {
                        return Err(format!(
                            "触发器 \"{name}\" 的 data 字段 \"{k}\" 未在目标 schema \"{into}\" 中声明"
                        ));
                    }
                }
            }
            // update/remove 目标补行级探针：静态 owner 条件叠加 RBAC 行条件后并入
            // filter（与写路径 `merge_row_condition` 同源；匹配不到即 0 行/拒绝），
            // 杜绝触发器 update/remove 静默命中他人行。insert 的行级写覆盖由
            // `cmd_insert_one`（filter_writable_data_overlay）承接，不在此叠加。
            let base_condition = condition.as_ref().cloned().unwrap_or_else(|| json!({}));
            let row_filtered = |action: &str| -> Value {
                let owner_merged = merge_owner_condition(
                    registry.role_rules(),
                    target,
                    ctx,
                    Some(base_condition.clone()),
                );
                merge_row_condition(registry, target, ctx, action, owner_merged)
                    .unwrap_or_else(|| json!({}))
            };
            let command = match op.as_str() {
                "insert" => cmd_insert_one(target, data),
                "update" => cmd_update_many(target, &row_filtered("update"), data),
                // remove：condition 圈定删除目标，无 data（parse 已拒）
                "remove" => cmd_delete_many(target, &row_filtered("remove")),
                other => return Err(format!("触发器 \"{name}\" 的 op \"{other}\" 不支持")),
            };
            Ok(base(json!({ "command": command })))
        }
        TriggerBody::Callback { fn_ref, args } => Ok(base(json!({
            "callback": { "fnRef": fn_ref, "args": args }
        }))),
    }
}
