//! `$near` → `$vectorSearch` 的抽取、校验与阶段构建（VR-03 / VR-05）
//!
//! 契约（03 §4.1）：`$near` 只允许出现在根 `$condition` 的顶层或 `$and` 内；
//! `$vector` 维度 / 零向量 / `$limit` / `$minScore` 一律在规划期显式校验，
//! 禁止静默截断或跳过。core 产出的中间形态是 Atlas 合法阶段
//! `$vectorSearch` + `$addFields._score`，SQL 方言识别同一形态翻译。

use serde_json::{json, Map, Value};

use crate::command::MAX_PAGE_SIZE;
use crate::schema::Schema;

/// Atlas `$vectorSearch.filter` 可表达的算子集合（不支持 `$regex` / `$not` / `$nor`）
const ATLAS_FILTER_OPS: [&str; 10] = [
    "$eq", "$ne", "$in", "$nin", "$gt", "$gte", "$lt", "$lte", "$exists", "$and",
];

/// Atlas 单次 `$vectorSearch` 的 `limit` 上限（服务端硬限制）
pub const ATLAS_MAX_LIMIT: i64 = 10_000;
/// Atlas `numCandidates` 上限
const ATLAS_MAX_CANDIDATES: i64 = 10_000;

#[derive(Debug, Clone)]
pub struct VectorSpec {
    pub field: String,
    pub vector: Vec<f64>,
    pub limit: i64,
    pub min_score: Option<f64>,
    pub metric: String,
    /// 可进 `$vectorSearch.filter` 的条件（已剥离 `$near`）
    pub filter: Option<Value>,
    /// 不可进 filter、须后置 `$match` 的条件（已剥离 `$near`）
    pub residual: Option<Value>,
    /// Atlas 索引名：schema 索引声明的 `options.indexName`，缺省 `vector_index`
    pub index: String,
}

impl VectorSpec {
    /// Atlas 要求 `numCandidates >= limit`；取 `min(max(limit*10, limit), 10000)`
    pub fn num_candidates(&self) -> i64 {
        (self.limit.saturating_mul(10))
            .max(self.limit)
            .min(ATLAS_MAX_CANDIDATES)
    }
}

/// `$vectorSearch`（首阶段）+ `$addFields._score`
///
/// `metric` 为内部元数据：应用层精确检索需要它按同一口径打分；Mongo 原生执行前
/// 由宿主执行器剥离（Atlas 阶段只认官方键）。
pub fn build_stages(spec: &VectorSpec) -> Vec<Value> {
    let mut stage = Map::new();
    stage.insert("index".into(), json!(spec.index));
    stage.insert("path".into(), json!(spec.field));
    stage.insert("queryVector".into(), json!(spec.vector));
    stage.insert("numCandidates".into(), json!(spec.num_candidates()));
    stage.insert("limit".into(), json!(spec.limit));
    stage.insert("metric".into(), json!(spec.metric));
    if let Some(f) = &spec.filter {
        stage.insert("filter".into(), f.clone());
    }
    vec![
        json!({ "$vectorSearch": Value::Object(stage) }),
        json!({ "$addFields": { "_score": { "$meta": "vectorSearchScore" } } }),
    ]
}

/// 从根 `$condition` 中抽出 `$near`。
///
/// 返回 `(Some(spec), 剩余条件)`；无 `$near` → `(None, 原条件)`。
pub fn split(schema: &Schema, condition: &Value) -> Result<(Option<VectorSpec>, Value), String> {
    let Some(obj) = condition.as_object() else {
        return Ok((None, condition.clone()));
    };
    if !contains_near(condition) {
        return Ok((None, condition.clone()));
    }

    let mut found: Vec<(String, Value)> = Vec::new();
    collect_near(obj, &mut found)?;
    if found.is_empty() {
        // `$near` 出现在非字段位置（如顶层 `$near` 键）
        return Err(
            "向量条件 $near 必须作用于 vector 字段：形如 {\"<字段>\": {\"$near\": {…}}}".into(),
        );
    }
    if found.len() > 1 {
        return Err("$near 只允许一个向量字段（一次查询只做一次向量检索）".into());
    }

    let (field, near) = found.pop().expect("len=1");
    let field_def = schema
        .fields
        .get(&field)
        .ok_or_else(|| format!("字段 {field} 未在 schema \"{}\" 中声明", schema.name))?;
    if field_def.field_type != "vector" {
        return Err(format!("字段 {field} 不是 vector 字段，不能使用 $near"));
    }
    let dim = field_def
        .dim
        .ok_or_else(|| format!("vector 字段 {field} 缺少必填 dim（注册期校验）"))?;
    let metric = field_def.metric_or_default().to_string();

    let near_obj = near
        .as_object()
        .ok_or_else(|| "$near 必须是对象：{\"$vector\": […], \"$limit\": n}".to_string())?;
    for k in near_obj.keys() {
        if !matches!(k.as_str(), "$vector" | "$limit" | "$minScore") {
            return Err(format!(
                "$near 出现未知键 {k}（仅支持 $vector/$limit/$minScore）"
            ));
        }
    }

    let vector = parse_query_vector(near_obj.get("$vector"), Some(dim), &metric, &field)?;
    let limit = parse_limit(near_obj.get("$limit"), &field)?;
    let min_score = parse_min_score(near_obj.get("$minScore"), &metric, &field)?;

    let rest_cond = strip_near(condition, &field);
    let (filter, residual) = split_filter(&rest_cond);

    Ok((
        Some(VectorSpec {
            index: resolve_index(schema, &field),
            field,
            vector,
            limit,
            min_score,
            metric,
            filter,
            residual,
        }),
        rest_cond,
    ))
}

/// 解析核心产出的 `$vectorSearch` 阶段（应用层精确检索 / SQL 翻译共用）。
///
/// `schema = None`：local 应用层求值只吃 core 规划产出的阶段（`build.rs` 已按 schema
/// 校验字段类型 / dim / metric），此处只做阶段自身形状校验，不再二次查 schema。
pub fn parse_stage(schema: Option<&Schema>, arg: &Value) -> Result<VectorSpec, String> {
    let o = arg
        .as_object()
        .ok_or_else(|| "$vectorSearch 必须是对象".to_string())?;
    let field = o
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "$vectorSearch 缺少 path（向量字段名）".to_string())?
        .to_string();
    let (dim, schema_metric) = match schema {
        Some(schema) => {
            let field_def = schema
                .fields
                .get(&field)
                .ok_or_else(|| format!("字段 {field} 未在 schema \"{}\" 中声明", schema.name))?;
            if field_def.field_type != "vector" {
                return Err(format!(
                    "字段 {field} 不是 vector 字段，不能用于 $vectorSearch"
                ));
            }
            let dim = field_def
                .dim
                .ok_or_else(|| format!("vector 字段 {field} 缺少必填 dim（注册期校验）"))?;
            (Some(dim), Some(field_def.metric_or_default().to_string()))
        }
        None => (None, None),
    };
    let metric = o
        .get("metric")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(schema_metric)
        .unwrap_or_else(|| "cosine".to_string());
    if !matches!(metric.as_str(), "cosine" | "l2" | "ip") {
        return Err(format!(
            "$vectorSearch.metric 仅支持 cosine/l2/ip，实际 {metric}（字段 {field}）"
        ));
    }

    let vector = parse_query_vector(o.get("queryVector"), dim, &metric, &field)?;
    let limit = parse_limit(o.get("limit"), &field)?;
    let index = o
        .get("index")
        .and_then(Value::as_str)
        .unwrap_or("vector_index")
        .to_string();

    Ok(VectorSpec {
        index,
        field,
        vector,
        limit,
        min_score: None,
        metric,
        filter: o.get("filter").cloned(),
        residual: None,
    })
}

// ---------------------------------------------------------------------------
// `$near` 定位
// ---------------------------------------------------------------------------

fn contains_near(v: &Value) -> bool {
    match v {
        Value::Object(o) => o.iter().any(|(k, val)| k == "$near" || contains_near(val)),
        Value::Array(a) => a.iter().any(contains_near),
        _ => false,
    }
}

/// 收集字段级 `$near`：只允许顶层字段与 `$and` 内字段。
fn collect_near(obj: &Map<String, Value>, out: &mut Vec<(String, Value)>) -> Result<(), String> {
    for (k, v) in obj {
        if k == "$and" {
            let arr = v
                .as_array()
                .ok_or_else(|| "$and 必须是条件数组".to_string())?;
            for item in arr {
                let sub = item
                    .as_object()
                    .ok_or_else(|| "$and 的元素必须是条件对象".to_string())?;
                collect_near(sub, out)?;
            }
            continue;
        }
        if matches!(k.as_str(), "$or" | "$nor" | "$not") {
            if contains_near(v) {
                return Err(format!(
                    "向量条件 $near 不能出现在 {k} 内（TopK 前无法保证该逻辑的语义）"
                ));
            }
            continue;
        }
        if k.starts_with('$') {
            if contains_near(v) {
                return Err(format!(
                    "向量条件 $near 不能出现在 {k} 内（仅支持 $condition 顶层或 $and 内）"
                ));
            }
            continue;
        }
        if let Some(fields) = v.as_object() {
            if let Some(near) = fields.get("$near") {
                out.push((k.clone(), near.clone()));
            }
        }
    }
    Ok(())
}

/// 移除字段上的 `$near`；空对象 / 空 `$and` 一并收敛。
fn strip_near(cond: &Value, field: &str) -> Value {
    let Some(obj) = cond.as_object() else {
        return cond.clone();
    };
    if let Some(arr) = obj.get("$and").and_then(Value::as_array) {
        let kept: Vec<Value> = arr
            .iter()
            .map(|it| strip_near(it, field))
            .filter(|v| !is_empty_condition(v))
            .collect();
        if kept.is_empty() {
            return Value::Null;
        }
        return json!({ "$and": kept });
    }

    let mut out = Map::new();
    for (k, v) in obj {
        if k == field {
            if let Some(fields) = v.as_object() {
                if fields.contains_key("$near") {
                    let mut rest = fields.clone();
                    rest.remove("$near");
                    if !rest.is_empty() {
                        out.insert(k.clone(), Value::Object(rest));
                    }
                    continue;
                }
            }
        }
        out.insert(k.clone(), v.clone());
    }
    if out.is_empty() {
        Value::Null
    } else {
        Value::Object(out)
    }
}

fn is_empty_condition(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// filter / residual 拆分
// ---------------------------------------------------------------------------

/// 把剩余条件拆为「可进 Atlas filter」与「须后置 `$match`」两段。
///
/// 判定以 `$and` 子项（或整棵条件树）为单位：任一操作符不可表达 → 该子树整体
/// 下沉 residual，不做部分拆分（避免语义被拆散）。
fn split_filter(cond: &Value) -> (Option<Value>, Option<Value>) {
    if is_empty_condition(cond) {
        return (None, None);
    }
    let items: Vec<Value> = match cond
        .as_object()
        .and_then(|o| o.get("$and"))
        .and_then(Value::as_array)
    {
        Some(arr) => arr.clone(),
        None => vec![cond.clone()],
    };
    let mut filterable: Vec<Value> = Vec::new();
    let mut residual: Vec<Value> = Vec::new();
    for it in items {
        if is_atlas_filterable(&it) {
            filterable.push(it);
        } else {
            residual.push(it);
        }
    }
    let join = |mut v: Vec<Value>| -> Option<Value> {
        match v.len() {
            0 => None,
            1 => Some(v.remove(0)),
            _ => Some(json!({ "$and": v })),
        }
    };
    (join(filterable), join(residual))
}

/// Atlas `filter` 可表达性判定（`$regex` / `$not` / `$nor` / `$or` 一律不可）
fn is_atlas_filterable(cond: &Value) -> bool {
    let Some(obj) = cond.as_object() else {
        return false;
    };
    for (k, v) in obj {
        if k == "$and" {
            let Some(arr) = v.as_array() else {
                return false;
            };
            if !arr.iter().all(is_atlas_filterable) {
                return false;
            }
            continue;
        }
        if k.starts_with('$') {
            return false;
        }
        if let Some(ops) = v.as_object() {
            // 字段级操作符对象：所有键都必须可表达；空对象视为不可表达（语义不明）
            if ops.is_empty() {
                return false;
            }
            for op in ops.keys() {
                if !ATLAS_FILTER_OPS.contains(&op.as_str()) {
                    return false;
                }
            }
            // 嵌套逻辑（如 `$not: {$gt: 1}`）走操作符键，已在上面拦截
            for val in ops.values() {
                if val.is_object() && contains_operator_key(val) {
                    return false;
                }
            }
        }
    }
    true
}

fn contains_operator_key(v: &Value) -> bool {
    match v {
        Value::Object(o) => o.keys().any(|k| k.starts_with('$')),
        Value::Array(a) => a.iter().any(contains_operator_key),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// 参数校验
// ---------------------------------------------------------------------------

fn parse_query_vector(
    raw: Option<&Value>,
    dim: Option<u32>,
    metric: &str,
    field: &str,
) -> Result<Vec<f64>, String> {
    let arr = raw
        .and_then(Value::as_array)
        .ok_or_else(|| format!("$near.$vector 必须是数值数组（字段 {field}）"))?;
    match dim {
        Some(dim) if arr.len() != dim as usize => {
            return Err(format!(
                "向量维度不符：字段 {field} 声明 dim={dim}，$vector 长度 {}（禁止截断或补零）",
                arr.len()
            ));
        }
        None if arr.is_empty() => {
            return Err(format!("向量不能为空数组（字段 {field}）"));
        }
        _ => {}
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let n = v
            .as_f64()
            .ok_or_else(|| format!("$near.$vector 第 {i} 个元素必须为数值，实际 {v}"))?;
        if !n.is_finite() {
            return Err(format!(
                "$near.$vector 第 {i} 个元素必须为有限数值，实际 {v}"
            ));
        }
        out.push(n);
    }
    if metric == "cosine" && out.iter().all(|x| *x == 0.0) {
        return Err(format!(
            "$near.$vector 为零向量（字段 {field}，cosine 距离未定义）"
        ));
    }
    Ok(out)
}

fn parse_limit(raw: Option<&Value>, field: &str) -> Result<i64, String> {
    let n = raw
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("$near.$limit 必填且必须为正整数（字段 {field}）"))?;
    if n <= 0 {
        return Err(format!(
            "$near.$limit 必须为正整数，实际 {n}（字段 {field}）"
        ));
    }
    if n > ATLAS_MAX_LIMIT {
        return Err(format!(
            "$near.$limit={n} 超过 Atlas 上限 {ATLAS_MAX_LIMIT}（字段 {field}）"
        ));
    }
    if n as f64 > MAX_PAGE_SIZE {
        return Err(format!(
            "$near.$limit={n} 超过单页上限 {}（与普通分页同口径，字段 {field}）",
            MAX_PAGE_SIZE as i64
        ));
    }
    Ok(n)
}

fn parse_min_score(raw: Option<&Value>, metric: &str, field: &str) -> Result<Option<f64>, String> {
    let Some(v) = raw else {
        return Ok(None);
    };
    let n = v
        .as_f64()
        .ok_or_else(|| format!("$near.$minScore 必须是数值（字段 {field}）"))?;
    if !n.is_finite() {
        return Err(format!(
            "$near.$minScore 必须是有限数值，实际 {v}（字段 {field}）"
        ));
    }
    if matches!(metric, "cosine" | "l2") && !(0.0..=1.0).contains(&n) {
        return Err(format!(
            "$near.$minScore={n} 越界：metric={metric} 时必须在 [0, 1]（字段 {field}）"
        ));
    }
    Ok(Some(n))
}

/// 索引名解析：`keys.<field> == "vector"` 的声明项读 `options.indexName`。
fn resolve_index(schema: &Schema, field: &str) -> String {
    for idx in &schema.indexes {
        let Some(keys) = idx.get("keys").and_then(Value::as_object) else {
            continue;
        };
        let is_vec = keys
            .get(field)
            .and_then(Value::as_str)
            .map(|t| t == "vector")
            .unwrap_or(false);
        if !is_vec {
            continue;
        }
        if let Some(name) = idx
            .get("options")
            .and_then(|o| o.get("indexName"))
            .and_then(Value::as_str)
        {
            return name.to_string();
        }
    }
    "vector_index".to_string()
}
