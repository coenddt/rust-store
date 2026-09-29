//! 原生 SQL 语句编译：命名占位符（`:name`）→ 方言占位符 + 读写推断（纯逻辑，无 IO）。
//!
//! 为宿主 `execute_raw` 的「两档」能力提供 core 侧唯一实现：
//! - **位置档**（params 为数组/null）：SQL 原样透传，占位符由调用方手写方言原生风格
//!   （对标 SQLAlchemy `exec_driver_sql()`）；
//! - **命名档**（params 为对象）：`:name` 按出现顺序编译为 [`Backend::placeholder`]，
//!   参数按引用顺序重排、同名复用（对标 SQLAlchemy `text()`）。
//!
//! 跳过边界（R4）保证字符串/注释/PG cast 内的冒号不被误编译；读写推断（R7）以
//! 「默认写」为安全方向——误判为写至多路由主库，误判为读会读到旧数据。

use std::collections::HashSet;

use serde_json::Value;

use super::Backend;

/// 编译产物：最终 SQL + 重排后参数 + 读写标记
#[derive(Debug)]
pub struct RawStmt {
    /// 位置档 = 原文；命名档 = 编译后文本
    pub sql: String,
    /// 位置档 = 原数组（顺序保持）；命名档 = 按 `:name` 出现顺序重排
    pub params: Vec<Value>,
    /// 显式指定优先；否则按首词推断（R7）
    pub is_write: bool,
}

/// 原生 SQL 语句编译。
///
/// - `params`：`Array` = 位置档（透传）；`Object` = 命名档（`:name` 编译）；
///   `Null` = 位置档空参；其余 → Err（R1）。
/// - `is_write`：`Some(b)` 显式采用；`None` 按首词推断（R7）。
pub fn compile_raw_stmt(
    backend: Backend,
    text: &str,
    params: Value,
    is_write: Option<bool>,
) -> Result<RawStmt, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("原生 SQL 文本为空".to_string());
    }
    let is_write = is_write.unwrap_or_else(|| infer_is_write(trimmed));
    match params {
        // R2 位置档：原样透传（Null 视为空参数组）
        Value::Array(items) => Ok(RawStmt { sql: text.to_string(), params: items, is_write }),
        Value::Null => Ok(RawStmt { sql: text.to_string(), params: Vec::new(), is_write }),
        // R3–R6 命名档：`:name` 编译 + 参数重排 + 一致性校验
        Value::Object(names) => {
            let (sql, ordered) = compile_named(backend, text, &names)?;
            Ok(RawStmt { sql, params: ordered, is_write })
        }
        other => Err(format!(
            "原生 SQL params 仅支持数组（位置档）或对象（命名档），收到 {}",
            json_type_name(&other)
        )),
    }
}

/// R3–R6：命名档扫描编译。
///
/// 状态机逐字符扫描：引号字符串 / 行注释 / 块注释 / `::` cast 内部原样复制（R4）；
/// 裸 `?` / `$n` 不在编译范围，保持原样（PG `jsonb ? 'k'` 等操作符合法，误用时由驱动
/// 报参数数错，不静默）。缺名 / 多余名显式 Err（R6，禁静默）。
fn compile_named(
    backend: Backend,
    text: &str,
    params: &serde_json::Map<String, Value>,
) -> Result<(String, Vec<Value>), String> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(text.len() + 8);
    let mut ordered: Vec<Value> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        // R4：单引号 / 双引号字符串（'' / "" 翻倍转义）
        if c == '\'' || c == '"' {
            let quote = c;
            out.push(c);
            i += 1;
            while i < n {
                if chars[i] == quote {
                    if i + 1 < n && chars[i + 1] == quote {
                        // 翻倍转义：两个字面引号，仍在字符串内
                        out.push(quote);
                        out.push(quote);
                        i += 2;
                        continue;
                    }
                    break;
                }
                out.push(chars[i]);
                i += 1;
            }
            if i < n {
                out.push(quote); // 收尾引号（未闭合引号原样保留，交由驱动报错）
                i += 1;
            }
            continue;
        }
        // R4：`-- …` 行注释（至行尾）
        if c == '-' && i + 1 < n && chars[i + 1] == '-' {
            while i < n && chars[i] != '\n' {
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }
        // R4：`/* … */` 块注释
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            out.push(c);
            out.push('*');
            i += 2;
            while i < n {
                if chars[i] == '*' && i + 1 < n && chars[i + 1] == '/' {
                    out.push('*');
                    out.push('/');
                    i += 2;
                    break;
                }
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }
        // R4：`::` cast（PG），双冒号一并跳过
        if c == ':' && i + 1 < n && chars[i + 1] == ':' {
            out.push(':');
            out.push(':');
            i += 2;
            continue;
        }
        // R3：`:name` 命名占位符（首字符字母/下划线，续字母/数字/下划线）
        if c == ':' && i + 1 < n && (chars[i + 1].is_ascii_alphabetic() || chars[i + 1] == '_') {
            let start = i + 1;
            let mut j = start;
            while j < n && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            let name: String = chars[start..j].iter().collect();
            match params.get(&name) {
                Some(v) => {
                    out.push_str(&backend.placeholder(ordered.len()));
                    ordered.push(v.clone());
                    used.insert(name);
                }
                None => return Err(format!("原生 SQL 命名参数 :{} 未在 params 中提供", name)),
            }
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    // R6：params 中 text 未使用的名字 → Err
    let mut unused: Vec<String> = params
        .keys()
        .filter(|k| !used.contains(*k))
        .map(|k| format!(":{}", k))
        .collect();
    if !unused.is_empty() {
        unused.sort();
        return Err(format!(
            "原生 SQL params 中存在未使用的命名参数: {}",
            unused.join(", ")
        ));
    }
    Ok((out, ordered))
}

/// R7：读写推断。trim 后跳过一层前导 `(` 与空白，取首词大写化；
/// ∈ {SELECT, WITH, EXPLAIN, SHOW, PRAGMA, TABLE} → 读，其余 → 写（安全方向）。
fn infer_is_write(trimmed: &str) -> bool {
    let mut s = trimmed.trim_start();
    if let Some(rest) = s.strip_prefix('(') {
        s = rest.trim_start();
    }
    let word: String = s
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    !matches!(
        word.to_ascii_uppercase().as_str(),
        "SELECT" | "WITH" | "EXPLAIN" | "SHOW" | "PRAGMA" | "TABLE"
    )
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
