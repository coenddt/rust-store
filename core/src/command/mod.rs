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
//! `source` / `database` / `schema` / `collection` 为**定位四元组**，由生成该命令的 schema
//! 一次填入（`source` 缺省 `"default"`，`database` / `schema` 缺省 `null` = 连接自身默认库/schema；
//! `schema` 仅 PostgreSQL 落点携带非 null）。
//! Host 依据 `cmd.source`（而非 collection 反查）选择连接；`database` / `schema` 的用法见
//! `multi-datasource-routing-plan.md`（Mongo client 型 source / SQL qualified 表名）。
//!
//! ```json
//! { "kind": "find", "source": "default", "database": null, "schema": null, "collection": "u", "filter": {...}, "projection": {...}|null }
//! { "kind": "aggregate", "source": "pg1", "database": "app_db", "schema": "app", "collection": "u", "pipeline": [ ... ] }
//! { "kind": "countDocuments", "source": "default", "database": null, "schema": null, "collection": "u", "filter": {...} }
//! { "kind": "findOne", "source": "default", "database": null, "schema": null, "collection": "u", "filter": {...}, "projection": {...}|null }
//! { "kind": "insertOne", "source": "default", "database": null, "schema": null, "collection": "u", "doc": {...} }
//! { "kind": "insertMany", "source": "default", "database": null, "schema": null, "collection": "u", "docs": [ ... ] }
//! { "kind": "findOneAndUpdate", "source": "default", "database": null, "schema": null, "collection": "u", "filter": {...}, "update": {...}, "options": {...} }
//! { "kind": "updateMany", "source": "default", "database": null, "schema": null, "collection": "u", "filter": {...}, "update": {...} }
//! { "kind": "deleteMany", "source": "default", "database": null, "schema": null, "collection": "u", "filter": {...} }
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
mod triggers;
mod write;
mod write_links;

use crate::permission::Context;
use crate::schema::{Profile, Registry};

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
    build_plan, check_readable_relations, has_pipeline, plan_query, plan_query_ast_mut,
    plan_query_mut, plan_query_one, resolve_page, restore_sort_order, sorts_by_relation, Mode,
    Page, QueryPlan,
};
pub use triggers::{before_probe_fields, expand_schedule_triggers, expand_triggers, has_triggers};
pub use write::{check_write_perm, plan_count, plan_exists, plan_insert, Probe};
pub use write_links::{
    err_write_cross_source, resolve_write_links, WriteLinkPolicy, WriteLinks,
    ERR_WRITE_CROSS_SOURCE_PREFIX,
};

/// 权限拒绝哨兵：Host 需映射为各自的 PermissionError。
///
/// 所有权限类错误统一携带 `ERR_PERMISSION:` 稳定前缀 —— Host 按**前缀**识别
/// 权限错误（构造后剥离前缀），不再对具体中文文案做脆弱匹配（core 文案可自由调整）。
pub const ERR_PERM_PREFIX: &str = "ERR_PERMISSION:";
pub const ERR_PERMISSION: &str = "ERR_PERMISSION:无访问权限";
pub const ERR_NO_WRITE: &str = "ERR_PERMISSION:无写入权限";
pub const ERR_NO_DELETE: &str = "ERR_PERMISSION:无删除权限";
pub const ERR_NO_BATCH_WRITE: &str = "ERR_PERMISSION:无批量写入权限";

/// `require_context` 开启时 ctx 缺失的拒绝哨兵（fail-secure；Host 按前缀识别，
/// 经 `CoreError::NoContext`（machine code `no_context`）映射对外语义 **403 / `PERMISSION_DENIED`**）。
///
/// 依据 `store-api/spec/04-context.md`：`requireContext` 开启且未注入上下文的请求按
/// 权限类映射 403，适配层不加第二层判断（该档保留独立 machine code 便于宿主/皮区分与告警）。
pub const ERR_NO_CONTEXT: &str =
    "ERR_NO_CONTEXT:require_context 已开启，调用必须携带用户上下文（内部调用请传 {\"internal\": true}）";

/// fail-secure 门禁：`require_context` 开启时拒绝 `ctx: None`（默认关闭时零开销放行）。
///
/// 挂在所有含 ctx 的 plan 公开入口顶部（读/写/联邦），保证「绑定层无论怎么调，
/// 只要开关开着，缺上下文一律显式报错」——而非静默按系统调用放行。
pub fn ensure_context(registry: &Registry, ctx: Option<&Context>) -> Result<(), String> {
    if registry.require_context() && ctx.is_none() {
        return Err(ERR_NO_CONTEXT.to_string());
    }
    // 档位叠加（正交但叠加）：text2query 档等效强制携带用户上下文（见 [`ensure_profile_ctx`]）。
    // 挂在同一处 → 所有已挂 `ensure_context` 的入口（读/写/联邦）自动获得档位强制，零遗漏。
    ensure_profile_ctx(registry, ctx)
}

/// 档位拒绝哨兵：text2query 档命中硬限制/收缩项时的稳定前缀。
///
/// Host 按**前缀**映射为各自的 ProfileViolation（构造后剥离前缀），
/// 不对中文文案做脆弱匹配（core 文案可自由调整）——与 `ERR_PERM_PREFIX` 同构。
pub const ERR_TEXT2QUERY: &str = "ERR_TEXT2QUERY:";

/// text2query 档硬限制（单点定义，Host 可读；取值**严于** standard）
///
/// - 单次取数行数：严于 [`MAX_PAGE_SIZE`]（5000）—— AI 问数交互式结果规模；
/// - 关系嵌套深度：严于 `pipeline::MAX_DEPTH`（10）—— AI 生成的关系嵌套实用上限；
/// - 联邦单源行数：严于 `federation::MAX_FEDERATION_ROWS`（100000）。
pub const T2Q_MAX_ROWS: f64 = 1000.0;
pub const T2Q_MAX_DEPTH: usize = 3;
pub const T2Q_MAX_FEDERATION_ROWS: usize = 10_000;

/// text2query 档强制上下文（叠加于 [`ensure_context`] 之上）。
///
/// `require_context` 与档位**正交但叠加**：text2query 档等效强制开启，
/// 退出档位后恢复用户原设置（见 Host 的 `text2query()` 上下文管理器）。
pub fn ensure_profile_ctx(registry: &Registry, ctx: Option<&Context>) -> Result<(), String> {
    if registry.profile() == Profile::Text2Query && ctx.is_none() {
        return Err(format!("{ERR_TEXT2QUERY}text2query 档必须携带用户上下文"));
    }
    Ok(())
}

/// text2query 档禁用某项能力（standard 档放行）。
///
/// 用于「DB 独有能力 / 直通 / 受限参数」类收缩项；standard 档不做任何拦截。
pub fn forbid_t2q(registry: &Registry, feature: &str) -> Result<(), String> {
    if registry.profile() == Profile::Text2Query {
        return Err(format!(
            "{ERR_TEXT2QUERY}text2query 档禁用 [{feature}]（功能收缩）"
        ));
    }
    Ok(())
}

/// text2query 档功能收缩 Err（无 `Registry` 场景：仅持 `Profile` 的形状校验函数用）。
///
/// 与 [`forbid_t2q`] 同构、文案形态单点收敛：
///
/// - 以 [`ERR_TEXT2QUERY`] 前缀开头（Host 按前缀映射 ProfileViolation 并 emit
///   `profile_blocked`，不匹配中文文案）；
/// - `[<feature>]` 方括号包裹门禁项名（Host 正则 `` \[(.+?)\] `` 提取为告警
///   `feature` 字段，缺括号即 feature 留白）；
/// - `detail` 携带原语义句与收缩编号（U1~U4 / 8c-2），供测试与 SKILL 文档子串断言。
pub fn forbid_t2q_shape(profile: Profile, feature: &str, detail: String) -> Result<(), String> {
    if profile == Profile::Text2Query {
        return Err(format!(
            "{ERR_TEXT2QUERY}text2query 档功能收缩 [{feature}]：{detail}"
        ));
    }
    Ok(())
}

/// text2query 档禁止携带 `route_override`（受信来源门禁）。
///
/// `route_override`（`{source, database, schema}`）是多租户路由覆盖的**受信服务端参数**，
/// 禁止透传用户输入 / AI 生成（否则可被用于跨源路由，CWE-639）。standard 档维持
/// 受信可用；text2query 档只要携带即拒。判决逻辑单点在此，绑定层（core-py/core-node）
/// 在 `with_route_override` 唯一注入点调用，覆盖读 / 写全部 plan 入口。
pub fn ensure_route_override_allowed(registry: &Registry, present: bool) -> Result<(), String> {
    if present && registry.profile() == Profile::Text2Query {
        return Err(format!(
            "{ERR_TEXT2QUERY}text2query 档禁用 route_override（受信参数，禁 AI 侧指定）"
        ));
    }
    Ok(())
}

/// 两阶段查询中由 Host 替换的阶段一 `_id` 顺序数组
pub const PHASE1_IDS: &str = "{{phase1.ids}}";

/// mutation 第 `idx` 步执行结果文档 `_id` 的占位符
pub fn step_id_placeholder(idx: usize) -> String {
    format!("{{{{step.{}._id}}}}", idx)
}

/// 读取上限（pageSize 封顶，防拖库）
pub const MAX_PAGE_SIZE: f64 = 5000.0;
