//! processNode 同步处理链：默认值填充 → 同步 fn 计算列 → 递归下钻 → 权限裁剪。

use std::collections::HashSet;

use serde_json::Value;

use crate::permission::{
    evaluate, get_readable_computes, get_readable_fields, get_readable_relations, Context, Doc,
};
use crate::pipeline::{flatten_object_fields_impl, RelAst};
use crate::schema::{Registry, Schema};

use super::super::cache::{ensure_cache, Cache};
use super::super::defaults::{fill_nested_defaults, is_object_field, raw_field_default};
use super::super::registry::FnRegistry;

/// GQL 请求字段 + fn 计算列依赖字段（去重，保序）
fn collect_needed(fields: &[String], cache: &Cache) -> Vec<String> {
    let mut needed: Vec<String> = fields.to_vec();
    let mut seen: HashSet<String> = needed.iter().cloned().collect();
    for entry in &cache.fn_list {
        for dep in &entry.depends {
            if seen.insert(dep.clone()) {
                needed.push(dep.clone());
            }
        }
    }
    needed
}

/// 收集点号嵌套字段信息 `root → [subPath, ...]`
fn collect_dot(fields: &[String]) -> Vec<(String, Vec<String>)> {
    let mut dot_fields: Vec<(String, Vec<String>)> = Vec::new();
    for f in fields {
        let Some(idx) = f.find('.') else { continue };
        let root = &f[..idx];
        let sub = &f[idx + 1..];
        match dot_fields.iter_mut().find(|(k, _)| k == root) {
            Some((_, subs)) => subs.push(sub.to_string()),
            None => dot_fields.push((root.to_string(), vec![sub.to_string()])),
        }
    }
    dot_fields
}

/// 补字段默认值（含点号字段的根字段）
fn fill_defaults(
    doc: &mut Value,
    cache: &Cache,
    needed: &[String],
    dot_fields: &[(String, Vec<String>)],
) {
    let Some(o) = doc.as_object_mut() else { return };
    let mut fill = |key: &str| {
        let absent = o.get(key).map(|v| v.is_null()).unwrap_or(true);
        if absent {
            if let Some(d) = cache.field_defaults.get(key) {
                o.insert(key.to_string(), d.clone());
            }
        }
    };
    for key in needed {
        fill(key);
    }
    for (root, _) in dot_fields {
        fill(root);
    }
}

/// 填充嵌套 object 的点号精确子字段默认值
fn fill_dot_nested(doc: &mut Value, schema: &Schema, root: &str, sub_path: &str) {
    let Some(field) = schema.fields.get(root) else {
        return;
    };
    if !is_object_field(field) {
        return;
    }
    let Some(sub_def) = field
        .fields
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|m| m.get(sub_path))
        .cloned()
    else {
        return;
    };
    let Some(root_obj) = doc.as_object_mut().and_then(|o| o.get_mut(root)) else {
        return;
    };
    let Some(ro) = root_obj.as_object_mut() else {
        return;
    };
    let absent = ro.get(sub_path).map(|v| v.is_null()).unwrap_or(true);
    if absent {
        if let Some(d) = raw_field_default(&sub_def) {
            ro.insert(sub_path.to_string(), d);
        }
    }
}

/// 递归填充嵌套 object 子字段默认值（含点号精确子字段）
fn fill_nested_objects(doc: &mut Value, schema: &Schema, needed: &[String]) {
    for key in needed {
        match key.find('.') {
            Some(idx) => fill_dot_nested(doc, schema, &key[..idx], &key[idx + 1..]),
            None => {
                let Some(field) = schema.fields.get(key) else {
                    continue;
                };
                if !is_object_field(field) {
                    continue;
                }
                let Some(fd) = field.fields.clone() else { continue };
                let Some(sub) = doc.as_object_mut().and_then(|o| o.get_mut(key)) else {
                    continue;
                };
                if sub.is_object() {
                    fill_nested_defaults(sub, &fd);
                }
            }
        }
    }
}

/// 跑 fn 计算列，再补计算列默认值（对应 JS `_runComputes`）
pub fn run_computes(
    doc: &mut Value,
    cache: &Cache,
    fn_registry: Option<&dyn FnRegistry>,
) -> Result<(), String> {
    if cache.fn_list.is_empty() {
        return Ok(());
    }
    // `doc[key] = fn(doc)`；fn 声明了但 Host 未注册实现 → 报错（与 JS 直接调用缺失函数一致）
    for entry in &cache.fn_list {
        let v = match fn_registry {
            Some(r) => r.call_sync(&entry.fn_ref, doc)?,
            None => return Err(format!("计算列 {} 未注册同步实现", entry.key)),
        };
        if let Some(o) = doc.as_object_mut() {
            o.insert(entry.key.clone(), v);
        }
    }
    // 补计算列默认值：fn 返回 nullish 且 type 有零值时
    for entry in &cache.fn_list {
        let Some(o) = doc.as_object_mut() else { continue };
        let absent = o.get(&entry.key).map(|v| v.is_null()).unwrap_or(true);
        if absent {
            if let Some(d) = cache.compute_defaults.get(&entry.key) {
                if !d.is_null() {
                    o.insert(entry.key.clone(), d.clone());
                }
            }
        }
    }
    Ok(())
}

/// 递归下钻嵌套关系文档（跳过不可读关系）
fn descend_relations(
    doc: &mut Value,
    relations: &mut [(String, RelAst)],
    schema: &Schema,
    ctx: Option<&Context>,
    registry: &Registry,
    fn_registry: Option<&dyn FnRegistry>,
) -> Result<(), String> {
    let readable = if ctx.is_some() {
        get_readable_relations(schema, ctx)
    } else {
        None
    };

    for (rel_name, rel_ast) in relations.iter_mut() {
        if let Some(set) = &readable {
            if !set.contains(rel_name) {
                continue;
            }
        }
        let Some(rel_def) = schema.relations.get(&*rel_name) else {
            continue;
        };
        let rel_schema = registry.get(&rel_def.model)?;

        let Some(rel_val) = doc.as_object_mut().and_then(|o| o.get_mut(&*rel_name)) else {
            continue;
        };
        match rel_val {
            Value::Array(arr) => {
                for item in arr.iter_mut() {
                    process_node(
                        item,
                        &mut rel_ast.fields,
                        &mut rel_ast.relations,
                        rel_schema,
                        ctx,
                        registry,
                        fn_registry,
                    )?;
                }
            }
            Value::Object(_) => {
                process_node(
                    rel_val,
                    &mut rel_ast.fields,
                    &mut rel_ast.relations,
                    rel_schema,
                    ctx,
                    registry,
                    fn_registry,
                )?;
            }
            _ => {}
        }
    }

    Ok(())
}

/// 仅保留 GQL 字段 + 关系名 + 点号根字段（_id 始终保留）
fn compute_keep(
    fields: &[String],
    relations: &[(String, RelAst)],
    dot_fields: &[(String, Vec<String>)],
) -> HashSet<String> {
    let mut keep: HashSet<String> = fields.iter().cloned().collect();
    for (root, _) in dot_fields {
        keep.insert(root.clone());
    }
    for (rel_name, _) in relations {
        keep.insert(rel_name.clone());
    }
    keep.insert("_id".to_string());
    keep
}

/// 从 keep 中移除用户不可读的字段 / 计算列 / 关系（读权限，不含 Owner 级）
fn apply_readable_prune(
    fields: &[String],
    relations: &[(String, RelAst)],
    schema: &Schema,
    ctx: Option<&Context>,
    keep: &mut HashSet<String>,
) {
    let readable_fields = get_readable_fields(schema, ctx);
    let readable_computes = get_readable_computes(schema, ctx);
    let readable_relations = get_readable_relations(schema, ctx);

    for key in fields {
        if key == "_id" {
            continue;
        }
        let unreadable_field = schema.fields.contains_key(key)
            && readable_fields
                .as_ref()
                .map(|s| !s.contains(key))
                .unwrap_or(false);
        let unreadable_compute = schema.compute(key).is_some()
            && readable_computes
                .as_ref()
                .map(|s| !s.contains(key))
                .unwrap_or(false);
        if unreadable_field || unreadable_compute {
            keep.remove(key);
        }
    }

    if let Some(rels) = readable_relations {
        for (rel_name, _) in relations {
            if !rels.contains(rel_name) {
                keep.remove(rel_name);
            }
        }
    }
}

/// 字段 / 计算列 / 关系的 Owner read 校验（逐条传入 doc 做 creator 检查）
fn apply_owner_read_prune(
    doc: &Value,
    fields: &[String],
    relations: &[(String, RelAst)],
    schema: &Schema,
    ctx: Option<&Context>,
    keep: &mut HashSet<String>,
) {
    for key in fields {
        if key == "_id" || !keep.contains(key) {
            continue;
        }
        if let Some(field) = schema.fields.get(key) {
            if let Some(rl) = &field.read {
                if !evaluate(ctx, Some(rl), Doc::Doc(doc)) {
                    keep.remove(key);
                }
            }
        }
    }

    for (key, comp) in &schema.computes {
        if !keep.contains(key) {
            continue;
        }
        if let Some(rl) = &comp.read {
            if !evaluate(ctx, Some(rl), Doc::Doc(doc)) {
                keep.remove(key);
            }
        }
    }

    for (rel_name, _) in relations {
        if !keep.contains(rel_name) {
            continue;
        }
        if let Some(rel) = schema.relations.get(rel_name) {
            if let Some(rl) = &rel.read {
                if !evaluate(ctx, Some(rl), Doc::Doc(doc)) {
                    keep.remove(rel_name);
                }
            }
        }
    }
}

/// 裁剪点号字段父对象中未请求的子字段
fn prune_dot_subfields(doc: &mut Value, dot_fields: &[(String, Vec<String>)]) {
    for (root, subs) in dot_fields {
        let Some(obj) = doc.as_object_mut().and_then(|o| o.get_mut(root)) else {
            continue;
        };
        if let Some(m) = obj.as_object_mut() {
            m.retain(|k, _| subs.contains(k));
        }
    }
}

/// 递归处理单条文档：补默认值 → 跑 fn 计算列 → 补计算列默认值 → 递归下钻 → 裁剪 → 权限裁剪
///
/// 时序严格（对应 JS `processNode`）：
///   1. 展平 object 子字段 → 2. 收集 needed / dotFields → 3. 补字段默认值
///   → 4. 跑 fn 计算列 + 补计算列默认值 → 5. 递归下钻 → 6. 权限裁剪 → 7. 裁剪字段
#[allow(clippy::too_many_arguments)]
pub fn process_node(
    doc: &mut Value,
    fields: &mut Vec<String>,
    relations: &mut Vec<(String, RelAst)>,
    schema: &Schema,
    ctx: Option<&Context>,
    registry: &Registry,
    fn_registry: Option<&dyn FnRegistry>,
) -> Result<(), String> {
    if doc.is_null() {
        return Ok(());
    }

    let cache = ensure_cache(schema);

    // ① 展平 object 子字段花括号语法 → dot-notation
    flatten_object_fields_impl(fields, relations, schema);

    // ② 收集 needed 字段（请求字段 + fn 依赖）与点号字段信息
    let needed = collect_needed(fields, &cache);
    let dot_fields = collect_dot(fields);

    // ③ 补字段默认值 + 递归填充嵌套 object 子字段
    fill_defaults(doc, &cache, &needed, &dot_fields);
    fill_nested_objects(doc, schema, &needed);

    // ④ 跑 fn 计算列 + 补计算列默认值
    run_computes(doc, &cache, fn_registry)?;

    // ⑤ 递归下钻（跳过不可读的关系）
    descend_relations(doc, relations, schema, ctx, registry, fn_registry)?;

    // ⑥ 计算 keep（GQL 字段 + 关系名 + 点号根字段 + _id）
    let mut keep = compute_keep(fields, relations, &dot_fields);

    // ⑦ 权限裁剪（读权限 + Owner 级 read 校验）
    if ctx.is_some() {
        apply_readable_prune(fields, relations, schema, ctx, &mut keep);
        apply_owner_read_prune(doc, fields, relations, schema, ctx, &mut keep);
    }

    // ⑧ 裁剪字段 + 点号父对象中未请求的子字段
    if let Some(o) = doc.as_object_mut() {
        o.retain(|k, _| keep.contains(k));
    }
    prune_dot_subfields(doc, &dot_fields);

    Ok(())
}
