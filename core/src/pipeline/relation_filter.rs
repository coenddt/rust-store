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

use crate::command::{forbid_t2q_shape, ERR_PERMISSION};
use crate::permission::{
    get_readable_relations, is_field_readable, merge_owner_condition, Context,
};
use crate::rbac::{is_field_readable_overlay, row_condition};
use crate::schema::{Profile, Registry, Schema};
use crate::types::{validate_condition, AGG_OPS};

use super::group::{self, AggDef};
use super::lookup::{is_array_local_field, rel_let_expr, rel_match_expr};
use super::util::{collect_having_agg_refs, non_nullish};

/// 关系聚合谓词 `$lookup.as` 前缀（SQL 侧据此识别并翻译为 `EXISTS` / `NOT EXISTS`）。
///
/// `as` 形态：`__rp{序号}__{关系名}`（关系名用于在目标 schema 上定位 `RelationDef`）。
pub const REL_PRED_PREFIX: &str = "__rp";

/// 嵌套关系 `$lookup.as` 前缀（8c-2）：关系谓词 `filter` 内下钻子 schema 的关系时，
/// 内层 `$lookup` 的 `as` 形态为 `__rn{序号}__{关系名}`（关系名在**子 schema** 上定位
/// `RelationDef`；SQL 侧据此识别并翻译为嵌套 `EXISTS`）。
pub const REL_NESTED_PREFIX: &str = "__rn";

/// 嵌套关系过滤（关系谓词 `filter` 内下钻子 schema 的关系，如 `items.price`）—— §4.1 #4 → 8c-2。
///
/// 语义：子行（关系目标）**再**满足其自身某个关系的条件（一层），
/// Mongo 侧走内层 `$lookup` + 点号路径（数组 ANY）；SQL 侧走嵌套 `EXISTS`（不扇出）。
#[derive(Debug, Clone)]
pub struct NestedRelFilter {
    /// 子 schema 的关系名（如 `items`）
    pub rel_name: String,
    /// Mongo 内层 `$lookup.as`（`__rn{序号}__{关系名}`）
    pub as_name: String,
    /// 该嵌套关系的目标 model
    pub model: String,
    /// 该嵌套关系的本地键（父 = 子 schema 表的字段）
    pub local_field: String,
    /// 该嵌套关系的外键（孙表字段）
    pub foreign_field: String,
    /// 该层下的条件（字段键为**去前缀**路径，如 `price` / `meta.level`）
    pub filter: Value,
}

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
    /// 子级过滤（已剥离嵌套关系路径；仅标量 / 对象点号路径 / 数组整值）；None = 无
    pub filter: Option<Value>,
    /// 子级过滤中的嵌套关系下钻（一层；8c-2）
    pub nested: Vec<NestedRelFilter>,
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
                check_filter_readable(registry, rel_schema, ctx, filter)?;
            }
            // 8c-2：嵌套关系下钻 —— 嵌套关系本身（R6）与其字段（F3）都要过读权限
            for n in &p.nested {
                if let Some(readable) = get_readable_relations(rel_schema, ctx) {
                    if !readable.contains(&n.rel_name) {
                        return Err(ERR_PERMISSION.to_string());
                    }
                }
                let nested_schema = registry.get(&n.model)?;
                check_filter_readable(registry, nested_schema, ctx, &n.filter)?;
            }
            for (_, def) in &p.agg {
                if let Some(field) = &def.field {
                    if !is_field_readable(rel_schema, ctx, field)
                        || !is_field_readable_overlay(registry, rel_schema, ctx, field)
                    {
                        return Err(ERR_PERMISSION.to_string());
                    }
                }
            }
            // RBAC 表级读判定（关系目标模型同样叠加，deny-wins）
            crate::rbac::ensure_read(registry, rel_schema, ctx)?;
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
    let profile = registry.profile();
    let (filter, mut nested, agg, having, extra_neg) = if is_main {
        parse_main(rel_schema, registry, rel_name, obj, profile)?
    } else {
        parse_simple(rel_schema, registry, rel_name, obj, profile)?
    };

    // 内层 `$lookup.as` 编号（单个关系谓词内局部唯一；SQL 侧据此前缀识别嵌套条件）
    for (i, n) in nested.iter_mut().enumerate() {
        n.as_name = format!("{}{}__{}", REL_NESTED_PREFIX, i, n.rel_name);
    }

    let as_name = format!("{}{}__{}", REL_PRED_PREFIX, preds.len(), rel_name);
    preds.push(RelPredicate {
        rel_name: rel_name.to_string(),
        as_name,
        model: rel_def.model.clone(),
        local_field: rel_def.local_field.clone(),
        foreign_field: rel_def.foreign_field.clone(),
        filter,
        nested,
        agg,
        having,
        negated: negated ^ extra_neg,
    });
    Ok(())
}

/// 关系聚合谓词解析结果：`(filter?, nested, agg, having, negated)`
type ParsedRelPredicate = Result<
    (
        Option<Value>,
        Vec<NestedRelFilter>,
        Vec<(String, AggDef)>,
        Value,
        bool,
    ),
    String,
>;

/// 主形式：`{ filter?, agg, having }`
fn parse_main(
    rel_schema: &Schema,
    registry: &Registry,
    rel_name: &str,
    obj: &Map<String, Value>,
    profile: Profile,
) -> ParsedRelPredicate {
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
    let mut nested: Vec<NestedRelFilter> = Vec::new();
    let filter = match non_nullish(obj.get("filter")).cloned() {
        Some(f) => non_empty_obj(parse_pred_filter(
            rel_schema,
            registry,
            rel_name,
            &f,
            profile,
            &mut nested,
            false,
        )?),
        None => None,
    };

    // agg：省略无法推导 having 引用的算子/字段 → Err（绝不臆测）
    let agg_v = non_nullish(obj.get("agg")).ok_or_else(|| {
        format!(
            "关系聚合谓词 \"{rel_name}\" 省略 agg 时无法推导 having 引用的聚合（请显式声明 agg）"
        )
    })?;
    let gs = group::parse(rel_schema, &json!({ "agg": agg_v }), registry)
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
    Ok((filter, nested, agg, having, false))
}

/// 简写形式：可选 `$filter` ＋ 恰好一个聚合谓词；或**整值条件对象**（阶段1）。
///
/// 整值条件对象：`{"<rel>": {"category": "meat"}}` —— spec 全部键都不带 `$` 前缀、
/// 也不是主形式键（filter/agg/having）时，视为 `$filter: {该对象}` + `$exists: true`
/// （semi-join 语义糖）。与 query / mutation 的「按关联表字段过滤」自然写法对齐；
/// 含任何 `$` 键时走原有解析（错误文案保持逐字节不变，守护既有 J 组负例）。
fn parse_simple(
    rel_schema: &Schema,
    registry: &Registry,
    rel_name: &str,
    obj: &Map<String, Value>,
    profile: Profile,
) -> ParsedRelPredicate {
    // ── 整值条件对象识别（必须在 $ 键检查之前）──
    let has_dollar = obj.keys().any(|k| k.starts_with('$'));
    let has_main = ["filter", "agg", "having"]
        .iter()
        .any(|k| obj.contains_key(*k));
    if !has_dollar && !has_main && !obj.is_empty() {
        let mut nested2: Vec<NestedRelFilter> = Vec::new();
        // 整对象即 filter：`{"category": "meat"}` → parse_pred_filter 校验字段可翻译性
        let cond = Value::Object(obj.clone());
        let f = parse_pred_filter(
            rel_schema,
            registry,
            rel_name,
            &cond,
            profile,
            &mut nested2,
            false,
        )?;
        let f = non_empty_obj(f).ok_or_else(|| {
            format!("关系聚合谓词 \"{rel_name}\" 的条件对象不能为空")
        })?;
        return Ok((
            Some(f),
            nested2,
            vec![(
                "n".to_string(),
                AggDef {
                    op: "$count".to_string(),
                    field: None,
                },
            )],
            cond_obj("n", "$gt", &json!(0)),
            false,
        ));
    }
    let mut filter: Option<Value> = None;
    let mut nested: Vec<NestedRelFilter> = Vec::new();
    let mut ops: Vec<(&String, &Value)> = Vec::new();
    for (k, v) in obj {
        if k == "$filter" {
            if !v.is_null() {
                filter = non_empty_obj(parse_pred_filter(
                    rel_schema,
                    registry,
                    rel_name,
                    v,
                    profile,
                    &mut nested,
                    false,
                )?);
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
                nested,
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
                nested,
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
                nested,
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
/// 逻辑组（`$and/$or/$nor`）递归；标量键过子模型 `field.read` ∧ RBAC readFields
/// （关系/数组/对象等形态已在 [`validate_scalar_filter`] 处拒绝）。越权 →
/// `Err(ERR_PERMISSION)`。
fn check_filter_readable(
    registry: &Registry,
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
                    check_filter_readable(registry, rel_schema, ctx, it)?;
                }
            }
            continue;
        }
        if k.starts_with('$') {
            continue;
        }
        if !is_field_readable(rel_schema, ctx, k)
            || !is_field_readable_overlay(registry, rel_schema, ctx, k)
        {
            return Err(ERR_PERMISSION.to_string());
        }
    }
    Ok(())
}

/// 子级过滤（`filter` / `$filter`）解析 + 校验 —— **按档分流**（执行文档 §4.1 #4）。
///
/// 返回**剥离嵌套关系路径后**的过滤（标量 / 对象点号路径 / 数组整值 / 逻辑组），
/// 并把命中的嵌套关系下钻收集进 `nested`（8c-2，仅一层）。
///
/// `standard` 档：数组字段整值（U1）、对象字段整值（U2）、对象点号路径（U3）放行
/// （SQL 侧已落 JSON 列并由 `dialect` 翻译，见 8b）；`text2query` 档维持显式 Err（功能收缩）。
/// 两档一律 Err：数组字段索引路径（`tags.0`，各后端索引语义不一致）、schema 外字段、
/// 操作符键（filter 仅接受字段条件）、**三级关系路径**、**`$or`/`$nor` 内的嵌套关系路径**
/// （Mongo 点号 ANY 无法与 SQL 的 OR-EXISTS 逐字节对齐 —— 绝不静默近似）。
fn parse_pred_filter(
    rel_schema: &Schema,
    registry: &Registry,
    rel_name: &str,
    filter: &Value,
    profile: Profile,
    nested: &mut Vec<NestedRelFilter>,
    in_or: bool,
) -> Result<Value, String> {
    let Value::Object(m) = filter else {
        return Err(format!("关系聚合谓词 \"{rel_name}\" 的 filter 必须是对象"));
    };
    let mut out = Map::new();
    for (k, v) in m {
        if matches!(k.as_str(), "$and" | "$or" | "$nor") {
            let arr = v
                .as_array()
                .ok_or_else(|| format!("关系聚合谓词 \"{rel_name}\" 的 filter 中 {k} 需要数组"))?;
            let child_or = in_or || matches!(k.as_str(), "$or" | "$nor");
            let mut na = Vec::with_capacity(arr.len());
            for it in arr {
                na.push(parse_pred_filter(
                    rel_schema, registry, rel_name, it, profile, nested, child_or,
                )?);
            }
            out.insert(k.clone(), Value::Array(na));
            continue;
        }
        if k.starts_with('$') {
            return Err(format!(
                "关系聚合谓词 \"{rel_name}\" 的 filter 不支持操作符 \"{k}\"（仅字段条件）"
            ));
        }
        let root = k.split('.').next().unwrap_or(k);
        let Some(rel_def) = rel_schema.relations.get(root) else {
            validate_pred_field(rel_schema, rel_name, k, profile)?;
            out.insert(k.clone(), v.clone());
            continue;
        };
        // ── 嵌套关系下钻（8c-2）：仅支持 `关系.字段`，仅一层，且不得出现在 `$or`/`$nor` 内 ──
        if profile == Profile::Text2Query {
            // 命中即 Err（guard 已保证 profile），map 仅对齐本函数的 `Result<Value, _>` 签名
            return forbid_t2q_shape(
                profile,
                "8c-2 嵌套关系路径",
                format!("关系聚合谓词 \"{rel_name}\" 字段 \"{k}\" 是嵌套关系路径（8c-2）"),
            )
            .map(|()| Value::Null);
        }
        if in_or {
            return Err(format!(
                "关系聚合谓词 \"{rel_name}\" 字段 \"{k}\" 是嵌套关系路径（不支持出现在 $or/$nor 内：\
                 两端下推语义无法逐字节对齐）"
            ));
        }
        if root == k {
            return Err(format!(
                "关系聚合谓词 \"{rel_name}\" 字段 \"{k}\" 是关系引用\
                 （须写成 关系名.字段 形式，如 \"{root}.<字段>\"）"
            ));
        }
        let rest = &k[root.len() + 1..];
        let nested_schema = registry.get(&rel_def.model)?;
        let rest_root = rest.split('.').next().unwrap_or(rest);
        if nested_schema.relations.contains_key(rest_root) {
            return Err(format!(
                "关系聚合谓词 \"{rel_name}\" 字段 \"{k}\" 是三级关系路径（本轮仅支持一层嵌套关系下钻）"
            ));
        }
        validate_pred_field(nested_schema, rel_name, rest, profile)?;
        let mut cm = Map::new();
        cm.insert(rest.to_string(), v.clone());
        let cond = Value::Object(cm);
        match nested.iter().position(|n| n.rel_name == root) {
            Some(i) => nested[i].filter = merge_and(&nested[i].filter, &cond),
            None => nested.push(NestedRelFilter {
                rel_name: root.to_string(),
                as_name: String::new(), // 由 build_pred 统一编号
                model: rel_def.model.clone(),
                local_field: rel_def.local_field.clone(),
                foreign_field: rel_def.foreign_field.clone(),
                filter: cond,
            }),
        }
    }
    Ok(Value::Object(out))
}

/// 剥离嵌套关系路径后可能得到空对象（filter 全为嵌套关系下钻）→ `None`，
/// 避免在 Mongo `$match` / SQL 子查询 WHERE 里塞入无意义的空条件。
fn non_empty_obj(v: Value) -> Option<Value> {
    if v.as_object().map(|m| m.is_empty()).unwrap_or(false) {
        None
    } else {
        Some(v)
    }
}

/// 合并两个过滤条件为 `$and`（展平已有顶层 `$and`，避免层层嵌套）
///
/// SQL 侧 [`crate::dialect::select::relation_agg`] 剥离嵌套关系前缀键时复用同一合并口径。
pub(crate) fn merge_and(a: &Value, b: &Value) -> Value {
    let mut parts: Vec<Value> = Vec::new();
    let flattened = a
        .as_object()
        .filter(|m| m.len() == 1)
        .and_then(|m| m.get("$and"))
        .and_then(|v| v.as_array());
    match flattened {
        Some(arr) => parts.extend(arr.iter().cloned()),
        None => parts.push(a.clone()),
    }
    parts.push(b.clone());
    json!({ "$and": parts })
}

/// 给过滤条件的字段键加前缀（嵌套关系下钻 → `__rn{i}__.`；逻辑组递归）
fn prefix_filter(v: &Value, prefix: &str) -> Result<Value, String> {
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, val) in m {
                if matches!(k.as_str(), "$and" | "$or" | "$nor") {
                    let arr = val
                        .as_array()
                        .ok_or_else(|| format!("嵌套关系过滤中 {k} 需要数组"))?;
                    let mut na = Vec::with_capacity(arr.len());
                    for it in arr {
                        na.push(prefix_filter(it, prefix)?);
                    }
                    out.insert(k.clone(), Value::Array(na));
                } else if k.starts_with('$') {
                    out.insert(k.clone(), val.clone());
                } else {
                    out.insert(format!("{}{}", prefix, k), val.clone());
                }
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

/// 过滤键的字段形态校验（判定与 `types::validate_condition_shape` 同构，文案带关系名前缀）
fn validate_pred_field(
    rel_schema: &Schema,
    rel_name: &str,
    field: &str,
    profile: Profile,
) -> Result<(), String> {
    let root = field.split('.').next().unwrap_or(field);
    if rel_schema.relations.contains_key(root) {
        return Err(format!(
            "关系聚合谓词 \"{rel_name}\" 字段 \"{field}\" 引用了关系 \"{root}\"\
             （一级关系谓词内不支持嵌套关系下钻）"
        ));
    }
    let Some(fd) = rel_schema.fields.get(root) else {
        return Err(format!(
            "关系聚合谓词 \"{rel_name}\" 引用了 schema 外字段 \"{field}\""
        ));
    };
    let dotted = field.contains('.');
    match (fd.field_type.as_str(), dotted) {
        // 数组字段索引路径（`tags.0`）：各后端数组索引语义不一致 → 两档一律 Err
        ("array", true) => Err(format!(
            "关系聚合谓词 \"{rel_name}\" 字段 \"{field}\" 是数组字段索引路径\
             （各后端数组索引语义不一致；请改用整值过滤或对象点号路径）"
        )),
        ("array", false) if profile == Profile::Text2Query => forbid_t2q_shape(
            profile,
            "U1 数组字段条件",
            format!("关系聚合谓词 \"{rel_name}\" 字段 \"{field}\" 是数组字段（U1/D2）"),
        ),
        ("object", false) if profile == Profile::Text2Query => forbid_t2q_shape(
            profile,
            "U2 对象字段条件",
            format!("关系聚合谓词 \"{rel_name}\" 字段 \"{field}\" 是对象字段（U2/D2）"),
        ),
        ("object", true) if profile == Profile::Text2Query => forbid_t2q_shape(
            profile,
            "U3 对象点号路径条件",
            format!("关系聚合谓词 \"{rel_name}\" 字段 \"{field}\" 是对象点号路径（U3/D2）"),
        ),
        _ => Ok(()),
    }
}

/// 聚合字段（`$of`）校验（子 schema）：必须标量 —— 关系 / 数组 / 对象 / 点号路径 /
/// schema 外一律 Err。与档位无关（聚合字段不是过滤条件，数组/对象做 `$sum`/`$avg`
/// 既无 Mongo 侧稳定语义、也无法映射为 SQL 标量列）。
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
    // 8c-2：嵌套关系下钻 —— 内层 `$lookup`（须先于本层 `$match`）+ 条件的字段键加前缀
    let mut nested_stages: Vec<Value> = Vec::new();
    for n in &p.nested {
        let n_schema = registry.get(&n.model)?;
        let n_is_array = is_array_local_field(rel_schema, &n.local_field);
        let n_let = format!("nrel_{}", n.local_field);
        let mut n_match: Vec<Value> = vec![rel_match_expr(&n.foreign_field, &n_let, n_is_array)];
        // 孙行越权防护（F3：与子行 owner 注入同源）+ RBAC 行条件叠加
        if let Some(owner) = merge_owner_condition(n_schema, ctx, None) {
            n_match.push(owner);
        }
        if let Some(rbac_cond) = row_condition(registry, n_schema, ctx, "read") {
            n_match.push(rbac_cond);
        }
        let n_match_doc = if n_match.len() == 1 {
            n_match.remove(0)
        } else {
            json!({ "$and": n_match })
        };
        let mut n_let_map = Map::new();
        n_let_map.insert(n_let, rel_let_expr(&n.local_field, n_is_array));
        let mut n_inner = Map::new();
        n_inner.insert(
            "from".to_string(),
            Value::String(n_schema.collection.clone()),
        );
        n_inner.insert("let".to_string(), Value::Object(n_let_map));
        n_inner.insert("pipeline".to_string(), json!([{ "$match": n_match_doc }]));
        n_inner.insert("as".to_string(), Value::String(n.as_name.clone()));
        nested_stages.push(json!({ "$lookup": Value::Object(n_inner) }));
        // 嵌套条件去前缀路径 → `__rn{i}__.xxx` 点号路径（数组 ANY，与 SQL 嵌套 EXISTS 同语义）
        ands.push(prefix_filter(&n.filter, &format!("{}.", n.as_name))?);
    }
    if let Some(owner) = merge_owner_condition(rel_schema, ctx, None) {
        ands.push(owner);
    }
    // RBAC 行条件叠加（两引擎同层 $and，deny-wins）
    if let Some(rbac_cond) = row_condition(registry, rel_schema, ctx, "read") {
        ands.push(rbac_cond);
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

    // 子 pipeline：内层 `$lookup`（嵌套关系）→ `$match`（外键 + filter + nested + owner）
    // → `$group` → `$match`(having)
    let mut stages: Vec<Value> = nested_stages;
    stages.push(json!({ "$match": match_doc }));
    stages.push(json!({ "$group": Value::Object(group_map) }));
    stages.push(json!({ "$match": p.having.clone() }));

    let mut inner = Map::new();
    inner.insert(
        "from".to_string(),
        Value::String(rel_schema.collection.clone()),
    );
    inner.insert("let".to_string(), Value::Object(let_map));
    inner.insert("pipeline".to_string(), Value::Array(stages));
    inner.insert("as".to_string(), Value::String(p.as_name.clone()));
    Ok(json!({ "$lookup": Value::Object(inner) }))
}

/// 从 `as` 名解析关系名（`__rp{序号}__{关系名}`）
pub fn rel_name_from_as(as_name: &str) -> Option<&str> {
    let rest = as_name.strip_prefix(REL_PRED_PREFIX)?;
    let (_, rel) = rest.split_once("__")?;
    Some(rel)
}

/// 从嵌套关系 `as` 名解析关系名（`__rn{序号}__{关系名}`，关系名在**子 schema** 上）
pub fn nested_rel_name_from_as(as_name: &str) -> Option<&str> {
    let rest = as_name.strip_prefix(REL_NESTED_PREFIX)?;
    let (_, rel) = rest.split_once("__")?;
    Some(rel)
}
