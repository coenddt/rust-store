//! 本地磁盘数据源 —— 聚合管道阶段求值（`$match $sort $skip $limit $project $unwind
//! $lookup $group $addFields/$set $facet $replaceRoot`）。
//!
//! 语义基准 = **MongoDB 驱动语义**（local 与 mongo 走同一条命令路径，必须同结果）。
//! 纯逻辑：无 IO / 无时钟 / 无随机（确定性是双端对拍的前提）——
//! 文件读写只在宿主，本层只吃「整目录集合快照 + core 规划产出的命令」。
//!
//! `$facet` / `$replaceRoot`：core 的全表单组空集护栏（`$group._id = null` 时
//! `$facet{__rows:[]}` + `$replaceRoot{$ifNull:[{$arrayElemAt:["$__rows",0]}, 缺省行]}`，
//! 见 `pipeline/group.rs` 第 416–439 行）必须被正确执行，否则「无 by 键的全表单组」
//! 在 SQL 侧返回 1 行、local 却返回 0 行（语义漂移）。故本层按 Mongo 语义实现这两个
//! 阶段的一般形态（`$facet` 逐子管道求值 → 单行 `{名: 行数组}`；`$replaceRoot` 以
//! `newRoot` 表达式替换每行），**未知阶段一律 `Err`**，绝不静默跳过或返回空结果。

use std::cmp::Ordering;
use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::local::filter;
use crate::local::value;
use crate::local::value::{eval_expr, get_path};

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 以 `source` 集合为起点执行聚合阶段序列。
///
/// `collections`：`{ "<物理集合名>": [文档, …] }` 整目录快照；`source` 或 `$lookup.from`
/// 指向的集合缺失 → 按**空集合**处理（对齐「未落盘即空表」）。
pub fn run(collections: &Value, source: &str, stages: &[Value]) -> Result<Vec<Value>, String> {
    let map = collections.as_object().ok_or_else(|| {
        format!(
            "collections 必须是对象 {{集合名: [文档…]}}，收到 {}",
            type_name(collections)
        )
    })?;
    let rows = collection_rows(map, source)?;
    run_stages(map, rows, stages, &Map::new())
}

/// 取集合的文档数组；缺失集合 → 空数组；非数组 → `Err`（禁静默当空集合）。
fn collection_rows(collections: &Map<String, Value>, name: &str) -> Result<Vec<Value>, String> {
    match collections.get(name) {
        None => Ok(Vec::new()),
        Some(Value::Array(a)) => Ok(a.clone()),
        Some(other) => Err(format!(
            "集合 \"{name}\" 必须是文档数组，收到 {}",
            type_name(other)
        )),
    }
}

/// 按序应用阶段（`vars` 承载 `$lookup.let` 注入的 `$$变量`）。
fn run_stages(
    collections: &Map<String, Value>,
    mut rows: Vec<Value>,
    stages: &[Value],
    vars: &Map<String, Value>,
) -> Result<Vec<Value>, String> {
    for stage in stages {
        let obj = stage
            .as_object()
            .ok_or_else(|| format!("管道阶段必须是单键对象，收到 {}", type_name(stage)))?;
        if obj.len() != 1 {
            return Err(format!(
                "管道阶段必须恰有一个键，收到 {} 个（{stage}）",
                obj.len()
            ));
        }
        let (key, arg) = obj.iter().next().expect("len=1");
        rows = apply_stage(collections, rows, key, arg, vars)?;
    }
    Ok(rows)
}

/// 单阶段求值。
fn apply_stage(
    collections: &Map<String, Value>,
    rows: Vec<Value>,
    key: &str,
    arg: &Value,
    vars: &Map<String, Value>,
) -> Result<Vec<Value>, String> {
    match key {
        "$match" => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                if filter::matches_with_vars(&row, arg, vars)? {
                    out.push(row);
                }
            }
            Ok(out)
        }
        "$sort" => {
            let spec = sort_spec(arg)?;
            let mut rows = rows;
            rows.sort_by(|a, b| compare_rows(a, b, &spec));
            Ok(rows)
        }
        "$skip" => {
            let n = usize_of("$skip", arg)?;
            Ok(rows.into_iter().skip(n).collect())
        }
        "$limit" => {
            let n = usize_of("$limit", arg)?;
            Ok(rows.into_iter().take(n).collect())
        }
        "$project" => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push(project_row(&row, arg, vars)?);
            }
            Ok(out)
        }
        "$unwind" => apply_unwind(rows, arg),
        "$lookup" => apply_lookup(collections, rows, arg, vars),
        "$group" => apply_group(rows, arg, vars),
        "$addFields" | "$set" => {
            let spec = arg
                .as_object()
                .ok_or_else(|| format!("{key} 需要对象，收到 {}", type_name(arg)))?;
            let mut out = Vec::with_capacity(rows.len());
            for mut row in rows {
                for (path, expr) in spec {
                    let v = eval_expr(expr, &row, vars)?;
                    value::set_path(&mut row, path, v);
                }
                out.push(row);
            }
            Ok(out)
        }
        // 逐子管道求值，输出单行 `{子管道名: 行数组}`（Mongo `$facet` 语义）
        "$facet" => {
            let spec = arg
                .as_object()
                .ok_or_else(|| format!("$facet 需要对象，收到 {}", type_name(arg)))?;
            let mut doc = Map::new();
            for (name, sub) in spec {
                let sub_stages = sub
                    .as_array()
                    .ok_or_else(|| format!("$facet 的子管道 \"{name}\" 必须是阶段数组"))?;
                let sub_rows = run_stages(collections, rows.clone(), sub_stages, vars)?;
                doc.insert(name.clone(), Value::Array(sub_rows));
            }
            Ok(vec![Value::Object(doc)])
        }
        // 以 `newRoot` 表达式替换每行；求值结果必须是文档（否则 `Err`，禁静默丢弃）
        "$replaceRoot" => {
            let spec = arg
                .as_object()
                .ok_or_else(|| format!("$replaceRoot 需要对象，收到 {}", type_name(arg)))?;
            let new_root = spec
                .get("newRoot")
                .ok_or_else(|| "$replaceRoot 缺少 newRoot".to_string())?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let v = eval_expr(new_root, &row, vars)?;
                if !v.is_object() {
                    return Err(format!(
                        "$replaceRoot.newRoot 求值结果必须是文档，收到 {}",
                        type_name(&v)
                    ));
                }
                out.push(v);
            }
            Ok(out)
        }
        other => Err(format!(
            "不支持的聚合阶段 {other}（本地求值器拒绝静默跳过；支持 $match/$sort/$skip/$limit/$project/$unwind/$lookup/$group/$addFields/$set/$facet/$replaceRoot）"
        )),
    }
}

// ---------------------------------------------------------------------------
// $sort / $skip / $limit
// ---------------------------------------------------------------------------

/// 排序规格：对象 `{键: 方向}` 或数组 `[[键, 方向], …]`。
fn sort_spec(arg: &Value) -> Result<Vec<(String, i64)>, String> {
    if let Some(o) = arg.as_object() {
        let mut out = Vec::with_capacity(o.len());
        for (k, v) in o {
            out.push((k.clone(), direction("$sort", k, v)?));
        }
        return Ok(out);
    }
    if let Some(a) = arg.as_array() {
        let mut out = Vec::with_capacity(a.len());
        for item in a {
            let pair = item
                .as_array()
                .ok_or_else(|| "$sort 数组形态的元素必须是 [键, 方向]".to_string())?;
            let k = pair
                .first()
                .and_then(Value::as_str)
                .ok_or_else(|| "$sort 数组形态的键必须是字符串".to_string())?;
            let dir = pair
                .get(1)
                .ok_or_else(|| "$sort 数组形态缺少方向".to_string())?;
            out.push((k.to_string(), direction("$sort", k, dir)?));
        }
        return Ok(out);
    }
    Err(format!(
        "$sort 需要对象或 [键, 方向] 数组，收到 {}",
        type_name(arg)
    ))
}

/// 排序方向：非零正数 → 升序、负数 → 降序。
fn direction(stage: &str, field: &str, v: &Value) -> Result<i64, String> {
    let d = v
        .as_i64()
        .or_else(|| v.as_f64().map(|f| f as i64))
        .ok_or_else(|| {
            format!(
                "{stage} 的方向必须是数字，字段 \"{field}\" 收到 {}",
                type_name(v)
            )
        })?;
    Ok(if d < 0 { -1 } else { 1 })
}

fn compare_rows(a: &Value, b: &Value, spec: &[(String, i64)]) -> Ordering {
    for (k, dir) in spec {
        let o = value::compare(&sort_value(a, k), &sort_value(b, k));
        if o != Ordering::Equal {
            return if *dir < 0 { o.reverse() } else { o };
        }
    }
    Ordering::Equal
}

/// 排序取值：点号路径严格取值；缺失 → `null`（Mongo 序最低）；数组字段取**最小元素**。
fn sort_value(doc: &Value, path: &str) -> Value {
    match get_path(doc, path) {
        None => Value::Null,
        Some(Value::Array(a)) => a
            .iter()
            .min_by(|x, y| value::compare(x, y))
            .cloned()
            .unwrap_or(Value::Null),
        Some(other) => other.clone(),
    }
}

/// `$skip` / `$limit` 的非负整数解析。
fn usize_of(stage: &str, v: &Value) -> Result<usize, String> {
    let n = v
        .as_i64()
        .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
        .ok_or_else(|| format!("{stage} 需要整数，收到 {}", type_name(v)))?;
    if n < 0 {
        return Err(format!("{stage} 不能为负数（{n}）"));
    }
    Ok(n as usize)
}

// ---------------------------------------------------------------------------
// $unwind
// ---------------------------------------------------------------------------

/// `$unwind`：`path`（`$` 前缀可有可无）+ `preserveNullAndEmptyArrays`（默认 `false`）。
///
/// - 数组非空 → 每元素一行；
/// - 数组为空 / 显式 `null` / 字段缺失 → 默认丢弃该行；`preserveNullAndEmptyArrays: true`
///   时保留该行并**移除该字段**（对齐 Mongo）；
/// - 非数组标量 → 原样一行。
fn apply_unwind(rows: Vec<Value>, arg: &Value) -> Result<Vec<Value>, String> {
    let spec = arg
        .as_object()
        .ok_or_else(|| format!("$unwind 需要对象，收到 {}", type_name(arg)))?;
    let raw = spec
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "$unwind 缺少 path".to_string())?;
    let path = raw.strip_prefix('$').unwrap_or(raw);
    let preserve = match spec.get("preserveNullAndEmptyArrays") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(other) => {
            return Err(format!(
                "$unwind.preserveNullAndEmptyArrays 必须是布尔，收到 {}",
                type_name(other)
            ));
        }
    };

    let mut out = Vec::new();
    for row in rows {
        match get_path(&row, path).cloned() {
            Some(Value::Array(a)) if !a.is_empty() => {
                for el in a {
                    let mut r = row.clone();
                    value::set_path(&mut r, path, el);
                    out.push(r);
                }
            }
            Some(Value::Array(_)) | Some(Value::Null) | None => {
                if preserve {
                    let mut r = row.clone();
                    value::remove_path(&mut r, path);
                    out.push(r);
                }
            }
            Some(_) => out.push(row),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// $lookup
// ---------------------------------------------------------------------------

/// `$lookup` 两形态：
/// 1. `{from, localField, foreignField, as}`（用户 `$pipeline` 直通常见形态）；
/// 2. `{from, let, pipeline, as}`（**core 关系下推的实际形态**：`let` 在父文档求值，
///    作为 `$$变量` 注入子管道，子管道以 `{$expr: {$eq: ["$fk", "$$var"]}}` 匹配外键）。
fn apply_lookup(
    collections: &Map<String, Value>,
    rows: Vec<Value>,
    arg: &Value,
    vars: &Map<String, Value>,
) -> Result<Vec<Value>, String> {
    let spec = arg
        .as_object()
        .ok_or_else(|| format!("$lookup 需要对象，收到 {}", type_name(arg)))?;
    let from = spec
        .get("from")
        .and_then(Value::as_str)
        .ok_or_else(|| "$lookup 缺少 from".to_string())?;
    let as_name = spec
        .get("as")
        .and_then(Value::as_str)
        .ok_or_else(|| "$lookup 缺少 as".to_string())?;
    let target = collection_rows(collections, from)?;

    if let Some(local_field) = spec.get("localField").and_then(Value::as_str) {
        let foreign_field = spec
            .get("foreignField")
            .and_then(Value::as_str)
            .unwrap_or(local_field);
        let mut out = Vec::with_capacity(rows.len());
        for mut row in rows {
            let locals = local_values(&row, local_field);
            let mut matched = Vec::new();
            for t in &target {
                let hits = value::resolve_path_candidates(t, foreign_field)
                    .iter()
                    .any(|c| locals.iter().any(|lv| value::matches_value(c, lv)));
                if hits {
                    matched.push(t.clone());
                }
            }
            value::set_path(&mut row, as_name, Value::Array(matched));
            out.push(row);
        }
        return Ok(out);
    }

    let pipeline = spec
        .get("pipeline")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if spec.get("pipeline").is_none() && spec.get("localField").is_none() {
        return Err("$lookup 需要 localField/foreignField 或 pipeline 之一".to_string());
    }
    let let_map = spec
        .get("let")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(rows.len());
    for mut row in rows {
        let mut inner_vars = vars.clone();
        for (name, expr) in &let_map {
            inner_vars.insert(name.clone(), eval_expr(expr, &row, vars)?);
        }
        let matched = run_stages(collections, target.clone(), &pipeline, &inner_vars)?;
        value::set_path(&mut row, as_name, Value::Array(matched));
        out.push(row);
    }
    Ok(out)
}

/// 本地字段取值：数组展开为元素（缺失 → 单个 `null`，对齐 Mongo 等值匹配语义）。
fn local_values(doc: &Value, field: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for c in value::resolve_path_candidates(doc, field) {
        match c {
            Value::Array(a) => out.extend(a.iter().cloned()),
            other => out.push(other.clone()),
        }
    }
    if out.is_empty() {
        out.push(Value::Null);
    }
    out
}

// ---------------------------------------------------------------------------
// $group
// ---------------------------------------------------------------------------

/// 根级 `$group`：`_id` 分组键表达式 + 累积器（`$sum` 覆盖 `$count` 两种形态）。
///
/// 分组顺序 = 首次出现顺序（确定性，供双端对拍）。空集语义与 SQL 侧一致：
/// `$sum` → `0`、`$avg/$min/$max` 无可用值 → `null`（§9.7）。
fn apply_group(
    rows: Vec<Value>,
    arg: &Value,
    vars: &Map<String, Value>,
) -> Result<Vec<Value>, String> {
    let spec = arg
        .as_object()
        .ok_or_else(|| format!("$group 需要对象，收到 {}", type_name(arg)))?;
    let id_expr = spec
        .get("_id")
        .ok_or_else(|| "$group 缺少 _id（Mongo 要求显式分组键，全表单组写 null）".to_string())?;
    let mut accs: Vec<(&String, &str, &Value)> = Vec::new();
    for (alias, def) in spec {
        if alias == "_id" {
            continue;
        }
        let d = def
            .as_object()
            .ok_or_else(|| format!("$group 累积器 \"{alias}\" 的定义必须是对象"))?;
        if d.len() != 1 {
            return Err(format!("$group 累积器 \"{alias}\" 必须恰有一个算子键"));
        }
        let (op, expr) = d.iter().next().expect("len=1");
        match op.as_str() {
            "$sum" | "$avg" | "$min" | "$max" => accs.push((alias, op, expr)),
            other => {
                return Err(format!(
                    "不支持的 $group 累积器算子 {other}（本地求值器仅支持 $sum/$avg/$min/$max，$count 由 core 展开为 $sum，拒绝静默）"
                ));
            }
        }
    }

    // 首次出现顺序 + 规范化键 → 分组下标（数字 1 与 1.0 归一，对齐 Mongo 等值）
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
    for row in rows {
        let key = eval_expr(id_expr, &row, vars)?;
        let slot = match index.get(&canonical(&key)) {
            Some(i) => *i,
            None => {
                index.insert(canonical(&key), groups.len());
                groups.push((key, Vec::new()));
                groups.len() - 1
            }
        };
        groups[slot].1.push(row);
    }

    let mut out = Vec::with_capacity(groups.len());
    for (key, members) in groups {
        let mut doc = Map::new();
        doc.insert("_id".to_string(), key);
        for (alias, op, expr) in &accs {
            doc.insert((*alias).clone(), accumulate(op, expr, &members, vars)?);
        }
        out.push(Value::Object(doc));
    }
    Ok(out)
}

/// 累积器求值（`$sum` / `$avg` / `$min` / `$max`）。
///
/// - `$sum`：累加数值，忽略非数值与 `null`；无可用值 → `0`（空集 / `$count` 语义）；
/// - `$avg`：数值平均，忽略非数值与 `null`；无可用值 → `null`；
/// - `$min` / `$max`：按 BSON 序取极值，忽略 `null`（Mongo 语义）；无可用值 → `null`。
fn accumulate(
    op: &str,
    expr: &Value,
    rows: &[Value],
    vars: &Map<String, Value>,
) -> Result<Value, String> {
    match op {
        "$sum" => {
            let mut sum = 0.0;
            for row in rows {
                if let Some(n) = value::as_number(&eval_expr(expr, row, vars)?) {
                    sum += n;
                }
            }
            Ok(number_of(sum))
        }
        "$avg" => {
            let (mut sum, mut count) = (0.0_f64, 0_usize);
            for row in rows {
                if let Some(n) = value::as_number(&eval_expr(expr, row, vars)?) {
                    sum += n;
                    count += 1;
                }
            }
            if count == 0 {
                Ok(Value::Null)
            } else {
                Ok(float_of(sum / count as f64))
            }
        }
        "$min" | "$max" => {
            let mut best: Option<Value> = None;
            for row in rows {
                let v = eval_expr(expr, row, vars)?;
                if v.is_null() {
                    continue;
                }
                best = Some(match best {
                    None => v,
                    Some(cur) => {
                        let take = if op == "$min" {
                            value::compare(&v, &cur) == Ordering::Less
                        } else {
                            value::compare(&v, &cur) == Ordering::Greater
                        };
                        if take {
                            v
                        } else {
                            cur
                        }
                    }
                });
            }
            Ok(best.unwrap_or(Value::Null))
        }
        other => Err(format!("不支持的 $group 累积器算子 {other}")),
    }
}

/// 分组键规范化字符串：数字归一为 `f64`（`1` 与 `1.0` 同组），对象/数组递归。
fn canonical(v: &Value) -> String {
    match v {
        Value::Null => "z~".to_string(),
        Value::Bool(b) => format!("b~{b}"),
        Value::Number(n) => format!("n~{}", n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => format!("s~{}", serde_json::to_string(s).unwrap_or_default()),
        Value::Array(a) => {
            let inner: Vec<String> = a.iter().map(canonical).collect();
            format!("a~[{}]", inner.join(","))
        }
        Value::Object(o) => {
            let inner: Vec<String> = o
                .iter()
                .map(|(k, val)| format!("{}:{}", k, canonical(val)))
                .collect();
            format!("o~{{{}}}", inner.join(","))
        }
    }
}

/// 数值输出：整数值用整数形态（对齐 Mongo `$sum` 的 int 结果），否则浮点。
/// 供 `local/value.rs` 的数组聚合表达式算子（`$sum` 表达式形态）复用。
pub(crate) fn number_of(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0 {
        Value::from(f as i64)
    } else {
        float_of(f)
    }
}

pub(crate) fn float_of(f: f64) -> Value {
    serde_json::Number::from_f64(f)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// $project
// ---------------------------------------------------------------------------

/// 投影：全 0/1/bool → 复用 [`value::apply_projection`]；含表达式值 → 聚合投影
/// （`_id` 指令、包含式路径、排除式路径、表达式/字面量计算字段；非 `_id` 的包含与排除混用 → `Err`）。
fn project_row(doc: &Value, spec: &Value, vars: &Map<String, Value>) -> Result<Value, String> {
    let obj = spec
        .as_object()
        .ok_or_else(|| format!("$project 需要对象，收到 {}", type_name(spec)))?;
    if obj.is_empty() {
        return Ok(doc.clone());
    }
    if obj.values().all(|v| flag_of(v).is_some()) {
        return value::apply_projection(doc, spec);
    }

    let mut id: Option<bool> = None;
    let mut includes: Vec<&String> = Vec::new();
    let mut excludes: Vec<&String> = Vec::new();
    let mut computed: Vec<(&String, &Value)> = Vec::new();
    for (k, v) in obj {
        match flag_of(v) {
            Some(b) if k == "_id" => id = Some(b),
            Some(true) => includes.push(k),
            Some(false) => excludes.push(k),
            None => computed.push((k, v)),
        }
    }
    if !includes.is_empty() && !excludes.is_empty() {
        return Err("投影不允许同时包含与排除字段（`_id` 除外）".to_string());
    }

    if includes.is_empty() && computed.is_empty() {
        // 纯排除式（含 `_id: 0`）
        let mut out = doc.clone();
        for p in &excludes {
            value::remove_path(&mut out, p);
        }
        if id == Some(false) {
            if let Value::Object(m) = &mut out {
                m.remove("_id");
            }
        }
        return Ok(out);
    }

    let mut out = Map::new();
    if id != Some(false) {
        if let Some(v) = doc.get("_id") {
            out.insert("_id".to_string(), v.clone());
        }
    }
    for p in &includes {
        if let Some(v) = value::project_include(doc, p) {
            value::merge_into(&mut out, v);
        }
    }
    for (k, expr) in &computed {
        let v = eval_expr(expr, doc, vars)?;
        out.insert((*k).clone(), v);
    }
    Ok(Value::Object(out))
}

/// 投影值是否为 0/1/bool 指令。
fn flag_of(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(0), _) | (_, Some(0.0)) => Some(false),
            (Some(1), _) | (_, Some(1.0)) => Some(true),
            _ => None,
        },
        _ => None,
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stages(v: Value) -> Vec<Value> {
        v.as_array().cloned().expect("阶段数组")
    }

    /// 总纲 E4 的**原样命令**（`$match + $lookup(let/$$var/$expr) + $unwind + $project`）。
    #[test]
    fn golden_e4_relation_command() {
        let collections = json!({
            "probeUsers": [
                {"_id": "pu_MUZQP2NUB5ZZT9I3", "age": 36, "name": "Ada"},
                {"_id": "pu_MUZQP2NX8PAQRG49", "age": 41, "name": "Bob"}
            ],
            "probePosts": [
                {"_id": "pp_MUZQP2NZVRYXQ18O", "title": "P1", "userId": "pu_MUZQP2NUB5ZZT9I3"},
                {"_id": "pp_MUZQP2NZ4F3KLEYP", "title": "P2", "userId": "pu_MUZQP2NX8PAQRG49"}
            ]
        });
        let out = run(
            &collections,
            "probePosts",
            &stages(json!([
                {"$match": {}},
                {"$lookup": {
                    "as": "author",
                    "from": "probeUsers",
                    "let": {"rel_userId": {"$ifNull": ["$userId", null]}},
                    "pipeline": [
                        {"$match": {"$expr": {"$eq": ["$_id", "$$rel_userId"]}}},
                        {"$project": {"_id": 1, "name": 1}}
                    ]
                }},
                {"$unwind": {"path": "$author", "preserveNullAndEmptyArrays": true}},
                {"$project": {"_id": 1, "author": 1, "title": 1}}
            ])),
        )
        .unwrap();
        assert_eq!(
            Value::Array(out),
            json!([
                {"_id": "pp_MUZQP2NZVRYXQ18O", "author": {"_id": "pu_MUZQP2NUB5ZZT9I3", "name": "Ada"}, "title": "P1"},
                {"_id": "pp_MUZQP2NZ4F3KLEYP", "author": {"_id": "pu_MUZQP2NX8PAQRG49", "name": "Bob"}, "title": "P2"}
            ])
        );
    }

    #[test]
    fn match_expr_and_condition() {
        let collections = json!({"t": [
            {"_id": 1, "a": 1, "b": 2},
            {"_id": 2, "a": 1, "b": 9}
        ]});
        let out = run(
            &collections,
            "t",
            &stages(json!([{"$match": {"$and": [
                {"a": 1},
                {"$expr": {"$eq": ["$b", 2]}}
            ]}}])),
        )
        .unwrap();
        assert_eq!(Value::Array(out), json!([{"_id": 1, "a": 1, "b": 2}]));
        // `$expr` 必须求值为布尔，否则 Err
        assert!(run(
            &collections,
            "t",
            &stages(json!([{"$match": {"$expr": "$b"}}]))
        )
        .is_err());
    }

    #[test]
    fn missing_collection_is_empty() {
        let out = run(&json!({}), "nowhere", &[]).unwrap();
        assert!(out.is_empty());
        assert!(run(&json!({"t": 1}), "t", &[]).is_err());
    }

    #[test]
    fn sort_skip_limit_and_null_order() {
        let collections = json!({"t": [
            {"_id": 1, "v": 3},
            {"_id": 2, "v": null},
            {"_id": 3, "v": 1},
            {"_id": 4}
        ]});
        let asc = run(&collections, "t", &stages(json!([{"$sort": {"v": 1}}]))).unwrap();
        assert_eq!(
            Value::Array(asc),
            json!([{"_id": 2, "v": null}, {"_id": 4}, {"_id": 3, "v": 1}, {"_id": 1, "v": 3}])
        );
        let desc = run(&collections, "t", &stages(json!([{"$sort": {"v": -1}}]))).unwrap();
        assert_eq!(
            Value::Array(desc),
            json!([{"_id": 1, "v": 3}, {"_id": 3, "v": 1}, {"_id": 2, "v": null}, {"_id": 4}])
        );
        let paged = run(
            &collections,
            "t",
            &stages(json!([{"$sort": {"v": 1}}, {"$skip": 1}, {"$limit": 2}])),
        )
        .unwrap();
        assert_eq!(Value::Array(paged), json!([{"_id": 4}, {"_id": 3, "v": 1}]));
        // 数组字段取最小元素排序
        let arr = run(
            &json!({"t": [{"_id": 1, "v": [5, 1]}, {"_id": 2, "v": [2]}]}),
            "t",
            &stages(json!([{"$sort": {"v": 1}}])),
        )
        .unwrap();
        assert_eq!(
            Value::Array(arr),
            json!([{"_id": 1, "v": [5, 1]}, {"_id": 2, "v": [2]}])
        );
    }

    #[test]
    fn sort_array_form_and_negative_skip_error() {
        let collections = json!({"t": [{"_id": 1, "v": 2}, {"_id": 2, "v": 1}]});
        let out = run(&collections, "t", &stages(json!([{"$sort": [["v", -1]]}]))).unwrap();
        assert_eq!(
            Value::Array(out),
            json!([{"_id": 1, "v": 2}, {"_id": 2, "v": 1}])
        );
        assert!(run(&collections, "t", &stages(json!([{"$sort": 5}]))).is_err());
        assert!(run(&collections, "t", &stages(json!([{"$skip": -1}]))).is_err());
    }

    #[test]
    fn unwind_drop_and_preserve() {
        let collections = json!({"t": [
            {"_id": 1, "tags": ["a", "b"]},
            {"_id": 2, "tags": []},
            {"_id": 3},
            {"_id": 4, "tags": null},
            {"_id": 5, "tags": "solo"}
        ]});
        let dropped = run(
            &collections,
            "t",
            &stages(json!([{"$unwind": {"path": "$tags"}}])),
        )
        .unwrap();
        assert_eq!(
            Value::Array(dropped),
            json!([{"_id": 1, "tags": "a"}, {"_id": 1, "tags": "b"}, {"_id": 5, "tags": "solo"}])
        );
        let kept = run(
            &collections,
            "t",
            &stages(json!([{"$unwind": {"path": "tags", "preserveNullAndEmptyArrays": true}}])),
        )
        .unwrap();
        assert_eq!(
            Value::Array(kept),
            json!([
                {"_id": 1, "tags": "a"}, {"_id": 1, "tags": "b"},
                {"_id": 2}, {"_id": 3}, {"_id": 4}, {"_id": 5, "tags": "solo"}
            ])
        );
    }

    #[test]
    fn lookup_local_and_foreign_fields() {
        let collections = json!({
            "users": [{"_id": "u1", "name": "A"}, {"_id": "u2", "name": "B"}],
            "posts": [{"_id": "p1", "uid": "u1"}, {"_id": "p2", "uid": "zz"}, {"_id": "p3"}]
        });
        let out = run(
            &collections,
            "posts",
            &stages(json!([{"$lookup": {"from": "users", "localField": "uid", "foreignField": "_id", "as": "author"}}])),
        )
        .unwrap();
        assert_eq!(
            Value::Array(out),
            json!([
                {"_id": "p1", "uid": "u1", "author": [{"_id": "u1", "name": "A"}]},
                {"_id": "p2", "uid": "zz", "author": []},
                {"_id": "p3", "author": []}
            ])
        );
        // 既无 localField 也无 pipeline → Err
        assert!(run(
            &collections,
            "posts",
            &stages(json!([{"$lookup": {"from": "users", "as": "a"}}]))
        )
        .is_err());
    }

    #[test]
    fn group_by_key_with_sum_avg_min_max() {
        let collections = json!({"orders": [
            {"_id": 1, "status": "paid", "amount": 10},
            {"_id": 2, "status": "paid", "amount": 5},
            {"_id": 3, "status": "open", "amount": 7},
            {"_id": 4, "status": "open", "amount": null}
        ]});
        let out = run(
            &collections,
            "orders",
            &stages(json!([
                {"$group": {
                    "_id": "$status",
                    "n": {"$sum": 1},
                    "total": {"$sum": "$amount"},
                    "avg": {"$avg": "$amount"},
                    "lo": {"$min": "$amount"},
                    "hi": {"$max": "$amount"}
                }},
                {"$sort": {"_id": 1}}
            ])),
        )
        .unwrap();
        assert_eq!(
            Value::Array(out),
            json!([
                {"_id": "open", "n": 2, "total": 7, "avg": 7.0, "lo": 7, "hi": 7},
                {"_id": "paid", "n": 2, "total": 15, "avg": 7.5, "lo": 5, "hi": 10}
            ])
        );
    }

    #[test]
    fn group_multi_key_and_count_distinct_field() {
        let collections = json!({"t": [
            {"_id": 1, "a": 1, "b": "x", "v": 1},
            {"_id": 2, "a": 1, "b": "x"},
            {"_id": 3, "a": 1, "b": "y", "v": 0},
            {"_id": 4, "a": 1, "b": "y", "v": null}
        ]});
        // 多键分组（`_id` 嵌套对象）+ `$count` 字段形态（core 展开为 $sum($cond)）
        let out = run(
            &collections,
            "t",
            &stages(json!([
                {"$group": {"_id": {"a": "$a", "b": "$b"}, "present": {"$sum": {"$cond": [{"$eq": [{"$ifNull": ["$v", null]}, null]}, 0, 1]}}}},
                {"$sort": {"_id.b": 1}}
            ])),
        )
        .unwrap();
        assert_eq!(
            Value::Array(out),
            json!([
                {"_id": {"a": 1, "b": "x"}, "present": 1},
                {"_id": {"a": 1, "b": "y"}, "present": 1}
            ])
        );
    }

    /// 无 `by` 键的全表单组：core 发射 `$group(_id:null)` + 空集护栏（`$facet` + `$replaceRoot`）。
    #[test]
    fn group_full_table_empty_input_guard() {
        let pipeline = stages(json!([
            {"$group": {"_id": null, "n": {"$sum": 1}}},
            {"$facet": {"__rows": []}},
            {"$replaceRoot": {"newRoot": {"$ifNull": [{"$arrayElemAt": ["$__rows", 0]}, {"_id": null, "n": 0}]}}}
        ]));
        let non_empty = run(&json!({"t": [{"_id": 1}, {"_id": 2}]}), "t", &pipeline).unwrap();
        assert_eq!(Value::Array(non_empty), json!([{"_id": null, "n": 2}]));
        let empty = run(&json!({"t": []}), "t", &pipeline).unwrap();
        assert_eq!(Value::Array(empty), json!([{"_id": null, "n": 0}]));
    }

    #[test]
    fn add_fields_and_set() {
        let collections = json!({"t": [{"_id": 1, "a": 2}, {"_id": 2}]});
        let out = run(
            &collections,
            "t",
            &stages(json!([
                {"$addFields": {"b": {"$ifNull": ["$a", 0]}, "nested.c": 1}}
            ])),
        )
        .unwrap();
        assert_eq!(
            Value::Array(out),
            json!([{"_id": 1, "a": 2, "b": 2, "nested": {"c": 1}}, {"_id": 2, "b": 0, "nested": {"c": 1}}])
        );
        // `$set` 与 `$addFields` 同义
        let out2 = run(&collections, "t", &stages(json!([{"$set": {"b": 9}}]))).unwrap();
        assert_eq!(
            Value::Array(out2),
            json!([{"_id": 1, "a": 2, "b": 9}, {"_id": 2, "b": 9}])
        );
    }

    #[test]
    fn project_include_exclude_and_expression() {
        let collections = json!({"t": [{"_id": "x", "n": 5, "drop": true}]});
        // 纯指令 → 复用 apply_projection 语义（包含式默认带 _id）
        let inc = run(&collections, "t", &stages(json!([{"$project": {"n": 1}}]))).unwrap();
        assert_eq!(Value::Array(inc), json!([{"_id": "x", "n": 5}]));
        // 表达式投影（core `$group` 末段投影的 `"$_id"` 形态）
        let expr = run(
            &collections,
            "t",
            &stages(json!([{"$project": {"_id": 0, "status": "$_id", "n": 1}}])),
        )
        .unwrap();
        assert_eq!(Value::Array(expr), json!([{"status": "x", "n": 5}]));
        // 非 _id 的包含与排除混用 → Err
        assert!(run(
            &collections,
            "t",
            &stages(json!([{"$project": {"n": 1, "drop": 0}}]))
        )
        .is_err());
    }

    #[test]
    fn pipeline_error_forms() {
        let collections = json!({"t": [{"_id": 1}]});
        // 未知阶段（含写副作用阶段）
        assert!(run(&collections, "t", &stages(json!([{"$out": "x"}]))).is_err());
        assert!(run(&collections, "t", &stages(json!([{"$count": "n"}]))).is_err());
        // 阶段非单键
        assert!(run(
            &collections,
            "t",
            &stages(json!([{"$match": {}, "$limit": 1}]))
        )
        .is_err());
        assert!(run(&collections, "t", &stages(json!(["$match"]))).is_err());
        // `$replaceRoot` 结果非文档 → Err
        assert!(run(
            &collections,
            "t",
            &stages(json!([{"$replaceRoot": {"newRoot": 5}}]))
        )
        .is_err());
        // `$group` 缺 `_id` / 未知累积器 → Err
        assert!(run(
            &collections,
            "t",
            &stages(json!([{"$group": {"n": {"$sum": 1}}}]))
        )
        .is_err());
        assert!(run(
            &collections,
            "t",
            &stages(json!([{"$group": {"_id": null, "p": {"$push": "$x"}}}]))
        )
        .is_err());
        // `$facet` 子管道非数组 → Err
        assert!(run(&collections, "t", &stages(json!([{"$facet": 1}]))).is_err());
        // `$unwind` 缺 path → Err
        assert!(run(&collections, "t", &stages(json!([{"$unwind": {}}]))).is_err());
    }
}
