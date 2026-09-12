//! core 错误类型：FFI 边界的结构化错误（评测报告 rust m-3）。
//!
//! core 内部约定错误为 `String`（各层 `?` 直通，消息即契约——绑定层错误文案
//! 与 JS 参考实现逐字对齐）。本模块把「哨兵识别」从 Host 侧收口到 core：
//!
//! - **core 内唯一的哨兵前缀匹配点**是 [`CoreError::classify`]：内层 `String`
//!   错误在 FFI 边界经 `From<String>` 归类为 [`CoreError`]，绑定层 `match`
//!   枚举构造原生异常（Node `Error` / Python `RuntimeError`），不再各自做
//!   字符串匹配（对比 nodejs-store m-1 / py-store crud/exec.py 的历史方案）。
//! - [`Display`] 输出**保留完整原始文案（含哨兵前缀）**：既有的「按前缀识别」
//!   Host 逻辑（含 `ERR_PERMISSION:` 剥离）不受影响，宿主可渐进迁移。
//!
//! 分类依据只引用 [`crate::command`] 的哨兵常量本体，靠 [`tests`] 模块锁定
//! 「常量前缀 ↔ 变体」关系，文案可自由调整而不破坏分类。

use crate::command::{ERR_NO_CONTEXT, ERR_PERM_PREFIX};

/// core 错误（FFI 边界可程序化穷举）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    /// 权限拒绝：内层消息携带 `ERR_PERMISSION:` 稳定前缀
    /// （[`ERR_PERMISSION`] / [`ERR_NO_WRITE`] / [`ERR_NO_DELETE`] /
    /// [`ERR_NO_BATCH_WRITE`] 等，Host 据此映射 403 类错误）。
    #[error("{0}")]
    Permission(String),
    /// `require_context` 开启且 ctx 缺失：调用方合约违反（`ERR_NO_CONTEXT:`
    /// 前缀），Host 一般映射 500 配置/契约错误而非 403。
    #[error("{0}")]
    NoContext(String),
    /// 其余语义错误（schema 校验 / 翻译层 / 参数校验 / 计算列……）
    #[error("{0}")]
    Other(String),
}

impl CoreError {
    /// 稳定错误码（FFI 边界 `match` 枚举之外的第二选择：日志/指标用）。
    pub fn code(&self) -> &'static str {
        match self {
            CoreError::Permission(_) => "permission",
            CoreError::NoContext(_) => "no_context",
            CoreError::Other(_) => "other",
        }
    }

    /// 把内层 `String` 错误按哨兵前缀归类（core 内唯一的前缀匹配收口点）。
    pub fn classify(msg: String) -> Self {
        if msg.starts_with(ERR_PERM_PREFIX) {
            CoreError::Permission(msg)
        } else if msg.starts_with(ERR_NO_CONTEXT) {
            CoreError::NoContext(msg)
        } else {
            CoreError::Other(msg)
        }
    }

    /// 完整原始消息（含哨兵前缀；与 `Display` 一致，供宿主按前缀渐进迁移）
    pub fn message(&self) -> &str {
        match self {
            CoreError::Permission(m) | CoreError::NoContext(m) | CoreError::Other(m) => m,
        }
    }
}

impl From<String> for CoreError {
    fn from(msg: String) -> Self {
        Self::classify(msg)
    }
}

impl From<&str> for CoreError {
    fn from(msg: &str) -> Self {
        Self::classify(msg.to_string())
    }
}

/// core 公开入口统一错误别名
pub type CoreResult<T> = Result<T, CoreError>;

#[cfg(test)]
mod tests {
    use super::CoreError;
    use crate::command::{
        ERR_NO_BATCH_WRITE, ERR_NO_CONTEXT, ERR_NO_DELETE, ERR_NO_WRITE, ERR_PERMISSION,
    };

    #[test]
    fn classify_maps_sentinels_to_variants() {
        // 权限族（四个哨兵全量穷举）→ Permission
        for m in [ERR_PERMISSION, ERR_NO_WRITE, ERR_NO_DELETE, ERR_NO_BATCH_WRITE] {
            let e = CoreError::classify(m.to_string());
            assert_eq!(e, CoreError::Permission(m.to_string()));
            assert_eq!(e.code(), "permission");
        }
        // ctx 缺失 → NoContext
        let e = CoreError::classify(ERR_NO_CONTEXT.to_string());
        assert_eq!(e, CoreError::NoContext(ERR_NO_CONTEXT.to_string()));
        assert_eq!(e.code(), "no_context");
        // 其余 → Other
        let e = CoreError::classify("translate: 未知命令".to_string());
        assert_eq!(e, CoreError::Other("translate: 未知命令".to_string()));
        assert_eq!(e.code(), "other");
    }

    #[test]
    fn display_preserves_message_verbatim() {
        // Display / message() 保留完整原文（含前缀）——宿主按前缀渐进迁移的兼容面
        let e = CoreError::from(ERR_NO_WRITE);
        assert_eq!(e.to_string(), ERR_NO_WRITE);
        assert_eq!(e.message(), ERR_NO_WRITE);
    }

    #[test]
    fn from_str_and_string_classify_identically() {
        assert_eq!(
            CoreError::from(ERR_PERMISSION),
            CoreError::classify(ERR_PERMISSION.to_string())
        );
    }
}
