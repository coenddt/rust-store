//! Schema 的数据结构与字段规范化（`Registry` 见 [`super::registry`]）。

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::datasource::DEFAULT_SOURCE;
use crate::types::{is_truthy, str_list};

/// 链路角色：读解析时决定链路的优先级层级（`Primary` 最高）。
/// 仅在 `store.config.json` 的 `links` 声明中出现，**禁写入 schema 定义文件**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 主链路：读路径的默认选中落点
    Primary,
    /// 从链路：主不可用时的备选
    Replica,
    /// 缓存链路
    Cache,
    /// 归档链路
    Archive,
}

impl Role {
    /// 非法字符串 ⇒ Err（禁静默回落）
    pub fn parse(s: &str) -> Result<Role, String> {
        match s {
            "primary" => Ok(Role::Primary),
            "replica" => Ok(Role::Replica),
            "cache" => Ok(Role::Cache),
            "archive" => Ok(Role::Archive),
            other => Err(format!(
                "未知链路角色: {other}（仅 primary/replica/cache/archive）"
            )),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Primary => "primary",
            Role::Replica => "replica",
            Role::Cache => "cache",
            Role::Archive => "archive",
        }
    }

    /// 排序键：Primary=0, Replica=1, Cache=2, Archive=3
    pub fn rank(&self) -> u8 {
        match self {
            Role::Primary => 0,
            Role::Replica => 1,
            Role::Cache => 2,
            Role::Archive => 3,
        }
    }
}

/// 落点：定义之外，由「目录语义 + 连接配置」解析得到（设计 §5.2）。
/// 落点维度 = `source` + `database` + `schema`（`schema` 仅 PostgreSQL）；
/// `role` / `priority` 为链路声明附着的可选属性（缺省 `None`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// 连接选择（source 名）；缺省 [`DEFAULT_SOURCE`]
    pub source: String,
    /// 连接内「库」：Mongo db / MySQL database / SQLite attached / PG 库
    pub database: Option<String>,
    /// 连接内「schema」：仅 PostgreSQL
    pub schema: Option<String>,
    /// 链路角色（由 `links` 声明附着；缺省 `None` = 未声明）
    pub role: Option<Role>,
    /// 链路优先级（同 role 内升序；`None` 视作最大；允许负数）
    pub priority: Option<i64>,
}

impl Default for Location {
    fn default() -> Self {
        Self {
            source: DEFAULT_SOURCE.to_string(),
            database: None,
            schema: None,
            role: None,
            priority: None,
        }
    }
}

impl Location {
    /// 落点同址判据（**单点**）：仅比 `(source, database, schema)`，不含 role/priority。
    ///
    /// `PartialEq` 由 derive 覆盖全部字段，故任何「两落点是否同址」的判断一律改用本方法。
    pub fn same_place(&self, other: &Location) -> bool {
        self.source == other.source
            && self.database == other.database
            && self.schema == other.schema
    }
}

#[derive(Debug, Clone)]
pub struct FieldDef {
    pub field_type: String,
    pub required: bool,
    pub default: Option<Value>,
    pub read: Option<Vec<String>>,
    pub write: Option<Vec<String>>,
    /// 嵌套 object 字段的原始定义（未规范化，与 JS 保持一致）
    pub fields: Option<Value>,
    /// ID 供给策略（阶段2）：仅 `_id` 字段有意义 —— `"autoincrement"` = 数据库自增列
    /// 赋值（SQL 后端），宿主不生成、core 不注入 `_id`；Mongo 后端无自增语义，
    /// 执行器对此显式报错（AUTOINCREMENT_NOT_SUPPORTED）。其余值非法（注册期校验）。
    pub strategy: Option<String>,
    /// 向量维度：`field_type == "vector"` 时必填，正整数（VR-01）
    pub dim: Option<u32>,
    /// 向量距离度量：cosine / l2 / ip，缺省 cosine（仅 vector 字段允许出现）
    pub metric: Option<String>,
}

impl FieldDef {
    /// 向量距离度量（缺省 cosine）；非 vector 字段调用无意义
    pub fn metric_or_default(&self) -> &str {
        self.metric.as_deref().unwrap_or("cosine")
    }
}

/// 向量维度统一上限：与 pgvector `vector` 类型上限一致，保证同一 schema 可跨源迁移。
/// 数值源唯一：05 的 `capability::APPLICATION_MAX_DIM` 引用本常量，禁止出现第二个 16000 字面量。
pub const MAX_VECTOR_DIM: u32 = 16000;

/// 注册期向量字段校验（VR-01 / A1）：vector 必带合法 dim；非 vector 不接受 dim / metric。
pub(super) fn validate_vector_fields(fields: &HashMap<String, FieldDef>) -> Result<(), String> {
    for (name, f) in fields {
        let is_vector = f.field_type == "vector";
        if is_vector {
            let dim = match f.dim {
                Some(d) => d,
                None => {
                    return Err(format!(
                        "字段 {name} 类型为 vector，但缺少必填 dim（正整数）"
                    ))
                }
            };
            if dim > MAX_VECTOR_DIM {
                return Err(format!(
                    "字段 {name} 的 dim 超过上限：{dim} > {MAX_VECTOR_DIM}"
                ));
            }
        } else {
            if f.dim.is_some() {
                return Err(format!(
                    "字段 {name} 类型为 {}，不接受 dim（仅 vector 允许）",
                    f.field_type
                ));
            }
            if f.metric.is_some() {
                return Err(format!(
                    "字段 {name} 类型为 {}，不接受 metric（仅 vector 允许）",
                    f.field_type
                ));
            }
        }
    }
    Ok(())
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
    /// 归一聚合算子（§9.2(2)）：`{"$count": "lessons"}` / `{"$sum": "lessons.duration"}`。
    /// 取代旧的 `lookup` + `addFields` Mongo 专用形态；SQL / Mongo 走同一语法（执行下推优先）。
    pub agg: Option<Value>,
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
    /// 保持注册顺序：`build_agg_stages` 的输出顺序依赖它
    pub computes: Vec<(String, ComputeDef)>,
    /// 写触发器声明（事件 → 触发列表；解析见 `super::triggers`，规划见 `crate::command::triggers`）
    pub triggers: super::triggers::Triggers,
    pub read: Option<Vec<String>>,
    pub write: Option<Vec<String>>,
    /// 原始索引定义（`[{keys: {...}, options: {...}}]`），upsert 条件构建依赖 unique 索引
    pub indexes: Vec<Value>,
    /// 数据源名（由 `register_batch` 的 [`Location`] 注入；`None` = 缺省 `default`）。
    /// 见 [`crate::datasource`]
    pub source: Option<String>,
    /// 连接内「库」（由 `register_batch` 注入；`None` = 连接默认：Mongo db 实例的库名、
    /// PG 的库、MySQL 的连接库、SQLite 的 main）。
    pub database: Option<String>,
    /// 连接内「schema」（仅 PG；由 `register_batch` 注入；`None` = 连接默认 search_path）。
    pub schema: Option<String>,
    /// 归属键（creator 伪角色判定 / owner-condition 注入 / update-remove 所有权检查
    /// 读取的文档字段名）。`None` = 缺省 `"createdBy"`（零破坏）；空串在注册期拦截。
    pub owner_key: Option<String>,
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
        self.source
            .as_deref()
            .unwrap_or(crate::datasource::DEFAULT_SOURCE)
    }

    /// 解析后的 database（空串归一为 `None`）
    pub fn database(&self) -> Option<&str> {
        self.database.as_deref().filter(|s| !s.is_empty())
    }

    /// 解析后的 schema（空串归一为 `None`；仅 PG）
    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref().filter(|s| !s.is_empty())
    }

    /// 主落点（由 `source`/`database`/`schema` 组装）。
    ///
    /// 等价于 `Registry::primary_location(name)` 的投影；链路缺失时的回落。
    pub fn location(&self) -> Location {
        Location {
            source: self.source().to_string(),
            database: self.database().map(String::from),
            schema: self.schema().map(String::from),
            role: None,
            priority: None,
        }
    }

    /// `_id` 是否声明 `strategy: "autoincrement"`（阶段2：数据库自增主键）
    pub fn id_is_autoincrement(&self) -> bool {
        self.fields.get("_id").and_then(|f| f.strategy.as_deref()) == Some("autoincrement")
    }

    /// 归属键（creator 伪角色判定 / 注入 / 所有权检查三处统一经此取键，禁止
    /// 调用点直接写 `createdBy` 字面量）。缺省 `"createdBy"`（零破坏）；
    /// `ownerKey` 空串视为非法（注册期已拦截，此处不兜底）。
    pub fn owner_key(&self) -> &str {
        self.owner_key.as_deref().unwrap_or("createdBy")
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
                strategy: None,
                dim: None,
                metric: None,
            }
        } else if let Some(o) = val.as_object() {
            // 向量属性必须显式提取：未提取的键会被静默丢弃（VR-01 / 取证 3.5）
            let dim = match o.get("dim") {
                None | Some(Value::Null) => None,
                Some(Value::Number(n)) => Some(
                    n.as_u64()
                        .filter(|v| *v > 0 && *v <= u32::MAX as u64)
                        .map(|v| v as u32)
                        .ok_or_else(|| format!("字段 {key} 的 dim 必须是正整数，实际 {n}"))?,
                ),
                Some(other) => return Err(format!("字段 {key} 的 dim 必须是正整数，实际 {other}")),
            };
            let metric = match o.get("metric") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if ["cosine", "l2", "ip"].contains(&s.as_str()) => {
                    Some(s.clone())
                }
                Some(other) => {
                    return Err(format!(
                        "字段 {key} 的 metric 仅支持 cosine/l2/ip，实际 {other}"
                    ))
                }
            };
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
                strategy: match o.get("strategy").and_then(|v| v.as_str()) {
                    None => None,
                    Some(v @ "autoincrement") => Some(v.to_string()),
                    // no-error-masking：未知策略显式拒绝，禁止静默吞掉后按默认行为跑
                    Some(other) => {
                        return Err(format!(
                            "字段 {key} 的 strategy \"{other}\" 非法（仅支持 \"autoincrement\"，且仅限 _id 字段）"
                        ))
                    }
                },
                dim,
                metric,
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
