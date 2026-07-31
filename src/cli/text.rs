//! P5: TUI 文本工具(显示宽度、字符/词边界、截断、格式化数字)。
//!
//! 所有函数都是**纯函数**,无副作用、无 IO,适合在 TUI 渲染热路径上被频繁调用。
//! 抽出后 cli.rs 的体积显著下降,且这些函数未来可以在多端共享(例如 web 端复用宽度算法)。

use unicode_width::UnicodeWidthChar;

/// 格式化数字(>1M 用 M,>10K 用 K,否则原样)。
pub fn fmt_num(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// 计算字符串在终端中的显示宽度。
/// 使用 unicode_width crate 处理 CJK 和 Emoji 的宽度，回退到 1 列。
pub fn unicode_display_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(1))
        .sum()
}

/// 判断字符是否为宽字符（显示宽度 >= 2）。
/// 使用 unicode_width crate 判断，比手动列 CJK 范围更完整、更准确。
pub fn is_wide(c: char) -> bool {
    UnicodeWidthChar::width(c).unwrap_or(1) >= 2
}

/// 向后兼容：is_cjk 保留为 is_wide 的别名
pub fn is_cjk(c: char) -> bool {
    is_wide(c)
}

/// 取位置 `pos` 之前的字符边界。
pub fn prev_char_boundary(s: &str, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut p = pos - 1;
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

/// 取位置 `pos` 之后的字符边界。
pub fn next_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    match s[pos..].chars().next() {
        Some(c) => pos + c.len_utf8(),
        None => s.len(),
    }
}

/// 位置 `pos` 之前的"词"边界(空白分隔)。
/// 反向逐字符扫描，零堆分配。直接用 s[..cur].chars().next_back() 避免重复调用 prev_char_boundary。
pub fn prev_word_boundary(s: &str, pos: usize) -> usize {
    let p = prev_char_boundary(s, pos);
    let mut cur = p;
    // 跳过空白
    while cur > 0 {
        let ch = s[..cur].chars().next_back().unwrap_or(' ');
        if !ch.is_whitespace() {
            break;
        }
        cur -= ch.len_utf8();
    }
    // 跳过非空白
    while cur > 0 {
        let ch = s[..cur].chars().next_back().unwrap_or('x');
        if ch.is_whitespace() {
            break;
        }
        cur -= ch.len_utf8();
    }
    cur
}

/// 位置 `pos` 之后的"词"边界(空白分隔)。
/// 正向逐字符扫描，零堆分配。
pub fn next_word_boundary(s: &str, pos: usize) -> usize {
    let mut cur = pos;
    // 跳过非空白
    while cur < s.len() {
        let Some(ch) = s[cur..].chars().next() else {
            break;
        };
        if ch.is_whitespace() {
            break;
        }
        cur += ch.len_utf8();
    }
    // 跳过空白
    while cur < s.len() {
        let Some(ch) = s[cur..].chars().next() else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        cur += ch.len_utf8();
    }
    cur
}

/// 字符串按字符数截断(省略号)。
/// `max_chars` 为 0 时返回 `"-"`(在 sidebar 列表里常见这种占位)。
pub fn truncate_str(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return "-".to_string();
    }
    let char_count = s.chars().count();
    if char_count <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}...", truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_num_rounds_correctly() {
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(999), "999");
        assert_eq!(fmt_num(10_000), "10.0K");
        assert_eq!(fmt_num(1_500_000), "1.50M");
    }

    #[test]
    fn display_width_counts_cjk_as_two() {
        assert_eq!(unicode_display_width("hi"), 2);
        assert_eq!(unicode_display_width("你好"), 4);
        assert_eq!(unicode_display_width("a☃b"), 3); // snowman is 1 column in unicode_width
    }

    #[test]
    fn char_boundaries_skip_mid_utf8() {
        let s = "a你b";
        assert_eq!(prev_char_boundary(s, 3), 1); // from after "你" back to "你"
        assert_eq!(next_char_boundary(s, 1), 4); // from after "a" forward past "你"
    }

    #[test]
    fn word_boundaries_skip_whitespace() {
        let s = "hello world foo";
        // pos=6 在 "hello|" 之后 → 往前跳过 hello,定位到 0
        assert_eq!(prev_word_boundary(s, 6), 0);
        // pos=12(在 "world|" 之后)→ 跳过 world,定位到 6("hello "之后)
        assert_eq!(prev_word_boundary(s, 12), 6);
        // 从 'w' 之后跳到 'foo'(跳过 " world " 的空白)
        assert_eq!(next_word_boundary(s, 6), 12);
    }

    #[test]
    fn truncate_handles_zero_and_short() {
        assert_eq!(truncate_str("abc", 0), "-");
        assert_eq!(truncate_str("abc", 5), "abc");
        assert_eq!(truncate_str("abcdef", 4), "abc...");
    }

    #[test]
    fn cjk_detection_covers_basic_ranges() {
        assert!(is_cjk('中'));
        assert!(is_cjk('あ'));
        assert!(is_cjk('ア'));
        assert!(is_cjk('한'));
        assert!(!is_cjk('a'));
    }
}
