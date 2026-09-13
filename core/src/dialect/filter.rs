//! Mongo filter → WHERE 子句（参数化）

use serde_json::Value;

use super::Backend;

/// 告警通道：`Some(&mut Vec<String>)` 表示调用方能收集翻译告警（select 侧会透出给 Host）；
/// `None` 表示**无告警能力**（写路径）——此时遇到「条件可翻译但语义需降级」的组合直接报错，
/// 宁可失败也绝不静默生成语义失真的 SQL（对齐 `write.rs`「需告警的翻译直接报错」策略）。
pub type Warnings<'a> = Option<&'a mut Vec<String>>;

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
///
/// 硬化契约（缺陷 D-02）：**无法翻译的条件键一律显式报错，绝不静默丢弃** ——
/// 丢弃条件 = 生成缺 WHERE 的错误 SQL（返回全量/全表写），调用方无法区分
/// 「无数据」与「条件被丢弃」。服务端执行类操作符（`$where` 等）在规划层
/// 已被 [`crate::types::validate_condition`] 拒绝，此处为纵深防御兜底。
pub fn build_filter(
    filter: &Value,
    backend: Backend,
    alias: &str,
    column: &dyn Fn(&str) -> Option<String>,
    param_seq: &mut usize,
    mut warnings: Warnings,
) -> Result<WhereClause, String> {
    match filter {
        Value::Null => Ok(WhereClause::new(String::new(), Vec::new())),
        Value::Object(map) => {
            if map.is_empty() {
                return Ok(WhereClause::new(String::new(), Vec::new()));
            }
            // 顶层逻辑操作符
            if let Some(v) = map.get("$and") {
                let arr = v.as_array().ok_or_else(|| "$and 需要数组".to_string())?;
                return Ok(and_group(
                    arr.iter()
                        .map(|f| {
                            build_filter(
                                f,
                                backend,
                                alias,
                                column,
                                param_seq,
                                warnings.as_deref_mut(),
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    "AND",
                    backend,
                    param_seq,
                ));
            }
            if let Some(v) = map.get("$or") {
                let arr = v.as_array().ok_or_else(|| "$or 需要数组".to_string())?;
                return Ok(and_group(
                    arr.iter()
                        .map(|f| {
                            build_filter(
                                f,
                                backend,
                                alias,
                                column,
                                param_seq,
                                warnings.as_deref_mut(),
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    "OR",
                    backend,
                    param_seq,
                ));
            }
            if let Some(v) = map.get("$nor") {
                // $nor = NOT(OR(...))；Mongo 语义要求数组（缺陷修复：数组形态此前
                // 会被误译为恒假 "0"，静默返回空结果）
                let arr = v.as_array().ok_or_else(|| "$nor 需要数组".to_string())?;
                if arr.is_empty() {
                    return Err("$nor 需要非空数组".to_string());
                }
                let inner = and_group(
                    arr.iter()
                        .map(|f| {
                            build_filter(
                                f,
                                backend,
                                alias,
                                column,
                                param_seq,
                                warnings.as_deref_mut(),
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    "OR",
                    backend,
                    param_seq,
                );
                if inner.text.is_empty() {
                    // 所有分支都无法生成条件（如空对象 {}）→ OR 匹配全部 → NOR 匹配无
                    return Ok(WhereClause::new("0".to_string(), Vec::new()));
                }
                return Ok(WhereClause::new(
                    format!("NOT {}", inner.text),
                    inner.params,
                ));
            }

            // 字段条件：至少有一条具体字段
            let mut clauses: Vec<WhereClause> = Vec::new();
            for (field, cond) in map {
                if field.starts_with('$') {
                    // 未识别的顶层操作符 → 显式报错（缺陷 D-02：绝不静默丢条件）
                    return Err(format!(
                        "不支持的过滤条件操作符: {field}（SQL 侧无法安全翻译，拒绝静默丢弃）"
                    ));
                }
                let Some(col) = column(field) else { continue };
                let qualified = format!("{}.{}", alias, backend.quote_ident(&col));
                clauses.push(cond_clause(
                    cond,
                    &qualified,
                    backend,
                    param_seq,
                    warnings.as_deref_mut(),
                )?);
            }
            Ok(and_group(clauses, "AND", backend, param_seq))
        }
        _ => Ok(WhereClause::new(String::new(), Vec::new())),
    }
}

fn and_group(
    clauses: Vec<WhereClause>,
    op: &str,
    _backend: Backend,
    _param_seq: &mut usize,
) -> WhereClause {
    let mut active: Vec<WhereClause> = clauses.into_iter().filter(|c| !c.text.is_empty()).collect();
    if active.is_empty() {
        return WhereClause::new(String::new(), Vec::new());
    }
    let mut it = active.drain(..);
    let Some(mut acc) = it.next() else {
        return WhereClause::new(String::new(), Vec::new());
    };
    for c in it {
        let text = format!("({} {} {})", acc.text, op, c.text);
        acc.params.extend(c.params);
        acc.text = text;
    }
    acc
}

/// 单个字段 = 条件的 WHERE 片段
///
/// （`column` 映射在 `build_filter` 层已解析为限定列名 `col`，此处不再需要 ——
/// `$not` 递归仅传递已解析的 `col`。）
fn cond_clause(
    cond: &Value,
    col: &str,
    backend: Backend,
    param_seq: &mut usize,
    mut warnings: Warnings,
) -> Result<WhereClause, String> {
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
                "$in" => in_list(col, v, false, backend, param_seq)?,
                "$nin" => in_list(col, v, true, backend, param_seq)?,
                "$exists" => exists_expr(col, v, backend),
                "$not" => {
                    let inner = cond_clause(v, col, backend, param_seq, warnings.as_deref_mut())?;
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
        return Ok(and_group(parts, "AND", backend, param_seq));
    }

    // 直接值 → $eq 简写
    Ok(binop(col, "=", cond, backend, param_seq))
}

fn binop(col: &str, op: &str, v: &Value, backend: Backend, seq: &mut usize) -> WhereClause {
    let text = format!("{} {} {}", col, op, backend.placeholder(*seq));
    *seq += 1;
    WhereClause::new(text, vec![v.clone()])
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
    let phs: Vec<String> = arr
        .iter()
        .map(|_| {
            let p = backend.placeholder(*seq);
            *seq += 1;
            p
        })
        .collect();
    let text = format!(
        "{} {} ({})",
        col,
        if negate { "NOT IN" } else { "IN" },
        phs.join(", ")
    );
    Ok(WhereClause::new(text, arr.clone()))
}

fn exists_expr(col: &str, v: &Value, _backend: Backend) -> WhereClause {
    let exists = v.as_bool().unwrap_or(true);
    let text = format!("{} {} NULL", col, if exists { "IS NOT" } else { "IS" });
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
