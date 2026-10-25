//! 命名冲突检测（设计 §6.3，A7）：同一命名空间内归一后撞名 ⇒ **报错，绝不静默覆盖**。

use super::canonical;

/// 稳定错误前缀（Host 按前缀识别）。经 `CoreError::classify`（`error.rs:46-54`）
/// 归入 `CoreError::Other` —— 本步**无需改 `error.rs`**。
pub const ERR_NAME_CONFLICT: &str = "ERR_NAME_CONFLICT:";

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
///
/// 【边界（原初设计）】`fields` 键空间**完全开放**：键 = 字段名，值 = 类型简写或定义对象，
/// 类型声明只存在于定义对象内部的 `type` 键——两者处于不同对象层，无天然冲突。
/// 字段名不是契约保留字：`type`/`default`/`read` 等均可作字段名（如
/// `fields: { type: { type: "string" } }` 合法）。本检测**只**拦「归一撞名」；
/// 字面保留名仅 `_id`（框架主键，豁免检测）与 `_score`（向量评分列，
/// `registry.rs` 单独字面拦截），不存在字段名保留字。
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

    /// 契约键不是字段名保留字：`type` / `fnRef` 等作字段名合法（原初设计：
    /// fields 键空间完全开放；类型声明在定义对象内部的 `type` 键，不同层无冲突）
    #[test]
    fn contract_keys_are_valid_field_names() {
        assert!(detect_conflicts(["type"]).is_ok());
        assert!(detect_conflicts([
            "type", "required", "default", "read", "write", "fields", "strategy", "fnRef"
        ])
        .is_ok());
        assert!(detect_conflicts(["fn_ref", "localField", "foreignField"]).is_ok());
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
