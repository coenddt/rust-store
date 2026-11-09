//! Upsert 路径：显式条件 upsert + mutation 复用的 upsert 更新文档构建。

use serde_json::{json, Map, Value};

use crate::command::cmd::cmd_find_one_and_update;
use crate::command::write::{effective_write_condition, has_creator_permission};
use crate::command::write_links::attach_write_links;
use crate::command::{ensure_context, ERR_NO_WRITE};
use crate::computes::{apply_field_defaults, DefaultFill};
use crate::permission::{can_write_schema, Context};
use crate::rbac::{ensure_write, filter_writable_data_overlay, WriteAction};
use crate::schema::{Registry, Schema};
use crate::types::{is_truthy, validate_condition, validate_condition_shape};

use super::{has_trim_str, object_of};

/// 收集 filter 中**构成单值基线**的顶层等值键：`{k: v}` 与 `{k: {"$eq": v}}`。
///
/// `$and` 是合取，内部等值键同样是必然命中的基线，向下收集；
/// `$or` / `$in` / `$ne` / 范围算子不构成单值基线，一律不收集。
fn equality_filter_keys(condition: &Value, out: &mut std::collections::HashSet<String>) {
    let Some(o) = condition.as_object() else {
        return;
    };
    for (k, v) in o {
        if k == "$and" {
            if let Some(arr) = v.as_array() {
                for sub in arr {
                    equality_filter_keys(sub, out);
                }
            }
            continue;
        }
        if k.starts_with('$') {
            continue;
        }
        let is_eq = match v {
            Value::Object(m) => m.contains_key("$eq"),
            _ => true,
        };
        if is_eq {
            out.insert(k.clone());
        }
    }
}

/// F1：把 schema 显式声明的字段 default 合并进 `$setOnInsert`（只在插入分支生效）。
///
/// `$set` 命中的键不补 default（更新已有文档时不得用 default 覆盖现值）；
/// `$setOnInsert` 已有键（`_id` / 时间戳 / 归属键）保持最高优先级；
/// filter 的等值键也不补 default —— 新文档的该字段由 filter 条件建立基线，
/// 补 default 会覆盖基线（userId / 唯一键被写成空串，幂等与 partial unique 一并失效）。
fn merge_insert_defaults(
    schema: &Schema,
    condition: &Value,
    set_data: &mut Map<String, Value>,
    set_on_insert: &mut Map<String, Value>,
) {
    // probe 必须携带**真实值**（此前一律填 Null，导致显式传入的嵌套 object 被当成
    // 非对象、递归默认值失效）：`$set` 里用户显式提供的 object 也应按 insert 语义
    // 递归补声明默认，否则同一入参在 insert / upsert 两个入口落库形状不同。
    // `$setOnInsert` 的键只需占位（不得据此递归）。
    let mut filter_keys = std::collections::HashSet::new();
    equality_filter_keys(condition, &mut filter_keys);
    let mut probe = set_data.clone();
    for k in set_on_insert.keys() {
        probe.entry(k.clone()).or_insert(Value::Null);
    }
    let filled = apply_field_defaults(&Value::Object(probe), schema, DefaultFill::Insert);
    let Some(o) = filled.as_object() else {
        return;
    };
    for (k, v) in o {
        if let Some(slot) = set_data.get_mut(k) {
            // 只回写「对象被补了子字段」的情形；标量用户值原样保留
            if slot.is_object() && *slot != *v {
                *slot = v.clone();
            }
        } else if k == "_id" || filter_keys.contains(k) {
            // `_id` 是主键：写入 `$setOnInsert` 后，upsert 命中已有文档的更新分支
            // 会被 MongoDB 判为 ImmutableField（update on the path '_id'）。
            // 无条件排除，不依赖 schema 是否声明 default。
            continue;
        } else if !set_on_insert.contains_key(k) {
            set_on_insert.insert(k.clone(), v.clone());
        }
    }
}

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
/// mutation 根文档 upsert 路径使用；含归属键自动填充）
pub(in crate::command) fn build_upsert_update(
    schema: &Schema,
    field_data: &Value,
    new_id: &str,
    now: i64,
    ctx: Option<&Context>,
    condition: &Value,
) -> Value {
    let mut set_data = object_of(field_data);
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

    // 自动设置归属键（upsert 新文档时）：
    // P4：优先取 ctx.userId（与 insert 一致），无 ctx 时才回落 `_id`
    // （mutation 的内部调用无用户身份，保留历史语义）。
    let owner = schema.owner_key();
    if has_creator_permission(schema) && !set_on_insert.get(owner).map(is_truthy).unwrap_or(false) {
        let owner_value = ctx
            .and_then(|c| c.user_id.clone())
            .filter(|u| !u.is_empty())
            .map(|u| json!(u))
            .or_else(|| set_on_insert.get("_id").cloned());
        if let Some(v) = owner_value {
            set_on_insert.insert(owner.to_string(), v);
        }
    }

    // F1：字段 default 只进 $setOnInsert（不覆盖更新分支的现值）
    merge_insert_defaults(schema, condition, &mut set_data, &mut set_on_insert);

    upsert_update_doc(set_data, set_on_insert)
}

/// type:'one' 子文档的 updateDoc（对应 JS `_upsertOne`，无 createdBy 自动填充）
pub(in crate::command) fn upsert_one_update(
    schema: &Schema,
    data: &Value,
    new_id: &str,
    now: i64,
    condition: &Value,
) -> Value {
    let mut set_data = object_of(data);
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

    // F1：字段 default 只进 $setOnInsert（子文档 upsert 同样适用）
    merge_insert_defaults(schema, condition, &mut set_data, &mut set_on_insert);

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
    validate_condition_shape(schema, condition, registry.profile())?;
    if !can_write_schema(registry.role_rules(), schema, ctx) {
        return Err(ERR_NO_WRITE.to_string());
    }
    // RBAC 表级写判定（upsert 按 Insert 动作判，deny-wins）
    ensure_write(registry, schema, ctx, WriteAction::Insert)?;
    // P1：creator 独占写必须带可用 userId，否则无法把已有行收窄到本人（禁越权 upsert）
    if crate::permission::write_restricted_to_creator(registry.role_rules(), schema, ctx)
        && !ctx
            .and_then(|c| c.user_id.as_deref())
            .map(|u| !u.is_empty())
            .unwrap_or(false)
    {
        return Err(ERR_NO_WRITE.to_string());
    }

    let filtered_data = match ctx {
        // RBAC 感知版：静态 writable ∩ RBAC writeFields（无策略时直通静态过滤）
        Some(_) => filter_writable_data_overlay(registry, schema, ctx, data),
        None => data.clone(),
    };
    // accessKeys：payload 目标值校验（过滤后；不被写入的字段无需校验）
    if let Some(obj) = filtered_data.as_object() {
        crate::access::validate_write_values(schema, ctx, obj)?;
    }
    // D3：object 字段收到非对象值 → 显式报错（与 insert / 普通 update 同码；
    // internal 上下文不豁免）。upsert 同样会落库，不能绕过类型强校验。
    crate::computes::validate_object_fields(&filtered_data, schema)?;

    // 执行条件（合入归属与 RBAC/accessKeys 行条件）先行计算：
    // `$setOnInsert` 合并 default 时必须知道 filter 的等值基线键，避免把基线覆盖成 default。
    let eff_condition =
        effective_write_condition(registry, schema, ctx, condition, WriteAction::Update)?;

    // JS：options.returnNew !== undefined ? options.returnNew : true
    let return_new = options
        .as_object()
        .and_then(|o| o.get("returnNew"))
        .map(is_truthy)
        .unwrap_or(true);

    let mut set_data = object_of(&filtered_data);
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

    // 自动设置归属键：P4 —— upsert 新文档的归属键必须是 ctx.userId（此前误用 `_id`，
    // 导致 creator 写权限下新建文档的归属人是文档主键而非用户）。
    let owner = schema.owner_key();
    if has_creator_permission(schema) && !set_on_insert.get(owner).map(is_truthy).unwrap_or(false) {
        let owner_value = ctx
            .and_then(|c| c.user_id.clone())
            .filter(|u| !u.is_empty())
            .map(|u| json!(u))
            .or_else(|| {
                // 无用户身份（内部调用）才回落 _id；undefined 键被 JSON 丢弃
                match set_on_insert.get("_id") {
                    Some(v) if is_truthy(v) => Some(v.clone()),
                    _ => condition.get("_id").cloned(),
                }
            });
        if let Some(v) = owner_value {
            set_on_insert.insert(owner.to_string(), v);
        }
    }

    // F1：字段 default 只进 $setOnInsert（更新分支不覆盖现值）；显式 object 递归补；
    // filter 等值键不补 default（新文档该字段由 filter 基线决定）
    merge_insert_defaults(schema, &eff_condition, &mut set_data, &mut set_on_insert);

    let update_doc = upsert_update_doc(set_data, set_on_insert);
    let fu_options = json!({
        "upsert": true,
        "returnDocument": if return_new { "after" } else { "before" },
    });
    // P1/P2：执行条件合入归属（creator 独占写）与 RBAC/accessKeys 行条件。
    // 行级语义按 update 判定（命中已有行即为更新；insert 无行级语义）。
    let command = cmd_find_one_and_update(schema, &eff_condition, &update_doc, &fu_options);
    attach_write_links(
        json!({ "command": command }),
        registry,
        &[schema_name],
        registry.write_link_policy(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 缺陷 1 fixture：等值键声明非空默认值 `""`，filter 值与默认值可区分。
    fn child_schema() -> Schema {
        let mut reg = Registry::new();
        reg.register(&json!({
            "name": "Child",
            "collection": "children",
            "idPrefix": "c",
            "timestamps": false,
            "fields": {
                "parentId": { "type": "string", "default": "" },
                "tenantId": { "type": "string", "default": "" },
                "title": { "type": "string" }
            },
            "relations": {}
        }))
        .expect("Child schema 注册应成功");
        reg.get("Child").expect("Child 应已注册").clone()
    }

    /// 缺陷 1 调用点 `build_upsert_update`：filter 等值键不得补 default。
    #[test]
    fn build_upsert_update_skips_filter_equality_key() {
        let schema = child_schema();
        let update = build_upsert_update(
            &schema,
            &json!({ "title": "x" }),
            "c1",
            0,
            None,
            &json!({ "tenantId": "t1" }),
        );
        assert!(
            update["$setOnInsert"].get("tenantId").is_none(),
            "缺陷 1：build_upsert_update 不得覆盖 filter 等值基线: {update}"
        );
    }

    /// 缺陷 1 调用点 `upsert_one_update`：foreignKey filter 等值键不得补 default。
    #[test]
    fn upsert_one_update_skips_filter_equality_key() {
        let schema = child_schema();
        let filter = json!({ "parentId": "p1" });
        let update = upsert_one_update(&schema, &json!({ "title": "x" }), "c1", 0, &filter);
        assert!(
            update["$setOnInsert"].get("parentId").is_none(),
            "缺陷 1：upsert_one_update 不得覆盖 foreignKey 基线: {update}"
        );
    }

    /// 缺陷 1 回归保护：filter 不含该键时 default 仍写入 `$setOnInsert`。
    #[test]
    fn build_upsert_update_keeps_default_when_filter_key_absent() {
        let schema = child_schema();
        let update =
            build_upsert_update(&schema, &json!({ "title": "x" }), "c1", 0, None, &json!({}));
        assert_eq!(
            update["$setOnInsert"]["tenantId"], "",
            "缺陷 1：filter 不含 tenantId 时 default 必须保留: {update}"
        );
    }
}
