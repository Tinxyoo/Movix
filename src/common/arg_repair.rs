use serde_json::{Map, Value};
use std::sync::OnceLock;

pub const MAX_ARG_LEN: usize = 1024 * 1024;

static TRAILING_COMMA_RE: OnceLock<regex::Regex> = OnceLock::new();

/// 获取尾逗号匹配的预编译正则
fn trailing_comma_regex() -> &'static regex::Regex {
    TRAILING_COMMA_RE.get_or_init(|| {
        regex::Regex::new(r",\s*([}\]])").expect("trailing comma regex should be valid")
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ArgRepairError {
    #[error("argument exceeded {0} chars; refusing to repair")]
    TooLarge(usize),
}

#[derive(Debug, Clone)]
pub struct RepairReport {
    pub changed: bool,
    pub notes: Vec<String>,
    pub fallback: bool,
    /// 修复(Bug #11):标记原始参数是否超过最大长度,超过时直接当作
    /// fallback 处理(不再返回 `{}` 让下游误以为合法)。调用方应检查
    /// `too_large` 来决定是否要拒绝执行工具调用。
    pub too_large: bool,
    /// 超过最大长度时,记录原始字节数(便于上游打点/告警)。
    pub original_len: usize,
}

pub fn repair(raw: &str) -> Result<Value, ArgRepairError> {
    Ok(repair_with_report(raw).0)
}

pub fn repair_with_report(raw: &str) -> (Value, RepairReport) {
    let mut report = RepairReport {
        changed: false,
        notes: Vec::new(),
        fallback: false,
        too_large: false,
        original_len: raw.len(),
    };

    if raw.len() > MAX_ARG_LEN {
        // 修复(Bug #11):之前这里直接返回 `Value::Object(Map::new())`,
        // 工具层拿到一个看起来合法的 `{}` 继续调度,可能在 `path: ""` 这
        // 类参数上误命中工作区根目录。改为返回 `Value::Null` 并在 report
        // 里明确标记 `too_large = true`,让上游可以拒绝执行。
        report.fallback = true;
        report.too_large = true;
        report
            .notes
            .push(format!("argument exceeded {} chars", raw.len()));
        return (Value::Null, report);
    }

    if let Ok(v) = serde_json::from_str(raw) {
        return (v, report);
    }

    report.changed = true;

    let mut s = strip_control_chars_in_strings(raw);
    if let Ok(v) = serde_json::from_str(&s) {
        report
            .notes
            .push("stripped control chars in strings".into());
        return (v, report);
    }

    s = strip_trailing_commas(&s);
    if let Ok(v) = serde_json::from_str(&s) {
        report.notes.push("stripped trailing commas".into());
        return (v, report);
    }

    s = balance_braces(&s, 50);
    if let Ok(v) = serde_json::from_str(&s) {
        report.notes.push("balanced braces/brackets".into());
        return (v, report);
    }

    s = strip_excess_closers(&s);
    if let Ok(v) = serde_json::from_str(&s) {
        report.notes.push("stripped excess closers".into());
        return (v, report);
    }

    report.fallback = true;
    report
        .notes
        .push("all repair attempts failed, falling back to empty object".into());
    (Value::Object(Map::new()), report)
}

fn strip_control_chars_in_strings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escape = false;
    for ch in s.chars() {
        if escape {
            out.push(ch);
            escape = false;
            continue;
        }
        if ch == '\\' {
            escape = true;
            out.push(ch);
            continue;
        }
        if in_string {
            if ch == '"' {
                in_string = false;
                out.push(ch);
            } else if ch.is_control() && ch != '\t' && ch != '\n' && ch != '\r' {
                continue;
            } else {
                out.push(ch);
            }
        } else {
            if ch == '"' {
                in_string = true;
            }
            out.push(ch);
        }
    }
    out
}

fn strip_trailing_commas(s: &str) -> String {
    trailing_comma_regex().replace_all(s, "$1").into_owned()
}

fn balance_braces(s: &str, max_depth: usize) -> String {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape = false;

    for ch in s.chars() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' {
            escape = true;
            continue;
        }
        if in_string {
            if ch == '"' {
                in_string = false;
                stack.pop();
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            stack.push('"');
        } else if ch == '{' || ch == '[' {
            stack.push(ch);
        } else if ch == '}' {
            if stack.last() == Some(&'{') {
                stack.pop();
            }
        } else if ch == ']' && stack.last() == Some(&'[') {
            stack.pop();
        }
    }

    let mut result = s.to_string();

    if in_string {
        result.push('"');
    }

    for ch in stack.iter().rev().take(max_depth) {
        match ch {
            '{' => result.push('}'),
            '[' => result.push(']'),
            '"' => {}
            _ => {}
        }
    }

    if result.trim().ends_with(':') {
        result.push_str(" null");
    }

    result
}

fn strip_excess_closers(s: &str) -> String {
    let mut open_count = 0i32;
    let mut close_count = 0i32;
    let mut in_string = false;
    let mut escape = false;

    for ch in s.chars() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' {
            escape = true;
            continue;
        }
        if in_string {
            if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
        } else if ch == '{' || ch == '[' {
            open_count += 1;
        } else if ch == '}' || ch == ']' {
            close_count += 1;
        }
    }

    if close_count <= open_count {
        return s.to_string();
    }

    let excess = (close_count - open_count) as usize;
    let mut result = s.to_string();

    // 修复(High #H10):原实现用 `result.rfind(['}', ']'])` 删除多余闭括号,
    // 但不区分字符串内外。若 JSON 值里含 `}`/`]`(如 {"msg":"done}"}),
    // 删除会命中字符串内部字符而非结构性括号,把参数"修复"成语义错误甚至危险的 JSON。
    //
    // 正确做法:从后向前扫描,跟踪字符串状态,只删除**字符串外**的结构性闭括号。
    // 由于 String 是 UTF-8 且我们要按 char 索引删除,这里转成 Vec<char> 处理。
    let mut chars: Vec<char> = result.chars().collect();
    let mut to_remove: Vec<usize> = Vec::with_capacity(excess);
    {
        // 修复(M9,关键):原反向扫描用 `escape = ch=='\\' && !escape` 判定字符串边界,
        // 在连续反斜杠(`"a\\"`)下会把转义状态算错,误判字符串边界,进而把字符串内的
        // `}`/`]` 当结构括号删除,破坏含 `}` 的字符串值(如正则参数 `{"re":"a\\}"}`)。
        //
        // 正确做法:**前向**单次扫描确定每个 char 是否在字符串内(前向 escape 计数天然正确:
        // 遇 `\` 翻转 escape,遇 `"` 且 !escape 翻转 in_string),记录每个位置的 in_string 状态,
        // 然后从后向前在"字符串外"的位置上收集 } / ] 索引。
        let mut in_string_flags = vec![false; chars.len()];
        {
            let mut in_string = false;
            let mut escape = false;
            for (i, &ch) in chars.iter().enumerate() {
                in_string_flags[i] = in_string;
                if in_string {
                    if ch == '"' && !escape {
                        in_string = false;
                    }
                    escape = ch == '\\' && !escape;
                } else if ch == '"' {
                    in_string = true;
                    escape = false;
                }
            }
        }
        // 从后向前收集字符串外的 } / ]。
        let mut i = chars.len() as i64 - 1;
        while i >= 0 {
            let idx = i as usize;
            let ch = chars[idx];
            let in_str = in_string_flags[idx];
            if !in_str && (ch == '}' || ch == ']') {
                to_remove.push(idx);
                if to_remove.len() >= excess {
                    break;
                }
            }
            i -= 1;
        }
    }

    // 按索引降序删除,避免索引移位。
    to_remove.sort_unstable_by(|a, b| b.cmp(a));
    for idx in to_remove {
        chars.remove(idx);
    }
    result = chars.into_iter().collect();

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_json_passthrough() {
        let v = repair(r#"{"path": "/foo/bar"}"#).unwrap();
        assert_eq!(v["path"], "/foo/bar");
    }

    #[test]
    fn test_trailing_comma_repair() {
        let v = repair(r#"{"path": "/foo",}"#).unwrap();
        assert_eq!(v["path"], "/foo");
    }

    #[test]
    fn test_unbalanced_braces_repair() {
        let v = repair(r#"{"path": "/foo""#).unwrap();
        assert_eq!(v["path"], "/foo");
    }

    #[test]
    fn test_fallback_empty_object() {
        let v = repair("not json at all {{{{").unwrap();
        assert!(v.is_object());
    }

    #[test]
    fn test_control_chars_stripped() {
        let input = "{\"path\": \"/foo\x00bar\"}";
        let v = repair(input).unwrap();
        assert_eq!(v["path"], "/foobar");
    }

    /// 回归(High #H10):strip_excess_closers 删除多余闭括号时不得破坏字符串内部的
    /// `}`/`]`。原实现 rfind 不区分字符串内外,会误删值中的括号。
    #[test]
    fn strip_excess_closers_preserves_in_string_braces() {
        // 值里含 `}`,且整体多了一个尾部 `}`。
        let input = r#"{"msg": "done}", "ok": true}"#;
        let repaired = strip_excess_closers(input);
        // 修复后应只删最后一个多余的结构性 `}`,保留字符串内的 `}`。
        let v: serde_json::Value = serde_json::from_str(&repaired).unwrap_or_else(|e| {
            panic!(
                "修复后的 JSON 必须可解析: {} | 输入: {:?} | 输出: {:?}",
                e, input, repaired
            )
        });
        assert_eq!(v["msg"], "done}", "字符串内的 }} 必须保留");
        assert_eq!(v["ok"], true);
    }
}
