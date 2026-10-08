//! 本地磁盘数据源 —— 过滤谓词求值（`$match` / 查询 / 计数 / 删除 / 更新命中判定）。
//!
//! 语义基准 = **MongoDB 驱动语义**（local 与 mongo 走同一条命令路径，必须同结果）。
//! 纯逻辑：无 IO / 无时钟 / 无随机（确定性是双端对拍的前提）。
//!
//! 三态 null：`{f: null}` 与 `{f: {$eq: null}}` **只匹配显式 `null`**，不匹配字段缺失 ——
//! 由 [`resolve_path_candidates`] 的「空 = 缺失、含 `Null` = 显式 null」承载（见 [`super::value`]）。
//!
//! 数组字段：元素任一匹配即命中（`{tags: "x"}` 命中含 `"x"` 的数组），对齐 Mongo；
//! 数组下钻（`a.b` 中 `a` 为文档数组）复用 [`resolve_path_candidates`] 的候选展开。

use std::cmp::Ordering;

use regex::Regex;
use serde_json::{Map, Value};

use crate::bson;
use crate::local::value::{compare, eval_expr, matches_value, resolve_path_candidates};

// ---------------------------------------------------------------------------
// 顶层入口
// ---------------------------------------------------------------------------

/// 文档是否命中过滤条件（无变量上下文）。
///
/// - `Value::Null` / 空对象 `{}` → 匹配全部（与 mongo 路径「null 表示无过滤」一致）；
/// - 对象 → 逐键：逻辑组（`$and` / `$or` / `$nor`）、`$expr` 与字段条件**统一 AND**；
/// - 其余类型（字符串 / 数字 / 布尔 / 数组）→ `Err`（拒绝静默按「无过滤」处理，
///   对齐 SQL 侧 `dialect/filter` 的 B-10-1 精神）。
pub fn matches(doc: &Value, filter: &Value) -> Result<bool, String> {
    matches_with_vars(doc, filter, &Map::new())
}

/// 带变量上下文的过滤求值：`vars` 供 `$lookup` 子管道 `$match.$expr` 里的 `$$var` 引用。
///
/// 语义与 [`matches`] 完全一致，仅额外注入 `$$let` 变量（core 的关系 `$lookup` 子管道
/// 以 `{$expr: {$eq: ["$fk", "$$rel_x"]}}` 形态做外键匹配，故本层必须支持 `$expr`）。
pub fn matches_with_vars(
    doc: &Value,
    filter: &Value,
    vars: &Map<String, Value>,
) -> Result<bool, String> {
    match filter {
        Value::Null => Ok(true),
        Value::Object(map) => matches_object(doc, map, vars),
        other => Err(format!(
            "filter 必须是对象或 null，收到 {}（拒绝静默按「无过滤」处理）",
            type_name(other)
        )),
    }
}

/// 对象 filter：先逻辑组（固定顺序 `$and → $or → $nor`）与 `$expr`，再字段条件；任一为假即短路。
fn matches_object(
    doc: &Value,
    map: &Map<String, Value>,
    vars: &Map<String, Value>,
) -> Result<bool, String> {
    for op in ["$and", "$or", "$nor"] {
        if let Some(v) = map.get(op) {
            if !logical_hit(op, v, doc, vars)? {
                return Ok(false);
            }
        }
    }
    if let Some(expr) = map.get("$expr") {
        match eval_expr(expr, doc, vars)? {
            Value::Bool(b) => {
                if !b {
                    return Ok(false);
                }
            }
            other => {
                return Err(format!(
                    "$expr 必须求值为布尔，收到 {}（拒绝静默按真值处理）",
                    type_name(&other)
                ));
            }
        }
    }
    for (field, cond) in map {
        if let Some(op) = field.strip_prefix('$') {
            if matches!(op, "and" | "or" | "nor" | "expr") {
                continue;
            }
            return Err(format!(
                "不支持的过滤操作符: ${op}（本地求值器仅支持顶层 $and/$or/$nor/$expr，拒绝静默丢弃）"
            ));
        }
        if !field_matches(doc, field, cond)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 顶层逻辑组：`$and` 全真 / `$or` 任一真 / `$nor` 全假。
///
/// 空数组行为对齐 Mongo 定义：`$and: []` → `true`、`$or: []` → `false`；
/// `$nor: []` → `Err`（对齐 `dialect/filter/mod.rs` 第 179 行既有约定）。
fn logical_hit(
    op: &str,
    v: &Value,
    doc: &Value,
    vars: &Map<String, Value>,
) -> Result<bool, String> {
    let arr = v.as_array().ok_or_else(|| format!("{op} 需要数组"))?;
    match op {
        "$and" => {
            for f in arr {
                if !matches_with_vars(doc, f, vars)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        "$or" => {
            for f in arr {
                if matches_with_vars(doc, f, vars)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        "$nor" => {
            if arr.is_empty() {
                return Err("$nor 需要非空数组".to_string());
            }
            for f in arr {
                if matches_with_vars(doc, f, vars)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => unreachable!("仅由 matches_object 以 $and/$or/$nor 调用"),
    }
}

// ---------------------------------------------------------------------------
// 字段条件
// ---------------------------------------------------------------------------

/// 单字段条件命中：取字段候选值后按条件形态求值。
fn field_matches(doc: &Value, path: &str, cond: &Value) -> Result<bool, String> {
    let candidates = resolve_path_candidates(doc, path);
    cond_matches(&candidates, cond)
}

/// 条件求值（对给定候选值集合）：运算符对象 → 逐算子 AND；否则按**结构等值**。
///
/// 运算符对象判据（对齐 Mongo）：对象非空、非扩展类型字面量（`$oid`/`$date`/…）、
/// 且存在以 `$` 开头的键。
fn cond_matches(candidates: &[&Value], cond: &Value) -> Result<bool, String> {
    if let Value::Object(o) = cond {
        if !o.is_empty() && !bson::is_extended(cond) && o.keys().any(|k| k.starts_with('$')) {
            return operators_hit(candidates, o);
        }
    }
    Ok(equals_hit(candidates, cond))
}

/// 运算符对象：逐算子求值并 AND；未知算子一律 `Err`（禁静默丢弃）。
fn operators_hit(candidates: &[&Value], ops: &Map<String, Value>) -> Result<bool, String> {
    for (op, arg) in ops {
        let hit = match op.as_str() {
            "$eq" => equals_hit(candidates, arg),
            "$ne" => !equals_hit(candidates, arg),
            "$gt" => ordered_hit(candidates, arg, |o| o == Ordering::Greater),
            "$gte" => ordered_hit(candidates, arg, |o| o != Ordering::Less),
            "$lt" => ordered_hit(candidates, arg, |o| o == Ordering::Less),
            "$lte" => ordered_hit(candidates, arg, |o| o != Ordering::Greater),
            "$in" => in_hit(candidates, arg, false)?,
            "$nin" => in_hit(candidates, arg, true)?,
            "$exists" => exists_hit(candidates, arg),
            // `$not` = 同字段条件取反（含 `{$not: {$gt: n}}` 形态）
            "$not" => !cond_matches(candidates, arg)?,
            "$regex" => regex_hit(candidates, arg, ops.get("$options"))?,
            // `$options` 是 `$regex` 的修饰符（其语义由 `$regex` 分支合并处理）；
            // 单独出现（无 `$regex`）→ 修饰符无承载对象，**绝不静默丢弃**
            "$options" => {
                if ops.contains_key("$regex") {
                    continue;
                }
                return Err("$options 出现在没有 $regex 的条件中，该修饰符无法生效".to_string());
            }
            other => {
                return Err(format!(
                    "不支持的过滤条件操作符: {other}（本地求值器拒绝静默丢弃）"
                ));
            }
        };
        if !hit {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 等值命中（`$eq` / 裸标量）：候选任一命中即真；数组字段元素命中由 [`matches_value`] 承担。
fn equals_hit(candidates: &[&Value], expected: &Value) -> bool {
    candidates.iter().any(|c| matches_value(c, expected))
}

/// 序比较命中（`$gt` / `$gte` / `$lt` / `$lte`）：候选任一满足谓词即真。
///
/// - 期望值为标量、候选为数组 → **逐元素**比较（Mongo 数组字段语义）；
/// - 期望值为数组 → 整数组参与比较（并逐元素比较）。
fn ordered_hit(candidates: &[&Value], expected: &Value, pred: impl Fn(Ordering) -> bool) -> bool {
    candidates.iter().any(|c| match c {
        Value::Array(a) if !expected.is_array() => a.iter().any(|el| pred(compare(el, expected))),
        Value::Array(a) => {
            pred(compare(c, expected)) || a.iter().any(|el| pred(compare(el, expected)))
        }
        _ => pred(compare(c, expected)),
    })
}

/// `$in` / `$nin`：候选值命中列表任一元素（数组字段元素命中由 [`matches_value`] 承担）。
fn in_hit(candidates: &[&Value], arg: &Value, negate: bool) -> Result<bool, String> {
    let name = if negate { "$nin" } else { "$in" };
    let arr = arg
        .as_array()
        .ok_or_else(|| format!("{name} 需要数组，收到 {}", type_name(arg)))?;
    if arr.is_empty() {
        // 空 `$in` → 恒假；空 `$nin` → 恒真
        return Ok(negate);
    }
    let hit = candidates
        .iter()
        .any(|c| arr.iter().any(|item| matches_value(c, item)));
    Ok(if negate { !hit } else { hit })
}

/// `$exists`：字段存在（含显式 null）⇔ 候选集合非空；取值按 Mongo 真值语义解释。
fn exists_hit(candidates: &[&Value], arg: &Value) -> bool {
    !candidates.is_empty() == exists_flag(arg)
}

/// `$exists` 取值真值：`false` / `null` / `0` 为假，其余为真（对齐 Mongo）。
fn exists_flag(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
        _ => true,
    }
}

/// `$regex`（含同对象 `$options`）：候选字符串（或数组内字符串元素）任一匹配即真。
///
/// 仅匹配字符串值；非字符串候选不命中（不报错）。
fn regex_hit(
    candidates: &[&Value],
    pattern: &Value,
    options: Option<&Value>,
) -> Result<bool, String> {
    let pat = pattern.as_str().ok_or_else(|| {
        format!("$regex 需要字符串模式，收到 {}", type_name(pattern))
    })?;
    let flags = parse_regex_options(options)?;
    let re = compile_regex(pat, &flags)?;
    Ok(candidates.iter().any(|c| match c {
        Value::String(s) => re.is_match(s),
        Value::Array(a) => a
            .iter()
            .any(|el| el.as_str().map(|s| re.is_match(s)).unwrap_or(false)),
        _ => false,
    }))
}

/// 解析 `$options`：`i` / `m` / `s` 三种修饰符，其余一律 `Err`（禁静默降级）。
fn parse_regex_options(options: Option<&Value>) -> Result<String, String> {
    let Some(v) = options else {
        return Ok(String::new());
    };
    if v.is_null() {
        return Ok(String::new());
    }
    let s = v
        .as_str()
        .ok_or_else(|| format!("$options 需要字符串，收到 {}", type_name(v)))?;
    let mut out = String::new();
    for ch in s.chars() {
        match ch {
            'i' | 'm' | 's' => out.push(ch),
            other => {
                return Err(format!(
                    "不支持的 $regex 修饰符: '{other}'（本地求值器仅支持 i/m/s，拒绝静默）"
                ));
            }
        }
    }
    Ok(out)
}

/// 编译 `regex` crate 模式：修饰符转内联标志前缀（`i` → `(?i)`）。
fn compile_regex(pattern: &str, flags: &str) -> Result<Regex, String> {
    let full = if flags.is_empty() {
        pattern.to_string()
    } else {
        format!("(?{flags}){pattern}")
    };
    Regex::new(&full).map_err(|e| format!("$regex 模式非法: {pattern}（{e}）"))
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

    // ---- 裸标量等值 / 三态 null / 结构等值 ---------------------------------

    #[test]
    fn bare_scalar_equality() {
        let doc = json!({ "a": 1, "s": "x", "b": true });
        assert!(matches(&doc, &json!({ "a": 1 })).unwrap());
        assert!(!matches(&doc, &json!({ "a": 2 })).unwrap());
        assert!(matches(&doc, &json!({ "s": "x" })).unwrap());
        assert!(matches(&doc, &json!({ "b": true })).unwrap());
        // 数字跨整型/浮点相等
        assert!(matches(&json!({ "a": 1 }), &json!({ "a": 1.0 })).unwrap());
    }

    #[test]
    fn bare_null_three_states() {
        // 显式 null → 命中；字段缺失 → 不命中
        assert!(matches(&json!({ "a": null }), &json!({ "a": null })).unwrap());
        assert!(!matches(&json!({ "b": 1 }), &json!({ "a": null })).unwrap());
    }

    #[test]
    fn structural_object_equality() {
        let doc = json!({ "m": { "k": 1, "j": 2 } });
        assert!(matches(&doc, &json!({ "m": { "k": 1, "j": 2 } })).unwrap());
        assert!(!matches(&doc, &json!({ "m": { "k": 1 } })).unwrap());
        // 扩展类型字面量按结构等值（$oid）→ 不误判为算子
        let id = json!({ "$oid": "abc" });
        assert!(matches(&json!({ "_id": id.clone() }), &json!({ "_id": id })).unwrap());
    }

    #[test]
    fn array_field_element_hit() {
        // 数组字段：元素任一命中即命中
        assert!(matches(&json!({ "tags": ["x", "y"] }), &json!({ "tags": "x" })).unwrap());
        assert!(!matches(&json!({ "tags": ["y"] }), &json!({ "tags": "x" })).unwrap());
    }

    #[test]
    fn dotted_path_and_nested_array() {
        let doc = json!({ "meta": { "level": 3 } });
        assert!(matches(&doc, &json!({ "meta.level": 3 })).unwrap());
        // a.b：a 为文档数组 → 逐元素下钻
        let arr = json!({ "a": [ { "b": 1 }, { "b": 2 } ] });
        assert!(matches(&arr, &json!({ "a.b": 2 })).unwrap());
        assert!(!matches(&arr, &json!({ "a.b": 9 })).unwrap());
    }

    // ---- $eq / $ne ---------------------------------------------------------

    #[test]
    fn eq_ne_ops() {
        let doc = json!({ "a": 5 });
        assert!(matches(&doc, &json!({ "a": { "$eq": 5 } })).unwrap());
        assert!(!matches(&doc, &json!({ "a": { "$eq": 6 } })).unwrap());
        assert!(matches(&doc, &json!({ "a": { "$ne": 6 } })).unwrap());
        assert!(!matches(&doc, &json!({ "a": { "$ne": 5 } })).unwrap());
        // $ne 命中缺失字段（纯取反）
        assert!(matches(&json!({ "b": 1 }), &json!({ "a": { "$ne": 5 } })).unwrap());
    }

    #[test]
    fn eq_null_three_states() {
        assert!(matches(&json!({ "a": null }), &json!({ "a": { "$eq": null } })).unwrap());
        assert!(!matches(&json!({ "b": 1 }), &json!({ "a": { "$eq": null } })).unwrap());
        // $ne:null → 非显式 null（含缺失）命中
        assert!(matches(&json!({ "b": 1 }), &json!({ "a": { "$ne": null } })).unwrap());
        assert!(!matches(&json!({ "a": null }), &json!({ "a": { "$ne": null } })).unwrap());
    }

    // ---- $gt / $gte / $lt / $lte ------------------------------------------

    #[test]
    fn comparison_ops_and_cross_type() {
        let doc = json!({ "n": 10, "s": "m" });
        assert!(matches(&doc, &json!({ "n": { "$gt": 5 } })).unwrap());
        assert!(!matches(&doc, &json!({ "n": { "$gt": 10 } })).unwrap());
        assert!(matches(&doc, &json!({ "n": { "$gte": 10 } })).unwrap());
        assert!(matches(&doc, &json!({ "n": { "$lt": 11 } })).unwrap());
        assert!(matches(&doc, &json!({ "n": { "$lte": 10 } })).unwrap());
        // 跨类型按 BSON 序：字符串 > 数字
        assert!(matches(&doc, &json!({ "s": { "$gt": 5 } })).unwrap());
        // null < number
        assert!(matches(&json!({ "a": null }), &json!({ "a": { "$lt": 5 } })).unwrap());
        // 缺失字段 → 不命中
        assert!(!matches(&json!({ "x": 1 }), &json!({ "n": { "$gt": 5 } })).unwrap());
    }

    #[test]
    fn comparison_array_field_elementwise() {
        // 数组字段：任一元素满足即命中
        assert!(matches(&json!({ "n": [1, 10] }), &json!({ "n": { "$gt": 5 } })).unwrap());
        assert!(!matches(&json!({ "n": [1, 2] }), &json!({ "n": { "$gt": 5 } })).unwrap());
    }

    // ---- $in / $nin --------------------------------------------------------

    #[test]
    fn in_nin_ops() {
        let doc = json!({ "a": 3 });
        assert!(matches(&doc, &json!({ "a": { "$in": [1, 2, 3] } })).unwrap());
        assert!(!matches(&doc, &json!({ "a": { "$in": [1, 2] } })).unwrap());
        assert!(matches(&doc, &json!({ "a": { "$nin": [1, 2] } })).unwrap());
        // 数组字段：元素命中列表
        assert!(matches(&json!({ "t": ["x", "y"] }), &json!({ "t": { "$in": ["y"] } })).unwrap());
        // 缺失字段：$nin 命中
        assert!(matches(&json!({ "b": 1 }), &json!({ "a": { "$nin": [1] } })).unwrap());
    }

    #[test]
    fn in_nin_boundaries() {
        let doc = json!({ "a": 1 });
        // 空 $in → 恒假；空 $nin → 恒真
        assert!(!matches(&doc, &json!({ "a": { "$in": [] } })).unwrap());
        assert!(matches(&doc, &json!({ "a": { "$nin": [] } })).unwrap());
        // 非数组 → Err
        assert!(matches(&doc, &json!({ "a": { "$in": 1 } })).is_err());
    }

    // ---- $exists -----------------------------------------------------------

    #[test]
    fn exists_op() {
        let doc = json!({ "a": null, "b": 1 });
        assert!(matches(&doc, &json!({ "a": { "$exists": true } })).unwrap());
        assert!(!matches(&doc, &json!({ "a": { "$exists": false } })).unwrap());
        assert!(matches(&doc, &json!({ "missing": { "$exists": false } })).unwrap());
        assert!(!matches(&doc, &json!({ "missing": { "$exists": true } })).unwrap());
        // 显式 null 视为存在；取值按 Mongo 真值（1 → true、0 → false）
        assert!(matches(&doc, &json!({ "a": { "$exists": 1 } })).unwrap());
        assert!(!matches(&doc, &json!({ "b": { "$exists": 0 } })).unwrap());
    }

    // ---- $not --------------------------------------------------------------

    #[test]
    fn not_op() {
        let doc = json!({ "n": 3 });
        assert!(matches(&doc, &json!({ "n": { "$not": { "$gt": 5 } } })).unwrap());
        assert!(!matches(&json!({ "n": 10 }), &json!({ "n": { "$not": { "$gt": 5 } } })).unwrap());
        // 缺失字段 → 内层为假 → $not 为真
        assert!(matches(&json!({ "x": 1 }), &json!({ "n": { "$not": { "$gt": 5 } } })).unwrap());
    }

    // ---- $regex / $options -------------------------------------------------

    #[test]
    fn regex_basic_and_options() {
        let doc = json!({ "s": "Hello World" });
        assert!(matches(&doc, &json!({ "s": { "$regex": "^Hello" } })).unwrap());
        assert!(!matches(&doc, &json!({ "s": { "$regex": "^hello" } })).unwrap());
        // i → 大小写不敏感
        assert!(matches(&doc, &json!({ "s": { "$regex": "^hello", "$options": "i" } })).unwrap());
        // 非字符串候选 → 不命中（不报错）
        assert!(!matches(&json!({ "n": 123 }), &json!({ "n": { "$regex": "1" } })).unwrap());
    }

    #[test]
    fn regex_multiline_and_array_field() {
        // m → 多行：^ 匹配行首
        let doc = json!({ "s": "a\nb" });
        assert!(matches(&doc, &json!({ "s": { "$regex": "^b", "$options": "m" } })).unwrap());
        // 数组字段：元素任一匹配
        assert!(matches(
            &json!({ "t": ["xyz", "abc"] }),
            &json!({ "t": { "$regex": "^a" } })
        )
        .unwrap());
    }

    #[test]
    fn regex_error_forms() {
        let doc = json!({ "s": "x" });
        // 非法模式 → Err
        assert!(matches(&doc, &json!({ "s": { "$regex": "(" } })).is_err());
        // 非字符串模式 → Err
        assert!(matches(&doc, &json!({ "s": { "$regex": 1 } })).is_err());
        // 不支持的修饰符 → Err
        assert!(matches(&doc, &json!({ "s": { "$regex": "x", "$options": "z" } })).is_err());
        // $options 无 $regex → Err
        assert!(matches(&doc, &json!({ "s": { "$options": "i" } })).is_err());
    }

    // ---- $and / $or / $nor -------------------------------------------------

    #[test]
    fn logical_ops() {
        let doc = json!({ "a": 1, "b": 2 });
        assert!(matches(&doc, &json!({ "$and": [{ "a": 1 }, { "b": 2 }] })).unwrap());
        assert!(!matches(&doc, &json!({ "$and": [{ "a": 1 }, { "b": 3 }] })).unwrap());
        assert!(matches(&doc, &json!({ "$or": [{ "a": 9 }, { "b": 2 }] })).unwrap());
        assert!(!matches(&doc, &json!({ "$or": [{ "a": 9 }, { "b": 9 }] })).unwrap());
        assert!(matches(&doc, &json!({ "$nor": [{ "a": 9 }] })).unwrap());
        assert!(!matches(&doc, &json!({ "$nor": [{ "a": 1 }] })).unwrap());
    }

    #[test]
    fn logical_boundaries_and_combination() {
        let doc = json!({ "a": 1 });
        // 空数组：$and → true；$or → false；$nor → Err
        assert!(matches(&doc, &json!({ "$and": [] })).unwrap());
        assert!(!matches(&doc, &json!({ "$or": [] })).unwrap());
        assert!(matches(&doc, &json!({ "$nor": [] })).is_err());
        // 逻辑组与同层字段条件统一 AND
        assert!(matches(&doc, &json!({ "$or": [{ "a": 1 }, { "b": 2 }], "a": 1 })).unwrap());
        assert!(!matches(&doc, &json!({ "$or": [{ "a": 1 }, { "b": 2 }], "a": 9 })).unwrap());
        // 逻辑组值非数组 → Err
        assert!(matches(&doc, &json!({ "$and": 1 })).is_err());
    }

    // ---- 非法形态 ----------------------------------------------------------

    #[test]
    fn invalid_forms_error() {
        let doc = json!({ "a": 1 });
        // 顶层未知算子 → Err
        assert!(matches(&doc, &json!({ "$where": "x" })).is_err());
        // 字段级未知算子 → Err
        assert!(matches(&doc, &json!({ "a": { "$foo": 1 } })).is_err());
        // 非对象 / 非 null 的 filter → Err
        assert!(matches(&doc, &json!(5)).is_err());
        assert!(matches(&doc, &json!([1, 2])).is_err());
    }

    #[test]
    fn null_and_empty_filter_match_all() {
        let doc = json!({ "a": 1 });
        assert!(matches(&doc, &Value::Null).unwrap());
        assert!(matches(&doc, &json!({})).unwrap());
    }
}
