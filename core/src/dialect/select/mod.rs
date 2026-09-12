//! SELECT 翻译：find / countDocuments / aggregate（含 $lookup → JOIN）

use serde_json::{json, Value};

use crate::schema::{Registry, Schema};

use crate::dialect::filter::build_filter;
use crate::dialect::ir::{RowShape, SqlStmt};
use crate::dialect::Backend;

mod aggregate;
mod find;
mod lookup_join;

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
    let mut schema = registry.get_for_command(source, namespace, collection)?.clone();
    if let Some(ns) = namespace {
        schema.namespace = Some(ns.to_string());
    }
    let schema = &schema;

    match kind {
        "countDocuments" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let mut seq = 0usize;
            let wh = build_filter(&filter, backend, "t", &col_fn(schema), &mut seq);
            let where_sql = if wh.text.is_empty() { String::new() } else { format!(" WHERE {}", wh.text) };
            let text = format!("SELECT COUNT(*) FROM {} t{}", tname(backend, schema), where_sql);
            Ok(vec![SqlStmt::select(text, wh.params, RowShape::empty())])
        }
        "find" | "findOne" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let projection = cmd.get("projection").cloned();
            translate_find(backend, schema, &filter, projection.as_ref())
        }
        "aggregate" => {
            let pipeline = cmd.get("pipeline").and_then(|v| v.as_array()).cloned().unwrap_or_default();
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
pub(in crate::dialect::select) fn projection_fields(schema: &Schema, projection: Option<&Value>) -> Vec<String> {
    match projection {
        None | Some(Value::Null) => schema.fields.keys().cloned().collect(),
        Some(p) => p.as_object()
            .map(|o| {
                let on: Vec<String> = o
                    .iter()
                    .filter(|(_, v)| {
                        v.as_i64().map(|n| n != 0).unwrap_or(false)
                            || v.as_bool() == Some(true)
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                if on.is_empty() { schema.fields.keys().cloned().collect() } else { on }
            })
            .unwrap_or_else(|| schema.fields.keys().cloned().collect()),
    }
}

pub(in crate::dialect::select) fn q(backend: Backend, ident: &str) -> String {
    backend.quote_ident(ident)
}

/// 表名 SQL：带 schema.namespace 限定（区别于列/别名的 `q`）
pub(in crate::dialect::select) fn tname(backend: Backend, schema: &Schema) -> String {
    backend.qualified_table(schema.ns(), &schema.collection)
}
