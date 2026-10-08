//! Schema 触发器声明：解析与校验（规划见 `crate::command::triggers`）。
//!
//! 声明形态（schema JSON 顶层 `triggers` 键）：
//!   "triggers": { "insert": [T...], "update": [T...] }
//! T 字段契约见执行总纲「常驻契约卡」。本模块只做**结构校验**，不做运行时判定。

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::types::str_list;

/// 事件名（insert / update / remove；其余键注册期 Err）
pub const EVENTS: [&str; 3] = ["insert", "update", "remove"];

/// 命令式触发支持的 op 白名单（首批不含 upsert）
pub const OPS: [&str; 3] = ["insert", "update", "remove"];

/// `triggers` = 事件名 → 触发列表
pub type Triggers = HashMap<String, Vec<TriggerDef>>;

#[derive(Debug, Clone)]
pub struct TriggerDef {
    /// 显式名（空 = 缺省 `<Schema>.<event>[<i>]`）
    pub name: String,
    /// 运行时条件（占位符替换后判定）；`None` = 恒真
    pub when: Option<Value>,
    /// 字段级：仅这些字段「值真的变化」才进入判定；空 = 记录级
    pub on_fields: Vec<String>,
    pub body: TriggerBody,
}

#[derive(Debug, Clone)]
pub enum TriggerBody {
    /// 命令式：写目标 schema
    Command {
        into: String,
        op: String,               // ∈ OPS
        condition: Option<Value>, // op=update 必填
        data: Value,              // 必填
    },
    /// 回调式：Host 侧 fnRef 实现
    Callback { fn_ref: String, args: Value },
}

/// 解析并校验 schema 顶层的 `triggers`；任一项非法 → Err（零静默）。
///
/// 校验清单（全部命中即 Err）：
///  1. `triggers` 非对象；
///  2. 事件键不在 `EVENTS`；
///  3. 事件值非数组 / 元素非对象；
///  4. `into`/`op`/`data`（命令式）与 `fnRef`（回调式）**互斥且必居其一**；
///  5. `op` 不在 `OPS`；
///  6. 命令式缺 `data`（`op=remove` 相反：出现 `data` 即 Err），或 `op=update`/`op=remove` 缺 `condition`；
///  7. `onFields` 非字符串数组，或字段未在 `fields` 中声明（`$` 前缀键除外）；
///  8. `when` 非对象；出现 `cascade` 键（首批不支持级联）；
///  9. 命令式 `data` 的键未在**目标 schema** 中声明（在展开期校验，见 `command::triggers`）。
pub fn parse_triggers(
    obj: &Map<String, Value>,
    owner: &str,
    fields: &HashMap<String, crate::schema::FieldDef>,
) -> Result<Triggers, String> {
    let mut out: Triggers = HashMap::new();
    let Some(Value::Object(tm)) = obj.get("triggers") else {
        return Ok(out);
    };
    for (event, val) in tm {
        if !EVENTS.contains(&event.as_str()) {
            return Err(format!(
                "schema \"{owner}\" 的 triggers 事件键 \"{event}\" 非法（仅 insert/update/remove）"
            ));
        }
        let arr = val
            .as_array()
            .ok_or_else(|| format!("schema \"{owner}\" 的 triggers.{event} 必须是数组"))?;
        let mut list = Vec::new();
        for (i, t) in arr.iter().enumerate() {
            let to = t
                .as_object()
                .ok_or_else(|| format!("schema \"{owner}\" 的 triggers.{event}[{i}] 必须是对象"))?;
            list.push(parse_one(owner, event, i, to, fields)?);
        }
        out.insert(event.clone(), list);
    }
    Ok(out)
}

fn parse_one(
    owner: &str,
    event: &str,
    idx: usize,
    to: &Map<String, Value>,
    fields: &HashMap<String, crate::schema::FieldDef>,
) -> Result<TriggerDef, String> {
    let at = format!("{owner}.triggers.{event}[{idx}]");
    let has_cmd = to.contains_key("into") || to.contains_key("op") || to.contains_key("data");
    let has_cb = to.contains_key("fnRef");
    if has_cmd == has_cb {
        return Err(format!(
            "{at} 必须二选一：命令式（into+op+data）或回调式（fnRef），不可同给或同缺"
        ));
    }
    // onFields 字段声明校验（仅 update 有意义；insert/remove 声明了也接受但忽略）
    let on_fields = str_list(to.get("onFields")).unwrap_or_default();
    for f in &on_fields {
        if !f.starts_with('$') && !fields.contains_key(f) {
            return Err(format!("{at} 的 onFields 字段 \"{f}\" 未在 schema 中声明"));
        }
    }
    if let Some(w) = to.get("when") {
        if !w.is_object() {
            return Err(format!("{at} 的 when 必须是对象"));
        }
    }
    if to.contains_key("cascade") {
        return Err(format!("{at} 不支持 cascade（首批触发链只展开一层）"));
    }
    let name = to
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();

    let body = if has_cb {
        TriggerBody::Callback {
            fn_ref: to
                .get("fnRef")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("{at} 的 fnRef 须为非空字符串"))?
                .to_string(),
            args: to.get("args").cloned().unwrap_or(Value::Null),
        }
    } else {
        let into = to
            .get("into")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("{at} 缺少 into"))?
            .to_string();
        let op = to
            .get("op")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{at} 缺少 op"))?
            .to_string();
        if !OPS.contains(&op.as_str()) {
            return Err(format!("{at} 的 op \"{op}\" 不在白名单（insert/update/remove）"));
        }
        let data = match op.as_str() {
            // remove：删除语义由 condition 圈定目标，不接受 data（出现即 Err，零静默）
            "remove" => {
                if to.get("data").map(|d| !d.is_null()).unwrap_or(false) {
                    return Err(format!("{at} 的 op=remove 不接受 data"));
                }
                Value::Null
            }
            _ => to
                .get("data")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| format!("{at} 的命令式触发缺 data（须为对象）"))?,
        };
        let condition = to.get("condition").cloned();
        if (op == "update" || op == "remove")
            && !condition.as_ref().map(|v| v.is_object()).unwrap_or(false)
        {
            return Err(format!("{at} 的 op={op} 必须提供 condition（对象）"));
        }
        TriggerBody::Command {
            into,
            op,
            condition,
            data,
        }
    };
    Ok(TriggerDef {
        name,
        when: to.get("when").cloned(),
        on_fields,
        body,
    })
}
