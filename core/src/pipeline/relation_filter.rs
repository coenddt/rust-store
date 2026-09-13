//! §9.6 关系聚合谓词（跨表条件过滤 / semi-join）
//!
//! `$condition` 的键命中 `schema.relations` 的关系名 → 关系聚合谓词（合法）；
//! 命中 `schema.fields` 的 array/object 字段 → 维持 U1/U2 → Err。
//!
//! 设计原则（§9.6）：**不新增阶段、不新增执行序** —— 它就是 `$condition` 的一种新键型，
//! 属 `$condition` 段，在根 `$group` 之前执行（§9.3 固定序）。
//!
//! 归一表示：每个关系谓词发射一个 `$lookup`（子 pipeline 内 `$match`+`$group`+`$match`），
//! 并在改写后的根条件里把该谓词替换为「哨兵字段」代理：
//!
//! - semi-join：`{ "<as>.0": { "$exists": true } }`（`$lookup` 结果非空即命中）
//! - anti-join：`{ "<as>.0": { "$exists": false } }`（`$not` / `$exists:false` 否定）
//!
//! 该代理在 Mongo 侧直接可执行（数组下标 0 存在 ⇔ 数组非空），在 SQL 侧由
//! [`crate::dialect::select`] 识别为 `EXISTS` / `NOT EXISTS`（§10.5 下推优先）。
//! 父行数量与文档形状都不变 —— **不扇出**。

use serde_json::{json, Map, Value};

use crate::command::ERR_PERMISSION;
use crate::permission::{
    get_readable_relations, is_field_readable, merge_owner_condition, Context,
};
use crate::schema::{Registry, Schema};
use crate::types::{validate_condition, AGG_OPS};

use super::group::{self, AggDef};
use super::lookup::{is_array_local_field, rel_let_expr, rel_match_expr};
use super::util::{collect_having_agg_refs, non_nullish};

/// 关系聚合谓词 `$lookup.as` 前缀（SQL 侧据此识别并翻译为 `EXISTS` / `NOT EXISTS`）。
///
/// `as` 形态：`__rp{序号}__{关系名}`（关系名用于在目标 schema 上定位 `RelationDef`）。
pub const REL_PRED_PREFIX: &str = "__rp";

/// 比较算子白名单（谓词值形状 `{ "$of"?: field, "<比较算子>": value }`）
const CMP_OPS: [&str; 6] = ["$gt", "$gte", "$lt", "$lte", "$eq", "$ne"];

/// 单个关系聚合谓词（归一表示）
#[derive(Debug, Clone)]
pub struct RelPredicate {
    /// 关系名（根 schema.relations 的键）
    pub rel_name: String,
    /// `$lookup.as`（同时是代理字段前缀）
    pub as_name: String,
    /// 目标 model（定位目标 schema 的唯一依据）
    pub model: String,
    /// 关系本地键（根表标量列）
    pub local_field: String,
    /// 关系外键（子表标量列，子 pipeline `$group` 键）
    pub foreign_field: String,
    /// 子级过滤（仅标量域）；None = 无
    pub filter: Option<Value>,
    /// 本块聚合别名 → 聚合定义（仅保留 having 引用到的）
    pub agg: Vec<(String, AggDef)>,
    /// 谓词条件（仅可引用本块 agg 别名）
    pub having: Value,
    /// 否定（`$not` / `$exists:false`）→ anti-join
    pub negated: bool,
}

/// 关系聚合谓词规划结果：`$lookup` 阶段 + 改写后的根条件
#[derive(Debug, Clone)]
pub struct RelationFilterPlan {
    pub lookups: Vec<Value>,
    pub condition: Value,
    pub predicates: Vec<RelPredicate>,
}

/// 从根条件抽取关系聚合谓词；无关系谓词 → `Ok(None)`（保持既有标量路径逐字节不变）。
pub fn plan(
    schema: &Schema,
    registry: &Registry,
    ctx: Option<&Context>,
    cond: &Value,
) -> Result<Option<RelationFilterPlan>, String> {
    // 服务端执行类操作符拒绝名单（D-02）：先于一切解析
    validate_condition(cond)?;

    let mut preds: Vec<RelPredicate> = Vec::new();
    let rewritten = rewrite_value(schema, registry, cond, &mut preds)?;
    if preds.is_empty() {
        return Ok(None);
    }

    // 关系级读权限（§9.1 R6）：不可读 → Err(ERR_PERMISSION)，绝不静默当 false
    // （R0 决策 #1：表级与关系级策略一致，同一 code + 文案）。
    if let Some(readable) = get_readable_relations(schema, ctx) {
        for p in &preds {
            if !readable.contains(&p.rel_name) {
                return Err(ERR_PERMISSION.to_string());
            }
        }
    }

    // F3：关系聚合谓词引用的**子字段**（`filter` / `agg` / 简写 `$of`）必须过
    // 子模型 `field.read`；越权 → Err(ERR_PERMISSION)（防止经聚合/谓词推断无权字段）。
    if ctx.is_some() {
        for p in &preds {
            let rel_schema = registry.get(&p.model)?;
            if let Some(filter) = &p.filter {
                check_filter_readable(rel_schema, ctx, filter)?;
            }
            for (_, def) in &p.agg {
                if let Some(field) = &def.field {
                    if !is_field_readable(rel_schema, ctx, field) {
                        return Err(ERR_PERMISSION.to_string());
                    }
                }
            }
        }
    }

    let mut lookups: Vec<Value> = Vec::new();
    for p in &preds {
        lookups.push(build_lookup_stage(schema, registry, ctx, p)?);
    }
    Ok(Some(RelationFilterPlan {
        lookups,
        condition: rewritten,
        predicates: preds,
    }))
}

/// 条件树改写：关系谓词叶子 → `$lookup` 代理键；逻辑组递归；二级关系路径 → Err。
fn rewrite_value(
    schema: &Schema,
    registry: &Registry,
    v: &Value,
    preds: &mut Vec<RelPredicate>,
) -> Result<Value, String> {
    match v {
        Value::Object(m) => rewrite_object(schema, registry, m, preds),
        Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for it in a {
                out.push(rewrite_value(schema, registry, it, preds)?);
            }
            Ok(Value::Array(out))
        }
        other => Ok(other.clone()),
    }
}

fn rewrite_object(
    schema: &Schema,
    registry: &Registry,
    m: &Map<String, Value>,
    preds: &mut Vec<RelPredicate>,
) -> Result<Value, String> {
    let mut out = Map::new();
    for (k, v) in m {
        match k.as_str() {
            "$and" | "$or" | "$nor" => {
                let arr = v.as_array().ok_or_else(|| format!("{k} 需要数组"))?;
                let mut na = Vec::with_capacity(arr.len());
                for it in arr {
                    na.push(rewrite_value(schema, registry, it, preds)?);
                }
                out.insert(k.clone(), Value::Array(na));
            }
            "$not" => {
                // §9.6：`{ "$not": { "<rel>": { … } } }` → anti-join
                let vo = v
                    .as_object()
                    .ok_or("$not 需要对象（仅支持包裹单个关系聚合谓词）")?;
                let mut it = vo.iter();
                let (rk, rv) = match (it.next(), it.next()) {
                    (Some(kv), None) => kv,
                    _ => {
                        return Err(
                            "$not 仅支持包裹单个关系聚合谓词（否定 = anti-join）".to_string()
                        )
                    }
                };
                if !schema.relations.contains_key(rk) {
                    return Err(format!(
                        "$not 仅支持包裹关系聚合谓词（否定 = anti-join），收到键 \"{rk}\""
                    ));
                }
                build_pred(schema, registry, rk, rv, true, preds)?;
                let p = preds
                    .last()
                    .ok_or("关系聚合谓词未产生条件（内部不变量破裂）")?;
                out.insert(proxy_key(&p.as_name), proxy_cond(p.negated));
            }
            _ => {
                if schema.relations.contains_key(k) {
                    build_pred(schema, registry, k, v, false, preds)?;
                    let p = preds
                        .last()
                        .ok_or("关系聚合谓词未产生条件（内部不变量破裂）")?;
                    out.insert(proxy_key(&p.as_name), proxy_cond(p.negated));
                } else {
                    // 二级关系路径（`orders.items…`）：首批不支持 → Err（§9.6 风险与护栏）
                    if let Some((head, _)) = k.split_once('.') {
                        if schema.relations.contains_key(head) {
                            return Err(format!(
                                "二级关系路径 \"{k}\" 首批不支持（关系聚合谓词仅支持一级关系）"
                            ));
                        }
                    }
                    out.insert(k.clone(), v.clone());
                }
            }
        }
    }
    Ok(Value::Object(out))
}

/// 代理键：`<as>.0`（Mongo 数组下标 0 存在 ⇔ 数组非空）
fn proxy_key(as_name: &str) -> String {
    format!("{}.0", as_name)
}

/// 代理条件：semi → `$exists:true`；anti → `$exists:false`
fn proxy_cond(negated: bool) -> Value {
    json!({ "$exists": !negated })
}

/// 解析并校验单个关系聚合谓词，推入 `preds`
fn build_pred(
    schema: &Schema,
    registry: &Registry,
    rel_name: &str,
    spec: &Value,
    negated: bool,
    preds: &mut Vec<RelPredicate>,
) -> Result<(), String> {
    let rel_def = schema
        .relations
        .get(rel_name)
        .ok_or_else(|| format!("关系 \"{rel_name}\" 未在 schema \"{}\" 中定义", schema.name))?;
    let rel_schema = registry.get(&rel_def.model)?;
    let obj = spec.as_object().ok_or_else(|| {
        format!("关系聚合谓词 \"{rel_name}\" 必须是对象（主形式 filter/agg/having 或简写形式）")
    })?;

    let is_main = ["filter", "agg", "having"]
        .iter()
        .any(|k| obj.contains_key(*k));
    let (filter, agg, having, extra_neg) = if is_main {
        parse_main(rel_schema, rel_name, obj)?
    } else {
        parse_simple(rel_schema, rel_name, obj)?
    };

    let as_name = format!("{}{}__{}", REL_PRED_PREFIX, preds.len(), rel_name);
    preds.push(RelPredicate {
        rel_name: rel_name.to_string(),
        as_name,
        model: rel_def.model.clone(),
        local_field: rel_def.local_field.clone(),
        foreign_field: rel_def.foreign_field.clone(),
        filter,
        agg,
        having,
        negated: negated ^ extra_neg,
    });
    Ok(())
}

/// 关系聚合谓词解析结果：`(filter?, agg, having, negated)`
type ParsedRelPredicate = Result<(Option<Value>, Vec<(String, AggDef)>, Value, bool), String>;

/// 主形式：`{ filter?, agg, having }`
fn parse_main(rel_schema: &Schema, rel_name: &str, obj: &Map<String, Value>) -> ParsedRelPredicate {
    for k in obj.keys() {
        if !matches!(k.as_str(), "filter" | "agg" | "having") {
            return Err(format!(
                "关系聚合谓词 \"{rel_name}\" 主形式仅支持 filter/agg/having，收到 \"{k}\""
            ));
        }
    }
    let having = non_nullish(obj.get("having"))
        .cloned()
        .ok_or_else(|| format!("关系聚合谓词 \"{rel_name}\" 主形式必须提供 having"))?;
    if !having.is_object() {
        return Err(format!(
            "关系聚合谓词 \"{rel_name}\" 的 having 必须是条件对象"
        ));
    }
    let filter = non_nullish(obj.get("filter")).cloned();
    if let Some(f) = &filter {
        validate_scalar_filter(rel_schema, rel_name, f)?;
    }

    // agg：省略无法推导 having 引用的算子/字段 → Err（绝不臆测）
    let agg_v = non_nullish(obj.get("agg")).ok_or_else(|| {
        format!(
            "关系聚合谓词 \"{rel_name}\" 省略 agg 时无法推导 having 引用的聚合（请显式声明 agg）"
        )
    })?;
    let gs = group::parse(rel_schema, &json!({ "agg": agg_v }))
        .map_err(|e| format!("关系聚合谓词 \"{rel_name}\" 的 agg 非法: {e}"))?;

    // having：仅可引用本块 agg 别名（`by` 恒为空 → by 键域为空）
    let check = group::GroupSpec {
        by: Vec::new(),
        agg: gs.agg.clone(),
    };
    group::validate_having(&check, &having)
        .map_err(|e| format!("关系聚合谓词 \"{rel_name}\" 的 having 非法: {e}"))?;

    // 按需发射（§9.2 按需计算）：仅保留 having 引用到的别名
    let mut refs = std::collections::HashSet::new();
    collect_having_agg_refs(&having, &|k| gs.agg.iter().any(|(a, _)| a == k), &mut refs);
    if refs.is_empty() {
        return Err(format!(
            "关系聚合谓词 \"{rel_name}\" 的 having 未引用任何 agg 别名"
        ));
    }
    let agg: Vec<(String, AggDef)> = gs
        .agg
        .into_iter()
        .filter(|(a, _)| refs.contains(a))
        .collect();
    Ok((filter, agg, having, false))
}

/// 简写形式：可选 `$filter` ＋ 恰好一个聚合谓词
fn parse_simple(
    rel_schema: &Schema,
    rel_name: &str,
    obj: &Map<String, Value>,
) -> ParsedRelPredicate {
    let mut filter: Option<Value> = None;
    let mut ops: Vec<(&String, &Value)> = Vec::new();
    for (k, v) in obj {
        if k == "$filter" {
            if !v.is_null() {
                validate_scalar_filter(rel_schema, rel_name, v)?;
                filter = Some(v.clone());
            }
        } else if k == "$exists" || AGG_OPS.contains(&k.as_str()) {
            ops.push((k, v));
        } else {
            return Err(format!(
                "关系聚合谓词 \"{rel_name}\" 简写形式不支持的键 \"{k}\"（仅 $filter ＋ 恰好一个聚合谓词）"
            ));
        }
    }
    if ops.len() != 1 {
        return Err(format!(
            "关系聚合谓词 \"{rel_name}\" 简写形式必须恰有一个聚合谓词（$exists/$count/$sum/$avg/$min/$max；多谓词请用主形式 having）"
        ));
    }
    let (opk, opv) = ops[0];
    match opk.as_str() {
        "$exists" => {
            let b = opv
                .as_bool()
                .ok_or_else(|| format!("关系聚合谓词 \"{rel_name}\" 的 $exists 必须是布尔"))?;
            Ok((
                filter,
                vec![(
                    "n".to_string(),
                    AggDef {
                        op: "$count".to_string(),
                        field: None,
                    },
                )],
                cond_obj("n", "$gt", &json!(0)),
                !b,
            ))
        }
        "$count" => {
            let (of, cmp, val) = parse_pred_value(opv, false, rel_name, "$count")?;
            if let Some(f) = &of {
                validate_scalar_field(rel_schema, rel_name, f)?;
            }
            Ok((
                filter,
                vec![(
                    "n".to_string(),
                    AggDef {
                        op: "$count".to_string(),
                        field: of,
                    },
                )],
                cond_obj("n", &cmp, &val),
                false,
            ))
        }
        op @ ("$sum" | "$avg" | "$min" | "$max") => {
            let (of, cmp, val) = parse_pred_value(opv, true, rel_name, op)?;
            let f =
                of.ok_or_else(|| format!("关系聚合谓词 \"{rel_name}\" 的 {op} 必须带 $of 字段"))?;
            validate_scalar_field(rel_schema, rel_name, &f)?;
            Ok((
                filter,
                vec![(
                    "v".to_string(),
                    AggDef {
                        op: op.to_string(),
                        field: Some(f),
                    },
                )],
                cond_obj("v", &cmp, &val),
                false,
            ))
        }
        _ => Err(format!(
            "关系聚合谓词 \"{rel_name}\" 简写形式不支持的聚合算子 \"{opk}\""
        )),
    }
}

/// 谓词值形状：`{ "$of"?: field, "<比较算子>": value }`（恰好一个比较算子）
fn parse_pred_value(
    v: &Value,
    require_of: bool,
    rel_name: &str,
    op: &str,
) -> Result<(Option<String>, String, Value), String> {
    let o = v
        .as_object()
        .ok_or_else(|| format!("关系聚合谓词 \"{rel_name}\" 的 {op} 值必须是对象"))?;
    let mut of: Option<String> = None;
    let mut cmp: Option<(String, Value)> = None;
    for (k, val) in o {
        if k == "$of" {
            let f = val
                .as_str()
                .ok_or_else(|| format!("关系聚合谓词 \"{rel_name}\" 的 $of 必须是字段名"))?;
            of = Some(f.to_string());
        } else if CMP_OPS.contains(&k.as_str()) {
            if cmp.is_some() {
                return Err(format!(
                    "关系聚合谓词 \"{rel_name}\" 的 {op} 仅允许一个比较算子（多谓词请用主形式 having）"
                ));
            }
            cmp = Some((k.clone(), val.clone()));
        } else {
            return Err(format!(
                "关系聚合谓词 \"{rel_name}\" 的 {op} 不支持的键 \"{k}\"（仅 $of ＋ 一个比较算子）"
            ));
        }
    }
    if require_of && of.is_none() {
        return Err(format!(
            "关系聚合谓词 \"{rel_name}\" 的 {op} 必须带 $of 字段"
        ));
    }
    let (cmp_op, cmp_val) = cmp.ok_or_else(|| {
        format!("关系聚合谓词 \"{rel_name}\" 的 {op} 缺少比较算子（$gt/$gte/$lt/$lte/$eq/$ne）")
    })?;
    Ok((of, cmp_op, cmp_val))
}

/// 构造单键条件对象 `{ alias: { op: val } }`
fn cond_obj(alias: &str, op: &str, val: &Value) -> Value {
    let mut inner = Map::new();
    inner.insert(op.to_string(), val.clone());
    let mut outer = Map::new();
    outer.insert(alias.to_string(), Value::Object(inner));
    Value::Object(outer)
}

/// F3：递归校验关系聚合谓词 `filter` 引用的子字段可读性。
///
/// 逻辑组（`$and/$or/$nor`）递归；标量键过子模型 `field.read`（关系/数组/对象等形态
/// 已在 [`validate_scalar_filter`] 处拒绝）。越权 → `Err(ERR_PERMISSION)`。
fn check_filter_readable(
    rel_schema: &Schema,
    ctx: Option<&Context>,
    filter: &Value,
) -> Result<(), String> {
    let Some(m) = filter.as_object() else {
        return Ok(());
    };
    for (k, v) in m {
        if matches!(k.as_str(), "$and" | "$or" | "$nor") {
            if let Some(arr) = v.as_array() {
                for it in arr {
                    check_filter_readable(rel_schema, ctx, it)?;
                }
            }
            continue;
        }
        if k.starts_with('$') {
            continue;
        }
        if !is_field_readable(rel_schema, ctx, k) {
            return Err(ERR_PERMISSION.to_string());
        }
    }
    Ok(())
}

/// 子级过滤（filter）仅标量域：关系 / 数组 / 对象 / 点号路径 / schema 外字段 → Err
fn validate_scalar_filter(
    rel_schema: &Schema,
    rel_name: &str,
    filter: &Value,
) -> Result<(), String> {
    let Value::Object(m) = filter else {
        return Err(format!(
            "关系聚合谓词 \"{rel_name}\" 的 filter 必须是对象（仅标量域）"
        ));
    };
    for (k, v) in m {
        if matches!(k.as_str(), "$and" | "$or" | "$nor") {
            let arr = v
                .as_array()
                .ok_or_else(|| format!("关系聚合谓词 \"{rel_name}\" 的 filter 中 {k} 需要数组"))?;
            for it in arr {
                validate_scalar_filter(rel_schema, rel_name, it)?;
            }
            continue;
        }
        if k.starts_with('$') {
            return Err(format!(
                "关系聚合谓词 \"{rel_name}\" 的 filter 不支持操作符 \"{k}\"（仅标量字段条件）"
            ));
        }
        validate_scalar_field(rel_schema, rel_name, k)?;
    }
    Ok(())
}

/// 标量字段校验（子 schema）：关系 / 数组 / 对象 / 点号路径 / schema 外 → Err
fn validate_scalar_field(rel_schema: &Schema, rel_name: &str, field: &str) -> Result<(), String> {
    if rel_schema.relations.contains_key(field) {
        return Err(format!(
            "关系聚合谓词 \"{rel_name}\" 引用了关系字段 \"{field}\"（仅支持一级关系的标量字段；二级路径首批不支持）"
        ));
    }
    if field.contains('.') {
        return Err(format!(
            "关系聚合谓词 \"{rel_name}\" 字段 \"{field}\" 不可为点号路径（仅支持标量字段）"
        ));
    }
    match rel_schema.fields.get(field) {
        None => Err(format!(
            "关系聚合谓词 \"{rel_name}\" 引用了 schema 外字段 \"{field}\""
        )),
        Some(fd) if fd.field_type == "array" => Err(format!(
            "关系聚合谓词 \"{rel_name}\" 字段 \"{field}\" 是数组字段（U1/D2：仅支持标量域）"
        )),
        Some(fd) if fd.field_type == "object" => Err(format!(
            "关系聚合谓词 \"{rel_name}\" 字段 \"{field}\" 是对象字段（U2/D2：仅支持标量域）"
        )),
        Some(_) => Ok(()),
    }
}

/// 构建单个关系谓词的 `$lookup`：子 pipeline = `$match`（外键 + filter + owner）→ `$group` → `$match`(having)
fn build_lookup_stage(
    schema: &Schema,
    registry: &Registry,
    ctx: Option<&Context>,
    p: &RelPredicate,
) -> Result<Value, String> {
    let rel_schema = registry.get(&p.model)?;
    let is_array = is_array_local_field(schema, &p.local_field);
    let let_var = format!("rel_{}", p.local_field);

    // 外键匹配 + 子级 filter + 目标 owner 注入（子行越权防护）
    let mut ands: Vec<Value> = vec![rel_match_expr(&p.foreign_field, &let_var, is_array)];
    if let Some(f) = &p.filter {
        ands.push(f.clone());
    }
    if let Some(owner) = merge_owner_condition(rel_schema, ctx, None) {
        ands.push(owner);
    }
    let match_doc = if ands.len() == 1 {
        ands.remove(0)
    } else {
        json!({ "$and": ands })
    };

    // `$group`：`_id` = 外键（`$match` 已限定单父，至多一组）；聚合别名参见 agg
    let mut group_map = Map::new();
    group_map.insert("_id".to_string(), json!(format!("${}", p.foreign_field)));
    for (alias, def) in &p.agg {
        group_map.insert(alias.clone(), group::acc_expr(def));
    }

    let mut let_map = Map::new();
    let_map.insert(let_var, rel_let_expr(&p.local_field, is_array));

    let mut inner = Map::new();
    inner.insert(
        "from".to_string(),
        Value::String(rel_schema.collection.clone()),
    );
    inner.insert("let".to_string(), Value::Object(let_map));
    inner.insert(
        "pipeline".to_string(),
        json!([
            { "$match": match_doc },
            { "$group": Value::Object(group_map) },
            { "$match": p.having.clone() },
        ]),
    );
    inner.insert("as".to_string(), Value::String(p.as_name.clone()));
    Ok(json!({ "$lookup": Value::Object(inner) }))
}

/// 从 `as` 名解析关系名（`__rp{序号}__{关系名}`）
pub fn rel_name_from_as(as_name: &str) -> Option<&str> {
    let rest = as_name.strip_prefix(REL_PRED_PREFIX)?;
    let (_, rel) = rest.split_once("__")?;
    Some(rel)
}
