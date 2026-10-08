//! Schema 触发器声明：解析与校验（规划见 `crate::command::triggers`）。
//!
//! 声明形态（schema JSON 顶层 `triggers` 键）：
//!   "triggers": { "insert": [T...], "update": [T...] }
//! T 字段契约见执行总纲「常驻契约卡」。本模块只做**结构校验**，不做运行时判定。

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::types::str_list;

/// 事件名（insert / update / remove / schedule；其余键注册期 Err）。
/// `schedule` 仅注册期接受 —— 写链永不 expand("schedule")，由宿主定时任务插件
/// 经 [`crate::command::triggers::expand_schedule_triggers`] 枚举执行。
pub const EVENTS: [&str; 4] = ["insert", "update", "remove", "schedule"];

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
    /// 5 段 cron（分 时 日 月 周）；仅 `schedule` 事件必填，其余事件禁给
    pub cron: Option<String>,
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
///  9. 命令式 `data` 的键未在**目标 schema** 中声明（在展开期校验，见 `command::triggers`）；
/// 10. `schedule` 事件缺 `cron` / cron 非法，或 body/when 出现 `{{root.`/`{{before.`
///     （schedule 无 root/before 上下文，只允许 `{{now}}`）；非 `schedule` 事件出现 `cron` 键。
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
                "schema \"{owner}\" 的 triggers 事件键 \"{event}\" 非法（仅 insert/update/remove/schedule）"
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
    // cron 键判定：schedule 必填且须合法 5 段；其余事件禁给（避免歧义）
    let cron = match to.get("cron") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(format!("{at} 的 cron 必须是字符串")),
        None => None,
    };
    if event == "schedule" {
        let c = cron
            .as_deref()
            .ok_or_else(|| format!("{at} 的 schedule 触发器缺少 cron"))?;
        validate_cron(c).map_err(|e| format!("{at} 的 cron \"{c}\" 非法：{e}"))?;
        // schedule 无 root/before 上下文（只有 {{now}}）——出现即注册期 Err，禁运行时留空替换
        forbid_root_before_refs(to.get("when"), &at)?;
        if let Some(w) = to.get("args") {
            forbid_root_before_refs(Some(w), &at)?;
        }
        if let Some(d) = to.get("data") {
            forbid_root_before_refs(Some(d), &at)?;
        }
        if let Some(c) = to.get("condition") {
            forbid_root_before_refs(Some(c), &at)?;
        }
    } else if cron.is_some() {
        return Err(format!("{at} 的 cron 仅 schedule 事件可配"));
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
        cron,
        body,
    })
}

/// 5 段 cron（分 时 日 月 周）校验：段内仅 `*` `,` `-` `/` 与数字；数字范围分校验；步进须为正整数。
pub fn validate_cron(expr: &str) -> Result<(), String> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(format!(
            "必须为 5 段（分 时 日 月 周），实为 {} 段",
            parts.len()
        ));
    }
    // (min, max)：分(0-59) 时(0-23) 日(1-31) 月(1-12) 周(0-7，0/7 均为周日)
    let ranges = [(0i64, 59), (0, 23), (1, 31), (1, 12), (0, 7)];
    for (seg, (lo, hi)) in parts.iter().zip(ranges) {
        if !seg
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '*' | ',' | '-' | '/'))
        {
            return Err(format!("段 \"{seg}\" 含非法字符（仅 * , - / 与数字）"));
        }
        for piece in seg.split(',') {
            if piece.is_empty() {
                return Err(format!("段 \"{seg}\" 存在空列表项"));
            }
            let body = match piece.split_once('/') {
                Some((b, s)) => {
                    if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) {
                        return Err(format!("段 \"{seg}\" 的步进 \"{s}\" 须为正整数"));
                    }
                    let n: i64 = s
                        .parse()
                        .map_err(|_| format!("段 \"{seg}\" 的步进 \"{s}\" 越界"))?;
                    if n <= 0 {
                        return Err(format!("段 \"{seg}\" 的步进 \"{s}\" 须为正整数"));
                    }
                    b
                }
                None => piece,
            };
            if body == "*" {
                continue;
            }
            let (a, b) = match body.split_once('-') {
                Some((x, y)) => (x, y),
                None => (body, body),
            };
            let parse_bound = |s: &str| -> Result<i64, String> {
                if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) {
                    return Err(format!("段 \"{seg}\" 含非法形态 \"{piece}\""));
                }
                let n: i64 = s
                    .parse()
                    .map_err(|_| format!("段 \"{seg}\" 的 \"{s}\" 越界"))?;
                if n < lo || n > hi {
                    return Err(format!("段 \"{seg}\" 的 {n} 超出范围 {lo}-{hi}"));
                }
                Ok(n)
            };
            let x = parse_bound(a)?;
            let y = parse_bound(b)?;
            if x > y {
                return Err(format!("段 \"{seg}\" 区间 \"{piece}\" 起点大于终点"));
            }
        }
    }
    Ok(())
}

/// schedule body/when 递归禁 `{{root.` / `{{before.`（只允许 `{{now}}`）
fn forbid_root_before_refs(v: Option<&Value>, at: &str) -> Result<(), String> {
    match v {
        Some(Value::String(s)) => {
            if s.contains("{{root.") || s.contains("{{before.") {
                return Err(format!(
                    "{at} 的 schedule 触发器不支持 {{{{root.}}}}/{{{{before.}}}} 占位符（仅 {{{{now}}}}）"
                ));
            }
            Ok(())
        }
        Some(Value::Array(a)) => a
            .iter()
            .try_for_each(|x| forbid_root_before_refs(Some(x), at)),
        Some(Value::Object(o)) => o
            .values()
            .try_for_each(|x| forbid_root_before_refs(Some(x), at)),
        _ => Ok(()),
    }
}
