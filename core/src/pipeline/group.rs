//! 根级 `$group` / `$having`（§9.2(1) 成组聚合，P5 批次一）
//!
//! GQL 入口：`Course($condition:@c0, $group:@g0, $having:@h0, $sort:@s0, $skip:@sk, $limit:@l0){ … }`
//! `$group` 规格：`{ "by": ["status","meta.level"], "agg": { "n": {"$count":"*"}, "total": {"$sum":"price"} } }`
//!
//! 固定执行序（§9.3）：`$condition`(WHERE) → `$group`(GROUP BY) → `$having`(HAVING)
//! → `$sort` → `$skip/$limit` → 投影。有 `$group` 时排序/分页作用于**分组结果**，
//! `$sort` 键域 = `by` 键 ∪ `agg` 别名。

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::command::{forbid_t2q, ERR_PERMISSION};
use crate::permission::{is_field_readable, Context};
use crate::schema::{Registry, Schema};
use crate::types::{validate_condition, AGG_OPS};

use super::util::{append_order, collect_having_agg_refs, non_nullish};

/// 单个聚合：算子 + 字段（`$count:"*"` 时 `field = None` 表示行数）
#[derive(Debug, Clone)]
pub struct AggDef {
    pub op: String,
    pub field: Option<String>,
}

/// 解析后的 `$group` 规格
#[derive(Debug, Clone)]
pub struct GroupSpec {
    /// 分组键（标量域，可含 object 点号路径）
    pub by: Vec<String>,
    /// 输出别名 → 聚合定义（保持声明顺序）
    pub agg: Vec<(String, AggDef)>,
}

impl GroupSpec {
    fn is_by(&self, name: &str) -> bool {
        self.by.iter().any(|k| k == name)
    }

    fn agg_def(&self, name: &str) -> Option<&AggDef> {
        self.agg.iter().find(|(k, _)| k == name).map(|(_, d)| d)
    }
}

/// `by` 键校验：仅标量域（含 object 点号路径）；关系 / 数组 / 裸对象 / schema 外字段 → Err。
///
/// **关系路径 by 键（阶段1 T1-03）**：`by: ["product.category"]`（`product` 为 **one 关系**
/// 且叶子是目标表标量字段）→ 合法，Mongo 侧由调用方发射 `$lookup` + `$unwind`（one 不扇出，
/// 行数与计数语义不变）；**many 关系路径扇出行数会破坏 `$count:*` 等聚合语义 → 显式 Err**。
///
/// **按档分流**：`by` 键里的 object 点号路径（如 `meta.level`）是
/// ⚠️ DB 独有能力（仅 MongoDB）：不建议用于业务查询 ——
///    会带来跨方言维护的特殊化处理；仅适合数据迁移 / 功能脚本。
///    准入：standard 档放行（Mongo 源执行；SQL 源由 dialect 显式报「无法映射到本表标量列」）；
///    text2query 档禁用（forbid_t2q）。
fn validate_by_key(schema: &Schema, key: &str, registry: &Registry) -> Result<(), String> {
    if schema.relations.contains_key(key) {
        return Err(format!(
            "$group 的 by 键 \"{key}\" 是关系名（分组键仅支持标量域，不支持关系字段）"
        ));
    }
    // 关系路径（`product.category`）：head 命中关系名 → 校验 one 关系 + 叶子标量
    if let Some((head, rest)) = key.split_once('.') {
        if let Some(rel) = schema.relations.get(head) {
            if rel.rel_type != "one" {
                return Err(format!(
                    "$group 的 by 键 \"{key}\" 走 many 关系 \"{}\"（会扇出行数破坏聚合语义；仅支持 one 关系路径）",
                    head
                ));
            }
            let root = rest.split('.').next().unwrap_or(rest);
            if !schema_fields_of(registry, &rel.model).contains(&root.to_string()) {
                return Err(format!(
                    "$group 的 by 键 \"{key}\" 引用了关系 \"{}\" 的 schema 外字段 \"{root}\"",
                    head
                ));
            }
            return Ok(());
        }
    }
    let root = key.split('.').next().unwrap_or(key);
    let Some(fd) = schema.fields.get(root) else {
        return Err(format!(
            "$group 的 by 键 \"{key}\" 引用了 schema 外字段 \"{root}\""
        ));
    };
    match fd.field_type.as_str() {
        "array" => Err(format!(
            "$group 的 by 键 \"{key}\" 是数组字段（分组键仅支持标量域）"
        )),
        // object 点号路径：standard 档放行（Mongo 原生下钻）、text2query 档功能收缩（见上方注释块）
        "object" if key.contains('.') => forbid_t2q(registry, "$group.by object 点号路径"),
        "object" => Err(format!(
            "$group 的 by 键 \"{key}\" 是对象字段（须用点号路径指明子字段，如 \"{key}.<子字段>\")"
        )),
        _ => Ok(()),
    }
}

/// 目标 schema 的字段名集（relation 路径 by 键的叶子校验用；未知 model → 空集 → Err）
fn schema_fields_of(registry: &Registry, model: &str) -> Vec<String> {
    registry
        .get(model)
        .map(|s| s.fields.keys().cloned().collect())
        .unwrap_or_default()
}

/// `agg` 字段校验：仅本表标量字段；关系 / 数组 / 对象 / 点号路径 / schema 外字段 → Err
fn validate_agg_field(schema: &Schema, op: &str, alias: &str, field: &str) -> Result<(), String> {
    if schema.relations.contains_key(field) {
        return Err(format!(
            "$group 的 agg \"{alias}\"（{op}）引用了关系 \"{field}\"（仅支持标量字段）"
        ));
    }
    if field.contains('.') {
        return Err(format!(
            "$group 的 agg \"{alias}\"（{op}）字段 \"{field}\" 不可为点号路径（仅支持标量字段）"
        ));
    }
    match schema.fields.get(field) {
        None => Err(format!(
            "$group 的 agg \"{alias}\"（{op}）引用了 schema 外字段 \"{field}\""
        )),
        Some(fd) if fd.field_type == "object" || fd.field_type == "array" => Err(format!(
            "$group 的 agg \"{alias}\"（{op}）字段 \"{field}\" 必须是标量字段"
        )),
        Some(_) => Ok(()),
    }
}

/// 解析并校验 `$group` 规格（`registry` 供 `by` 键档位分流，见 [`validate_by_key`]）
pub fn parse(schema: &Schema, group_v: &Value, registry: &Registry) -> Result<GroupSpec, String> {
    let obj = group_v
        .as_object()
        .ok_or("$group 参数必须是对象 { by, agg }")?;

    let mut by: Vec<String> = Vec::new();
    match obj.get("by") {
        None | Some(Value::Null) => {}
        Some(Value::Array(arr)) => {
            for v in arr {
                let key = v.as_str().ok_or("$group.by 的元素必须是字段名字符串")?;
                validate_by_key(schema, key, registry)?;
                if by.iter().any(|k| k == key) {
                    return Err(format!("$group.by 存在重复键 \"{key}\""));
                }
                by.push(key.to_string());
            }
        }
        Some(_) => return Err("$group.by 必须是字段名数组（省略 / [] = 全表单组）".to_string()),
    }

    let mut agg: Vec<(String, AggDef)> = Vec::new();
    if let Some(v) = obj.get("agg") {
        if !v.is_null() {
            let m = v
                .as_object()
                .ok_or("$group.agg 必须是对象 { 别名: { 算子: 字段 } }")?;
            for (alias, def) in m {
                if alias.is_empty() {
                    return Err("$group.agg 的别名不能为空".to_string());
                }
                if by.iter().any(|k| k == alias) {
                    return Err(format!("$group.agg 别名 \"{alias}\" 与 by 键冲突"));
                }
                let d = def
                    .as_object()
                    .ok_or_else(|| format!("$group.agg \"{alias}\" 的定义必须是对象"))?;
                let mut it = d.iter();
                let (op, arg) = match (it.next(), it.next()) {
                    (Some(kv), None) => kv,
                    _ => return Err(format!("$group.agg \"{alias}\" 必须恰有一个算子键")),
                };
                if !AGG_OPS.contains(&op.as_str()) {
                    return Err(format!(
                        "$group.agg \"{alias}\" 的算子 {op} 不在白名单（$count/$sum/$avg/$min/$max）"
                    ));
                }
                let field = if op == "$count" && arg.as_str() == Some("*") {
                    None
                } else {
                    let f = arg.as_str().ok_or_else(|| {
                        format!(
                            "$group.agg \"{alias}\"（{op}）必须带标量字段名（$count 可用 \"*\"）"
                        )
                    })?;
                    validate_agg_field(schema, op, alias, f)?;
                    Some(f.to_string())
                };
                agg.push((
                    alias.clone(),
                    AggDef {
                        op: op.clone(),
                        field,
                    },
                ));
            }
        }
    }

    if by.is_empty() && agg.is_empty() {
        return Err("$group 至少需要 by 键或 agg 别名之一".to_string());
    }
    Ok(GroupSpec { by, agg })
}

/// F2：`$group` 的 `by` 键 / `agg` 引用字段必须过所属 schema 的 `field.read` ∧
/// RBAC readFields（deny-wins）。
///
/// `$having` 仅可引用 `by` 键 / `agg` 别名（[`validate_having`]），其背后的引用字段即
/// `by` 键与 `agg` 字段本身，故由本函数一并覆盖。越权 → `Err(ERR_PERMISSION)`；
/// `ctx = None` 放行（fail-open）。
pub fn validate_read_permission(
    registry: &Registry,
    schema: &Schema,
    ctx: Option<&Context>,
    spec: &GroupSpec,
) -> Result<(), String> {
    if ctx.is_none() {
        return Ok(());
    }
    for key in &spec.by {
        if !is_field_readable(schema, ctx, key)
            || !crate::rbac::is_field_readable_overlay(registry, schema, ctx, key)
        {
            return Err(ERR_PERMISSION.to_string());
        }
    }
    for (_, def) in &spec.agg {
        if let Some(field) = &def.field {
            if !is_field_readable(schema, ctx, field)
                || !crate::rbac::is_field_readable_overlay(registry, schema, ctx, field)
            {
                return Err(ERR_PERMISSION.to_string());
            }
        }
    }
    Ok(())
}

/// `$having` 校验：仅可引用 `by` 键 / `agg` 别名；其余字段 → Err
pub fn validate_having(spec: &GroupSpec, having: &Value) -> Result<(), String> {
    fn walk(spec: &GroupSpec, v: &Value) -> Result<(), String> {
        let Some(m) = v.as_object() else {
            return Ok(());
        };
        for (k, val) in m {
            match k.as_str() {
                "$and" | "$or" | "$nor" => {
                    let arr = val
                        .as_array()
                        .ok_or_else(|| format!("$having 的 {k} 需要数组"))?;
                    for it in arr {
                        walk(spec, it)?;
                    }
                }
                _ if k.starts_with('$') => {
                    return Err(format!(
                        "$having 不支持操作符 {k}（仅可引用 by 键 / agg 别名，支持 $and/$or/$nor）"
                    ));
                }
                _ => {
                    if !spec.is_by(k) && spec.agg_def(k).is_none() {
                        return Err(format!(
                            "$having 引用了未声明的字段 \"{k}\"（仅可引用 by 键 / agg 别名）"
                        ));
                    }
                }
            }
        }
        Ok(())
    }
    if !having.is_object() {
        return Err("$having 必须是条件对象".to_string());
    }
    walk(spec, having)
}

/// 构建根级 `$group` 聚合控制（Mongo 阶段数组，含末段投影）
///
/// 阶段1 T1-03：by 键含 **one 关系路径**（`product.category`）时，返回
/// `lookup_stages`（`$lookup` + `$unwind`，one 不扇出行数不变），调用方置于
/// `$match` / `$group` 之前；by 键在 `$group._id` 与投影里按 `"<rel>.<field>"` 原样
/// 引用（`$lookup` 已把关联文档并入该键名下）。
// 聚合阶段构建需 spec / 字段 / 条件 / having / 排序 / 分页等完整上下文，拆结构体反而增加跨模块传递成本
#[allow(clippy::too_many_arguments)]
pub fn build_stages(
    spec: &GroupSpec,
    fields: &[String],
    relations_empty: bool,
    condition: Option<&Value>,
    having: Option<&Value>,
    sort: Option<&Value>,
    skip: Option<&Value>,
    limit: Option<&Value>,
    schema: &Schema,
    registry: &Registry,
) -> Result<Vec<Value>, String> {
    if !relations_empty {
        return Err(
            "$group 查询不支持关系字段（选择集仅可为 by 键 / agg 别名，无关系下钻）".to_string(),
        );
    }
    if fields.is_empty() {
        return Err("$group 查询必须显式选择输出字段（by 键 / agg 别名）".to_string());
    }
    // 选择集 ⊆ by ∪ agg（引用未声明别名 → Err，不静默补空）
    for f in fields {
        if !spec.is_by(f) && spec.agg_def(f).is_none() {
            return Err(format!(
                "选择集字段 \"{f}\" 未在 $group 的 by 键 / agg 别名中声明"
            ));
        }
    }
    if let Some(h) = non_nullish(having) {
        validate_condition(h)?;
        validate_having(spec, h)?;
    }
    // `$sort` 键域 = by 键 ∪ agg 别名
    if let Some(s) = non_nullish(sort) {
        let m = s.as_object().ok_or("$sort 必须是对象")?;
        for k in m.keys() {
            if !spec.is_by(k) && spec.agg_def(k).is_none() {
                return Err(format!(
                    "$sort 键 \"{k}\" 不在 $group 的 by 键 / agg 别名域内"
                ));
            }
        }
    }

    // 按需计算：选择集未请求的 agg 不下发计算（having 引用到的除外，否则无法求值）
    let mut refs: HashSet<String> = HashSet::new();
    if let Some(h) = non_nullish(having) {
        collect_having_agg_refs(h, &|k| spec.agg_def(k).is_some(), &mut refs);
    }
    let needed: Vec<&str> = spec
        .agg
        .iter()
        .filter(|(alias, _)| fields.iter().any(|f| f == alias) || refs.contains(alias))
        .map(|(alias, _)| alias.as_str())
        .collect();

    let mut stages: Vec<Value> = Vec::new();

    // one 关系路径 by 键（阶段1 T1-03）：`$lookup` + `$unwind`（one 不扇出，行数不变），
    // 置于 `$match` 之前 —— `$unwind` 后 by 键 `"<rel>.<field>"` 成为普通点号路径引用。
    // preserveNullAndEmptyArrays：无匹配行保留（by 键为 null 组，对齐 SQL LEFT JOIN 语义）。
    let mut rel_by_keys: Vec<(String, &str, String, String)> = Vec::new(); // (by键, rel名, local, foreign)
    for key in &spec.by {
        if let Some((head, rest)) = key.split_once('.') {
            if let Some(rel) = schema.relations.get(head) {
                rel_by_keys.push((
                    key.clone(),
                    head,
                    rel.local_field.clone(),
                    rel.foreign_field.clone(),
                ));
                let _ = rest;
            }
        }
    }
    if !rel_by_keys.is_empty() {
        let mut seen: Vec<&str> = Vec::new();
        for (key, head, local, foreign) in &rel_by_keys {
            if seen.contains(head) {
                continue;
            }
            seen.push(head);
            let rel = &schema.relations[*head];
            let rel_schema = registry.get(&rel.model)?;
            let mut let_map = Map::new();
            let_map.insert(
                format!("g_{}", head),
                json!(format!("${}", local)),
            );
            let mut inner = Map::new();
            inner.insert("from".to_string(), json!(rel_schema.collection));
            inner.insert("let".to_string(), Value::Object(let_map));
            // 外键匹配：one 关系 local 是标量，`$$g_<head>` 直接等值
            inner.insert(
                "pipeline".to_string(),
                json!([{ "$match": { "$expr": { "$eq": [
                    format!("${}", foreign),
                    format!("$$g_{}", head)
                ] } } }]),
            );
            inner.insert("as".to_string(), json!(head));
            stages.push(json!({ "$lookup": Value::Object(inner) }));
            stages.push(json!({
                "$unwind": {
                    "path": format!("${}", head),
                    "preserveNullAndEmptyArrays": true
                }
            }));
            let _ = key;
        }
    }

    if let Some(cond) = non_nullish(condition) {
        validate_condition(cond)?;
        stages.push(json!({ "$match": cond }));
    }

    // $group：`_id` = by 键组合（单键用该键、多键用嵌套对象、省略 = null）
    let mut group_map = Map::new();
    group_map.insert("_id".to_string(), id_expr(&spec.by));
    for (alias, def) in &spec.agg {
        if !needed.contains(&alias.as_str()) {
            continue;
        }
        group_map.insert(alias.clone(), acc_expr(def));
    }
    stages.push(json!({ "$group": Value::Object(group_map) }));

    // 全表单组空输入护栏（§9.7）：Mongo `$group` 空输入 0 行，SQL 无 GROUP BY 返回 1 行
    // → 用 `$facet`（恒 1 行）+ `$replaceRoot` 缺省行对齐到「1 行」。护栏在 `$having` 之前。
    if spec.by.is_empty() {
        let mut def_row = Map::new();
        def_row.insert("_id".to_string(), Value::Null);
        for (alias, def) in &spec.agg {
            if !needed.contains(&alias.as_str()) {
                continue;
            }
            def_row.insert(
                alias.clone(),
                if def.op == "$count" {
                    json!(0)
                } else {
                    Value::Null
                },
            );
        }
        stages.push(json!({ "$facet": { "__rows": [] } }));
        stages.push(json!({
            "$replaceRoot": { "newRoot": { "$ifNull": [
                { "$arrayElemAt": ["$__rows", 0] },
                Value::Object(def_row)
            ] } }
        }));
    }

    // $having → `$group` 之后的 `$match`
    if let Some(h) = non_nullish(having) {
        stages.push(json!({ "$match": rewrite_having(spec, h) }));
    }

    // 分组后 `$sort`：by 键在 `$group` 后已改名为 `_id` / `_id.<key>` → 须同步重写，
    // 否则 `$sort` 作用在不存在的字段上（Mongo 视为 no-op）→ 与 SQL 侧排序不一致。
    let order_src = non_nullish(sort).map(|s| rewrite_order(spec, s));
    append_order(&mut stages, order_src.as_ref(), skip, limit);

    // 投影：by 键（点号路径还原嵌套对象）+ 被请求的 agg 别名；不输出 `_id`（除非在 by 中）
    stages.push(json!({ "$project": projection(spec, fields) }));

    Ok(stages)
}

/// `$group._id` 表达式
fn id_expr(by: &[String]) -> Value {
    match by.len() {
        0 => Value::Null,
        1 => json!(format!("${}", by[0])),
        _ => {
            let mut root = Map::new();
            for key in by {
                let parts: Vec<&str> = key.split('.').collect();
                insert_nested(&mut root, &parts, json!(format!("${}", key)));
            }
            Value::Object(root)
        }
    }
}

/// 把叶子值写入嵌套对象（点号路径还原为嵌套结构）
fn insert_nested(obj: &mut Map<String, Value>, parts: &[&str], val: Value) {
    if parts.len() == 1 {
        obj.insert(parts[0].to_string(), val);
        return;
    }
    let entry = obj.entry(parts[0].to_string()).or_insert_with(|| json!({}));
    if let Value::Object(m) = entry {
        insert_nested(m, &parts[1..], val);
    }
}

/// 累积器表达式
pub(crate) fn acc_expr(def: &AggDef) -> Value {
    let field_ref = format!("${}", def.field.clone().unwrap_or_default());
    match def.op.as_str() {
        // 行数 / 字段非空计数（非空计数含 0、false、""，故用 $ifNull 判定缺失/null）
        "$count" => match &def.field {
            None => json!({ "$sum": 1 }),
            Some(_) => json!({
                "$sum": { "$cond": [
                    { "$eq": [ { "$ifNull": [field_ref, null] }, null ] },
                    0,
                    1
                ] }
            }),
        },
        "$sum" => json!({ "$sum": field_ref }),
        "$avg" => json!({ "$avg": field_ref }),
        "$min" => json!({ "$min": field_ref }),
        _ => json!({ "$max": field_ref }),
    }
}

/// 末段投影：by 键（点号路径 → 嵌套还原）+ 被请求的 agg 别名
fn projection(spec: &GroupSpec, fields: &[String]) -> Value {
    let mut proj = Map::new();
    let requested_by: Vec<&String> = spec
        .by
        .iter()
        .filter(|k| fields.iter().any(|f| f == *k))
        .collect();
    let id_in_by = requested_by.iter().any(|k| k.as_str() == "_id");
    if !id_in_by {
        proj.insert("_id".to_string(), json!(0));
    }
    for key in &requested_by {
        let expr = if key.as_str() == "_id" {
            if spec.by.len() == 1 {
                json!(1)
            } else {
                json!("$_id._id")
            }
        } else if spec.by.len() == 1 {
            json!("$_id")
        } else {
            json!(format!("$_id.{}", key))
        };
        proj.insert((*key).clone(), expr);
    }
    for (alias, _) in &spec.agg {
        if fields.iter().any(|f| f == alias) {
            proj.insert(alias.clone(), json!(1));
        }
    }
    Value::Object(proj)
}

/// `$having` 重写：by 键 → 分组 `_id` 路径；agg 别名保持顶层
fn rewrite_having(spec: &GroupSpec, h: &Value) -> Value {
    match h {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, v) in m {
                if matches!(k.as_str(), "$and" | "$or" | "$nor") {
                    let arr = v
                        .as_array()
                        .map(|a| a.iter().map(|it| rewrite_having(spec, it)).collect())
                        .unwrap_or_default();
                    out.insert(k.clone(), Value::Array(arr));
                } else if spec.is_by(k) {
                    let nk = if spec.by.len() == 1 {
                        "_id".to_string()
                    } else {
                        format!("_id.{}", k)
                    };
                    out.insert(nk, v.clone());
                } else {
                    out.insert(k.clone(), v.clone());
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// 分组后 `$sort` 重写：by 键 → 分组 `_id` 路径（与 [`rewrite_having`] 同一映射规则）；
/// agg 别名保持顶层。
fn rewrite_order(spec: &GroupSpec, s: &Value) -> Value {
    let Some(m) = s.as_object() else {
        return s.clone();
    };
    let mut out = Map::new();
    for (k, v) in m {
        if spec.is_by(k) {
            let nk = if spec.by.len() == 1 {
                "_id".to_string()
            } else {
                format!("_id.{}", k)
            };
            out.insert(nk, v.clone());
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}
