//! Upsert 路径：显式条件 upsert + mutation 复用的 upsert 更新文档构建。

use serde_json::{json, Map, Value};

use crate::command::cmd::cmd_find_one_and_update;
use crate::command::write::has_creator_permission;
use crate::command::{ensure_context, ERR_NO_WRITE};
use crate::permission::{can_write_schema, filter_writable_data, Context};
use crate::schema::{Registry, Schema};
use crate::types::{is_truthy, validate_condition, validate_condition_shape};

use super::{has_trim_str, object_of, remove_undefined};

/// 构建 upsert 条件组（对应 JS `_buildUpsertConditions`，`$or` 数组）：
/// 1. `data._id` 非空 → `{_id}`；2. unique 索引 keys 在 data 中均非空 → 整组加入
pub(in crate::command) fn build_upsert_conditions(schema: &Schema, data: &Value) -> Vec<Value> {
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
pub(in crate::command) fn build_upsert_update(
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
pub(in crate::command) fn upsert_one_update(
    schema: &Schema,
    data: &Value,
    new_id: &str,
    now: i64,
) -> Value {
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
        let created = data.get("createdAt").cloned().unwrap_or_else(|| json!(now));
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
///
/// 签名与 JS 参考实现逐一对应（跨语言 parity 优先于参数个数），保持位置参数。
#[allow(clippy::too_many_arguments)]
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
    ensure_context(registry, ctx)?;
    let schema = registry.get(schema_name)?;
    // 条件拒绝名单（缺陷 D-02）：upsert 条件命中拒绝名单即显式报错
    validate_condition(condition)?;
    // §11.4（D2）：写路径条件与读路径同码拒绝 U1~U4 形态（数组/对象/点号路径）
    validate_condition_shape(schema, condition)?;
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
    } else if !schema.id_prefix.is_empty() && !condition.get("_id").map(is_truthy).unwrap_or(false)
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
    let command = cmd_find_one_and_update(schema, condition, &update_doc, &fu_options);
    Ok(json!({ "command": command }))
}
