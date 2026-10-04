//! Schema 注册表：注册/定位/配置（数据结构与规范化见 [`super::definition`]）。

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::types::{is_truthy, str_list, AGG_OPS};

use super::definition::{normalize_fields, ComputeDef, FieldDef, Location, RelationDef, Schema};

use crate::command::WriteLinkPolicy;

/// 查询档位：判决唯一在 core（照 [`Registry::require_context`] 既有范式）。
///
/// - [`Profile::Standard`]（默认）：标准调用 —— 跨方言对齐的公共能力集；
///   DB 独有能力可用但须代码注释标注「不建议业务查询」（见执行文档 §4.5）。
/// - [`Profile::Text2Query`]：AI 问数 —— 功能收缩 + 硬性限制（行数/深度/准入全收紧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// 标准调用：跨方言对齐的公共能力集；DB 独有能力可用但须注释标注（不建议业务查询）
    #[default]
    Standard,
    /// AI 问数：功能收缩 + 硬性限制
    Text2Query,
}

impl Profile {
    /// 档位字符串（Host / 绑定层单点取用）
    pub fn as_str(&self) -> &'static str {
        match self {
            Profile::Standard => "standard",
            Profile::Text2Query => "text2query",
        }
    }

    /// 字符串 → 档位；未知值 **Err**（禁静默回落 `Standard`，见执行文档 §7）
    pub fn from_str_or_err(s: &str) -> Result<Profile, String> {
        match s {
            "standard" => Ok(Profile::Standard),
            "text2query" => Ok(Profile::Text2Query),
            other => Err(format!("未知 profile: {other}（仅 standard / text2query）")),
        }
    }
}

/// 一条注册项：一份主结构 + 主/从链路 + 版本（D4/D5/D13）。
#[derive(Debug, Clone)]
struct Entry {
    schema: Schema,
    /// 主 + 从链路；`links[0]` 恒为主落点（= `schema` 的定位投影）
    links: Vec<Location>,
    /// 版本：首次注册 = 1；跨批次 reload 同名 ⇒ +1
    version: u64,
}

/// 批次分组结果（判据单点输出；`register_batch` 与 `schema::load::plan_load` 共用）。
pub(super) struct BatchClass {
    /// 组名（按首次出现顺序）
    pub order: Vec<String>,
    /// name → (主定义, 落点, collection)；每组至多一份主
    pub primaries: HashMap<String, (Value, Location, String)>,
    /// name → 从项（原序）
    pub replicas: HashMap<String, Vec<(Value, Location)>>,
}

/// 批次判据错误（调用方各自格式化文案：注册期与装载期文案口径不同）。
pub(super) enum BatchError {
    /// 解析错误（非对象 / 缺 name）
    Parse(String),
    /// 同名主 ≥2（A1）：组名 + 主项在输入中的下标
    PrimaryDuplicate { name: String, indices: Vec<usize> },
    /// 批内四元组冲突
    LocationConflict {
        collection: String,
        prev: String,
        name: String,
        source: String,
        database: Option<String>,
        schema: Option<String>,
    },
}

/// 批次判据单点：**分组 + 主唯一（≥2 ⇒ `Err`）+ 批内四元组冲突（⇒ `Err`）**。
///
/// **0 主不在此报错**（交由调用方决定：`register_batch` 走既有 entry 追加从链路，
/// `plan_load` 直接拒——「主定义缺失」）。`register_batch` 与 `plan_load` 共用本函数，
/// 禁各写一份（判据单点，防 A1/A3 漂移）。
pub(super) fn classify_batch(items: &[(Value, Location)]) -> Result<BatchClass, BatchError> {
    let mut order: Vec<String> = Vec::new();
    let mut primaries: HashMap<String, (Value, Location, String)> = HashMap::new();
    let mut primary_idx: HashMap<String, usize> = HashMap::new();
    let mut replicas: HashMap<String, Vec<(Value, Location)>> = HashMap::new();

    for (idx, (defn, loc)) in items.iter().enumerate() {
        let obj = defn
            .as_object()
            .ok_or_else(|| BatchError::Parse("schema 定义必须是对象".to_string()))?;
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| BatchError::Parse("schema 缺少 name".to_string()))?
            .to_string();
        if !order.iter().any(|n| n == &name) {
            order.push(name.clone());
        }

        if obj.get("replica").map(is_truthy).unwrap_or(false) {
            replicas
                .entry(name)
                .or_default()
                .push((defn.clone(), loc.clone()));
            continue;
        }

        let collection = obj
            .get("collection")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| name.clone());
        if let Some(first) = primary_idx.get(&name) {
            // 同名主 ≥2 ⇒ Err（A1）；携带下标供调用方定位（rel 路径 / 提示）
            return Err(BatchError::PrimaryDuplicate {
                name,
                indices: vec![*first, idx],
            });
        }
        primary_idx.insert(name.clone(), idx);
        primaries.insert(name, (defn.clone(), loc.clone(), collection));
    }

    // 批内四元组冲突：主 + 从全部链路（判据函数 `detect_location_conflict`）
    let mut quads: Vec<(String, String, Location)> = Vec::new();
    for name in &order {
        if let Some((_, loc, coll)) = primaries.get(name) {
            quads.push((name.clone(), coll.clone(), loc.clone()));
        }
        if let Some(rs) = replicas.get(name) {
            for (defn, loc) in rs {
                let coll = defn
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(name)
                    .to_string();
                quads.push((name.clone(), coll, loc.clone()));
            }
        }
    }
    if let Some(c) = detect_location_conflict(&quads) {
        return Err(BatchError::LocationConflict {
            collection: c.collection,
            prev: c.prev,
            name: c.name,
            source: c.source,
            database: c.database,
            schema: c.schema,
        });
    }

    Ok(BatchClass {
        order,
        primaries,
        replicas,
    })
}

/// 四元组冲突信息（供调用方格式化文案）
pub(super) struct LocationConflict {
    pub collection: String,
    pub prev: String,
    pub name: String,
    pub source: String,
    pub database: Option<String>,
    pub schema: Option<String>,
}

/// 四元组 `(source, database, schema, collection)` 冲突检测（**判据单点**）：
/// 同一四元组被两个不同 schema 名占用 ⇒ `Some(冲突)`。`register_batch`（含既有 entry）
/// 与 `plan_load`（批内）共用。
pub(super) fn detect_location_conflict(
    entries: &[(String, String, Location)],
) -> Option<LocationConflict> {
    let mut seen: HashMap<(String, Option<String>, Option<String>, String), String> = HashMap::new();
    for (name, collection, loc) in entries {
        let key = (
            loc.source.clone(),
            loc.database.clone(),
            loc.schema.clone(),
            collection.clone(),
        );
        if let Some(prev) = seen.get(&key) {
            if prev != name {
                return Some(LocationConflict {
                    collection: collection.clone(),
                    prev: prev.clone(),
                    name: name.clone(),
                    source: loc.source.clone(),
                    database: loc.database.clone(),
                    schema: loc.schema.clone(),
                });
            }
        } else {
            seen.insert(key, name.clone());
        }
    }
    None
}

/// `BatchError` → 注册期文案（保留既有子串：`同名主定义重复` / `定位冲突`）。
fn batch_err_to_msg(e: BatchError) -> String {
    match e {
        BatchError::Parse(m) => m,
        BatchError::PrimaryDuplicate { name, .. } => format!(
            "同名主定义重复: {}（同一装载批次内 name 必须唯一，从定义请用 replica: true）",
            name
        ),
        BatchError::LocationConflict {
            collection,
            prev,
            source,
            database,
            schema,
            ..
        } => format!(
            "定位冲突: ({}, {:?}, {:?}, {}) 已被 schema `{}` 占用",
            source, database, schema, collection, prev
        ),
    }
}

#[derive(Debug, Clone, Default)]
pub struct Registry {
    entries: HashMap<String, Entry>,
    order: Vec<String>,
    /// 上下文强制开关（默认关闭 = fail-open，与 JS 原版 parity）；
    /// 开启后所有 plan 入口对 `ctx: None` 显式报错（fail-secure，见 `permission` 模块文档）。
    /// 内部调用请传显式系统上下文（JSON `{"internal": true}` / `Context::system()`）。
    require_context: bool,
    /// 查询档位（默认 [`Profile::Standard`]；text2query 由 AI 问数链路显式进入）。
    profile: Profile,
    /// RBAC 动态策略（`None` = 未启用，判决原语直通、行为与现状一致）。见 [`crate::rbac`]
    rbac: Option<crate::rbac::RbacPolicy>,
    /// 角色清单与未配置姿态（豁免 / 拒写 / Open|Closed，默认 []/[]/Open）。见 [`crate::permission::RoleRules`]
    role_rules: crate::permission::RoleRules,
    /// 定义层门禁策略（默认 Open —— 全放行，保持既有 parity）。见 [`crate::permission::MetaPolicy`]
    meta_policy: crate::permission::MetaPolicy,
    /// 跨连接写策略（默认 [`WriteLinkPolicy::Reject`]）。见 [`crate::command::write_links`]
    write_link_policy: WriteLinkPolicy,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个 schema（含自动注册 `<Name>Deleted` 归档表），对应 JS `register`。
    ///
    /// 兼容入口：单条 + 缺省落点（`source="default"`、`database/schema=None`）。
    /// 定义层门禁按当前 [`MetaPolicy`](crate::permission::MetaPolicy) 判决
    /// （默认 Open → 全放行）。需带 ctx 显式过门禁请用 [`Self::register_with_ctx`]。
    pub fn register(&mut self, defn: &Value) -> Result<(), String> {
        self.register_with_ctx(defn, None)
    }

    /// 带 ctx 的注册（单条 + 缺省落点）：内部转 [`Self::register_batch`]。
    pub fn register_with_ctx(
        &mut self,
        defn: &Value,
        ctx: Option<&crate::permission::Context>,
    ) -> Result<(), String> {
        self.register_batch(&[(defn.clone(), Location::default())], ctx)
    }

    /// 带定位的批量注册（D13：唯一性校验落「装载批次集合」）。
    ///
    /// 算法（**先构造后落库，任一步 Err ⇒ 零副作用**）：
    /// 1. 判据单点 [`classify_batch`]（分组 / 主唯一 ≥2 ⇒ `Err` / 批内四元组冲突），与
    ///    `schema::load::plan_load` 同源。
    /// 2. `can_register` 门禁（判决先于 `build_schema`）。
    /// 3. 逐组：有主 ⇒ `build_schema` 为 **主**；0 主 ⇒ 追加既有同名 entry 的从链路，无既有 ⇒ `Err`。
    /// 4. 全批 + 既有 `(source, database, schema, collection)` 跨名冲突 ⇒ `Err`
    ///    （判据函数 [`detect_location_conflict`]，替代旧 `check_location_unique`）。
    /// 5. 提交：`links = [primary.loc] ++ replicas`；命中既有 entry ⇒ `version + 1`
    ///    （`order` 位置不变）、替换结构与链路；新名 ⇒ `version = 1`、`order.push`。
    ///    `Schema.source/database/schema` 由 `primary.loc` 注入（`source == "default"` ⇒ `None`）。
    /// 6. 归档派生：主 `name` 不以 `"Deleted"` 结尾且非 `_isArchive` ⇒ 派生 `<Name>Deleted`
    ///    （`links` 继承主链路，定位同主）。
    pub fn register_batch(
        &mut self,
        items: &[(Value, Location)],
        ctx: Option<&crate::permission::Context>,
    ) -> Result<(), String> {
        // ── 1：判据单点（分组 / 主唯一 / 批内四元组冲突），与 plan_load 同源 ──
        let class = classify_batch(items).map_err(batch_err_to_msg)?;

        // 定义层门禁：判决先于 build_schema —— 拒绝即返回，绝不部分写入
        for (defn, _loc) in items {
            let name = defn.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if !crate::permission::can_register(&self.meta_policy, ctx) {
                return Err(format!("ERR_PERMISSION: 无权注册或覆盖定义 {name}"));
            }
        }

        // ── 2：逐组拟定 entry 序列（主 + 归档附表，保序） ──
        struct Planned {
            name: String,
            schema: Schema,
            links: Vec<Location>,
        }
        let mut planned: Vec<Planned> = Vec::new();
        for name in &class.order {
            let rs: Vec<Location> = class
                .replicas
                .get(name)
                .map(|v| v.iter().map(|(_, l)| l.clone()).collect())
                .unwrap_or_default();
            if let Some((raw, loc, collection)) = class.primaries.get(name) {
                let obj = raw.as_object().unwrap();
                let schema = build_schema(obj, name.clone(), collection.clone(), loc)?;
                let is_archive = obj.get("_isArchive").map(is_truthy).unwrap_or(false);
                let mut links = vec![loc.clone()];
                links.extend(rs);
                planned.push(Planned {
                    name: name.clone(),
                    schema: schema.clone(),
                    links: links.clone(),
                });
                // 归档附表紧随主定义派生（`links` 继承主链路，定位同主）
                if !is_archive && !name.ends_with("Deleted") {
                    let aobj = archive_defn(obj, name, &schema.collection)
                        .as_object()
                        .cloned()
                        .unwrap();
                    let aname = format!("{}Deleted", name);
                    let acoll = format!("{}_deleted", schema.collection);
                    let aschema = build_schema(&aobj, aname.clone(), acoll, &links[0])?;
                    planned.push(Planned {
                        name: aname,
                        schema: aschema,
                        links,
                    });
                }
            } else {
                // 0 主：只能是对既有 entry 追加从链路（跨批演进）
                let Some(existing) = self.entries.get(name) else {
                    return Err(format!(
                        "未找到主 schema: {}（从定义 `replica` 必须伴随同批主定义，或该 name 已注册）",
                        name
                    ));
                };
                let mut links = existing.links.clone();
                links.extend(rs);
                planned.push(Planned {
                    name: name.clone(),
                    schema: existing.schema.clone(),
                    links,
                });
            }
        }

        // ── 3：四元组唯一性（本批 + 既有，跨名冲突即 Err；判据函数单点） ──
        let batch_names: std::collections::HashSet<&str> =
            planned.iter().map(|p| p.name.as_str()).collect();
        let mut quads: Vec<(String, String, Location)> = Vec::new();
        for (n, e) in &self.entries {
            if batch_names.contains(n.as_str()) {
                continue; // 本批将整体替换该名，旧链路不参与冲突判定
            }
            for l in &e.links {
                quads.push((n.clone(), e.schema.collection.clone(), l.clone()));
            }
        }
        for p in &planned {
            for l in &p.links {
                quads.push((p.name.clone(), p.schema.collection.clone(), l.clone()));
            }
        }
        if let Some(c) = detect_location_conflict(&quads) {
            return Err(format!(
                "定位冲突: ({}, {:?}, {:?}, {}) 已被 schema `{}` 占用",
                c.source, c.database, c.schema, c.collection, c.prev
            ));
        }

        // ── 4：提交（命中既有 ⇒ version + 1 且 order 位置不变；新名 ⇒ version = 1 + push） ──
        for p in planned {
            if let Some(e) = self.entries.get_mut(&p.name) {
                e.schema = p.schema;
                e.links = p.links;
                e.version += 1;
            } else {
                self.entries.insert(
                    p.name.clone(),
                    Entry {
                        schema: p.schema,
                        links: p.links,
                        version: 1,
                    },
                );
                self.order.push(p.name);
            }
        }

        Ok(())
    }

    /// 主链路 + 从链路（只读）
    pub fn links_of(&self, name: &str) -> Result<&[Location], String> {
        Ok(&self.get_entry(name)?.links)
    }

    /// 主落点（= `links[0]`）
    pub fn primary_location(&self, name: &str) -> Result<&Location, String> {
        self.get_entry(name)?
            .links
            .first()
            .ok_or_else(|| format!("Schema 无落点链路: {}", name))
    }

    /// 版本（首次 = 1）
    pub fn version_of(&self, name: &str) -> Result<u64, String> {
        Ok(self.get_entry(name)?.version)
    }

    fn get_entry(&self, name: &str) -> Result<&Entry, String> {
        self.entries
            .get(name)
            .ok_or_else(|| format!("Schema 未注册: {}", name))
    }

    /// 按名称获取 schema
    pub fn get(&self, name: &str) -> Result<&Schema, String> {
        self.get_entry(name).map(|e| &e.schema)
    }

    pub fn has(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// 清空 schema 注册表（`schemas` + `order`），对应绑定层的测试隔离 / 动态重建场景。
    ///
    /// 只清 schema，**不动**配置开关（`require_context` / `profile` / `rbac` /
    /// `role_rules`）与回调表（`clear_fns` 对称：各清各的）——开关生命周期属
    /// Registry 配置面，不随 schema 集合重建而丢。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
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

    /// 设置查询档位（`standard` / `text2query`）。
    ///
    /// 档位为**单值状态**（非栈）：由 Host 的 `text2query()` 上下文管理器负责
    /// 进入时设档、退出时恢复（见 `py-store` / `nodejs-store` 门面）。判决一律在 core。
    pub fn set_profile(&mut self, profile: Profile) {
        self.profile = profile;
    }

    /// 当前查询档位
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// 豁免角色清单：命中者在一切判决环节（静态 + RBAC）直接放行。默认空——无豁免。
    pub fn set_exempt_roles(&mut self, roles: Vec<String>) {
        self.role_rules.exempt_roles = roles;
    }

    /// 拒写角色清单：命中者一切写路径拒绝（读不受影响）。默认空——无拒写。
    pub fn set_deny_write_roles(&mut self, roles: Vec<String>) {
        self.role_rules.deny_write_roles = roles;
    }

    /// schema 白名单缺失/为空时的默认姿态。默认 Open（保持现状语义）。
    pub fn set_unconfigured_policy(&mut self, policy: crate::permission::UnconfiguredPolicy) {
        self.role_rules.unconfigured = policy;
    }

    /// 当前角色规则（静态判决函数与 RBAC decide 的共用取参入口）
    pub fn role_rules(&self) -> &crate::permission::RoleRules {
        &self.role_rules
    }

    /// 定义层门禁策略（`closed=true` 时仅 internal 或 `roles` 白名单可注册/覆盖）。
    /// 默认 Open —— 全放行（保持既有 parity；须在首次业务注册前调用方生效于该次注册）。
    pub fn set_meta_policy(&mut self, closed: bool, roles: Vec<String>) {
        self.meta_policy = crate::permission::MetaPolicy { closed, roles };
    }

    /// 当前定义层门禁策略
    pub fn meta_policy(&self) -> &crate::permission::MetaPolicy {
        &self.meta_policy
    }

    /// 设置跨连接写策略（默认 [`WriteLinkPolicy::Reject`]）。
    ///
    /// 判决唯一在 core（[`crate::command::write_links::resolve_write_links`]）；
    /// 宿主仅需在初始化期把它透传到 registry。
    pub fn set_write_link_policy(&mut self, policy: WriteLinkPolicy) {
        self.write_link_policy = policy;
    }

    /// 当前跨连接写策略
    pub fn write_link_policy(&self) -> WriteLinkPolicy {
        self.write_link_policy
    }

    /// 只读定义层判决（`workflow` 等宿主侧定义面复用同一门禁；判决唯一在 core）。
    ///
    /// 不改变任何注册状态：Open 全放行；Closed 仅 `internal` 或 `roles` 白名单角色。
    /// 语义与 [`Self::register_with_ctx`] 的门禁判据**完全一致**（同一
    /// [`can_register`](crate::permission::can_register)），仅作只读暴露。
    pub fn can_register(&self, ctx: Option<&crate::permission::Context>) -> bool {
        crate::permission::can_register(&self.meta_policy, ctx)
    }

    /// 注入/清除 RBAC 动态策略；`None` = 关闭（判决原语直通）。
    /// 解析失败 Err（fail-fast，禁静默吞配置错误），成功后判决链路即刻生效。
    pub fn set_rbac(&mut self, policy: Option<&Value>) -> Result<(), String> {
        match policy {
            None => {
                self.rbac = None;
                Ok(())
            }
            Some(v) => {
                let p = crate::rbac::RbacPolicy::from_json(v)?;
                self.rbac = Some(p);
                Ok(())
            }
        }
    }

    /// 当前 RBAC 策略（`None` = 未启用）
    pub fn rbac(&self) -> Option<&crate::rbac::RbacPolicy> {
        self.rbac.as_ref()
    }

    /// 按定位四元组精确获取 schema（命令路由的唯一定位入口）
    ///
    /// 命中判据：`collection` 相等 **且** 四元组命中该 entry 的**任一链路**。
    /// **不做任何猜测回落**。
    pub fn get_by_location(
        &self,
        source: &str,
        database: Option<&str>,
        schema: Option<&str>,
        collection: &str,
    ) -> Result<&Schema, String> {
        let db = database.filter(|s| !s.is_empty());
        let sc = schema.filter(|s| !s.is_empty());
        self.entries
            .values()
            .find(|e| {
                e.schema.collection == collection
                    && e.links.iter().any(|l| {
                        l.source == source
                            && l.database.as_deref().filter(|s| !s.is_empty()) == db
                            && l.schema.as_deref().filter(|s| !s.is_empty()) == sc
                    })
            })
            .map(|e| &e.schema)
            .ok_or_else(|| {
                format!(
                    "Schema 未定位（source = {}, database = {:?}, schema = {:?}, collection = {}）",
                    source, db, sc, collection
                )
            })
    }

    /// 命令结构定位（方言翻译用）：优先四元组精确命中；未命中时（`route_override`
    /// 改写落点的多租户场景）回落 `(source, collection)` 定位 —— 结构 schema
    /// 与定位落点正交（见 multi-datasource-routing-plan.md §6：权限/字段校验仍按结构 schema）。
    ///
    /// `(source, collection)` 恰一候选 → 命中；多候选时取 `database`/`schema` 均 `null` 的
    /// 结构声明，无该声明 → 报错（拒绝猜测，铁律 3）。
    pub fn get_for_command(
        &self,
        source: &str,
        database: Option<&str>,
        schema: Option<&str>,
        collection: &str,
    ) -> Result<&Schema, String> {
        if let Ok(s) = self.get_by_location(source, database, schema, collection) {
            return Ok(s);
        }
        let candidates: Vec<&Entry> = self
            .entries
            .values()
            .filter(|e| {
                e.schema.collection == collection && e.links.iter().any(|l| l.source == source)
            })
            .collect();
        match candidates.len() {
            0 => self.get_by_location(source, database, schema, collection),
            1 => Ok(&candidates[0].schema),
            _ => candidates
                .iter()
                .find(|e| e.schema.database().is_none() && e.schema.schema().is_none())
                .map(|e| &e.schema)
                .ok_or_else(|| {
                    format!(
                        "命令定位 ({}, {:?}, {:?}, {}) 存在多个结构 schema（多租户 override 需唯一的 (source, collection) 结构声明）",
                        source, database, schema, collection
                    )
                }),
        }
    }

    pub fn list(&self) -> Vec<String> {
        self.order.clone()
    }

    /// schema 绑定的数据源名（= 主链路 source；`default` → `None`）
    pub fn schema_datasource(&self, name: &str) -> Result<Option<String>, String> {
        Ok(self.get(name)?.source.clone())
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
        let declared = self.get(schema_name)?.source.as_deref();
        let cfg = crate::datasource::DataSourceConfig::from_json(config)?;
        cfg.resolve(declared)
    }
}

/// 由原始定义构建 [`Schema`]（字段/关系/计算列/元数据分块解析，保持单一职责）。
/// 落点（`source`/`database`/`schema`）由 `loc` 注入（定义文件零落点）。
fn build_schema(
    obj: &Map<String, Value>,
    name: String,
    collection: String,
    loc: &Location,
) -> Result<Schema, String> {
    let timestamps = parse_timestamps(obj)?;
    let mut fields = normalize_fields(obj.get("fields"))?;
    if timestamps {
        add_timestamp_fields(&mut fields);
    }
    let relations = parse_relations(obj);
    // 计算列的 agg 需要校验「引用的关系存在」→ 必须晚于 relations 解析
    let computes = parse_computes(obj, &relations)?;
    // 命名冲突检测（A7）：fields ∪ relations ∪ computes 归一后撞名 / 撞保留键 ⇒ 报错
    // （三者同属结果文档命名空间；`_id` 已在 detect_conflicts 内豁免）
    let mut ids: Vec<&str> = Vec::new();
    ids.extend(fields.keys().map(String::as_str));
    ids.extend(relations.keys().map(String::as_str));
    ids.extend(computes.iter().map(|(k, _)| k.as_str()));
    crate::naming::detect_conflicts(ids)?;
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
        relations,
        computes,
        read: str_list(obj.get("read")),
        write: str_list(obj.get("write")),
        indexes: obj
            .get("indexes")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        source: if loc.source == crate::datasource::DEFAULT_SOURCE {
            None
        } else {
            Some(loc.source.clone())
        },
        database: loc.database.clone().filter(|s| !s.is_empty()),
        schema: loc.schema.clone().filter(|s| !s.is_empty()),
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
            strategy: None,
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

fn parse_computes(
    obj: &Map<String, Value>,
    relations: &HashMap<String, RelationDef>,
) -> Result<Vec<(String, ComputeDef)>, String> {
    let mut computes = Vec::new();
    if let Some(Value::Object(cm)) = obj.get("computes") {
        for (key, val) in cm {
            let vo = val.as_object();
            let get = |k: &str| vo.and_then(|o| o.get(k)).cloned();
            let has_fn = get("fn").map(|v| is_truthy(&v)).unwrap_or(false);
            let has_async_fn = get("asyncFn").map(|v| is_truthy(&v)).unwrap_or(false);
            let agg = parse_agg(key, get("agg").as_ref(), relations, has_fn || has_async_fn)?;
            computes.push((
                key.clone(),
                ComputeDef {
                    comp_type: get("type")
                        .and_then(|v| v.as_str().map(String::from))
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "any".to_string()),
                    has_fn,
                    has_async_fn,
                    fn_ref: get("fnRef")
                        .and_then(|v| v.as_str().map(String::from))
                        .filter(|s| !s.is_empty()),
                    agg,
                    depends: str_list(get("depends").as_ref()).unwrap_or_default(),
                    read: str_list(get("read").as_ref()),
                },
            ));
        }
    }
    Ok(computes)
}

/// 解析并校验归一聚合计算列（§9.2(2)）：`{"$count":"lessons"}` / `{"$sum":"lessons.duration"}`。
///
/// - 算子白名单 `$count/$sum/$avg/$min/$max`，不在白名单 → Err（绝不静默）；
/// - 取值必须是**关系路径**：`$count` 仅关系整行计数（`lessons`，不接受字段），
///   其余算子必须带字段（`lessons.duration`）；
/// - 二级关系路径（`orders.items.price`）首批不支持 → Err；
/// - 与 `fn` / `asyncFn` 互斥。
fn parse_agg(
    key: &str,
    raw: Option<&Value>,
    relations: &HashMap<String, RelationDef>,
    has_callback: bool,
) -> Result<Option<Value>, String> {
    let Some(v) = raw.filter(|v| is_truthy(v)) else {
        return Ok(None);
    };
    if has_callback {
        return Err(format!(
            "计算列 \"{key}\" 不能同时声明 agg 与 fn/asyncFn（二选一）"
        ));
    }
    let obj = v.as_object().ok_or_else(|| {
        format!("计算列 \"{key}\" 的 agg 必须是对象，如 {{\"$count\": \"lessons\"}}")
    })?;
    let mut it = obj.iter();
    let (op, arg) = match (it.next(), it.next()) {
        (Some(kv), None) => kv,
        _ => return Err(format!("计算列 \"{key}\" 的 agg 必须且只能声明一个算子键")),
    };
    if !AGG_OPS.contains(&op.as_str()) {
        return Err(format!(
            "计算列 \"{key}\" 的 agg 算子 {op} 不在白名单（$count/$sum/$avg/$min/$max）"
        ));
    }
    let path = arg
        .as_str()
        .filter(|s| !s.is_empty() && *s != "*")
        .ok_or_else(|| {
            format!("计算列 \"{key}\" 的 agg 取值必须是关系路径字符串（如 \"lessons\" / \"lessons.duration\"）")
        })?;
    let (rel, field) = match path.split_once('.') {
        Some((r, f)) => (r, Some(f)),
        None => (path, None),
    };
    if !relations.contains_key(rel) {
        return Err(format!(
            "计算列 \"{key}\" 的 agg 引用的关系 \"{rel}\" 未在 schema 中定义"
        ));
    }
    if field
        .map(|f| f.is_empty() || f.contains('.'))
        .unwrap_or(false)
    {
        return Err(format!(
            "计算列 \"{key}\" 的 agg 关系路径 \"{path}\" 非法：仅支持单级「关系.字段」（二级路径首批不支持）"
        ));
    }
    match op.as_str() {
        "$count" if field.is_some() => Err(format!(
            "计算列 \"{key}\" 的 $count 仅支持关系整行计数（如 \"lessons\"），按字段计数待 P5"
        )),
        "$count" => Ok(Some(v.clone())),
        _ if field.is_none() => Err(format!(
            "计算列 \"{key}\" 的 agg 算子 {op} 必须引用关系字段（如 \"lessons.duration\"）"
        )),
        _ => Ok(Some(v.clone())),
    }
}

/// 构造 `<Name>Deleted` 归档表 schema 定义（字段 = 原字段 + `deletedAt`）。
/// 归档表与原表同落点 —— 落点不由定义文件携带，改由批次继承主链路（见 `register_batch`）。
fn archive_defn(obj: &Map<String, Value>, name: &str, collection: &str) -> Value {
    let mut arch_fields = obj
        .get("fields")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    arch_fields.insert("deletedAt".to_string(), json!({ "type": "number" }));
    // 剔除 `_id` 的自增策略：归档表显式拷贝源行 `_id` 值（upsert-by-_id 幂等），
    // 不走数据库自增；DDL 侧也据此保持归档表 `_id` 为普通主键列
    if let Some(idf) = arch_fields.get_mut("_id") {
        if let Some(o) = idf.as_object_mut() {
            o.remove("strategy");
        }
    }
    let arch = json!({
        "name": format!("{}Deleted", name),
        "collection": format!("{}_deleted", collection),
        "idPrefix": "",
        "_isArchive": true,
        "fields": Value::Object(arch_fields),
        "indexes": obj.get("indexes").cloned().unwrap_or_else(|| json!([])),
    });
    arch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_link_policy_defaults_to_reject() {
        let r = Registry::new();
        assert_eq!(r.write_link_policy(), WriteLinkPolicy::Reject);
        let mut r2 = Registry::new();
        assert_eq!(r2.write_link_policy(), WriteLinkPolicy::Reject);
        r2.set_write_link_policy(WriteLinkPolicy::PrimaryOnly);
        assert_eq!(r2.write_link_policy(), WriteLinkPolicy::PrimaryOnly);
    }
}
