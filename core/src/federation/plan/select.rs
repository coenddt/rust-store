//! 读路径落点择优：为一次联邦查询涉及的每个 schema 选一条候选链路，
//! 使「切分组数最少」（V4 两级字典序），**不做加权求和**。
//!
//! 术语：
//! - 候选链路 = schema 的主 + 从 [`Location`]（主在首位），来自 [`Registry::links_of`]；
//! - 取数单元 = 一组可在同一连接内下推的 schema（[`compatible`] 判定边是否并组）；
//! - 择优 = 在「schema → 候选」的笛卡尔积上，`min 组数 → max 组内边数 → 规范序最小`。
//!
//! 约束先于优化：[`filter_reachable`] 在 [`select_links`] 之前过滤不可达候选；
//! 过滤后为空即 [`select_links`] 报 `ERR_FEDERATION_NO_REACHABLE_LINK`（绝不回落）。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::datasource::{DataSource, DataSourceConfig};
use crate::pipeline::RelAst;
use crate::schema::{Location, Registry};

/// 候选组合数上限（防组合爆炸；超出走确定性近似）。经验护栏，待压测校准（设计 §13）。
pub const MAX_LINK_COMBINATIONS: usize = 4096;

/// schema name → 候选链路（主 + 从，主在首位）
pub(super) type Candidates = HashMap<String, Vec<Location>>;

/// 查询图：`(schemas：root 在首位、去重保序, edges：(parent, child) 逐关系一条)`
type Graph = (Vec<String>, Vec<(String, String)>);

/// 两个落点能否在**同一取数单元**内下推（决定是否并进同一组）。
///
/// 沿用 `route.rs`（旧 `can_pushdown`）既有语义：
/// - 跨 `source` → 否（内存 join）；
/// - 双方都是 SQL 且同 `source` → 是（`qualified` 表名可跨 `database`/`schema` JOIN）；
/// - 其余（Mongo，或 kind 未知）→ 仅 `database` 与 `schema` **同时相等**才下推。
pub(super) fn compatible(a: &Location, b: &Location, ds_cfg: &DataSourceConfig) -> bool {
    if a.source != b.source {
        return false;
    }
    let ka = ds_cfg.resolve(Some(&a.source)).ok();
    let kb = ds_cfg.resolve(Some(&b.source)).ok();
    match (ka, kb) {
        (Some(DataSource::Sql(_)), Some(DataSource::Sql(_))) => true,
        _ => a.database == b.database && a.schema == b.schema,
    }
}

/// 预扫本次查询涉及的 schema 与关系边（不改 AST）。
///
/// 必须与 [`super::route::walk`] 的遍历口径一致（同一 `relations` 输入、同
/// `rel_def.model.is_empty()` 跳过规则）。返回 `(schemas：root 在首位、去重保序,
/// edges：(parent_model, child_model) 逐关系一条)`。
pub(super) fn collect_graph(
    root: &str,
    root_relations: &[(String, RelAst)],
    registry: &Registry,
) -> Result<Graph, String> {
    fn push_unique(v: &mut Vec<String>, name: &str) {
        if !v.iter().any(|x| x == name) {
            v.push(name.to_string());
        }
    }

    fn dfs(
        parent: &str,
        relations: &[(String, RelAst)],
        registry: &Registry,
        schemas: &mut Vec<String>,
        edges: &mut Vec<(String, String)>,
    ) -> Result<(), String> {
        let parent_schema = registry.get(parent)?;
        for (name, rel_ast) in relations {
            let Some(rel_def) = parent_schema.relations.get(name) else {
                continue;
            };
            // 未声明 model 的「关系」不是关联（与 walk 一致）
            if rel_def.model.is_empty() {
                continue;
            }
            let child = rel_def.model.clone();
            push_unique(schemas, &child);
            edges.push((parent.to_string(), child.clone()));
            dfs(&child, &rel_ast.relations, registry, schemas, edges)?;
        }
        Ok(())
    }

    let mut schemas: Vec<String> = Vec::new();
    let mut edges: Vec<(String, String)> = Vec::new();
    push_unique(&mut schemas, root);
    dfs(root, root_relations, registry, &mut schemas, &mut edges)?;
    Ok((schemas, edges))
}

/// 取 schema 的候选链路（主 + 从，主在首位）。
///
/// 01 实际模型把链路挂在 [`Registry`] 上（非 `Schema`），故按 schema name 取；
/// 无链路信息时回落「单落点」= `registry.primary_location(name)`。
pub(super) fn candidates_of(registry: &Registry, name: &str) -> Result<Vec<Location>, String> {
    let links = registry.links_of(name)?;
    if links.is_empty() {
        Ok(vec![registry.primary_location(name)?.clone()])
    } else {
        Ok(links.to_vec())
    }
}

/// 约束先于优化：按上下文可达 source 过滤候选（`reachable = None` 表示不限制）。
///
/// 过滤后为空 → [`select_links`] 阶段报 `ERR_FEDERATION_NO_REACHABLE_LINK`（绝不静默回落）。
pub(super) fn filter_reachable(cands: &mut Candidates, reachable: Option<&[String]>) {
    let Some(allowed) = reachable else {
        return;
    };
    for locs in cands.values_mut() {
        locs.retain(|l| allowed.iter().any(|s| s == &l.source));
    }
}

/// 择优结果：每个涉及 schema 的选中落点 + 评测指标。
#[derive(Debug, Clone)]
pub(super) struct Selection {
    /// schema name → 选中落点
    pub(super) locs: HashMap<String, Location>,
    /// 切分组数 = 取数单元数
    #[allow(dead_code)] // 契约指标：随 Selection 返回供诊断/后续复用
    pub(super) groups: usize,
    /// 组内（可下推）边数
    #[allow(dead_code)] // 契约指标：随 Selection 返回供诊断/后续复用
    pub(super) internal_edges: usize,
    /// 是否走了近似算法
    pub(super) approximate: bool,
}

/// 并查集（评测「组数」用）
struct Dsu {
    parent: Vec<usize>,
}

impl Dsu {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        // 路径压缩
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra] = rb;
        }
    }

    fn components(&mut self) -> usize {
        (0..self.parent.len())
            .filter(|&i| self.find(i) == i)
            .count()
    }
}

/// 评测一组选择：返回 `(组数, 组内边数)`。
fn evaluate(
    chosen: &[Location],
    idx: &HashMap<&str, usize>,
    edges: &[(String, String)],
    ds_cfg: &DataSourceConfig,
) -> (usize, usize) {
    let mut dsu = Dsu::new(chosen.len());
    let mut internal = 0usize;
    for (u, v) in edges {
        let (iu, iv) = match (idx.get(u.as_str()), idx.get(v.as_str())) {
            (Some(&a), Some(&b)) => (a, b),
            _ => continue,
        };
        if compatible(&chosen[iu], &chosen[iv], ds_cfg) {
            dsu.union(iu, iv);
            internal += 1;
        }
    }
    (dsu.components(), internal)
}

/// 规范序键条目：`(schema, source, database, pg_schema)`
type CanonEntry = (String, String, Option<String>, Option<String>);

/// 规范序键：把 `(schema, source, database, pg_schema)` 排序后比较（稳定 tie-break）。
fn canon_key(schemas: &[String], chosen: &[Location]) -> Vec<CanonEntry> {
    let mut v: Vec<CanonEntry> = schemas
        .iter()
        .zip(chosen.iter())
        .map(|(s, l)| {
            (
                s.clone(),
                l.source.clone(),
                l.database.clone(),
                l.schema.clone(),
            )
        })
        .collect();
    v.sort();
    v
}

/// 两级字典序判定：`min groups → max internal_edges → 规范序最小`。
fn better(
    new: &(Vec<Location>, usize, usize),
    best: &Option<(Vec<Location>, usize, usize)>,
    schemas: &[String],
) -> bool {
    match best {
        None => true,
        Some(b) => {
            if new.1 != b.1 {
                return new.1 < b.1;
            }
            if new.2 != b.2 {
                return new.2 > b.2;
            }
            canon_key(schemas, &new.0) < canon_key(schemas, &b.0)
        }
    }
}

/// 两级字典序择优（V4）：`min groups → max internal_edges`；同分取规范序最小（稳定）。
///
/// `cands` 须已过约束过滤且非空（空 → `Err(ERR_FEDERATION_NO_REACHABLE_LINK)`）。
/// 组合数超 [`MAX_LINK_COMBINATIONS`] → 走确定性近似（[`select_links_approx`]）。
pub(super) fn select_links(
    schemas: &[String],
    edges: &[(String, String)],
    cands: &Candidates,
    ds_cfg: &DataSourceConfig,
) -> Result<Selection, String> {
    for s in schemas {
        if cands.get(s).map(|v| v.is_empty()).unwrap_or(true) {
            return Err(format!(
                "ERR_FEDERATION_NO_REACHABLE_LINK: {} 无可用落点（候选链路被上下文/权限过滤后为空）",
                s
            ));
        }
    }

    let combos: u128 = schemas.iter().map(|s| cands[s].len() as u128).product();
    if combos > MAX_LINK_COMBINATIONS as u128 {
        return Ok(select_links_approx(schemas, edges, cands, ds_cfg));
    }

    let idx: HashMap<&str, usize> = schemas
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), i))
        .collect();
    let n = schemas.len();
    let mut indices = vec![0usize; n];
    let mut best: Option<(Vec<Location>, usize, usize)> = None;

    loop {
        let chosen: Vec<Location> = (0..n)
            .map(|i| cands[&schemas[i]][indices[i]].clone())
            .collect();
        let (groups, internal) = evaluate(&chosen, &idx, edges, ds_cfg);
        let cand = (chosen, groups, internal);
        if better(&cand, &best, schemas) {
            best = Some(cand);
        }

        // 里程表进位
        let mut k = 0usize;
        while k < n {
            indices[k] += 1;
            if indices[k] < cands[&schemas[k]].len() {
                break;
            }
            indices[k] = 0;
            k += 1;
        }
        if k >= n {
            break;
        }
    }

    let (locs, groups, internal_edges) = best.expect("非空候选笛卡尔积至少有一个组合");
    Ok(Selection {
        locs: schemas.iter().cloned().zip(locs).collect(),
        groups,
        internal_edges,
        approximate: false,
    })
}

/// 近似择优：以「全主链路」为基线，再对每个出现的候选落点做一次单点对齐，取最优。
///
/// `approximate = true`（随 `plan.degraded` 输出 `linkSelectionApprox`，绝不静默）。
fn select_links_approx(
    schemas: &[String],
    edges: &[(String, String)],
    cands: &Candidates,
    ds_cfg: &DataSourceConfig,
) -> Selection {
    let idx: HashMap<&str, usize> = schemas
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), i))
        .collect();
    let n = schemas.len();

    let base: Vec<Location> = (0..n).map(|i| cands[&schemas[i]][0].clone()).collect();
    let (g, e) = evaluate(&base, &idx, edges, ds_cfg);
    let mut best: (Vec<Location>, usize, usize) = (base.clone(), g, e);

    for i in 0..n {
        for alt in cands[&schemas[i]].iter().skip(1) {
            let mut trial = base.clone();
            trial[i] = alt.clone();
            let (g, e) = evaluate(&trial, &idx, edges, ds_cfg);
            let cand = (trial, g, e);
            if better(&cand, &Some(best.clone()), schemas) {
                best = cand;
            }
        }
    }

    Selection {
        locs: schemas.iter().cloned().zip(best.0).collect(),
        groups: best.1,
        internal_edges: best.2,
        approximate: true,
    }
}

/// 近似择优的降级声明（随 `plan.degraded` 输出，绝不静默）。
pub(super) fn degraded_approx() -> Value {
    json!({
        "code": "linkSelectionApprox",
        "layer": "federation",
        "message": format!(
            "涉及 schema 数 × 候选链路数超过 {}，落点择优走确定性近似",
            MAX_LINK_COMBINATIONS
        ),
        "hint": "收敛本次查询涉及的表/链路数，以恢复精确择优",
    })
}
