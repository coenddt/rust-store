//! 单字段条件的操作符翻译（`$eq` / `$in` / `$regex` / `$not` …）。
//!
//! 契约：无法翻译的操作符/取值一律显式报错，绝不静默丢弃条件（缺陷 D-02）。

use serde_json::Value;

use super::{and_group, Warnings, WhereClause};
use crate::dialect::Backend;

/// 单个字段 = 条件的 WHERE 片段
///
/// （`column` 映射在 `filter` 层已解析为限定列名 `col`，此处不再需要 ——
/// `$not` 递归仅传递已解析的 `col`。）
pub(super) fn cond_clause(
    cond: &Value,
    col: &str,
    field_token: &str,
    alias: &str,
    backend: Backend,
    param_seq: &mut usize,
    mut warnings: Warnings,
) -> Result<WhereClause, String> {
    // 运算符对象
    if let Some(op) = cond.as_object() {
        let mut parts: Vec<WhereClause> = Vec::new();
        for (k, v) in op {
            let part = match k.as_str() {
                // F-07 三态契约：`$eq/$ne` 取值为 null 时，SQL 必须译为 `IS NULL` / `IS NOT NULL`。
                // `col = NULL` / `col <> NULL` 在 SQL 三值逻辑下恒否 → 显式 null 的行读不到（回归）。
                // 且 `$eq:null` 须仅命中「显式 null」而非「缺失」 → 追加 existence(alias.__present) 判定。
                "$eq" => null_eq_ne(col, "=", v, field_token, alias, backend, param_seq),
                "$ne" => null_eq_ne(col, "<>", v, field_token, alias, backend, param_seq),
                "$gt" => binop(col, ">", v, backend, param_seq),
                "$gte" => binop(col, ">=", v, backend, param_seq),
                "$lt" => binop(col, "<", v, backend, param_seq),
                "$lte" => binop(col, "<=", v, backend, param_seq),
                "$in" => in_list(col, v, false, backend, param_seq)?,
                "$nin" => in_list(col, v, true, backend, param_seq)?,
                // `$exists`：语义为「字段显式存在（含显式 null）」⇔ 在 __present 集合中。
                // 缺失行不在集合 → `$exists:false` 命中缺失。相较旧实现 `col IS [NOT] NULL`，
                // 这能把「缺失」与「显式 null」区分开（F-07/A-19）。
                "$exists" => exists_expr(col, field_token, alias, v, backend),
                "$not" => {
                    let inner = cond_clause(
                        v,
                        col,
                        field_token,
                        alias,
                        backend,
                        param_seq,
                        warnings.as_deref_mut(),
                    )?;
                    let text = if inner.text.is_empty() {
                        String::new()
                    } else {
                        format!("NOT {}", inner.text)
                    };
                    WhereClause {
                        text,
                        params: inner.params,
                    }
                }
                "$regex" => regex_expr(
                    col,
                    v,
                    op.get("$options"),
                    backend,
                    param_seq,
                    warnings.as_deref_mut(),
                )?,
                // $options 是 $regex 的大小写/匹配模式修饰符（评测 M-8-3）：
                // - 与 $regex 同用 → 条件语义由 $regex 分支合并处理（其会读取同对象 $options），此处不生成条件；
                // - 单独出现（无 $regex）→ 该修饰符无承载对象，**绝不静默丢弃**：有告警通道则告警后忽略，
                //   无通道（写路径）直接报错 —— 丢弃修饰符意味着匹配语义静默变化。
                "$options" => {
                    if !op.contains_key("$regex") {
                        let msg = format!(
                            "$options 出现在没有 $regex 的条件中（字段 {col}），该修饰符无法生效"
                        );
                        match warnings.as_deref_mut() {
                            Some(w) => w.push(msg),
                            None => return Err(msg),
                        }
                    }
                    WhereClause::new(String::new(), Vec::new())
                }
                // 未识别操作符（$expr / $elemMatch / $all / $where …）→ 显式报错
                // （缺陷 D-02：绝不静默丢弃条件生成缺 WHERE 的错误 SQL）
                _ => {
                    return Err(format!(
                        "不支持的过滤条件操作符: {k}（字段 {col}；SQL 侧无法安全翻译，拒绝静默丢弃）"
                    ));
                }
            };
            if !part.text.is_empty() {
                parts.push(part);
            }
        }
        // 空对象 `{}` → 无条件
        if op.is_empty() {
            return Ok(WhereClause::new(String::new(), Vec::new()));
        }
        return Ok(and_group(parts, "AND"));
    }

    // 直接值 → $eq 简写
    Ok(null_eq_ne(
        col,
        "=",
        cond,
        field_token,
        alias,
        backend,
        param_seq,
    ))
}

/// existence 判定片段：谓词「字段存在于 ``__present`` 集合」。
/// `col IS NULL` / `$exists` 需要与它做 AND / NOT。
/// `alias` 为空 = 表达式模式（HAVING，无哨兵列）→ 恒真（调用方改用 `IS [NOT] NULL`）。
fn present_pred(alias: &str, field_token: &str) -> String {
    if alias.is_empty() {
        return "TRUE".to_string();
    }
    format!(
        "COALESCE({}.__present, ',') LIKE '%,{},%'",
        alias, field_token
    )
}

/// `$eq/$ne` 翻译，取值 null 特判（F-07 三态）：
/// - `$eq:null` → `col IS NULL AND 字段存在`（显式 null，不含缺失）
/// - `$ne:null` → `col IS NOT NULL`（非空值；缺失/显式 null 都排外）
fn null_eq_ne(
    col: &str,
    op: &str,
    v: &Value,
    field_token: &str,
    alias: &str,
    backend: Backend,
    seq: &mut usize,
) -> WhereClause {
    if v.is_null() {
        if op == "=" {
            let is_null = format!("{} IS NULL", col);
            let exist = present_pred(alias, field_token);
            return WhereClause::new(format!("({} AND {})", is_null, exist), Vec::new());
        }
        return WhereClause::new(format!("{} IS NOT NULL", col), Vec::new());
    }
    binop(col, op, v, backend, seq)
}

fn binop(col: &str, op: &str, v: &Value, backend: Backend, seq: &mut usize) -> WhereClause {
    let text = format!("{} {} {}", col, op, param_expr(backend, seq, v));
    WhereClause::new(text, vec![v.clone()])
}

/// 参数占位表达式（消耗一个参数序号）。
///
/// §9.7「数值归 double」在 **PostgreSQL** 上的必要补充：PG 的扩展协议由**服务端按上下文**
/// 推断 `$n` 的类型 —— `int_col > $1`（字面量 `2.5`）会把 `$1` 推断为 `integer`，执行期直接
/// 报 `invalid input syntax for type integer: "2.5"`（MySQL/SQLite 动态类型无此问题）。
/// 此处对 PG 的**非整数字面量**显式标注 `CAST($n AS double precision)`，与 py 宿主 asyncpg
/// 「Python float → float8」的原生行为一致，保证两宿主对同一查询的行为相同。
fn param_expr(backend: Backend, seq: &mut usize, v: &Value) -> String {
    let ph = backend.placeholder(*seq);
    *seq += 1;
    if backend == Backend::Postgres {
        if let Value::Number(n) = v {
            if n.is_f64() {
                return format!("CAST({} AS double precision)", ph);
            }
        }
    }
    ph
}

fn in_list(
    col: &str,
    v: &Value,
    negate: bool,
    backend: Backend,
    seq: &mut usize,
) -> Result<WhereClause, String> {
    let Some(arr) = v.as_array() else {
        // $in/$nin 非数组 → 此前静默丢条件，现显式报错（缺陷 D-02）
        return Err(format!(
            "$in/$nin 需要数组（字段 {col}），拒绝静默丢弃该条件"
        ));
    };
    if arr.is_empty() {
        // 空 $in → 恒假；空 $nin → 恒真
        return Ok(if negate {
            WhereClause::new("1".to_string(), Vec::new())
        } else {
            WhereClause::new("0".to_string(), Vec::new())
        });
    }
    // 与 `binop` 同一占位表达式（PG 浮点字面量需显式 `CAST`，见 [`param_expr`]）
    let phs: Vec<String> = arr.iter().map(|v| param_expr(backend, seq, v)).collect();
    let text = format!(
        "{} {} ({})",
        col,
        if negate { "NOT IN" } else { "IN" },
        phs.join(", ")
    );
    Ok(WhereClause::new(text, arr.clone()))
}

/// `$exists` → 字段是否在 `__present` 集合（存在含显式 null，区别于缺失）。
/// 表达式模式（`alias` 为空，HAVING）：无哨兵列 → 退化为 `IS [NOT] NULL`。
fn exists_expr(
    col: &str,
    field_token: &str,
    alias: &str,
    v: &Value,
    _backend: Backend,
) -> WhereClause {
    let exists = v.as_bool().unwrap_or(true);
    if alias.is_empty() {
        let text = if exists {
            format!("{} IS NOT NULL", col)
        } else {
            format!("{} IS NULL", col)
        };
        return WhereClause::new(text, Vec::new());
    }
    let present = present_pred(alias, field_token);
    let text = if exists {
        present
    } else {
        format!("NOT ({})", present)
    };
    WhereClause::new(text, Vec::new())
}

/// `$regex`（含同对象 `$options` 修饰符）→ 后端正则匹配 SQL（评测 M-8-3）
///
/// 后端对 flags `i`（大小写不敏感）的表达能力：
/// - PostgreSQL：`i` → `~*` 运算符；无 `$options` → 大小写敏感 `~`
/// - MySQL：`i` → `REGEXP_LIKE(col, ?, 'i')` —— MySQL 8 的 REGEXP **默认区分大小写**，
///   不能依赖列 collation，必须显式传第三参（需 MySQL 8.0+，其正则引擎为 ICU）；
///   无 `$options` → `col REGEXP ?`（默认区分大小写，与 Mongo 无 flags 语义一致）
/// - SQLite：**不支持任何 flags** —— 带 `i` 时保留 `REGEXP` 翻译（条件本身不丢），
///   但推送告警声明「`i` 语义降级为大小写敏感」；无告警通道（写路径）时直接报错
///
/// 其余 flags（Mongo 的 `m`/`s`/`x` 及未知值）任何后端都无法表达 → 同样告警/报错，绝不静默。
///
/// **宿主前置条件（评测 M-8-2）**：SQLite 默认**不提供** `REGEXP` 函数 —— 须由宿主在连接层
/// 预先注册（如 rusqlite 的 `create_scalar_function("regexp", ...)`，签名 `regexp(pattern, value)`），
/// 否则执行期报 `no such function: REGEXP`。翻译层不做破坏性改判（宿主注册后即可用），
/// 该前置条件由本注释与宿主文档声明。
fn regex_expr(
    col: &str,
    v: &Value,
    options: Option<&Value>,
    backend: Backend,
    seq: &mut usize,
    warnings: Warnings,
) -> Result<WhereClause, String> {
    let Some(pattern) = v.as_str() else {
        // $regex 非字符串 → 此前静默丢条件，现显式报错（缺陷 D-02）
        return Err(format!(
            "$regex 需要字符串模式（字段 {col}），拒绝静默丢弃该条件"
        ));
    };
    // 解析 $options：逐字符核对 —— 'i' 可在后端表达（SQLite 除外）；其余一律进「无法表达」清单
    let mut case_insensitive = false;
    let mut inexpressible = String::new();
    if let Some(opts) = options.and_then(|o| o.as_str()) {
        for ch in opts.chars() {
            match ch {
                'i' if backend != Backend::Sqlite => case_insensitive = true,
                _ => inexpressible.push(ch),
            }
        }
    }
    if !inexpressible.is_empty() {
        // 无法表达的 flags：有告警通道 → 告警后继续（条件不丢、语义降级已明示）；
        // 无通道（写路径）→ 报错，绝不静默生成语义失真的 SQL
        let msg = format!(
            "$regex 的 $options 含后端 {} 无法表达的 flags（{}），相应语义无法生效（字段 {col}）",
            backend.as_str(),
            inexpressible
        );
        match warnings {
            Some(w) => w.push(msg),
            None => return Err(msg),
        }
    }
    let text = match backend {
        Backend::Mysql => {
            if case_insensitive {
                // MySQL 8 REGEXP 默认区分大小写，用 REGEXP_LIKE 第三参 'i' 显式声明（需 8.0+）
                format!("REGEXP_LIKE({}, {}, 'i')", col, backend.placeholder(*seq))
            } else {
                format!("{} REGEXP {}", col, backend.placeholder(*seq))
            }
        }
        Backend::Postgres => {
            if case_insensitive {
                // PG：~* 即大小写不敏感匹配
                format!("{} ~* {}", col, backend.placeholder(*seq))
            } else {
                format!("{} ~ {}", col, backend.placeholder(*seq))
            }
        }
        Backend::Sqlite => format!("{} REGEXP {}", col, backend.placeholder(*seq)),
    };
    *seq += 1;
    Ok(WhereClause::new(
        text,
        vec![Value::String(pattern.to_string())],
    ))
}
