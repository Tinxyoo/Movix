//! Scavenger: 从 LLM 思维内容中提取隐含的工具调用候选。
//!
//! 修复(P4.3):从 arg_repair.rs 拆分为独立模块。
//! Scavenger 负责从 LLM 输出中"拾荒"遗漏的工具调用,
//! arg_repair 负责修复损坏的 JSON 参数,两者职责不同不应混在一起。

use std::collections::HashSet;
use std::sync::OnceLock;

use crate::common::arg_repair::repair_with_report;

static DSML_RE: OnceLock<regex::Regex> = OnceLock::new();
static JSON_TOOL_RE: OnceLock<regex::Regex> = OnceLock::new();
static NAME_ONLY_RE: OnceLock<regex::Regex> = OnceLock::new();

fn dsml_regex() -> &'static regex::Regex {
    // 修复(Medium #3):原 `([^<]*)` 禁止任何 `<`,含 `<` 的 JSON 参数(比较运算符、
    // HTML 片段)会被截断。改用非贪婪到 `</arguments>` 的匹配。
    DSML_RE.get_or_init(|| {
        regex::Regex::new(
            r#"(?s)\{tool_call\}\s*<name>([^<]+)</name>\s*<arguments>(.*?)</arguments>\s*\{/tool_call\}"#,
        )
        .expect("DSML regex should be valid")
    })
}

fn json_tool_regex() -> &'static regex::Regex {
    // 修复(Medium #M12):原捕获组 `(\{.*?\})` 非贪婪,遇嵌套对象参数(如
    // {"arguments":{"path":"/x","opts":{"a":1}}})会在第一个 `}` 提前终止,
    // 截断嵌套 JSON。这里只捕获 `"name"` 和 `"arguments"\s*:`,后面跟一个
    // 占位 `{`,真正的参数体由 extract_json_tool_calls 用括号配对扫描提取,
    // 支持任意深度嵌套。
    JSON_TOOL_RE.get_or_init(|| {
        regex::Regex::new(r#"(?s)"name"\s*:\s*"([^"]+)"\s*,\s*"arguments"\s*:\s*\{"#)
            .expect("JSON tool regex should be valid")
    })
}

fn name_only_regex() -> &'static regex::Regex {
    NAME_ONLY_RE.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)(?:^|[\s\.\?!;:])(?:call|use|invoke|execute|run)\b\s+(?:the\s+|a\s+|an\s+|tool\s+)?[`'"]?([a-zA-Z_][a-zA-Z0-9_]*)[`'"]?"#,
        )
        .expect("Name-only regex should be valid")
    })
}

pub struct Scavenger;

impl Scavenger {
    pub fn scavenge(
        thinking_content: &str,
        existing_tool_names: &[String],
        known_tools: &[String],
    ) -> Vec<ToolCallCandidate> {
        let mut candidates = Vec::new();
        let existing_set: HashSet<&str> = existing_tool_names.iter().map(|s| s.as_str()).collect();
        let known_set: HashSet<&str> = known_tools.iter().map(|s| s.as_str()).collect();

        candidates.extend(Self::extract_dsml(thinking_content));
        candidates.extend(Self::extract_json_tool_calls(thinking_content));
        candidates.extend(Self::extract_name_only(thinking_content, &known_set));

        candidates.retain(|c| !existing_set.contains(c.name.as_str()));

        let mut best_by_name: std::collections::HashMap<String, ToolCallCandidate> =
            std::collections::HashMap::with_capacity(candidates.len());
        for cand in candidates {
            match best_by_name.entry(cand.name.clone()) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if cand.confidence > e.get().confidence {
                        e.insert(cand);
                    }
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(cand);
                }
            }
        }
        let mut deduped: Vec<ToolCallCandidate> = best_by_name.into_values().collect();
        deduped.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        deduped
    }

    fn extract_dsml(content: &str) -> Vec<ToolCallCandidate> {
        let mut candidates = Vec::new();
        for cap in dsml_regex().captures_iter(content) {
            let name = cap[1].trim().to_string();
            let args_str = cap[2].trim();
            let (args, confidence) = if args_str.is_empty() {
                (serde_json::Value::Object(serde_json::Map::new()), 0.6)
            } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(args_str) {
                (v, 0.9)
            } else {
                let (repaired, report) = repair_with_report(args_str);
                if report.fallback {
                    (repaired, 0.5)
                } else {
                    (repaired, 0.75)
                }
            };
            candidates.push(ToolCallCandidate {
                name,
                arguments: args.to_string(),
                confidence,
                source: ScavengeSource::Dsml,
            });
        }
        candidates
    }

    fn extract_json_tool_calls(content: &str) -> Vec<ToolCallCandidate> {
        let mut candidates = Vec::new();
        for cap in json_tool_regex().captures_iter(content) {
            let name = cap[1].trim().to_string();
            // 修复(Medium #M12):正则只锚定到 `"arguments"\s*:\s*{`,真正的参数体
            // 用括号配对扫描提取,支持任意深度嵌套对象。
            // cap.get(0) 是整体匹配,其 end 位置就是 `{` 之后第一个字符。
            let after_open = match cap.get(0) {
                Some(m) => m.end(),
                None => continue,
            };
            let args_str = match extract_balanced_braces(content, after_open) {
                Some(s) => format!("{{{}}}", s),
                None => continue,
            };
            let (args, confidence) =
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&args_str) {
                    (v, 0.85)
                } else {
                    let (repaired, report) = repair_with_report(&args_str);
                    if report.fallback {
                        (repaired, 0.4)
                    } else {
                        (repaired, 0.7)
                    }
                };
            candidates.push(ToolCallCandidate {
                name,
                arguments: args.to_string(),
                confidence,
                source: ScavengeSource::Json,
            });
        }
        candidates
    }

    fn extract_name_only(content: &str, known_tools: &HashSet<&str>) -> Vec<ToolCallCandidate> {
        // 修复(Medium #M13):原 confidence=0.3,且参数为空 "{}"。自然语言动词短语
        // ("let me run the build"、"use grep to find...")极易误命中已知工具名,
        // 若消费方不设 ≥0.5 阈值就可能以空参数执行 grep/build 等工具。
        // 降到 0.15,使任何"只在没更好候选时才考虑"的消费阈值(常见 ≥0.2)都能滤掉,
        // 同时保留"作为最后手段的线索"语义。
        let mut candidates = Vec::new();
        for cap in name_only_regex().captures_iter(content) {
            let name = cap[1].trim().to_string();
            if known_tools.contains(name.as_str()) {
                candidates.push(ToolCallCandidate {
                    name,
                    arguments: "{}".to_string(),
                    confidence: 0.15,
                    source: ScavengeSource::NameOnly,
                });
            }
        }
        candidates
    }
}

/// 从 `content` 的 `start_byte` 位置开始,提取一个括号配对的字符串(不含首尾括号)。
///
/// 用于 `extract_json_tool_calls`:正则已锚定到 `"arguments"\s*:\s*{`,
/// 本函数从这个 `{` 之后扫描,跟踪字符串状态与 `{}` 嵌套深度,在深度归零时返回。
/// 支持任意深度嵌套对象;遇字符串内的括号不计入深度。
fn extract_balanced_braces(content: &str, start_byte: usize) -> Option<String> {
    // 修复(G-M12):原实现用 bytes[i] + `out.push(ch as char)` 逐字节处理,
    // 多字节 UTF-8 字符(中文3字节/emoji4字节)被拆成独立码点 → 乱码。
    // 改为按字节扫描(用于检测 {} 和 "),但输出按 UTF-8 字符边界切片追加,
    // 保证多字节字符保真。
    if start_byte == 0 || start_byte > content.len() {
        return None;
    }
    let bytes = content.as_bytes();
    let mut depth: i32 = 1;
    let mut in_string = false;
    let mut escape = false;
    // 记录「上一个未复制的边界」,用字节切片批量追加,保证 UTF-8 边界对齐。
    let chunk_start = start_byte;
    let mut i = start_byte;
    while i < bytes.len() {
        let ch = bytes[i];
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if ch == b'\\' && in_string {
            escape = true;
            i += 1;
            continue;
        }
        if ch == b'"' {
            in_string = !in_string;
            i += 1;
            continue;
        }
        if !in_string && (ch == b'{' || ch == b'}') {
            if ch == b'{' {
                depth += 1;
                i += 1;
            } else {
                depth -= 1;
                if depth == 0 {
                    // 到达匹配的闭合 `}`,返回 chunk_start..i(不含闭合括号)。
                    return Some(content[chunk_start..i].to_string());
                }
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    // 未闭合;返回已扫描部分,交给下游 repair 尝试。
    if depth > 0 {
        Some(content[chunk_start..].to_string())
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub struct ToolCallCandidate {
    pub name: String,
    pub arguments: String,
    pub confidence: f64,
    pub source: ScavengeSource,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScavengeSource {
    Dsml,
    Json,
    NameOnly,
}

impl std::fmt::Display for ScavengeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScavengeSource::Dsml => write!(f, "DSML"),
            ScavengeSource::Json => write!(f, "JSON"),
            ScavengeSource::NameOnly => write!(f, "NameOnly"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_extraction() {
        let content =
            r#"I should call {"name": "grep", "arguments": {"pattern": "TODO"}} to find todos."#;
        let candidates = Scavenger::scavenge(content, &[], &["grep".to_string()]);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].name, "grep");
    }

    #[test]
    fn test_dedup_existing() {
        let content = "some thinking";
        let candidates = Scavenger::scavenge(content, &["read_file".to_string()], &[]);
        assert!(candidates.is_empty());
    }

    /// 回归(G-M12):extract_balanced_braces 必须保真中文参数。
    /// 原实现按字节 `u8 as char` 处理,中文(3字节)被腐蚀成乱码。
    #[test]
    fn extract_balanced_braces_preserves_cjk() {
        // content: `{"name":"write_file","arguments": {"content":"你好"}}`
        // 正则锚定到 `"arguments":\s*{`,start_byte 是 `{` 之后。
        let content = r#"{"name":"write_file","arguments": {"content":"你好"}}"#;
        // 找到 `"arguments":` 后第一个 `{` 的位置 + 1(跳过 `{`)
        let args_pos = content.find(r#""arguments""#).expect("必须有 arguments");
        let brace_pos = content[args_pos..]
            .find('{')
            .map(|p| p + args_pos)
            .expect("必须有 {");
        let start = brace_pos + 1;
        let extracted = extract_balanced_braces(content, start).expect("应提取成功");
        // 中文"你好"必须保真(非乱码)
        assert!(
            extracted.contains("你好"),
            "中文参数必须保真,实际: {:?}",
            extracted
        );
    }
}
