//! 命名冲突检测（设计 §6.3，A7）：归一后撞名 / 撞契约保留键 ⇒ **报错，绝不静默覆盖**。

use super::canonical;

/// 稳定错误前缀（Host 按前缀识别）。经 `CoreError::classify`（`error.rs:46-54`）
/// 归入 `CoreError::Other` —— 本步**无需改 `error.rs`**。
pub const ERR_NAME_CONFLICT: &str = "ERR_NAME_CONFLICT:";

/// 契约保留键（**归一后**的 token 串）。来源 = **字段 / 关系 / 计算列** 层实际被消费的契约键：
/// - field 级：`type`/`required`/`default`/`read`/`write`/`fields`/`strategy`（`definition.rs`）
/// - relation 级：`model`/`type`/`localField`/`foreignField`/`read`（`registry.rs`）
/// - compute 级：`type`/`fn`/`asyncFn`/`fnRef`/`agg`/`depends`/`read`（`registry.rs`）
///
/// 【边界（依据设计 §6.3 意图 + 实测收窄）】schema 级契约键（`name`/`collection`/`fields`/…）
/// **不**纳入：本检测作用于 `fields ∪ relations ∪ computes` 三个「结果文档命名空间」的标识符，
/// 而 schema 级键位于外层定义对象，不与这些标识符同层；把 schema 级键一并纳入会误拒常见数据
/// 字段名（如 `name`），导致 core 既有用例大面积回归失败（实测 47 项 guards 用例）。故保留键
/// 收窄为「与标识符同层的三级契约键」。设计 §6.3 仅举例 `fnref`，未穷举边界。
pub const RESERVED_KEYS: &[&str] = &[
    // field 级
    "type",
    "required",
    "default",
    "read",
    "write",
    "fields",
    "strategy",
    // relation 级
    "model",
    "localfield",
    "foreignfield",
    // compute 级
    "fn",
    "asyncfn",
    "fnref",
    "agg",
    "depends",
];

fn norm_key(s: &str) -> String {
    canonical(s).join("")
}

/// 系统保留标识符：框架主键 `_id`（`dialect/write/insert.rs:176`），不参与翻译与冲突检测。
fn is_system_reserved(name: &str) -> bool {
    name == "_id"
}

/// 一组逻辑名（**同一命名空间**）→ 冲突检测；命中即 `Err`（`ERR_NAME_CONFLICT:`），空 = 通过。
///
/// 保序、确定性（首次出现顺序），错误文案含两个冲突原名的具名。
pub fn detect_conflicts<'a, I>(names: I) -> Result<(), String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut seen: Vec<(String, &'a str)> = Vec::new();
    for name in names {
        if is_system_reserved(name) {
            continue;
        }
        let key = norm_key(name);
        if key.is_empty() {
            continue;
        }
        if let Some((_, prev)) = seen.iter().find(|(k, _)| k == &key) {
            return Err(format!(
                "{ERR_NAME_CONFLICT}名称 \"{name}\" 与 \"{prev}\" 归一后相同（{key}）"
            ));
        }
        if RESERVED_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "{ERR_NAME_CONFLICT}名称 \"{name}\" 归一后命中契约保留键 \"{key}\""
            ));
        }
        seen.push((key, name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A7：同 schema 归一撞名 ⇒ Err
    #[test]
    fn duplicate_after_normalize_errors() {
        assert!(detect_conflicts(["orderTotal", "order_total"]).is_err());
        assert!(detect_conflicts(["orderTotal", "orderItem"]).is_ok());
        assert!(detect_conflicts(["orderTotal"]).is_ok());
    }

    /// A7：归一后命中契约保留键 ⇒ Err
    #[test]
    fn reserved_key_errors() {
        let e = detect_conflicts(["fnRef"]).unwrap_err();
        assert!(e.starts_with(ERR_NAME_CONFLICT));
        assert!(detect_conflicts(["fn_ref"]).is_err());
    }

    /// `_id` 为框架保留主键，不参与冲突检测
    #[test]
    fn system_reserved_exempt() {
        assert!(detect_conflicts(["_id", "id"]).is_ok());
    }

    #[test]
    fn error_text_names_both() {
        let e = detect_conflicts(["orderTotal", "order_total"]).unwrap_err();
        assert!(e.contains("orderTotal") && e.contains("order_total"));
    }
}
