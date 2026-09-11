//! AST 数据定义与 JSON 互转

use std::collections::HashMap;

use serde_json::{Map, Value};

/// 关系节点。`relations` 用 `Vec` 保序：pipeline 阶段数组顺序依赖 GQL 中的出现顺序。
#[derive(Debug, Clone)]
pub struct RelAst {
    pub fields: Vec<String>,
    pub relations: Vec<(String, RelAst)>,
    pub params: HashMap<String, String>,
}

impl RelAst {
    pub fn to_value(&self) -> Value {
        Value::Object(Map::from_iter([
            (
                "fields".to_string(),
                Value::Array(self.fields.iter().cloned().map(Value::String).collect()),
            ),
            ("relations".to_string(), relations_to_value(&self.relations)),
            (
                "params".to_string(),
                Value::Object(
                    self.params
                        .iter()
                        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                        .collect(),
                ),
            ),
        ]))
    }

    /// 从 [`to_value`] 产出的 JSON 重建（供 finalize_query 从 plan.postprocess 还原 AST）
    pub fn from_value(v: &Value) -> Result<RelAst, String> {
        let obj = v.as_object().ok_or_else(|| "RelAst 必须是对象".to_string())?;
        let fields = match obj.get("fields") {
            Some(Value::Array(arr)) => arr
                .iter()
                .map(|f| f.as_str().unwrap_or_default().to_string())
                .collect(),
            _ => Vec::new(),
        };
        let mut relations = Vec::new();
        if let Some(Value::Object(rm)) = obj.get("relations") {
            for (k, rv) in rm {
                relations.push((k.clone(), RelAst::from_value(rv)?));
            }
        }
        let mut params = HashMap::new();
        if let Some(Value::Object(pm)) = obj.get("params") {
            for (k, pv) in pm {
                if let Some(s) = pv.as_str() {
                    params.insert(k.clone(), s.to_string());
                }
            }
        }
        Ok(RelAst {
            fields,
            relations,
            params,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Ast {
    pub model: String,
    pub params: HashMap<String, String>,
    pub fields: Vec<String>,
    pub relations: Vec<(String, RelAst)>,
}

impl Ast {
    pub fn to_value(&self) -> Value {
        Value::Object(Map::from_iter([
            ("model".to_string(), Value::String(self.model.clone())),
            (
                "params".to_string(),
                Value::Object(
                    self.params
                        .iter()
                        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                        .collect(),
                ),
            ),
            (
                "fields".to_string(),
                Value::Array(self.fields.iter().cloned().map(Value::String).collect()),
            ),
            ("relations".to_string(), relations_to_value(&self.relations)),
        ]))
    }

    /// 从 [`to_value`] 产出的 JSON 重建（供 finalize_query 从 plan.postprocess 还原 AST）
    pub fn from_value(v: &Value) -> Result<Ast, String> {
        let obj = v.as_object().ok_or_else(|| "Ast 必须是对象".to_string())?;
        let model = obj
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let mut params = HashMap::new();
        if let Some(Value::Object(pm)) = obj.get("params") {
            for (k, pv) in pm {
                if let Some(s) = pv.as_str() {
                    params.insert(k.clone(), s.to_string());
                }
            }
        }
        let fields = match obj.get("fields") {
            Some(Value::Array(arr)) => arr
                .iter()
                .map(|f| f.as_str().unwrap_or_default().to_string())
                .collect(),
            _ => Vec::new(),
        };
        let mut relations = Vec::new();
        if let Some(Value::Object(rm)) = obj.get("relations") {
            for (k, rv) in rm {
                relations.push((k.clone(), RelAst::from_value(rv)?));
            }
        }
        Ok(Ast {
            model,
            params,
            fields,
            relations,
        })
    }
}

fn relations_to_value(relations: &[(String, RelAst)]) -> Value {
    Value::Object(
        relations
            .iter()
            .map(|(k, v)| (k.clone(), v.to_value()))
            .collect(),
    )
}
