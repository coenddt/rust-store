//! Mongo 方言 → 关系型 SQL 翻译层（纯逻辑，无 IO）
//!
//! 背景：core 按约定继续产出 **Mongo 命令 JSON**（`find`/`aggregate`/`countDocuments`/…）。
//! 本模块提供一条**纯函数**翻译通道，把这些命令落成 MySQL / PostgreSQL / SQLite 各后端的
//! 参数化 SQL 语句（[`ir::SqlStmt`]），并负责把驱动返回的**平铺 JOIN 行**还原为嵌套 Mongo
//! 文档（[`row::restore_rows`]）。
//!
//! 设计边界（对齐「用 MongoDB 方言，其他数据库适配」的要求）：
//! 1. [`translate::translate`] 只做 JSON → SQL 中间表示，完全不触数据库；Host 拿到
//!    `SqlStmt` 后自行绑定驱动。
//! 2. 关系模型采用**严格关系范式**：标量字段落在单表，`object`/`array` 展平为附属表，
//!    关系用 SQL JOIN 解析。
//! 3. 无法安全翻译的组合输出 `_unsupported` 标志 + warning，**绝不生成错误 SQL**。
//! 4. 平铺行 → 嵌套文档的还原与 introspection 行 → schemaJSON 的映射都是纯逻辑，四侧对拍可复现。

pub mod filter;
pub mod introspect;
pub mod ir;
pub mod overlay;
pub mod raw;
pub mod row;
pub mod select;
pub mod translate;
pub mod write;

// 保持对外路径稳定：`crate::dialect::*` 直接可用（函数与其同名模块共存）
pub use introspect::{introspect_to_schema_json, schema_def_from_rows};
pub use overlay::merge_schema;
pub use raw::{compile_raw_stmt, RawStmt};
pub use row::restore_rows_json;
pub use translate::translate;

use crate::schema::Schema;

/// 标量字段 → 列名（本表列）；object/array 展平字段跳过；点号路径按整串处理。
///
/// **读（[`select`]）与写（[`write`]）共用此唯一实现。** 两侧语义若各自漂移会产生
/// 读写不对称（读得到的列写不进 / 写进去的列读不出），故收口到此，两侧只做薄包装。
pub(crate) fn scalar_column(schema: &Schema, field: &str) -> Option<String> {
    if field.contains('.') {
        let (head, _) = field.split_once('.')?;
        // 点号字段：若 head 是 object/array（其子字段展平到附属表）则跳过；否则按整串处理
        let head_type = schema.fields.get(head).map(|f| f.field_type.as_str());
        if matches!(head_type, Some("object") | Some("array")) {
            return None;
        }
        return Some(field.to_string());
    }
    match schema.fields.get(field).map(|f| f.field_type.as_str()) {
        Some("object") | Some("array") => None,
        _ => Some(field.to_string()),
    }
}

/// 字段 → 列引用（过滤 / 投影 / 回读用；object/array 走 JSON 单列）。
///
/// 与 [`scalar_column`] 的差异：`scalar_column` 只认标量列（object/array 返回 `None`，
/// 供关系键解析、分组键等**必须标量**的场景）；本函数在其基础上把 object/array 字段
/// 展开为 JSON 列引用（见执行文档 §4.5 / 不足清单 #3）：
/// - 标量字段 → [`ColumnRef::Scalar`]（调用方按别名限定 + 引号化）；
/// - object/array 整值 → [`ColumnRef::Json`]（单列存 JSON 文本，读取时解析；`JsonKind` 区分
///   数组/对象，供 U1/U2 整值条件选择翻译方式）；
/// - object/array 的点号路径（U3 过滤 / U4 排序）→ [`ColumnRef::JsonPath`]（提取表达式）。
///
/// 未声明字段返回 `None`（调用方据此显式报错，绝不静默）。
pub(crate) fn field_column_ref(schema: &Schema, field: &str) -> Option<ColumnRef> {
    if let Some((head, rest)) = field.split_once('.') {
        let head_type = schema.fields.get(head).map(|f| f.field_type.as_str());
        match head_type {
            // U3/U4：object/array 点号路径 → JSON 提取（根列 head + 子路径 rest）
            Some("object") | Some("array") => {
                let path: Vec<String> = rest.split('.').map(String::from).collect();
                Some(ColumnRef::JsonPath(head.to_string(), path))
            }
            // 点号字段的 head 非 object/array（如标量同名字段）→ 按整串列名处理（既有语义）
            _ => Some(ColumnRef::Scalar(field.to_string())),
        }
    } else {
        match schema.fields.get(field).map(|f| f.field_type.as_str()) {
            Some("object") => Some(ColumnRef::Json(field.to_string(), JsonKind::Object)),
            Some("array") => Some(ColumnRef::Json(field.to_string(), JsonKind::Array)),
            // 已声明标量 / 未声明字段（与 `scalar_column` 宽松语义一致：按裸列名处理，
            // 如物理主键 `_id` 不在 schema.fields 但恒为列）
            _ => Some(ColumnRef::Scalar(field.to_string())),
        }
    }
}

/// JSON 整列的形态（`JsonKind`）：决定 U1/U2 整值条件的翻译方式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonKind {
    /// `object` 字段：整值条件按 JSON 结构等值比较（U2）
    Object,
    /// `array` 字段：整值条件按「数组包含元素 / 数组整体等值」翻译（U1）
    Array,
}

/// 字段 → 列引用（见 [`field_column_ref`]）：区分标量列 / JSON 整列 / JSON 点号路径。
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnRef {
    /// 标量列：字段名（调用方按表别名限定 + 引号化）
    Scalar(String),
    /// JSON 整列：object/array 字段落单列存 JSON 文本（读取时解析回嵌套对象）
    Json(String, JsonKind),
    /// JSON 点号路径（U3 过滤 / U4 排序）：根列名 + 子路径（调用方构造提取表达式）
    JsonPath(String, Vec<String>),
}

/// §9.7「布尔归一」：schema 字段是否为布尔类型（`boolean` 规范拼写 / `bool` 简写）。
///
/// 判定依据是 schema 的**声明类型**（不是驱动元数据，也不是物理列类型）——SQL 侧把布尔
/// 存成 `TINYINT(1)` / `INTEGER` / `BOOLEAN` 都可能，唯一稳定的依据是 schema。命中即由
/// [`row::restore_rows`] 在行还原时把 `0/1` 归一为 JSON `bool`。
///
/// 两种拼写都接受：core 的类型文档与 introspection 产出 `boolean`，而示例/用户 schema
/// 常写 `bool`（如场景矩阵的 `paid` / `free`），二者语义相同。
pub(crate) fn field_is_bool(schema: &Schema, field: &str) -> bool {
    schema
        .fields
        .get(field)
        .map(|f| {
            f.field_type.eq_ignore_ascii_case("bool")
                || f.field_type.eq_ignore_ascii_case("boolean")
        })
        .unwrap_or(false)
}

/// 保留物理名（I3）：`_id` 恒原样；`__` 前缀内部合成名不翻译。
fn is_reserved_physical(name: &str) -> bool {
    name == "_id" || name.starts_with("__")
}

/// 逻辑名 → 本后端物理标识符（设计 §6，翻译单点在 [`crate::naming`]）。
///
/// 保留名（I3：`_id` / `__` 前缀）原样返回，其余走 `naming` 归一为 snake_case（SQL 物理风格）。
/// **只做正向（逻辑 → 物理）**；反向翻译不在本层。
pub(crate) fn physical_of(name: &str) -> String {
    if is_reserved_physical(name) {
        name.to_string()
    } else {
        crate::naming::to_snake(&crate::naming::canonical(name))
    }
}

/// 支持的数据库后端
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Mysql,
    Postgres,
    Sqlite,
}

impl Backend {
    /// 从字符串解析后端（未知 → Err）
    pub fn parse(s: &str) -> Result<Backend, String> {
        match s.to_lowercase().as_str() {
            "mysql" => Ok(Backend::Mysql),
            "postgres" | "pg" | "postgresql" => Ok(Backend::Postgres),
            "sqlite" => Ok(Backend::Sqlite),
            _ => Err(format!("不支持的后端: {}", s)),
        }
    }

    /// 双重引号标识符
    ///
    /// **仅用于别名 / 已物理名**（`t`、`r0`、`__present`、`__rn`、`fk`、`v`… 等合成名）。
    /// 逻辑数据标识符（字段 / 关系字段 / 计算列 key / 表名）一律走 [`Backend::pcol`]。
    pub fn quote_ident(&self, ident: &str) -> String {
        match self {
            Backend::Mysql => format!("`{}`", ident.replace('`', "``")),
            _ => format!("\"{}\"", ident.replace('"', "\"\"")),
        }
    }

    /// 列引用统一出口：`quote_ident(physical_of(logical))`（设计 §6 翻译边界「落库」）。
    ///
    /// 逻辑数据标识符 → 本后端物理名（snake_case）后再引号化。
    pub(crate) fn pcol(&self, logical: &str) -> String {
        self.quote_ident(&physical_of(logical))
    }

    /// 双精度浮点类型名（§9.7「数值归 double」）
    ///
    /// `SUM/AVG` 的结果在不同后端原生精度不同：MySQL `AVG(<int 列>)` 只保留 4 位小数
    /// （`DECIMAL(scale+4)`），PG `AVG(<int 列>)` 返回精确 `numeric`，均与 Mongo `$avg`
    /// 的 IEEE-754 double 存在差异。故 SQL 侧 `AVG` 前先把入参 `CAST` 到双精度。
    pub fn double_type(&self) -> &'static str {
        match self {
            Backend::Mysql => "DOUBLE",
            Backend::Postgres => "DOUBLE PRECISION",
            Backend::Sqlite => "REAL",
        }
    }

    /// 表名 → SQL：`database` / `schema` 可选限定（空串按 `None` 处理）。
    ///
    /// **PG 用 `schema` 限定**（`"schema"."table"`；database 由连接承载，**禁**跨库限定）；
    /// **MySQL/SQLite 用 `database` 限定**（`"db"."table"`）。表名走 [`physical_of`]（物理名）。
    /// 见设计 §3.2 / §6.4。
    pub fn qualified_table(
        &self,
        database: Option<&str>,
        schema: Option<&str>,
        table: &str,
    ) -> String {
        let t = self.quote_ident(&physical_of(table));
        let qual = match self {
            Backend::Postgres => schema,
            _ => database,
        };
        match qual.filter(|s| !s.is_empty()) {
            Some(q) => format!("{}.{}", self.quote_ident(q), t),
            None => t,
        }
    }

    /// 占位符：SQLite/MySQL 用 `?`，PostgreSQL 用 `$n`（调用方保证按顺序传入 index）
    pub fn placeholder(&self, _index: usize) -> String {
        match self {
            Backend::Postgres => format!("${}", _index + 1),
            _ => "?".to_string(),
        }
    }

    /// 后端标识（同 `Debug`，供绑定层序列化）
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Mysql => "mysql",
            Backend::Postgres => "postgres",
            Backend::Sqlite => "sqlite",
        }
    }

    /// MySQL 无 RETURNING —— 需由 Host 编排 UPDATE + find 两段；PG/SQLite 原生支持
    pub fn supports_returning(&self) -> bool {
        !matches!(self, Backend::Mysql)
    }

    /// JSON 列的方言类型名（`object`/`array` 字段落单列存储的契约，见 §4.5 / 不足清单 #3）
    ///
    /// ⚠️ 跨方言对齐：`object`/`array` 字段在 SQL 后端**以单列存 JSON 文本** ——
    /// MySQL `JSON` / PG `jsonb` / SQLite `TEXT`（JSON1 扩展）。core 不写 DDL（铁律 6），
    /// 此常量供示例 DDL、introspection 与文档约定引用。
    pub fn json_type_name(&self) -> &'static str {
        match self {
            Backend::Mysql => "JSON",
            Backend::Postgres => "jsonb",
            Backend::Sqlite => "TEXT",
        }
    }

    /// JSON 点号路径 → **标量**提取表达式（U3 对象点号路径过滤 / U4 排序用）
    ///
    /// - MySQL：`JSON_UNQUOTE(JSON_EXTRACT(col, '$.a.b'))`
    /// - PG：`(col #>> '{a,b}')`
    /// - SQLite：`json_extract(col, '$.a.b')`
    ///
    /// 三者统一为「提取后标量（数值/字符串）」，供比较与 `ORDER BY` 使用；
    /// `col` 须为已按后端引号化的**列标识符**，`path` 为点号各段（不含列名）。
    ///
    /// **注入防护（N1）**：每段先过 [`validate_json_path_seg`] 白名单校验，段含
    /// `'` / `\` / `"` / `,` / 控制字符等 → 显式 `Err`（前缀 `ERR_GQL_PARSE:`），
    /// 绝不静默丢弃/替换；输出层再按后端字面量口径转义（纵深防御）。
    pub fn json_extract_scalar(&self, col: &str, path: &[&str]) -> Result<String, String> {
        for seg in path {
            validate_json_path_seg(seg)?;
        }
        match self {
            Backend::Mysql => Ok(format!(
                "JSON_UNQUOTE(JSON_EXTRACT({}, '{}'))",
                col,
                json_path_dollar(path)?
            )),
            // PG `#>>` 为数组字面量：段逐个按 PG 数组字面量规则转义后 join
            Backend::Postgres => {
                let segs: Vec<String> = path.iter().map(|s| pg_array_literal_seg(s)).collect();
                Ok(format!("({} #>> '{{{}}}')", col, segs.join(",")))
            }
            Backend::Sqlite => Ok(format!("json_extract({}, '{}')", col, json_path_dollar(path)?)),
        }
    }

    /// JSON 数组「包含元素」谓词（U1：`{tags: "x"}` ⇒ 数组含 `"x"`）
    ///
    /// - MySQL：`JSON_CONTAINS(col, CAST(? AS JSON))`（候选是**值的 JSON 字面量**，如 `"x"` / `1`）
    /// - PG：`col @> ?::jsonb`（候选是**单元素数组**的 JSON 文本，如 `["x"]`）
    /// - SQLite：`EXISTS (SELECT 1 FROM json_each(col) WHERE json_each.value = ?)`（候选是标量原值）
    ///
    /// `ph` 为已生成的占位符（`?` / `$n`），参数由调用方按上述契约绑定（见 `filter::json_cond`）。
    pub fn json_array_contains(&self, col: &str, ph: &str) -> String {
        match self {
            Backend::Mysql => format!("JSON_CONTAINS({}, CAST({} AS JSON))", col, ph),
            Backend::Postgres => format!("{} @> {}::jsonb", col, ph),
            Backend::Sqlite => format!(
                "EXISTS (SELECT 1 FROM json_each({}) WHERE json_each.value = {})",
                col, ph
            ),
        }
    }

    /// JSON 整值等值比较（U1 数组整体等值 / U2 对象结构等值）
    ///
    /// - MySQL：`col [<>] CAST(? AS JSON)`；PG：`col [<>] ?::jsonb`；SQLite：`json(col) [<>] json(?)`
    ///
    /// ⚠️ 跨后端语义差异（**已知不对齐点**，调用方须告警、禁静默，见「自动反馈原则」）：
    /// MySQL `JSON` / PG `jsonb` 的对象比较是**键序无关**的（存储即规范化），
    /// 而 MongoDB 的对象等值匹配**字段顺序敏感**、SQLite（`json()` 文本 minify）**保留键序**。
    /// 故 `object` 字段（U2）在 MySQL/PG 上可能比 Mongo/SQLite 命中更多行 ——
    /// **不建议用于业务查询**，建议改用对象点号路径（U3）；仅适合数据迁移 / 功能脚本。
    pub fn json_value_eq(&self, col: &str, ph: &str, negate: bool) -> String {
        let op = if negate { "<>" } else { "=" };
        match self {
            Backend::Mysql => format!("{} {} CAST({} AS JSON)", col, op, ph),
            Backend::Postgres => format!("{} {} {}::jsonb", col, op, ph),
            Backend::Sqlite => format!("json({}) {} json({})", col, op, ph),
        }
    }
}

/// JSON 点号路径段白名单校验（N1 注入防护唯一收口点）。
///
/// 路径段语义 = 字段名段，仅允许字母/数字/`_`/`-`/中文（CJK）等字段名常用字符；
/// 含 `'`、`"`、`\`、`` ` ``、`{`、`}`、`,`、`[`、`]`、`$`、空格、控制字符等 → 显式 `Err`
/// （前缀 `ERR_GQL_PARSE:`，沿用既有解析类前缀）。**禁静默丢弃/替换**。
fn validate_json_path_seg(seg: &str) -> Result<(), String> {
    if seg.is_empty() {
        return Err("ERR_GQL_PARSE:JSON 点号路径段为空（非法路径段）".to_string());
    }
    let ok = seg.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || c == '_'
            || c == '-'
            || ('\u{4e00}'..='\u{9fff}').contains(&c)
    });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "ERR_GQL_PARSE:JSON 点号路径段 \"{seg}\" 含非法字符（仅允许字母/数字/下划线/连字符/中文）"
        ))
    }
}

/// PG `#>>` 数组字面量元素转义：段内含 `"` / `\` / `,` / `{` / `}` / 空白或为空时，
/// 须以双引号包裹（PG 数组字面量规则），段内 `"` / `\` 反斜杠转义。
fn pg_array_literal_seg(seg: &str) -> String {
    let needs_quote = seg.is_empty()
        || seg
            .chars()
            .any(|c| matches!(c, '"' | '\\' | ',' | '{' | '}' | ' '));
    if !needs_quote {
        return seg.to_string();
    }
    let mut out = String::from("\"");
    for ch in seg.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// 点号路径 → MySQL/SQLite `JSON_EXTRACT` 的 `$` 路径字面量（`["a","b"]` → `$.a.b`）
///
/// 每段先过 [`validate_json_path_seg`]（非法即 `Err`）；输出层再按 MySQL/SQLite 单引号
/// 字面量口径转义（`'` → `''`、`\` → `\\`）作纵深防御。
fn json_path_dollar(path: &[&str]) -> Result<String, String> {
    let mut s = String::from("$");
    for seg in path {
        validate_json_path_seg(seg)?;
        s.push('.');
        for ch in seg.chars() {
            match ch {
                '\'' => s.push_str("''"),
                '\\' => s.push_str("\\\\"),
                _ => s.push(ch),
            }
        }
    }
    Ok(s)
}
