//! 本地磁盘数据源 —— 值工具（点号路径取值 / Mongo BSON 类型序比较 / 极小表达式 / 投影）。
//!
//! 语义基准 = **MongoDB 驱动语义**（local 与 mongo 走同一条命令路径，必须同结果）。
//! 纯逻辑：无 IO / 无时钟 / 无随机（确定性是双端对拍的前提）。
//!
//! 三态契约：`显式 null`（`Value::Null`）与`字段缺失`是两种不同状态 ——
//! [`resolve_path_candidates`] 以「空 = 缺失、含 `Null` = 显式 null」承载该区分。

use std::cmp::Ordering;

use serde_json::{Map, Value};

use crate::bson;

/// 单集合文档数护栏：超限一律 `Err`（禁静默截断，见执行文档 §4.1 / §8.4）。
pub const MAX_LOCAL_COLLECTION_DOCS: usize = 100_000;

// ---------------------------------------------------------------------------
// Mongo BSON 类型序权重（升序；跨类型比较的第一基准）
// ---------------------------------------------------------------------------

const RANK_MINKEY: u8 = 1;
const RANK_NULL: u8 = 2;
const RANK_NUMBER: u8 = 3;
const RANK_STRING: u8 = 4;
const RANK_OBJECT: u8 = 5;
const RANK_ARRAY: u8 = 6;
const RANK_BINARY: u8 = 7;
const RANK_OID: u8 = 8;
const RANK_BOOL: u8 = 9;
const RANK_DATE: u8 = 10;
const RANK_TIMESTAMP: u8 = 11;
const RANK_REGEX: u8 = 12;
const RANK_MAXKEY: u8 = 13;

/// 把点号路径拆段（`a.b.c` → `["a","b","c"]`；空路径 → 空段列表）。
fn segments(path: &str) -> Vec<&str> {
    if path.is_empty() {
        Vec::new()
    } else {
        path.split('.').collect()
    }
}

// ---------------------------------------------------------------------------
// 点号路径取值
// ---------------------------------------------------------------------------

/// 严格点号取值（**不展开数组**）：路径任一段缺失 → `None`；显式 `null` → `Some(&Value::Null)`。
///
/// 「缺失」与「显式 null」由此区分（三态契约的基础）。空路径 → 整文档。
/// 数组下钻语义由 [`resolve_path_candidates`] 承担。
pub fn get_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = doc;
    for seg in segments(path) {
        cur = match cur {
            Value::Object(o) => o.get(seg)?,
            // 严格取值：遇数组/标量不再下钻
            _ => return None,
        };
    }
    Some(cur)
}

/// 解析字段路径在文档上的**全部候选值**（Mongo 过滤命中语义，含数组下钻）。
///
/// 规则（对齐 MongoDB 路径匹配）：
/// - 对象 → 取键、消费一段；
/// - 数组 → **不消费段**，对每个元素继续解析（用于 `a.b` 中 `a` 为文档数组）；
/// - 段缺失 / 非对象非数组 → 该分支终止、不产出。
///
/// 返回：空 = 路径缺失（对应 `$exists:false` / 不命中）；含 `Value::Null` = 存在显式 null。
/// 仅返回末段解析到的值本身（数组字段返回该数组整体，元素命中由调用方按 [`matches_value`] 判定）。
pub fn resolve_path_candidates<'a>(doc: &'a Value, path: &str) -> Vec<&'a Value> {
    let mut out = Vec::new();
    collect_candidates(doc, &segments(path), &mut out);
    out
}

fn collect_candidates<'a>(v: &'a Value, segs: &[&str], out: &mut Vec<&'a Value>) {
    if segs.is_empty() {
        out.push(v);
        return;
    }
    match v {
        Value::Object(o) => {
            if let Some(next) = o.get(segs[0]) {
                collect_candidates(next, &segs[1..], out);
            }
        }
        Value::Array(a) => {
            for el in a {
                collect_candidates(el, segs, out);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Mongo BSON 类型序比较
// ---------------------------------------------------------------------------

/// BSON 类型序权重：跨类型比较时先比权重（`MinKey < Null < Number < String < Object < Array < …`）。
///
/// 扩展类型（`$numberDecimal` / `$numberLong`）归入 Number；`$minKey` / `$maxKey` / `$timestamp` /
/// `$regularExpression` 各归其序。
pub fn bson_rank(v: &Value) -> u8 {
    match v {
        Value::Null => RANK_NULL,
        Value::Bool(_) => RANK_BOOL,
        Value::Number(_) => RANK_NUMBER,
        Value::String(_) => RANK_STRING,
        Value::Array(_) => RANK_ARRAY,
        Value::Object(o) => extended_kind(o).unwrap_or(RANK_OBJECT),
    }
}

/// 单键扩展类型对象 → 其 BSON 序权重；非扩展对象 → `None`（按普通文档处理）。
fn extended_kind(o: &Map<String, Value>) -> Option<u8> {
    if o.len() != 1 {
        return None;
    }
    match o.keys().next()?.as_str() {
        "$minKey" => Some(RANK_MINKEY),
        "$maxKey" => Some(RANK_MAXKEY),
        "$numberDecimal" | "$numberLong" => Some(RANK_NUMBER),
        "$oid" => Some(RANK_OID),
        "$date" => Some(RANK_DATE),
        "$timestamp" => Some(RANK_TIMESTAMP),
        "$binary" => Some(RANK_BINARY),
        "$regularExpression" => Some(RANK_REGEX),
        _ => None,
    }
}

/// Mongo BSON 序比较（`Ordering::Less/Greater/Equal`）。
///
/// 数字间按数值比较（整型/浮点/扩展 Long 统一，`1` 与 `1.0` 相等）；
/// 字符串按字节序；文档逐键（键名 → 键值）比较，前缀相等时短者小；数组逐元素、前缀相等时短者小。
pub fn compare(a: &Value, b: &Value) -> Ordering {
    let (ra, rb) = (bson_rank(a), bson_rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match ra {
        RANK_NUMBER => compare_numbers(a, b),
        RANK_STRING => a.as_str().unwrap_or("").cmp(b.as_str().unwrap_or("")),
        RANK_BOOL => bool_rank(a).cmp(&bool_rank(b)),
        RANK_OBJECT => compare_objects(
            a.as_object().expect("rank=object"),
            b.as_object().expect("rank=object"),
        ),
        RANK_ARRAY => compare_arrays(
            a.as_array().expect("rank=array"),
            b.as_array().expect("rank=array"),
        ),
        RANK_OID => oid_str(a).cmp(oid_str(b)),
        RANK_DATE => date_ms(a).cmp(&date_ms(b)),
        RANK_TIMESTAMP => ts_parts(a).cmp(&ts_parts(b)),
        RANK_BINARY => compare_binary(a, b),
        RANK_REGEX => regex_parts(a).cmp(&regex_parts(b)),
        // Null / MinKey / MaxKey：同类型彼此相等
        _ => Ordering::Equal,
    }
}

/// 等值判定（= BSON 序比较为 `Equal`）。数字跨整型/浮点相等；文档按结构（键序由 serde_json 归一）。
pub fn values_equal(a: &Value, b: &Value) -> bool {
    compare(a, b) == Ordering::Equal
}

/// Mongo 等值命中：`actual` 等于 `expected`，或 `actual` 为**数组**且任一元素等于 `expected`。
///
/// 供过滤层 `{f: v}` / `$eq` 的「数组字段任一元素命中」语义复用。
pub fn matches_value(actual: &Value, expected: &Value) -> bool {
    if values_equal(actual, expected) {
        return true;
    }
    match actual {
        Value::Array(a) => a.iter().any(|el| values_equal(el, expected)),
        _ => false,
    }
}

fn compare_numbers(a: &Value, b: &Value) -> Ordering {
    numeric_value(a)
        .partial_cmp(&numeric_value(b))
        .unwrap_or(Ordering::Equal)
}

/// 数字的数值形式（普通 JSON number / 扩展 `$numberLong` / `$numberDecimal` 统一为 f64）。
fn numeric_value(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::Object(o) => {
            if let Some(s) = o.get("$numberLong").and_then(Value::as_str) {
                s.parse::<f64>().unwrap_or(0.0)
            } else if let Some(s) = o.get("$numberDecimal").and_then(Value::as_str) {
                s.parse::<f64>().unwrap_or(0.0)
            } else {
                0.0
            }
        }
        _ => 0.0,
    }
}

fn bool_rank(v: &Value) -> u8 {
    u8::from(v.as_bool().unwrap_or(false))
}

fn compare_objects(a: &Map<String, Value>, b: &Map<String, Value>) -> Ordering {
    let mut ai = a.iter();
    let mut bi = b.iter();
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some((ka, va)), Some((kb, vb))) => {
                let key_cmp = ka.cmp(kb);
                if key_cmp != Ordering::Equal {
                    return key_cmp;
                }
                let val_cmp = compare(va, vb);
                if val_cmp != Ordering::Equal {
                    return val_cmp;
                }
            }
        }
    }
}

fn compare_arrays(a: &[Value], b: &[Value]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let c = compare(x, y);
        if c != Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

fn oid_str(v: &Value) -> &str {
    v.get("$oid").and_then(Value::as_str).unwrap_or("")
}

fn date_ms(v: &Value) -> i64 {
    v.get("$date").and_then(Value::as_i64).unwrap_or(0)
}

fn ts_parts(v: &Value) -> (i64, i64) {
    match v.get("$timestamp").and_then(Value::as_object) {
        Some(o) => (
            o.get("t").and_then(Value::as_i64).unwrap_or(0),
            o.get("i").and_then(Value::as_i64).unwrap_or(0),
        ),
        None => (0, 0),
    }
}

fn compare_binary(a: &Value, b: &Value) -> Ordering {
    // Mongo 序：先按数据长度、再按子类型、再按字节（base64 长度与数据长度成比例，用作长度基准）
    let (a_b64, a_sub) = binary_parts(a);
    let (b_b64, b_sub) = binary_parts(b);
    a_b64
        .len()
        .cmp(&b_b64.len())
        .then_with(|| a_sub.cmp(&b_sub))
        .then_with(|| a_b64.cmp(&b_b64))
}

fn binary_parts(v: &Value) -> (String, String) {
    match v.get("$binary").and_then(Value::as_object) {
        Some(o) => (
            o.get("base64")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            o.get("subType")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        None => (String::new(), String::new()),
    }
}

fn regex_parts(v: &Value) -> (String, String) {
    match v.get("$regularExpression").and_then(Value::as_object) {
        Some(o) => (
            o.get("pattern")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            o.get("options")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        None => (String::new(), String::new()),
    }
}

// ---------------------------------------------------------------------------
// 极小聚合表达式求值（`$expr` / `$lookup.pipeline` / `$group` 键 / `$project` 计算字段）
// ---------------------------------------------------------------------------

/// 求值极小聚合表达式。
///
/// 支持：
/// - 字面量（数字 / 字符串 / 布尔 / null / 数组 / 扩展类型对象）原样返回；
/// - `"$field.path"`：从 `doc` 严格取值，缺失 → `null`；
/// - `"$$var"` / `"$$var.path"`：从 `vars` 取值，缺失 → `null`（供 `$lookup.let` 注入）；
/// - `{ "$eq": [a, b] }`、`{ "$and": [...] }`、`{ "$or": [...] }`、`{ "$ifNull": [a, b] }`；
/// - 普通对象 `{k: <expr>, …}` 逐键求值构造新文档；普通数组逐元素求值。
///
/// 未知算子一律 `Err`（禁静默）。
pub fn eval_expr(expr: &Value, doc: &Value, vars: &Map<String, Value>) -> Result<Value, String> {
    match expr {
        Value::String(s) => eval_ref(s, doc, vars),
        Value::Array(a) => a
            .iter()
            .map(|e| eval_expr(e, doc, vars))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(o) => {
            // 扩展类型对象（`$oid` 等）原样返回，避免被误判为算子
            if bson::is_extended(expr) {
                return Ok(expr.clone());
            }
            if o.len() == 1 {
                if let Some(op) = o.keys().next().map(String::as_str) {
                    if let Some(stripped) = op.strip_prefix('$') {
                        return eval_operator(stripped, o.values().next().expect("len=1"), doc, vars);
                    }
                }
            }
            let mut out = Map::new();
            for (k, v) in o {
                out.insert(k.clone(), eval_expr(v, doc, vars)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

fn eval_operator(
    op: &str,
    arg: &Value,
    doc: &Value,
    vars: &Map<String, Value>,
) -> Result<Value, String> {
    match op {
        "eq" => {
            let (a, b) = two_operands("$eq", arg)?;
            Ok(Value::Bool(values_equal(
                &eval_expr(a, doc, vars)?,
                &eval_expr(b, doc, vars)?,
            )))
        }
        "and" => {
            for e in array_operands("$and", arg)? {
                if !truthy(&eval_expr(e, doc, vars)?) {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }
        "or" => {
            for e in array_operands("$or", arg)? {
                if truthy(&eval_expr(e, doc, vars)?) {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        }
        "ifNull" => {
            let (a, b) = two_operands("$ifNull", arg)?;
            let v = eval_expr(a, doc, vars)?;
            if v.is_null() {
                eval_expr(b, doc, vars)
            } else {
                Ok(v)
            }
        }
        other => Err(format!(
            "不支持的聚合表达式算子: ${other}（本地求值器仅支持 $eq/$and/$or/$ifNull，拒绝静默）"
        )),
    }
}

fn two_operands<'a>(name: &str, arg: &'a Value) -> Result<(&'a Value, &'a Value), String> {
    let arr = arg
        .as_array()
        .ok_or_else(|| format!("{name} 需要操作数数组"))?;
    if arr.len() != 2 {
        return Err(format!("{name} 需要恰好两个操作数，收到 {}", arr.len()));
    }
    Ok((&arr[0], &arr[1]))
}

fn array_operands<'a>(name: &str, arg: &'a Value) -> Result<&'a [Value], String> {
    arg.as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{name} 需要操作数数组"))
}

/// Mongo 聚合布尔真值：`false` / `null` / `0` 为假，其余为真。
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
        _ => true,
    }
}

/// `"$field"` / `"$$var"` 引用求值；非 `$` 前缀的字符串为字面量。
fn eval_ref(s: &str, doc: &Value, vars: &Map<String, Value>) -> Result<Value, String> {
    if let Some(rest) = s.strip_prefix("$$") {
        let (name, path) = match rest.split_once('.') {
            Some((n, p)) => (n, p),
            None => (rest, ""),
        };
        let base = vars.get(name).cloned().unwrap_or(Value::Null);
        Ok(reference_path(&base, path))
    } else if let Some(rest) = s.strip_prefix('$') {
        Ok(get_path(doc, rest).cloned().unwrap_or(Value::Null))
    } else {
        Ok(Value::String(s.to_string()))
    }
}

fn reference_path(base: &Value, path: &str) -> Value {
    if path.is_empty() {
        base.clone()
    } else {
        get_path(base, path).cloned().unwrap_or(Value::Null)
    }
}

// ---------------------------------------------------------------------------
// 投影（包含 / 排除；`_id` 可与包含式混用）
// ---------------------------------------------------------------------------

/// 应用投影：包含式 / 排除式；`_id` 特殊处理（包含式默认带 `_id`，`_id: 0` 可与之混用）。
///
/// 空投影对象 → 原样返回（视为无投影）。同时含非 `_id` 的包含与排除 → `Err`（禁静默）。
pub fn apply_projection(doc: &Value, projection: &Value) -> Result<Value, String> {
    let obj = projection
        .as_object()
        .ok_or_else(|| format!("投影必须是对象，收到 {}", type_name(projection)))?;
    if obj.is_empty() {
        return Ok(doc.clone());
    }

    let mut include_paths: Vec<&String> = Vec::new();
    let mut exclude_paths: Vec<&String> = Vec::new();
    let mut id: Option<bool> = None;
    for (k, v) in obj {
        let flag = projection_flag(v)?;
        if k == "_id" {
            id = Some(flag);
        } else if flag {
            include_paths.push(k);
        } else {
            exclude_paths.push(k);
        }
    }
    if !include_paths.is_empty() && !exclude_paths.is_empty() {
        return Err("投影不允许同时包含与排除字段（`_id` 除外）".to_string());
    }

    let include_mode = if !include_paths.is_empty() {
        true
    } else if !exclude_paths.is_empty() {
        false
    } else {
        // 仅出现 `_id` 指令：`_id:1` → 只保留 `_id`；`_id:0` → 全保留除 `_id`
        id == Some(true)
    };

    if include_mode {
        let mut out = Map::new();
        if id != Some(false) {
            if let Some(v) = doc.get("_id") {
                out.insert("_id".to_string(), v.clone());
            }
        }
        for path in &include_paths {
            if let Some(v) = project_included(doc, &segments(path)) {
                merge_into(&mut out, v);
            }
        }
        Ok(Value::Object(out))
    } else {
        let mut out = doc.clone();
        for path in &exclude_paths {
            remove_path(&mut out, &segments(path));
        }
        if id == Some(false) {
            if let Value::Object(m) = &mut out {
                m.remove("_id");
            }
        }
        Ok(out)
    }
}

/// 投影值 → 包含(bool=true) / 排除(bool=false)；仅接受 `0/1` 与 `false/true`。
fn projection_flag(v: &Value) -> Result<bool, String> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(0), _) | (_, Some(0.0)) => Ok(false),
            (Some(1), _) | (_, Some(1.0)) => Ok(true),
            _ => Err(format!("投影值必须是 0/1 或 false/true，收到 {n}")),
        },
        other => Err(format!(
            "不支持的投影形态: {other}（本地求值器仅支持包含/排除，拒绝静默）"
        )),
    }
}

/// 按剩余段对 `doc` 做包含投影，返回投影后的子树；路径不存在 → `None`。
/// 遇数组：逐元素按同段继续（Mongo 数组映射语义的简化），无命中项跳过。
fn project_included(doc: &Value, segs: &[&str]) -> Option<Value> {
    if segs.is_empty() {
        return Some(doc.clone());
    }
    match doc {
        Value::Object(o) => {
            let child = o.get(segs[0])?;
            let sub = project_included(child, &segs[1..])?;
            let mut m = Map::new();
            m.insert(segs[0].to_string(), sub);
            Some(Value::Object(m))
        }
        Value::Array(a) => {
            let mut arr = Vec::with_capacity(a.len());
            for el in a {
                if let Some(v) = project_included(el, segs) {
                    arr.push(v);
                }
            }
            Some(Value::Array(arr))
        }
        _ => None,
    }
}

/// 把投影子树并入输出文档；同名对象递归合并，其余覆盖。
fn merge_into(dst: &mut Map<String, Value>, src: Value) {
    let Value::Object(src) = src else {
        return;
    };
    for (k, v) in src {
        let nested = matches!(dst.get(&k), Some(Value::Object(_))) && v.is_object();
        if nested {
            if let Some(Value::Object(d)) = dst.get_mut(&k) {
                merge_into(d, v);
            }
        } else {
            dst.insert(k, v);
        }
    }
}

/// 按段做排除投影（原地）；遇数组对每个元素继续。
fn remove_path(doc: &mut Value, segs: &[&str]) {
    if segs.is_empty() {
        return;
    }
    match doc {
        Value::Object(o) => {
            if segs.len() == 1 {
                o.remove(segs[0]);
            } else if let Some(v) = o.get_mut(segs[0]) {
                remove_path(v, &segs[1..]);
            }
        }
        Value::Array(a) => {
            for el in a {
                remove_path(el, segs);
            }
        }
        _ => {}
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

    #[test]
    fn max_docs_guard_constant() {
        assert_eq!(MAX_LOCAL_COLLECTION_DOCS, 100_000);
    }

    #[test]
    fn get_path_three_states() {
        let doc = json!({ "a": null, "b": { "c": 1 } });
        // 显式 null：Some(Null)；缺失：None
        assert_eq!(get_path(&doc, "a"), Some(&Value::Null));
        assert_eq!(get_path(&doc, "missing"), None);
        assert_eq!(get_path(&doc, "b.missing"), None);
        assert_eq!(get_path(&doc, "b.c"), Some(&json!(1)));
        // 严格取值不下钻数组
        let arr = json!({ "a": [ { "b": 1 } ] });
        assert_eq!(get_path(&arr, "a.b"), None);
    }

    #[test]
    fn candidates_three_states_and_array_expansion() {
        let doc = json!({ "a": null, "arr": [ { "b": 1 }, { "b": 2 } ] });
        // 缺失 → 空；显式 null → [Null]
        assert!(resolve_path_candidates(&doc, "missing").is_empty());
        assert_eq!(resolve_path_candidates(&doc, "a"), vec![&Value::Null]);
        // 数组下钻：a.b 命中每个元素
        assert_eq!(
            resolve_path_candidates(&doc, "arr.b"),
            vec![&json!(1), &json!(2)]
        );
        // 末段为数组字段：候选为数组整体
        let tags = json!({ "tags": ["x", "y"] });
        assert_eq!(
            resolve_path_candidates(&tags, "tags"),
            vec![&json!(["x", "y"])]
        );
    }

    #[test]
    fn bson_type_order_across_types() {
        let min = json!({ "$minKey": 1 });
        let null = Value::Null;
        let num = json!(5);
        let s = json!("abc");
        let obj = json!({ "x": 1 });
        let arr = json!([1, 2]);
        let boolean = json!(true);
        let date = json!({ "$date": 0 });
        let max = json!({ "$maxKey": 1 });
        let seq = [&min, &null, &num, &s, &obj, &arr, &boolean, &date, &max];
        for (i, a) in seq.iter().enumerate() {
            for (j, b) in seq.iter().enumerate() {
                assert_eq!(compare(a, b), i.cmp(&j), "rank {i} vs {j}");
            }
        }
    }

    #[test]
    fn number_order_across_int_and_float() {
        assert_eq!(compare(&json!(1), &json!(1.0)), Ordering::Equal);
        assert!(values_equal(&json!(1), &json!(1.0)));
        assert_eq!(compare(&json!(2), &json!(1.5)), Ordering::Greater);
        assert_eq!(compare(&json!(-1), &json!(0)), Ordering::Less);
        // 扩展 Long 与普通数字同序
        assert!(values_equal(&json!({ "$numberLong": "3" }), &json!(3)));
    }

    #[test]
    fn compare_object_and_array() {
        assert_eq!(
            compare(&json!({ "a": 1 }), &json!({ "a": 2 })),
            Ordering::Less
        );
        // 前缀相等 → 短者小
        assert_eq!(
            compare(&json!({ "a": 2 }), &json!({ "a": 2, "b": 1 })),
            Ordering::Less
        );
        assert_eq!(compare(&json!([1, 2]), &json!([1, 2, 3])), Ordering::Less);
        assert_eq!(compare(&json!([1, 3]), &json!([1, 2])), Ordering::Greater);
    }

    #[test]
    fn array_eq_hit_helper() {
        // 数组字段等值命中（Mongo 语义）：候选为数组，元素命中即真
        assert!(matches_value(&json!(["x", "y"]), &json!("x")));
        assert!(matches_value(&json!("x"), &json!("x")));
        assert!(!matches_value(&json!(["y"]), &json!("x")));
    }

    #[test]
    fn eval_expr_minimal() {
        let doc = json!({ "x": 1, "y": 2 });
        let vars = Map::new();
        assert_eq!(
            eval_expr(&json!({ "$eq": ["$x", 1] }), &doc, &vars).unwrap(),
            json!(true)
        );
        assert_eq!(
            eval_expr(&json!({ "$eq": ["$x", "$y"] }), &doc, &vars).unwrap(),
            json!(false)
        );
        assert_eq!(
            eval_expr(
                &json!({ "$and": [ { "$eq": ["$x", 1] }, { "$eq": ["$y", 2] } ] }),
                &doc,
                &vars
            )
            .unwrap(),
            json!(true)
        );
        // $ifNull：缺失字段 → 回退值
        assert_eq!(
            eval_expr(&json!({ "$ifNull": ["$missing", "d"] }), &doc, &vars).unwrap(),
            json!("d")
        );
        // $$var 引用
        let mut vars2 = Map::new();
        vars2.insert("v".to_string(), json!(42));
        assert_eq!(eval_expr(&json!("$$v"), &doc, &vars2).unwrap(), json!(42));
        assert_eq!(
            eval_expr(&json!({ "$eq": ["$x", "$$v"] }), &doc, &vars2).unwrap(),
            json!(false)
        );
        // 未知算子 → Err（禁静默）
        assert!(eval_expr(&json!({ "$mul": [1, 2] }), &doc, &vars).is_err());
    }

    #[test]
    fn projection_include_nested() {
        let doc = json!({ "_id": 7, "a": { "b": 1, "c": 2 }, "d": 3 });
        // 嵌套包含：仅保留 a.b
        assert_eq!(
            apply_projection(&doc, &json!({ "a.b": 1 })).unwrap(),
            json!({ "_id": 7, "a": { "b": 1 } })
        );
        // 包含式默认带 _id
        assert_eq!(
            apply_projection(&doc, &json!({ "d": 1 })).unwrap(),
            json!({ "_id": 7, "d": 3 })
        );
        // _id:0 与包含式混用
        assert_eq!(
            apply_projection(&doc, &json!({ "d": 1, "_id": 0 })).unwrap(),
            json!({ "d": 3 })
        );
    }

    #[test]
    fn projection_exclude_and_errors() {
        let doc = json!({ "_id": 7, "a": { "b": 1, "c": 2 }, "d": 3 });
        // 嵌套排除
        assert_eq!(
            apply_projection(&doc, &json!({ "a.b": 0 })).unwrap(),
            json!({ "_id": 7, "a": { "c": 2 }, "d": 3 })
        );
        // 仅 _id:0 → 去掉 _id，其余保留
        assert_eq!(
            apply_projection(&doc, &json!({ "_id": 0 })).unwrap(),
            json!({ "a": { "b": 1, "c": 2 }, "d": 3 })
        );
        // 非 _id 的包含/排除混用 → Err
        assert!(apply_projection(&doc, &json!({ "a": 1, "d": 0 })).is_err());
        // 非法投影值 → Err
        assert!(apply_projection(&doc, &json!({ "a": 2 })).is_err());
    }
}