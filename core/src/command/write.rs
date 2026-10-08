//! 写路径与简单查询计划

use serde_json::{json, Map, Value};

use crate::computes::{apply_defaults_and_computes, FnRegistry};
use crate::permission::{can_write_schema, evaluate, merge_owner_condition, Context, Doc};
use crate::rbac::{
    ensure_write, ensure_write_on_doc, filter_writable_data_overlay, merge_row_condition,
    probe_condition_fields, WriteAction,
};
use crate::schema::{Registry, Schema};
use crate::types::{
    has_relation_predicate, is_truthy, validate_condition, validate_condition_shape,
};

use super::cmd::{cmd_count_documents, cmd_find_one, cmd_insert_one};
use super::write_links::attach_write_links;
use super::{ensure_context, ERR_NO_WRITE};

/// 生成插入命令（对应 JS `insert`）
///
/// `now` 与 `new_id` 由 Host 提供（core 无时钟与随机源，且这样可让产出确定、便于对拍）。
pub fn plan_insert(
    schema_name: &str,
    registry: &Registry,
    ctx: Option<&Context>,
    data: &Value,
    now: i64,
    new_id: &str,
    fn_registry: Option<&dyn FnRegistry>,
) -> Result<Value, String> {
    ensure_context(registry, ctx)?;
    let schema = registry.get(schema_name)?;

    if !can_write_schema(registry.role_rules(), schema, ctx) {
        return Err(ERR_NO_WRITE.to_string());
    }
    // RBAC 表级写判定（deny-wins，叠加于静态白名单之后）
    ensure_write(registry, schema, ctx, WriteAction::Insert)?;

    let doc = build_insert_doc(registry, schema, ctx, data, now, new_id)?;
    let doc = Value::Object(doc);
    let plan = json!({
        "command": cmd_insert_one(schema, &doc),
        "returns": apply_defaults_and_computes(&doc, schema, fn_registry)?,
    });
    // 写链路附接（多落点 schema 才追加 writeLinks；单落点原样返回）
    attach_write_links(plan, registry, &[schema_name], registry.write_link_policy())
}

/// insert 文档规范化（对应 JS `insert` 主体）：
/// 权限过滤 → 保留显式 null → 自动 _id / createdBy / 时间戳
pub(super) fn build_insert_doc(
    registry: &Registry,
    schema: &Schema,
    ctx: Option<&Context>,
    data: &Value,
    now: i64,
    new_id: &str,
) -> Result<Map<String, Value>, String> {
    let filtered = match ctx {
        // RBAC 感知版：静态 writable ∩ RBAC writeFields（无策略时直通静态过滤）
        Some(_) => filter_writable_data_overlay(registry, schema, ctx, data),
        None => data.clone(),
    };

    // 保留显式 null：历史实现把 null 一律剔除（当成缺失），导致「空串/NULL/缺失」三态
    // 无法区分（F-07），null 字段读回也被吞（H-01）。显式 `null` 应原样落库，SQL 落 NULL。
    let mut doc: Map<String, Value> = filtered.as_object().cloned().unwrap_or_default();

    // 自动生成 ID（仅在 schema 配了 idPrefix 且未提供有效 _id 时）；
    // 无 idPrefix 且无 _id → 显式报错（缺陷 D-03）：静默产出「无 _id 文档」会让
    // Host 返回 _id=undefined，且 Mongo 自动 ObjectId 会触发跨绑定序列化崩溃
    if !doc.get("_id").map(is_truthy).unwrap_or(false) {
        if schema.id_is_autoincrement() {
            // 阶段2：数据库自增主键 —— doc 不注入 `_id`（SQL INSERT 列白名单来自 doc 键，
            // 无 `_id` 列即由数据库赋值；PG/SQLite 走 RETURNING 回读、MySQL 走宿主
            // lastrowid）。Mongo 无自增语义，由宿主 Mongo 执行器对此形态显式报错
            // （AUTOINCREMENT_NOT_SUPPORTED），禁 ObjectId 静默顶替。
        } else if !schema.id_prefix.is_empty() {
            doc.insert("_id".to_string(), json!(new_id));
        } else {
            return Err(format!(
                "schema \"{}\" 未配置 idPrefix 且未提供有效 _id：请显式提供 _id，或为 schema 配置 idPrefix",
                schema.name
            ));
        }
    }

    // 自动设置 createdBy（creator 权限场景）
    if has_creator_permission(schema) && !doc.get("createdBy").map(is_truthy).unwrap_or(false) {
        if let Some(uid) = ctx.and_then(|c| c.user_id.clone()) {
            doc.insert("createdBy".to_string(), json!(uid));
        }
    }

    // 自动时间戳
    if schema.timestamps {
        if !doc.get("createdAt").map(is_truthy).unwrap_or(false) {
            doc.insert("createdAt".to_string(), json!(now));
        }
        doc.insert("updatedAt".to_string(), json!(now));
    }
    Ok(doc)
}

/// 判断存在性（对应 JS `exists`）
pub fn plan_exists(
    schema_name: &str,
    registry: &Registry,
    condition: &Value,
) -> Result<Value, String> {
    // 条件拒绝名单（缺陷 D-02）
    validate_condition(condition)?;
    let schema = registry.get(schema_name)?;
    // §11.4（D2）：与 pipeline 读路径同码拒绝 U1~U4 形态（数组/对象/点号路径）
    validate_condition_shape(schema, condition, registry.profile())?;
    // §9.6 关系聚合谓词无法用标量 `findOne` 表达 → 显式 Err（否则 Mongo 静默给错结果）
    if has_relation_predicate(schema, condition) {
        return Err(
            "exists 不支持关系聚合谓词条件（§9.6）：标量查询无法表达关系谓词，\
             请改用 query 并自行判断是否有结果"
                .to_string(),
        );
    }
    Ok(cmd_find_one(schema, condition, Some(&json!({ "_id": 1 }))))
}

/// 统计数量（对应 JS `count`）：filter 为 nullish 时用 `{}`
///
/// R1：owner(read=creator) 场景下，count 必须注入 `createdBy = ctx.userId`，
/// 否则非 admin 用户 count 会越权统计全表（E-08）。
pub fn plan_count(
    schema_name: &str,
    registry: &Registry,
    filter: Option<&Value>,
    ctx: Option<&Context>,
) -> Result<Value, String> {
    if let Some(f) = filter {
        // 条件拒绝名单（缺陷 D-02）
        validate_condition(f)?;
    }
    let schema = registry.get(schema_name)?;
    // §11.4（D2）：与 pipeline 读路径同码拒绝 U1~U4 形态（数组/对象/点号路径）
    if let Some(f) = filter {
        validate_condition_shape(schema, f, registry.profile())?;
        // §9.6 关系聚合谓词无法用标量 count 表达 → 显式 Err（与 query_with_count 同口径）
        if has_relation_predicate(schema, f) {
            return Err(
                "count 不支持关系聚合谓词条件（§9.6）：标量计数无法表达关系谓词，\
                 请改用 query 并在调用方自行统计"
                    .to_string(),
            );
        }
    }
    let base = match filter {
        None | Some(Value::Null) => json!({}),
        Some(v) => v.clone(),
    };
    // 静态 owner 条件叠加 RBAC 行条件（两引擎 $and，deny-wins）
    let owner_merged = merge_owner_condition(registry.role_rules(), schema, ctx, Some(base));
    let filter = merge_row_condition(registry, schema, ctx, "read", owner_merged)
        .unwrap_or_else(|| json!({}));
    Ok(cmd_count_documents(schema, &filter))
}

pub(super) fn has_creator_permission(schema: &Schema) -> bool {
    let has = |list: &Option<Vec<String>>| {
        list.as_ref()
            .map(|l| l.iter().any(|r| r == "creator"))
            .unwrap_or(false)
    };
    has(&schema.read) || has(&schema.write)
}

// ─── 写权限探针（对应 JS `_checkWritePerm`） ─────────────────

/// creator 写权限的探针状态：
/// `NotProbed` → 尚未探查（可能返回需要探针的命令）
/// `NoResult` → 探针无结果（拒绝）
/// `Found`    → 探针命中的 `{_id, createdBy}` 文档
#[derive(Debug, Clone, Copy)]
pub enum Probe<'a> {
    NotProbed,
    NoResult,
    Found(&'a Value),
}

/// Schema 级写权限检查（对应 JS `_checkWritePerm`）：
/// 拒写清单命中直接拒绝；非写授权时仅 creator 命中才放行（需 Host 先执行探针命令）。
///
/// RBAC 叠加（deny-wins）：
/// - 表级：`ensure_write(action)` 在 guest 检查后判定，deny 即拒；
/// - 行级：`row_condition` 非空（ownerOnly / condition）时**同样需要探针**——
///   静态放行的路径也要先取 `{_id, createdBy, condition 键}` 再重入判定
///   （[`ensure_write_on_doc`]），杜绝 ownerOnly 单条更新静默命中他人文档；
/// - 探针重入（`Probe::Found`）时静态 `evaluate` 与 RBAC 行级判定都过才放行。
///
/// `extra_probe_fields`：触发器探针需额外投影的字段（`onFields` ∪ `{{before.*}}`
/// 引用，见 `command::triggers::before_probe_fields`）。非空时即使权限本不需探针
/// 也发探针（Host 据探针结果替换 `{{before.*}}` 并判定字段级命中）；为空时行为
/// 与无参版本**逐字节等价**（零回归契约）。
///
/// 返回：`Ok(None)` = 放行；`Ok(Some(cmd))` = Host 先执行该 findOne 探针后携
/// [`Probe::Found`] / [`Probe::NoResult`] 重入；`Err` = 拒绝（Host 映射 PermissionError）。
#[allow(clippy::too_many_arguments)]
pub fn check_write_perm(
    registry: &Registry,
    schema: &Schema,
    ctx: Option<&Context>,
    condition: &Value,
    deny_msg: &str,
    probe: Probe,
    action: WriteAction,
    extra_probe_fields: &[String],
) -> Result<Option<Value>, String> {
    // JS：`if (ctx)` 才做检查 —— 无上下文（内部调用）不设防
    let Some(c) = ctx else {
        return Ok(None);
    };
    // 拒写清单（默认空——无拒写；原 guest 硬编码随清单化移除，设计 §11.4）
    let rules = registry.role_rules();
    if crate::permission::has_any_role(c, &rules.deny_write_roles) {
        return Err(deny_msg.to_string());
    }
    // RBAC 表级写判定（deny-wins；拒绝文案自带 RBAC 标识，不走入参 deny_msg）
    ensure_write(registry, schema, Some(c), action)?;
    // RBAC 行级限制是否存在（存在则探针投影需并入 condition 字段键）
    let rbac_row_restricted =
        crate::rbac::row_condition(registry, schema, Some(c), action.as_str()).is_some();

    let probe_projection = |extra: &[String]| -> Value {
        let mut m = Map::new();
        m.insert("_id".to_string(), json!(1));
        m.insert("createdBy".to_string(), json!(1));
        for f in extra {
            // 仅取 schema 已声明字段（防触发器声明引用未声明键污染投影）
            if schema.fields.contains_key(f) {
                m.insert(f.clone(), json!(1));
            }
        }
        Value::Object(m)
    };
    // 探针投影字段全集：RBAC 行级条件字段 ∪ 触发器 onFields / before 引用字段
    let mut merged = probe_condition_fields(registry, schema, Some(c), action.as_str());
    for f in extra_probe_fields {
        if !merged.contains(f) {
            merged.push(f.clone());
        }
    }

    if can_write_schema(registry.role_rules(), schema, Some(c)) {
        // 静态放行：RBAC 行级受限时仍需探针（否则静默放行越权行更新/删除）
        if rbac_row_restricted && is_truthy(condition) {
            return match probe {
                Probe::NotProbed => Ok(Some(cmd_find_one(
                    schema,
                    condition,
                    Some(&probe_projection(&merged)),
                ))),
                Probe::NoResult => Err(deny_msg.to_string()),
                Probe::Found(doc) => {
                    ensure_write_on_doc(registry, schema, Some(c), action, doc).map(|_| None)
                }
            };
        }
        // 触发器需要 before 值 / 字段级命中判定 → 权限本不需探针也发探针
        // （静态权限已由 can_write_schema 放行；Found 重入直接放行）
        if !extra_probe_fields.is_empty() {
            return match probe {
                Probe::NotProbed => Ok(Some(cmd_find_one(
                    schema,
                    condition,
                    Some(&probe_projection(&merged)),
                ))),
                Probe::NoResult => Err(deny_msg.to_string()),
                Probe::Found(_) => Ok(None),
            };
        }
        return Ok(None);
    }
    let creator_only = schema
        .write
        .as_ref()
        .map(|w| w.iter().any(|r| r == "creator"))
        .unwrap_or(false);
    if creator_only && is_truthy(condition) {
        let needs_row_check = rbac_row_restricted;
        return match probe {
            Probe::NotProbed => Ok(Some(cmd_find_one(
                schema,
                condition,
                Some(&probe_projection(&merged)),
            ))),
            Probe::NoResult => Err(deny_msg.to_string()),
            Probe::Found(doc) => {
                if evaluate(
                    registry.role_rules(),
                    Some(c),
                    schema.write.as_deref(),
                    Doc::Doc(doc),
                ) {
                    if needs_row_check {
                        // 静态 creator 过 → RBAC 行级再判（两引擎都过才放行）
                        ensure_write_on_doc(registry, schema, Some(c), action, doc).map(|_| None)
                    } else {
                        Ok(None)
                    }
                } else {
                    Err(deny_msg.to_string())
                }
            }
        };
    }
    Err(deny_msg.to_string())
}
