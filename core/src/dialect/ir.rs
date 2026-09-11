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
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowCol {
    pub alias: String,
    pub json_path: Vec<String>,
    /// 是否为关系聚合数组字段（`many` 关系的子文档）
    pub is_array: bool,
    /// 若为空：标量直接取该列；若非空：构造子文档
    pub sub_shape: Option<RowShape>,
}

impl RowShape {
    pub fn empty() -> Self {
        RowShape { columns: Vec::new() }
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
                    "subShape": c.sub_shape.as_ref().map(|s| s.to_value()).unwrap_or(Value::Null),
                })
            })
            .collect();
        json!({ "columns": cols })
    }
}

impl RowCol {
    pub fn scalar(alias: &str, path: &[&str]) -> Self {
        RowCol {
            alias: alias.to_string(),
            json_path: path.iter().map(|s| s.to_string()).collect(),
            is_array: false,
            sub_shape: None,
        }
    }

    pub fn array(alias: &str, sub_shape: RowShape) -> Self {
        RowCol {
            alias: alias.to_string(),
            json_path: vec![alias.to_string()],
            is_array: true,
            sub_shape: Some(sub_shape),
        }
    }
}