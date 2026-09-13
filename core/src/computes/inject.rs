//! asyncFn depends 注入 / 剔除（查询前注入 GQL 片段，查询后裁掉注入字段）

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::pipeline::{parse, tokenize, RelAst};
use crate::schema::Schema;

use super::cache::ensure_cache;

/// 单个关系的依赖字段需求（保序）
#[derive(Debug, Clone, PartialEq)]
pub struct RelDep {
    pub name: String,
    /// 需要的字段；空表示只依赖关系本身（注入整个关系）
    pub fields: Vec<String>,
}

impl RelDep {
    pub fn to_value(&self) -> Value {
        Value::Object(Map::from_iter([
            ("name".to_string(), Value::String(self.name.clone())),
            (
                "fields".to_string(),
                Value::Array(self.fields.iter().cloned().map(Value::String).collect()),
            ),
        ]))
    }
}

/// 注入信息：新增关系 → `All`（整条返回且需整体剔除）；已有关系补齐字段 → `Fields`
#[derive(Debug, Clone, PartialEq)]
pub enum Injected {
    All,
    Fields(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct InjectInfo {
    pub relations: Vec<(String, Injected)>,
    /// R7：asyncFn 计算列的**普通字段**依赖（非关系子字段），注入根 AST 后供 process_node
    /// 保留、宿主 asyncFn 读取，随后在 strip 阶段从根文档剥离，避免泄漏到最终输出。
    pub fields: Vec<String>,
}

impl InjectInfo {
    pub fn is_empty(&self) -> bool {
        self.relations.is_empty() && self.fields.is_empty()
    }

    /// 序列化为 JS 侧形状（Set → 数组，`'__all__'` → 字符串）
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        for (name, inj) in &self.relations {
            let v = match inj {
                Injected::All => Value::String("__all__".to_string()),
                Injected::Fields(f) => Value::Array(f.iter().cloned().map(Value::String).collect()),
            };
            m.insert(name.clone(), v);
        }
        let mut root = Map::new();
        if let Some(rm) = Value::Object(m).as_object() {
            if !rm.is_empty() {
                root.insert("relations".to_string(), Value::Object(rm.clone()));
            }
        }
        if !self.fields.is_empty() {
            root.insert(
                "fields".to_string(),
                Value::Array(self.fields.iter().cloned().map(Value::String).collect()),
            );
        }
        Value::Object(Map::from_iter([(
            "inject".to_string(),
            Value::Object(root),
        )]))
    }

    /// 从 [`to_value`] 产出的 JSON 重建（供 finalize_query 从 plan.postprocess 还原）
    pub fn from_value(v: &Value) -> InjectInfo {
        let mut relations = Vec::new();
        let mut fields = Vec::new();
        if let Some(Value::Object(m)) = v.get("inject").or(Some(v)) {
            if let Some(Value::Object(rm)) = m.get("relations") {
                for (name, inj) in rm {
                    let parsed = match inj {
                        Value::String(s) if s == "__all__" => Injected::All,
                        Value::Array(arr) => Injected::Fields(
                            arr.iter()
                                .map(|f| f.as_str().unwrap_or_default().to_string())
                                .collect(),
                        ),
                        _ => continue,
                    };
                    relations.push((name.clone(), parsed));
                }
            }
            if let Some(Value::Array(fa)) = m.get("fields") {
                fields = fa
                    .iter()
                    .filter_map(|f| f.as_str().map(String::from))
                    .collect();
            }
        }
        InjectInfo { relations, fields }
    }
}

fn dep_slot<'a>(deps: &'a mut Vec<RelDep>, name: &str) -> &'a mut RelDep {
    let idx = match deps.iter().position(|d| d.name == name) {
        Some(i) => i,
        None => {
            deps.push(RelDep {
                name: name.to_string(),
                fields: Vec::new(),
            });
            deps.len() - 1
        }
    };
    &mut deps[idx]
}

/// R7：收集 asyncFn 计算列的**普通字段**依赖（非关系子字段、非关系名、非 `_id`），
/// 并入根 AST 字段（供投影取数与 `process_node` 裁剪保留），返回被注入的字段列表。
///
/// 关系子字段/关系名依赖由 [`collect_rel_deps`] 处理；这里只处理根级标量字段。
pub fn collect_field_deps(fields: &mut Vec<String>, schema: &Schema) -> Vec<String> {
    let cache = ensure_cache(schema);
    let mut added = Vec::new();
    for entry in &cache.async_fn_list {
        for dep in &entry.depends {
            let trimmed = dep.trim();
            if trimmed.is_empty() || trimmed == "_id" {
                continue;
            }
            // 关系子字段（`lessons{duration}`）或关系名 → 由 collect_rel_deps 负责，跳过
            if trimmed.contains('{') || schema.relations.contains_key(trimmed) {
                continue;
            }
            if !fields.contains(&trimmed.to_string()) {
                fields.push(trimmed.to_string());
                added.push(trimmed.to_string());
            }
        }
    }
    added
}

/// 收集所有 asyncFn 计算列 depends 中的关系字段需求（对应 JS `_collectRelDeps`）
pub fn collect_rel_deps(schema: &Schema) -> Result<Vec<RelDep>, String> {
    let cache = ensure_cache(schema);
    let mut deps: Vec<RelDep> = Vec::new();

    for entry in &cache.async_fn_list {
        for dep in &entry.depends {
            let trimmed = dep.trim();
            if trimmed.is_empty() || trimmed == "_id" {
                continue;
            }
            if trimmed.contains('{') {
                let parsed = parse(&tokenize(trimmed))?;
                if !schema.relations.contains_key(&parsed.model) {
                    continue;
                }
                let slot = dep_slot(&mut deps, &parsed.model);
                for f in &parsed.fields {
                    if f != "_id" && !slot.fields.contains(f) {
                        slot.fields.push(f.clone());
                    }
                }
            } else {
                if !schema.relations.contains_key(trimmed) {
                    continue;
                }
                dep_slot(&mut deps, trimmed);
            }
        }
    }

    Ok(deps)
}

/// 把关系字段需求合并注入到 AST（对应 JS `_injectIntoAst`）
pub fn inject_into_ast(relations: &mut Vec<(String, RelAst)>, rel_deps: &[RelDep]) -> InjectInfo {
    let mut info = InjectInfo::default();

    for dep in rel_deps {
        match relations.iter().position(|(n, _)| n == &dep.name) {
            Some(idx) => {
                let (_, rel_ast) = &mut relations[idx];
                let mut added = Vec::new();
                for f in &dep.fields {
                    if !rel_ast.fields.contains(f) {
                        rel_ast.fields.push(f.clone());
                        added.push(f.clone());
                    }
                }
                if !added.is_empty() {
                    info.relations
                        .push((dep.name.clone(), Injected::Fields(added)));
                }
            }
            None => {
                relations.push((
                    dep.name.clone(),
                    RelAst {
                        fields: dep.fields.clone(),
                        relations: Vec::new(),
                        params: HashMap::new(),
                    },
                ));
                info.relations.push((dep.name.clone(), Injected::All));
            }
        }
    }

    info
}

/// 收集 asyncFn depends 的 GQL 片段并合并注入到查询 AST（对应 JS `_mergeDependsIntoAst`）
pub fn merge_depends_into_ast(
    relations: &mut Vec<(String, RelAst)>,
    schema: &Schema,
) -> Result<InjectInfo, String> {
    if ensure_cache(schema).async_fn_list.is_empty() {
        return Ok(InjectInfo::default());
    }
    let rel_deps = collect_rel_deps(schema)?;
    if rel_deps.is_empty() {
        return Ok(InjectInfo::default());
    }
    Ok(inject_into_ast(relations, &rel_deps))
}

/// 从结果中裁剪依赖注入的字段（对应 JS `_stripDepInjected`）
pub fn strip_dep_injected(items: &mut [Value], info: &InjectInfo) {
    if info.is_empty() {
        return;
    }
    for item in items.iter_mut() {
        let Some(o) = item.as_object_mut() else {
            continue;
        };
        // R7：剥离注入的 asyncFn 普通字段依赖（如 `name`），避免泄漏到最终输出
        if !info.fields.is_empty() {
            for f in &info.fields {
                o.remove(f);
            }
        }
        for (rel_name, injected) in &info.relations {
            match injected {
                Injected::All => {
                    o.remove(rel_name);
                }
                Injected::Fields(fields) => match o.get_mut(rel_name) {
                    Some(Value::Array(arr)) => {
                        for sub in arr.iter_mut() {
                            if let Some(so) = sub.as_object_mut() {
                                for f in fields {
                                    so.remove(f);
                                }
                            }
                        }
                    }
                    Some(Value::Object(so)) => {
                        for f in fields {
                            so.remove(f);
                        }
                    }
                    _ => {}
                },
            }
        }
    }
}
