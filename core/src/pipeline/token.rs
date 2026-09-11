//! 词法分析：GQL 字符串 → Token 序列

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: &'static str,
    pub value: String,
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.'
}

fn is_ref_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

pub fn tokenize(gql: &str) -> Vec<Token> {
    let chars: Vec<char> = gql.chars().collect();
    let n = chars.len();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < n {
        let ch = chars[i];
        // 空白
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        // 标点
        if "(){},:".contains(ch) {
            tokens.push(Token {
                kind: "p",
                value: ch.to_string(),
            });
            i += 1;
            continue;
        }
        // 标识符（模型名/字段名/关系名，支持点号嵌套）
        if is_ident_start(ch) {
            let mut v = String::new();
            while i < n && is_ident_char(chars[i]) {
                v.push(chars[i]);
                i += 1;
            }
            tokens.push(Token { kind: "id", value: v });
            continue;
        }
        // 参数引用 @xxx
        if ch == '@' {
            let mut v = String::from("@");
            i += 1;
            while i < n && is_ref_char(chars[i]) {
                v.push(chars[i]);
                i += 1;
            }
            tokens.push(Token { kind: "ref", value: v });
            continue;
        }
        i += 1; // 跳过未知字符
    }
    tokens
}

pub fn token_to_value(t: &Token) -> Value {
    Value::Object(Map::from_iter([
        ("t".to_string(), Value::String(t.kind.to_string())),
        ("v".to_string(), Value::String(t.value.clone())),
    ]))
}
