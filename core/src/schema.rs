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
    /// 连接内的库/schema 名（可选；缺省 = `null`，用连接自身默认：Mongo db 实例的库名、
    /// PG 的 search_path、MySQL 的连接库、SQLite 的 main）。
    /// 与 `datasource`/`collection` 构成定位三元组，见 [`Registry::get_by_location`]。
    pub namespace: Option<String>,
}

impl Schema {
    pub fn compute(&self, name: &str) -> Option<&ComputeDef> {
        self.computes.iter().find(|(k, _)| k == name).map(|(_, c)| c)
    }

    /// 解析后的数据源名（缺省 `default`）
    pub fn source(&self) -> &str {
        self.datasource.as_deref().unwrap_or(crate::datasource::DEFAULT_SOURCE)
    }

    /// 解析后的 namespace（空串归一为 `None`）
    pub fn ns(&self) -> Option<&str> {
        self.namespace.as_deref().filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone)]
pub struct Registry {
    schemas: HashMap<String, Schema>,
    order: Vec<String>,
    /// 用户 `$pipeline` 直通开关（默认允许；AI 查询宿主可关闭作纵深防御）
    pub allow_user_pipeline: bool,
    /// 上下文强制开关（默认关闭 = fail-open，与 JS 原版 parity）；
    /// 开启后所有 plan 入口对 `ctx: None` 显式报错（fail-secure，见 `permission` 模块文档）。
    /// 内部调用请传显式系统上下文（JSON `{"internal": true}` / `Context::system()`）。
    require_context: bool,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            schemas: HashMap::new(),
            order: Vec::new(),
            allow_user_pipeline: true,
            require_context: false,
        }
    }
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
        // timestamps 仅接受 true/false/'ms'/'s'；单位换算在 Host（core 无时钟，now 由 Host 传入）
        let timestamps_enabled = match obj.get("timestamps") {
            None | Some(Value::Null) | Some(Value::Bool(true)) => true,
            Some(Value::Bool(false)) => false,
            Some(Value::String(s)) if s == "ms" || s == "s" => true,
            Some(Value::String(s)) => {
                return Err(format!("timestamps 仅支持 true/false/'ms'/'s'，实际 {:?}", s))
            }
            Some(other) => {
                return Err(format!("timestamps 仅支持 true/false/'ms'/'s'，实际 {}", other))
            }
        };

        let mut fields = normalize_fields(obj.get("fields"))?;
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
            namespace: obj
                .get("namespace")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
        };

        // 定位三元组唯一性（fail fast，绝不静默串源）：同名覆盖除外
        self.check_location_unique(&name, &schema)?;

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
            // 归档表与原表同库：datasource 与 namespace 一并继承
            if let Some(ds) = obj.get("datasource") {
                arch["datasource"] = ds.clone();
            }
            if let Some(ns) = obj.get("namespace") {
                arch["namespace"] = ns.clone();
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

    /// 开关用户 `$pipeline` 直通（默认允许；AI 查询宿主建议关闭作纵深防御）
    pub fn set_allow_user_pipeline(&mut self, allow: bool) {
        self.allow_user_pipeline = allow;
    }

    /// 开关「上下文强制」（默认关闭 = fail-open，保持 JS parity）。
    /// 开启后：plan 入口遇 `ctx: None` 报 `ERR_NO_CONTEXT`（fail-secure），
    /// 内部调用须显式传系统上下文（`{"internal": true}`）。
    pub fn set_require_context(&mut self, require: bool) {
        self.require_context = require;
    }

    /// 「上下文强制」开关当前值
    pub fn require_context(&self) -> bool {
        self.require_context
    }

    /// 按定位三元组精确获取 schema（命令路由的唯一定位入口）
    ///
    /// `(source, namespace, collection)` 三元组在 Registry 内唯一（注册期校验），
    /// 故此处零候选 = 未注册；一候选 = 精确命中。**不做任何猜测回落**。
    pub fn get_by_location(
        &self,
        source: &str,
        namespace: Option<&str>,
        collection: &str,
    ) -> Result<&Schema, String> {
        let ns = namespace.filter(|s| !s.is_empty());
        self.schemas
            .values()
            .find(|s| {
                s.collection == collection
                    && s.source() == source
                    && s.ns() == ns
            })
            .ok_or_else(|| {
                format!(
                    "Schema 未定位（source = {}, namespace = {:?}, collection = {}）",
                    source, ns, collection
                )
            })
    }

    /// 命令结构定位（方言翻译用）：优先三元组精确命中；未命中时（`route_override`
    /// 改写 namespace 的多租户场景）回落 `(source, collection)` 定位 —— 结构 schema
    /// 与定位 namespace 正交（见 multi-datasource-routing-plan.md §6：权限/字段
    /// 校验仍按结构 schema）。
    ///
    /// `(source, collection)` 恰一候选 → 命中；多候选时取 `namespace = null` 的
    /// 结构声明，无 null 声明 → 报错（拒绝猜测，铁律 3）。
    pub fn get_for_command(
        &self,
        source: &str,
        namespace: Option<&str>,
        collection: &str,
    ) -> Result<&Schema, String> {
        if let Ok(s) = self.get_by_location(source, namespace, collection) {
            return Ok(s);
        }
        let candidates: Vec<&Schema> = self
            .schemas
            .values()
            .filter(|s| s.collection == collection && s.source() == source)
            .collect();
        match candidates.len() {
            0 => self.get_by_location(source, namespace, collection),
            1 => Ok(candidates[0]),
            _ => candidates
                .iter()
                .find(|s| s.ns().is_none())
                .copied()
                .ok_or_else(|| {
                    format!(
                        "命令定位 ({}, {:?}, {}) 存在多个结构 schema（多租户 override 需唯一的 (source, collection) 结构声明）",
                        source, namespace, collection
                    )
                }),
        }
    }

    /// 定位三元组唯一性校验：不同 schema 名占用同一 `(source, namespace, collection)`
    /// → Err（同名覆盖 = 更新语义，放行）。
    fn check_location_unique(&self, name: &str, schema: &Schema) -> Result<(), String> {
        let conflict = self.schemas.iter().find(|(n, s)| {
            n.as_str() != name
                && s.collection == schema.collection
                && s.source() == schema.source()
                && s.ns() == schema.ns()
        });
        match conflict {
            None => Ok(()),
            Some((other, s)) => Err(format!(
                "定位三元组冲突: ({}, {:?}, {}) 已被 schema `{}`（collection = {}）占用",
                schema.source(),
                schema.ns(),
                schema.collection,
                other,
                s.collection
            )),
        }
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
fn normalize_fields(v: Option<&Value>) -> Result<HashMap<String, FieldDef>, String> {
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
