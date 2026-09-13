//! 读路径计划：分页解析、两阶段优化、排序还原

use serde_json::{json, Map, Value};

use crate::bson::id_key;
use crate::computes::{collect_field_deps, merge_depends_into_ast, InjectInfo};
use crate::permission::{can_read_schema, is_relation_readable, merge_owner_condition, Context};
use crate::pipeline::{
    build_pipeline, build_projection, flatten_object_fields, is_nullish, param, parse_gql, Ast,
    RelAst, REL_PRED_PREFIX,
};
use crate::schema::{Registry, Schema};
use crate::types::is_truthy;

use super::cmd::{cmd_aggregate, cmd_find, num_value, to_number};
use super::{ensure_context, ERR_PERMISSION, MAX_PAGE_SIZE, PHASE1_IDS};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Page {
    pub page: f64,
    pub page_size: f64,
}

impl Page {
    pub fn to_value(&self) -> Value {
        json!({
            "page": num_value(self.page),
            "pageSize": num_value(self.page_size),
        })
    }

    /// `(page + 1) * pageSize < total`
    pub fn has_more(&self, total: f64) -> bool {
        (self.page + 1.0) * self.page_size < total
    }
}

/// 分页参数解析（对应 JS `_resolvePage`）：page/pageSize 优先，否则由 `$skip/$limit` 反推
pub fn resolve_page(ast: &Ast, params: &Map<String, Value>) -> Page {
    let (page, page_size) = if params.contains_key("page") || params.contains_key("pageSize") {
        let page = match params.get("page").filter(|v| !v.is_null()) {
            Some(v) => to_number(Some(v)).max(0.0).floor(),
            None => 0.0,
        };
        let page_size = match params.get("pageSize").filter(|v| !v.is_null()) {
            Some(v) => to_number(Some(v)),
            None => 50.0,
        };
        (page, page_size)
    } else {
        let skip_val = param(params, ast.params.get("skip"));
        let limit_val = param(params, ast.params.get("limit"));
        let page = if !is_nullish(skip_val) && is_truthy(limit_val.unwrap_or(&Value::Null)) {
            (to_number(skip_val) / to_number(limit_val)).floor()
        } else {
            0.0
        };
        let page_size = if is_nullish(limit_val) {
            50.0
        } else {
            to_number(limit_val)
        };
        (page, page_size)
    };

    Page {
        page,
        page_size: if page_size < MAX_PAGE_SIZE {
            page_size
        } else {
            MAX_PAGE_SIZE
        },
    }
}

/// 读路径形态：决定 Host 如何执行命令序列
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// 纯 `$match` → 走 `find` 快路径
    Find,
    /// `$lookup` + 分页 → 两阶段（先取 ID 再关联）
    TwoPhase,
    /// 标准单阶段聚合
    Aggregate,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Find => "find",
            Mode::TwoPhase => "two_phase",
            Mode::Aggregate => "aggregate",
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub collection: String,
    pub mode: Mode,
    pub commands: Vec<Value>,
    /// 结果回喂 core 做后处理（补默认值 / 递归裁剪 / 剥离注入依赖）所需信息
    pub postprocess: Option<Value>,
    /// 两阶段还原排序用的 `$sort` 阶段
    pub sort: Option<Value>,
}

impl QueryPlan {
    pub fn to_value(&self) -> Value {
        json!({
            "collection": self.collection,
            "mode": self.mode.as_str(),
            "commands": self.commands,
            "postprocess": self.postprocess.clone().unwrap_or(Value::Null),
            "sort": self.sort.clone().unwrap_or(Value::Null),
        })
    }
}

fn postprocess_value(ast: &Ast, inject: &InjectInfo) -> Value {
    json!({
        "ast": ast.to_value(),
        "inject": if inject.is_empty() { Value::Null } else { inject.to_value() },
    })
}

/// 生成读路径命令序列（对应 JS `query` + `_executePipeline` 的路径选择）
pub fn plan_query(
    gql: &str,
    params: &Map<String, Value>,
    registry: &Registry,
    ctx: Option<&Context>,
) -> Result<QueryPlan, String> {
    let mut params = params.clone();
    plan_query_mut(gql, &mut params, registry, ctx)
}

/// 与 [`plan_query`] 相同，但会把 owner 条件注入写回 `params`（供 queryWithCount 复用）
pub fn plan_query_mut(
    gql: &str,
    params: &mut Map<String, Value>,
    registry: &Registry,
    ctx: Option<&Context>,
) -> Result<QueryPlan, String> {
    let ast = parse_gql(gql)?;
    plan_query_ast_mut(ast, params, registry, ctx)
}

/// queryOne：语义为「取第一条」——用户 GQL 未显式给 `$limit` 时强制下推 `$limit(1)`，
/// 大集合不再全量取回后丢弃（对齐 MongoDB `findOne` 的 limit-1 语义）。
///
/// - GQL 已有 `$limit` 时不改写用户意图（取其结果首条）；
/// - 注入键名固定 `__core_one_limit__`（覆盖式写入，防用户 params 键名碰撞）。
pub fn plan_query_one(
    gql: &str,
    params: &Map<String, Value>,
    registry: &Registry,
    ctx: Option<&Context>,
) -> Result<QueryPlan, String> {
    let mut params = params.clone();
    let mut ast = parse_gql(gql)?;
    if !ast.params.contains_key("limit") {
        ast.params
            .insert("limit".to_string(), "@__core_one_limit__".to_string());
        params.insert("__core_one_limit__".to_string(), json!(1));
    }
    plan_query_ast_mut(ast, &mut params, registry, ctx)
}

/// 由**已解析的 AST** 规划读路径（[`plan_query_mut`] 与联邦计划共用同一套语义）
///
/// 含：schema 读权限校验 → owner 条件注入 → asyncFn 依赖注入 → pipeline 构建 → 形态选择。
/// 联邦计划（[`crate::federation`]）会先把跨源关系从 fetch AST 中剥离后再调用
/// [`build_plan`]，因此这里把「取命令」与「取后处理」的 AST 分离开。
pub fn plan_query_ast_mut(
    mut ast: Ast,
    params: &mut Map<String, Value>,
    registry: &Registry,
    ctx: Option<&Context>,
) -> Result<QueryPlan, String> {
    ensure_context(registry, ctx)?;
    let schema = registry.get(&ast.model)?;

    if ctx.is_some() && !can_read_schema(schema, ctx) {
        return Err(ERR_PERMISSION.to_string());
    }

    // 所有者条件注入（非 admin 用户只看自己的数据，无需 $condition 也不能越权读全表）
    if ctx.is_some() {
        if let Some(r) = ast.params.get("condition").cloned() {
            let key = r.get(1..).unwrap_or("").to_string();
            match merge_owner_condition(schema, ctx, params.get(&key).cloned()) {
                Some(v) => {
                    params.insert(key, v);
                }
                None => {
                    params.remove(&key);
                }
            }
        } else if let Some(owner) = merge_owner_condition(schema, ctx, None) {
            // GQL 未显式给 $condition：注入合成 owner 条件为基准 $match，防越权读全表
            ast.params
                .insert("condition".to_string(), "@__core_owner__".to_string());
            params.insert("__core_owner__".to_string(), owner);
        }
    }

    let mut inject = merge_depends_into_ast(&mut ast.relations, schema)?;
    // R7：asyncFn 计算列的普通字段依赖（如 `name`）并入根 AST，供投影取数与
    // process_node 裁剪保留；宿主 asyncFn 执行后由 strip_dep_injected 剥离。
    inject.fields = collect_field_deps(&mut ast.fields, schema);

    // `postprocess.ast` 取「展平后、含全部关系」的快照（build_pipeline 会原地展平 fetch ast）
    let mut post_ast = ast.clone();
    flatten_object_fields(&mut post_ast, schema);

    build_plan(ast, &post_ast, &inject, params, registry, ctx)
}

/// T2 / L1 / L2 / L5 / L6：递归校验 GQL **显式请求的关系**可读性。
///
/// 判定口径（R0 决策 #1 / #6，与表级策略一致）：关系 `read` 可读 **∧** 目标 model
/// `can_read_schema` 可读；任一不通过 → `Err(ERR_PERMISSION)`（不再静默省略 / 降级）。
/// 计算列 `depends` 自动引入的关系（`merge_depends_into_ast`）同样在此覆盖（L6）。
/// `ctx = None` 时放行（fail-open，与默认姿态一致）。
pub fn check_readable_relations(
    relations: &[(String, RelAst)],
    schema: &Schema,
    registry: &Registry,
    ctx: Option<&Context>,
) -> Result<(), String> {
    if ctx.is_none() {
        return Ok(());
    }
    for (rel_name, rel_ast) in relations {
        let Some(rel_def) = schema.relations.get(rel_name) else {
            continue;
        };
        // 未声明 model 的「关系」不是关联（与 federation `walk` 一致），按普通字段处理
        if rel_def.model.is_empty() {
            continue;
        }
        if !is_relation_readable(schema, ctx, rel_name) {
            return Err(ERR_PERMISSION.to_string());
        }
        let rel_schema = registry.get(&rel_def.model)?;
        if !can_read_schema(rel_schema, ctx) {
            return Err(ERR_PERMISSION.to_string());
        }
        check_readable_relations(&rel_ast.relations, rel_schema, registry, ctx)?;
    }
    Ok(())
}

/// 由 AST 构建 [`QueryPlan`]：pipeline 用 `fetch_ast`，后处理用 `post_ast`。
///
/// 单库路径下两者相同；联邦路径下 `fetch_ast` 已剥离跨源关系（该源下推不到），
/// `post_ast` 仍保留全部关系，供 [`crate::command::finalize_query`] 收尾时
/// 递归下钻 / 权限裁剪 / 计算列（对齐联邦契约「postprocess 与单库同形状」）。
pub fn build_plan(
    mut fetch_ast: Ast,
    post_ast: &Ast,
    inject: &InjectInfo,
    params: &Map<String, Value>,
    registry: &Registry,
    ctx: Option<&Context>,
) -> Result<QueryPlan, String> {
    ensure_context(registry, ctx)?;
    let schema = registry.get(&fetch_ast.model)?;

    // T2 / L1 / L6：本单元显式请求的关系可读性（含 depends 自动引入的关系）——
    // 关系不可读 → Err，绝不静默省略（§9.1 R6 / D2 归一）。
    check_readable_relations(&fetch_ast.relations, schema, registry, ctx)?;

    let pipeline = build_pipeline(&mut fetch_ast, params, registry, ctx)?;
    let stages: Vec<Value> = pipeline.as_array().cloned().unwrap_or_default();

    // 根级 `$group`：命令自身已含分组投影与分组后分页（§9.3 固定序），
    // 不能再走两阶段优化 / 追加普通 `$project`；且**分组行不是普通文档**，
    // 不可做关系下钻 / 计算列 / 默认值后处理 → `postprocess` 置空。
    let grouped = stages.iter().any(|s| s.get("$group").is_some());

    let projection = if grouped {
        None
    } else {
        build_projection(&fetch_ast, schema, ctx)
    };

    let collection = schema.collection.clone();
    let post = || {
        if grouped {
            None
        } else {
            Some(postprocess_value(post_ast, inject))
        }
    };

    // ── 纯 $match 无关联 → find 快路径 ──
    if stages.len() == 1 {
        if let Some(filter) = stages[0].get("$match") {
            return Ok(QueryPlan {
                collection: collection.clone(),
                mode: Mode::Find,
                commands: vec![cmd_find(schema, filter, projection.as_ref())],
                postprocess: post(),
                sort: None,
            });
        }
    }

    // ── 两阶段优化（$lookup + $skip/$limit，且 sort 未引用关联字段） ──
    // §9.6 关系聚合谓词 `$lookup`（`as` = `__rp…`）属于 `$condition` 段，必须留在阶段一
    // 过滤中，故不视作「关联下钻」的起点（否则谓词过滤被推到阶段二，分页计数失真）。
    let first_lookup = stages.iter().position(|s| {
        s.get("$lookup")
            .map(|lo| {
                !lo.get("as")
                    .and_then(|v| v.as_str())
                    .map(|a| a.starts_with(REL_PRED_PREFIX))
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    });
    let has_skip_limit = stages
        .iter()
        .any(|s| s.get("$skip").is_some() || s.get("$limit").is_some());
    let sort_stage = stages.iter().find(|s| s.get("$sort").is_some()).cloned();

    if let Some(idx) = first_lookup {
        if !grouped && has_skip_limit && !sorts_by_relation(sort_stage.as_ref()) {
            let mut id_pipeline: Vec<Value> = stages[..idx].to_vec();
            for key in ["$sort", "$skip", "$limit"] {
                if let Some(st) = stages.iter().find(|s| s.get(key).is_some()) {
                    id_pipeline.push(st.clone());
                }
            }
            id_pipeline.push(json!({ "$project": { "_id": 1 } }));

            let mut full: Vec<Value> = stages[idx..]
                .iter()
                .filter(|s| {
                    !(s.get("$sort").is_some()
                        || s.get("$skip").is_some()
                        || s.get("$limit").is_some())
                })
                .cloned()
                .collect();
            full.insert(0, json!({ "$match": { "_id": { "$in": PHASE1_IDS } } }));
            if let Some(p) = projection.as_ref() {
                full.push(json!({ "$project": p }));
            }

            return Ok(QueryPlan {
                collection: collection.clone(),
                mode: Mode::TwoPhase,
                commands: vec![
                    cmd_aggregate(schema, &id_pipeline),
                    cmd_aggregate(schema, &full),
                ],
                postprocess: post(),
                sort: sort_stage,
            });
        }
    }

    // ── 标准单阶段聚合 ──
    let mut final_stages = stages;
    if let Some(p) = projection.as_ref() {
        final_stages.push(json!({ "$project": p }));
    }

    Ok(QueryPlan {
        collection,
        mode: Mode::Aggregate,
        commands: vec![cmd_aggregate(schema, &final_stages)],
        postprocess: post(),
        sort: None,
    })
}

/// pipeline 的 `$sort` 是否引用关联表点号字段（如 `bidders.amount`）
pub fn sorts_by_relation(sort_stage: Option<&Value>) -> bool {
    sort_stage
        .and_then(|s| s.get("$sort"))
        .and_then(|s| s.as_object())
        .map(|o| o.keys().any(|k| k.contains('.')))
        .unwrap_or(false)
}

/// 两阶段查询后按阶段一 `_id` 顺序重排（对应 JS `_restoreSortOrder`）
pub fn restore_sort_order(items: &mut [Value], ids: &[Value], sort_stage: Option<&Value>) {
    if sort_stage.is_none() || ids.len() <= 1 {
        return;
    }
    // 阶段一 id → 位次索引（首个命中优先，保留 `position` 的重复 id 语义）；
    // O(n+m) 替代逐项线性扫描的 O(n·m)
    let mut index: std::collections::HashMap<_, usize> =
        std::collections::HashMap::with_capacity(ids.len());
    for (i, id) in ids.iter().enumerate() {
        index.entry(id_key(id)).or_insert(i);
    }
    let keyed: Vec<(usize, Value)> = items
        .iter()
        .map(|it| {
            let k = id_key(it.get("_id").unwrap_or(&Value::Null));
            let pos = index.get(&k).copied().unwrap_or(ids.len());
            (pos, it.clone())
        })
        .collect();
    let mut sorted = keyed;
    sorted.sort_by_key(|(pos, _)| *pos);
    for (slot, (_, v)) in items.iter_mut().zip(sorted) {
        *slot = v;
    }
}
