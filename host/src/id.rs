//! ID 生成（对齐 nodejs-store/src/crud/id.js:18-26）：`prefix + Date36大写 + 8位随机base36`。
//! core 无时钟无随机源（铁律），now 与 new_id 一律由 Host 供给。

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前毫秒时间戳（Host 时钟职责）
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const BASE36: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

fn to_base36(mut n: u64) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(BASE36[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_default()
}

/// 8 位随机 base36（xorshift 种子取纳秒时钟 + 地址熵，宿主侧非密码学用途）
fn rand8(state: &mut u64) -> String {
    let mut out = String::with_capacity(8);
    for _ in 0..8 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        out.push(BASE36[(*state % 36) as usize] as char);
    }
    out
}

/// 生成一个 schema ID（`idPrefix` + 毫秒36大写 + 8位随机36）
pub fn generate_id(prefix: &str, state: &mut u64) -> String {
    format!(
        "{}{}{}",
        prefix,
        to_base36(now_ms() as u64).to_uppercase(),
        rand8(state)
    )
}

/// 当前时间戳毫秒（语义名，供写路径取 `now`）
pub fn now() -> i64 {
    now_ms()
}
