//! canonical 归一与三种重组（设计 §6.2；**单点实现**，绑定只转发）。

/// 词分隔符（设计 §6.2）：`_` / `-` / `.` / 空格。
#[inline]
fn is_sep(c: char) -> bool {
    matches!(c, '_' | '-' | '.' | ' ')
}

fn flush(cur: &mut String, out: &mut Vec<String>) {
    if !cur.is_empty() {
        out.push(std::mem::take(cur).to_lowercase());
    }
}

/// 用户任意写法 → token 序列（**全小写**）。
///
/// 规则（设计 §6.2、V7）：
/// - 分隔符 `_`/`-`/`.`/空格 处切分；其它非 ASCII 字母数字字符**一律按分隔符**处理；
/// - 小写或数字 → 大写处切分（`orderTotal` → `[order, total]`）；
/// - 连续大写后跟小写时，**最后一个大写归下一段**（`HTTPServer` → `[http, server]`、
///   `userID` → `[user, id]`）；
/// - **数字视为词内字符、不单独成词**（`order2Items` → `[order2, items]`）。
pub fn canonical(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for i in 0..chars.len() {
        let c = chars[i];
        if is_sep(c) || !c.is_ascii_alphanumeric() {
            flush(&mut cur, &mut out);
            continue;
        }
        if cur.is_empty() {
            cur.push(c);
            continue;
        }
        let prev = cur.chars().last().unwrap();
        if c.is_ascii_uppercase() {
            let prev_lower_or_digit = prev.is_ascii_lowercase() || prev.is_ascii_digit();
            let next_lower = chars
                .get(i + 1)
                .map(|n| n.is_ascii_lowercase())
                .unwrap_or(false);
            // 前一枚为小写/数字 → 切；连续大写串的最后一枚大写且其后为小写 → 切
            if prev_lower_or_digit || (prev.is_ascii_uppercase() && next_lower) {
                flush(&mut cur, &mut out);
            }
            cur.push(c);
        } else {
            cur.push(c);
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn cap(s: &str) -> String {
    let mut it = s.chars();
    match it.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + it.as_str(),
        None => String::new(),
    }
}

/// token 序列 → snake_case（`t1_t2`）
pub fn to_snake(tokens: &[String]) -> String {
    tokens.join("_")
}

/// token 序列 → camelCase（`t1` + `T2`）
pub fn to_camel(tokens: &[String]) -> String {
    let mut out = String::new();
    for (i, t) in tokens.iter().enumerate() {
        if i == 0 {
            out.push_str(t);
        } else {
            out.push_str(&cap(t));
        }
    }
    out
}

/// token 序列 → PascalCase（`T1T2`）
pub fn to_pascal(tokens: &[String]) -> String {
    tokens.iter().map(|t| cap(t)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    /// 设计 §6.2 全部示例向量（V7 含数字词内字符）
    #[test]
    fn canonical_design_vectors() {
        assert_eq!(canonical("orderTotal"), v(&["order", "total"]));
        assert_eq!(canonical("order_total"), v(&["order", "total"]));
        assert_eq!(canonical("Order.Total"), v(&["order", "total"]));
        assert_eq!(canonical("order2Items"), v(&["order2", "items"]));
        assert_eq!(canonical("HTTPServer"), v(&["http", "server"]));
        assert_eq!(canonical("userID"), v(&["user", "id"]));
    }

    #[test]
    fn canonical_edges() {
        assert_eq!(canonical(""), v(&[]));
        assert_eq!(canonical("order-Total"), v(&["order", "total"]));
        assert_eq!(canonical("order  total"), v(&["order", "total"])); // 连续分隔符折叠
        assert_eq!(canonical("_id"), v(&["id"])); // 见 §8 注意 2：调用方不得对 _id 翻译
        assert_eq!(canonical("HTTP"), v(&["http"])); // 全大写无后随小写
    }

    /// 重组三式（设计 §6.2）
    #[test]
    fn reassemble_vectors() {
        let t = v(&["order", "total"]);
        assert_eq!(to_snake(&t), "order_total");
        assert_eq!(to_camel(&t), "orderTotal");
        assert_eq!(to_pascal(&t), "OrderTotal");
    }

    /// 可逆性：三式重组结果再归一须回到同一 token 序列
    #[test]
    fn roundtrip_stable() {
        for s in [
            "orderTotal",
            "order_total",
            "Order.Total",
            "userID",
            "HTTPServer",
        ] {
            let t = canonical(s);
            assert_eq!(canonical(&to_snake(&t)), t);
            assert_eq!(canonical(&to_camel(&t)), t);
            assert_eq!(canonical(&to_pascal(&t)), t);
        }
    }
}
