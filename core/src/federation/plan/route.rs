//! 递归拆源：判定下推 / 剥离跨源关系 / 收集降级项。

use serde_json::{json, Map, Value};

use crate::datasource::{DataSource, DataSourceConfig};
use crate::pipeline::{is_nullish, param, Ast, RelAst};
use crate::schema::{Registry, Schema};

use super::{loc_of, EdgeSpec, UnitSpec};

/// 同源下推判定：
/// - 跨 source → 不下推（内存 join）；
/// - 双方都是 SQL → 下推（同/跨 namespace 都行，qualified 表名）；
/// - 其余（Mongo，或 kind 未知）→ 仅同 namespace 下推（`$lookup` 不能跨 db；
///   kind 未知时保守不跨 ns 下推，宁拆勿错）。
pub(super) fn can_pushdown(
    parent: &Schema,
    child: &Schema,
    ds_cfg: &DataSourceConfig,
) -> Result<bool, String> {
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

/// 递归拆源：就地剥离跨源关系，产出子取数单元与 join 边
///
/// `fields` / `relations` 是**当前层**的取数 AST 片段（父模型视角）。
/// `units` / `edges` / `degraded` 为递归累加的出参（federation 规划全程单线程），
/// 拆散到 struct 反而模糊「就地累加」语义，保持位置参数。
#[allow(clippy::too_many_arguments)]
pub(super) fn walk(
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
pub(super) fn detect_cross_source_sort(
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
