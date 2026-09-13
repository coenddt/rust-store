//! §9.6 关系聚合谓词（跨表条件过滤 / semi-join）的 SQL 下推（§10.5）
//!
//! core 产出的 Mongo 阶段：每个关系谓词一个 `$lookup`（`as` = `__rp…`，子 pipeline 内
//! `$match` + `$group` + `$match`）+ 外层 `$match` 的代理键 `{ "<as>.0": { "$exists": bool } }`。
//!
//! SQL 翻译：代理键 → `EXISTS (SELECT 1 FROM 子表 c WHERE c.fk = t.local [AND filter]
//! GROUP BY c.fk HAVING <谓词>)`；`$exists:false` → `NOT EXISTS`。**不扇出**（父行形状不变）。

use serde_json::Value;

use crate::dialect::filter::{build_filter, build_filter_raw, Warnings, WhereClause};
use crate::dialect::{scalar_column, Backend};
use crate::pipeline::rel_name_from_as;
use crate::schema::{Registry, Schema};

use super::aggregate::lookup_extra_condition;
use super::{col_fn, count_field_pattern, q, tname};

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
    /// 子级 filter + 目标 owner 注入（`$lookup` 内层 `$match` 的附加条件）
    pub extra: Option<Value>,
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
    // 附加条件只取 `$group` **之前**的 `$match`（子级 filter + owner），
    // `$group` 之后的 `$match` 是 having，不得混入子查询 WHERE。
    let extra = lookup_extra_condition(&pipeline[..group_idx]);
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
        having,
        aggs,
    })
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
        Ok(format!("c.{}", q(backend, &c)))
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

    // having：聚合别名 → SQL 聚合表达式
    let expr_of = |name: &str| -> Option<String> {
        p.aggs
            .iter()
            .find(|(a, _)| a == name)
            .map(|(_, e)| e.clone())
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
        "{}EXISTS (SELECT 1 FROM {} c WHERE c.{} = {}.{}{} GROUP BY c.{} HAVING {})",
        if negated { "NOT " } else { "" },
        tname(backend, rel_schema),
        q(backend, &fk_col),
        root_alias,
        q(backend, &local_col),
        extra_sql,
        q(backend, &fk_col),
        having_sql,
    );
    Ok(WhereClause { text, params })
}
