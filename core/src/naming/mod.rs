//! 命名归一与定向翻译（设计 §6）。**唯一实现**；`core-node` / `core-py` 仅透出。

mod canonical;
mod conflict;

pub use canonical::{canonical, to_camel, to_pascal, to_snake};
pub use conflict::{detect_conflicts, ERR_NAME_CONFLICT};

/// 目标命名风格（设计 §6.1）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// snake_case
    Snake,
    /// camelCase
    Camel,
    /// PascalCase（须导出）
    Pascal,
}

/// 翻译目标：介质（物理库）/ 语言（代码，计算列跟随），见设计 §6.1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    // 物理库
    Mysql,
    Postgres,
    Sqlite,
    Mongodb,
    // 代码（计算列跟随）
    Node,
    Java,
    Csharp,
    Rust,
    Go,
    Python,
}

impl Target {
    /// 字符串 → 目标；未知值 **Err**（禁静默回落，对齐 `Profile::from_str_or_err`）
    pub fn from_str_or_err(s: &str) -> Result<Target, String> {
        match s.to_lowercase().as_str() {
            "mysql" => Ok(Target::Mysql),
            "postgres" | "pg" | "postgresql" => Ok(Target::Postgres),
            "sqlite" => Ok(Target::Sqlite),
            "mongo" | "mongodb" => Ok(Target::Mongodb),
            "node" | "nodejs" => Ok(Target::Node),
            "java" => Ok(Target::Java),
            "csharp" | "cs" | "c#" => Ok(Target::Csharp),
            "rust" => Ok(Target::Rust),
            "go" | "golang" => Ok(Target::Go),
            "python" | "py" => Ok(Target::Python),
            other => Err(format!("未知命名目标: {other}")),
        }
    }

    /// 数据源 kind（`DataSource::as_str` 取值：mongo/mysql/postgres/sqlite）→ 目标
    pub fn for_datasource_kind(kind: &str) -> Result<Target, String> {
        Target::from_str_or_err(kind)
    }

    /// 目标 → 风格（设计 §6.1 风格表）
    pub fn style(&self) -> Style {
        match self {
            Target::Mysql | Target::Postgres | Target::Sqlite | Target::Python => Style::Snake,
            Target::Mongodb | Target::Node | Target::Java | Target::Csharp | Target::Rust => {
                Style::Camel
            }
            Target::Go => Style::Pascal,
        }
    }
}

/// 是否已是合法的 camelCase 标识符（允许前导 `_`，其余仅 ASCII 字母数字，首字母小写）。
///
/// 这类名字本身就是 schema 声明的逻辑名，也是落库物理名（Mongo/Node/Java 等），
/// 归一化会把连续大写缩写降级（`sandboxXIRR` → `sandboxXirr`）并丢掉前导下划线
/// （`_cutoverDebug` → `cutoverDebug`），导致与存量数据字段名对不上。
fn is_camel_identity(s: &str) -> bool {
    let body = s.trim_start_matches('_');
    let mut chars = body.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    body.chars().all(|c| c.is_ascii_alphanumeric())
}

/// 逻辑名 → 目标风格物理名（可逆：先 `canonical` 再按 `style` 重组）
///
/// camelCase 目标对「已合规的 camelCase 逻辑名」恒等返回：翻译必须幂等且不改变
/// 声明名，否则连续大写缩写/前导下划线字段会与物理数据错位。
pub fn translate(logical: &str, target: Target) -> String {
    if target.style() == Style::Camel && is_camel_identity(logical) {
        return logical.to_string();
    }
    let tokens = canonical(logical);
    match target.style() {
        Style::Snake => to_snake(&tokens),
        Style::Camel => to_camel(&tokens),
        Style::Pascal => to_pascal(&tokens),
    }
}

/// 字符串版翻译（绑定入口）：目标串非法 ⇒ Err（禁静默回落）
pub fn translate_by_str(logical: &str, target: &str) -> Result<String, String> {
    Ok(translate(logical, Target::from_str_or_err(target)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 设计 §6.1 目标风格表逐项
    #[test]
    fn target_style_table() {
        assert_eq!(translate("orderTotal", Target::Mysql), "order_total");
        assert_eq!(translate("orderTotal", Target::Postgres), "order_total");
        assert_eq!(translate("orderTotal", Target::Sqlite), "order_total");
        assert_eq!(translate("orderTotal", Target::Mongodb), "orderTotal");
        assert_eq!(translate("orderTotal", Target::Node), "orderTotal");
        assert_eq!(translate("orderTotal", Target::Java), "orderTotal");
        assert_eq!(translate("orderTotal", Target::Csharp), "orderTotal");
        assert_eq!(translate("orderTotal", Target::Rust), "orderTotal");
        assert_eq!(translate("orderTotal", Target::Go), "OrderTotal"); // V6
        assert_eq!(translate("orderTotal", Target::Python), "order_total");
    }

    /// 设计 §6.2 管线：三写归一同一，各目标一致
    #[test]
    fn design_section_6_2_pipeline() {
        for src in ["orderTotal", "order_total", "Order.Total"] {
            assert_eq!(translate(src, Target::Mysql), "order_total");
            assert_eq!(translate(src, Target::Mongodb), "orderTotal");
            assert_eq!(translate(src, Target::Go), "OrderTotal");
            assert_eq!(translate(src, Target::Python), "order_total");
        }
    }

    #[test]
    fn unknown_target_errors() {
        assert!(Target::from_str_or_err("oracle").is_err());
        assert!(translate_by_str("orderTotal", "oracle").is_err());
    }

    /// 连续大写缩写 / 前导下划线的声明名在 camelCase 目标下必须恒等（回归：
    /// `sandboxXIRR` 曾被译成 `sandboxXirr`，读存量数据得 null）。
    #[test]
    fn camel_identity_preserves_acronyms_and_leading_underscore() {
        for (src, dst) in [
            ("sandboxXIRR", "sandboxXIRR"),
            ("sandboxMOIC", "sandboxMOIC"),
            ("estimatedIRR", "estimatedIRR"),
            ("userID", "userID"),
            ("myURLField", "myURLField"),
            ("_cutoverDebug", "_cutoverDebug"),
        ] {
            assert_eq!(translate(src, Target::Mongodb), dst);
            assert_eq!(translate(src, Target::Node), dst);
            // 幂等：再次翻译不变
            assert_eq!(
                translate(&translate(src, Target::Mongodb), Target::Mongodb),
                dst
            );
        }
    }

    /// 非恒等输入仍走归一（分隔符/首字母大写/全大写单词）。
    #[test]
    fn camel_identity_does_not_bypass_normalization() {
        assert_eq!(translate("order_total", Target::Mongodb), "orderTotal");
        assert_eq!(translate("Order.Total", Target::Mongodb), "orderTotal");
        assert_eq!(translate("OrderTotal", Target::Mongodb), "orderTotal");
        assert_eq!(translate("HTTP", Target::Mongodb), "http");
    }
}
