//! SQL 中间表示（IR）

use serde_json::{json, Value};

/// 一条参数化 SQL 语句（参数位置用 `?` / `$n` 占位，params 顺序对应）
#[derive(Debug, Clone, PartialEq)]
pub struct SqlStmt {
    /// SQL 文本（标识符已按后端加引号，值全部参数化）
    pub text: String,
    /// 绑定参数（按占位顺序）
    pub params: Vec<Value>,
    /// 是否为写语句
    pub is_write: bool,
    /// SELECT 时的「列 → JSON 还原路径」元数据（见 `row_shape`）
    pub row_shape: Option<RowShape>,
    /// RETURNING 列（PG/SQLite 写语句回读用；MySQL 无）
    pub returning: Vec<String>,
}

impl SqlStmt {
    pub fn select(text: String, params: Vec<Value>, row_shape: RowShape) -> Self {
        SqlStmt {
            text,
            params,
            is_write: false,
            row_shape: Some(row_shape),
            returning: Vec::new(),
        }
    }

    pub fn write(text: String, params: Vec<Value>) -> Self {
        SqlStmt {
            text,
            params,
            is_write: true,
            row_shape: None,
            returning: Vec::new(),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "text": self.text,
            "params": self.params,
            "isWrite": self.is_write,
            "rowShape": self.row_shape.as_ref().map(|r| r.to_value()).unwrap_or(Value::Null),
            "returning": self.returning,
        })
    }
}

/// SELECT 输出的一列：SQL 别名（如 `t1_title` / `rel0_amount`）→ JSON 里的嵌套还原路径
#[derive(Debug, Clone, PartialEq)]
pub struct RowShape {
    /// 列 → { jsonPath, isArray }。jsonPath 形如 `["title"]` 或 `["items","amount"]`，
    /// 表示该列应嵌套还原到文档的哪个位置；isArray 表示该字段是关系数组（聚合还原）。
    pub columns: Vec<RowCol>,
    /// `__present` 哨兵列的别名（缺失 vs null 三态，F-07/H-01）：该列存每行
    /// 「哪些标量字段显式存在」的集合。读取时标量列值为 null，仅当字段存在于
    /// 该集合才还原为 `key: null`，否则（缺失）不产出该键。`None` 表示本条语句
    /// 未查该列（如 count / 关系聚合），还原时不做存在性判定。
    pub present_alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowCol {
    pub alias: String,
    pub json_path: Vec<String>,
    /// 是否为关系聚合数组字段（`many` 关系的子文档）
    pub is_array: bool,
    /// 是否为 `one` 关系（LEFT JOIN 后还原为对象或 `null`，而非数组）
    pub one: bool,
    /// 沿 `json_path` 每一级关系的基数（`true` = `one`）。长度 = 关系级数
    /// （`json_path.len() - 1`）；根标量列为空。嵌套关系靠它逐级决定「对象 / 数组」。
    pub ones: Vec<bool>,
    /// 若为空：标量直接取该列；若非空：构造子文档
    pub sub_shape: Option<RowShape>,
    /// 值恒「存在」：为 `null` 时也强制写入（不依赖 `__present` 哨兵）。用于归一聚合
    /// 计算列（§9.2(2)）—— 空集 `$sum/$avg/$min/$max` 的语义是显式 `null`（§9.7），
    /// 与 Mongo 输出逐行对齐。
    pub always: bool,
    /// §9.7「布尔归一」：该输出列对应 schema 的 `boolean` 字段 → 行还原时把 SQL 的
    /// `0/1` 归一为 JSON `bool`（对齐 Mongo；PG 原生 `BOOLEAN` 已是 bool，归一为 no-op）。
    /// 依据**schema 声明类型**而非驱动元数据，故对任意用户 DDL 都成立。
    pub is_bool: bool,
}

impl RowShape {
    pub fn empty() -> Self {
        RowShape {
            columns: Vec::new(),
            present_alias: None,
        }
    }

    pub fn to_value(&self) -> Value {
        let cols: Vec<Value> = self
            .columns
            .iter()
            .map(|c| {
                json!({
                    "alias": c.alias,
                    "path": c.json_path,
                    "isArray": c.is_array,
                    "one": c.one,
                    "ones": c.ones,
                    "subShape": c.sub_shape.as_ref().map(|s| s.to_value()).unwrap_or(Value::Null),
                    "always": c.always,
                    "bool": c.is_bool,
                })
            })
            .collect();
        json!({
            "columns": cols,
            "present": self.present_alias.clone().unwrap_or_default(),
        })
    }
}

impl RowCol {
    pub fn scalar(alias: &str, path: &[&str]) -> Self {
        RowCol {
            alias: alias.to_string(),
            json_path: path.iter().map(|s| s.to_string()).collect(),
            is_array: false,
            one: false,
            ones: Vec::new(),
            sub_shape: None,
            always: false,
            is_bool: false,
        }
    }

    /// 标量列 + 布尔标记（§9.7）：来自 schema `boolean` 字段的输出列
    pub fn scalar_bool(alias: &str, path: &[&str], is_bool: bool) -> Self {
        let mut c = RowCol::scalar(alias, path);
        c.is_bool = is_bool;
        c
    }

    /// 归一聚合计算列（§9.2(2)）：根标量，且 `null` 也强制写入（见 [`RowCol::always`]）
    pub fn computed(alias: &str, path: &[&str]) -> Self {
        RowCol {
            alias: alias.to_string(),
            json_path: path.iter().map(|s| s.to_string()).collect(),
            is_array: false,
            one: false,
            ones: Vec::new(),
            sub_shape: None,
            always: true,
            is_bool: false,
        }
    }

    pub fn array(alias: &str, sub_shape: RowShape) -> Self {
        RowCol {
            alias: alias.to_string(),
            json_path: vec![alias.to_string()],
            is_array: true,
            one: false,
            ones: vec![false],
            sub_shape: Some(sub_shape),
            always: false,
            is_bool: false,
        }
    }
}
