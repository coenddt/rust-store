//! `Registry` 的方法分块（同 struct 多个 `#[napi] impl` 块）。
//!
//! 分块只按职责切分，不改语义；对外方法集与签名与拆分前一致。

pub(crate) mod compute;
pub(crate) mod datasource;
pub(crate) mod dialect;
pub(crate) mod federation;
pub(crate) mod perm;
pub(crate) mod plan;
