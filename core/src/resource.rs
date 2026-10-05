//! 资源纯语义：内容寻址路径 + 引用归一 + URL 模板组合。
//!
//! 铁律：本模块**无 IO / 无时钟 / 无随机 / 无密钥**——只做确定性字符串与路径运算。
//! 签名参数（依赖时钟与密钥）由宿主注入，不在此实现。

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// 内容寻址路径：`objects/{sha1[0..2]}/{sha1}`。
///
/// `sha1` 为空或不足 2 字符 → Err（显式拒绝，禁静默产出半截路径）。
pub fn content_path(sha1: &str) -> Result<String, String> {
    if sha1.len() < 2 {
        return Err(format!("sha1 非法（长度 < 2）: \"{sha1}\""));
    }
    let shard = &sha1[..2];
    Ok(format!("objects/{shard}/{sha1}"))
}

/// 引用是否为外部 URL：`http://` / `https://` / `data:` 前缀（大小写不敏感）。
pub fn is_external_url(reference: &str) -> bool {
    let r = reference.trim().to_ascii_lowercase();
    r.starts_with("http://") || r.starts_with("https://") || r.starts_with("data:")
}

/// 引用类型：`"external"` | `"internal"`。
pub fn ref_kind(reference: &str) -> &'static str {
    if is_external_url(reference) { "external" } else { "internal" }
}

/// URL 纯组合（无 IO）。cfg：
///   {
///     "baseUrl": "https://cdn.example.com",
///     "pathTemplate": "/{contentPath}",          // 默认 "{key}"；可用 {key}/{sha1}/{contentPath}
///     "query": { "x-oss-process": "image/resize,w_{w}" },  // 值内可含 {var}
///     "vars": { "w": "200", "h": "100" },        // 供模板与 query 值替换
///     "externalPassthrough": true                 // 默认 true；false 时遇外部 URL 显式 Err
///   }
///
/// 规则：
///   1. reference 为空 → Err；
///   2. 外部 URL：externalPassthrough（默认 true）→ 原样返回；显式 false → Err；
///   3. 内部 id：baseUrl 与渲染后的 path 拼接（缺斜杠自动补一个），query 按键名排序追加并 percent-encode。
pub fn compose_url(reference: &str, cfg: &Value) -> Result<String, String> {
    if reference.is_empty() {
        return Err("resource reference 为空".to_string());
    }
    let obj = cfg.as_object();
    if is_external_url(reference) {
        let passthrough = obj
            .and_then(|o| o.get("externalPassthrough"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        return if passthrough {
            Ok(reference.to_string())
        } else {
            Err(format!("外部 URL 直返被禁用: \"{reference}\""))
        };
    }
    let empty = Map::new();
    let o = obj.unwrap_or(&empty);
    let base = o.get("baseUrl").and_then(|v| v.as_str()).unwrap_or("");
    let template = o
        .get("pathTemplate")
        .and_then(|v| v.as_str())
        .unwrap_or("{key}");
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    vars.insert("key".into(), reference.to_string());
    vars.insert("sha1".into(), reference.to_string());
    vars.insert("contentPath".into(), content_path(reference)?);
    if let Some(Value::Object(m)) = o.get("vars") {
        for (k, v) in m {
            vars.insert(k.clone(), scalar_to_string(v));
        }
    }
    let path = render(template, &vars);
    let mut url = join_base(base, &path);
    if let Some(Value::Object(q)) = o.get("query") {
        let mut pairs: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in q {
            pairs.insert(k.clone(), percent_encode(&render(&scalar_to_string(v), &vars)));
        }
        if !pairs.is_empty() {
            let qs = pairs
                .iter()
                .map(|(k, v)| format!("{}={}", percent_encode(k), v))
                .collect::<Vec<_>>()
                .join("&");
            url.push('?');
            url.push_str(&qs);
        }
    }
    Ok(url)
}

/// 模板渲染：把 `{name}` 用 vars 替换；未知名原样保留（显式，不静默删）。
fn render(template: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                match vars.get(name) {
                    Some(v) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// base 与 path 拼接（正确处理斜杠边界；base 为空则直接返回 path）。
fn join_base(base: &str, path: &str) -> String {
    if base.is_empty() {
        return path.to_string();
    }
    let b = base.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{b}{path}")
    } else {
        format!("{b}/{path}")
    }
}

/// 标量 → 字符串（字符串原样；数字/布尔用 JSON 字面；其余空串）。
fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// RFC3986 unreserved 之外全部 percent-encode（UTF-8 逐字节）。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
