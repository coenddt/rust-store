//! 根级 `$group` / `$having` 的 SQL 翻译（§9.2(1) 批次一）
//!
//! Mongo `$group`（`_id` = by 键组合 + 累积器） → `GROUP BY` + `SELECT 聚合列`；
//! `$group` 之后的 `$match`（即 `$having`）→ `HAVING`；分组后的 `$sort/$skip/$limit`
//! → `ORDER BY ... LIMIT/OFFSET`（§9.3 固定序：WHERE → GROUP BY → HAVING → ORDER BY → LIMIT）。
//!
//! 全表单组（`by` 省略 / `[]`）→ 无 `GROUP BY`（SQL 天然对空输入返回 1 行，与 §9.7 对齐），
//! 此时 `group::build_stages` 在 Mongo 侧追加的 `$facet` + `$replaceRoot` 空集护栏对 SQL 是 no-op。

use serde_json::Value;

use crate::dialect::filter::{build_filter, build_filter_raw, WhereClause};
use crate::dialect::ir::{RowCol, RowShape, SqlStmt};
use crate::dialect::{field_is_bool, scalar_column, Backend};
use crate::schema::Schema;

use super::{col_fn, count_field_pattern, limit_offset_sql, q, tname};

/// 分组键：输出名（JSON 路径，可含点号）→ 本表标量字段
struct GroupKey {
    out: String,
    field: String,
}

/// 聚合算子（白名单 §9.5 批次一）
#[derive(Debug, Clone, Copy, PartialEq)]
enum AggOp {
    /// `$count:"*"` → `COUNT(*)`
    CountRows,
    /// `$count:"<field>"`（非空计数）→ `COUNT(col)`
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

/// 聚合列
struct GroupAggCol {
    alias: String,
    op: AggOp,
    field: Option<String>,
}

/// 解析后的 `$group` 阶段
pub(super) struct GroupSpec {
    keys: Vec<GroupKey>,
    aggs: Vec<GroupAggCol>,
}

/// 解析 `$group` 阶段对象（`{"_id": …, "<别名>": <累积器>}`）
pub(super) fn parse_group(g: &Value) -> Result<GroupSpec, String> {
    let obj = g.as_object().ok_or("$group 阶段必须是对象")?;

    let mut keys: Vec<GroupKey> = Vec::new();
    match obj.get("_id") {
        None | Some(Value::Null) => {}
        Some(v) => flatten_id(v, "", &mut keys)?,
    }

    let mut aggs: Vec<GroupAggCol> = Vec::new();
    for (alias, acc) in obj {
        if alias == "_id" {
            continue;
        }
        let (op, field) = parse_acc(acc)?;
        aggs.push(GroupAggCol {
            alias: alias.clone(),
            op,
            field,
        });
    }
    Ok(GroupSpec { keys, aggs })
}

/// `_id` 展开：字符串 `"$f"`（单键）或嵌套对象 `{a: "$x", b: {c: "$y"}}`（多键）
fn flatten_id(v: &Value, prefix: &str, out: &mut Vec<GroupKey>) -> Result<(), String> {
    match v {
        Value::String(s) => {
            let field = s
                .strip_prefix('$')
                .ok_or_else(|| format!("$group._id 仅支持字段引用，收到 {s}"))?;
            let out_key = if prefix.is_empty() {
                field.to_string()
            } else {
                prefix.to_string()
            };
            out.push(GroupKey {
                out: out_key,
                field: field.to_string(),
            });
            Ok(())
        }
        Value::Object(m) => {
            for (k, sub) in m {
                let p = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_id(sub, &p, out)?;
            }
            Ok(())
        }
        other => Err(format!(
            "$group._id 仅支持字段引用 / 字段嵌套对象 / null，收到 {other}"
        )),
    }
}

/// 解析累积器（识别 `pipeline::group` 生成的形态）
fn parse_acc(v: &Value) -> Result<(AggOp, Option<String>), String> {
    let o = v.as_object().ok_or("$group 累积器必须恰有一个算子键")?;
    let mut it = o.iter();
    let (op, arg) = match (it.next(), it.next()) {
        (Some(kv), None) => kv,
        _ => return Err("$group 累积器必须恰有一个算子键".to_string()),
    };
    match op.as_str() {
        "$sum" => {
            // `$count:"*"` → `{$sum: 1}`
            if arg.as_i64() == Some(1) {
                return Ok((AggOp::CountRows, None));
            }
            if let Some(f) = arg.as_str() {
                return Ok((AggOp::Sum, Some(field_ref(f)?)));
            }
            // `$count:"<f>"` → 非空计数形态
            if let Some(f) = count_field_pattern(arg) {
                return Ok((AggOp::Count, Some(f)));
            }
            Err(format!("SQL 后端无法翻译 $group 的 $sum 累积器 {arg}"))
        }
        "$avg" => Ok((AggOp::Avg, Some(field_ref(acc_field(arg, "$avg")?)?))),
        "$min" => Ok((AggOp::Min, Some(field_ref(acc_field(arg, "$min")?)?))),
        "$max" => Ok((AggOp::Max, Some(field_ref(acc_field(arg, "$max")?)?))),
        other => Err(format!(
            "SQL 后端不支持的 $group 累积器 {other}（白名单 $count/$sum/$avg/$min/$max）"
        )),
    }
}

fn acc_field<'a>(arg: &'a Value, op: &str) -> Result<&'a str, String> {
    arg.as_str()
        .ok_or_else(|| format!("$group 的 {op} 累积器必须带字段引用"))
}

fn field_ref(s: &str) -> Result<String, String> {
    s.strip_prefix('$')
        .map(|x| x.to_string())
        .ok_or_else(|| format!("$group 累积器仅支持字段引用，收到 {s}"))
}

/// 聚合列 SQL 表达式
fn agg_expr(backend: Backend, schema: &Schema, a: &GroupAggCol) -> Result<String, String> {
    let col = |f: Option<&str>| -> Result<String, String> {
        let f = f.ok_or_else(|| format!("$group 聚合列 \"{}\" 缺少字段", a.alias))?;
        let c = scalar_column(schema, f).ok_or_else(|| {
            format!(
                "$group 聚合字段 \"{f}\" 无法映射到本表标量列（\"{}\"）",
                a.alias
            )
        })?;
        Ok(format!("t.{}", q(backend, &c)))
    };
    Ok(match a.op {
        AggOp::CountRows => "COUNT(*)".to_string(),
        AggOp::Count => format!("COUNT({})", col(a.field.as_deref())?),
        AggOp::Sum => format!("SUM({})", col(a.field.as_deref())?),
        // §9.7「数值归 double」：先 CAST 到双精度，消除 MySQL `AVG(int)` 的 4 位小数截断
        AggOp::Avg => format!(
            "AVG(CAST({} AS {}))",
            col(a.field.as_deref())?,
            backend.double_type()
        ),
        AggOp::Min => format!("MIN({})", col(a.field.as_deref())?),
        AggOp::Max => format!("MAX({})", col(a.field.as_deref())?),
    })
}

/// 翻译根级 `$group` 聚合（返回单条 SELECT）
// SQL 聚合翻译需后端 / schema / 规格 / 别名 / 参数序号等完整上下文，拆结构体反而增加传递成本
#[allow(clippy::too_many_arguments)]
pub(super) fn translate_group(
    backend: Backend,
    schema: &Schema,
    spec: &GroupSpec,
    root_matches: &[Value],
    having: Option<&Value>,
    order: &[(String, i64)],
    skip: i64,
    limit: Option<i64>,
    project: Option<&Value>,
    param_seq: &mut usize,
    warnings: &mut Vec<String>,
) -> Result<SqlStmt, String> {
    // ── 分组键 → 本表标量列 ──
    let mut keys: Vec<(String, String)> = Vec::new(); // (输出名, SQL 表达式)
    for k in &spec.keys {
        let col = scalar_column(schema, &k.field).ok_or_else(|| {
            format!(
                "SQL 后端无法把 $group 的 by 键 \"{}\" 映射到本表标量列（object/array 点号路径未映射为列）",
                k.field
            )
        })?;
        keys.push((k.out.clone(), format!("t.{}", q(backend, &col))));
    }

    // ── 聚合列 → SQL 表达式 ──
    let mut aggs: Vec<(String, String)> = Vec::new();
    for a in &spec.aggs {
        aggs.push((a.alias.clone(), agg_expr(backend, schema, a)?));
    }

    // `$having` / 分组后 `$sort` 均可引用 by 键 / agg 别名（§9.2(1)）。
    // core 产出的 pipeline 是 **Mongo 形态**：`$group` 之后 by 键已改名为 `_id`
    // （单键）或 `_id.<key>`（多键），`$having`/`$sort` 中引用的 by 键也已被同步改写。
    // 故此处先做反向映射（`_id` / `_id.<key>` → 分组键表达式），再按原名匹配，
    // 保证「Mongo 支持 ⇒ SQL 同样支持」，不因改名而落到「未映射列」显式 Err。
    let expr_of = |name: &str| -> Option<String> {
        let by_alias: Option<&str> = if name == "_id" {
            // 单 by 键：`_id` 即该键；多 by 键：`_id` 是对象，不是合法分组列引用
            if keys.len() == 1 {
                Some(keys[0].0.as_str())
            } else {
                None
            }
        } else {
            name.strip_prefix("_id.")
        };
        if let Some(a) = by_alias {
            if let Some((_, e)) = keys.iter().find(|(o, _)| o == a) {
                return Some(e.clone());
            }
        }
        if let Some((_, e)) = keys.iter().find(|(o, _)| o == name) {
            return Some(e.clone());
        }
        aggs.iter().find(|(a, _)| a == name).map(|(_, e)| e.clone())
    };

    // ── 输出列：`$project` 决定最终字段集（排除 `_id:0`）；无 `$project` 则全出 ──
    let requested: Option<Vec<String>> = project.and_then(|p| p.as_object()).map(|o| {
        o.iter()
            .filter(|(k, v)| {
                !(k.as_str() == "_id" && (v.as_i64() == Some(0) || v.as_bool() == Some(false)))
            })
            .map(|(k, _)| k.clone())
            .collect()
    });
    let wanted = |name: &str| -> bool {
        requested
            .as_ref()
            .map(|r| r.iter().any(|x| x == name))
            .unwrap_or(true)
    };

    let mut cols_sql: Vec<String> = Vec::new();
    let mut columns: Vec<RowCol> = Vec::new();
    for (i, k) in spec.keys.iter().enumerate() {
        let out = &k.out;
        if !wanted(out) {
            continue;
        }
        let alias = format!("b{i}");
        cols_sql.push(format!("{} AS {}", keys[i].1, q(backend, &alias)));
        // by 键为 `null` 时也须输出该键（分组结果行恒有该字段）→ `always`
        columns.push(RowCol {
            alias,
            json_path: out.split('.').map(String::from).collect(),
            is_array: false,
            one: false,
            ones: Vec::new(),
            sub_shape: None,
            always: true,
            // §9.7 布尔归一：按 `boolean` 字段分组时分组键 0/1 → bool（对齐 Mongo 的 `_id`）
            is_bool: field_is_bool(schema, &k.field),
        });
    }
    for (i, (alias_name, expr)) in aggs.iter().enumerate() {
        if !wanted(alias_name) {
            continue;
        }
        let alias = format!("a{i}");
        cols_sql.push(format!("{} AS {}", expr, q(backend, &alias)));
        columns.push(RowCol {
            alias,
            json_path: vec![alias_name.clone()],
            is_array: false,
            one: false,
            ones: Vec::new(),
            sub_shape: None,
            always: true,
            is_bool: false,
        });
    }
    if cols_sql.is_empty() {
        return Err("$group 查询没有可输出的分组键 / 聚合列".to_string());
    }

    // ── WHERE（$group 之前的 $match） ──
    let mut wheres: Vec<WhereClause> = Vec::new();
    for m in root_matches {
        let wh = build_filter(
            m,
            backend,
            "t",
            &col_fn(schema),
            param_seq,
            Some(&mut *warnings),
        )?;
        if !wh.text.is_empty() {
            wheres.push(wh);
        }
    }
    let mut params: Vec<Value> = Vec::new();
    let where_sql = if wheres.is_empty() {
        String::new()
    } else {
        let text = wheres
            .iter()
            .map(|w| w.text.clone())
            .collect::<Vec<_>>()
            .join(" AND ");
        for w in &wheres {
            params.extend(w.params.clone());
        }
        format!(" WHERE {text}")
    };

    // ── GROUP BY（by 全部键，未请求的键也必须参与分组） ──
    let group_sql = if keys.is_empty() {
        String::new()
    } else {
        format!(
            " GROUP BY {}",
            keys.iter()
                .map(|(_, e)| e.clone())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    // ── HAVING（`$group` 之后的 $match） ──
    let having_sql = match having {
        Some(h) if !h.is_null() => {
            let wh = build_filter_raw(h, backend, &expr_of, param_seq, Some(&mut *warnings))?;
            if wh.text.is_empty() {
                String::new()
            } else {
                params.extend(wh.params);
                format!(" HAVING {}", wh.text)
            }
        }
        _ => String::new(),
    };

    // ── ORDER BY（分组后 $sort，键域 = by 键 ∪ agg 别名） ──
    let mut order_parts: Vec<String> = Vec::new();
    for (k, dir) in order {
        let e = expr_of(k)
            .ok_or_else(|| format!("$sort 键 \"{k}\" 不在 $group 的 by 键 / agg 别名域内"))?;
        order_parts.push(format!("{} {}", e, if *dir >= 0 { "ASC" } else { "DESC" }));
    }
    let order_sql = if order_parts.is_empty() {
        String::new()
    } else {
        format!(" ORDER BY {}", order_parts.join(", "))
    };

    let (limit_sql, limit_params) = limit_offset_sql(backend, limit, skip, param_seq);
    params.extend(limit_params);

    let text = format!(
        "SELECT {} FROM {}{}{}{}{}{}",
        cols_sql.join(", "),
        tname(backend, schema),
        " t",
        where_sql,
        group_sql,
        having_sql,
        order_sql,
    ) + &limit_sql;

    Ok(SqlStmt::select(
        text,
        params,
        RowShape {
            columns,
            present_alias: None,
        },
    ))
}
