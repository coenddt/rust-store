//! Schema 注册表：注册/定位/配置（数据结构与规范化见 [`super::definition`]）。

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::types::{is_truthy, str_list};

use super::definition::{normalize_fields, ComputeDef, FieldDef, RelationDef, Schema};

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

        let schema = build_schema(obj, name.clone(), collection.clone())?;

        // 定位三元组唯一性（fail fast，绝不静默串源）：同名覆盖除外
        self.check_location_unique(&name, &schema)?;

        let is_archive = obj.get("_isArchive").map(is_truthy).unwrap_or(false);
        self.schemas.insert(name.clone(), schema);
        self.order.push(name.clone());

        // 自动注册删除附表 schema —— 每个业务表对应一个 `<collection>_deleted` 归档表
        if !is_archive && !name.ends_with("Deleted") {
            self.register(&archive_defn(obj, &name, &collection))?;
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
    /// 开启后：plan 入口遇 `ctx: None` 报 `ERR_NO_CONTEXT`（fail-secure）。
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
            .find(|s| s.collection == collection && s.source() == source && s.ns() == ns)
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

/// 由原始定义构建 [`Schema`]（字段/关系/计算列/元数据分块解析，保持单一职责）
fn build_schema(
    obj: &Map<String, Value>,
    name: String,
    collection: String,
) -> Result<Schema, String> {
    let timestamps = parse_timestamps(obj)?;
    let mut fields = normalize_fields(obj.get("fields"))?;
    if timestamps {
        add_timestamp_fields(&mut fields);
    }
    Ok(Schema {
        name,
        collection,
        id_prefix: obj
            .get("idPrefix")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        timestamps,
        fields,
        relations: parse_relations(obj),
        computes: parse_computes(obj),
        read: str_list(obj.get("read")),
        write: str_list(obj.get("write")),
        indexes: obj
            .get("indexes")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        datasource: opt_nonempty_str(obj.get("datasource")),
        namespace: opt_nonempty_str(obj.get("namespace")),
    })
}

/// `timestamps` 仅接受 true/false/'ms'/'s'；单位换算在 Host（core 无时钟，now 由 Host 传入）
fn parse_timestamps(obj: &Map<String, Value>) -> Result<bool, String> {
    match obj.get("timestamps") {
        None | Some(Value::Null) | Some(Value::Bool(true)) => Ok(true),
        Some(Value::Bool(false)) => Ok(false),
        Some(Value::String(s)) if s == "ms" || s == "s" => Ok(true),
        Some(Value::String(s)) => Err(format!(
            "timestamps 仅支持 true/false/'ms'/'s'，实际 {:?}",
            s
        )),
        Some(other) => Err(format!(
            "timestamps 仅支持 true/false/'ms'/'s'，实际 {}",
            other
        )),
    }
}

/// 自动补时间戳字段（`timestamps !== false` 时；已声明则不覆盖）
fn add_timestamp_fields(fields: &mut HashMap<String, FieldDef>) {
    for key in ["createdAt", "updatedAt"] {
        fields.entry(key.to_string()).or_insert_with(|| FieldDef {
            field_type: "number".to_string(),
            required: false,
            default: None,
            read: None,
            write: None,
            fields: None,
        });
    }
}

fn parse_relations(obj: &Map<String, Value>) -> HashMap<String, RelationDef> {
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
    relations
}

fn parse_computes(obj: &Map<String, Value>) -> Vec<(String, ComputeDef)> {
    let mut computes = Vec::new();
    if let Some(Value::Object(cm)) = obj.get("computes") {
        for (key, val) in cm {
            let vo = val.as_object();
            let get = |k: &str| vo.and_then(|o| o.get(k)).cloned();
            // R8：lookup 计算列的 `<addFields>` 表达式以兄弟键书写，
            // 注册时并入 lookup 对象，供 `build_add_fields` 读取物化表达式。
            let lookup = merge_lookup_add_fields(&get);
            computes.push((
                key.clone(),
                ComputeDef {
                    comp_type: get("type")
                        .and_then(|v| v.as_str().map(String::from))
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "any".to_string()),
                    has_fn: get("fn").map(|v| is_truthy(&v)).unwrap_or(false),
                    has_async_fn: get("asyncFn").map(|v| is_truthy(&v)).unwrap_or(false),
                    fn_ref: get("fnRef")
                        .and_then(|v| v.as_str().map(String::from))
                        .filter(|s| !s.is_empty()),
                    lookup,
                    depends: str_list(get("depends").as_ref()).unwrap_or_default(),
                    read: str_list(get("read").as_ref()),
                },
            ));
        }
    }
    computes
}

/// 取非空字符串字段（空串归一为 `None`）
fn opt_nonempty_str(v: Option<&Value>) -> Option<String> {
    v.and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// lookup 计算列的 `lookup` 值：若计算列以**兄弟键** `addFields` 提供物化表达式，
/// 则并入 lookup 对象（`build_add_fields` 从 lookup 内读 `<addFields>`）。
/// 其它情况原样返回（`lookup` 无效 → None）。
fn merge_lookup_add_fields(get: &impl Fn(&str) -> Option<Value>) -> Option<Value> {
    let lookup = get("lookup").filter(|v| is_truthy(v));
    let Some(Value::Object(mut lo)) = lookup else {
        return lookup;
    };
    let Some(af) = get("addFields").filter(|v| is_truthy(v)) else {
        return Some(Value::Object(lo));
    };
    lo.insert("addFields".to_string(), af);
    Some(Value::Object(lo))
}

/// 构造 `<Name>Deleted` 归档表 schema 定义（字段 = 原字段 + `deletedAt`）；
/// 归档表与原表同库 —— `datasource` 与 `namespace` 一并继承。
fn archive_defn(obj: &Map<String, Value>, name: &str, collection: &str) -> Value {
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
    if let Some(ds) = obj.get("datasource") {
        arch["datasource"] = ds.clone();
    }
    if let Some(ns) = obj.get("namespace") {
        arch["namespace"] = ns.clone();
    }
    arch
}
