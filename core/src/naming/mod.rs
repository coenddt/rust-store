//! 命名归一与定向翻译（设计 §6）。**唯一实现**；`core-node` / `core-py` 仅透出。

mod canonical;
mod conflict;

pub use canonical::{canonical, to_camel, to_pascal, to_snake};
pub use conflict::{detect_conflicts, ERR_NAME_CONFLICT, RESERVED_KEYS};

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

/// 逻辑名 → 目标风格物理名（可逆：先 `canonical` 再按 `style` 重组）
pub fn translate(logical: &str, target: Target) -> String {
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
}
