//! 计算列引擎（对应 JS `src/computes.js`）
//!
//! 与 JS 版的差异（fnRef 回调桥）：
//!   - `fn` / `asyncFn` 是宿主语言原生闭包，无法跨 FFI 传递。schema 里以
//!     `"fn": true` / `"asyncFn": true` 声明（`fnRef` 指定回调标识，缺省为 key 名），
//!     执行体由 Host 实现 [`FnRegistry`] 后传入；core 保留声明信息与执行时机。
//!   - 模块级 `_defaultsCache` 改为每次调用现算（无跨请求共享状态）。
//!
//! 子模块划分：
//!   - [`registry`]：fn / asyncFn 回调桥
//!   - [`cache`]：schema 维度缓存（默认值 / 计算列声明）
//!   - [`defaults`]：字段默认值填充 + 同步计算列
//!   - [`inject`]：asyncFn depends 注入 / 剔除
//!   - [`run`]：processNode 主流程与 asyncFn 批量执行

mod cache;
mod defaults;
mod inject;
mod registry;
mod run;

pub use cache::{ensure_cache, Cache, ComputeEntry};
pub use defaults::{apply_defaults_and_computes, field_default, fill_nested_defaults};
pub use inject::{
    collect_field_deps, collect_rel_deps, inject_into_ast, merge_depends_into_ast,
    strip_dep_injected, InjectInfo, Injected, RelDep,
};
pub use registry::FnRegistry;
pub use run::{process_node, run_async_fns, run_computes, select_async_fns};
