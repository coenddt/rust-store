//! Schema 的数据结构与字段规范化（`Registry` 见 [`super::registry`]）。

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::types::{is_truthy, str_list};

#[derive(Debug, Clone)]
pub struct FieldDef {
    pub field_type: String,
    pub required: bool,
    pub default: Option<Value>,
    pub read: Option<Vec<String>>,
    pub write: Option<Vec<String>>,
    /// 嵌套 object 字段的原始定义（未规范化，与 JS 保持一致）
    pub fields: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct RelationDef {
    pub model: String,
    pub rel_type: String,
    pub local_field: String,
    pub foreign_field: String,
    pub read: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct ComputeDef {
    pub comp_type: String,
    pub has_fn: bool,
    pub has_async_fn: bool,
    /// 回调标识：Host 经 FnRegistry 把它绑定到实现；缺省 = 计算列 key 名
    pub fn_ref: Option<String>,
    pub lookup: Option<Value>,
    pub depends: Vec<String>,
    pub read: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct Schema {
    pub name: String,
    pub collection: String,
    pub id_prefix: String,
    pub timestamps: bool,
    pub fields: HashMap<String, FieldDef>,
    pub relations: HashMap<String, RelationDef>,
    /// 保持注册顺序：`build_compute_lookup_stages` 的输出顺序依赖它
    pub computes: Vec<(String, ComputeDef)>,
    pub read: Option<Vec<String>>,
    pub write: Option<Vec<String>>,
    /// 原始索引定义（`[{keys: {...}, options: {...}}]`），upsert 条件构建依赖 unique 索引
    pub indexes: Vec<Value>,
    /// 绑定的数据源名（可选；缺省 = `default`，回落单源 Mongo）。见 [`crate::datasource`]
    pub datasource: Option<String>,
    /// 连接内的库/schema 名（可选；缺省 = `null`，用连接自身默认：Mongo db 实例的库名、
    /// PG 的 search_path、MySQL 的连接库、SQLite 的 main）。
    /// 与 `datasource`/`collection` 构成定位三元组，见 [`super::Registry::get_by_location`]。
    pub namespace: Option<String>,
}

impl Schema {
    pub fn compute(&self, name: &str) -> Option<&ComputeDef> {
        self.computes
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, c)| c)
    }

    /// 解析后的数据源名（缺省 `default`）
    pub fn source(&self) -> &str {
        self.datasource
            .as_deref()
            .unwrap_or(crate::datasource::DEFAULT_SOURCE)
    }

    /// 解析后的 namespace（空串归一为 `None`）
    pub fn ns(&self) -> Option<&str> {
        self.namespace.as_deref().filter(|s| !s.is_empty())
    }
}

/// 规范化 fields 定义（字符串简写 → `{type, required:false}`）
pub(super) fn normalize_fields(v: Option<&Value>) -> Result<HashMap<String, FieldDef>, String> {
    let mut out = HashMap::new();
    let Some(Value::Object(map)) = v else {
        return Ok(out);
    };
    for (key, val) in map {
        let fd = if let Some(s) = val.as_str() {
            FieldDef {
                field_type: s.to_string(),
                required: false,
                default: None,
                read: None,
                write: None,
                fields: None,
            }
        } else if let Some(o) = val.as_object() {
            FieldDef {
                field_type: o
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                required: o.get("required").and_then(|v| v.as_bool()).unwrap_or(false),
                default: o.get("default").filter(|v| !v.is_null()).cloned(),
                read: str_list(o.get("read")),
                write: str_list(o.get("write")),
                fields: o.get("fields").filter(|v| is_truthy(v)).cloned(),
            }
        } else {
            // 非字符串、非对象的定义（如 `fields: { price: 123 }`）= 脏 schema，
            // fail-fast 报错而非静默产出空 field_type（后续 object/array 展平、
            // 标量列判定都会走错且无信号）
            return Err(format!("字段 {} 定义类型非法（须为类型字符串或对象）", key));
        };
        out.insert(key.clone(), fd);
    }
    Ok(out)
}

/// 便捷构造：`Map` from `[(&str, Value)]`
pub fn map_of(pairs: Vec<(&str, Value)>) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    m
}
