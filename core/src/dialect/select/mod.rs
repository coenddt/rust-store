//! SELECT 翻译：find / countDocuments / aggregate（含 $lookup → JOIN）

use serde_json::{json, Value};

use crate::schema::{Registry, Schema};

use crate::dialect::filter::build_filter;
use crate::dialect::ir::{RowShape, SqlStmt};
use crate::dialect::Backend;

mod aggregate;
mod find;
mod group_agg;
mod lookup_join;
mod relation_agg;

use aggregate::translate_aggregate;
use find::translate_find;

/// 翻译 find / countDocuments / aggregate 命令
pub fn translate_select(
    backend: Backend,
    cmd: &Value,
    registry: &Registry,
    warnings: &mut Vec<String>,
    unsupported: &mut Vec<Value>,
) -> Result<Vec<SqlStmt>, String> {
    let kind = cmd.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let collection = cmd.get("collection").and_then(|v| v.as_str()).unwrap_or("");
    // 三元组定位：source 缺省 default / namespace 缺省 null（兼容无定位字段的旧命令）
    let source = cmd
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or(crate::datasource::DEFAULT_SOURCE);
    let namespace = cmd.get("namespace").and_then(|v| v.as_str());
    // 结构 schema 按 (source, collection) 定位（override 回落见 `get_for_command`）；
    // 表名限定跟随命令 namespace（§6：定位由命令决定，结构由 Registry 决定）
    let mut schema = registry
        .get_for_command(source, namespace, collection)?
        .clone();
    if let Some(ns) = namespace {
        schema.namespace = Some(ns.to_string());
    }
    let schema = &schema;

    match kind {
        "countDocuments" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let mut seq = 0usize;
            let wh = build_filter(
                &filter,
                backend,
                "t",
                &col_fn(schema),
                &mut seq,
                Some(warnings),
            )?;
            let where_sql = if wh.text.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", wh.text)
            };
            let text = format!(
                "SELECT COUNT(*) FROM {} t{}",
                tname(backend, schema),
                where_sql
            );
            Ok(vec![SqlStmt::select(text, wh.params, RowShape::empty())])
        }
        "find" | "findOne" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let projection = cmd.get("projection").cloned();
            translate_find(
                backend,
                schema,
                &filter,
                projection.as_ref(),
                Some(warnings),
            )
        }
        "aggregate" => {
            let pipeline = cmd
                .get("pipeline")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            translate_aggregate(backend, schema, &pipeline, registry, warnings, unsupported)
        }
        _ => Err(format!("translate: 未知命令 kind = {}", kind)),
    }
}

/// 字段 → 列名：标量字段在本表；object/array 附属表字段跳过（标量字段原样）
///
/// 读侧薄包装；语义唯一出处见 [`super::scalar_column`]（与写侧共用同一实现，
/// 避免读写列映射语义漂移）。
pub(in crate::dialect::select) fn col_fn(schema: &Schema) -> impl Fn(&str) -> Option<String> + '_ {
    move |field: &str| super::scalar_column(schema, field)
}

/// 投影字段：null / 全 1 → 所有标量字段；否则取值为「非 0」的字段
pub(in crate::dialect::select) fn projection_fields(
    schema: &Schema,
    projection: Option<&Value>,
) -> Vec<String> {
    match projection {
        None | Some(Value::Null) => schema.fields.keys().cloned().collect(),
        Some(p) => p
            .as_object()
            .map(|o| {
                let on: Vec<String> = o
                    .iter()
                    .filter(|(_, v)| {
                        v.as_i64().map(|n| n != 0).unwrap_or(false) || v.as_bool() == Some(true)
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                if on.is_empty() {
                    schema.fields.keys().cloned().collect()
                } else {
                    on
                }
            })
            .unwrap_or_else(|| schema.fields.keys().cloned().collect()),
    }
}

/// 投影可用性校验：显式请求 schema 声明的 `object`/`array` 字段时，
/// SQL 侧无对应列（对象/数组不建列）→ 显式 `Err`，**绝不静默返回残缺行**（D2：绝不静默）。
///
/// 仅校验**显式请求**的字段：`projection = None`（全字段）维持既有「跳过」语义，
/// 避免影响内部全字段取数（关系目标展开等）。
/// 识别 `$count:"<f>"` 生成的「非空计数」形态：
/// `{"$cond":[{"$eq":[{"$ifNull":["$f", null]}, null]}, 0, 1]}`
///
/// 根级 `$group`（`group_agg.rs`）与关系聚合谓词（`relation_agg.rs`）共用同一形态识别。
pub(super) fn count_field_pattern(v: &Value) -> Option<String> {
    let arr = v.get("$cond")?.as_array()?;
    if arr.len() != 3 {
        return None;
    }
    let cond = arr[0].get("$eq")?.as_array()?;
    let if_null = cond.first()?.get("$ifNull")?.as_array()?;
    let f = if_null.first()?.as_str()?;
    if cond.get(1)?.is_null() && arr[1].as_i64()? == 0 && arr[2].as_i64()? == 1 {
        return f.strip_prefix('$').map(|x| x.to_string());
    }
    None
}

pub(in crate::dialect::select) fn check_projection_supported(
    schema: &Schema,
    projection: Option<&Value>,
) -> Result<(), String> {
    let Some(o) = projection.and_then(|p| p.as_object()) else {
        return Ok(());
    };
    for (k, v) in o {
        let requested = v.as_i64().map(|n| n != 0).unwrap_or(false) || v.as_bool() == Some(true);
        if !requested || k == "_id" {
            continue;
        }
        // 关系名与计算列不是真实列，交由关系 JOIN / 派生表处理，不在本检查范围
        if schema.relations.contains_key(k.as_str()) || schema.compute(k).is_some() {
            continue;
        }
        if super::scalar_column(schema, k).is_none() {
            return Err(format!(
                "SQL 后端不支持投影 object/array 字段 \"{k}\"（无对应列；D2：绝不静默返回残缺结果）"
            ));
        }
    }
    Ok(())
}

pub(in crate::dialect::select) fn q(backend: Backend, ident: &str) -> String {
    backend.quote_ident(ident)
}

/// 表名 SQL：带 schema.namespace 限定（区别于列/别名的 `q`）
pub(in crate::dialect::select) fn tname(backend: Backend, schema: &Schema) -> String {
    backend.qualified_table(schema.ns(), &schema.collection)
}

/// LIMIT/OFFSET 子句（含绑定参数），`param_seq` 为占位序号游标。
///
/// 仅 offset（无 limit）时没有跨后端统一写法，必须按后端生成合法惯用法（评测 M-8-1）：
/// PostgreSQL 省略 LIMIT 只写 OFFSET；MySQL 用超大 LIMIT 表示「取到末尾」；SQLite 用 `LIMIT -1`。
pub(in crate::dialect::select) fn limit_offset_sql(
    backend: Backend,
    limit: Option<i64>,
    offset: i64,
    param_seq: &mut usize,
) -> (String, Vec<Value>) {
    let mut sql = String::new();
    let mut params: Vec<Value> = Vec::new();
    if let Some(lim) = limit {
        match backend {
            Backend::Postgres => {
                sql = format!(" LIMIT ${}", *param_seq + 1);
                *param_seq += 1;
                params.push(json!(lim));
                if offset > 0 {
                    sql.push_str(&format!(" OFFSET ${}", *param_seq + 1));
                    params.push(json!(offset));
                }
            }
            _ => {
                if offset > 0 {
                    sql = " LIMIT ? OFFSET ?".to_string();
                    params.push(json!(lim));
                    params.push(json!(offset));
                } else {
                    sql = " LIMIT ?".to_string();
                    params.push(json!(lim));
                }
            }
        }
    } else if offset > 0 {
        match backend {
            Backend::Postgres => {
                sql = format!(" OFFSET ${}", *param_seq + 1);
                params.push(json!(offset));
            }
            Backend::Mysql => {
                sql = " LIMIT 18446744073709551615 OFFSET ?".to_string();
                params.push(json!(offset));
            }
            Backend::Sqlite => {
                sql = " LIMIT -1 OFFSET ?".to_string();
                params.push(json!(offset));
            }
        }
    }
    (sql, params)
}
