//! 写路径规划器（对应 JS `crud.js` 的 insertMany / update / updateMany / remove / upsert）
//!
//! 全部为纯函数：core 产出 Command 序列，由 Host 用原生驱动执行；
//! 依赖执行结果的 `returns`（如 findOneAndUpdate 的返回文档）由 Host 回喂
//! `apply_defaults_and_computes` 补默认值。
//!
//! 文件组织：本文件放共享小工具与 re-export；单条写、批量写、upsert 分别见
//! [`single`] / [`many`] / [`upsert`]，对外路径 `crate::command::mutate::*` 不变。

use serde_json::{json, Map, Value};

use crate::permission::{filter_writable_data, Context};
use crate::schema::Schema;
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
/// 对齐 JS `insert`：仅此情形才调用 `_generateId` 消耗随机源）
pub(in crate::command) fn needs_new_id(schema: &Schema, data: &Value) -> bool {
    !data.get("_id").map(is_truthy).unwrap_or(false) && !schema.id_prefix.is_empty()
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
fn build_raw_update(schema: &Schema, ctx: Option<&Context>, data: &Value, now: i64) -> Value {
    let mut data = data.clone();
    if ctx.is_some() {
        if let Some(set_part) = data.get("$set").filter(|v| !v.is_null()).cloned() {
            let filtered = filter_writable_data(schema, ctx, &set_part);
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
fn build_set_data(schema: &Schema, ctx: Option<&Context>, data: &Value) -> Map<String, Value> {
    let src = match ctx {
        Some(_) => filter_writable_data(schema, ctx, data),
        None => data.clone(),
    };
    let mut set_data = object_of(&remove_undefined(&src));
    set_data.remove("_id");
    set_data
}
