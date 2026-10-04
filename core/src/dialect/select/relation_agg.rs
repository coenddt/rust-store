//! §9.6 关系聚合谓词（跨表条件过滤 / semi-join）的 SQL 下推（§10.5）
//!
//! core 产出的 Mongo 阶段：每个关系谓词一个 `$lookup`（`as` = `__rp…`，子 pipeline 内
//! `$match` + `$group` + `$match`）+ 外层 `$match` 的代理键 `{ "<as>.0": { "$exists": bool } }`。
//!
//! SQL 翻译：代理键 → `EXISTS (SELECT 1 FROM 子表 c WHERE c.fk = t.local [AND filter]
//! GROUP BY c.fk HAVING <谓词>)`；`$exists:false` → `NOT EXISTS`。**不扇出**（父行形状不变）。
//!
//! 8c-2（§4.1 #4 多级下推）：子级 filter 内一层嵌套关系下钻（如 `items.price`）时，Mongo 在
//! 子 pipeline 前置内层 `$lookup`（`as` = `__rn…`）并把嵌套条件的字段键加 `__rn….` 前缀并入
//! 本层 `$match`（点号路径 = 数组 ANY）。SQL 侧对称翻译为**嵌套 `EXISTS`**（`n{i}` 别名，
//! 相关子查询挂在 `c` 上），并把前缀键剥离还原为孙表条件 —— 两端同语义，绝不静默近似。

use serde_json::{json, Map, Value};

use crate::dialect::filter::{build_filter, build_filter_raw, Warnings, WhereClause};
use crate::dialect::{scalar_column, Backend, ColumnRef};
use crate::pipeline::{merge_and, nested_rel_name_from_as, rel_name_from_as};
use crate::schema::{Registry, Schema};

use super::aggregate::lookup_extra_condition;
use super::{col_fn, count_field_pattern, tname};

/// 解析后的嵌套关系下钻（SQL 侧）：子级 filter 内一层关系下钻 → 嵌套 `EXISTS`
pub(super) struct SqlNested {
    /// 内层 `$lookup.as`（`__rn{序号}__{关系名}`，外层 `$match` 中作字段键前缀）
    pub as_name: String,
    /// 嵌套关系名（在**子 schema** 上）
    pub rel_name: String,
    /// 嵌套关系目标 model（孙表）
    pub model: String,
    /// 嵌套关系本地键（**子表** `c` 上的标量列）
    pub local_field: String,
    /// 嵌套关系外键（孙表标量列）
    pub foreign_field: String,
    /// 内层 `$lookup` 子 pipeline `$match` 的附加条件（孙表 owner 注入）
    pub extra: Option<Value>,
    /// 从本层 extra 剥离出的嵌套条件（键为**去前缀**路径，如 `price` / `meta.level`）
    pub filter: Option<Value>,
}

/// 解析后的关系聚合谓词（SQL 侧）
pub(super) struct SqlRelPredicate {
    /// `$lookup.as`（代理键前缀）
    pub as_name: String,
    /// 关系名
    pub rel_name: String,
    /// 目标 model 名
    pub model: String,
    /// 关系本地键（根表标量列）
    pub local_field: String,
    /// 关系外键（子表标量列）
    pub foreign_field: String,
    /// 子级 filter（已剥离嵌套关系路径）+ 目标 owner 注入（`$lookup` 内层 `$match` 的附加条件）
    pub extra: Option<Value>,
    /// 嵌套关系下钻（一层，8c-2）
    pub nested: Vec<SqlNested>,
    /// 谓词条件（引用聚合别名）
    pub having: Value,
    /// 聚合别名 → SQL 聚合表达式（含 `c.` 前缀）
    pub aggs: Vec<(String, String)>,
}

/// 识别并解析关系聚合谓词的 `$lookup` 阶段
pub(super) fn parse(
    lo: &Value,
    root_schema: &Schema,
    registry: &Registry,
    backend: Backend,
) -> Result<SqlRelPredicate, String> {
    let as_name = lo
        .get("as")
        .and_then(|v| v.as_str())
        .ok_or("关系聚合谓词 $lookup 缺少 as")?
        .to_string();
    let rel_name = rel_name_from_as(&as_name)
        .ok_or_else(|| format!("关系聚合谓词 $lookup.as \"{as_name}\" 形态非法"))?
        .to_string();
    let rel_def = root_schema.relations.get(&rel_name).ok_or_else(|| {
        format!(
            "关系聚合谓词引用的关系 \"{rel_name}\" 未在 schema \"{}\" 中定义",
            root_schema.name
        )
    })?;
    let rel_schema = registry.get(&rel_def.model)?;
    let pipeline: Vec<Value> = lo
        .get("pipeline")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // `$group`（聚合别名）与紧随其后的 `$match`（having）
    let group_idx = pipeline
        .iter()
        .position(|s| s.get("$group").is_some())
        .ok_or("关系聚合谓词 $lookup 子 pipeline 缺少 $group")?;

    // 8c-2：内层 `$lookup`（嵌套关系下钻）—— 位于 `$group` 之前，逐个提取为 `SqlNested`。
    // 其 `pipeline` 的 `$match` 附加条件（孙表 owner 注入）由 `lookup_extra_condition` 取出。
    let mut nested: Vec<SqlNested> = Vec::new();
    for stage in &pipeline[..group_idx] {
        let Some(n_lo) = stage.get("$lookup") else {
            continue;
        };
        let n_as = n_lo.get("as").and_then(|v| v.as_str()).unwrap_or("");
        let n_rel = nested_rel_name_from_as(n_as).ok_or_else(|| {
            format!(
                "关系聚合谓词内层 $lookup.as \"{n_as}\" 形态非法（应为 __rn{{序号}}__{{关系名}}）"
            )
        })?;
        let n_rel_def = rel_schema.relations.get(n_rel).ok_or_else(|| {
            format!(
                "关系聚合谓词嵌套关系 \"{n_rel}\" 未在 schema \"{}\" 中定义",
                rel_schema.name
            )
        })?;
        let n_pipeline: Vec<Value> = n_lo
            .get("pipeline")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        nested.push(SqlNested {
            as_name: n_as.to_string(),
            rel_name: n_rel.to_string(),
            model: n_rel_def.model.clone(),
            local_field: n_rel_def.local_field.clone(),
            foreign_field: n_rel_def.foreign_field.clone(),
            extra: lookup_extra_condition(&n_pipeline),
            filter: None,
        });
    }

    // 附加条件只取 `$group` **之前**的 `$match`（子级 filter + owner），
    // `$group` 之后的 `$match` 是 having，不得混入子查询 WHERE。
    // 8c-2：其中带 `__rn….` 前缀的键是嵌套关系条件 → 剥离前缀还原为孙表条件。
    let extra = split_nested_keys(lookup_extra_condition(&pipeline[..group_idx]), &mut nested)?;
    let group = pipeline[group_idx]
        .get("$group")
        .ok_or("关系聚合谓词 $group 阶段缺失（内部不变量破裂）")?;
    let having = pipeline
        .iter()
        .skip_while(|s| s.get("$group").is_none())
        .skip(1)
        .find_map(|s| s.get("$match"))
        .cloned()
        .ok_or("关系聚合谓词 $lookup 子 pipeline 缺少 $group 之后的 $match（having）")?;

    let mut aggs: Vec<(String, String)> = Vec::new();
    let gmap = group
        .as_object()
        .ok_or("关系聚合谓词 $group 阶段必须是对象")?;
    for (alias, acc) in gmap {
        if alias == "_id" {
            continue;
        }
        aggs.push((alias.clone(), acc_to_sql(backend, rel_schema, acc)?));
    }
    if aggs.is_empty() {
        return Err("关系聚合谓词 $group 没有聚合列".to_string());
    }

    Ok(SqlRelPredicate {
        as_name,
        rel_name,
        model: rel_def.model.clone(),
        local_field: rel_def.local_field.clone(),
        foreign_field: rel_def.foreign_field.clone(),
        extra,
        nested,
        having,
        aggs,
    })
}

/// 从本层 `$match` 附加条件中剥离嵌套关系前缀键（`__rn{i}__.`）：
/// 剥离后按 `as_name` 归入对应 [`SqlNested::filter`]（同关系多条件 → `$and` 合并），
/// 剩余标量条件原样返回（作为子查询 WHERE）。**逻辑组内出现嵌套前缀键 → Err**
/// （规划期已拒绝；走到此处即内外口径不一致，绝不静默近似）。
fn split_nested_keys(
    extra: Option<Value>,
    nested: &mut [SqlNested],
) -> Result<Option<Value>, String> {
    let Some(e) = extra else {
        return Ok(None);
    };
    // 展平顶层 `$and`（`prefix_filter` 会产出嵌套 `$and`），逐叶子分类
    let mut leaves: Vec<Value> = Vec::new();
    flatten_and(e, &mut leaves);
    let mut scalar: Vec<Value> = Vec::new();
    for leaf in leaves {
        let hit = leaf.as_object().filter(|m| m.len() == 1).and_then(|m| {
            let (k, v) = m.iter().next()?;
            let i = nested
                .iter()
                .position(|n| k.starts_with(&format!("{}.", n.as_name)))?;
            Some((i, k.clone(), v.clone()))
        });
        match hit {
            Some((i, key, val)) => {
                // 前缀 = `<as_name>.`，剥离后即为孙表字段路径
                let rest = key[nested[i].as_name.len() + 1..].to_string();
                let mut cm = Map::new();
                cm.insert(rest, val);
                let cond = Value::Object(cm);
                nested[i].filter = Some(match nested[i].filter.take() {
                    Some(prev) => merge_and(&prev, &cond),
                    None => cond,
                });
            }
            None => {
                if let Some(bad) = find_nested_key(&leaf, nested) {
                    return Err(format!(
                        "嵌套关系条件 \"{bad}\" 出现在逻辑组（$or/$nor）内，SQL 下推无法与 Mongo 语义\
                         逐字节对齐（内部不变量破裂）"
                    ));
                }
                scalar.push(leaf);
            }
        }
    }
    Ok(match scalar.len() {
        0 => None,
        1 => Some(scalar.remove(0)),
        _ => Some(json!({ "$and": scalar })),
    })
}

/// 展平顶层 `$and` 为叶子条件列表（`$and` 结合律下等价，避免嵌套组干扰前缀分类）
fn flatten_and(v: Value, out: &mut Vec<Value>) {
    if let Some(m) = v.as_object().filter(|m| m.len() == 1) {
        if let Some(arr) = m.get("$and").and_then(|a| a.as_array()) {
            for it in arr {
                flatten_and(it.clone(), out);
            }
            return;
        }
    }
    out.push(v);
}

/// 在条件子树中查找首个嵌套关系前缀键（用于逻辑组内的不变量断言）
fn find_nested_key(v: &Value, nested: &[SqlNested]) -> Option<String> {
    match v {
        Value::Object(m) => m.iter().find_map(|(k, val)| {
            if nested
                .iter()
                .any(|n| k.starts_with(&format!("{}.", n.as_name)))
            {
                Some(k.clone())
            } else {
                find_nested_key(val, nested)
            }
        }),
        Value::Array(a) => a.iter().find_map(|it| find_nested_key(it, nested)),
        _ => None,
    }
}

/// 单个累积器 → SQL 聚合表达式（识别 `pipeline::group::acc_expr` 生成的形态）
fn acc_to_sql(backend: Backend, rel_schema: &Schema, acc: &Value) -> Result<String, String> {
    let o = acc
        .as_object()
        .ok_or("关系聚合谓词累积器必须恰有一个算子键")?;
    let mut it = o.iter();
    let (op, arg) = match (it.next(), it.next()) {
        (Some(kv), None) => kv,
        _ => return Err("关系聚合谓词累积器必须恰有一个算子键（内部不变量破裂）".to_string()),
    };
    let col = |f: &str| -> Result<String, String> {
        // Mongo 字段引用带 `$` 前缀（如 `"$qty"`），SQL 列名须去掉
        let f = f.trim_start_matches('$');
        let c = scalar_column(rel_schema, f)
            .ok_or_else(|| format!("关系聚合谓词聚合字段 \"{f}\" 无法映射到标量列"))?;
        Ok(format!("c.{}", backend.pcol(&c)))
    };
    match op.as_str() {
        // `$count:"*"` → `{$sum: 1}`
        "$sum" => {
            if arg.as_i64() == Some(1) {
                return Ok("COUNT(*)".to_string());
            }
            if let Some(f) = arg.as_str() {
                return Ok(format!("SUM({})", col(f)?));
            }
            // `$count:"<f>"` → 非空计数形态
            if let Some(f) = count_field_pattern(arg) {
                return Ok(format!("COUNT({})", col(&f)?));
            }
            Err(format!("关系聚合谓词无法翻译 $sum 累积器 {arg}"))
        }
        "$avg" | "$min" | "$max" => {
            let f = arg
                .as_str()
                .ok_or_else(|| format!("关系聚合谓词 {op} 累积器必须带字段引用"))?;
            // §9.7「数值归 double」：`$avg` 先 CAST 到双精度（对齐 Mongo `$avg` 与各后端）
            Ok(if op == "$avg" {
                format!("AVG(CAST({} AS {}))", col(f)?, backend.double_type())
            } else {
                let fn_name = if op == "$min" { "MIN" } else { "MAX" };
                format!("{}({})", fn_name, col(f)?)
            })
        }
        other => Err(format!(
            "关系聚合谓词不支持的累积器 {other}（白名单 $count/$sum/$avg/$min/$max）"
        )),
    }
}

/// 生成 `EXISTS` / `NOT EXISTS` 子查询
///
/// 参数较多（根上下文 + 谓词 + 参数序号 + 警告通道），但其职责单一（只发射一条 SQL），
/// 拆结构体反而增加跨模块传递成本，故保留多参数形态。
#[allow(clippy::too_many_arguments)]
pub(super) fn exists_clause(
    backend: Backend,
    root_alias: &str,
    root_schema: &Schema,
    registry: &Registry,
    p: &SqlRelPredicate,
    negated: bool,
    param_seq: &mut usize,
    mut warnings: Warnings,
) -> Result<WhereClause, String> {
    let rel_schema = registry.get(&p.model)?;
    let local_col = scalar_column(root_schema, &p.local_field).ok_or_else(|| {
        format!(
            "关系聚合谓词 \"{}\" 的本地键 \"{}\" 无法映射到根表标量列",
            p.rel_name, p.local_field
        )
    })?;
    let fk_col = scalar_column(rel_schema, &p.foreign_field).ok_or_else(|| {
        format!(
            "关系聚合谓词 \"{}\" 的外键 \"{}\" 无法映射到子表标量列",
            p.rel_name, p.foreign_field
        )
    })?;

    let mut params: Vec<Value> = Vec::new();

    // 子级 filter + owner → `WHERE … AND …`（先过滤后聚合）
    let mut extra_sql = String::new();
    if let Some(extra) = &p.extra {
        let wh = build_filter(
            extra,
            backend,
            "c",
            &col_fn(rel_schema),
            param_seq,
            warnings.as_deref_mut(),
        )?;
        if !wh.text.is_empty() {
            extra_sql = format!(" AND {}", wh.text);
            params.extend(wh.params);
        }
    }

    // 8c-2：嵌套关系下钻 → 孙表 `EXISTS`（相关子查询挂在子表 `c` 上，位于 `$group` 之前）。
    // 键序与 Mongo 子 pipeline 对齐：内层 `$lookup`（条件）先于本层 `$match` —— 参数顺序：
    // 子级 extra → 嵌套（owner → filter）→ having。
    let mut nested_sql = String::new();
    for (i, n) in p.nested.iter().enumerate() {
        let n_schema = registry.get(&n.model)?;
        let n_local_col = scalar_column(rel_schema, &n.local_field).ok_or_else(|| {
            format!(
                "关系聚合谓词 \"{}\" 的嵌套关系 \"{}\" 本地键 \"{}\" 无法映射到子表标量列",
                p.rel_name, n.rel_name, n.local_field
            )
        })?;
        let n_fk_col = scalar_column(n_schema, &n.foreign_field).ok_or_else(|| {
            format!(
                "关系聚合谓词 \"{}\" 的嵌套关系 \"{}\" 外键 \"{}\" 无法映射到孙表标量列",
                p.rel_name, n.rel_name, n.foreign_field
            )
        })?;
        let alias = format!("n{i}");
        let mut conds: Vec<String> = vec![format!(
            "{}.{} = c.{}",
            alias,
            backend.pcol(&n_fk_col),
            backend.pcol(&n_local_col)
        )];
        // 孙表 owner 注入（与子表 owner 同源；越权防护）
        if let Some(ne) = &n.extra {
            let wh = build_filter(
                ne,
                backend,
                &alias,
                &col_fn(n_schema),
                param_seq,
                warnings.as_deref_mut(),
            )?;
            if !wh.text.is_empty() {
                conds.push(wh.text);
                params.extend(wh.params);
            }
        }
        // 嵌套条件（已剥离前缀的标量 / 对象点号路径条件）
        if let Some(nf) = &n.filter {
            let wh = build_filter(
                nf,
                backend,
                &alias,
                &col_fn(n_schema),
                param_seq,
                warnings.as_deref_mut(),
            )?;
            if !wh.text.is_empty() {
                conds.push(wh.text);
                params.extend(wh.params);
            }
        }
        nested_sql.push_str(&format!(
            " AND EXISTS (SELECT 1 FROM {} {} WHERE {})",
            tname(backend, n_schema),
            alias,
            conds.join(" AND ")
        ));
    }

    // having：聚合别名 → SQL 聚合表达式（完整表达式 → 包成 ColumnRef::Scalar）
    let expr_of = |name: &str| -> Option<ColumnRef> {
        p.aggs
            .iter()
            .find(|(a, _)| a == name)
            .map(|(_, e)| ColumnRef::Scalar(e.clone()))
    };
    let hw = build_filter_raw(&p.having, backend, &expr_of, param_seq, warnings)?;
    // having 为空：pipeline 侧已保证 having 至少引用一个 agg 别名（见 `collect_having_agg_refs`），
    // 走到这里说明规划与翻译口径不一致 → 显式 Err，绝不用恒真条件静默放大匹配集（D2：绝不静默）
    if hw.text.is_empty() {
        return Err(format!(
            "关系聚合谓词 \"{}\" 的 having 未翻译出任何条件（规划与翻译口径不一致）",
            p.rel_name
        ));
    }
    params.extend(hw.params);
    let having_sql = hw.text;

    let text = format!(
        "{}EXISTS (SELECT 1 FROM {} c WHERE c.{} = {}.{}{}{} GROUP BY c.{} HAVING {})",
        if negated { "NOT " } else { "" },
        tname(backend, rel_schema),
        backend.pcol(&fk_col),
        root_alias,
        backend.pcol(&local_col),
        extra_sql,
        nested_sql,
        backend.pcol(&fk_col),
        having_sql,
    );
    Ok(WhereClause { text, params })
}
