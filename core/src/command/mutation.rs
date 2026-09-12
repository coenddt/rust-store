//! mutation 规划器（对应 JS `_mutationOne` + `_applyRelations`）
//!
//! 把一次 mutation 展开为**有序步骤序列**：每个步骤是一条命令（insertOne 或
//! findOneAndUpdate），父子步骤间的依赖（子文档外键 = 父文档 `_id`）用占位符表达：
//!
//! - `{{step.<N>._id}}`（见 [`step_id_placeholder`]）：第 N 步执行结果文档的 `_id`，
//!   Host 在该步执行完成后回填到后续命令中
//! - 需要新生成 `_id` 的位置由 Host 供给的 `new_ids` 按序填充（与 JS `_generateId`
//!   的调用顺序一致；core 无随机源）
//!
//! 根步骤永远是 `steps[0]`；Host 依次执行后，用根步骤结果文档调
//! `apply_defaults_and_computes` 得到最终返回值。

use serde_json::{json, Map, Value};

use crate::permission::{can_write_schema, evaluate, filter_writable_data, Context, Doc};
use crate::schema::Registry;

use super::cmd::{cmd_find_one_and_update, cmd_insert_one};
use super::mutate::{
    build_upsert_conditions, build_upsert_update, needs_new_id, upsert_one_update, IdCursor,
    object_of,
};
use super::write::build_insert_doc;
use super::{step_id_placeholder, ERR_NO_WRITE};

/// 规划一条 mutation（对应 JS `mutation` 的单条分支 `_mutationOne`）。
///
/// 数组形态与空数组短路（`[]` / `null`）由 Host 处理：逐条调用本函数即可；
/// `new_ids` 由多条 mutation 共享一个游标语义（每次调用传剩余数组）。
pub fn plan_mutation(
    schema_name: &str,
    registry: &Registry,
    ctx: Option<&Context>,
    data: &Value,
    now: i64,
    new_ids: &[String],
) -> Result<Value, String> {
    let mut steps: Vec<Value> = Vec::new();
    let mut ids = IdCursor::new(new_ids);
    plan_mutation_node(schema_name, registry, ctx, data, now, &mut ids, &mut steps)?;
    Ok(json!({ "steps": steps }))
}

/// 展开一个 mutation 节点：根写入步骤 + 各 relation 子步骤（`many` 递归）
fn plan_mutation_node(
    schema_name: &str,
    registry: &Registry,
    ctx: Option<&Context>,
    data: &Value,
    now: i64,
    ids: &mut IdCursor,
    steps: &mut Vec<Value>,
) -> Result<(), String> {
    let schema = registry.get(schema_name)?;
    if !can_write_schema(schema, ctx) {
        return Err(ERR_NO_WRITE.to_string());
    }
    let Some(obj) = data.as_object() else {
        return Err("mutation 数据必须是对象".to_string());
    };

    // ── 1. 按 schema.relations 拆分 fieldData + relationData（保持遍历顺序） ──
    let mut field_data = Map::new();
    let mut relation_data: Vec<(&String, &Value)> = Vec::new();
    for (key, val) in obj {
        if let Some(rel_def) = schema.relations.get(key) {
            // JS：relDef.read && !evaluate(ctx, relDef.read) → 跳过该关系
            let allowed = match ctx {
                None => true,
                Some(c) => match &rel_def.read {
                    Some(rl) => evaluate(Some(c), Some(rl), Doc::Missing),
                    None => true,
                },
            };
            if allowed {
                relation_data.push((key, val));
            }
        } else {
            field_data.insert(key.clone(), val.clone());
        }
    }

    let filtered_field = match ctx {
        Some(_) => filter_writable_data(schema, ctx, &Value::Object(field_data)),
        None => Value::Object(field_data),
    };

    // ── 2/3. 写入 parent（upsert 或 insert 路径在规划期即可确定） ──
    let step_idx = steps.len();
    let parent_ph = step_id_placeholder(step_idx);
    let or_conditions = build_upsert_conditions(schema, &filtered_field);
    let command = if or_conditions.is_empty() {
        // Insert 路径（复用 JS `insert` 逻辑；仅需要新 `_id` 时才消耗游标）
        let new_id = if needs_new_id(schema, &filtered_field) {
            ids.next()?
        } else {
            ""
        };
        let doc = build_insert_doc(schema, ctx, &filtered_field, now, new_id)?;
        cmd_insert_one(schema, &Value::Object(doc))
    } else {
        // Upsert 路径（JS `_buildUpsertUpdate` 仅在无 _id 时才 `_generateId`）
        let new_id = if needs_new_id(schema, &filtered_field) {
            ids.next()?
        } else {
            ""
        };
        let update_doc = build_upsert_update(schema, &filtered_field, new_id, now);
        let fu_options = json!({ "upsert": true, "returnDocument": "after" });
        cmd_find_one_and_update(
            schema,
            &json!({ "$or": or_conditions }),
            &update_doc,
            &fu_options,
        )
    };
    steps.push(json!({ "model": schema_name, "command": command }));

    // ── 4. 处理 relation 子文档 ──
    for (rel_name, rel_val) in relation_data {
        if rel_val.is_null() {
            continue;
        }
        let rel_def = &schema.relations[rel_name];
        let rel_schema = registry.get(&rel_def.model)?;

        if rel_def.rel_type == "one" {
            // type:'one' 子文档强制按 foreignKey upsert（无权限过滤，对齐 JS `_upsertOne`）
            let mut child = object_of(rel_val);
            child.insert(rel_def.foreign_field.clone(), json!(parent_ph));
            let child_val = Value::Object(child);
            // JS `_upsertOne`：仅无 _id 时才 `_generateId`
            let new_id = if needs_new_id(rel_schema, &child_val) {
                ids.next()?
            } else {
                ""
            };
            let update_doc = upsert_one_update(rel_schema, &child_val, new_id, now);
            let filter = json!({ rel_def.foreign_field.clone(): parent_ph });
            let fu_options = json!({ "upsert": true, "returnDocument": "after" });
            let cmd =
                cmd_find_one_and_update(rel_schema, &filter, &update_doc, &fu_options);
            steps.push(json!({ "model": rel_def.model, "command": cmd }));
        } else if rel_def.rel_type == "many" {
            let arr: Vec<Value> = match rel_val {
                Value::Array(a) => a.clone(),
                _ => vec![rel_val.clone()],
            };
            for child in arr {
                if child.is_null() {
                    continue;
                }
                let mut c = object_of(&child);
                c.insert(rel_def.foreign_field.clone(), json!(parent_ph));
                plan_mutation_node(&rel_def.model, registry, ctx, &Value::Object(c), now, ids, steps)?;
            }
        }
        // 其他 type：JS 无分支，静默跳过
    }

    Ok(())
}
