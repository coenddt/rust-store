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

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::command::{build_plan, plan_query_ast_mut, ERR_PERMISSION};
use crate::computes::{merge_depends_into_ast, InjectInfo};
use crate::datasource::{DataSource, DataSourceConfig};
use crate::permission::{can_read_schema, merge_owner_condition, Context};
use crate::pipeline::{flatten_object_fields, is_nullish, param, parse_gql, Ast, RelAst};
use crate::schema::{Registry, Schema};

/// 契约版本：形状变更必须升版并附迁移说明
pub const FEDERATION_VERSION: u64 = 2;

/// 定位二元组（source + namespace）
#[derive(Debug, Clone, PartialEq)]
struct Loc {
    source: String,
    namespace: Option<String>,
}

fn loc_of(schema: &Schema) -> Loc {
    Loc {
        source: schema.source().to_string(),
        namespace: schema.namespace.clone(),
    }
}

/// 同源下推判定：
/// - 跨 source → 不下推（内存 join）；
/// - 双方都是 SQL → 下推（同/跨 namespace 都行，qualified 表名）；
/// - 其余（Mongo，或 kind 未知）→ 仅同 namespace 下推（`$lookup` 不能跨 db；
///   kind 未知时保守不跨 ns 下推，宁拆勿错）。
fn can_pushdown(parent: &Schema, child: &Schema, ds_cfg: &DataSourceConfig) -> Result<bool, String> {
    if parent.source() != child.source() {
        return Ok(false);
    }
    let pds = ds_cfg.resolve(parent.datasource.as_deref()).ok();
    let cds = ds_cfg.resolve(child.datasource.as_deref()).ok();
    Ok(match (pds, cds) {
        (Some(DataSource::Sql(_)), Some(DataSource::Sql(_))) => true,
        _ => parent.namespace == child.namespace,
    })
}

/// 确保 `field` 出现在请求字段里（跨源 join 的键必须取回才能内存 join）
///
/// `_id` 由 projection 无条件带回，无需显式追加。
fn ensure_field(fields: &mut Vec<String>, field: &str) {
    if field.is_empty() || field == "_id" {
        return;
    }
    if !fields.iter().any(|f| f == field) {
        fields.push(field.to_string());
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

/// 递归拆源：就地剥离跨源关系，产出子取数单元与 join 边
///
/// `fields` / `relations` 是**当前层**的取数 AST 片段（父模型视角）。
fn walk(
    parent_model: &str,
    ds_cfg: &DataSourceConfig,
    fields: &mut Vec<String>,
    relations: &mut Vec<(String, RelAst)>,
    path: &[String],
    params: &Map<String, Value>,
    registry: &Registry,
    units: &mut Vec<UnitSpec>,
    edges: &mut Vec<EdgeSpec>,
    degraded: &mut Vec<Value>,
) -> Result<(), String> {
    let parent_schema = registry.get(parent_model)?.clone();

    let mut i = 0usize;
    while i < relations.len() {
        let name = relations[i].0.clone();
        let Some(rel_def) = parent_schema.relations.get(&name).cloned() else {
            i += 1;
            continue;
        };
        // 未声明 model 的「关系」不是关联，按普通字段处理
        if rel_def.model.is_empty() {
            i += 1;
            continue;
        }

        let child_schema = registry.get(&rel_def.model)?.clone();
        let child_loc = loc_of(&child_schema);

        // 先递归处理更深层：深层跨源关系会从本关系里剥离并各自成单元
        let mut child_path = path.to_vec();
        child_path.push(name.clone());
        {
            let (_, rel_ast) = &mut relations[i];
            walk(
                &rel_def.model,
                ds_cfg,
                &mut rel_ast.fields,
                &mut rel_ast.relations,
                &child_path,
                params,
                registry,
                units,
                edges,
                degraded,
            )?;
        }

        if can_pushdown(&parent_schema, &child_schema, ds_cfg)? {
            // 同源（SQL 同源含跨 namespace；Mongo 同源同库）：留在本层，由该源 $lookup / JOIN 下推
            i += 1;
            continue;
        }

        // 跨源：cardinality 必须明确（决定还原为对象还是数组），否则早失败
        if rel_def.rel_type != "one" && rel_def.rel_type != "many" {
            return Err(format!(
                "跨源关系 {}.{} 的 cardinality 非法（须为 one/many，实际 {:?}）",
                parent_model, name, rel_def.rel_type
            ));
        }

        let (_, rel_ast) = relations.remove(i);

        // 父侧 join 键（local）必须取回
        ensure_field(fields, &rel_def.local_field);

        // 子侧取数 AST：根为子模型，字段取自关系节点，join 键（foreign）必须取回
        let mut child_ast = Ast {
            model: rel_def.model.clone(),
            params: rel_ast.params.clone(),
            fields: rel_ast.fields.clone(),
            relations: rel_ast.relations.clone(),
        };
        ensure_field(&mut child_ast.fields, &rel_def.foreign_field);

        // 关系自带的分页/排序无法「按父」下推 → 降级（Host 兜底，绝不静默错误）
        for key in ["sort", "skip", "limit"] {
            if let Some(r) = rel_ast.params.get(key) {
                if !is_nullish(param(params, Some(r))) {
                    degraded.push(json!({
                        "code": "crossSourceChildPaging",
                        "layer": "federation",
                        "message": format!(
                            "跨源关系 {}.{} 的 ${} 无法按父下推",
                            parent_model, name, key
                        ),
                        "hint": "把该关系的 $sort/$skip/$limit 上移到根查询，或由 Host 在内存 join 后自行分页",
                    }));
                }
            }
        }

        // 单元 key 只是标识（不是位置），因此后续按深度重排不影响 join 边引用
        let key = (units.len() + 1).to_string();
        let depth = path.len();
        units.push(UnitSpec {
            key: key.clone(),
            source: child_loc.source.clone(),
            namespace: child_loc.namespace.clone(),
            model: rel_def.model.clone(),
            ast: child_ast,
            depth,
        });
        edges.push(EdgeSpec {
            parent_model: parent_model.to_string(),
            rel: name,
            local: rel_def.local_field.clone(),
            foreign: rel_def.foreign_field.clone(),
            cardinality: rel_def.rel_type.clone(),
            path: path.to_vec(),
            key,
        });
        // remove 后同位置即下一元素，索引不前进
    }

    Ok(())
}

/// 根查询 `$sort` 是否引用跨源关系字段（该源内不存在该字段 → 无法下推）
fn detect_cross_source_sort(
    fetch_ast: &Ast,
    params: &Map<String, Value>,
    edges: &[EdgeSpec],
    degraded: &mut Vec<Value>,
) {
    let cross_root_rels: Vec<&str> = edges
        .iter()
        .filter(|e| e.path.is_empty())
        .map(|e| e.rel.as_str())
        .collect();
    if cross_root_rels.is_empty() {
        return;
    }
    let Some(r) = fetch_ast.params.get("sort") else {
        return;
    };
    let Some(Value::Object(sort)) = param(params, Some(r)) else {
        return;
    };
    for k in sort.keys() {
        let prefix = k.split('.').next().unwrap_or("");
        if cross_root_rels.contains(&prefix) {
            degraded.push(json!({
                "code": "crossSourceSort",
                "layer": "federation",
                "message": format!("跨源排序无法下推: {}", k),
                "hint": "排序改用本源字段，或由 Host 在内存 join 后自行排序",
            }));
        }
    }
}

/// 生成联邦计划（纯逻辑）
///
/// `ds_config`：`{ "sources": { name: kind } }`（Host `init` 时的 kind 配置；
/// `null` = 单源 Mongo）。下推判定依赖它区分 SQL / Mongo（见 [`can_pushdown`]）。
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
        .filter_map(|(i, s)| s.get("key").and_then(|k| k.as_str()).map(|k| (k.to_string(), i)))
        .collect()
}
