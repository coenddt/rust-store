//! introspection 行 JSON → schemaJSON（纯映射，可喂 `Registry::register`）
//!
//! 联网查库在 Host 侧做，只把结构化的 `rows` JSON（tables / columns / fks / indexes）传给
//! 本模块做纯映射。SQLite 用 `sqlite_master` + `PRAGMA`；MySQL/PG 用 `information_schema`，
//! Host 归一化成下列行形状：

use serde_json::{json, Map, Value};

use super::Backend;

/// 从规范化 introspection 行 JSON 生成 schemaJSON 数组。
///
/// 期望输入（Host 归一化后的结构）：
/// ```json
/// {
///   "tables": [{ "name": "posts" }],
///   "columns": [{ "table": "posts", "name": "title", "type": "TEXT",
///                 "notnull": 0, "pk": 0 }],
///   "fks":      [{ "table": "order_items", "column": "order_id",
///                  "refTable": "orders", "refColumn": "_id" }],
///   "indexes":  [{ "table": "posts", "name": "idx_status", "columns": ["status"], "unique": 0 }]
/// }
/// ```
///
/// 行形状扩展：`tables[].namespace`（可选）— 当 Host 明确知道表所属库/schema
/// （如 SQLite attached db、MySQL 显式 database、PG 显式 schema）时携带，
/// 生成的 def 会带上该 `namespace`（缺省不产出该字段 = 连接默认）。
///
/// 输出：schemaJSON 数组（每个 `Schema` 一个 def，可直接传给 `Registry::register`）。
pub fn schema_def_from_rows(rows: &Value) -> Result<Value, String> {
    let tables = rows.get("tables").and_then(|t| t.as_array()).cloned().unwrap_or_default();
    let columns = rows.get("columns").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    let fks = rows.get("fks").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    // 索引仅作元数据（铁律 6：绝不写 DDL 回库），此解析结果暂不参与 schema 生成
    let _indexes = rows.get("indexes").and_then(|c| c.as_array()).cloned().unwrap_or_default();

    let mut defs: Vec<Value> = Vec::new();
    for t in &tables {
        let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let table_cols: Vec<&Value> = columns
            .iter()
            .filter(|c| c.get("table").and_then(|v| v.as_str()) == Some(name.as_str()))
            .collect();
        let table_fks: Vec<&Value> = fks
            .iter()
            .filter(|f| f.get("table").and_then(|v| v.as_str()) == Some(name.as_str()))
            .collect();

        let mut fields_map = Map::new();
        let mut relations_map = Map::new();

        for c in &table_cols {
            let col = c.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let db_type = c.get("type").and_then(|v| v.as_str()).unwrap_or("").to_uppercase();
            let required = c.get("notnull").and_then(|v| v.as_i64()).unwrap_or(0) != 0;
            let is_pk = c.get("pk").and_then(|v| v.as_i64()).unwrap_or(0) != 0;
            if is_pk {
                fields_map.insert("_id".to_string(), json!({ "type": "string", "required": true }));
                // 主键列可能非 _id 命名：映射后再补原始列
                if col != "_id" {
                    fields_map.insert("__pk_col".to_string(), json!(col));
                }
                continue;
            }
            fields_map.insert(col.clone(), json!({ "type": db_field_type(&col, &db_type), "required": required }));
        }

        // 外键 → 关系；被引用表的主键列名（默认 _id）为 foreignField
        for f in &table_fks {
            let col = f.get("column").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let ref_table = f.get("refTable").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let ref_column = f.get("refColumn").and_then(|v| v.as_str()).unwrap_or("_id").to_string();
            if col.is_empty() || ref_table.is_empty() {
                continue;
            }
            // 关系名：被引用表（关系目标）
            // 本表持有外键 → 对 refTable 是 many（本表多条指向一条）；也反推给 refTable 一条
            relations_map.insert(
                ref_table.clone(),
                json!({
                    "model": ref_table,
                    "type": "one",
                    "localField": col,
                    "foreignField": ref_column,
                }),
            );
            // 反向关系：在 refTable 侧补 a many 到本表 —— 用 `__rev_<refTable>` 占位，
            // 由第二轮扫描统一归一到 refTable 的 def 里（localField 用引用列，foreignField 用外键列）
            relations_map.insert(
                format!("__rev_{}", ref_table),
                json!({
                    "model": name,
                    "type": "many",
                    "localField": ref_column,
                    "foreignField": col,
                }),
            );
        }

        let mut def = json!({
            "name": name,
            "collection": name,
            "idPrefix": "",
            "timestamps": false,
            "fields": Value::Object(fields_map),
            "relations": Value::Object(relations_map),
        });
        // tables[].namespace（可选）→ def.namespace（连接内库/schema 显式定位）
        let ns = t.get("namespace").and_then(|v| v.as_str()).unwrap_or("");
        if !ns.is_empty() {
            def["namespace"] = json!(ns);
        }
        defs.push(def);
    }

    // 第二轮：把反向 `__rev_<refTable>` 关系归一到被引用表的 `relations` 里
    let mut by_name: Map<String, Value> = Map::new();
    for d in &defs {
        let n = d.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        by_name.insert(n, d.clone());
    }
    // 收集反向关系
    let mut rev_rels: Vec<(String, String, Value)> = Vec::new(); // (target_table, rel_name, rel_def)
    for d in &defs {
        let n = d.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let rels = d.get("relations").and_then(|r| r.as_object());
        if let Some(rels) = rels {
            for (k, v) in rels {
                if let Some(t) = k.strip_prefix("__rev_").map(String::from) {
                    rev_rels.push((t, n.clone(), v.clone()));
                }
            }
        }
    }
    for (target, rel_name, rel_def) in rev_rels {
        if let Some(d) = by_name.get_mut(&target) {
            let rels = d.get_mut("relations").and_then(|r| r.as_object_mut());
            if let Some(rels) = rels {
                rels.insert(rel_name, rel_def);
            }
        }
    }
    // 由合并后的 by_name 输出，去掉 `__rev_` 占位
    let mut names: Vec<String> = by_name.keys().cloned().collect();
    names.sort();
    let out: Vec<Value> = names
        .into_iter()
        .map(|n| by_name.get(&n).cloned().unwrap_or(Value::Null))
        .map(|mut d| {
            if let Some(rels) = d.get_mut("relations").and_then(|r| r.as_object_mut()) {
                rels.retain(|k, _| !k.starts_with("__rev_"));
            }
            d
        })
        .collect();
    Ok(Value::Array(out))
}

/// SQL 列类型 → rust-store 字段类型
fn db_field_type(col: &str, db_type: &str) -> &'static str {
    // 附属表识别（object/array 展平）：`<field>_object` / `<field>_list` 后缀由 Host 命名
    if col.ends_with("_object") || db_type.contains("JSON") || db_type.contains("OBJECT") {
        return "object";
    }
    if col.ends_with("_list") || db_type.contains("ARRAY") {
        return "array";
    }
    match db_type {
        t if t.contains("INT") || t.contains("NUMERIC") || t.contains("DECIMAL") || t.contains("REAL") || t.contains("FLOAT") || t.contains("DOUBLE") => "number",
        t if t.contains("BOOL") => "boolean",
        t if t.contains("DATE") || t.contains("TIME") || t.contains("TIMESTAMP") => "date",
        _ => "string",
    }
}

/// 方便绑定层：直接返回已注册集合的 schemaJSON（等价 `schema_def_from_rows`，但归一到 model 名）
pub fn introspect_to_schema_json(rows: &Value, _backend: &Backend) -> Result<Value, String> {
    schema_def_from_rows(rows)
}