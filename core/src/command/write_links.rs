//! 写路径单连接判决（落点择优的写侧）。
//!
//! 一条 schema 可有主 + 从多条链路（见 [`crate::schema::Registry::links_of`]）。
//! 写同步**必须落在同一 `source`（连接）**才可同事务：
//! - 单 `source`（含同连接内跨 `database`/`schema`）→ 允许，产出 `writeLinks`；
//! - 跨 `source` → 默认 [`WriteLinkPolicy::Reject`] 直接 `Err`；[`WriteLinkPolicy::PrimaryOnly`]
//!   显式降级（仅写主链路 + `degraded.writePrimaryOnly`），**绝不静默拆多次写**。
//!
//! `writeLinks` 仅对**多落点** schema 出现；单落点 schema 计划形状零变更（parity）。

use serde_json::{json, Value};

use crate::schema::{Location, Registry, Schema};

/// 跨连接写同步的稳定错误前缀（宿主可按前缀识别）
pub const ERR_WRITE_CROSS_SOURCE_PREFIX: &str = "ERR_FEDERATION_WRITE_CROSS_SOURCE:";

/// 构造跨连接写拒绝错误（前缀 + schema 名 + source 列表）
pub fn err_write_cross_source(schema: &str, sources: &[String]) -> String {
    format!(
        "{} {} 的写链路跨连接（sources: {}）；同连接内跨 db/schema 才可同事务，跨连接无跨源事务",
        ERR_WRITE_CROSS_SOURCE_PREFIX,
        schema,
        sources.join(", ")
    )
}

/// 跨连接写策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteLinkPolicy {
    /// 默认：跨 `source` 写链路直接拒绝
    #[default]
    Reject,
    /// 降级：仅写主链路，从链路未同步（随计划 emit `degraded.writePrimaryOnly`）
    PrimaryOnly,
}

/// 一个写计划的链路解析结果。
#[derive(Debug, Clone)]
pub struct WriteLinks {
    /// 主链路 source
    pub source: String,
    /// 写目标落点（去重、主链路在首位）
    pub targets: Vec<Location>,
    /// 降级声明（形如 `code = writePrimaryOnly`）
    pub degraded: Vec<Value>,
}

/// 解析一个 schema 的写落点并做单连接判决。
///
/// `links` 为该 schema 的主 + 从链路（主在首位，由调用方从 registry 取）。
/// - `source` 去重后 ≤ 1 → `Ok`（targets = 去重后的全部落点）；
/// - 否则按 `policy`：`Reject` → `Err`；`PrimaryOnly` → 仅主链路 + 降级声明。
pub fn resolve_write_links(
    schema: &Schema,
    links: &[Location],
    policy: WriteLinkPolicy,
) -> Result<WriteLinks, String> {
    // 去重保序（主链路在首位）
    let mut targets: Vec<Location> = Vec::new();
    for l in links {
        if !targets.contains(l) {
            targets.push(l.clone());
        }
    }
    if targets.is_empty() {
        targets.push(schema.location());
    }

    // source 去重保序
    let mut sources: Vec<String> = Vec::new();
    for t in &targets {
        if !sources.iter().any(|s| s == &t.source) {
            sources.push(t.source.clone());
        }
    }

    if sources.len() <= 1 {
        return Ok(WriteLinks {
            source: targets[0].source.clone(),
            targets,
            degraded: Vec::new(),
        });
    }

    match policy {
        WriteLinkPolicy::Reject => Err(err_write_cross_source(&schema.name, &sources)),
        WriteLinkPolicy::PrimaryOnly => Ok(WriteLinks {
            source: targets[0].source.clone(),
            targets: vec![targets[0].clone()],
            degraded: vec![json!({
                "code": "writePrimaryOnly",
                "layer": "federation",
                "message": format!(
                    "{} 的写链路跨连接（sources: {}）；按策略 writePrimaryOnly 仅写主链路，从链路未同步",
                    schema.name,
                    sources.join(", ")
                ),
                "hint": "将同名链路收敛到同一连接，或由上游补偿同步",
            })],
        }),
    }
}

/// 把写链路信息附加到写计划 `Value`。
///
/// 规则：
/// - **零变更**：所有 schema 的**声明落点**去重后总数 ≤ 1 → 原样返回（不追加 `writeLinks`，
///   不改计划形状），保证单落点 schema 的既有 parity 夹具零改动；
/// - 否则追加 `writeLinks`，并把 `degraded` 合并进 `plan.degraded`（无则新建）。
///
/// `atomic` 判定取**声明落点**的 source 去重数：跨连接即使经 `PrimaryOnly` 只写主链路，
/// 也因从链路未同步而为 `false`（显式降级，禁止当作整体成功）。
pub(crate) fn attach_write_links(
    plan: Value,
    registry: &Registry,
    schemas: &[&str],
    policy: WriteLinkPolicy,
) -> Result<Value, String> {
    // 声明落点（不受 policy 降级影响）用于零变更判定与 atomic 判定
    let mut declared: Vec<Location> = Vec::new();
    let mut resolved: Vec<WriteLinks> = Vec::new();
    for name in schemas {
        let schema = registry.get(name)?;
        let links = registry.links_of(name)?;
        for l in links {
            if !declared.contains(l) {
                declared.push(l.clone());
            }
        }
        resolved.push(resolve_write_links(schema, links, policy)?);
    }

    // 单落点/零变更：原样返回
    if declared.len() <= 1 {
        return Ok(plan);
    }

    let mut targets: Vec<Location> = Vec::new();
    let mut degraded: Vec<Value> = Vec::new();
    for wl in &resolved {
        for t in &wl.targets {
            if !targets.contains(t) {
                targets.push(t.clone());
            }
        }
        degraded.extend(wl.degraded.iter().cloned());
    }

    let mut declared_sources: Vec<String> = Vec::new();
    for t in &declared {
        if !declared_sources.iter().any(|s| s == &t.source) {
            declared_sources.push(t.source.clone());
        }
    }
    let atomic = declared_sources.len() <= 1;
    let source = targets
        .first()
        .map(|t| t.source.clone())
        .unwrap_or_default();
    let targets_json: Vec<Value> = targets
        .iter()
        .map(|t| {
            json!({
                "source": t.source,
                "database": t.database,
                "schema": t.schema,
            })
        })
        .collect();

    let mut plan = plan;
    if let Some(obj) = plan.as_object_mut() {
        obj.insert(
            "writeLinks".to_string(),
            json!({
                "atomic": atomic,
                "source": source,
                "targets": targets_json,
                "degraded": degraded,
            }),
        );
        if !degraded.is_empty() {
            let entry = obj
                .entry("degraded".to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            match entry.as_array_mut() {
                Some(arr) => arr.extend(degraded.iter().cloned()),
                None => *entry = Value::Array(degraded.clone()),
            }
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Registry;
    use serde_json::json;

    fn loc(source: &str, database: Option<&str>) -> Location {
        Location {
            source: source.to_string(),
            database: database.map(String::from),
            schema: None,
        }
    }

    fn order_defn() -> Value {
        json!({
            "name": "Order", "collection": "orders", "timestamps": false,
            "fields": { "code": { "type": "string" } }, "relations": {}
        })
    }

    fn replica_defn() -> Value {
        json!({ "name": "Order", "replica": true })
    }

    #[test]
    fn same_source_multi_target_is_atomic() {
        let mut r = Registry::new();
        r.register_batch(
            &[
                (order_defn(), loc("mongo_main", Some("orders_db"))),
                (replica_defn(), loc("mongo_main", Some("archived_db"))),
            ],
            None,
        )
        .unwrap();
        let schema = r.get("Order").cloned().unwrap();
        let links = r.links_of("Order").unwrap().to_vec();
        let out = resolve_write_links(&schema, &links, WriteLinkPolicy::Reject).unwrap();
        assert_eq!(out.source, "mongo_main");
        assert_eq!(out.targets.len(), 2);
        assert!(out.degraded.is_empty());
    }

    #[test]
    fn cross_source_reject_errors() {
        let mut r = Registry::new();
        r.register_batch(
            &[
                (order_defn(), loc("mongo_main", Some("orders_db"))),
                (replica_defn(), loc("pg_main", Some("analytics"))),
            ],
            None,
        )
        .unwrap();
        let schema = r.get("Order").cloned().unwrap();
        let links = r.links_of("Order").unwrap().to_vec();
        let err = resolve_write_links(&schema, &links, WriteLinkPolicy::Reject)
            .expect_err("跨 source 默认应拒绝");
        assert!(
            err.starts_with(ERR_WRITE_CROSS_SOURCE_PREFIX),
            "err = {err}"
        );
    }

    #[test]
    fn cross_source_primary_only_degrades() {
        let mut r = Registry::new();
        r.register_batch(
            &[
                (order_defn(), loc("mongo_main", Some("orders_db"))),
                (replica_defn(), loc("pg_main", Some("analytics"))),
            ],
            None,
        )
        .unwrap();
        let schema = r.get("Order").cloned().unwrap();
        let links = r.links_of("Order").unwrap().to_vec();
        let out = resolve_write_links(&schema, &links, WriteLinkPolicy::PrimaryOnly).unwrap();
        assert_eq!(out.source, "mongo_main");
        assert_eq!(out.targets.len(), 1, "PrimaryOnly 仅写主链路");
        assert_eq!(out.degraded.len(), 1);
        assert_eq!(out.degraded[0]["code"], "writePrimaryOnly");
    }

    #[test]
    fn single_location_schema_plan_unchanged() {
        let mut r = Registry::new();
        r.register_batch(
            &[(order_defn(), loc("mongo_main", Some("orders_db")))],
            None,
        )
        .unwrap();
        let plan = json!({ "command": { "kind": "insertOne" }, "returns": {} });
        let out = attach_write_links(plan.clone(), &r, &["Order"], r.write_link_policy()).unwrap();
        assert_eq!(out, plan, "单落点 schema 计划形状必须零变更");
        assert!(out.get("writeLinks").is_none());
    }

    #[test]
    fn multi_location_same_source_attaches_write_links() {
        let mut r = Registry::new();
        r.register_batch(
            &[
                (order_defn(), loc("mongo_main", Some("orders_db"))),
                (replica_defn(), loc("mongo_main", Some("archived_db"))),
            ],
            None,
        )
        .unwrap();
        let plan = json!({ "command": { "kind": "insertOne" } });
        let out = attach_write_links(plan, &r, &["Order"], r.write_link_policy()).unwrap();
        assert_eq!(out["writeLinks"]["atomic"], true);
        assert_eq!(out["writeLinks"]["source"], "mongo_main");
        assert_eq!(out["writeLinks"]["targets"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn cross_source_primary_only_never_atomic() {
        let mut r = Registry::new();
        r.register_batch(
            &[
                (order_defn(), loc("mongo_main", Some("orders_db"))),
                (replica_defn(), loc("pg_main", Some("analytics"))),
            ],
            None,
        )
        .unwrap();
        r.set_write_link_policy(WriteLinkPolicy::PrimaryOnly);
        let plan = json!({ "command": { "kind": "insertOne" } });
        let out = attach_write_links(plan, &r, &["Order"], r.write_link_policy()).unwrap();
        assert_eq!(out["writeLinks"]["atomic"], false);
        let codes: Vec<Value> = out["degraded"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["code"].clone())
            .collect();
        assert!(codes.contains(&json!("writePrimaryOnly")), "out = {out}");
    }
}
