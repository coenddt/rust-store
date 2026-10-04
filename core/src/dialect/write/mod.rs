//! 写语句翻译：insertOne / insertMany / updateMany / findOneAndUpdate / deleteMany
//!
//! 约束（铁律 1/8）：值全部参数化；标识符只来自 Registry 字段白名单 + `Backend::quote_ident`；
//! 无法安全翻译的组合直接报错，**绝不生成错误 SQL**。
//!
//! MySQL 无 `RETURNING`（`Backend::supports_returning() == false`）：写后回读拆成
//! `UPDATE/INSERT` + `SELECT` 两条语句，由 Host 执行器顺序执行（执行器只做「绑定 + 执行」，
//! 不做任何 SQL 拼装）。
//!
//! 文件组织：INSERT 在 [`insert`]，UPDATE / 回读在 [`update`]，upsert 在 [`upsert`]；
//! 派发入口与共享工具（Binder / 标识符引用 / 回读列）在本文件。

use serde_json::{json, Value};

use crate::schema::{Registry, Schema};

use super::filter::build_filter;
use super::ir::{RowCol, RowShape, SqlStmt};
use super::{field_is_bool, physical_of, Backend};

mod insert;
mod update;
mod upsert;

/// 翻译写命令
///
/// 写路径不产出 `warnings`（`unsupported` 机制在 select 侧）——需要告警的翻译在此直接报错，
/// 故不再保留占位参数（评测报告 I-2）。
pub fn translate_write(
    backend: Backend,
    cmd: &Value,
    registry: &Registry,
) -> Result<Vec<SqlStmt>, String> {
    let kind = cmd.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let collection = cmd.get("collection").and_then(|v| v.as_str()).unwrap_or("");
    // 四元组定位：source 缺省 default / database / schema 缺省 null（兼容无定位字段的旧命令）
    let source = cmd
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or(crate::datasource::DEFAULT_SOURCE);
    let database = cmd.get("database").and_then(|v| v.as_str());
    let schema_q = cmd.get("schema").and_then(|v| v.as_str());
    // 结构 schema 按 (source, collection) 定位（override 回落见 `get_for_command`）；
    // 表名限定跟随命令 database/schema（§6：定位由命令决定，结构由 Registry 决定）
    let mut schema = registry
        .get_for_command(source, database, schema_q, collection)?
        .clone();
    schema.database = database.filter(|s| !s.is_empty()).map(String::from);
    schema.schema = schema_q.filter(|s| !s.is_empty()).map(String::from);
    let schema = &schema;

    match kind {
        "insertOne" => {
            let doc = cmd.get("doc").cloned().unwrap_or(json!({}));
            Ok(vec![insert::build_insert(backend, schema, &doc)])
        }
        "insertMany" => {
            let docs = cmd
                .get("docs")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            // 多条 → 一条 VALUES (...) 多组
            if docs.is_empty() {
                return Err("insertMany 无文档".to_string());
            }
            let upsert_by_id = cmd
                .get("upsertById")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            insert::build_insert_many(backend, schema, &docs, upsert_by_id)
        }
        "updateMany" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let update = cmd.get("update").cloned().unwrap_or(json!({}));
            update::translate_update_many(backend, schema, &filter, &update)
        }
        "findOneAndUpdate" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let update = cmd.get("update").cloned().unwrap_or(json!({}));
            let options = cmd.get("options").cloned().unwrap_or(json!({}));
            update::translate_find_one_and_update(backend, schema, &filter, &update, &options)
        }
        "deleteMany" => {
            let filter = cmd.get("filter").cloned().unwrap_or(json!({}));
            let mut seq = 0usize;
            // 写路径无告警通道（None）：filter 中出现无法表达的语义组合时直接报错（见 filter::Warnings）
            let wh = build_filter(&filter, backend, "t", &col_map(schema), &mut seq, None)?;
            let where_sql = if wh.text.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", wh.text)
            };
            let text = format!("DELETE FROM {} AS t{}", tname(backend, schema), where_sql);
            Ok(vec![SqlStmt::write(text, wh.params)])
        }
        _ => Err(format!("translate: 未知写命令 kind = {}", kind)),
    }
}

// ─── 参数绑定 ────────────────────────────────────────────────

/// 参数绑定游标：统一 `?`（MySQL/SQLite）与 `$n`（PostgreSQL）占位符。
///
/// `bind` 返回当前位置的占位符并把值入队；PostgreSQL 的 `$n` 序号与队列长度一致，
/// 从而保证「文本占位符顺序 == params 顺序」。
struct Binder {
    backend: Backend,
    params: Vec<Value>,
}

impl Binder {
    fn new(backend: Backend) -> Self {
        Binder {
            backend,
            params: Vec::new(),
        }
    }

    fn bind(&mut self, v: Value) -> String {
        let ph = self.backend.placeholder(self.params.len());
        self.params.push(v);
        ph
    }

    /// 下一个占位符序号（PostgreSQL 用；MySQL/SQLite 恒 0 起点无影响）
    fn seq(&self) -> usize {
        self.params.len()
    }
}

// ─── 字段/列白名单 ───────────────────────────────────────────

/// 标量字段 → 列名（写侧薄包装；语义唯一出处见 [`super::scalar_column`]）。
///
/// 仅用于**必须标量**的场景（upsert 唯一键条件）；object/array 字段因落 JSON 列，
/// 走 [`writable_col`]。
fn scalar_col(schema: &Schema, field: &str) -> Option<String> {
    super::scalar_column(schema, field)
}

/// 可写列名（标量 / object / array 同表单列）：写目标不接受点号路径。
///
/// object/array 字段落 JSON 单列（见执行文档 §4.5），与标量一样可直接 `SET`/`INSERT`。
/// 非点号字段一律可写（与 [`super::scalar_column`] 的宽松语义一致：未声明字段按裸列名处理）。
fn writable_col(_schema: &Schema, field: &str) -> Option<String> {
    if field.contains('.') {
        return None;
    }
    Some(field.to_string())
}

/// 绑定值：object/array 字段 → JSON 文本字符串（跨后端落 JSON 列）；其余原样。
///
/// 跨方言对齐：MySQL `JSON` / PG `jsonb` / SQLite `TEXT` 均接受 JSON 文本参数；
/// 空值（`null`）原样绑定为 SQL NULL（缺失 vs 显式 null 由 `__present` 哨兵区分）。
pub(super) fn bind_value(schema: &Schema, col: &str, v: &Value) -> Value {
    let is_json = matches!(
        schema.fields.get(col).map(|f| f.field_type.as_str()),
        Some("object") | Some("array")
    );
    if is_json && !v.is_null() {
        Value::String(serde_json::to_string(v).unwrap_or_default())
    } else {
        v.clone()
    }
}

fn col_map(schema: &Schema) -> impl Fn(&str) -> Option<super::ColumnRef> + '_ {
    move |field: &str| super::field_column_ref(schema, field)
}

/// 回读列：`_id`（物理主键列）恒首位 + 其余字段（含 object/array JSON 列）按字典序
/// （确定性输出，供 parity）。
///
/// `_id` 不要求出现在 `schema.fields`（core 不自动补 `_id`），但物理表恒有该列，
/// 且 `restore_rows` 依赖它做根分组，故强制补上。
fn returning_cols(schema: &Schema) -> Vec<String> {
    let mut cols: Vec<String> = schema
        .fields
        .keys()
        .filter(|f| f.as_str() != "_id")
        .cloned()
        .collect();
    cols.sort();
    cols.insert(0, "_id".to_string());
    // 缺失 vs null 三态（F-07/H-01）：回读（RETURNING 或 MySQL 写后回读）同样携带
    // `__present` 哨兵列，供还原时区分「显式 null（有键）」与「缺失（无键）」。
    cols.push("__present".to_string());
    cols
}

/// 回读列 → RowShape（标量直接还原到 `[field]`；object/array 列标记 JSON 解析；
/// §9.7 布尔列标记归一）。
///
/// 回读边界（设计 §6.4）：SQL 发射物理名 → **alias 用物理名**（与驱动返回列键一致），
/// **`json_path` 保持逻辑名**（还原为逻辑键）；`returning_cols` 亦保持逻辑。
fn returning_shape(schema: &Schema, cols: &[String]) -> RowShape {
    RowShape {
        columns: cols
            .iter()
            .filter(|c| c.as_str() != "__present")
            .map(|c| {
                let is_json = matches!(
                    schema.fields.get(c.as_str()).map(|f| f.field_type.as_str()),
                    Some("object") | Some("array")
                );
                if is_json {
                    RowCol::json(&physical_of(c), &[c.as_str()])
                } else {
                    RowCol::scalar_bool(&physical_of(c), &[c.as_str()], field_is_bool(schema, c))
                }
            })
            .collect(),
        present_alias: Some("__present".to_string()),
    }
}

// ─── 标识符引用 ─────────────────────────────────────────────

fn q(backend: Backend, ident: &str) -> String {
    backend.quote_ident(ident)
}

/// 表名 SQL：带 schema.database/schema 限定（区别于列/别名的 `q`）
fn tname(backend: Backend, schema: &Schema) -> String {
    backend.qualified_table(schema.database(), schema.schema(), &schema.collection)
}
