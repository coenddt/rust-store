//! Mongo filter → WHERE 子句（参数化）
//!
//! 文件组织：本文件负责上下文与结构遍历（`build_filter` / `Ctx` / 逻辑组 / 字段条件），
//! 单字段**操作符翻译**在 [`op`]。

use serde_json::{Map, Value};

use super::Backend;

use op::cond_clause;

mod op;

/// 告警通道：`Some(&mut Vec<String>)` 表示调用方能收集翻译告警（select 侧会透出给 Host）；
/// `None` 表示**无告警能力**（写路径）——此时遇到「条件可翻译但语义需降级」的组合直接报错，
/// 宁可失败也绝不静默生成语义失真的 SQL（对齐 `write`「需告警的翻译直接报错」策略）。
pub type Warnings<'a> = Option<&'a mut Vec<String>>;

/// 单条 WHERE 片段（文本 + 已生成的参数、排序号）
#[derive(Debug, Clone)]
pub struct WhereClause {
    pub text: String,
    pub params: Vec<Value>,
}

impl WhereClause {
    fn new(text: String, params: Vec<Value>) -> Self {
        WhereClause { text, params }
    }

    pub fn and(a: WhereClause, b: WhereClause) -> WhereClause {
        let text = match (a.text.is_empty(), b.text.is_empty()) {
            (true, true) => String::new(),
            (true, false) => b.text,
            (false, true) => a.text,
            (false, false) => format!("({} AND {})", a.text, b.text),
        };
        let mut params = a.params;
        params.extend(b.params);
        WhereClause { text, params }
    }
}

/// 把 Mongo filter 翻译为 WHERE。`param_seq` 是 Postgres 占位序号游标（就地递增）。
/// `alias` 是本表别名（`t`）；`column` 是字段 → 列名的映射。
///
/// 硬化契约（缺陷 D-02）：**无法翻译的条件键一律显式报错，绝不静默丢弃** ——
/// 丢弃条件 = 生成缺 WHERE 的错误 SQL（返回全量/全表写），调用方无法区分
/// 「无数据」与「条件被丢弃」。服务端执行类操作符（`$where` 等）在规划层
/// 已被 [`crate::types::validate_condition`] 拒绝，此处为纵深防御兜底。
pub fn build_filter(
    filter: &Value,
    backend: Backend,
    alias: &str,
    column: &dyn Fn(&str) -> Option<String>,
    param_seq: &mut usize,
    warnings: Warnings,
) -> Result<WhereClause, String> {
    let mut ctx = Ctx {
        backend,
        alias,
        column,
        param_seq,
        warnings,
    };
    ctx.root(filter)
}

/// 过滤翻译上下文：打包 `backend` / `alias` / `column` / `param_seq` / `warnings`，
/// 便于按职责拆分逻辑（`build_filter` 主体已收窄到分发），
/// 并让各辅助函数的参数个数保持在 4 以内。
struct Ctx<'a> {
    backend: Backend,
    alias: &'a str,
    column: &'a dyn Fn(&str) -> Option<String>,
    param_seq: &'a mut usize,
    warnings: Warnings<'a>,
}

impl Ctx<'_> {
    /// 派生一个沿用同一参数游标与告警通道的子上下文（供递归/分支使用）
    fn child(&mut self) -> Ctx<'_> {
        Ctx {
            backend: self.backend,
            alias: self.alias,
            column: self.column,
            param_seq: &mut *self.param_seq,
            warnings: self.warnings.as_deref_mut(),
        }
    }

    /// 顶层：`null` = 匹配全部；对象 = 逐键翻译；其余类型 = 非法条件报错（缺陷 B-10-1）
    fn root(&mut self, filter: &Value) -> Result<WhereClause, String> {
        match filter {
            // `null` 显式表示「无过滤条件」→ 匹配全部（项目约定：与 `{}` 同义，见 plan_count）
            Value::Null => Ok(WhereClause::new(String::new(), Vec::new())),
            Value::Object(map) => self.object(map),
            // 非 `null` / 非对象的 filter（字符串/数字/布尔/数组）是**非法条件**：
            // 缺陷 B-10-1：此前静默返回「无过滤」→ 写路径产出无 WHERE 的无界
            // `DELETE FROM t` / `UPDATE t SET …`（一票否决：数据破坏风险），且与 Mongo
            // 语义（非文档 filter 应报错）及本模块「绝不静默丢弃条件」契约冲突 → 显式报错。
            other => Err(format!(
                "filter 必须是对象或 null，收到 {}（拒绝静默按「无过滤」处理）",
                json_type_name(other)
            )),
        }
    }

    /// 对象 filter：逻辑组（$and/$or/$nor）与字段条件**统一 AND 合并**。
    /// 缺陷 C-10-1：此前逻辑组命中即 `return`，静默丢弃同对象兄弟字段条件，
    /// 导致查询/计数/更新/删除范围被放大。
    fn object(&mut self, map: &Map<String, Value>) -> Result<WhereClause, String> {
        if map.is_empty() {
            return Ok(WhereClause::new(String::new(), Vec::new()));
        }
        let mut clauses = self.logical_clauses(map)?;
        clauses.extend(self.field_clauses(map)?);
        Ok(and_group(clauses, "AND"))
    }

    /// 逻辑组子句（固定顺序处理，确定性输出，不依赖 map 迭代序）
    fn logical_clauses(&mut self, map: &Map<String, Value>) -> Result<Vec<WhereClause>, String> {
        let mut out = Vec::new();
        for op in ["$and", "$or", "$nor"] {
            let Some(v) = map.get(op) else { continue };
            let arr = v.as_array().ok_or_else(|| format!("{op} 需要数组"))?;
            if op == "$nor" && arr.is_empty() {
                return Err("$nor 需要非空数组".to_string());
            }
            let mut sub = self.child();
            let inner = and_group(
                arr.iter()
                    .map(|f| sub.root(f))
                    .collect::<Result<Vec<_>, _>>()?,
                if op == "$and" { "AND" } else { "OR" },
            );
            out.push(if op == "$nor" {
                // $nor = NOT(OR(…))；全部分支都无法生成条件（如空对象）→ OR 匹配全部 → NOR 匹配无
                if inner.text.is_empty() {
                    WhereClause::new("0".to_string(), Vec::new())
                } else {
                    WhereClause::new(format!("NOT {}", inner.text), inner.params)
                }
            } else {
                inner
            });
        }
        Ok(out)
    }

    /// 普通字段条件（逻辑组键跳过；未识别顶层操作符显式报错 —— 缺陷 D-02）
    fn field_clauses(&mut self, map: &Map<String, Value>) -> Result<Vec<WhereClause>, String> {
        let mut out = Vec::new();
        for (field, cond) in map {
            if field.starts_with('$') {
                if matches!(field.as_str(), "$and" | "$or" | "$nor") {
                    continue;
                }
                return Err(format!(
                    "不支持的过滤条件操作符: {field}（SQL 侧无法安全翻译，拒绝静默丢弃）"
                ));
            }
            let Some(col) = (self.column)(field) else {
                // S1 / 缺陷 D-02：object/array 复杂 JSON 字段或未声明字段的条件，SQL 无法
                // 语义等价翻译（如 `tags="python"`、`coupon:{...}`、`meta.seo.title`）。
                // 此前 `continue` 静默丢弃 → 生成缺该条件的 WHERE → 返回全表。
                // 硬化契约：不可翻译即显式报错，绝不静默丢弃（读/写路径同样安全）。
                return Err(format!(
                    "SQL 后端无法翻译字段 \"{field}\" 的过滤条件（复杂 JSON 或未声明字段）：拒绝静默丢弃后返回全表"
                ));
            };
            let qualified = format!("{}.{}", self.alias, self.backend.quote_ident(&col));
            let clause = self.cond(cond, &qualified, field, self.alias)?;
            out.push(clause);
        }
        Ok(out)
    }

    /// 单字段条件：转发到操作符翻译（`op::cond_clause`），透传参数游标与告警通道。
    /// `field_token` 是 schema 字段名（即 `__present` 哨兵里的存在性 token），
    /// `alias` 用于构造 `alias.__present` 存在性判定。
    fn cond(
        &mut self,
        cond: &Value,
        qualified: &str,
        field_token: &str,
        alias: &str,
    ) -> Result<WhereClause, String> {
        cond_clause(
            cond,
            qualified,
            field_token,
            alias,
            self.backend,
            self.param_seq,
            self.warnings.as_deref_mut(),
        )
    }
}

/// JSON 值类型名（用于错误信息，避免把非法 filter 静默当空条件）
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// 用 `op` 连接若干条件片段（空片段先剔除；全空 → 无条件）
fn and_group(clauses: Vec<WhereClause>, op: &str) -> WhereClause {
    let mut active: Vec<WhereClause> = clauses.into_iter().filter(|c| !c.text.is_empty()).collect();
    if active.is_empty() {
        return WhereClause::new(String::new(), Vec::new());
    }
    let mut it = active.drain(..);
    let Some(mut acc) = it.next() else {
        return WhereClause::new(String::new(), Vec::new());
    };
    for c in it {
        let text = format!("({} {} {})", acc.text, op, c.text);
        acc.params.extend(c.params);
        acc.text = text;
    }
    acc
}
