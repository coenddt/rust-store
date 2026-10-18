//! 应用层精确向量检索（MySQL / SQLite / 自建 Mongo / local / memory 共用）
//!
//! 契约（03 §4.6）：
//! - 打分口径与 Atlas / pgvector 逐值一致：cosine `dot/(‖a‖·‖b‖)`、
//!   l2 `1/(1+√Σ(aᵢ-bᵢ)²)`、ip `Σaᵢ·bᵢ`；
//! - `‖b‖ == 0`（存量零向量行）不参与排序，计入 `skipped_zero` 并显式告警，禁返回 `NaN`/`Infinity`；
//! - 排序 `_score` 降序、同分 `_id` 升序（稳定）；`minScore` 在排序后过滤，条数可少于 `limit`。

use std::cmp::Ordering;

use serde_json::{json, Value};

use crate::local::value;
use crate::pipeline::vector::VectorSpec;

/// 单次应用层向量扫描的最大行数；超限显式 Err（禁静默截断）。
pub const MAX_VECTOR_SCAN_ROWS: usize = 100_000;

/// 解析核心产出的 `$vectorSearch` 阶段（与 `dialect/select/aggregate.rs` 共用同一份实现）。
///
/// 阶段由 `pipeline::build` 产出，字段类型 / dim 已在规划期校验，此处不做 schema 二次校验。
pub fn parse_stage(arg: &Value) -> Result<VectorSpec, String> {
    crate::pipeline::vector::parse_stage(None, arg)
}

/// 一致性打分：与 Atlas / pgvector 同口径。
pub fn score(metric: &str, a: &[f64], b: &[f64]) -> Result<f64, String> {
    if a.len() != b.len() {
        return Err(format!(
            "向量维度不一致：查询 {} 维，行 {} 维（禁止截断或补零）",
            a.len(),
            b.len()
        ));
    }
    let r = match metric {
        "cosine" => {
            let (na, nb) = (norm(a), norm(b));
            if na == 0.0 || nb == 0.0 {
                return Err(format!("零向量不参与 cosine 打分（维度 {}）", a.len()));
            }
            dot(a, b) / (na * nb)
        }
        "l2" => {
            let sum: f64 = a
                .iter()
                .zip(b.iter())
                .map(|(x, y)| (x - y) * (x - y))
                .sum();
            1.0 / (1.0 + sum.sqrt())
        }
        "ip" => dot(a, b),
        other => {
            return Err(format!(
                "不支持的向量度量 {other}（仅 cosine/l2/ip）"
            ));
        }
    };
    if !r.is_finite() {
        return Err(format!("向量打分结果非有限值（metric={metric}）"));
    }
    Ok(r)
}

/// 对候选行做精确 TopK + `minScore`，并写入 `_score`。
pub fn search(rows: Vec<Value>, spec: &VectorSpec) -> Result<Vec<Value>, String> {
    if rows.len() > MAX_VECTOR_SCAN_ROWS {
        return Err(format!(
            "vectorScanOverflow：候选行数 {} 超过上限 {}（禁止静默截断，请先缩小过滤条件）",
            rows.len(),
            MAX_VECTOR_SCAN_ROWS
        ));
    }

    let mut skipped_zero = 0_usize;
    let mut scored: Vec<Value> = Vec::with_capacity(rows.len().min(spec.limit as usize));
    for mut row in rows {
        let raw = value::get_path(&row, &spec.field).ok_or_else(|| {
            format!(
                "候选行缺少向量字段 {}（禁止拿空列打分）",
                spec.field
            )
        })?;
        let b = row_vector(raw, &spec.field)?;
        if norm(&b) == 0.0 {
            // 契约：零向量行不参与排序，但必须显式计数并告警（禁止静默丢行）。
            skipped_zero += 1;
            continue;
        }
        let s = score(&spec.metric, &spec.vector, &b)?;
        value::set_path(&mut row, "_score", json!(s));
        scored.push(row);
    }
    if skipped_zero > 0 {
        // core local 层无 feedback 通道（05 建）；此处先以 stderr 显式告警，禁止静默。
        eprintln!(
            "vector_skipped_zero: 字段 {} 有 {skipped_zero} 行零向量未参与排序",
            spec.field
        );
    }

    scored.sort_by(|a, b| {
        let sa = a.get("_score").and_then(Value::as_f64).unwrap_or(0.0);
        let sb = b.get("_score").and_then(Value::as_f64).unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(Ordering::Equal)
            .then_with(|| match (a.get("_id"), b.get("_id")) {
                (Some(x), Some(y)) => value::compare(x, y),
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            })
    });

    if let Some(min) = spec.min_score {
        scored.retain(|r| r.get("_score").and_then(Value::as_f64).unwrap_or(0.0) >= min);
    }
    scored.truncate(spec.limit as usize);
    Ok(scored)
}

/// 行内向量值 → `Vec<f64>`（必须是有限数值数组；非数组 / 非数值一律 Err）。
fn row_vector(raw: &Value, field: &str) -> Result<Vec<f64>, String> {
    let arr = raw
        .as_array()
        .ok_or_else(|| format!("字段 {field} 的值不是数值数组（禁止拿空列打分）"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let n = v
            .as_f64()
            .ok_or_else(|| format!("字段 {field} 第 {i} 个元素不是数值，实际 {v}"))?;
        if !n.is_finite() {
            return Err(format!("字段 {field} 第 {i} 个元素不是有限数值，实际 {v}"));
        }
        out.push(n);
    }
    Ok(out)
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn norm(v: &[f64]) -> f64 {
    v.iter().map(|x| x * x).sum::<f64>().sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::vector::build_stages;
    use serde_json::json;

    fn spec(limit: i64) -> VectorSpec {
        VectorSpec {
            field: "embedding".into(),
            vector: vec![1.0, 0.0],
            limit,
            min_score: None,
            metric: "cosine".into(),
            filter: None,
            residual: None,
            index: "vector_index".into(),
        }
    }

    fn rows() -> Vec<Value> {
        vec![
            json!({"_id": "a", "embedding": [1.0, 0.0]}),
            json!({"_id": "b", "embedding": [0.0, 1.0]}),
            json!({"_id": "c", "embedding": [3.0, 4.0]}),
            json!({"_id": "d", "embedding": [0.5, 0.0]}),
        ]
    }

    #[test]
    fn topk_matches_brute_force() {
        let out = search(rows(), &spec(3)).unwrap();
        // 暴力算：a=1.0、d=1.0（同分按 _id 升序）、c=3/5=0.6、b=0.0
        let ids: Vec<&str> = out.iter().map(|r| r["_id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["a", "d", "c"]);
        let scores: Vec<f64> = out.iter().map(|r| r["_score"].as_f64().unwrap()).collect();
        assert!((scores[0] - 1.0).abs() < 1e-12);
        assert!((scores[1] - 1.0).abs() < 1e-12);
        assert!((scores[2] - 0.6).abs() < 1e-12);
    }

    #[test]
    fn zero_vector_row_skipped_without_nan() {
        let mut input = rows();
        input.push(json!({"_id": "z", "embedding": [0.0, 0.0]}));
        let out = search(input, &spec(10)).unwrap();
        assert!(out.iter().all(|r| r["_id"] != "z"));
        assert!(out
            .iter()
            .all(|r| r["_score"].as_f64().map(f64::is_finite) == Some(true)));
    }

    #[test]
    fn dimension_mismatch_errors() {
        let input = vec![json!({"_id": "x", "embedding": [1.0, 0.0, 0.0]})];
        let err = search(input, &spec(1)).unwrap_err();
        assert!(err.contains("维度"), "err = {err}");
        assert!(score("cosine", &[1.0, 0.0], &[1.0]).is_err());
    }

    #[test]
    fn l2_and_ip_scores_match_formulas() {
        // l2: Σ(aᵢ-bᵢ)² = 0+4 → sqrt=2 → 1/(1+2) = 1/3；ip: 2*3 + 1*4 = 10
        assert!((score("l2", &[1.0, 1.0], &[1.0, 3.0]).unwrap() - 1.0 / 3.0).abs() < 1e-12);
        assert!((score("ip", &[2.0, 1.0], &[3.0, 4.0]).unwrap() - 10.0).abs() < 1e-12);
    }

    #[test]
    fn min_score_filters_after_sort() {
        let mut s = spec(10);
        s.min_score = Some(0.9);
        let out = search(rows(), &s).unwrap();
        let ids: Vec<&str> = out.iter().map(|r| r["_id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["a", "d"]);
    }

    #[test]
    fn pipeline_vector_search_scores_and_meta_is_noop() {
        let collections = json!({ "artworks": rows() });
        let stages = build_stages(&spec(2));
        let out = crate::local::pipeline::run(&collections, "artworks", &stages).unwrap();
        let ids: Vec<&str> = out.iter().map(|r| r["_id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["a", "d"]);
        assert!((out[0]["_score"].as_f64().unwrap() - 1.0).abs() < 1e-12);

        // residual `$match` 在 TopK 之后（Atlas 同序）
        let mut later = stages.clone();
        later.push(json!({ "$match": { "_id": "d" } }));
        let out = crate::local::pipeline::run(&collections, "artworks", &later).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["_id"], "d");
    }
}
