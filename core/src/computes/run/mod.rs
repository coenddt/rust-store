//! processNode 主流程与 asyncFn 批量执行
//!
//! 文件组织：同步处理链见 [`sync`]（processNode / run_computes），asyncFn
//! 选取与批量执行见 [`async_`]；对外路径 `crate::computes::*` 不变。

mod async_;
mod sync;

pub use async_::{run_async_fns, select_async_fns};
pub use sync::{process_node, run_computes};
