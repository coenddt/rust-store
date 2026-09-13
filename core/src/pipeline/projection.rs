//! $project 投影构建

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::permission::{get_readable_fields, Context};
use crate::schema::{ComputeDef, Schema};

use super::ast::Ast;

/// 从投影中移除当前用户不可读的字段
fn apply_permission_prune(proj: &mut Map<String, Value>, schema: &Schema, ctx: Option<&Context>) {
    let Some(c) = ctx else { return };
    let Some(readable) = get_readable_fields(schema, Some(c)) else {
        return;
    };
    let keys: Vec<String> = proj.keys().cloned().collect();
    for key in keys {
        if key == "_id" {
            continue;
        }
        if schema.fields.contains_key(&key) && !readable.contains(&key) {
            proj.remove(&key);
        }
    }
}

fn merge_compute_depends(
    proj: &mut Map<String, Value>,
    app_computes: &[&ComputeDef],
    dot_parent_fields: &HashSet<String>,
) {
    for c in app_computes {
        for dep in &c.depends {
            if dot_parent_fields.contains(dep) {
                continue;
            }
            // R10：depends 为关系子字段形式（`lessons{duration}`）时，**绝不**把原始
            // 子字段串当投影列注入（否则 SQL 生成非法列 `t.lessons{duration}`）；
            // 退化为注入关系根名，字段数据经关系 `$lookup`/应用层计算列读取。
            if dep.contains('{') {
                let root = dep.split('{').next().unwrap_or(dep.as_str()).trim();
                if !root.is_empty() {
                    proj.entry(root.to_string()).or_insert(json!(1));
                }
                continue;
            }
            proj.entry(dep.clone()).or_insert(json!(1));
        }
    }
}

/// 按计算列 depends 把依赖字段并入投影
///
/// NOTE: JS 侧还有一条 `_mergeAllSchemaFields` 兜底分支
/// （`appComputes.every(c => Array.isArray(c.depends))` 为假时走全字段并入），
/// 但 `register()` 规范化时恒有 `depends: val.depends || []`，故该条件恒为真、
/// 兜底分支实际不可达 —— 这里只移植可达路径，待 P0 契约确认后再定去留。
fn append_compute_deps(
    proj: &mut Map<String, Value>,
    schema: &Schema,
    dot_parent_fields: &HashSet<String>,
) {
    let app_computes: Vec<&ComputeDef> = schema
        .computes
        .iter()
        .map(|(_, c)| c)
        .filter(|c| c.has_fn || c.has_async_fn)
        .collect();
    if app_computes.is_empty() {
        return;
    }
    merge_compute_depends(proj, &app_computes, dot_parent_fields);
}

/// 收集真实 schema 字段的投影条目（排除计算列/点号重复）
fn collect_real_fields(
    ast: &Ast,
    schema: &Schema,
    compute_keys: &HashSet<String>,
) -> (Map<String, Value>, HashSet<String>, bool) {
    let mut proj: Map<String, Value> = Map::new();
    let mut dot_parent_fields: HashSet<String> = HashSet::new();
    let mut any_field_requested: HashSet<String> = HashSet::new();
    let mut has_real_field = false;

    for f in &ast.fields {
        if compute_keys.contains(f) {
            continue;
        }
        if f.contains('.') {
            let root = f.split('.').next().unwrap_or("").to_string();
            if schema.fields.contains_key(&root) {
                dot_parent_fields.insert(root.clone());
                if !any_field_requested.contains(&root) {
                    proj.insert(f.clone(), json!(1));
                }
                has_real_field = true;
            }
        } else if schema.fields.contains_key(f) {
            proj.insert(f.clone(), json!(1));
            any_field_requested.insert(f.clone());
            has_real_field = true;
        }
    }

    // 如果父字段被整个请求，移除其 dot-notation 子条目
    if has_real_field {
        for root in dot_parent_fields.iter().cloned().collect::<Vec<_>>() {
            if proj.contains_key(&root) {
                for key in proj.keys().cloned().collect::<Vec<_>>() {
                    if key.starts_with(&format!("{}.", root)) {
                        proj.remove(&key);
                    }
                }
            }
        }
    }

    (proj, dot_parent_fields, has_real_field)
}

/// R9：`$pipeline` 模式的顶层投影 —— 只按请求保留 `_id` + 真实 schema 字段，
/// 不追加 compute 层/依赖/关系（`$pipeline` 语义：按用户 pipeline 输出，仅做字段选择）。
/// 无有效真实字段（全为计算列/空）时返回 None（不施加投影，原样输出）。
pub fn build_pipeline_projection(ast: &Ast, schema: &Schema) -> Option<Value> {
    let mut proj: Map<String, Value> = Map::new();
    proj.insert("_id".to_string(), json!(1));
    let mut any = false;
    for f in &ast.fields {
        if f == "_id" {
            continue;
        }
        if schema.fields.contains_key(f) {
            proj.insert(f.clone(), json!(1));
            any = true;
        }
    }
    if !any {
        return None;
    }
    Some(Value::Object(proj))
}

/// 从 GQL 根字段列表 + schema computes 计算投影；无有效字段时返回 None
pub fn build_projection(ast: &Ast, schema: &Schema, ctx: Option<&Context>) -> Option<Value> {
    if ast.fields.is_empty() {
        return None;
    }

    let compute_keys: HashSet<String> = schema.computes.iter().map(|(k, _)| k.clone()).collect();
    let mut proj: Map<String, Value> = Map::new();
    proj.insert("_id".to_string(), json!(1));

    // 仅包含实际 schema 字段（排除计算列与点号重复）
    let (fields_proj, dot_parent_fields, has_real_field) =
        collect_real_fields(ast, schema, &compute_keys);
    for (k, v) in fields_proj {
        proj.insert(k, v);
    }
    if !has_real_field {
        return None;
    }

    // 收集需要在应用层执行的计算列（fn + asyncFn），其依赖字段不能被投影排除
    append_compute_deps(&mut proj, schema, &dot_parent_fields);

    // R8：数据库阶段已物化的 lookup 计算列（如 lessonCount=$size）必须保留在
    // 顶层 $project 输出，否则会被 $project 丢弃。fn/asyncFn 计算列在应用层求值，
    // 只需保依赖（见上），不在这里投影其字段本身。
    let lookup_compute_keys: HashSet<&str> = schema
        .computes
        .iter()
        .filter(|(_, c)| !c.has_fn && !c.has_async_fn && c.lookup.is_some())
        .map(|(k, _)| k.as_str())
        .collect();
    for f in &ast.fields {
        if lookup_compute_keys.contains(f.as_str()) {
            proj.insert(f.clone(), json!(1));
        }
    }

    // 关系名加入投影（否则 $project 阶段会丢弃 $lookup 的结果）
    for (rel_name, _) in &ast.relations {
        if !proj.contains_key(rel_name) {
            proj.insert(rel_name.clone(), json!(1));
        }
    }

    // 权限裁剪：从投影中移除当前用户不可读的字段
    apply_permission_prune(&mut proj, schema, ctx);

    Some(Value::Object(proj))
}
