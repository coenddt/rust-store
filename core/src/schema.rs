//! Schema 管理（对应 JS `src/schema.js`）
//!
//! 与 JS 版的差异：注册表由模块级全局 `_schemas` 改为显式 `Registry` 实例。

use std::collections::HashMap;

use serde_json::{json, Map, Value};

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
}

impl Schema {
    pub fn compute(&self, name: &str) -> Option<&ComputeDef> {
        self.computes.iter().find(|(k, _)| k == name).map(|(_, c)| c)
    }
}

#[derive(Debug, Default, Clone)]
pub struct Registry {
    schemas: HashMap<String, Schema>,
    order: Vec<String>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个 schema（含自动注册 `<Name>Deleted` 归档表），对应 JS `register`
    pub fn register(&mut self, defn: &Value) -> Result<(), String> {
        let obj = defn
            .as_object()
            .ok_or_else(|| "schema 定义必须是对象".to_string())?;
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "schema 缺少 name".to_string())?
            .to_string();
        let collection = obj
            .get("collection")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| name.clone());
        let timestamps_enabled = obj
            .get("timestamps")
            .map(|v| v != &Value::Bool(false))
            .unwrap_or(true);

        let mut fields = normalize_fields(obj.get("fields"));
        // 自动补时间戳字段（timestamps !== false 时）
        if timestamps_enabled {
            fields
                .entry("createdAt".to_string())
                .or_insert_with(|| FieldDef {
                    field_type: "number".to_string(),
                    required: false,
                    default: None,
                    read: None,
                    write: None,
                    fields: None,
                });
            fields
                .entry("updatedAt".to_string())
                .or_insert_with(|| FieldDef {
                    field_type: "number".to_string(),
                    required: false,
                    default: None,
                    read: None,
                    write: None,
                    fields: None,
                });
        }

        let mut relations = HashMap::new();
        if let Some(Value::Object(rm)) = obj.get("relations") {
            for (key, val) in rm {
                let vo = val.as_object();
                let get_str = |k: &str| -> Option<String> {
                    vo.and_then(|o| o.get(k))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                };
                relations.insert(
                    key.clone(),
                    RelationDef {
                        model: get_str("model").unwrap_or_default(),
                        rel_type: get_str("type").unwrap_or_else(|| "many".to_string()),
                        local_field: get_str("localField").unwrap_or_else(|| "_id".to_string()),
                        foreign_field: get_str("foreignField").unwrap_or_else(|| key.clone()),
                        read: str_list(vo.and_then(|o| o.get("read"))),
                    },
                );
            }
        }

        let mut computes = Vec::new();
        if let Some(Value::Object(cm)) = obj.get("computes") {
            for (key, val) in cm {
                let vo = val.as_object();
                let get = |k: &str| vo.and_then(|o| o.get(k));
                computes.push((
                    key.clone(),
                    ComputeDef {
                        comp_type: get("type")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .unwrap_or_else(|| "any".to_string()),
                        has_fn: get("fn").map(is_truthy).unwrap_or(false),
                        has_async_fn: get("asyncFn").map(is_truthy).unwrap_or(false),
                        fn_ref: get("fnRef")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from),
                        lookup: get("lookup").filter(|v| is_truthy(v)).cloned(),
                        depends: str_list(get("depends")).unwrap_or_default(),
                        read: str_list(get("read")),
                    },
                ));
            }
        }

        let schema = Schema {
            name: name.clone(),
            collection: collection.clone(),
            id_prefix: obj
                .get("idPrefix")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            timestamps: timestamps_enabled,
            fields,
            relations,
            computes,
            read: str_list(obj.get("read")),
            write: str_list(obj.get("write")),
            indexes: obj
                .get("indexes")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
            datasource: obj
                .get("datasource")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
        };

        let is_archive = obj.get("_isArchive").map(is_truthy).unwrap_or(false);
        self.schemas.insert(name.clone(), schema);
        self.order.push(name.clone());

        // 自动注册删除附表 schema —— 每个业务表对应一个 `<collection>_deleted` 归档表
        if !is_archive && !name.ends_with("Deleted") {
            let mut arch_fields = obj
                .get("fields")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            arch_fields.insert("deletedAt".to_string(), json!({ "type": "number" }));
            let mut arch = json!({
                "name": format!("{}Deleted", name),
                "collection": format!("{}_deleted", collection),
                "idPrefix": "",
                "_isArchive": true,
                "fields": Value::Object(arch_fields),
                "indexes": obj.get("indexes").cloned().unwrap_or_else(|| json!([])),
            });
            // 归档表与原表同库
            if let Some(ds) = obj.get("datasource") {
                arch["datasource"] = ds.clone();
            }
            self.register(&arch)?;
        }

        Ok(())
    }

    /// 按名称获取 schema
    pub fn get(&self, name: &str) -> Result<&Schema, String> {
        self.schemas
            .get(name)
            .ok_or_else(|| format!("Schema 未注册: {}", name))
    }

    pub fn has(&self, name: &str) -> bool {
        self.schemas.contains_key(name)
    }

    /// 按 collection 名获取 schema（翻译器等按命令里的 `collection` 反向定位）
    pub fn get_by_collection(&self, collection: &str) -> Result<&Schema, String> {
        self.schemas
            .values()
            .find(|s| s.collection == collection)
            .ok_or_else(|| format!("Schema 未注册（collection = {}）", collection))
    }

    pub fn list(&self) -> Vec<String> {
        self.order.clone()
    }

    /// schema 声明的数据源名（未声明 → `None`，语义为 `default`）
    pub fn schema_datasource(&self, name: &str) -> Result<Option<String>, String> {
        Ok(self.get(name)?.datasource.clone())
    }

    /// 解析 schema 绑定的数据源（`config` 为 `{ "sources": { name: kind } }`）
    ///
    /// 纯逻辑：不持有连接（连接句柄留在 Host）。`null` / 空配置下等价单源 Mongo，
    /// 保证既有调用方与 parity fixture 零变更。见 [`crate::datasource`]。
    pub fn resolve_datasource(
        &self,
        schema_name: &str,
        config: &Value,
    ) -> Result<crate::datasource::DataSource, String> {
        let declared = self.get(schema_name)?.datasource.as_deref();
        let cfg = crate::datasource::DataSourceConfig::from_json(config)?;
        cfg.resolve(declared)
    }
}

/// 规范化 fields 定义（字符串简写 → `{type, required:false}`）
fn normalize_fields(v: Option<&Value>) -> HashMap<String, FieldDef> {
    let mut out = HashMap::new();
    let Some(Value::Object(map)) = v else {
        return out;
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
            FieldDef {
                field_type: String::new(),
                required: false,
                default: None,
                read: None,
                write: None,
                fields: None,
            }
        };
        out.insert(key.clone(), fd);
    }
    out
}

/// 便捷构造：`Map` from `[(&str, Value)]`
pub fn map_of(pairs: Vec<(&str, Value)>) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    m
}
