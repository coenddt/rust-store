//! 平铺 JOIN 行 → 嵌套 Mongo 文档还原

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::ir::{RowCol, RowShape};

/// 把一组平铺行（`Vec<Map<String, Value>>`）按 row_shape 还原为嵌套 Mongo 文档数组。
///
/// 规则：
/// - 标量列（`is_array=false`）写入其 `json_path` 指定的嵌套位置；
/// - 关系列（`is_array=true`）按 `json_path` 逐级归并：每一级按该关系的主键分组去重，
///   基数由 `ones[level]` 决定（`true`→对象或 `null`，`false`→数组）。
pub fn restore_rows(shape: &RowShape, rows: &[Value]) -> Value {
    // 先按「根标量键」分组，同根聚为一篇文档；无根键列（如 `$group` 分组结果，
    // 其输出没有 `_id` 列）→ 每行独立成篇（分组结果每行本就是一个文档）。
    // 用 HashMap 索引根键 → 桶下标（`buckets` 保持插入顺序），避免线性 `find` 的 O(行数 × 根数)。
    let mut index: HashMap<Value, usize> = HashMap::new();
    let mut buckets: Vec<Vec<&Value>> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let key = root_key(shape, row).unwrap_or_else(|| Value::from(i as u64));
        match index.get(&key) {
            Some(&idx) => buckets[idx].push(row),
            None => {
                index.insert(key, buckets.len());
                buckets.push(vec![row]);
            }
        }
    }

    let cols: Vec<&RowCol> = shape.columns.iter().collect();
    let docs: Vec<Value> = buckets
        .into_iter()
        .map(|group| {
            // 每行「显式存在字段集合」（`__present` 哨兵，缺失 vs null 三态）；
            // 仅作用于根层标量（哨兵列来自根表）
            let present = shape
                .present_alias
                .as_deref()
                .and_then(|a| row_val(group[0], a).as_str())
                .map(|s| s.to_owned());
            Value::Object(build_object(&cols, &group, 0, present.as_deref()))
        })
        .collect();
    Value::Array(docs)
}

/// 由若干平铺行构造**一层**对象：
/// - `path_len` 是本层的 `json_path` 前缀长度（根 = 0，`lessons` 内 = 1，…）；
/// - 标量列（`json_path.len() == path_len + 1`，或**非关系列**的多段点号路径，
///   如 `$group` 的 by 键 `meta.level`）取首行值写入（点号路径还原为嵌套对象）；
/// - 更深的**关系列**（`is_array`，`json_path` 更深）= 本级关系，按关系名分组递归（`relation_value`）。
fn build_object(
    cols: &[&RowCol],
    rows: &[&Value],
    path_len: usize,
    present: Option<&str>,
) -> Map<String, Value> {
    let mut obj = Map::new();
    for c in cols {
        if c.json_path.len() == path_len + 1 || (!c.is_array && c.json_path.len() > path_len + 1) {
            let val = row_val(rows[0], &c.alias).clone();
            merge_path(
                &mut obj,
                &c.json_path[path_len..],
                val,
                present,
                c.always,
                c.is_bool,
            );
        }
    }
    // 本级关系名（仅关系列 `is_array`，保留出现顺序）
    let mut rels: Vec<String> = Vec::new();
    for c in cols {
        if c.is_array && c.json_path.len() > path_len + 1 {
            let r = &c.json_path[path_len];
            if !rels.iter().any(|x| x == r) {
                rels.push(r.clone());
            }
        }
    }
    for r in rels {
        let sub: Vec<&RowCol> = cols
            .iter()
            .copied()
            .filter(|c| {
                c.is_array && c.json_path.len() > path_len + 1 && c.json_path[path_len] == r
            })
            .collect();
        obj.insert(r.clone(), relation_value(&sub, rows, path_len));
    }
    obj
}

/// 还原 `path_len` 这一级的关系值：按该关系主键分组去重后递归构造子文档。
/// 基数 `ones[path_len]`：`true` → 至多一条（对象或 `null`），`false` → 数组。
fn relation_value(cols: &[&RowCol], rows: &[&Value], path_len: usize) -> Value {
    let one = cols
        .first()
        .and_then(|c| c.ones.get(path_len).copied())
        .unwrap_or_else(|| cols.first().map(|c| c.one).unwrap_or(false));
    let id_col = cols
        .iter()
        .find(|c| c.json_path.len() == path_len + 2 && c.json_path[path_len + 1] == "_id");
    // 每个 SQL 行 = 一个 (父, 子) 对；多关系 LEFT JOIN 会形成笛卡尔积，同一子行重复出现。
    // 按子文档主键分组（Mongo `$lookup` 每个子文档只出现一次），无主键列时退化为逐行独立。
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&Value>> = HashMap::new();
    for row in rows {
        // 该关系在本行完全缺失（LEFT JOIN 全 NULL）→ 不是子文档
        if cols.iter().all(|c| row_val(row, &c.alias).is_null()) {
            continue;
        }
        let key = match id_col.map(|c| row_val(row, &c.alias)) {
            Some(v) if !v.is_null() => v.to_string(),
            _ => format!("\u{0}{}", order.len()),
        };
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(row);
    }
    let docs: Vec<Value> = order
        .into_iter()
        .map(|k| Value::Object(build_object(cols, &groups[&k], path_len + 1, None)))
        .collect();
    if one {
        docs.into_iter().next().unwrap_or(Value::Null)
    } else {
        Value::Array(docs)
    }
}

/// 从行取值（按 alias 直接取列）
fn row_val<'a>(row: &'a Value, alias: &str) -> &'a Value {
    match row {
        Value::Object(map) => map.get(alias).unwrap_or(&Value::Null),
        _ => &Value::Null,
    }
}

/// 根分组键：根表 `_id` 标量列（`json_path == ["_id"]`）。无该列（如 `$group`
/// 分组结果）→ `None`，调用方按行独立成篇。
fn root_key(shape: &RowShape, row: &Value) -> Option<Value> {
    let id_alias = shape
        .columns
        .iter()
        .find(|c| !c.is_array && c.json_path.len() == 1 && c.json_path[0] == "_id")?
        .alias
        .clone();
    row.get(&id_alias).cloned()
}

/// 把 value 写到 obj 的嵌套路径（创建中间 object）
///
/// 缺失 vs null 三态（F-07/H-01）：标量值为 null 时，仅当字段在该行「显式存在集合」
/// （`present`）中才写入 `key: null`，否则（字段缺失）不产出该键 —— 对齐 Mongo
/// 「显式 null 有键、缺失无键」。`present == None` 表示本语句未查哨兵列（count/关系聚合），
/// 退化为旧语义：null 一律不产出键。
///
/// `always`（§9.2(2) 归一聚合计算列）：为 `true` 时即使 `null` 也强制写入，
/// 与 Mongo `$addFields` 对空集聚合产出显式 `null`（§9.7）逐行对齐。
///
/// `is_bool`（§9.7 布尔归一）：为 `true` 时先把 SQL 的 `0/1` 归一为 JSON `bool`，
/// 使「同一逻辑布尔字段」在 MySQL/SQLite（`TINYINT(1)` / `INTEGER`）与
/// PostgreSQL（原生 `BOOLEAN`）、MongoDB（`true/false`）间**类型一致**。
fn merge_path(
    obj: &mut Map<String, Value>,
    path: &[String],
    val: Value,
    present: Option<&str>,
    always: bool,
    is_bool: bool,
) {
    let val = if is_bool { to_json_bool(val) } else { val };
    if path.len() <= 1 {
        if let Some(k) = path.first() {
            if always || !val.is_null() || present_contains(present, k) {
                obj.insert(k.clone(), val);
            }
        }
        return;
    }
    let head = path[0].clone();
    let entry = obj
        .entry(head.clone())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(m) = entry {
        merge_path(m, &path[1..], val, present, always, false);
    }
}

/// §9.7 布尔归一：`1/0`（数值或数值字符串）→ `true/false`；其余（含 `null`、已是 bool、
/// PG 原生 bool）原样透传 —— 绝不把非 0/1 的值静默改判为布尔。
fn to_json_bool(v: Value) -> Value {
    let as_num = match &v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    };
    match as_num {
        Some(1.0) => Value::Bool(true),
        Some(0.0) => Value::Bool(false),
        _ => v,
    }
}

/// `present` 集合形如 `,field1,field2,`，判定 `key` 是否显式存在（用 `,key,` 匹配）
fn present_contains(present: Option<&str>, key: &str) -> bool {
    match present {
        Some(s) => s.contains(&format!(",{},", key)),
        None => false,
    }
}

/// 便捷序列化：把 `rowShape + rows` 两步合成一次调用（绑定层常用）
pub fn restore_rows_json(shape: &Value, rows: &Value) -> Result<Value, String> {
    let shape = RowShape::from_value(shape)?;
    let rows = match rows {
        Value::Array(a) => a.clone(),
        _ => Vec::new(),
    };
    Ok(restore_rows(&shape, &rows))
}

impl RowShape {
    /// 从绑定层传入的 JSON 解析（与 `to_value` 对称）
    pub fn from_value(v: &Value) -> Result<RowShape, String> {
        let obj = v
            .get("columns")
            .and_then(|c| c.as_array())
            .ok_or("rowShape 缺少 columns")?;
        let mut columns = Vec::new();
        for c in obj {
            let alias = c
                .get("alias")
                .and_then(|a| a.as_str())
                .ok_or("列缺 alias")?
                .to_string();
            let path: Vec<String> = c
                .get("path")
                .and_then(|p| p.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let is_array = c.get("isArray").and_then(|b| b.as_bool()).unwrap_or(false);
            let one = c.get("one").and_then(|b| b.as_bool()).unwrap_or(false);
            // 每级关系基数：新契约显式携带 `ones`；旧 shape 缺省时按顶层 `one` 退化
            let ones: Vec<bool> = match c.get("ones").and_then(|o| o.as_array()) {
                Some(a) => a.iter().map(|b| b.as_bool().unwrap_or(false)).collect(),
                None if is_array => vec![one],
                None => Vec::new(),
            };
            let sub_shape = c
                .get("subShape")
                .and_then(|s| {
                    if s.is_null() {
                        None
                    } else {
                        Some(RowShape::from_value(s))
                    }
                })
                .transpose()?;
            let always = c.get("always").and_then(|b| b.as_bool()).unwrap_or(false);
            let is_bool = c.get("bool").and_then(|b| b.as_bool()).unwrap_or(false);
            columns.push(RowCol {
                alias,
                json_path: path,
                is_array,
                one,
                ones,
                sub_shape,
                always,
                is_bool,
            });
        }
        let present_alias = v
            .get("present")
            .and_then(|p| p.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        Ok(RowShape {
            columns,
            present_alias,
        })
    }
}
