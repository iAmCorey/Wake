//! 跨数据层与 UI 共用的文本小工具。wake crate 的 `format::one_line` 转发到
//! 这里——同一条"折行截断"规则只写一次(曾在两个 crate 各有一份、同名不同义)

/// 折成一行:空白压成单空格,超过 `max_chars` 个字符截断并加 `…`
/// (按字符数计,CJK 安全)
pub fn one_line(s: &str, max_chars: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max_chars) {
        Some((ix, _)) => format!("{}…", &flat[..ix]),
        None => flat,
    }
}

/// 英文复数后缀:1 → ""、其余 → "s"
pub fn plural(n: i64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_flattens_and_clips_by_chars() {
        assert_eq!(one_line("  a\n\n b\tc ", 10), "a b c");
        assert_eq!(one_line("二维码扫描登录", 3), "二维码…");
        assert_eq!(one_line("abc", 3), "abc", "恰好等长不截");
    }

    #[test]
    fn plural_suffix() {
        assert_eq!(plural(1), "");
        assert_eq!(plural(0), "s");
        assert_eq!(plural(2), "s");
    }
}
