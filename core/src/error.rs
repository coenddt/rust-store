//! core 错误类型：FFI 边界的结构化错误（评测报告 rust m-3；框架缺陷修复 R3）。
//!
//! core 内部约定错误为 `String`（各层 `?` 直通，消息即契约——绑定层错误文案
//! 与 JS 参考实现逐字对齐）。本模块把「哨兵识别」从 Host 侧收口到 core：
//!
//! - **core 内唯一的哨兵前缀匹配点**是 [`CoreError::classify`]：内层 `String`
//!   错误在 FFI 边界经 `From<String>` 归类为 [`CoreError`]，绑定层按 [`CoreError::code`]
//!   透出稳定 machine code 并 `match` 枚举构造原生异常（Node `Error` / Python `RuntimeError`），
//!   不再各自做字符串匹配（对比 nodejs-store m-1 / py-store crud/exec.py 的历史方案）。
//! - **machine code 稳定且可穷举**（[`CoreError::code`]）：`permission_denied` /
//!   `no_context` / `profile_blocked` / `other`——四端（宿主 + 四皮）据此映射对外语义，
//!   与字符串前缀解耦。`Parse` / `NotFound` / `Validation` 三档按 R3 分步计划后续细化
//!   （本步先落上列四档，见 `doc/execution/2026/10/...-02-...` §4.3）。
//! - [`Display`] 输出**保留完整原始文案（含哨兵前缀）**：既有的「按前缀识别」
//!   Host 逻辑（含 `ERR_PERMISSION:` 剥离）不受影响，宿主可渐进迁移。
//!
//! 分类依据只引用 [`crate::command`] 的哨兵常量本体，靠 [`tests`] 模块锁定
//! 「常量前缀 ↔ 变体 ↔ machine code」关系，文案可自由调整而不破坏分类。

use crate::command::{ERR_NO_CONTEXT, ERR_PERM_PREFIX, ERR_TEXT2QUERY};

/// 策略视图只读守卫的稳定错误前缀：非 base 视图禁止注册 / 清空 schema（目录唯一真源在 base）。
/// 归入 [`CoreError::Other`]（非权限族），Host 按前缀识别即可（对齐 `no-error-masking`）。
pub const ERR_POLICY_VIEW_READONLY: &str = "ERR_POLICY_VIEW_READONLY:";

/// core 错误（FFI 边界可程序化穷举）。
///
/// 每个变体携带稳定 machine code（[`CoreError::code`]），Host 与四皮据此映射对外语义：
///
/// | 变体 | machine code | 触发前缀 | 对外语义 |
/// |---|---|---|---|
/// | [`Permission`](CoreError::Permission) | `permission_denied` | `ERR_PERMISSION:` | 403 / `PERMISSION_DENIED` |
/// | [`NoContext`](CoreError::NoContext) | `no_context` | `ERR_NO_CONTEXT:` | 403 / `PERMISSION_DENIED` |
/// | [`ProfileBlocked`](CoreError::ProfileBlocked) | `profile_blocked` | `ERR_TEXT2QUERY:` | 档位拦截（emit 反馈） |
/// | [`Other`](CoreError::Other) | `other` | —（兜底） | 500 透传原文 |
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    /// 权限拒绝：内层消息携带 `ERR_PERMISSION:` 稳定前缀
    /// （[`ERR_PERMISSION`](crate::command::ERR_PERMISSION) /
    /// [`ERR_NO_WRITE`](crate::command::ERR_NO_WRITE) /
    /// [`ERR_NO_DELETE`](crate::command::ERR_NO_DELETE) /
    /// [`ERR_NO_BATCH_WRITE`](crate::command::ERR_NO_BATCH_WRITE) 等，Host 据此映射 403 类错误）。
    #[error("{0}")]
    Permission(String),
    /// `require_context` 开启且 ctx 缺失（`ERR_NO_CONTEXT:` 前缀）：调用方合约违反。
    ///
    /// 对外语义为 **403 / `PERMISSION_DENIED`**——依据 `store-api/spec/04-context.md`
    /// （`requireContext` 开启且未注入上下文的请求按权限类映射 403，适配层不加第二层判断），
    /// 与 [`Permission`](CoreError::Permission) 同档对外呈现，但保留独立 machine code
    /// （`no_context`）以便宿主/皮区分语义与告警。
    #[error("{0}")]
    NoContext(String),
    /// 档位拦截：内层消息携带 `ERR_TEXT2QUERY:` 稳定前缀（text2query 档命中硬限制/收缩项）。
    ///
    /// Host 据此 emit `profile_blocked` 反馈并构造各自的 ProfileViolation（构造后剥离前缀），
    /// 不对中文文案做脆弱匹配——与 `ERR_PERMISSION:` 同构。
    #[error("{0}")]
    ProfileBlocked(String),
    /// 其余语义错误（schema 校验 / 翻译层 / 参数校验 / 计算列……）：无专属前缀，落兜底档，
    /// 服务端按 500 透传原文（`no-error-masking`）。
    #[error("{0}")]
    Other(String),
}

impl CoreError {
    /// 稳定 machine code（FFI 边界构造原生异常与四端映射的唯一依据；非字符串前缀）。
    ///
    /// 取值穷举见枚举文档表；新增档须单列，禁复用既有前缀语义。
    pub fn code(&self) -> &'static str {
        match self {
            CoreError::Permission(_) => "permission_denied",
            CoreError::NoContext(_) => "no_context",
            CoreError::ProfileBlocked(_) => "profile_blocked",
            CoreError::Other(_) => "other",
        }
    }

    /// 把内层 `String` 错误按哨兵前缀归类（core 内唯一的前缀匹配收口点）。
    pub fn classify(msg: String) -> Self {
        if msg.starts_with(ERR_PERM_PREFIX) {
            CoreError::Permission(msg)
        } else if msg.starts_with(ERR_NO_CONTEXT) {
            CoreError::NoContext(msg)
        } else if msg.starts_with(ERR_TEXT2QUERY) {
            CoreError::ProfileBlocked(msg)
        } else {
            CoreError::Other(msg)
        }
    }

    /// 完整原始消息（含哨兵前缀；与 `Display` 一致，供宿主按前缀渐进迁移）
    pub fn message(&self) -> &str {
        match self {
            CoreError::Permission(m)
            | CoreError::NoContext(m)
            | CoreError::ProfileBlocked(m)
            | CoreError::Other(m) => m,
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
        ERR_TEXT2QUERY,
    };

    #[test]
    fn classify_maps_sentinels_to_variants() {
        // 权限族（四个哨兵全量穷举）→ Permission
        for m in [
            ERR_PERMISSION,
            ERR_NO_WRITE,
            ERR_NO_DELETE,
            ERR_NO_BATCH_WRITE,
        ] {
            let e = CoreError::classify(m.to_string());
            assert_eq!(e, CoreError::Permission(m.to_string()));
            assert_eq!(e.code(), "permission_denied");
        }
        // ctx 缺失 → NoContext（machine code `no_context`）
        let e = CoreError::classify(ERR_NO_CONTEXT.to_string());
        assert_eq!(e, CoreError::NoContext(ERR_NO_CONTEXT.to_string()));
        assert_eq!(e.code(), "no_context");
        // 档位拦截 → ProfileBlocked（machine code `profile_blocked`）
        let e = CoreError::classify(format!("{ERR_TEXT2QUERY}单次取数行数超限"));
        assert_eq!(
            e,
            CoreError::ProfileBlocked(format!("{ERR_TEXT2QUERY}单次取数行数超限"))
        );
        assert_eq!(e.code(), "profile_blocked");
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
