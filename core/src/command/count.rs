//! 列表 + total 计划

use serde_json::{json, Map, Value};

use crate::permission::{merge_owner_condition, Context};
use crate::pipeline::{param, parse_gql};
use crate::schema::Registry;
use crate::types::is_truthy;

use super::cmd::{cmd_count_documents, num_value};
use super::query::{plan_query_ast_mut, resolve_page, Page, QueryPlan};

#[derive(Debug, Clone)]
pub struct CountQueryPlan {
    pub query: QueryPlan,
    pub page: Page,
    /// `total` 统计命令（忽略 skip/limit，但含所有者条件）
    pub count_command: Value,
}

impl CountQueryPlan {
    pub fn to_value(&self) -> Value {
        let mut m = self
            .query
            .to_value()
            .as_object()
            .cloned()
            .unwrap_or_default();
        m.insert("countCommand".to_string(), self.count_command.clone());
        if let Some(o) = self.page.to_value().as_object() {
            for (k, v) in o {
                m.insert(k.clone(), v.clone());
            }
        }
        Value::Object(m)
    }

    /// `hasMore = (page + 1) * pageSize < total`
    pub fn has_more(&self, total: f64) -> bool {
        self.page.has_more(total)
    }
}

/// 生成「列表 + total」命令序列（对应 JS `queryWithCount`）
pub fn plan_query_with_count(
    gql: &str,
    params: &Map<String, Value>,
    registry: &Registry,
    ctx: Option<&Context>,
) -> Result<CountQueryPlan, String> {
    let mut ast = parse_gql(gql)?;
    let schema = registry.get(&ast.model)?;

    let mut params = params.clone();
    let page = resolve_page(&ast, &params);

    // R5：让 page/pageSize 真正落库为 LIMIT/OFFSET。
    // 当用户经 params 传了 page/pageSize 而 GQL 未显式引用 $skip/$limit 时，
    // 注入合成引用 + 参数，否则 build_pipeline 不产生分页阶段 → 返回全表（A-18 根因）。
    let wants_pagination = params.contains_key("page") || params.contains_key("pageSize");
    if wants_pagination && !ast.params.contains_key("skip") {
        ast.params
            .insert("skip".to_string(), "@__core_page_skip__".to_string());
        params.insert(
            "__core_page_skip__".to_string(),
            num_value(page.page * page.page_size),
        );
    }
    if wants_pagination && !ast.params.contains_key("limit") {
        ast.params
            .insert("limit".to_string(), "@__core_page_limit__".to_string());
        params.insert(
            "__core_page_limit__".to_string(),
            num_value(page.page_size),
        );
    }

    // 确保 GQL 实际使用上述分页值（GQL 已引用 $skip/$limit 时用推导值覆盖，防宿主参数漂移）
    if let Some(r) = ast.params.get("skip").cloned() {
        let key = r.get(1..).unwrap_or("").to_string();
        params.insert(key, num_value(page.page * page.page_size));
    }
    if let Some(r) = ast.params.get("limit").cloned() {
        let key = r.get(1..).unwrap_or("").to_string();
        params.insert(key, num_value(page.page_size));
    }

    // 统计 total：取（已被 owner 注入过的）条件后再叠加一次 owner 条件，与 JS 行为一致
    let count_filter = ast
        .params
        .get("condition")
        .and_then(|r| param(&params, Some(r)).cloned())
        .filter(is_truthy)
        .unwrap_or_else(|| json!({}));
    let count_filter =
        merge_owner_condition(schema, ctx, Some(count_filter)).unwrap_or_else(|| json!({}));

    let query = plan_query_ast_mut(ast, &mut params, registry, ctx)?;

    let count_command = cmd_count_documents(schema, &count_filter);

    Ok(CountQueryPlan {
        query,
        page,
        count_command,
    })
}
