//! 本地磁盘数据源 —— 写算子求值（`insertOne` / `insertMany` / `updateMany` /
//! `findOneAndUpdate` / `deleteMany`）。
//!
//! 语义基准 = **MongoDB 驱动语义**（local 与 mongo 走同一条命令路径，必须同结果）。
//! 纯逻辑：无 IO / 无时钟 / 无随机 —— 一切不确定性输入（`_id`、时间戳）均由 core /
//! 宿主在命令中给足（core 不生成 `_id`）。
//!
//! 更新算子仅支持 `$set` / `$inc` / `$unset`（+ upsert 分支的 `$setOnInsert`，
//! 对齐 `dialect/write/update.rs` 第 35–47 行）；其余操作符（`$push` / `$addToSet` /
//! `$pull` …）一律 `Err`（禁静默丢弃）。

use serde_json::{json, Map, Number, Value};

use crate::local::filter;
use crate::local::value::{self, get_path, remove_path, set_path};

// ---------------------------------------------------------------------------
// insertOne / insertMany
// ---------------------------------------------------------------------------

/// `insertOne`：追加 `cmd.doc`；同 `_id` 冲突 → `Err`（对齐 Mongo 唯一键冲突）。
///
/// 返回写入后的文档（与驱动 `insertOne` 返回文档形态一致）。
pub fn insert_one(rows: &mut Vec<Value>, doc: &Value) -> Result<Value, String> {
    let doc = normalize_doc(doc)?;
    if let Some(id) = doc.get("_id") {
        if rows.iter().any(|r| id_equal(r, id)) {
            return Err(format!("insertOne: _id 冲突（{id}）"));
        }
    }
    rows.push(doc.clone());
    Ok(doc)
}

/// `insertMany`：`upsertById == true` → 按 `_id` **replace**（归档幂等，对齐
/// `mongo.js` 第 136–143 行）；否则同 `_id` 冲突（含批内重复）→ `Err`。
///
/// 返回 `{"insertedCount": n}`。
pub fn insert_many(
    rows: &mut Vec<Value>,
    docs: &[Value],
    upsert_by_id: bool,
) -> Result<Value, String> {
    let normalized = docs
        .iter()
        .map(normalize_doc)
        .collect::<Result<Vec<_>, _>>()?;

    if upsert_by_id {
        for doc in normalized {
            match doc.get("_id") {
                Some(id) => match rows.iter_mut().find(|r| id_equal(r, id)) {
                    Some(slot) => *slot = doc,
                    None => rows.push(doc),
                },
                None => rows.push(doc),
            }
        }
        return Ok(json!({ "insertedCount": docs.len() }));
    }

    for (i, doc) in normalized.iter().enumerate() {
        if let Some(id) = doc.get("_id") {
            if rows.iter().any(|r| id_equal(r, id)) {
                return Err(format!("insertMany: _id 冲突（{id}）"));
            }
            if normalized[..i].iter().any(|prev| id_equal(prev, id)) {
                return Err(format!("insertMany: 批内 _id 重复（{id}）"));
            }
        }
    }
    rows.extend(normalized);
    Ok(json!({ "insertedCount": docs.len() }))
}

// ---------------------------------------------------------------------------
// updateMany / findOneAndUpdate / deleteMany
// ---------------------------------------------------------------------------

/// `updateMany`：过滤命中项全部应用 `cmd.update`；返回 `{"modifiedCount": n}`。
///
/// `modifiedCount` 为**实际发生变更**的文档数（对齐驱动语义：`$set` 同值不计）。
pub fn update_many(rows: &mut [Value], condition: &Value, update: &Value) -> Result<Value, String> {
    let mut modified = 0_usize;
    for row in rows.iter_mut() {
        if filter::matches(row, condition)? {
            let before = row.clone();
            apply_update(row, update, false)?;
            if !value::values_equal(&before, row) {
                modified += 1;
            }
        }
    }
    Ok(json!({ "modifiedCount": modified }))
}

/// `findOneAndUpdate`：首条命中应用更新；
/// - 无命中且 `options.upsert == true` → 以过滤条件等值字段为底、应用 `$set`/`$setOnInsert` 后插入；
/// - 无命中且非 upsert → `null`；
/// - `returnDocument == "before"` → 更新前文档（upsert 首次插入时为 `null`，对齐驱动），否则返回更新后文档。
pub fn find_one_and_update(
    rows: &mut Vec<Value>,
    condition: &Value,
    update: &Value,
    options: &Value,
) -> Result<Value, String> {
    let upsert = options
        .get("upsert")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let return_before = options
        .get("returnDocument")
        .and_then(Value::as_str)
        .map(|s| s == "before")
        .unwrap_or(false);

    if let Some(idx) = find_first(rows, condition)? {
        let before = rows[idx].clone();
        let mut doc = before.clone();
        apply_update(&mut doc, update, false)?;
        rows[idx] = doc.clone();
        return Ok(if return_before { before } else { doc });
    }
    if !upsert {
        return Ok(Value::Null);
    }
    let mut doc = build_upsert_base(condition);
    apply_update(&mut doc, update, true)?;
    rows.push(doc.clone());
    Ok(if return_before { Value::Null } else { doc })
}

/// `deleteMany`：删除命中项；返回 `{"deletedCount": n}`。
pub fn delete_many(rows: &mut Vec<Value>, condition: &Value) -> Result<Value, String> {
    let mut kept = Vec::with_capacity(rows.len());
    let mut deleted = 0_usize;
    for row in rows.drain(..) {
        if filter::matches(&row, condition)? {
            deleted += 1;
        } else {
            kept.push(row);
        }
    }
    *rows = kept;
    Ok(json!({ "deletedCount": deleted }))
}

// ---------------------------------------------------------------------------
// 更新算子
// ---------------------------------------------------------------------------

/// 应用更新文档：逐算子求值（`$set` / `$inc` / `$unset`；`$setOnInsert` 仅插入分支生效）。
fn apply_update(doc: &mut Value, update: &Value, is_insert: bool) -> Result<(), String> {
    let obj = update
        .as_object()
        .ok_or_else(|| format!("update 必须是对象，收到 {}", type_name(update)))?;
    if obj.is_empty() {
        return Err("没有提供要更新的字段".to_string());
    }
    if !doc.is_object() {
        *doc = Value::Object(Map::new());
    }
    for (op, arg) in obj {
        match op.as_str() {
            "$set" => apply_set(doc, arg)?,
            "$inc" => apply_inc(doc, arg)?,
            "$unset" => apply_unset(doc, arg)?,
            // upsert 专用：仅在插入分支生效，非插入路径忽略（对齐 dialect/write/update.rs 第 47 行）
            "$setOnInsert" => {
                if is_insert {
                    apply_set(doc, arg)?;
                }
            }
            other => {
                return Err(format!(
                    "不支持的更新操作符: {other}（本地求值器仅支持 $set/$inc/$unset/$setOnInsert，拒绝静默）"
                ));
            }
        }
    }
    Ok(())
}

/// `$set`：点号路径逐字段写入（对齐 Mongo；含显式 `null` 亦写入）。
fn apply_set(doc: &mut Value, arg: &Value) -> Result<(), String> {
    let fields = arg
        .as_object()
        .ok_or_else(|| format!("$set 需要对象，收到 {}", type_name(arg)))?;
    for (path, v) in fields {
        set_path(doc, path, v.clone());
    }
    Ok(())
}

/// `$inc`：数值自增；字段缺失视作 `0`（结果保留增量类型）；目标非数值 → `Err`（对齐 Mongo）。
fn apply_inc(doc: &mut Value, arg: &Value) -> Result<(), String> {
    let fields = arg
        .as_object()
        .ok_or_else(|| format!("$inc 需要对象，收到 {}", type_name(arg)))?;
    for (path, inc) in fields {
        if inc.as_f64().is_none() {
            return Err(format!(
                "$inc 的增量必须是数字，字段 \"{path}\" 收到 {}",
                type_name(inc)
            ));
        }
        let new = match get_path(doc, path) {
            // 缺失 → 视为 0 + 增量 = 增量本身（保留整型/浮点形态）
            None => inc.clone(),
            Some(cur) => {
                let base = cur.as_f64().ok_or_else(|| {
                    format!(
                        "$inc 目标字段 \"{path}\" 非数值（{}）：Mongo 语义要求对数值字段自增",
                        type_name(cur)
                    )
                })?;
                let inc_f = inc.as_f64().unwrap_or(0.0);
                match (cur.as_i64(), inc.as_i64()) {
                    (Some(a), Some(b)) => match a.checked_add(b) {
                        Some(s) => Value::from(s),
                        None => float_value(a as f64 + b as f64),
                    },
                    _ => float_value(base + inc_f),
                }
            }
        };
        set_path(doc, path, new);
    }
    Ok(())
}

/// `$unset`：点号路径逐字段删除（取值无关，对齐 Mongo）。
fn apply_unset(doc: &mut Value, arg: &Value) -> Result<(), String> {
    let fields = arg
        .as_object()
        .ok_or_else(|| format!("$unset 需要对象，收到 {}", type_name(arg)))?;
    for path in fields.keys() {
        remove_path(doc, path);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 文档规范化：对象原样；`null` → 空对象（对齐 `mongo.js` 的 `cmd.doc || {}`）；其余 → `Err`。
fn normalize_doc(v: &Value) -> Result<Value, String> {
    match v {
        Value::Object(_) => Ok(v.clone()),
        Value::Null => Ok(Value::Object(Map::new())),
        other => Err(format!(
            "文档必须是对象，收到 {}（拒绝静默按空文档写入）",
            type_name(other)
        )),
    }
}

/// `_id` 等值判定（候选文档的 `_id` 与目标 `_id`）。
fn id_equal(row: &Value, id: &Value) -> bool {
    row.get("_id")
        .map(|v| value::values_equal(v, id))
        .unwrap_or(false)
}

/// 首条命中下标。
fn find_first(rows: &[Value], condition: &Value) -> Result<Option<usize>, String> {
    for (i, row) in rows.iter().enumerate() {
        if filter::matches(row, condition)? {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

/// upsert 新文档基底：取过滤条件中的**顶层等值字段**（`{k: v}` 或 `{k: {$eq: v}}`）。
fn build_upsert_base(condition: &Value) -> Value {
    let mut m = Map::new();
    if let Some(o) = condition.as_object() {
        for (k, v) in o {
            if k.starts_with('$') {
                continue;
            }
            match v {
                Value::Object(inner) => {
                    if let Some(eq) = inner.get("$eq") {
                        m.insert(k.clone(), eq.clone());
                    }
                }
                other => {
                    m.insert(k.clone(), other.clone());
                }
            }
        }
    }
    Value::Object(m)
}

fn float_value(f: f64) -> Value {
    Number::from_f64(f)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows(v: Value) -> Vec<Value> {
        v.as_array().cloned().expect("文档数组")
    }

    #[test]
    fn insert_one_appends_and_conflicts() {
        let mut r = rows(json!([]));
        let out = insert_one(&mut r, &json!({ "_id": "a", "n": 1 })).unwrap();
        assert_eq!(out, json!({ "_id": "a", "n": 1 }));
        assert_eq!(r.len(), 1);
        // 同 _id 冲突 → Err
        assert!(insert_one(&mut r, &json!({ "_id": "a", "n": 2 })).is_err());
        assert_eq!(r.len(), 1);
        // null doc → 空对象（不报错）
        assert_eq!(insert_one(&mut r, &Value::Null).unwrap(), json!({}));
        // 非对象 → Err
        assert!(insert_one(&mut r, &json!(5)).is_err());
    }

    #[test]
    fn insert_many_plain_and_conflicts() {
        let mut r = rows(json!([{ "_id": "a" }]));
        // 与既有冲突
        assert!(insert_many(&mut r, &[json!({ "_id": "a" })], false).is_err());
        // 批内重复
        assert!(insert_many(
            &mut r,
            &[json!({ "_id": "b" }), json!({ "_id": "b" })],
            false
        )
        .is_err());
        // 正常插入
        let out = insert_many(
            &mut r,
            &[json!({ "_id": "b" }), json!({ "_id": "c" })],
            false,
        )
        .unwrap();
        assert_eq!(out, json!({ "insertedCount": 2 }));
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn insert_many_upsert_by_id_is_idempotent() {
        let mut r = rows(json!([{ "_id": "a", "n": 1 }]));
        // 按 _id replace（归档幂等）
        let out = insert_many(
            &mut r,
            &[json!({ "_id": "a", "n": 9 }), json!({ "_id": "z", "n": 5 })],
            true,
        )
        .unwrap();
        assert_eq!(out, json!({ "insertedCount": 2 }));
        assert_eq!(
            r,
            rows(json!([{ "_id": "a", "n": 9 }, { "_id": "z", "n": 5 }]))
        );
        // 重复调用结果稳定
        insert_many(&mut r, &[json!({ "_id": "a", "n": 9 })], true).unwrap();
        assert_eq!(
            r,
            rows(json!([{ "_id": "a", "n": 9 }, { "_id": "z", "n": 5 }]))
        );
    }

    #[test]
    fn update_many_set_inc_unset() {
        let mut r = rows(json!([
            { "_id": 1, "s": "paid", "n": 1 },
            { "_id": 2, "s": "open", "n": 2 }
        ]));
        let out = update_many(
            &mut r,
            &json!({ "s": "paid" }),
            &json!({ "$set": { "tag": "x" }, "$inc": { "n": 10 }, "$unset": { "s": "" } }),
        )
        .unwrap();
        assert_eq!(out, json!({ "modifiedCount": 1 }));
        assert_eq!(r[0], json!({ "_id": 1, "n": 11, "tag": "x" }));
        assert_eq!(r[1], json!({ "_id": 2, "s": "open", "n": 2 }));
        // $set 同值 → 不计入 modifiedCount
        let out2 = update_many(
            &mut r,
            &json!({ "_id": 1 }),
            &json!({ "$set": { "n": 11 } }),
        )
        .unwrap();
        assert_eq!(out2, json!({ "modifiedCount": 0 }));
    }

    #[test]
    fn update_inc_type_and_non_numeric_error() {
        let mut r = rows(json!([{ "_id": 1, "i": 2 }, { "_id": 2, "s": "x" }]));
        // 整型 + 整型 → 整型
        update_many(&mut r, &json!({ "_id": 1 }), &json!({ "$inc": { "i": 3 } })).unwrap();
        assert_eq!(r[0]["i"], json!(5));
        // 缺失字段 → 取增量值（保留类型）
        update_many(
            &mut r,
            &json!({ "_id": 1 }),
            &json!({ "$inc": { "gone": 1 } }),
        )
        .unwrap();
        assert_eq!(r[0]["gone"], json!(1));
        // 非数值目标 → Err
        assert!(update_many(&mut r, &json!({ "_id": 2 }), &json!({ "$inc": { "s": 1 } })).is_err());
    }

    #[test]
    fn update_unknown_operator_errors() {
        let mut r = rows(json!([{ "_id": 1 }]));
        assert!(update_many(&mut r, &json!({}), &json!({ "$push": { "a": 1 } })).is_err());
        assert!(update_many(&mut r, &json!({}), &json!({})).is_err());
    }

    #[test]
    fn find_one_and_update_after_before_and_upsert() {
        let mut r = rows(json!([{ "_id": 1, "n": 1 }, { "_id": 2, "n": 2 }]));
        // 命中 → 返回更新后文档（默认 after）
        let out = find_one_and_update(
            &mut r,
            &json!({ "_id": 1 }),
            &json!({ "$set": { "n": 7 } }),
            &json!({}),
        )
        .unwrap();
        assert_eq!(out, json!({ "_id": 1, "n": 7 }));
        // returnDocument=before → 返回更新前
        let out2 = find_one_and_update(
            &mut r,
            &json!({ "_id": 1 }),
            &json!({ "$set": { "n": 8 } }),
            &json!({ "returnDocument": "before" }),
        )
        .unwrap();
        assert_eq!(out2, json!({ "_id": 1, "n": 7 }));
        assert_eq!(r[0], json!({ "_id": 1, "n": 8 }));
        // 无命中且非 upsert → null
        let miss = find_one_and_update(
            &mut r,
            &json!({ "_id": 99 }),
            &json!({ "$set": { "n": 1 } }),
            &json!({}),
        )
        .unwrap();
        assert_eq!(miss, Value::Null);
        assert_eq!(r.len(), 2);
        // upsert：条件等值字段为底 + $set + $setOnInsert
        let up = find_one_and_update(
            &mut r,
            &json!({ "_id": 3 }),
            &json!({ "$set": { "n": 3 }, "$setOnInsert": { "_id": 3, "createdAt": 100 } }),
            &json!({ "upsert": true }),
        )
        .unwrap();
        assert_eq!(up, json!({ "_id": 3, "n": 3, "createdAt": 100 }));
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn delete_many_counts() {
        let mut r = rows(json!([
            { "_id": 1, "s": "a" },
            { "_id": 2, "s": "b" },
            { "_id": 3, "s": "a" }
        ]));
        let out = delete_many(&mut r, &json!({ "s": "a" })).unwrap();
        assert_eq!(out, json!({ "deletedCount": 2 }));
        assert_eq!(r, rows(json!([{ "_id": 2, "s": "b" }])));
        // 命中 0 → 0
        assert_eq!(
            delete_many(&mut r, &json!({ "s": "nope" })).unwrap(),
            json!({ "deletedCount": 0 })
        );
    }
}
