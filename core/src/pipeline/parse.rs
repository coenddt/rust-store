//! 语法分析：Token 序列 → AST

use std::collections::HashMap;

use super::ast::{Ast, RelAst};
use super::token::{tokenize, Token};

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

/// `parse_body` 产出：`(字段列表, 关系列表)`
type BodyParts = (Vec<String>, Vec<(String, RelAst)>);

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn consume(&mut self, kind: &str, value: Option<&str>) -> Result<Token, String> {
        let tk = match self.peek() {
            None => {
                return Err(format!(
                    "期望 {}({}) 但已到达末尾",
                    kind,
                    value.unwrap_or("undefined")
                ))
            }
            Some(t) => t.clone(),
        };
        if tk.kind != kind || value.map(|v| tk.value != v).unwrap_or(false) {
            return Err(format!(
                "期望 {}({}) 实际 {}({}) 位置 {}",
                kind,
                value.unwrap_or("undefined"),
                tk.kind,
                tk.value,
                self.pos
            ));
        }
        self.pos += 1;
        Ok(tk)
    }

    fn parse_params(&mut self) -> Result<HashMap<String, String>, String> {
        let mut p = HashMap::new();
        let has_paren = matches!(self.peek(), Some(t) if t.kind == "p" && t.value == "(");
        if !has_paren {
            return Ok(p);
        }
        self.consume("p", Some("("))?;
        loop {
            if matches!(self.peek(), Some(t) if t.value == ")") {
                break;
            }
            let key = self
                .consume("id", None)?
                .value
                .trim_start_matches('$')
                .to_string();
            // `$pipeline` 直通已移除（D18：不留 Mongo 逃生舱）——显式报错而非静默忽略，
            // 否则用户自控的 pipeline 参数会被无声丢弃（与「绝不静默」冲突）。
            if key == "pipeline" {
                return Err(
                    "$pipeline 直通已移除：请改用标准 GQL（$condition/$sort/$skip/$limit + 关系字段）"
                        .to_string(),
                );
            }
            self.consume("p", Some(":"))?;
            let val = self.peek().cloned();
            if let Some(v) = &val {
                if v.kind == "ref" || v.kind == "id" {
                    p.insert(key, v.value.clone());
                }
            }
            self.pos += 1;
            if matches!(self.peek(), Some(t) if t.value == ",") {
                self.consume("p", Some(","))?;
            }
        }
        self.consume("p", Some(")"))?;
        Ok(p)
    }

    fn parse_body(&mut self) -> Result<BodyParts, String> {
        let mut fields = Vec::new();
        let mut relations = Vec::new();
        let has_brace = matches!(self.peek(), Some(t) if t.kind == "p" && t.value == "{");
        if !has_brace {
            return Ok((fields, relations));
        }
        self.consume("p", Some("{"))?;
        loop {
            match self.peek() {
                None => break,
                Some(t) if t.value == "}" => break,
                Some(t) if t.value == "," => {
                    self.consume("p", Some(","))?;
                    continue;
                }
                _ => {}
            }
            let name = self.consume("id", None)?.value;
            let has_paren = matches!(self.peek(), Some(t) if t.kind == "p" && t.value == "(");
            // 关系名后「参数列表 + 选择集」可同时出现（README 记载语法，
            // 缺陷 D-01：has_brace 必须在消费完参数之后再采样，否则
            // `Rel($limit:@l){f}` 的 `{` 会被误判为下一个字段导致解析失败）
            let prm = if has_paren {
                self.parse_params()?
            } else {
                HashMap::new()
            };
            let has_brace = matches!(self.peek(), Some(t) if t.kind == "p" && t.value == "{");
            if has_paren || has_brace {
                let (f, r) = if has_brace {
                    self.parse_body()?
                } else {
                    (Vec::new(), Vec::new())
                };
                relations.push((
                    name,
                    RelAst {
                        fields: f,
                        relations: r,
                        params: prm,
                    },
                ));
            } else {
                fields.push(name);
            }
            if matches!(self.peek(), Some(t) if t.value == ",") {
                self.consume("p", Some(","))?;
            }
        }
        self.consume("p", Some("}"))?;
        Ok((fields, relations))
    }
}

pub fn parse(tokens: &[Token]) -> Result<Ast, String> {
    let mut p = Parser {
        tokens: tokens.to_vec(),
        pos: 0,
    };
    let model = p.consume("id", None)?.value;
    let params = p.parse_params()?;
    let (fields, relations) = p.parse_body()?;
    Ok(Ast {
        model,
        params,
        fields,
        relations,
    })
}

/// 解析 GQL 字符串 → AST
pub fn parse_gql(gql: &str) -> Result<Ast, String> {
    parse(&tokenize(gql))
}
