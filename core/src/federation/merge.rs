//! 联邦结果合并：按 join 边做内存哈希 join + 数组/对象还原
//!
//! Host 逐源执行完 [`crate::federation::plan_federated`] 产出的命令后，把
//! **与 `plan.sources` 同序**的结果数组交回本模块，得到嵌套文档数组；
//! 之后照常走 [`crate::command::prepare_query`] / [`crate::command::strip_query`]
//! 做计算列与权限收尾（与单库完全同一套尾处理）。
//!
//! 边界（对齐契约）：
//!   - 只做纯内存计算，不做任何 IO（铁律 1）；
//!   - 每源结果行数超 [`MAX_FEDERATION_ROWS`] 时显式报错，**拒绝静默全表拉取**。

use std::collections::HashMap;

use serde_json::Value;

use crate::bson::id_key;

use super::plan::unit_index;

/// 单源取数结果行数上限（内存 join 前的护栏）
pub const MAX_FEDERATION_ROWS: usize = 100_000;

/// 收集 `docs` 中位于 `path` 层级的所有文档（可变引用）
///
/// `path` 为从根到该层的关系名序列；数组关系展开为多个元素，对象关系取单值。
/// 空 `path` 即根层。
fn collect_at_path<'a>(docs: &'a mut [Value], path: &[String], out: &mut Vec<&'a mut Value>) {
    let Some((head, tail)) = path.split_first() else {
        for d in docs.iter_mut() {
            out.push(d);
        }
        return;
    };
    for d in docs.iter_mut() {
        let Some(obj) = d.as_object_mut() else {
            continue;
        };
        let Some(child) = obj.get_mut(head.as_str()) else {
            continue;
        };
        match child {
            Value::Array(arr) => collect_at_path(arr.as_mut_slice(), tail, out),
            other => collect_at_path(std::slice::from_mut(other), tail, out),
        }
    }
}

fn as_str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// 合并各源结果 → 嵌套文档数组
///
/// `results` 必须与 `plan.sources` **同序同长**（`results[i]` 即 `plan.sources[i]`
/// 那组命令执行后的文档数组）。
pub fn merge_federated(plan: &Value, results: &[Value]) -> Result<Value, String> {
    let sources = plan
        .get("sources")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "联邦计划缺少 sources".to_string())?;

    if results.len() != sources.len() {
        return Err(format!(
            "联邦结果数与取数单元数不一致: results = {}, sources = {}",
            results.len(),
            sources.len()
        ));
    }

    let mut rows_by_unit: Vec<Vec<Value>> = Vec::with_capacity(results.len());
    for (i, r) in results.iter().enumerate() {
        let rows = r
            .as_array()
            .ok_or_else(|| format!("第 {} 个取数单元的结果必须是数组", i))?;
        if rows.len() > MAX_FEDERATION_ROWS {
            return Err(format!(
                "第 {} 个取数单元返回 {} 行，超过联邦内存 join 上限 {}（拒绝静默全表拉取）",
                i,
                rows.len(),
                MAX_FEDERATION_ROWS
            ));
        }
        rows_by_unit.push(rows.clone());
    }

    // 根单元恒为第一个（plan_federated 保证）
    let mut root_docs = rows_by_unit.first().cloned().unwrap_or_default();
    let index = unit_index(sources);

    let edges: Vec<Value> = plan
        .get("join")
        .and_then(|j| j.get("edges"))
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default();

    for edge in &edges {
        let rel = as_str_field(edge, "rel");
        let local = as_str_field(edge, "local");
        let foreign = as_str_field(edge, "foreign");
        let cardinality = as_str_field(edge, "cardinality");
        let key = as_str_field(edge, "key");
        if rel.is_empty() || cardinality.is_empty() {
            return Err("非法 join 边：缺少 rel / cardinality".to_string());
        }
        let idx = *index
            .get(&key)
            .ok_or_else(|| format!("join 边引用了不存在的取数单元 key: {}", key))?;
        let children = &rows_by_unit[idx];

        let path: Vec<String> = edge
            .get("path")
            .and_then(|p| p.as_array())
            .map(|a| {
                a.iter()
                    .map(|x| x.as_str().unwrap_or("").to_string())
                    .collect()
            })
            .unwrap_or_default();

        // foreign 值 → 子文档（哈希 join）
        let mut by_key: HashMap<String, Vec<&Value>> = HashMap::new();
        for c in children {
            let k = id_key(c.get(&foreign).unwrap_or(&Value::Null));
            by_key.entry(k).or_default().push(c);
        }

        let mut parents: Vec<&mut Value> = Vec::new();
        collect_at_path(root_docs.as_mut_slice(), &path, &mut parents);

        for p in parents {
            let Some(obj) = p.as_object_mut() else {
                continue;
            };
            let lk = id_key(obj.get(&local).unwrap_or(&Value::Null));
            let matched = by_key.get(&lk).cloned().unwrap_or_default();
            let value = if cardinality == "one" {
                matched.first().map(|v| (*v).clone()).unwrap_or(Value::Null)
            } else {
                Value::Array(matched.into_iter().cloned().collect())
            };
            obj.insert(rel.clone(), value);
        }
    }

    Ok(Value::Array(root_docs))
}
