//! `$lookup` → LEFT JOIN 的关系解析

use crate::schema::{Registry, Schema};

#[derive(Debug, Clone)]
pub(super) struct Join {
    pub(super) rel_name: String,
    pub(super) alias: String,
    /// 关系目标 model 名（定位目标 schema 的唯一依据，绝不经 collection 名猜测）
    pub(super) model: String,
    pub(super) local_col: String,
    pub(super) foreign_col: String,
}

/// 从 schema.relations 解析 $lookup JOIN 键
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
