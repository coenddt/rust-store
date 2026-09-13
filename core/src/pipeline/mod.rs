//! GQL 解析 + Aggregate Pipeline 构建器（对应 JS `src/pipeline.js`）
//!
//! GQL 语法（极简，5 个参数）:
//!   ModelName($condition:@c0,$sort:@s1,$skip:@sk,$limit:@l1,$pipeline:@p1) {
//!     field1, field2,
//!     RelationName($condition:@c2,$sort:@s3) { field3, NestedRelation { field4 } }
//!   }
//! 值用 `@key` 引用 params 对象。
//!
//! 子模块划分（按职责，非按行数平均切）：
//!   - [`token`]：词法分析
//!   - [`ast`]：AST 数据定义与 JSON 互转
//!   - [`parse`]：语法分析
//!   - [`lookup`]：$lookup / $addFields 阶段构建
//!   - [`build`]：整条 pipeline 的编排
//!   - [`projection`]：$project 投影构建
//!   - [`util`]：跨模块共享的小工具

mod ast;
mod build;
mod lookup;
mod parse;
mod projection;
mod token;
mod util;

pub use ast::{Ast, RelAst};
pub(crate) use build::flatten_object_fields_impl;
pub use build::{build_pipeline, flatten_object_fields};
pub use lookup::{build_add_fields, build_compute_lookup_stages, build_empty_lookup, build_lookup};
pub use parse::{parse, parse_gql};
pub use projection::build_projection;
pub use token::{token_to_value, tokenize, Token};
pub(crate) use util::{is_nullish, param};

// ─── 递归保护 ──────────────────────────────────────────────
pub const MAX_DEPTH: usize = 10;
pub const MAX_PAGINATED_DEPTH: usize = 4;
