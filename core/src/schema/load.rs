//! 目录语义装载（设计 §5.2/§5.3）：纯逻辑、零 IO。
//!
//! 输入 = 「store.config.json 的 JSON + 目录扫描结果（相对 defs-root 的路径 + 已读入的 defn）」；
//! 输出 = 定位后的装载项 / 显式错误。IO（读文件 / walk）在宿主与脚手架。
//!
//! 批次判据（分组 / 主唯一 / 四元组冲突）**与 [`super::registry::register_batch`] 同源**：
//! 共用 [`super::registry::classify_batch`] / [`super::registry::detect_location_conflict`]，
//! 禁各写一份（判据单点，防 A1/A3 漂移）。

use serde_json::Value;

use crate::datasource::DataSource;
use crate::dialect::Backend;

use super::definition::{Location, Role};
use super::registry::{classify_batch, detect_location_conflict, BatchError};

/// 一个连接声明（`store.config.json` 的 `sources` 一项）
#[derive(Debug, Clone)]
pub struct SourceDecl {
    pub name: String,
    /// 小写归一后：mongodb/mysql/pg/sqlite（kind 白名单见执行文档 §4.2）
    pub kind: String,
    /// 该连接维护的库
    pub databases: Vec<String>,
}

/// 一条链路声明（`store.config.json` 的 `links[<schema>]` 一项）。
///
/// 外置配置，**禁写入 schema 定义文件**（设计 §4.1）；`source`/`database`/`schema`
/// 三元组标识一个既有或待补建的落点，`role`/`priority` 为读解析排序属性。
#[derive(Debug, Clone)]
pub struct LinkDecl {
    /// 连接名（必须已在 `sources` 声明）
    pub source: String,
    /// 链路角色；缺省 `None` = 未声明（排序时视作最低层，不参与 primary 唯一性校验）
    pub role: Option<Role>,
    /// 同 role 内升序排序；缺省 `None` = 视作最大；允许负数
    pub priority: Option<i64>,
    /// 库：缺省 = `sources[source].databases` 的唯一库（多库须显式给）
    pub database: Option<String>,
    /// 连接内 schema（仅 PG 有意义）
    pub schema: Option<String>,
}

/// 装载配置（`store.config.json` 的 `sources` + `defs` + `links`）
#[derive(Debug, Clone, Default)]
pub struct LoadConfig {
    /// 保持声明顺序（`serde_json::Map` 为 BTreeMap ⇒ 字典序；确定性输出）
    pub sources: Vec<SourceDecl>,
    pub defs: Vec<String>,
    /// schema 名 → 有序链路声明（保声明序；缺省为空 ⇒ 零回归）
    pub links: Vec<(String, Vec<LinkDecl>)>,
}

impl LoadConfig {
    /// 解析 `{ "sources": { <name>: { kind, databases:[...] } }, "defs": [<root>...], "links": {...} }`。
    ///
    /// 非法即 `Err`（禁静默兜底；含 kind 白名单、databases 非空、defs 非空、links 六条校验）。
    /// `links` 缺省 ⇒ 空（零回归）；非空时每项须 `source` 已声明、`role` 合法、
    /// `priority` 为整数、恰一条 `primary`、`database` 可判（多库须显式给）。
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let obj = v
            .as_object()
            .ok_or_else(|| "ERR:LOAD store.config.json 必须是对象".to_string())?;

        let sources_obj = obj
            .get("sources")
            .and_then(|s| s.as_object())
            .ok_or_else(|| {
                "ERR:LOAD 缺 sources（须为 { name: { kind, databases } } 对象）".to_string()
            })?;
        if sources_obj.is_empty() {
            return Err("ERR:LOAD sources 不得为空".to_string());
        }

        let mut sources = Vec::new();
        for (name, decl) in sources_obj {
            let d = decl
                .as_object()
                .ok_or_else(|| format!("ERR:LOAD 连接 {name} 声明必须是对象"))?;
            let raw_kind = d
                .get("kind")
                .and_then(|k| k.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("ERR:LOAD 连接 {name} 缺 kind"))?;
            // kind 白名单：复用 DataSource::from_kind（mongo/mongodb/mysql/pg/postgres/postgresql/sqlite/local/memory）
            DataSource::from_kind(raw_kind).map_err(|_| {
                format!(
                    "ERR:LOAD 连接 {name} 的 kind 非法: {raw_kind}（仅 mongodb/mysql/pg/sqlite/local/memory）"
                )
            })?;
            let databases: Vec<String> = d
                .get("databases")
                .and_then(|ds| ds.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            if databases.is_empty() {
                return Err(format!(
                    "ERR:LOAD 连接 {name} 的 databases 须为非空字符串数组"
                ));
            }
            sources.push(SourceDecl {
                name: name.clone(),
                kind: raw_kind.to_lowercase(),
                databases,
            });
        }

        let defs: Vec<String> = obj
            .get("defs")
            .and_then(|ds| ds.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        if defs.is_empty() {
            return Err("ERR:LOAD defs 须为非空字符串数组（定义根）".to_string());
        }

        // links：schema 名 → 有序链路声明；缺省 ⇒ 空（零回归，全走既有目录语义）
        let links = match obj.get("links") {
            None | Some(Value::Null) => Vec::new(),
            Some(v) => {
                let links_obj = v.as_object().ok_or_else(|| {
                    "ERR:LOAD links 必须是对象（schema 名 → 链路声明数组）".to_string()
                })?;
                let mut out: Vec<(String, Vec<LinkDecl>)> = Vec::with_capacity(links_obj.len());
                for (name, decls_v) in links_obj {
                    let arr = decls_v.as_array().ok_or_else(|| {
                        format!("ERR:LOAD links[{name}] 必须是数组（链路声明列表）")
                    })?;
                    let mut decls: Vec<LinkDecl> = Vec::with_capacity(arr.len());
                    let mut primary_count = 0usize;
                    for (i, item) in arr.iter().enumerate() {
                        let d = item
                            .as_object()
                            .ok_or_else(|| format!("ERR:LOAD links[{name}][{i}] 必须是对象"))?;
                        // source 必填，且必须已在 sources 声明（未声明即 Err，禁猜）
                        let source = d
                            .get("source")
                            .and_then(|s| s.as_str())
                            .filter(|s| !s.is_empty())
                            .ok_or_else(|| format!("ERR:LOAD links[{name}][{i}] 缺 source"))?;
                        let src_decl = sources.iter().find(|s| s.name == source).ok_or_else(|| {
                            format!("ERR:LOAD links[{name}] 的 source 未在 sources 中声明: {source}")
                        })?;
                        // role 缺省 ⇒ None；给值须经 Role::parse（非法 ⇒ Err）
                        let role = match d.get("role") {
                            None | Some(Value::Null) => None,
                            Some(rv) => {
                                let rs = rv.as_str().ok_or_else(|| {
                                    format!("ERR:LOAD links[{name}][{i}] 的 role 须为字符串")
                                })?;
                                Some(Role::parse(rs)?)
                            }
                        };
                        if role == Some(Role::Primary) {
                            primary_count += 1;
                        }
                        // priority 必须为整数
                        let priority = match d.get("priority") {
                            None | Some(Value::Null) => None,
                            Some(pv) => Some(pv.as_i64().ok_or_else(|| {
                                format!("ERR:LOAD links[{name}][{i}] 的 priority 须为整数")
                            })?),
                        };
                        // database 缺省：source 恰 1 库 ⇒ 取其值；多库 ⇒ Err（禁猜）
                        let database = match d.get("database") {
                            None | Some(Value::Null) => {
                                if src_decl.databases.len() == 1 {
                                    Some(src_decl.databases[0].clone())
                                } else {
                                    return Err(format!(
                                        "ERR:LOAD links[{name}] 的 source {source} 维护多库，须显式给 database"
                                    ));
                                }
                            }
                            Some(dv) => Some(
                                dv.as_str()
                                    .filter(|s| !s.is_empty())
                                    .ok_or_else(|| {
                                        format!(
                                            "ERR:LOAD links[{name}][{i}] 的 database 须为非空字符串"
                                        )
                                    })?
                                    .to_string(),
                            ),
                        };
                        let schema = match d.get("schema") {
                            None | Some(Value::Null) => None,
                            Some(sv) => Some(
                                sv.as_str()
                                    .filter(|s| !s.is_empty())
                                    .ok_or_else(|| {
                                        format!(
                                            "ERR:LOAD links[{name}][{i}] 的 schema 须为非空字符串"
                                        )
                                    })?
                                    .to_string(),
                            ),
                        };
                        decls.push(LinkDecl {
                            source: source.to_string(),
                            role,
                            priority,
                            database,
                            schema,
                        });
                    }
                    // 同一 links[<name>] 内 role=primary 恰好一条
                    if primary_count != 1 {
                        return Err(format!(
                            "ERR:LOAD links[{name}] 必须恰好一条 role=primary（实际 {primary_count} 条）"
                        ));
                    }
                    out.push((name.clone(), decls));
                }
                out
            }
        };

        Ok(LoadConfig {
            sources,
            defs,
            links,
        })
    }

    /// db 名 → (连接声明, 语义深度)：同一 db 名挂多个连接 ⇒ `Err`（设计 §5.2）。
    ///
    /// 深度由 [`depth_layered`] 按 kind 决定（PG=2，其余=1）。
    pub fn source_of_db(&self, db: &str) -> Result<(&SourceDecl, u8), String> {
        let mut hit: Option<&SourceDecl> = None;
        for s in &self.sources {
            if s.databases.iter().any(|d| d == db) {
                if let Some(prev) = hit {
                    return Err(format!(
                        "ERR:LOAD 库归属冲突: {db} 同时声明在连接 {} 与 {}（同一 db 名只能挂一个连接）",
                        prev.name, s.name
                    ));
                }
                hit = Some(s);
            }
        }
        let decl = hit.ok_or_else(|| {
            format!(
                "ERR:LOAD 库目录未声明: {db}（须在 store.config.json 的某连接 databases 中列出）"
            )
        })?;
        let depth = depth_layered(&decl.kind)?;
        Ok((decl, depth))
    }
}

/// 语义层级深度：PG=2（L2=schema），Mongo/MySQL/SQLite=1（L2+ 打平）。
/// kind 非法 ⇒ `Err`（禁回落默认）。
pub fn depth_layered(kind: &str) -> Result<u8, String> {
    let ds = DataSource::from_kind(kind).map_err(|_| {
        format!("ERR:LOAD 连接 kind 非法: {kind}（仅 mongodb/mysql/pg/sqlite/local/memory）")
    })?;
    Ok(match ds.backend() {
        Some(Backend::Postgres) => 2,
        _ => 1,
    })
}

/// 由「db 目录以下的相对路径」算落点（设计 §5.2 固定语义，D3）：
/// - `rel_under_db` 至少 1 段（文件名）；除末段（文件名）外最多 `depth-1` 段参与语义，其余**打平**；
/// - `depth == 2`（PG）：有子目录 ⇒ 子目录首段 = `schema`；无子目录 ⇒ `schema = None`
///   （回落连接默认 search_path，见 `definition.rs` Location 语义）；
/// - `depth == 1`：`schema = None`；L2+ 全部打平（不影响 `database` 归属）；
/// - 输出 `Location { source, database: Some(db), schema }`。
pub fn locate(rel_under_db: &str, source: &str, db: &str, depth: u8) -> Result<Location, String> {
    let segs: Vec<&str> = rel_under_db
        .split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .collect();
    if segs.is_empty() {
        return Err(format!("ERR:LOAD 定义路径非法（缺文件名）: {rel_under_db}"));
    }
    // 末段 = 文件名；其余 = 目录段
    let dirs = &segs[..segs.len() - 1];
    let schema = if depth >= 2 && !dirs.is_empty() {
        Some(dirs[0].to_string())
    } else {
        None
    };
    Ok(Location {
        source: source.to_string(),
        database: Some(db.to_string()),
        schema,
        role: None,
        priority: None,
    })
}

/// 纯批次规划（设计 §5.2 伪码 + §5.3 主从）：
/// `files: [(rel_from_defs_root, defn)]`（`rel` 首段 = db 目录名）。
///
/// 步骤：① 按 `rel` 首段定位 db → `source_of_db` 反查（未声明 / 重挂 ⇒ `Err`）；
///      ② `depth_layered(kind)` → `locate`；
///      ③ 判据单点 `classify_batch`（分组 / 主唯一 / 批内四元组冲突）；
///      ④ 每组主**恰好一份**，0 份 ⇒ `Err`（A1/A3）；
///      ⑤ 输出有序装载项：**主在前、其后从**（组间按 name 字典序）。
pub fn plan_load(
    cfg: &LoadConfig,
    files: &[(String, Value)],
) -> Result<Vec<(Value, Location)>, String> {
    // ①/② 归属 + locate
    let mut items: Vec<(Value, Location)> = Vec::with_capacity(files.len());
    for (rel, defn) in files {
        let segs: Vec<&str> = rel.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            return Err(format!("ERR:LOAD 定义路径非法: {rel}"));
        }
        let db = segs[0];
        let (decl, depth) = cfg.source_of_db(db)?;
        let rel_under_db = segs[1..].join("/");
        let loc = locate(&rel_under_db, &decl.name, db, depth)?;
        items.push((defn.clone(), loc));
    }

    // ③ 判据单点（分组 / 主唯一 / 批内四元组冲突）
    let class = classify_batch(&items).map_err(|e| batch_err_to_load(e, files))?;

    // ④/⑤ 组间按 name 字典序；主在前、其后从
    let mut names: Vec<&String> = class.order.iter().collect();
    names.sort();

    // 批内四元组冲突（与 register_batch 同源的判据函数）
    let mut quads: Vec<(String, String, Location)> = Vec::new();
    for name in &names {
        if let Some((_, loc, coll)) = class.primaries.get(*name) {
            quads.push(((*name).clone(), coll.clone(), loc.clone()));
        }
        if let Some(rs) = class.replicas.get(*name) {
            for (defn, loc) in rs {
                let coll = defn
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(name)
                    .to_string();
                quads.push(((*name).clone(), coll, loc.clone()));
            }
        }
    }
    if let Some(c) = detect_location_conflict(&quads) {
        return Err(format!(
            "ERR:LOAD 落点冲突: {} 同时映射 ({}, {:?}, {:?})（同一落点只能有一个 collection；冲突名 `{}` 与 `{}`）",
            c.collection, c.source, c.database, c.schema, c.prev, c.name
        ));
    }

    let mut out: Vec<(Value, Location)> = Vec::with_capacity(items.len());
    for name in names {
        let Some((defn, loc, _coll)) = class.primaries.get(name) else {
            return Err(format!(
                "ERR:LOAD 主定义缺失: {name}（同名项均为 replica:true，须恰好一份主）"
            ));
        };
        out.push((defn.clone(), loc.clone()));
        if let Some(rs) = class.replicas.get(name) {
            for (rdefn, rloc) in rs {
                out.push((rdefn.clone(), rloc.clone()));
            }
        }
    }
    Ok(out)
}

/// `BatchError` → `ERR:LOAD` 文案（`indices` → `files[i].rel` 供定位重复主定义）。
fn batch_err_to_load(e: BatchError, files: &[(String, Value)]) -> String {
    match e {
        BatchError::Parse(m) => format!("ERR:LOAD {m}"),
        BatchError::PrimaryDuplicate { name, indices } => {
            let rels: Vec<String> = indices
                .iter()
                .map(|i| {
                    files
                        .get(*i)
                        .map(|(r, _)| r.clone())
                        .unwrap_or_else(|| format!("#{i}"))
                })
                .collect();
            format!(
                "ERR:LOAD 主定义重复: {name}（{}）——同名只允许一份主，其余须 replica:true",
                rels.join(" 与 ")
            )
        }
        BatchError::LocationConflict(c) => format!(
            "ERR:LOAD 落点冲突: {} 同时映射 ({}, {:?}, {:?})（同一落点只能有一个 collection；冲突名 `{}` 与 `{}`）",
            c.collection, c.source, c.database, c.schema, c.prev, c.name
        ),
    }
}
