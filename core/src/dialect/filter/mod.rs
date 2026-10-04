//! Mongo filter → WHERE 子句（参数化）
//!
//! 文件组织：本文件负责上下文与结构遍历（`build_filter` / `Ctx` / 逻辑组 / 字段条件），
//! 单字段**操作符翻译**在 [`op`]。

use serde_json::{Map, Value};

use super::{Backend, ColumnRef, JsonKind};

use op::{cond_clause, present_pred};

mod op;

/// 告警通道：`Some(&mut Vec<String>)` 表示调用方能收集翻译告警（select 侧会透出给 Host）；
/// `None` 表示**无告警能力**（写路径）——此时遇到「条件可翻译但语义需降级」的组合直接报错，
/// 宁可失败也绝不静默生成语义失真的 SQL（对齐 `write`「需告警的翻译直接报错」策略）。
pub type Warnings<'a> = Option<&'a mut Vec<String>>;

/// 关系聚合谓词（§9.6）解析器：给字段名与条件值，返回 `Some(WhereClause)` 表示该键
/// 是关系谓词代理（翻译为 `EXISTS` / `NOT EXISTS`），`None` = 普通字段条件。
pub trait RelPredResolver {
    fn resolve(
        &self,
        field: &str,
        cond: &Value,
        param_seq: &mut usize,
        warnings: Warnings<'_>,
    ) -> Result<Option<WhereClause>, String>;
}

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
    column: &dyn Fn(&str) -> Option<ColumnRef>,
    param_seq: &mut usize,
    warnings: Warnings,
) -> Result<WhereClause, String> {
    build_filter_with_relations(filter, backend, alias, column, None, param_seq, warnings)
}

/// [`build_filter`] 的关系谓词版：额外接受 [`RelPredResolver`]，把 §9.6 关系聚合谓词的
/// 代理键（`__rp…​.0`）翻译为 `EXISTS` / `NOT EXISTS` 子查询；`rel_pred = None` 时与
/// [`build_filter`] 完全一致。
pub fn build_filter_with_relations(
    filter: &Value,
    backend: Backend,
    alias: &str,
    column: &dyn Fn(&str) -> Option<ColumnRef>,
    rel_pred: Option<&dyn RelPredResolver>,
    param_seq: &mut usize,
    warnings: Warnings,
) -> Result<WhereClause, String> {
    let mut ctx = Ctx {
        backend,
        alias,
        column,
        rel_pred,
        param_seq,
        warnings,
    };
    ctx.root(filter)
}

/// 把条件翻译为**表达式过滤**（`GROUP BY` 之后的 `HAVING`）：`expr_of(name)` 返回
/// **完整 SQL 表达式**（如分组列 `t."status"` 或聚合表达式 `COUNT(*)`），本函数
/// **不做表别名限定**；分组结果无 `__present` 哨兵列，故 `$exists` / `$eq:null`
/// 退化为 `IS [NOT] NULL`（`present_pred` 恒真）。
pub fn build_filter_raw(
    filter: &Value,
    backend: Backend,
    expr_of: &dyn Fn(&str) -> Option<ColumnRef>,
    param_seq: &mut usize,
    warnings: Warnings,
) -> Result<WhereClause, String> {
    let mut ctx = Ctx {
        backend,
        alias: "",
        column: expr_of,
        rel_pred: None,
        param_seq,
        warnings,
    };
    ctx.root(filter)
}

/// 过滤翻译上下文：打包 `backend` / `alias` / `column` / `rel_pred` / `param_seq` / `warnings`，
/// 便于按职责拆分逻辑（`build_filter` 主体已收窄到分发），
/// 并让各辅助函数的参数个数保持在 4 以内。
struct Ctx<'a> {
    backend: Backend,
    alias: &'a str,
    column: &'a dyn Fn(&str) -> Option<ColumnRef>,
    rel_pred: Option<&'a dyn RelPredResolver>,
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
            rel_pred: self.rel_pred,
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
            // §9.6 关系聚合谓词代理键（`__rp….0`）→ EXISTS / NOT EXISTS
            if let Some(rp) = self.rel_pred {
                if let Some(clause) =
                    rp.resolve(field, cond, self.param_seq, self.warnings.as_deref_mut())?
                {
                    out.push(clause);
                    continue;
                }
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
            // U1/U2：object/array 整值条件走专用翻译（数组包含 / 整值等值），
            // 不走标量表达式的算符通道（见 `json_cond`）
            if matches!(col, ColumnRef::Json(..)) {
                let clause = self.json_cond(cond, &col, field)?;
                out.push(clause);
                continue;
            }
            let qualified = self.qualified(&col, field)?;
            let clause = self.cond(cond, &qualified, field, self.alias)?;
            out.push(clause);
        }
        Ok(out)
    }

    /// 列标识符 → 别名限定 + 引号化（表达式模式 alias 为空时原样返回）。
    fn ident_of(&self, c: &str) -> String {
        if self.alias.is_empty() {
            // 表达式模式（HAVING）：`c` 已是完整 SQL 表达式，不再限定/加引号
            c.to_string()
        } else {
            format!("{}.{}", self.alias, self.backend.pcol(c))
        }
    }

    /// 列引用 → 完整 SQL 表达式（U3 对象点号路径走 JSON 提取；整值 JSON 比较走 `json_cond`）。
    fn qualified(&self, col: &ColumnRef, field: &str) -> Result<String, String> {
        match col {
            ColumnRef::Scalar(c) => Ok(self.ident_of(c)),
            // U2（object/array 整值比较）：在 `field_clauses` 已分派给 `json_cond`，
            // 不应到达此处 → 显式报错（纵深防御，绝不静默）
            ColumnRef::Json(..) => Err(format!(
                "内部错误：JSON 整值列 \"{field}\" 未经 json_cond 分派（拒绝静默）"
            )),
            // U3/U4：object/array 点号路径 → 后端各自的 JSON 标量提取表达式
            ColumnRef::JsonPath(c, path) => {
                let base = self.ident_of(c);
                let segs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
                Ok(self.backend.json_extract_scalar(&base, &segs))
            }
        }
    }

    /// JSON 整列（object/array）的单字段条件（U1/U2；standard 档放行）。
    ///
    /// 语义对齐 MongoDB 原生：
    /// - `array` + 标量值 → 「数组包含该元素」（Mongo `{tags: "x"}`）；
    /// - `array` + 数组值 → 「数组整体等值」；
    /// - `object` + 对象值 → 「对象整值等值」（U2；跨后端键序差异须告警，禁静默）；
    /// - `$ne` 取反；`$exists` 走 `__present` 哨兵（与标量字段同源）；
    /// - 其余操作符/取值组合无法语义等价翻译 → 显式 Err（禁静默丢弃）。
    fn json_cond(
        &mut self,
        cond: &Value,
        col: &ColumnRef,
        field: &str,
    ) -> Result<WhereClause, String> {
        let (name, kind) = match col {
            ColumnRef::Json(n, k) => (n.as_str(), *k),
            _ => {
                return Err(format!(
                    "内部错误：json_cond 收到非 JSON 列引用（字段 {field}）"
                ))
            }
        };
        let base = self.ident_of(name);
        // 直写值 ⇒ `$eq` 简写；操作符对象（全 `$` 前缀键）⇒ 仅允许 `$eq` / `$ne` / `$exists`。
        // 注意：非 `$` 前缀键的对象是**整值等值目标**（Mongo `{meta: {level: "a"}}` 是子文档等值），
        // 不能被误当操作符对象；混用（既有 `$eq` 又有普通键）语义歧义 → 显式 Err。
        let mut negate = false;
        let mut value: Option<&Value> = None;
        let mut present: Option<bool> = None;
        match cond {
            Value::Object(map) => {
                let dollar = map.keys().filter(|k| k.starts_with('$')).count();
                if dollar > 0 && dollar < map.len() {
                    return Err(format!(
                        "JSON 字段 \"{field}\" 的条件混用了操作符与普通键（语义歧义，拒绝翻译）"
                    ));
                }
                if dollar == 0 {
                    // 整值等值目标（对象等值 U2 / 空对象等值）
                    value = Some(cond);
                } else {
                    for (k, v) in map {
                        match k.as_str() {
                            "$eq" => value = Some(v),
                            "$ne" => {
                                value = Some(v);
                                negate = true;
                            }
                            "$exists" => present = Some(v.as_bool().unwrap_or(true)),
                            other => {
                                return Err(format!(
                                    "JSON 字段 \"{field}\" 不支持条件操作符 {other}\
                                     （U1/U2：仅 $eq/$ne/$exists；拒绝静默丢弃）"
                                ))
                            }
                        }
                    }
                }
            }
            other => value = Some(other),
        }
        let mut parts: Vec<WhereClause> = Vec::new();
        if let Some(v) = value {
            parts.push(self.json_value_clause(&base, kind, v, negate, field)?);
        }
        if let Some(exists) = present {
            parts.push(self.json_exists_clause(&base, field, exists));
        }
        Ok(and_group(parts, "AND"))
    }

    /// JSON 列的 `$exists` 条件 → WHERE 片段（沿用标量字段的存在性哨兵语义）
    fn json_exists_clause(&self, base: &str, field: &str, exists: bool) -> WhereClause {
        if self.alias.is_empty() {
            // 表达式模式（HAVING）：无哨兵列 → 退化为 NULL 判定
            let text = if exists {
                format!("{} IS NOT NULL", base)
            } else {
                format!("{} IS NULL", base)
            };
            return WhereClause::new(text, Vec::new());
        }
        let pred = present_pred(self.alias, field);
        let text = if exists {
            pred
        } else {
            format!("NOT ({})", pred)
        };
        WhereClause::new(text, Vec::new())
    }

    /// 单个 JSON 整值条件 → WHERE 片段（含跨后端语义差异告警，禁静默）
    fn json_value_clause(
        &mut self,
        base: &str,
        kind: JsonKind,
        v: &Value,
        negate: bool,
        field: &str,
    ) -> Result<WhereClause, String> {
        match (kind, v) {
            // U1：数组整体等值（Mongo 数组比较 = 顺序相关的逐元素比较）
            (JsonKind::Array, Value::Array(_)) => self.json_eq_clause(base, v, negate),
            // U1：数组包含元素（Mongo `{tags: "x"}` / `{tags: {$eq: "x"}}`）
            (JsonKind::Array, Value::String(_) | Value::Number(_) | Value::Bool(_)) => {
                let ph = self.backend.placeholder(*self.param_seq);
                *self.param_seq += 1;
                let expr = self.backend.json_array_contains(base, &ph);
                // 参数契约（见 `Backend::json_array_contains`）：MySQL 传值的 JSON 字面量；
                // PG 传单元素数组；SQLite 传标量原值（保持类型，数字不比文本）
                let param = match self.backend {
                    Backend::Postgres => Value::String(format!("[{}]", json_text(v)?)),
                    Backend::Mysql => Value::String(json_text(v)?),
                    Backend::Sqlite => v.clone(),
                };
                let text = if negate {
                    format!("NOT ({})", expr)
                } else {
                    expr
                };
                Ok(WhereClause::new(text, vec![param]))
            }
            // U2：对象整值等值（已知跨后端键序差异 → 必须告警；写路径无告警通道 → Err）
            (JsonKind::Object, Value::Object(_)) => {
                let msg = format!(
                    "对象字段 \"{field}\" 的整值等值（U2）存在跨后端语义差异：{} 的对象比较键序无关，\
                     而 MongoDB 字段顺序敏感 / SQLite 保留键序 —— 可能命中更多行；\
                     不建议用于业务查询，建议改用对象点号路径（U3）",
                    self.backend.as_str()
                );
                match self.warnings.as_deref_mut() {
                    Some(w) => w.push(msg),
                    None => return Err(msg),
                }
                self.json_eq_clause(base, v, negate)
            }
            _ => Err(format!(
                "JSON 字段 \"{field}\" 不支持该条件取值（U1/U2）：数组字段仅支持标量 / 数组值、\
                 对象字段仅支持对象值；拒绝静默丢弃"
            )),
        }
    }

    /// JSON 整值等值谓词（U1 数组整体 / U2 对象）→ WHERE 片段
    fn json_eq_clause(
        &mut self,
        base: &str,
        v: &Value,
        negate: bool,
    ) -> Result<WhereClause, String> {
        let ph = self.backend.placeholder(*self.param_seq);
        *self.param_seq += 1;
        let expr = self.backend.json_value_eq(base, &ph, negate);
        Ok(WhereClause::new(expr, vec![Value::String(json_text(v)?)]))
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

/// JSON 值 → JSON 字面量文本（U1/U2 整值条件的参数绑定用；MySQL `CAST(? AS JSON)`、
/// PG `?::jsonb`、SQLite `json(?)` 均要求参数为 JSON 文本）
fn json_text(v: &Value) -> Result<String, String> {
    serde_json::to_string(v).map_err(|e| format!("JSON 值序列化失败：{e}"))
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
