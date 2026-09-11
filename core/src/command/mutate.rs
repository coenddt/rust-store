//! 写路径规划器（对应 JS `crud.js` 的 insertMany / update / updateMany / remove / upsert）
//!
//! 全部为纯函数：core 产出 Command 序列，由 Host 用原生驱动执行；
//! 依赖执行结果的 `returns`（如 findOneAndUpdate 的返回文档）由 Host 回喂
//! `apply_defaults_and_computes` 补默认值。

use serde_json::{json, Map, Value};

use crate::computes::{apply_defaults_and_computes, FnRegistry};
use crate::permission::{can_write_schema, filter_writable_data, Context};
use crate::schema::{Registry, Schema};
use crate::types::is_truthy;

use super::cmd::{
    cmd_delete_many, cmd_find, cmd_find_one_and_update, cmd_insert_many, cmd_update_many,
};
use super::write::{build_insert_doc, check_write_perm, has_creator_permission, Probe};
use super::ERR_NO_WRITE;

// ─── 共享小工具 ──────────────────────────────────────────────

/// Host 供给的新 ID 游标（core 无随机源；按规划/生成顺序消费）
pub(super) struct IdCursor<'a> {
    ids: &'a [String],
    pos: usize,
}

impl<'a> IdCursor<'a> {
    pub(super) fn new(ids: &'a [String]) -> Self {
        Self { ids, pos: 0 }
    }

    pub(super) fn next(&mut self) -> Result<&'a str, String> {
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
pub(super) fn needs_new_id(schema: &Schema, data: &Value) -> bool {
    !data.get("_id").map(is_truthy).unwrap_or(false) && !schema.id_prefix.is_empty()
}

/// JS `_removeUndefined`：剔除对象中的 null 值（JSON 无 undefined），返回新对象
pub(super) fn remove_undefined(obj: &Value) -> Value {
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
pub(super) fn has_trim_str(v: &Value) -> bool {
    is_truthy(v)
        && match v {
            Value::String(s) => !s.trim().is_empty(),
            _ => true,
        }
}

pub(super) fn object_of(v: &Value) -> Map<String, Value> {
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

// ─── 批量插入 ────────────────────────────────────────────────

/// 批量插入（对应 JS `insertMany`）。
///
/// 需要生成 `_id` 的文档按顺序消费 `new_ids`（与 JS `_generateId` 的调用顺序一致）；
/// `new_ids` 不足时报错，Host 应按数据规模多备一些（剩余忽略）。
pub fn plan_insert_many(
    schema_name: &str,
    registry: &Registry,
    ctx: Option<&Context>,
    docs: &[Value],
    now: i64,
    new_ids: &[String],
    fn_registry: Option<&dyn FnRegistry>,
) -> Result<Value, String> {
    // JS：非数组/空数组直接返回 []，不产生命令
    if docs.is_empty() {
        return Ok(json!({ "command": Value::Null, "returns": [] }));
    }
    let schema = registry.get(schema_name)?;
    if !can_write_schema(schema, ctx) {
        return Err(ERR_NO_WRITE.to_string());
    }

    let mut ids = IdCursor::new(new_ids);
    let mut processed = Vec::with_capacity(docs.len());
    for data in docs {
        // 仅需要生成 `_id` 的文档才消耗游标（对齐 JS `_generateId` 的调用次数）
        let new_id = if needs_new_id(schema, data) {
            ids.next()?
        } else {
            ""
        };
        let doc = build_insert_doc(schema, ctx, data, now, new_id)?;
        processed.push(Value::Object(doc));
    }
    let returns = processed
        .iter()
        .map(|d| apply_defaults_and_computes(d, schema, fn_registry))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({
        "command": cmd_insert_many(&schema.collection, &processed),
        "returns": returns,
    }))
}

// ─── 更新 ────────────────────────────────────────────────────

/// 更新一条（对应 JS `update`，`findOneAndUpdate` + returnDocument AFTER）。
///
/// creator 写权限需探针时返回 `{"needsProbe": cmd}`；Host 执行探针后携
/// [`Probe::Found`] / [`Probe::NoResult`] 重入即得 `{"command": cmd}`。
/// `options` 为用户透传选项（如 upsert / arrayFilters）。
pub fn plan_update(
    schema_name: &str,
    registry: &Registry,
    ctx: Option<&Context>,
    condition: &Value,
    data: &Value,
    options: &Value,
    now: i64,
    probe: Probe,
) -> Result<Value, String> {
    let schema = registry.get(schema_name)?;
    if let Some(cmd) = check_write_perm(schema, ctx, condition, "无写入权限", probe)? {
        return Ok(json!({ "needsProbe": cmd }));
    }

    let update_doc = if has_raw_operators(data) {
        build_raw_update(schema, ctx, data, now)
    } else {
        let mut set_data = build_set_data(schema, ctx, data);
        // JS：空字段检查在追加时间戳之前
        if set_data.is_empty() {
            return Err("没有提供要更新的字段".to_string());
        }
        if schema.timestamps {
            set_data.insert("updatedAt".to_string(), json!(now));
        }
        json!({ "$set": Value::Object(set_data) })
    };

    let command = cmd_find_one_and_update(
        &schema.collection,
        condition,
        &update_doc,
        &find_one_and_update_options(options),
    );
    Ok(json!({ "command": command }))
}

/// 批量更新（对应 JS `updateMany`）。guest / 无写授权直接拒绝，不走 creator 探针。
pub fn plan_update_many(
    schema_name: &str,
    registry: &Registry,
    ctx: Option<&Context>,
    condition: &Value,
    data: &Value,
    now: i64,
) -> Result<Value, String> {
    let schema = registry.get(schema_name)?;
    if let Some(c) = ctx {
        let guest = c
            .roles
            .clone()
            .unwrap_or_default()
            .iter()
            .any(|r| r == "guest");
        if guest || !can_write_schema(schema, ctx) {
            return Err("无批量写入权限".to_string());
        }
    }

    let update_doc = if has_raw_operators(data) {
        build_raw_update(schema, ctx, data, now)
    } else {
        let mut set_data = build_set_data(schema, ctx, data);
        if schema.timestamps {
            set_data.insert("updatedAt".to_string(), json!(now));
        }
        json!({ "$set": Value::Object(set_data) })
    };

    let command = cmd_update_many(&schema.collection, condition, &update_doc);
    Ok(json!({ "command": command }))
}

// ─── 删除（归档） ────────────────────────────────────────────

/// 删除计划（对应 JS `remove`）：
/// 归档表存在时返回 find 命令（Host 取源文档后调 [`plan_archive_docs`]）+ deleteMany 命令。
pub fn plan_remove(
    schema_name: &str,
    registry: &Registry,
    ctx: Option<&Context>,
    condition: &Value,
    probe: Probe,
) -> Result<Value, String> {
    let schema = registry.get(schema_name)?;
    if let Some(cmd) = check_write_perm(schema, ctx, condition, "无删除权限", probe)? {
        return Ok(json!({ "needsProbe": cmd }));
    }

    let archive_name = format!("{}Deleted", schema_name);
    let (archive_collection, find_command) = if registry.has(&archive_name) {
        let arch = registry.get(&archive_name)?;
        (
            json!(arch.collection),
            Some(cmd_find(&schema.collection, condition, None)),
        )
    } else {
        (Value::Null, None)
    };
    let delete_command = cmd_delete_many(&schema.collection, condition);
    Ok(json!({
        "archiveCollection": archive_collection,
        "findCommand": find_command,
        "deleteCommand": delete_command,
    }))
}

/// 归档文档命令：源文档补 `deletedAt` 后批量写入 `<collection>_deleted`
/// （对应 JS `remove` 内的归档段；Host 仅在 find 有结果时调用）。
pub fn plan_archive_docs(
    schema_name: &str,
    registry: &Registry,
    docs: &[Value],
    now: i64,
) -> Result<Value, String> {
    let archive_name = format!("{}Deleted", schema_name);
    let arch = registry.get(&archive_name)?;
    let archived: Vec<Value> = docs
        .iter()
        .map(|d| {
            let mut o = object_of(d);
            o.insert("deletedAt".to_string(), json!(now));
            Value::Object(o)
        })
        .collect();
    Ok(json!({
        "command": cmd_insert_many(&arch.collection, &archived),
    }))
}

// ─── Upsert ──────────────────────────────────────────────────

/// 构建 upsert 条件组（对应 JS `_buildUpsertConditions`，`$or` 数组）：
/// 1. `data._id` 非空 → `{_id}`；2. unique 索引 keys 在 data 中均非空 → 整组加入
pub(super) fn build_upsert_conditions(schema: &Schema, data: &Value) -> Vec<Value> {
    let mut conditions = Vec::new();

    if let Some(id) = data.get("_id") {
        if has_trim_str(id) {
            conditions.push(json!({ "_id": id }));
        }
    }

    for idx in &schema.indexes {
        let options = idx.get("options");
        // JS：!options || !options.unique → 跳过
        let unique = options.and_then(|o| o.get("unique"));
        if !unique.map(is_truthy).unwrap_or(false) {
            continue;
        }
        let Some(keys_obj) = idx.get("keys").and_then(|k| k.as_object()) else {
            continue;
        };
        let all_present = keys_obj.keys().all(|k| match data.get(k) {
            // JS：v !== null && v !== undefined && !(typeof v === 'string' && !v.trim())
            None => false,
            Some(v) if v.is_null() => false,
            Some(Value::String(s)) => !s.trim().is_empty(),
            Some(_) => true,
        });
        if all_present {
            let mut cond = Map::new();
            for k in keys_obj.keys() {
                cond.insert(k.clone(), data.get(k).cloned().unwrap_or(Value::Null));
            }
            conditions.push(Value::Object(cond));
        }
    }

    conditions
}

/// 由 fieldData 生成 upsert 的 updateDoc（对应 JS `_buildUpsertUpdate`，
/// mutation 根文档 upsert 路径使用；含 createdBy 自动填充）
pub(super) fn build_upsert_update(
    schema: &Schema,
    field_data: &Value,
    new_id: &str,
    now: i64,
) -> Value {
    let mut set_data = object_of(&remove_undefined(field_data));
    let mut set_on_insert = Map::new();

    // _id：有则 $setOnInsert（不 $set，避免修改已有文档的 _id）
    if let Some(id) = set_data.get("_id").cloned() {
        if has_trim_str(&id) {
            set_on_insert.insert("_id".to_string(), id);
        }
    } else if !schema.id_prefix.is_empty() {
        set_on_insert.insert("_id".to_string(), json!(new_id));
    }
    set_data.remove("_id");

    if schema.timestamps {
        set_data.insert("updatedAt".to_string(), json!(now));
        let created = field_data
            .get("createdAt")
            .cloned()
            .unwrap_or_else(|| json!(now));
        set_on_insert.insert("createdAt".to_string(), created);
    }
    set_data.remove("createdAt");

    // 自动设置 createdBy（upsert 新文档时）
    // JS：setOnInsert.createdBy = setOnInsert._id（_id 缺席时为 undefined，JSON 序列化会丢弃该键）
    if has_creator_permission(schema)
        && !set_on_insert
            .get("createdBy")
            .map(is_truthy)
            .unwrap_or(false)
    {
        if let Some(v) = set_on_insert.get("_id").cloned() {
            set_on_insert.insert("createdBy".to_string(), v);
        }
    }

    upsert_update_doc(set_data, set_on_insert)
}

/// type:'one' 子文档的 updateDoc（对应 JS `_upsertOne`，无 createdBy 自动填充）
pub(super) fn upsert_one_update(schema: &Schema, data: &Value, new_id: &str, now: i64) -> Value {
    let mut set_data = object_of(&remove_undefined(data));
    let mut set_on_insert = Map::new();

    if let Some(id) = set_data.get("_id").cloned() {
        if has_trim_str(&id) {
            set_on_insert.insert("_id".to_string(), id);
        }
    } else if !schema.id_prefix.is_empty() {
        set_on_insert.insert("_id".to_string(), json!(new_id));
    }
    set_data.remove("_id");

    if schema.timestamps {
        set_data.insert("updatedAt".to_string(), json!(now));
        let created = data
            .get("createdAt")
            .cloned()
            .unwrap_or_else(|| json!(now));
        set_on_insert.insert("createdAt".to_string(), created);
    }
    set_data.remove("createdAt");

    upsert_update_doc(set_data, set_on_insert)
}

fn upsert_update_doc(set_data: Map<String, Value>, set_on_insert: Map<String, Value>) -> Value {
    let mut update = Map::new();
    update.insert("$set".to_string(), Value::Object(set_data));
    if !set_on_insert.is_empty() {
        update.insert("$setOnInsert".to_string(), Value::Object(set_on_insert));
    }
    Value::Object(update)
}

/// 显式条件 upsert（对应 JS `upsert`）。
///
/// `_id` 需要生成时使用 `new_id`（Host 供给，core 无随机源）。
pub fn plan_upsert(
    schema_name: &str,
    registry: &Registry,
    ctx: Option<&Context>,
    condition: &Value,
    data: &Value,
    options: &Value,
    now: i64,
    new_id: &str,
) -> Result<Value, String> {
    let schema = registry.get(schema_name)?;
    if !can_write_schema(schema, ctx) {
        return Err(ERR_NO_WRITE.to_string());
    }

    let filtered_data = match ctx {
        Some(_) => filter_writable_data(schema, ctx, data),
        None => data.clone(),
    };

    // JS：options.returnNew !== undefined ? options.returnNew : true
    let return_new = options
        .as_object()
        .and_then(|o| o.get("returnNew"))
        .map(is_truthy)
        .unwrap_or(true);

    let mut set_data = object_of(&remove_undefined(&filtered_data));
    let mut set_on_insert = Map::new();

    // _id：从 data 移到 $setOnInsert（不 $set，避免修改已有文档的 _id）
    if let Some(id) = set_data.get("_id").cloned() {
        if has_trim_str(&id) {
            set_on_insert.insert("_id".to_string(), id);
        }
    } else if !schema.id_prefix.is_empty()
        && !condition.get("_id").map(is_truthy).unwrap_or(false)
    {
        set_on_insert.insert("_id".to_string(), json!(new_id));
    }
    set_data.remove("_id");

    if schema.timestamps {
        set_data.insert("updatedAt".to_string(), json!(now));
        let created = filtered_data
            .get("createdAt")
            .cloned()
            .unwrap_or_else(|| json!(now));
        set_on_insert.insert("createdAt".to_string(), created);
    }
    set_data.remove("createdAt");

    // 自动设置 createdBy（JS：setOnInsert._id || condition._id；undefined 键被 JSON 丢弃）
    if has_creator_permission(schema)
        && !set_on_insert
            .get("createdBy")
            .map(is_truthy)
            .unwrap_or(false)
    {
        let idv = match set_on_insert.get("_id") {
            Some(v) if is_truthy(v) => Some(v.clone()),
            _ => condition.get("_id").cloned(),
        };
        if let Some(v) = idv {
            set_on_insert.insert("createdBy".to_string(), v);
        }
    }

    let update_doc = upsert_update_doc(set_data, set_on_insert);
    let fu_options = json!({
        "upsert": true,
        "returnDocument": if return_new { "after" } else { "before" },
    });
    let command = cmd_find_one_and_update(&schema.collection, condition, &update_doc, &fu_options);
    Ok(json!({ "command": command }))
}
