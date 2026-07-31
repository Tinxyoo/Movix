use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// 修复(S3,关键):剥离文本中的终端控制字符 / ANSI 转义序列,防止模型或工具输出
/// (web_fetch/read_file 抓到的内容、被诱导的 LLM 回复)劫持终端。
///
/// 攻击向量:输出含 `\x1b[2J`(清屏)、`\x1b]0;evil\x07`(改标题栏钓鱼)、
/// `\x1b[?1049h`(切备用屏隐藏操作)、`\x1b]52;c;<base64>\x07`(OSC 52 剪贴板窃取)等。
/// ratatui 的 Span 不转义控制字符,会原样写入后端 buffer,crossterm flush 时按控制序列
/// 解释 → 终端劫持 + 审批 UI 可被视觉欺骗(社工绕过审批)。
///
/// 策略:1) 删除 CSI 序列 `ESC [` ...终结符;2) 删除 OSC 序列 `ESC ]` ...`BEL`/`ST`;
/// 3) 其余 C0 控制字符(0x00-0x1F 除 Tab/LF/CR)与 DEL(0x7F)、8-bit CSI(0x9B)替换为 `·`。
/// pub(crate) 供 cli/render.rs 对思考块、工具输出、审批弹窗等所有渲染路径复用。
pub(crate) fn sanitize_terminal_output(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == '\x1b' {
            // CSI: ESC [ ... 0x40-0x7E
            if i + 1 < bytes.len() && bytes[i + 1] == '[' {
                i += 2;
                while i < bytes.len() && !((bytes[i] as u32) >= 0x40 && (bytes[i] as u32) <= 0x7E) {
                    i += 1;
                }
                // 跳过终结符
                if i < bytes.len() {
                    i += 1;
                }
                continue;
            }
            // OSC: ESC ] ... (BEL \x07 或 ST \x1b\\)
            if i + 1 < bytes.len() && bytes[i + 1] == ']' {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == '\x07' {
                        i += 1;
                        break;
                    }
                    if bytes[i] == '\x1b' && i + 1 < bytes.len() && bytes[i + 1] == '\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            // 其它 ESC 序列(如 ESC = , ESC > ):跳过 ESC 与下一字符
            i += 2;
            continue;
        }
        let code = c as u32;
        if code == 0x9B {
            // 8-bit CSI,按 CSI 处理
            i += 1;
            while i < bytes.len() && !((bytes[i] as u32) >= 0x40 && (bytes[i] as u32) <= 0x7E) {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        // C0 控制字符:保留 \t \n \r,其余替换为 ·
        if code < 0x20 && c != '\t' && c != '\n' && c != '\r' {
            out.push('·');
            i += 1;
            continue;
        }
        if code == 0x7F {
            out.push('·');
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

pub fn render_markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    // 修复(S3):入口处先剥离终端控制字符,防止模型/工具输出劫持终端。
    let text = sanitize_terminal_output(text);
    let mut lines = Vec::new();
    let mut in_code_block = false;

    for raw_line in text.lines() {
        if raw_line.starts_with("```") {
            if in_code_block {
                in_code_block = false;
                lines.push(Line::from(Span::styled(
                    "  └──".to_string(),
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                in_code_block = true;
                // 修复(S17):lang 限制为 [A-Za-z0-9+-],超长截断,防止 label 断行/注入。
                let code_lang_raw = raw_line.trim_start_matches('`').trim();
                let code_lang: String = code_lang_raw
                    .chars()
                    .filter(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '_' | '#' | '.')
                    })
                    .take(20)
                    .collect();
                let label = if code_lang.is_empty() {
                    "  ┌─ code".to_string()
                } else {
                    format!("  ┌─ {}", code_lang)
                };
                lines.push(Line::from(Span::styled(
                    label,
                    Style::default().fg(Color::DarkGray),
                )));
            }
            continue;
        }

        if in_code_block {
            lines.push(Line::from(vec![
                Span::styled("  │ ".to_string(), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    raw_line.to_string(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
            continue;
        }

        let trimmed = raw_line.trim();

        // 统一处理标题：数 # 个数确定级别
        let heading_level = trimmed.chars().take_while(|c| *c == '#').count();
        if heading_level > 0
            && heading_level <= 6
            && trimmed.chars().nth(heading_level) == Some(' ')
        {
            let content = trimmed[heading_level..].trim();
            let indent = " ".repeat(3 - heading_level.min(3));
            let style = Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD);
            lines.push(Line::from(render_inline(
                &format!("{}{}", indent, content),
                Some(style),
                width,
            )));
            if heading_level == 1 {
                lines.push(Line::from(Span::styled(
                    "─".repeat(width.min(60)),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        } else if trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("• ")
        {
            // 修复(panic):`•`(U+2022)是 3 字节字符,`• ` 前缀共 4 字节。原代码写死
            // `trimmed[2..]`,对 `- ` / `* `(2 字节)恰好安全,但 `• xxx` 一行会在
            // 字节索引 2 处切进 `•` 中间,触发 "byte index 2 is not a char boundary"
            // panic,直接崩溃整个 TUI。改为按实际匹配到的前缀计算字节长度。
            let bullet_len = if trimmed.starts_with("• ") {
                "• ".len()
            } else {
                2
            };
            let content = trimmed[bullet_len..].trim();
            lines.push(Line::from(vec![
                Span::styled("  • ", Style::default().fg(Color::Cyan)),
                Span::styled(content.to_string(), Style::default()),
            ]));
        } else if trimmed.starts_with("---")
            || trimmed.starts_with("***")
            || trimmed.starts_with("___")
        {
            lines.push(Line::from(Span::styled(
                "─".repeat(width.min(60)),
                Style::default().fg(Color::DarkGray),
            )));
        } else if trimmed.is_empty() {
            lines.push(Line::from(""));
        } else {
            lines.push(Line::from(render_inline(trimmed, None, width)));
        }
    }

    lines
}

fn render_inline(text: &str, base_style: Option<Style>, _width: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut remaining = text;
    let default_style = base_style.unwrap_or_default();

    while !remaining.is_empty() {
        if let Some(pos) = remaining.find("**") {
            if pos > 0 {
                spans.push(Span::styled(remaining[..pos].to_string(), default_style));
            }
            let after = &remaining[pos + 2..];
            if let Some(end) = after.find("**") {
                spans.push(Span::styled(
                    after[..end].to_string(),
                    default_style.add_modifier(Modifier::BOLD),
                ));
                remaining = &after[end + 2..];
            } else {
                spans.push(Span::styled(format!("**{}", after), default_style));
                break;
            }
        } else if let Some(pos) = remaining.find('`') {
            if pos > 0 {
                spans.push(Span::styled(remaining[..pos].to_string(), default_style));
            }
            let after = &remaining[pos + 1..];
            if let Some(end) = after.find('`') {
                spans.push(Span::styled(
                    after[..end].to_string(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::ITALIC),
                ));
                remaining = &after[end + 1..];
            } else {
                spans.push(Span::styled(format!("`{}", after), default_style));
                break;
            }
        } else {
            spans.push(Span::styled(remaining.to_string(), default_style));
            break;
        }
    }

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), default_style));
    }

    spans
}

pub fn render_thinking(text: &str, streaming: bool, _width: usize) -> Vec<Line<'static>> {
    let line_count = text.lines().count();
    let max_preview = 4;
    let thinking_style = Style::default()
        .fg(Color::Rgb(211, 170, 112))
        .add_modifier(Modifier::ITALIC);

    // 确定要显示的行（转为 owned String 避免 lifetime 问题）
    let lines_to_show: Vec<String> = if streaming && line_count > max_preview {
        text.lines()
            .rev()
            .take(max_preview)
            .collect::<Vec<&str>>()
            .into_iter()
            .rev()
            .map(|s| s.to_string())
            .collect()
    } else if !streaming {
        let summary = extract_thinking_summary(text);
        if summary.is_empty() {
            return vec![];
        }
        summary
            .lines()
            .take(max_preview)
            .map(|s| s.to_string())
            .collect()
    } else {
        text.lines().map(|s| s.to_string()).collect()
    };

    // 修复(终端注入):思考内容原样渲染可含 ESC 转义(OSC52 剪贴板窃取/清屏/改标题),
    // 逐行 sanitize。
    let mut result: Vec<Line<'static>> = lines_to_show
        .iter()
        .map(|line| {
            Line::from(Span::styled(
                format!("╎ {}", sanitize_terminal_output(line)),
                thinking_style,
            ))
        })
        .collect();

    // 流式模式且行数超过预览时，末尾加省略
    if streaming && line_count > max_preview {
        result.push(Line::from(Span::styled(
            "╎ …".to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    }

    result
}

fn extract_thinking_summary(text: &str) -> String {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("summary:") || trimmed.starts_with("Summary:") {
            return trimmed
                .trim_start_matches("summary:")
                .trim_start_matches("Summary:")
                .trim()
                .to_string();
        }
        if trimmed.starts_with("In summary")
            || trimmed.starts_with("In short")
            || trimmed.starts_with("To summarize")
        {
            return trimmed.to_string();
        }
    }
    let line_count = text.lines().count();
    if line_count > 6 {
        text.lines().next_back().unwrap_or("").to_string()
    } else {
        text.to_string()
    }
}

pub fn tool_glyph(tool_name: &str) -> (&'static str, Color) {
    match tool_name {
        "read_file" => ("▷", Color::Cyan),
        "write_file" => ("◆", Color::Green),
        "list_directory" => ("▷", Color::Cyan),
        "execute_shell" => ("▶", Color::Yellow),
        "search_code" | "grep" => ("⌕", Color::Magenta),
        "web_search" | "web_fetch" => ("⌕", Color::Blue),
        "git_status" | "git_diff" | "git_log" => ("▷", Color::Cyan),
        _ => ("•", Color::DarkGray),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归(审查):`• ` 开头的无序列表行此前触发 `byte index 2 is not a char boundary`
    /// panic,崩溃整个 TUI。修复后必须正常渲染。
    #[test]
    fn bullet_unicode_line_does_not_panic() {
        let lines = render_markdown("第一行\n• 项目符号行\n- 短横线项\n* 星号项", 80);
        assert!(!lines.is_empty());
        // `• ` 前缀的行被渲染为 bullet 行
        assert!(lines.len() >= 3);
    }

    #[test]
    fn sanitize_strips_escape_sequences() {
        let clean = sanitize_terminal_output("\x1b]52;c;c2VjcmV0\x07hello\x1b[2J world");
        assert!(!clean.contains('\x1b'));
        assert!(clean.contains("hello"));
        assert!(clean.contains("world"));
        assert!(!clean.contains("c2VjcmV0"));
    }

    #[test]
    fn sanitize_keeps_ansi_friendly_text() {
        let clean = sanitize_terminal_output("正常文本 with \u{2022} bullet");
        assert_eq!(clean, "正常文本 with \u{2022} bullet");
    }
}
