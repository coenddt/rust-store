//! 跨库联邦查询（Phase 5）
//!
//! 核心难点：Mongo 的 `$lookup` 与 SQL 的 `JOIN` **都只能在同一数据源内**，
//! 跨源无法下推为一条命令。故本模块把一条 GQL 拆成：
//!
//! ```text
//!   plan_federated  → 各源命令序列 + join 边（纯逻辑，无 IO）
//!   Host            → 逐源执行（Mongo 原生 / SQL translate→exec）
//!   merge_federated → 内存哈希 join + 嵌套还原
//!   Host            → prepare_query / strip_query（与单库同一套尾处理）
//! ```
//!
//! 契约（对齐执行文档第七节）：
//!   - `postprocess` 与单库 `plan_query` 的 `postprocess` **同形状**（`{ast, inject}`），
//!     从而直接复用 [`crate::command::finalize_query`] / `strip_query`，无第二套实现；
//!   - `sources[].source` 只取自 `Registry` 的 schema `datasource` 名（缺省 `default`），
//!     禁止 Host 自造名；
//!   - 跨源关系必须能确定 `cardinality`（`one` / `many`），否则早失败；
//!   - 无法安全下推的组合一律进 `degraded` + warning，**绝不静默产生错误结果**。
//!
//! 子模块划分：
//!   - [`plan`]：拆源（哪些关系同源可下推、哪些跨源要拆成 join 边）
//!   - [`merge`]：按 join 边做内存哈希 join 与数组/对象还原

mod merge;
mod plan;

pub use merge::{merge_federated, MAX_FEDERATION_ROWS};
pub use plan::{plan_federated, FEDERATION_VERSION};
