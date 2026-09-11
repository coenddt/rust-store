//! Mongo filter → WHERE 子句（参数化）

use serde_json::Value;

use super::Backend;

/// 单条 WHERE 片段（文本 + 已生成的参数、排序号）
#[derive(Debug, Clone)]
pub struct WhereClause {
    pub text: String,
    pub params: Vec<Value>,
}

impl WhereClause {
    fn new(text: String, params: Vec<Value>) -> Self {
        WhereClause { text, params }
    }

    pub fn and(a: WhereClause, b: WhereClause) -> WhereClause {
        let text = match (a.text.is_empty(), b.text.is_empty()) {
            (true, true) => String::new(),
            (true, false) => b.text,
            (false, true) => a.text,
            (false, false) => format!("({} AND {})", a.text, b.text),
        };
        let mut params = a.params;
        params.extend(b.params);
        WhereClause { text, params }
    }
}

/// 把 Mongo filter 翻译为 WHERE。`param_seq` 是 Postgres 占位序号游标（就地递增）。
/// `alias` 是本表别名（`t`）；`column` 是字段 → 列名的映射。
pub fn build_filter(
    filter: &Value,
    backend: Backend,
    alias: &str,
    column: &dyn Fn(&str) -> Option<String>,
    param_seq: &mut usize,
) -> WhereClause {
    match filter {
        Value::Null => WhereClause::new(String::new(), Vec::new()),
        Value::Object(map) => {
            if map.is_empty() {
                return WhereClause::new(String::new(), Vec::new());
            }
            // 顶层逻辑操作符
            if let Some(arr) = map.get("$and").and_then(|v| v.as_array()) {
                return and_group(arr.iter().map(|f| build_filter(f, backend, alias, column, param_seq)).collect(), "AND", backend, param_seq);
            }
            if let Some(arr) = map.get("$or").and_then(|v| v.as_array()) {
                return and_group(arr.iter().map(|f| build_filter(f, backend, alias, column, param_seq)).collect(), "OR", backend, param_seq);
            }
            if let Some(v) = map.get("$nor") {
                let inner = build_filter(v, backend, alias, column, param_seq);
                if inner.text.is_empty() {
                    return WhereClause::new("0".to_string(), Vec::new());
                }
                return WhereClause::new(format!("NOT {}", inner.text), inner.params);
            }

            // 字段条件：至少有一条具体字段
            let mut clauses: Vec<WhereClause> = Vec::new();
            for (field, cond) in map {
                if field.starts_with('$') {
                    // 未识别的顶层逻辑操作符 → 保守跳过（不生成错误 SQL）
                    continue;
                }
                let Some(col) = column(field) else { continue };
                let qualified = format!("{}.{}", alias, backend.quote_ident(&col));
                clauses.push(cond_clause(cond, &qualified, backend, column, param_seq));
            }
            and_group(clauses, "AND", backend, param_seq)
        }
        _ => WhereClause::new(String::new(), Vec::new()),
    }
}

fn and_group(clauses: Vec<WhereClause>, op: &str, _backend: Backend, _param_seq: &mut usize) -> WhereClause {
    let mut active: Vec<WhereClause> = clauses.into_iter().filter(|c| !c.text.is_empty()).collect();
    if active.is_empty() {
        return WhereClause::new(String::new(), Vec::new());
    }
    let mut it = active.drain(..);
    let mut acc = it.next().unwrap();
    for c in it {
        let text = format!("({} {} {})", acc.text, op, c.text);
        acc.params.extend(c.params);
        acc.text = text;
    }
    acc
}

/// 单个字段 = 条件的 WHERE 片段
fn cond_clause(
    cond: &Value,
    col: &str,
    backend: Backend,
    column: &dyn Fn(&str) -> Option<String>,
    param_seq: &mut usize,
) -> WhereClause {
    let ph = |seq: &mut usize| backend.placeholder(*seq);

    // 运算符对象
    if let Some(op) = cond.as_object() {
        let mut parts: Vec<WhereClause> = Vec::new();
        for (k, v) in op {
            let part = match k.as_str() {
                "$eq" => binop(col, "=", v, backend, param_seq),
                "$ne" => binop(col, "<>", v, backend, param_seq),
                "$gt" => binop(col, ">", v, backend, param_seq),
                "$gte" => binop(col, ">=", v, backend, param_seq),
                "$lt" => binop(col, "<", v, backend, param_seq),
                "$lte" => binop(col, "<=", v, backend, param_seq),
                "$in" => in_list(col, v, false, backend, param_seq),
                "$nin" => in_list(col, v, true, backend, param_seq),
                "$exists" => exists_expr(col, v, backend),
                "$not" => {
                    let inner = cond_clause(v, col, backend, column, param_seq);
                    let text = if inner.text.is_empty() { String::new() } else { format!("NOT {}", inner.text) };
                    WhereClause { text, params: inner.params }
                }
                "$regex" | "$options" => {
                    if k == "$regex" {
                        regex_expr(col, v, backend, param_seq)
                    } else {
                        WhereClause::new(String::new(), Vec::new())
                    }
                }
                // $expr / $elemMatch / $all 等复杂语义 → 保守不翻译（返回无条件，配合警告）
                _ => WhereClause::new(String::new(), Vec::new()),
            };
            if !part.text.is_empty() {
                parts.push(part);
            }
        }
        // 空对象 `{}` → 无条件
        if op.is_empty() {
            return WhereClause::new(String::new(), Vec::new());
        }
        return and_group(parts, "AND", backend, param_seq);
    }

    // 直接值 → $eq 简写
    binop(col, "=", cond, backend, param_seq)
}

fn binop(col: &str, op: &str, v: &Value, backend: Backend, seq: &mut usize) -> WhereClause {
    let text = format!("{} {} {}", col, op, backend.placeholder(*seq));
    *seq += 1;
    WhereClause::new(text, vec![v.clone()])
}

fn in_list(col: &str, v: &Value, negate: bool, backend: Backend, seq: &mut usize) -> WhereClause {
    let Some(arr) = v.as_array() else { return WhereClause::new(String::new(), Vec::new()) };
    if arr.is_empty() {
        // 空 $in → 恒假；空 $nin → 恒真
        return if negate {
            WhereClause::new("1".to_string(), Vec::new())
        } else {
            WhereClause::new("0".to_string(), Vec::new())
        };
    }
    let phs: Vec<String> = arr.iter().map(|_| {
        let p = backend.placeholder(*seq);
        *seq += 1;
        p
    }).collect();
    let text = format!("{} {} ({})", col, if negate { "NOT IN" } else { "IN" }, phs.join(", "));
    WhereClause::new(text, arr.clone())
}

fn exists_expr(col: &str, v: &Value, _backend: Backend) -> WhereClause {
    let exists = v.as_bool().unwrap_or(true);
    let text = format!("{} {} NULL", col, if exists { "IS NOT" } else { "IS" });
    WhereClause::new(text, Vec::new())
}

fn regex_expr(col: &str, v: &Value, backend: Backend, seq: &mut usize) -> WhereClause {
    let Some(pattern) = v.as_str() else { return WhereClause::new(String::new(), Vec::new()) };
    let text = match backend {
        Backend::Mysql => format!("{} REGEXP {}", col, backend.placeholder(*seq)),
        Backend::Postgres => format!("{} ~ {}", col, backend.placeholder(*seq)),
        Backend::Sqlite => format!("{} REGEXP {}", col, backend.placeholder(*seq)),
    };
    *seq += 1;
    WhereClause::new(text, vec![Value::String(pattern.to_string())])
}