//! `$lookup` → LEFT JOIN 的关系解析

use serde_json::Value;

use crate::schema::{Registry, Schema};

#[derive(Debug, Clone)]
pub(super) struct Join {
    pub(super) rel_name: String,
    pub(super) alias: String,
    /// 关系目标 model 名（定位目标 schema 的唯一依据，绝不经 collection 名猜测）
    pub(super) model: String,
    pub(super) local_col: String,
    pub(super) foreign_col: String,
    /// `one` 关系（LEFT JOIN 后还原为对象/null；`many` 为数组）
    pub(super) one: bool,
    /// 关系子 pipeline `$match` 中除 join 键外的附加条件（关系 `$condition` + 目标 owner 注入）
    /// → 作为 JOIN 的 `ON ... AND <条件>` 下推，绝不静默丢弃
    pub(super) extra: Option<Value>,
    /// 父 JOIN 下标（`None` = 根表）；嵌套关系靠它决定 `ON <子>.<fk> = <父>.<lk>`
    pub(super) parent: Option<usize>,
    /// 关系名路径（根 → 本层，含本层关系名）—— 决定平铺列还原到文档的哪一层
    pub(super) path: Vec<String>,
    /// 沿 `path` 每级关系的基数（`true` = `one`），与 `path` 等长
    pub(super) ones: Vec<bool>,
    /// 关系子 pipeline 的每父 top-N（`$sort` / `$skip` / `$limit`）→ 窗口函数下推
    pub(super) child_sort: Option<Value>,
    pub(super) child_skip: Option<i64>,
    pub(super) child_limit: Option<i64>,
}

/// 从 schema.relations 解析 $lookup JOIN 键（`parent`/`path`/`ones`/`child_*` 由调用方填充）
pub(super) fn resolve_join(
    schema: &Schema,
    registry: &Registry,
    alias: &str,
    from: &str,
) -> Option<Join> {
    let make = |name: &str, d: &crate::schema::RelationDef| Join {
        rel_name: name.to_string(),
        alias: alias.to_string(),
        model: d.model.clone(),
        local_col: d.local_field.clone(),
        foreign_col: d.foreign_field.clone(),
        one: d.rel_type == "one",
        extra: None,
        parent: None,
        path: vec![name.to_string()],
        ones: vec![d.rel_type == "one"],
        child_sort: None,
        child_skip: None,
        child_limit: None,
    };
    // 优先按关系名匹配
    if let Some((name, d)) = schema.relations.iter().find(|(n, _)| n.as_str() == alias) {
        return Some(make(name, d));
    }
    // 按 model/collection 匹配
    schema.relations.iter().find_map(|(name, d)| {
        let ok = d.model == from
            || registry
                .get(&d.model)
                .ok()
                .map(|s| s.collection == from)
                .unwrap_or(false);
        if ok {
            Some(make(name, d))
        } else {
            None
        }
    })
}
