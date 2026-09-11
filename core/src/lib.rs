//! mongo-store 语言无关核心（Phase 1）
//!
//! 本 crate 只承载**纯逻辑**：GQL 解析、Aggregate Pipeline 构建、Projection 计算。
//! 不持有 MongoDB 驱动，不做任何 IO —— 对应方案 A 中「core 产出 Command，Host 执行」的切分。
//!
//! 与现有 JS 实现（`src/*.js`）的显式差异（即 P0 契约）：
//! 1. 权限上下文 `ctx` 由隐式 `AsyncLocalStorage` 改为**显式入参**；
//! 2. schema 注册表由模块级全局 `_schemas` 改为显式 `Registry` 实例。

pub mod bson;
pub mod command;
pub mod computes;
pub mod dialect;
pub mod permission;
pub mod pipeline;
pub mod schema;
pub mod types;
