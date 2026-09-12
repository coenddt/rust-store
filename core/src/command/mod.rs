//! Command 序列契约（方案 A 的「core 产出指令、Host 执行」边界）
//!
//! core 不持有 MongoDB 驱动。读/写路径被拆成两步：
//!
//! ```text
//!   Host: plan = core.plan_query(gql, params, ctx)      // 纯逻辑，产出命令
//!   Host: for cmd in plan.commands { driver.execute(cmd) }   // 唯一 IO 边界
//!   Host: items = core.finalize(plan, items)            // 回喂 core 做后处理
//! ```
//!
//! 命令 JSON 形状（语言无关，Node/Python 两侧共用）：
//!
//! `source` / `namespace` / `collection` 为**定位三元组**，由生成该命令的 schema 一次填入
//! （`source` 缺省 `"default"`，`namespace` 缺省 `null` = 连接自身默认库/schema）。
//! Host 依据 `cmd.source`（而非 collection 反查）选择连接；`namespace` 的用法见
//! `multi-datasource-routing-plan.md`（Mongo client 型 source / SQL qualified 表名）。
//!
//! ```json
//! { "kind": "find", "source": "default", "namespace": null, "collection": "u", "filter": {...}, "projection": {...}|null }
//! { "kind": "aggregate", "source": "pg1", "namespace": "app", "collection": "u", "pipeline": [ ... ] }
//! { "kind": "countDocuments", "source": "default", "namespace": null, "collection": "u", "filter": {...} }
//! { "kind": "findOne", "source": "default", "namespace": null, "collection": "u", "filter": {...}, "projection": {...}|null }
//! { "kind": "insertOne", "source": "default", "namespace": null, "collection": "u", "doc": {...} }
//! { "kind": "insertMany", "source": "default", "namespace": null, "collection": "u", "docs": [ ... ] }
//! { "kind": "findOneAndUpdate", "source": "default", "namespace": null, "collection": "u", "filter": {...}, "update": {...}, "options": {...} }
//! { "kind": "updateMany", "source": "default", "namespace": null, "collection": "u", "filter": {...}, "update": {...} }
//! { "kind": "deleteMany", "source": "default", "namespace": null, "collection": "u", "filter": {...} }
//! ```
//!
//! 两阶段查询（`$lookup` + `$skip/$limit`）的命令序列里，第二条 aggregate 的
//! `$match._id.$in` 为占位符 [`PHASE1_IDS`]，Host 需用第一条命令返回的
//! `_id` 数组（**保持顺序**）替换后再执行；执行完调用 [`restore_sort_order`] 还原排序。
//!
//! 写路径占位符：mutation 步骤间的父子依赖用 `{{step.<N>._id}}`
//! （[`step_id_placeholder`]）表达——第 N 步执行结果文档的 `_id`，Host 在该步
//! 执行完成后回填到后续命令。需新生成的 `_id` 不用占位符：由 Host 供给
//! `new_ids` / `new_id` 参数，core 按序消费（core 无随机源）。
//!
//! 子模块划分：
//!   - [`cmd`]：命令构造 + 数值工具
//!   - [`query`]：读路径计划（分页 / 两阶段 / 排序还原）
//!   - [`count`]：列表 + total 计划
//!   - [`write`]：插入与简单查询计划 + 写权限探针
//!   - [`mutate`]：update / updateMany / remove / upsert / insertMany 计划
//!   - [`mutation`]：mutation 递归规划（父子文档步骤序列）
//!   - [`finalize`]：结果回喂（后处理 / asyncFn 桥 / 剥离注入）

mod cmd;
mod count;
mod finalize;
mod mutate;
mod mutation;
mod query;
mod write;

pub use cmd::{
    apply_route_override, cmd_aggregate, cmd_count_documents, cmd_delete_many, cmd_find,
    cmd_find_one, cmd_find_one_and_update, cmd_insert_many, cmd_insert_one, cmd_update_many,
};
pub use count::{plan_query_with_count, CountQueryPlan};
pub use finalize::{finalize_query, prepare_query, strip_query};
pub use mutate::{
    plan_archive_docs, plan_insert_many, plan_remove, plan_update, plan_update_many, plan_upsert,
};
pub use mutation::plan_mutation;
pub use query::{
    build_plan, plan_query, plan_query_ast_mut, plan_query_mut, resolve_page, restore_sort_order,
    sorts_by_relation, Mode, Page, QueryPlan,
};
pub use write::{check_write_perm, plan_aggregate, plan_count, plan_exists, plan_insert, Probe};

/// 权限拒绝哨兵：Host 需映射为各自的 PermissionError
pub const ERR_PERMISSION: &str = "无访问权限";
pub const ERR_NO_WRITE: &str = "无写入权限";

/// 两阶段查询中由 Host 替换的阶段一 `_id` 顺序数组
pub const PHASE1_IDS: &str = "{{phase1.ids}}";

/// mutation 第 `idx` 步执行结果文档 `_id` 的占位符
pub fn step_id_placeholder(idx: usize) -> String {
    format!("{{{{step.{}._id}}}}", idx)
}

/// 读取上限（pageSize 封顶，防拖库）
pub const MAX_PAGE_SIZE: f64 = 5000.0;
