//! 写路径规划器（对应 JS `crud.js` 的 insertMany / update / updateMany / remove / upsert）
//!
//! 全部为纯函数：core 产出 Command 序列，由 Host 用原生驱动执行；
//! 依赖执行结果的 `returns`（如 findOneAndUpdate 的返回文档）由 Host 回喂
//! `apply_defaults_and_computes` 补默认值。
//!
//! 文件组织：本文件放共享小工具与 re-export；单条写、批量写、upsert 分别见
//! [`single`] / [`many`] / [`upsert`]，对外路径 `crate::command::mutate::*` 不变。

use serde_json::{json, Map, Value};

use crate::permission::Context;
use crate::rbac::filter_writable_data_overlay;
use crate::schema::{Registry, Schema};
use crate::types::is_truthy;

mod many;
mod single;
mod upsert;

pub use many::{plan_insert_many, plan_update_many};
pub use single::{plan_archive_docs, plan_remove, plan_update};
pub use upsert::plan_upsert;
pub(in crate::command) use upsert::{
    build_upsert_conditions, build_upsert_update, upsert_one_update,
};

/// mutation 关系谓词归一（阶段1 T1-02/T1-04）：条件含 schema 关系名时，
/// 借 §9.6 关系聚合谓词规划（`relation_filter::plan`：读权限 R6/F3 校验 + 代理键改写），
/// 产出 **preCommand（aggregate 取命中 `_id`）**，主命令条件改写为 `_id $in`。
///
/// - SQL 侧：preCommand 由 aggregate 翻译下推为 `EXISTS`（§10.5），主命令 `_id $in`
///   是标量条件 —— 两段均为既有翻译路径，dialect 层零改动；
/// - Mongo 侧：两段原生命令直接可执行 —— 修复原「关系谓词被静默忽略为 no-op」；
/// - `_id $in []`（空集）：各后端均翻译为恒假（不命中任何行），语义 = 无匹配行。
pub(crate) struct RelPredMutationPlan {
    pub lookup_command: Value,
    pub condition: Value,
}

pub(crate) fn plan_rel_pred_mutation(
    schema: &Schema,
    registry: &Registry,
    ctx: Option<&Context>,
    condition: &Value,
) -> Result<RelPredMutationPlan, String> {
    let Some(plan) = crate::pipeline::plan_relation_filter(schema, registry, ctx, condition)?
    else {
        // 调用方仅在确认含关系名键时进入本函数；防御性兜底保持显式
        return Err("内部错误：关系谓词规划返回 None（拒绝静默）".to_string());
    };
    // preCommand pipeline = lookups（挂关系数组）+ $match（代理键改写条件：数组非空 + 标量条件
    // + 权限注入）+ `$_id` 投影 —— 行集由这一步完整计算。
    // 主命令条件 = `_id $in` 占位，**不带代理键**：代理键（`__rp….0 $exists`）只属于 preCommand
    // 的 `$match`；主命令（updateMany/deleteMany 的 SQL 翻译走无关系解析器的 build_filter）
    // 不识别代理键。Host 执行 preCommand 后以真实 `_id` 列表回填占位；
    // `$in: []` 空集在各后端语义一致 = 不命中任何行。
    let mut pipeline = plan.lookups.clone();
    pipeline.push(serde_json::json!({ "$match": plan.condition }));
    pipeline.push(serde_json::json!({ "$project": { "_id": 1 } }));
    let lookup_command = crate::command::cmd::cmd_aggregate(schema, &pipeline);
    let cond = json!({ "_id": { "$in": "__REL_PRED_IDS__" } });
    Ok(RelPredMutationPlan {
        lookup_command,
        condition: cond,
    })
}

// ─── 共享小工具 ──────────────────────────────────────────────
//
// 原 `mutate.rs` 里以 `pub(super)` 暴露给 `crate::command`（供 `mutation.rs`
// 复用），拆分后等价写法为 `pub(in crate::command)`。

/// Host 供给的新 ID 游标（core 无随机源；按规划/生成顺序消费）
pub(in crate::command) struct IdCursor<'a> {
    ids: &'a [String],
    pos: usize,
}

impl<'a> IdCursor<'a> {
    pub(in crate::command) fn new(ids: &'a [String]) -> Self {
        Self { ids, pos: 0 }
    }

    pub(in crate::command) fn next(&mut self) -> Result<&'a str, String> {
        let id = self
            .ids
            .get(self.pos)
            .ok_or_else(|| "Host 供给的 newId 数量不足".to_string())?;
        self.pos += 1;
        Ok(id)
    }
}

/// data 是否需要由 Host 供给新 `_id`（无有效 `_id` 且 schema 配了 idPrefix；
/// 对齐 JS `insert`：仅此情形才调用 `_generateId` 消耗随机源）。
/// 阶段2：`strategy: "autoincrement"` 的 `_id` 由数据库赋值，恒不消耗 Host 随机源。
pub(in crate::command) fn needs_new_id(schema: &Schema, data: &Value) -> bool {
    !data.get("_id").map(is_truthy).unwrap_or(false)
        && !schema.id_prefix.is_empty()
        && !schema.id_is_autoincrement()
}

/// 空条件判断（R4 / B-10/B-12）：`{}`、`null`、以及空逻辑组（`{"$and":[]}` 等）
/// 一律视为无条件 → 批量写（updateMany / remove）不得落全表，必须显式拒绝。
pub(in crate::command) fn is_blank_condition(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Array(a) => a.is_empty(),
        Value::Object(o) if o.is_empty() => true,
        Value::Object(o) => {
            // 空态 = 每一个值都是空数组逻辑组（"$and":[] 等）；出现任一非空数组
            // 或标量/对象谓词即为非空
            !o.iter().any(|(_, v)| match v {
                Value::Array(a) => !a.is_empty(),
                _ => true,
            })
        }
        _ => false,
    }
}

/// JS `_removeUndefined`：剔除对象中的 null 值（JSON 无 undefined），返回新对象
pub(in crate::command) fn remove_undefined(obj: &Value) -> Value {
    match obj.as_object() {
        Some(o) => Value::Object(
            o.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        None => obj.clone(),
    }
}

/// JS `v && String(v).trim()`：真值且（若为字符串）trim 后非空
pub(in crate::command) fn has_trim_str(v: &Value) -> bool {
    is_truthy(v)
        && match v {
            Value::String(s) => !s.trim().is_empty(),
            _ => true,
        }
}

pub(in crate::command) fn object_of(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// data 是否含原生操作符（`$` 开头的 key）
fn has_raw_operators(data: &Value) -> bool {
    data.as_object()
        .map(|o| o.keys().any(|k| k.starts_with('$')))
        .unwrap_or(false)
}

/// JS `{ returnDocument: ReturnDocument.AFTER, ...options }`
fn find_one_and_update_options(options: &Value) -> Value {
    let mut m = Map::new();
    m.insert("returnDocument".to_string(), json!("after"));
    if let Some(o) = options.as_object() {
        for (k, v) in o {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Object(m)
}

/// update/updateMany 共用：原生操作符模式（`$` 开头的 key 透传 $inc/$unset/$addToSet 等）
fn build_raw_update(
    registry: &Registry,
    schema: &Schema,
    ctx: Option<&Context>,
    data: &Value,
    now: i64,
) -> Value {
    let mut data = data.clone();
    if ctx.is_some() {
        if let Some(set_part) = data.get("$set").filter(|v| !v.is_null()).cloned() {
            // RBAC 感知版：静态 writable ∩ RBAC writeFields（无策略时直通静态过滤）
            let filtered = filter_writable_data_overlay(registry, schema, ctx, &set_part);
            data["$set"] = remove_undefined(&filtered);
        }
    }
    if schema.timestamps {
        let set_part = data.get("$set").cloned().unwrap_or_else(|| json!({}));
        let mut merged = object_of(&set_part);
        merged.insert("updatedAt".to_string(), json!(now));
        data["$set"] = Value::Object(merged);
    }
    data
}

/// update/updateMany 共用：$set 模式数据（过滤 + 去 null + 去 _id；
/// 时间戳与空字段检查的先后顺序两种路径不同，由调用方处理）
fn build_set_data(
    registry: &Registry,
    schema: &Schema,
    ctx: Option<&Context>,
    data: &Value,
) -> Map<String, Value> {
    let src = match ctx {
        // RBAC 感知版：静态 writable ∩ RBAC writeFields（无策略时直通静态过滤）
        Some(_) => filter_writable_data_overlay(registry, schema, ctx, data),
        None => data.clone(),
    };
    let mut set_data = object_of(&remove_undefined(&src));
    set_data.remove("_id");
    set_data
}
