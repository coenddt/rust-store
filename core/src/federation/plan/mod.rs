//! 联邦计划：把一条 GQL 拆成「各源命令序列 + join 边」
//!
//! 拆源规则（`can_pushdown`，见 `multi-datasource-routing-plan.md` §五）：
//!   - 关系两端**不同 source** → 跨源，从取数 AST 中剥离，登记为一条 `join` 边，
//!     并为子模型单独生成一个取数单元（自己那一源）；
//!   - 同 source 且同 namespace → **下推**，留在该源取数 AST（`$lookup` / `JOIN`）；
//!   - 同 source 跨 namespace：SQL 后端物理支持（qualified 表名 JOIN）→ **仍下推**；
//!     Mongo 跨 db 无 `$lookup` → 剥离（内存 join）。
//!
//! 父取数 AST 会被就地改成「只剩可下推关系」；后处理 AST（`postprocess.ast`）仍是
//! **含全部关系**的完整快照，从而 `strip_query` / `process_node` 可递归下钻到内存
//! join 还原出来的嵌套文档（对齐契约「postprocess 与单库同形状」）。
//!
//! 文件组织：本文件为计划入口与数据契约（单元 / join 边定义），
//! 递归拆源与降级检测在 [`route`]。

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::command::{build_plan, ensure_context, plan_query_ast_mut, ERR_PERMISSION};
use crate::computes::{merge_depends_into_ast, InjectInfo};
use crate::datasource::DataSourceConfig;
use crate::permission::{can_read_schema, merge_owner_condition, Context};
use crate::pipeline::{flatten_object_fields, is_nullish, param, parse_gql, Ast};
use crate::schema::{Registry, Schema};

use route::{detect_cross_source_sort, walk};

mod route;

/// 契约版本：形状变更必须升版并附迁移说明
pub const FEDERATION_VERSION: u64 = 2;

/// 定位二元组（source + namespace）
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Loc {
    pub(super) source: String,
    pub(super) namespace: Option<String>,
}

pub(super) fn loc_of(schema: &Schema) -> Loc {
    Loc {
        source: schema.source().to_string(),
        namespace: schema.namespace.clone(),
    }
}

/// 一个取数单元（某数据源上的一组命令）
struct UnitSpec {
    /// 结果回喂的键（`merge_federated` 的 `results[key]`）
    key: String,
    source: String,
    namespace: Option<String>,
    model: String,
    ast: Ast,
    /// 该单元在嵌套结构中的父层级深度（= 父边 `path.len()`，根为 0）
    ///
    /// `sources` 按此升序排列：父单元先于子单元，Host 结果回喂顺序与之一致。
    depth: usize,
}

/// 一条跨源 join 边
struct EdgeSpec {
    parent_model: String,
    rel: String,
    local: String,
    foreign: String,
    cardinality: String,
    /// 从根到**父**层的关系名路径（空 = 根层）；merge 据此定位待挂载的父文档
    path: Vec<String>,
    /// 子取数单元的 key
    key: String,
}

/// 生成联邦计划（纯逻辑）
///
/// `ds_config`：`{ "sources": { name: kind } }`（Host `init` 时的 kind 配置；
/// `null` = 单源 Mongo）。下推判定依赖它区分 SQL / Mongo（见 [`route::can_pushdown`]）。
///
/// 单源（无跨源关系）时同样可用：`sources` 只有根单元、`join.edges` 为空，
/// Host 走与单库一致的执行路径。
pub fn plan_federated(
    gql: &str,
    params: &Map<String, Value>,
    registry: &Registry,
    ctx: Option<&Context>,
    ds_config: &Value,
) -> Result<Value, String> {
    let ds_cfg = DataSourceConfig::from_json(ds_config)?;
    let mut params = params.clone();
    let mut ast = parse_gql(gql)?;
    ensure_context(registry, ctx)?;
    let root_schema = registry.get(&ast.model)?.clone();

    if ctx.is_some() && !can_read_schema(&root_schema, ctx) {
        return Err(ERR_PERMISSION.to_string());
    }

    // 所有者条件注入（与单库 `plan_query_ast_mut` 同语义：非 admin 只看自己的数据）
    if ctx.is_some() {
        if let Some(r) = ast.params.get("condition").cloned() {
            let key = r.get(1..).unwrap_or("").to_string();
            match merge_owner_condition(&root_schema, ctx, params.get(&key).cloned()) {
                Some(v) => {
                    params.insert(key, v);
                }
                None => {
                    params.remove(&key);
                }
            }
        }
    }

    let has_pipeline = ast
        .params
        .get("pipeline")
        .map(|r| !is_nullish(param(&params, Some(r))))
        .unwrap_or(false);

    // Registry 级守卫：与单库 plan_query_ast_mut 同款（联邦含单源场景）
    if has_pipeline && !registry.allow_user_pipeline {
        return Err("用户 $pipeline 已被禁用（allow_user_pipeline = false）".to_string());
    }

    // asyncFn 依赖注入在**完整 AST** 上做：postprocess 才能带全注入信息
    let inject = if has_pipeline {
        InjectInfo::default()
    } else {
        merge_depends_into_ast(&mut ast.relations, &root_schema)?
    };

    // 后处理 AST 快照：展平后、含全部关系（与单库同形状）
    let mut post_ast = ast.clone();
    if !has_pipeline {
        flatten_object_fields(&mut post_ast, &root_schema);
    }

    let root_loc = loc_of(&root_schema);
    let mut units: Vec<UnitSpec> = Vec::new();
    let mut edges: Vec<EdgeSpec> = Vec::new();
    let mut degraded: Vec<Value> = Vec::new();

    let mut fetch_ast = ast;
    walk(
        &fetch_ast.model,
        &ds_cfg,
        &mut fetch_ast.fields,
        &mut fetch_ast.relations,
        &[],
        &params,
        registry,
        &mut units,
        &mut edges,
        &mut degraded,
    )?;

    if has_pipeline && !edges.is_empty() {
        return Err("联邦查询不支持用户 $pipeline（无法跨源下推）".to_string());
    }

    detect_cross_source_sort(&fetch_ast, &params, &edges, &mut degraded);

    // 父层级深度升序：merge 依序物化，保证「用前已挂载」
    edges.sort_by_key(|e| e.path.len());

    // ── 根取数单元：pipeline 用（已剥离跨源关系的）fetch AST，后处理用完整 AST ──
    let root_plan = build_plan(fetch_ast, &post_ast, &inject, &params, registry, ctx)?;

    let mut sources = vec![json!({
        "key": "0",
        "source": root_loc.source,
        "namespace": root_loc.namespace.map(|s| json!(s)).unwrap_or(Value::Null),
        "model": root_schema.name,
        "mode": root_plan.mode.as_str(),
        "commands": root_plan.commands,
        "sort": root_plan.sort.clone().unwrap_or(Value::Null),
    })];

    // ── 子取数单元：只取命令序列；后处理统一由根 postprocess 承接 ──
    // 父层级深度升序（稳定排序）：父单元先于子单元，Host 回喂 results 时同序
    let mut unit_order: Vec<usize> = (0..units.len()).collect();
    unit_order.sort_by_key(|&i| units[i].depth);
    for &i in &unit_order {
        let unit = &units[i];
        let mut unit_params = params.clone();
        let plan = plan_query_ast_mut(unit.ast.clone(), &mut unit_params, registry, ctx)?;
        sources.push(json!({
            "key": unit.key,
            "source": unit.source,
            "namespace": unit.namespace.as_ref().map(|s| json!(s)).unwrap_or(Value::Null),
            "model": unit.model,
            "mode": plan.mode.as_str(),
            "commands": plan.commands,
            "sort": plan.sort.clone().unwrap_or(Value::Null),
        }));
    }

    let join_edges: Vec<Value> = edges
        .iter()
        .map(|e| {
            json!({
                "parent": e.parent_model,
                "rel": e.rel,
                "local": e.local,
                "foreign": e.foreign,
                "cardinality": e.cardinality,
                "path": e.path,
                "key": e.key,
            })
        })
        .collect();

    Ok(json!({
        "v": FEDERATION_VERSION,
        "kind": "federated",
        "root": root_schema.name,
        "sources": sources,
        "join": { "type": "hash", "edges": join_edges },
        "postprocess": root_plan.postprocess.clone().unwrap_or(Value::Null),
        "degraded": degraded,
    }))
}

/// 供测试与 Host 参考：把 `sources` 按 key 建索引
pub(crate) fn unit_index(sources: &[Value]) -> HashMap<String, usize> {
    sources
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            s.get("key")
                .and_then(|k| k.as_str())
                .map(|k| (k.to_string(), i))
        })
        .collect()
}
