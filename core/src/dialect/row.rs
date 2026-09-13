//! 平铺 JOIN 行 → 嵌套 Mongo 文档还原

use serde_json::{Map, Value};

use super::ir::{RowCol, RowShape};

/// 把一组平铺行（`Vec<Map<String, Value>>`）按 row_shape 还原为嵌套 Mongo 文档数组。
///
/// 规则：
/// - 标量列（`is_array=false`）写入其 `json_path` 指定的嵌套位置；
/// - `is_array=true` 的列属于关系聚合：按父文档分组，所有行聚合成数组。
#[allow(clippy::type_complexity)]
pub fn restore_rows(shape: &RowShape, rows: &[Value]) -> Value {
    // 先按「根标量键」分组，同根聚为一篇文档
    let mut buckets: Vec<(Value, Vec<&Value>)> = Vec::new();
    for row in rows {
        let root = root_key(shape, row);
        match buckets.iter_mut().find(|(k, _)| k == &root) {
            Some((_, group)) => group.push(row),
            None => buckets.push((root, vec![row])),
        }
    }

    let docs: Vec<Value> = buckets
        .into_iter()
        .map(|(_, group)| {
            let mut obj = Map::new();
            // 先写标量列（同根组内第一条）
            for col in &shape.columns {
                if col.is_array {
                    continue;
                }
                let val = row_val(group[0], &col.alias);
                merge_path(&mut obj, &col.json_path, val.clone());
            }
            // 再按关系（rel_name = 数组列 json_path[0]）聚合子文档数组
            let rel_names = rel_names_of(shape);
            for rel in &rel_names {
                let rel_cols: Vec<&RowCol> = shape
                    .columns
                    .iter()
                    .filter(|c| {
                        c.is_array && c.json_path.first().map(|s| s == rel).unwrap_or(false)
                    })
                    .collect();
                // 每个 SQL 行 = 一个 (父, 子) 对：有任一关系列非空则是一条子文档
                let mut child_docs: Vec<Value> = Vec::new();
                for row in &group {
                    let has = rel_cols.iter().any(|c| !row_val(row, &c.alias).is_null());
                    if !has {
                        continue;
                    }
                    let mut child = Map::new();
                    for c in &rel_cols {
                        let val = row_val(row, &c.alias).clone();
                        if !val.is_null() {
                            let field = c.json_path.last().cloned().unwrap_or_default();
                            if !child.contains_key(&field) {
                                child.insert(field, val);
                            }
                        }
                    }
                    child_docs.push(Value::Object(child));
                }
                obj.insert(rel.clone(), Value::Array(child_docs));
            }
            Value::Object(obj)
        })
        .collect();
    Value::Array(docs)
}

/// 唯一的数组关系名（保留出现顺序）
fn rel_names_of(shape: &RowShape) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in &shape.columns {
        if c.is_array {
            if let Some(head) = c.json_path.first() {
                if !out.iter().any(|r| r == head) {
                    out.push(head.clone());
                }
            }
        }
    }
    out
}

/// 从行取值（按 alias 直接取列）
fn row_val<'a>(row: &'a Value, alias: &str) -> &'a Value {
    match row {
        Value::Object(map) => map.get(alias).unwrap_or(&Value::Null),
        _ => &Value::Null,
    }
}

/// 根分组键：根表的 _id 标量列
fn root_key(shape: &RowShape, row: &Value) -> Value {
    let id_alias = shape
        .columns
        .iter()
        .find(|c| !c.is_array && c.json_path.last().map(|s| s == "_id").unwrap_or(false))
        .map(|c| c.alias.clone())
        .unwrap_or_else(|| "_id".to_string());
    row.get(&id_alias).cloned().unwrap_or(Value::Null)
}

/// 把 value 写到 obj 的嵌套路径（创建中间 object）
fn merge_path(obj: &mut Map<String, Value>, path: &[String], val: Value) {
    if path.len() <= 1 {
        if let Some(k) = path.first() {
            if !val.is_null() {
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
        merge_path(m, &path[1..], val);
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
            columns.push(RowCol {
                alias,
                json_path: path,
                is_array,
                sub_shape,
            });
        }
        Ok(RowShape { columns })
    }
}
