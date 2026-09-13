//! Schema 管理（对应 JS `src/schema.js`）
//!
//! 与 JS 版的差异：注册表由模块级全局 `_schemas` 改为显式 `Registry` 实例。
//!
//! 文件组织：数据结构与字段规范化在 [`definition`]，注册/定位/配置在 [`registry`]。
//! 对外 API（`Registry` / `Schema` / `FieldDef` / `RelationDef` / `ComputeDef` / `map_of`）
//! 由本模块统一再导出，拆分不改语义。

mod definition;
mod registry;

pub use definition::{map_of, ComputeDef, FieldDef, RelationDef, Schema};
pub use registry::Registry;
