//! P5+:TUI 渲染层。
//!
//! 所有 `draw_*` / `build_*` / `extract_*` / `set_cursor_*` 等
//! "把 `App` 状态画到 ratatui frame" 的纯函数集中在此模块。
//!
//! 关键约束:
//! - 只读 `App` 状态(或 `&mut App` 仅在缓存失效时,如 `set_cursor_from_mouse`)
//! - 辅助 `input_height` / `ui_label` 来自父模块,通过 `use super::*` 引入
//! - `pub(crate) fn` 暴露给 `cli::events::run_interactive` 调用

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::cli::layout::{calc_approval_height, root_rows};
use crate::cli::text::{fmt_num, truncate_str, unicode_display_width};
use crate::cli::theme::{rgb, *};
use crate::common::i18n::Lang;
use crate::common::markdown;

// 父模块的辅助 fn / 常量 / 类型(子模块可访问父模块私有项)。
// 注意:`input_height` / `command_suggestion_area` / `command_at` / `contains` /
// `copy_to_clipboard` / `set_cursor_from_mouse` / `extract_selected_text` /
// `col_range_to_byte_range` / `input_cursor_position` 都在本模块内部,
// 不需要从 super 引用。
use super::types::MessageRole;
use super::{
    App, FocusZone, MIN_TERM_HEIGHT, MIN_TERM_WIDTH, PALETTE_MAX_VISIBLE, PALETTE_MAX_WIDTH,
    SIDEBAR_MEDIUM, SIDEBAR_MEDIUM_THRESHOLD, SIDEBAR_WIDE, SIDEBAR_WIDE_THRESHOLD, SPINNER,
    TaskStatus, TurnStatus,
};

// 跨子模块的辅助 fn(留在 mod.rs 中,本模块作为消费者)。
use super::{estimate_cost, ui_label};

pub(crate) fn draw_ui(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width < MIN_TERM_WIDTH || area.height < MIN_TERM_HEIGHT {
        f.render_widget(
            Paragraph::new(format!(
                "Terminal too small (min {}x{})",
                MIN_TERM_WIDTH, MIN_TERM_HEIGHT
            ))
            .style(Style::default().fg(ds_red())),
            area,
        );
        return;
    }

    // 修复(颜色):先填充整个屏幕的深色背景。此前未覆盖区域会露出终端默认底色
    // (Color::Reset),如果终端是白底(Terminal.app Basic profile),这些区域就是
    // 白色,和深色面板混杂 → 视觉割裂。统一填深色背景确保所有平台一致。
    f.render_widget(
        Block::default().style(Style::default().bg(rgb(12, 14, 22))),
        area,
    );

    let approval_h = if let Some((ref tool, ref detail)) = app.agent_state.pending_approval {
        calc_approval_height(tool, detail, area.width).max(10)
    } else {
        0
    };
    let rows = root_rows(area, input_height(app), approval_h);

    draw_header(f, rows[0], app);

    let sidebar_w = if area.width >= SIDEBAR_WIDE_THRESHOLD {
        SIDEBAR_WIDE
    } else if area.width >= SIDEBAR_MEDIUM_THRESHOLD {
        SIDEBAR_MEDIUM
    } else {
        0
    };

    let chat_area = rows[1];
    let approval_area = if approval_h > 0 { Some(rows[2]) } else { None };

    if sidebar_w > 0 {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(42), Constraint::Length(sidebar_w)])
            .split(chat_area);
        draw_chat(f, body[0], app);
        draw_sidebar(f, body[1], app);
    } else {
        draw_chat(f, chat_area, app);
    }

    if let Some(area) = approval_area {
        draw_approval_notice(f, area, app);
    }

    let input_area = if approval_h > 0 { rows[3] } else { rows[2] };
    draw_input(f, input_area, app);

    let stats_area = if approval_h > 0 { rows[4] } else { rows[3] };
    draw_stats_bar(f, stats_area, app);

    if app.view.help_open {
        draw_help(f, area, app);
    } else if app.input.input_buf.starts_with('/') && !app.view.streaming {
        draw_command_suggestions(f, area, input_area, app);
    }
}

/// 计算输入框区域高度，考虑视觉换行（长行自动折行）
// 留在 cli.rs(读 `App::input_buf` 私有字段);同款算法可见 `tui_layout::calc_approval_height`。
pub(crate) fn input_height(app: &App) -> u16 {
    let term_w = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
    let avail_w = (term_w as usize).saturating_sub(2);
    let mut visual_lines: u16 = 0;
    for line in app.input.input_buf.split('\n') {
        if avail_w == 0 {
            visual_lines += 1;
            continue;
        }
        let line_w = unicode_width::UnicodeWidthStr::width(line);
        visual_lines += if line_w == 0 {
            1
        } else {
            line_w.div_ceil(avail_w) as u16
        };
    }
    (visual_lines.max(1) + 2).clamp(5, 20)
}

pub(crate) fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    // 修复(颜色):此前用 Color::Reset 透出终端默认底色,如果终端是白底
    // (Terminal.app Basic profile),头部就是白色,和深色面板割裂。
    // 改用和全屏背景一致的深色,确保视觉统一。
    let block = Block::default()
        .borders(Borders::NONE)
        .style(Style::default().bg(rgb(12, 14, 22)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(16)])
        .split(inner);

    let busy = app.view.streaming;
    let tick = app.view.tick as usize;
    let status_glyph = if busy {
        // ── Dual-wave interference scanner ──
        // Two counter-propagating waves create a dynamic moiré-like pattern
        // across 5 columns with 4 intensity levels:  ·  ▪  ▣  ■
        let t1 = tick % 23; // primary wave period (0..22 → p1: 0..4)
        let t2 = (tick.wrapping_mul(3) / 2) % 22; // secondary wave (faster, detuned, 0..21 → p2: 0..4)
        let level = ['·', '▪', '▣', '■'];
        let mut s = String::with_capacity(5);
        for col in 0..5usize {
            let col2 = col as isize;
            // Wave 1: left → right scanning bar, soft trailing edge
            let p1 = (t1 as isize * 2 + 10) / 11; // 0..4 mapped from 0..27
            let d1 = (col2 - p1).abs();
            let v1 = if d1 <= 2 { (2 - d1) as usize } else { 0 };
            // Wave 2: right → left ghost pulse, faster
            let p2 = 4 - ((t2 as isize * 2 + 10) / 11);
            let d2 = (col2 - p2).abs();
            let v2 = if d2 == 0 {
                3usize
            } else if d2 == 1 {
                1
            } else {
                0
            };
            let idx = (v1 + v2).min(3);
            s.push(level[idx]);
        }
        s
    } else {
        // ── Idle: subtle anchor dot, rest dim ──
        "▪ · · · ·".to_string()
    };
    let status_color = if busy { ds_blue() } else { rgb(44, 55, 72) };

    let brand = Line::from(vec![
        Span::styled(
            " Movix ",
            Style::default()
                .fg(focus_chat())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            status_glyph,
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(Paragraph::new(brand), cols[0]);

    // ── Context bar: minimal ──────────────────────────────────────────
    let pct = if app.view.context_max > 0 {
        (app.view.context_used as f64 / app.view.context_max as f64 * 100.0).min(100.0)
    } else {
        0.0
    };

    // Silent slate by default; only accent at warning thresholds
    let accent = if pct >= 90.0 {
        rgb(248, 113, 113) // red   – critical
    } else if pct >= 75.0 {
        rgb(251, 146, 60) // orange – warning
    } else {
        rgb(51, 65, 85) // slate-700 – invisible calm
    };
    let pct_fg = if pct >= 75.0 {
        accent
    } else {
        rgb(71, 85, 105)
    };

    let bar_w = 8usize;
    let filled = ((pct / 100.0) * bar_w as f64).round() as usize;
    let filled = filled.min(bar_w);

    let ctx_line = Line::from(vec![
        Span::styled("◆".repeat(filled), Style::default().fg(accent)),
        Span::styled(
            "◇".repeat(bar_w - filled),
            Style::default().fg(rgb(30, 41, 59)),
        ),
        Span::styled(format!("  {:.0}%", pct), Style::default().fg(pct_fg)),
    ]);
    f.render_widget(
        Paragraph::new(ctx_line).alignment(ratatui::layout::Alignment::Right),
        cols[1],
    );
}

pub(crate) fn draw_thinking_block(lines: &mut Vec<Line>, w: usize, app: &App) {
    let thinking_text = app.view.thinking_lines.join("\n");
    let thinking_lines = markdown::render_thinking(&thinking_text, app.view.streaming, w);
    let reas_bg = rgb(30, 25, 10);
    let left_border = Span::styled(
        " ▍ ",
        Style::default().fg(amber()).add_modifier(Modifier::BOLD),
    );

    let label = format!(
        "🗲 REASONING [ {} ]",
        if app.view.streaming {
            ui_label(app, "active").to_uppercase()
        } else {
            ui_label(app, "complete").to_uppercase()
        }
    );
    let label_len = unicode_display_width(&label);
    let pad_len = w.saturating_sub(3 + label_len);
    let pad_str = " ".repeat(pad_len);

    lines.push(Line::from(vec![
        left_border.clone(),
        Span::styled(
            label,
            Style::default()
                .fg(amber())
                .bg(reas_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(pad_str, Style::default().bg(reas_bg)),
    ]));

    if app.view.thinking_expanded {
        for tl in &thinking_lines {
            let mut spans = vec![left_border.clone()];
            let line_w = tl
                .spans
                .iter()
                .map(|s| unicode_display_width(&s.content))
                .sum::<usize>();
            let pad_len = w.saturating_sub(3 + line_w);
            let pad_str = " ".repeat(pad_len);

            let styled_spans: Vec<Span> = tl
                .spans
                .clone()
                .into_iter()
                .map(|mut s| {
                    s.style = s.style.fg(text_reasoning()).bg(reas_bg);
                    s
                })
                .collect();

            spans.extend(styled_spans);
            spans.push(Span::styled(pad_str, Style::default().bg(reas_bg)));
            lines.push(Line::from(spans));
        }
    } else {
        let col_text = ui_label(app, "reasoning_collapsed").to_string();
        let text_w = unicode_display_width(&col_text);
        let pad_len = w.saturating_sub(3 + text_w);
        let pad_str = " ".repeat(pad_len);

        lines.push(Line::from(vec![
            left_border.clone(),
            Span::styled(col_text, Style::default().fg(text_dim()).bg(reas_bg)),
            Span::styled(pad_str, Style::default().bg(reas_bg)),
        ]));
    }
    lines.push(Line::from(""));
}

pub(crate) fn stats_title_line(app: &App) -> String {
    let zh = matches!(app.lang, Lang::Zh);
    let mut parts: Vec<String> = Vec::new();

    let total = if let Some(ref stats) = app.session.last_stats {
        stats.total_tokens
    } else {
        app.session.session_total_tokens
    };
    if total > 0 {
        parts.push(format!(
            "{} {}",
            if zh { "总" } else { "T" },
            fmt_num(total)
        ));
    }

    if let Some(ref stats) = app.session.last_stats {
        if stats.prompt_tokens > 0 || stats.completion_tokens > 0 {
            parts.push(format!(
                "{} {}",
                if zh { "入" } else { "I" },
                fmt_num(stats.prompt_tokens)
            ));
            parts.push(format!(
                "{} {}",
                if zh { "出" } else { "O" },
                fmt_num(stats.completion_tokens)
            ));
        }
        if stats.reasoning_tokens > 0 {
            parts.push(format!(
                "{} {}",
                if zh { "推" } else { "R" },
                fmt_num(stats.reasoning_tokens)
            ));
        }
        if stats.cache_hit_tokens + stats.cache_miss_tokens > 0 {
            let hit_rate = stats.cache_hit_tokens as f64
                / (stats.cache_hit_tokens + stats.cache_miss_tokens) as f64
                * 100.0;
            parts.push(format!("C {:.0}%", hit_rate));
            parts.push(format!("{}H", fmt_num(stats.cache_hit_tokens)));
        }
        let cost = estimate_cost(stats, app.agent_state.agent.current_model());
        if cost > 0.0 {
            parts.push(format!("¥{:.4}", cost));
        }
    }

    if app.session.session_total_cost > 0.0 {
        parts.push(format!("¥{:.4}", app.session.session_total_cost));
    }

    if app.view.streaming {
        if app.session.last_elapsed > 0.0 {
            parts.push(format!(
                "{} {:.1}s",
                if zh { "耗时" } else { "Time" },
                app.session.last_elapsed
            ));
        }
        if app.session.last_iterations > 0 {
            parts.push(format!(
                "{} {}",
                if zh { "轮" } else { "Iter" },
                app.session.last_iterations
            ));
        }
    }

    parts.join("  │  ")
}

/// 构建聊天区域的所有渲染行，仅在消息变化时调用
pub(crate) fn build_chat_lines(app: &App, w: usize, h: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    if !app.view.messages.is_empty() {
        lines.push(Line::from(""));
    }

    let last_user_idx = app
        .view
        .messages
        .iter()
        .rposition(|m| m.role == MessageRole::User);

    for (i, msg) in app.view.messages.iter().enumerate() {
        let is_last_user = Some(i) == last_user_idx;
        match msg.role {
            MessageRole::User => {
                let user_accent = app.mode_color();
                let user_bg = rgb(20, 26, 42);
                let left_border = Span::styled(
                    " ▍ ",
                    Style::default()
                        .fg(user_accent)
                        .add_modifier(Modifier::BOLD),
                );

                let (badge_text, _, _) = app.mode_badge();
                let label = format!("👤 USER [ {} ]", badge_text);
                let label_len = unicode_display_width(&label);
                let pad_len = w.saturating_sub(3 + label_len);
                let pad_str = " ".repeat(pad_len);

                lines.push(Line::from(vec![
                    left_border.clone(),
                    Span::styled(
                        label,
                        Style::default()
                            .fg(user_accent)
                            .bg(user_bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(pad_str, Style::default().bg(user_bg)),
                ]));

                for line in msg.content.lines() {
                    let line_str = line.to_string();
                    let text_w = unicode_display_width(&line_str);
                    let pad_len = w.saturating_sub(3 + text_w);
                    let pad_str = " ".repeat(pad_len);
                    lines.push(Line::from(vec![
                        left_border.clone(),
                        Span::styled(line_str, Style::default().fg(text_body()).bg(user_bg)),
                        Span::styled(pad_str, Style::default().bg(user_bg)),
                    ]));
                }
                lines.push(Line::from(""));

                if is_last_user
                    && !app.view.thinking_lines.is_empty()
                    && !app.view.thinking_lines[0].is_empty()
                {
                    draw_thinking_block(&mut lines, w, app);
                }
            }
            MessageRole::Assistant => {
                let ai_bg = rgb(16, 24, 40);
                let left_border = Span::styled(
                    " ▍ ",
                    Style::default().fg(ds_sky()).add_modifier(Modifier::BOLD),
                );

                let label = "🤖 Movix ";
                let label_len = unicode_display_width(label);
                let pad_len = w.saturating_sub(3 + label_len);
                let pad_str = " ".repeat(pad_len);

                lines.push(Line::from(vec![
                    left_border.clone(),
                    Span::styled(
                        label,
                        Style::default()
                            .fg(ds_sky())
                            .bg(ai_bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(pad_str, Style::default().bg(ai_bg)),
                ]));

                let md_lines = markdown::render_markdown(&msg.content, w.saturating_sub(4));
                for md_line in md_lines {
                    let mut spans = vec![left_border.clone()];
                    let line_w = md_line
                        .spans
                        .iter()
                        .map(|s| unicode_display_width(&s.content))
                        .sum::<usize>();
                    let pad_len = w.saturating_sub(3 + line_w);
                    let pad_str = " ".repeat(pad_len);

                    let styled_spans: Vec<Span> = md_line
                        .spans
                        .into_iter()
                        .map(|mut s| {
                            s.style = s.style.bg(ai_bg);
                            s
                        })
                        .collect();

                    spans.extend(styled_spans);
                    spans.push(Span::styled(pad_str, Style::default().bg(ai_bg)));
                    lines.push(Line::from(spans));
                }
                lines.push(Line::from(""));
            }
            MessageRole::ToolResult => {
                let colon_pos = msg.content.find(':').unwrap_or(0);
                let tool_name = &msg.content[..colon_pos];
                let (glyph, glyph_color) = markdown::tool_glyph(tool_name);
                let tool_bg = rgb(20, 24, 38);
                let left_border = Span::styled(
                    " ▍ ",
                    Style::default()
                        .fg(glyph_color)
                        .add_modifier(Modifier::BOLD),
                );

                let after_colon = msg.content.get(colon_pos + 1..).unwrap_or("").trim_start();
                let (detail_line_raw, output_text) = match after_colon.find('\n') {
                    Some(pos) => (&after_colon[..pos], &after_colon[pos + 1..]),
                    None => (after_colon, ""),
                };
                // 修复(终端注入):detail(工具参数/命令)来自模型输出,可能含 ESC 转义,
                // 原样渲染可劫持终端(OSC52 剪贴板窃取/伪造审批 UI)。统一 sanitize。
                let detail_line = markdown::sanitize_terminal_output(detail_line_raw);

                let is_mutating = tool_name == "write_file" || tool_name == "patch_file";
                let has_diff = output_text.contains('\n')
                    && (output_text.contains("\n+") || output_text.contains("\n-"));

                lines.push(Line::from(vec![
                    left_border.clone(),
                    Span::styled(
                        format!("  {}  ", glyph),
                        Style::default()
                            .fg(glyph_color)
                            .bg(tool_bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {} ", tool_name),
                        Style::default()
                            .fg(glyph_color)
                            .bg(tool_bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {} ", detail_line),
                        Style::default().fg(text_muted()).bg(tool_bg),
                    ),
                    Span::styled(
                        " ".repeat(w.saturating_sub(
                            6 + unicode_display_width(tool_name)
                                + unicode_display_width(&detail_line)
                                + 6,
                        )),
                        Style::default().bg(tool_bg),
                    ),
                ]));

                let diff_add_bg = rgb(16, 40, 24);
                let diff_add_fg = rgb(100, 220, 130);
                let diff_del_bg = rgb(45, 16, 20);
                let diff_del_fg = rgb(240, 100, 100);
                let diff_sep_fg = rgb(80, 90, 110);
                let diff_hunk_fg = rgb(120, 140, 180);

                if !output_text.is_empty() {
                    let max_diff_lines = 40usize;
                    let mut shown_lines = 0usize;
                    let mut total_lines = 0usize;
                    let mut skipped = false;

                    for line_raw in output_text.lines() {
                        total_lines += 1;
                        if shown_lines >= max_diff_lines {
                            skipped = true;
                            break;
                        }

                        // 修复(终端注入):工具输出(web_fetch 抓到的页面、文件内容)可能含
                        // ESC 转义,原样渲染可劫持终端。每行 sanitize 后再渲染。
                        let line = markdown::sanitize_terminal_output(line_raw);
                        let text_w = unicode_display_width(&line);
                        let pad_len = w.saturating_sub(3 + text_w);

                        if line.starts_with('+') && !line.starts_with("++") {
                            let pad_str = " ".repeat(pad_len);
                            lines.push(Line::from(vec![
                                left_border.clone(),
                                Span::styled(
                                    line.to_string(),
                                    Style::default().fg(diff_add_fg).bg(diff_add_bg),
                                ),
                                Span::styled(pad_str, Style::default().bg(diff_add_bg)),
                            ]));
                            shown_lines += 1;
                        } else if line.starts_with('-') && !line.starts_with("--") {
                            let pad_str = " ".repeat(pad_len);
                            lines.push(Line::from(vec![
                                left_border.clone(),
                                Span::styled(
                                    line.to_string(),
                                    Style::default().fg(diff_del_fg).bg(diff_del_bg),
                                ),
                                Span::styled(pad_str, Style::default().bg(diff_del_bg)),
                            ]));
                            shown_lines += 1;
                        } else if line.starts_with("---") || line.starts_with("...") {
                            let pad_str = " ".repeat(pad_len);
                            lines.push(Line::from(vec![
                                left_border.clone(),
                                Span::styled(
                                    line.to_string(),
                                    Style::default().fg(diff_sep_fg).bg(tool_bg),
                                ),
                                Span::styled(pad_str, Style::default().bg(tool_bg)),
                            ]));
                            shown_lines += 1;
                        } else if line.starts_with("@@") {
                            let pad_str = " ".repeat(pad_len);
                            lines.push(Line::from(vec![
                                left_border.clone(),
                                Span::styled(
                                    line.to_string(),
                                    Style::default().fg(diff_hunk_fg).bg(tool_bg),
                                ),
                                Span::styled(pad_str, Style::default().bg(tool_bg)),
                            ]));
                            shown_lines += 1;
                        } else if is_mutating || has_diff {
                            let pad_str = " ".repeat(pad_len);
                            lines.push(Line::from(vec![
                                left_border.clone(),
                                Span::styled(
                                    line.to_string(),
                                    Style::default().fg(text_dim()).bg(tool_bg),
                                ),
                                Span::styled(pad_str, Style::default().bg(tool_bg)),
                            ]));
                            shown_lines += 1;
                        }
                    }

                    if skipped {
                        let remaining = total_lines.saturating_sub(max_diff_lines);
                        let trunc_line = format!("  ... +{} more lines", remaining);
                        let pad_len = w.saturating_sub(3 + unicode_display_width(&trunc_line));
                        lines.push(Line::from(vec![
                            left_border.clone(),
                            Span::styled(trunc_line, Style::default().fg(text_dim()).bg(tool_bg)),
                            Span::styled(" ".repeat(pad_len), Style::default().bg(tool_bg)),
                        ]));
                    }
                }
                lines.push(Line::from(""));
            }
        }
    }

    if app.view.messages.is_empty() && !app.view.streaming {
        let zh = matches!(app.lang, Lang::Zh);
        let vspace = h.saturating_sub(10) / 2;
        for _ in 0..vspace {
            lines.push(Line::from(""));
        }

        let logo_w = 15;
        let pad = w.saturating_sub(logo_w) / 2;
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(pad), Style::default()),
            Span::styled("▐", Style::default().fg(rgb(20, 30, 50))),
            Span::styled("  ", Style::default().bg(rgb(12, 18, 32))),
            Span::styled(
                "M O V I X",
                Style::default()
                    .fg(ds_blue())
                    .bg(rgb(12, 18, 32))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default().bg(rgb(12, 18, 32))),
            Span::styled("▌", Style::default().fg(rgb(20, 30, 50))),
        ]));

        lines.push(Line::from(""));
        let version_str = concat!("v", env!("CARGO_PKG_VERSION"));
        let ver_w = unicode_display_width(version_str);
        let ver_pad = w.saturating_sub(ver_w) / 2;
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(ver_pad), Style::default()),
            Span::styled(
                version_str.to_string(),
                Style::default().fg(rgb(51, 65, 85)),
            ),
        ]));
        lines.push(Line::from(""));
        let sub = if zh {
            "专为deepseek设计的Agent"
        } else {
            "Agent designed for deepseek"
        };
        let sub_w = unicode_display_width(sub);
        let sub_pad = w.saturating_sub(sub_w) / 2;
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(sub_pad), Style::default()),
            Span::styled(
                sub.to_string(),
                Style::default().fg(ds_sky()).add_modifier(Modifier::ITALIC),
            ),
        ]));
        lines.push(Line::from(""));
        let sep_w = w.min(52);
        let sep_pad = (w.saturating_sub(sep_w)) / 2;
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(sep_pad), Style::default()),
            Span::styled("─".repeat(sep_w), Style::default().fg(rgb(25, 35, 55))),
        ]));

        let indent = sep_pad + 2;
        let (k1, k2, k3) = if zh {
            (
                "Tab       切换模式  Plan → Agent → Auto → YOLO",
                "Ctrl+T    切换模型  Pro ↔ Flash",
                "Shift+Tab 切换思考  off → auto → high → max",
            )
        } else {
            (
                "Tab       cycle mode  Plan → Agent → Auto → YOLO",
                "Ctrl+T    toggle model  Pro ↔ Flash",
                "Shift+Tab cycle think  off → auto → high → max",
            )
        };
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(indent), Style::default()),
            Span::styled("◆ ", Style::default().fg(ds_blue())),
            Span::styled(k1.to_string(), Style::default().fg(text_muted())),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(indent), Style::default()),
            Span::styled("◆ ", Style::default().fg(ds_blue())),
            Span::styled(k2.to_string(), Style::default().fg(text_muted())),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(indent), Style::default()),
            Span::styled("◆ ", Style::default().fg(ds_blue())),
            Span::styled(k3.to_string(), Style::default().fg(text_muted())),
        ]));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(""));
    }

    lines
}

pub(crate) fn draw_chat(f: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::default().borders(Borders::NONE);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if !app.view.chat_cache_valid {
        app.view.cached_chat_lines = build_chat_lines(
            app,
            inner.width.saturating_sub(4) as usize,
            inner.height as usize,
        );
        app.view.chat_cache_valid = true;
    }

    let total_lines = app.view.cached_chat_lines.len();
    let visible = inner.height as usize;
    let skip = if app.view.auto_scroll && total_lines > visible {
        total_lines.saturating_sub(visible)
    } else {
        let max_offset = total_lines.saturating_sub(visible);
        max_offset.saturating_sub(app.view.scroll_offset)
    };

    let has_selection = app.view.select_start.is_some() && app.view.select_end.is_some();
    let sel_range = if has_selection {
        let (sx, sy) = app.view.select_start.unwrap_or((0, 0));
        let (ex, ey) = app.view.select_end.unwrap_or((0, 0));
        let (min_row, max_row) = if sy <= ey { (sy, ey) } else { (ey, sy) };
        let (min_col, max_col) = if sy < ey || (sy == ey && sx <= ex) {
            (sx, ex)
        } else {
            (ex, sx)
        };
        Some((min_row, min_col, max_row, max_col))
    } else {
        None
    };

    let visible_lines: Vec<Line> = app
        .view
        .cached_chat_lines
        .iter()
        .skip(skip)
        .enumerate()
        .map(|(vi, line)| {
            let screen_row = (skip + vi) as u16;
            if let Some((min_row, min_col, max_row, max_col)) = sel_range
                && screen_row >= min_row
                && screen_row <= max_row
            {
                let sel_bg = rgb(30, 58, 138);
                let mut new_spans: Vec<Span> = Vec::new();
                let mut col_acc: u16 = 0;
                for span in &line.spans {
                    let span_w = UnicodeWidthStr::width(&*span.content) as u16;
                    let span_start = col_acc;
                    let span_end = col_acc + span_w;

                    let need_highlight = if min_row == max_row {
                        span_end > min_col && span_start < max_col
                    } else if screen_row == min_row {
                        span_end > min_col
                    } else if screen_row == max_row {
                        span_start < max_col
                    } else {
                        true
                    };

                    if need_highlight {
                        new_spans.push(Span::styled(
                            span.content.clone(),
                            span.style.patch(Style::default().bg(sel_bg)),
                        ));
                    } else {
                        new_spans.push(span.clone());
                    }
                    col_acc = span_end;
                }
                return Line::from(new_spans);
            }
            line.clone()
        })
        .collect();

    f.render_widget(
        Paragraph::new(ratatui::text::Text::from(visible_lines)).wrap(Wrap { trim: false }),
        inner,
    );

    if !app.view.auto_scroll && total_lines > visible {
        let pct = skip as f64 / total_lines.saturating_sub(1).max(1) as f64 * 100.0;
        let label = format!(" {:.0}% scroll ", pct);
        let label_w = label.len() as u16;
        let x = inner.width.saturating_sub(label_w);
        let y = inner.height.saturating_sub(1);
        f.render_widget(
            Paragraph::new(Span::styled(
                label,
                Style::default()
                    .fg(text_body())
                    .bg(rgb(30, 41, 59))
                    .add_modifier(Modifier::BOLD),
            )),
            Rect::new(inner.x + x, inner.y + y, label_w, 1),
        );
    }
}

pub(crate) fn draw_approval_notice(f: &mut Frame, area: Rect, app: &App) {
    if let Some((ref tool, ref detail)) = app.agent_state.pending_approval {
        let is_auto_mode = app.agent_state.agent.mode() == crate::agent::modes::AppMode::Auto;
        // P1:cli.rs 这条路径是"用户已点开审批 UI"——此时 risk_level 不可得,
        // 兜底用 Medium,保留旧的"关键字匹配"行为;P3 切到 decide_for_tool 后可拿到真 risk_level。
        let is_high_risk = is_auto_mode
            && crate::agent::modes::is_high_risk_action(
                tool,
                detail,
                crate::tools::RiskLevel::Medium,
            );
        let zh = matches!(app.lang, Lang::Zh);

        let (accent, dim_accent, bg_deep, bg_card) = if is_high_risk {
            (
                rgb(248, 113, 113),
                rgb(127, 29, 29),
                rgb(20, 8, 8),
                rgb(30, 12, 12),
            )
        } else if is_auto_mode {
            (
                rgb(251, 191, 36),
                rgb(120, 80, 10),
                rgb(20, 16, 6),
                rgb(30, 24, 10),
            )
        } else {
            (
                rgb(96, 165, 250),
                rgb(30, 58, 138),
                rgb(6, 10, 20),
                rgb(12, 18, 32),
            )
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(dim_accent))
            .style(Style::default().bg(bg_deep));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let inner_w = inner.width as usize;
        let detail_clean =
            markdown::sanitize_terminal_output(detail.trim_start_matches("[needs-approval] "));

        let (risk_label, risk_badge) = if is_high_risk {
            let label = if zh { "高风险操作" } else { "HIGH RISK" };
            let badge = " ⚠ ";
            (label, badge)
        } else if is_auto_mode {
            let label = if zh { "需确认" } else { "REVIEW" };
            let badge = " ⚡ ";
            (label, badge)
        } else {
            let label = if zh { "等待批准" } else { "APPROVAL" };
            let badge = " ⏳ ";
            (label, badge)
        };

        // 修复(Bug #21):合并两张平行的 tool→icon / tool→label 表为单次 lookup。
        let (tool_icon, tool_label) = tool_meta(tool.as_str(), zh);

        let mut lines: Vec<Line> = Vec::new();

        let header_bg = bg_card;
        let badge_text = format!("{}{} ", risk_badge, risk_label);
        let badge_w = UnicodeWidthStr::width(badge_text.as_str());
        let right_info = if is_high_risk {
            if zh {
                "🔒 安全审查 "
            } else {
                "🔒 Security Review "
            }
        } else if is_auto_mode {
            if zh {
                "🤖 智能审查 "
            } else {
                "🤖 Smart Review "
            }
        } else {
            if zh {
                "📋 操作审批 "
            } else {
                "📋 Approval "
            }
        };
        let right_w = UnicodeWidthStr::width(right_info);
        let pad = inner_w.saturating_sub(badge_w + right_w);

        lines.push(Line::from(vec![
            Span::styled(
                badge_text,
                Style::default()
                    .fg(bg_deep)
                    .bg(accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ".repeat(pad), Style::default().bg(header_bg)),
            Span::styled(
                right_info.to_string(),
                Style::default()
                    .fg(accent)
                    .bg(header_bg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}{} ", tool_icon, tool_label),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "─".repeat(inner_w.saturating_sub(
                    UnicodeWidthStr::width(tool_icon) + UnicodeWidthStr::width(tool_label) + 3,
                )),
                Style::default().fg(dim_accent),
            ),
        ]));

        let detail_w = inner_w.saturating_sub(4);
        let detail_display = if UnicodeWidthStr::width(detail_clean.as_str()) > detail_w {
            let mut s = String::new();
            let mut w = 0;
            for ch in detail_clean.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                if w + cw > detail_w.saturating_sub(1) {
                    break;
                }
                s.push(ch);
                w += cw;
            }
            format!("{}…", s)
        } else {
            detail_clean.to_string()
        };

        lines.push(Line::from(vec![
            Span::styled(" ▸ ", Style::default().fg(accent)),
            Span::styled(detail_display, Style::default().fg(text_body())),
        ]));

        if is_auto_mode && is_high_risk {
            let warn = if zh {
                "检测到高风险操作特征，需要确认后执行"
            } else {
                "High-risk pattern detected, confirmation required"
            };
            lines.push(Line::from(vec![
                Span::styled("   ", Style::default()),
                Span::styled("⚠ ", Style::default().fg(rgb(248, 113, 113))),
                Span::styled(warn.to_string(), Style::default().fg(rgb(252, 165, 165))),
            ]));
        }

        lines.push(Line::from(""));

        let (y_label, n_label) = if zh {
            ("确认执行", "拒绝操作")
        } else {
            ("Approve", "Deny")
        };
        let y_btn = format!(" ✓ {} ", y_label);
        let n_btn = format!(" ✗ {} ", n_label);
        let y_btn_w = UnicodeWidthStr::width(y_btn.as_str());
        let n_btn_w = UnicodeWidthStr::width(n_btn.as_str());
        let sep_w = 3;
        let total_btn_w = y_btn_w + sep_w + n_btn_w;
        let btn_pad = inner_w.saturating_sub(total_btn_w) / 2;

        lines.push(Line::from(vec![
            Span::styled(" ".repeat(btn_pad), Style::default()),
            Span::styled(
                y_btn,
                Style::default()
                    .fg(rgb(6, 78, 59))
                    .bg(rgb(52, 211, 153))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ".repeat(sep_w), Style::default()),
            Span::styled(
                n_btn,
                Style::default()
                    .fg(rgb(127, 29, 29))
                    .bg(rgb(248, 113, 113))
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        let y_hint = "(Enter/Y)";
        let n_hint = "(Esc/N)";
        let hint_w = y_btn_w + sep_w + n_btn_w;
        let hint_pad = inner_w.saturating_sub(hint_w) / 2;

        lines.push(Line::from(vec![
            Span::styled(" ".repeat(hint_pad), Style::default()),
            Span::styled(
                format!("{:^width$}", y_hint, width = y_btn_w),
                Style::default().fg(rgb(74, 222, 128)),
            ),
            Span::styled(" ".repeat(sep_w), Style::default()),
            Span::styled(
                format!("{:^width$}", n_hint, width = n_btn_w),
                Style::default().fg(rgb(248, 113, 113)),
            ),
        ]));

        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

pub(crate) fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.view.focus == FocusZone::Input;
    let border_color = if focused {
        focus_input()
    } else {
        unfocus_border()
    };

    let mc = app.agent_state.agent.current_model().to_string();
    let (badge_text, badge_fg, _badge_bg) = app.mode_badge();
    let title_text = format!(" {} {} {} ", mc, badge_text, app.think_str());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(title_text)
        .title_style(
            Style::default()
                .fg(if focused { app.model_color() } else { badge_fg })
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default());
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.input.input_buf.is_empty() {
        // 左右各留 1 列空白
        let placeholder_inner = Rect::new(
            inner.x + 1,
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        );
        f.render_widget(
            Paragraph::new(Span::styled(
                ui_label(app, "placeholder"),
                Style::default().fg(text_dim()),
            )),
            placeholder_inner,
        );
        // 仅输入框激活（focused）时显示闪烁光标
        if focused {
            f.set_cursor_position(Position::new(inner.x + 1, inner.y));
        }
    } else {
        // 内容区域左右各留 1 列空白
        let text_inner = Rect::new(
            inner.x + 1,
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        );
        let avail_w = text_inner.width as usize;
        let scroll = app.input.input_scroll_y as usize;
        // 计算输入框文本选择范围（字节）
        let sel_byte_range = app.input.input_select_anchor.map(|anchor| {
            let s = anchor.min(app.input.cursor_pos);
            let e = anchor.max(app.input.cursor_pos);
            (s, e)
        });
        let sel_style = Style::default().fg(Color::White).bg(rgb(30, 60, 100));

        let mut visual_lines: Vec<Line> = Vec::new();
        let mut visual_idx: usize = 0;
        let mut global_byte_pos: usize = 0;

        for line_part in app.input.input_buf.split('\n') {
            if avail_w == 0 {
                if visual_idx >= scroll {
                    visual_lines.push(Line::from(Span::styled(
                        String::new(),
                        Style::default().fg(text_body()),
                    )));
                }
                visual_idx += 1;
                global_byte_pos += line_part.len() + 1;
                continue;
            }
            let mut row = String::new();
            let mut row_width: usize = 0;
            let mut row_byte_start = global_byte_pos;

            for ch in line_part.chars() {
                let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                if row_width + w > avail_w && !row.is_empty() {
                    if visual_idx >= scroll {
                        emit_sel_row(
                            &mut visual_lines,
                            &row,
                            row_byte_start,
                            &sel_byte_range,
                            &sel_style,
                        );
                    }
                    visual_idx += 1;
                    row_byte_start = global_byte_pos;
                    row.clear();
                    row_width = 0;
                }
                row.push(ch);
                row_width += w;
                global_byte_pos += ch.len_utf8();
            }
            if visual_idx >= scroll {
                emit_sel_row(
                    &mut visual_lines,
                    &row,
                    row_byte_start,
                    &sel_byte_range,
                    &sel_style,
                );
            }
            visual_idx += 1;
            global_byte_pos += 1; // 跳过 '\n'
        }
        f.render_widget(
            Paragraph::new(ratatui::text::Text::from(visual_lines)),
            text_inner,
        );
        // 仅输入框激活（focused）时显示闪烁光标
        if focused {
            let (cx, cy) = input_cursor_position(app, text_inner);
            f.set_cursor_position(Position::new(cx, cy));
        }
    }
}

/// 根据选择范围将一行文本拆分为带高亮的多段 Span，追加到 visual_lines。
fn emit_sel_row(
    lines: &mut Vec<Line>,
    row: &str,
    row_start: usize,
    sel_range: &Option<(usize, usize)>,
    sel_style: &Style,
) {
    let row_end = row_start + row.len();
    let spans = if let Some(range) = sel_range {
        let (ss, se) = *range;
        let os = row_start.max(ss);
        let oe = row_end.min(se);
        if os < oe {
            let mut parts: Vec<Span> = Vec::new();
            // 选中前部分
            let before_len = os - row_start;
            if before_len > 0 {
                parts.push(Span::styled(
                    row[..before_len].to_string(),
                    Style::default().fg(text_body()),
                ));
            }
            // 选中部分
            let sel_len = oe - os;
            parts.push(Span::styled(
                row[before_len..before_len + sel_len].to_string(),
                *sel_style,
            ));
            // 选中后部分
            let after_start = before_len + sel_len;
            if after_start < row.len() {
                parts.push(Span::styled(
                    row[after_start..].to_string(),
                    Style::default().fg(text_body()),
                ));
            }
            parts
        } else {
            vec![Span::styled(
                row.to_string(),
                Style::default().fg(text_body()),
            )]
        }
    } else {
        vec![Span::styled(
            row.to_string(),
            Style::default().fg(text_body()),
        )]
    };
    lines.push(Line::from(spans));
}

pub(crate) fn draw_stats_bar(f: &mut Frame, area: Rect, app: &App) {
    let stats_str = stats_title_line(app);
    if !stats_str.is_empty() {
        // 修复(颜色):此前用 Color::Reset 透出终端默认底色,白底终端下显示为白色。
        let bg_color = rgb(12, 14, 22);
        let line = Line::from(Span::styled(
            format!("  {} ", stats_str),
            Style::default().fg(text_dim()).bg(bg_color),
        ));
        f.render_widget(
            Paragraph::new(line)
                .alignment(ratatui::layout::Alignment::Right)
                .style(Style::default().bg(bg_color)),
            area,
        );
    }
}

pub(crate) fn draw_sidebar(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.view.focus == FocusZone::Sidebar;
    let border_color = if focused {
        focus_sidebar()
    } else {
        unfocus_border()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title_style(
            Style::default()
                .fg(if focused {
                    focus_sidebar()
                } else {
                    text_muted()
                })
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    let w = inner.width as usize;

    if !app.view.git_context.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(" ⌥ ", Style::default().fg(text_dim())),
            Span::styled(
                "BRANCH",
                Style::default().fg(text_dim()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", Style::default()),
            Span::styled(
                truncate_str(&app.view.git_context, w.saturating_sub(12)),
                Style::default().fg(text_muted()),
            ),
        ]));
        lines.push(Line::from(""));
    }

    push_section(&mut lines, "TASK LOG", w);
    let max_tasks = 8;
    let total_tasks = app.view.task_list.len();
    let scroll = app
        .view
        .sidebar_scroll
        .min(total_tasks.saturating_sub(max_tasks));
    let start = if total_tasks <= max_tasks {
        0
    } else {
        total_tasks.saturating_sub(max_tasks).saturating_sub(scroll)
    };
    let tasks_slice: Vec<_> = app
        .view
        .task_list
        .iter()
        .skip(start)
        .take(max_tasks)
        .collect();
    let task_count = tasks_slice.len();
    for (i, task) in tasks_slice.into_iter().enumerate() {
        let is_last = i == task_count - 1;
        let branch = if is_last { " └─" } else { " ├─" };
        let (marker, marker_color) = match task.status {
            TaskStatus::Running => (
                SPINNER[(app.view.tick as usize) % SPINNER.len()].to_string(),
                amber(),
            ),
            TaskStatus::Done => ("✓".to_string(), diff_added()),
            TaskStatus::Failed => ("✗".to_string(), ds_red()),
            TaskStatus::WaitingApproval => ("⏳".to_string(), amber()),
        };
        let (glyph, glyph_color) = markdown::tool_glyph(&task.tool);
        let tool_name = truncate_str(&task.tool, 9);
        lines.push(Line::from(vec![
            Span::styled(format!("{} ", branch), Style::default().fg(rgb(50, 60, 85))),
            Span::styled(format!("{:>2} ", marker), Style::default().fg(marker_color)),
            Span::styled(format!("{} ", glyph), Style::default().fg(glyph_color)),
            Span::styled(
                format!("{:<10}", tool_name),
                Style::default().fg(ds_blue()).add_modifier(Modifier::BOLD),
            ),
        ]));
        let detail_w = w.saturating_sub(8);
        let detail_display = truncate_str(&task.detail, detail_w);
        lines.push(Line::from(vec![
            Span::styled(" │  ", Style::default().fg(rgb(35, 45, 65))),
            Span::styled(detail_display, Style::default().fg(text_muted())),
        ]));
    }
    if app.view.task_list.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(" └─ ", Style::default().fg(rgb(50, 60, 85))),
            Span::styled(
                ui_label(app, "idle").to_string(),
                Style::default().fg(text_dim()),
            ),
        ]));
    }
    lines.push(Line::from(""));

    if !app.view.modified_files.is_empty() {
        push_section(&mut lines, "MUTATED", w);
        let len = app.view.modified_files.len();
        for (i, file_path) in app.view.modified_files.iter().enumerate() {
            let is_last = i == len - 1;
            let branch = if is_last { " └─" } else { " ├─" };
            let name = file_path.split('/').next_back().unwrap_or(file_path);
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", branch), Style::default().fg(rgb(50, 60, 85))),
                Span::styled("+ ", Style::default().fg(diff_added())),
                Span::styled(
                    truncate_str(name, w.saturating_sub(9)),
                    Style::default().fg(text_muted()),
                ),
            ]));
        }
        lines.push(Line::from(""));
    }

    if total_tasks > max_tasks {
        let visible_from_scroll = total_tasks.saturating_sub(start);
        let hidden = total_tasks
            .saturating_sub(start)
            .saturating_sub(max_tasks.min(visible_from_scroll));
        if hidden > 0 {
            lines.push(Line::from(Span::styled(
                format!("  ... +{} {}", hidden, app.s.sidebar_more),
                Style::default().fg(text_dim()),
            )));
        }
    }

    if !app.session.turn_stats.is_empty() {
        // 修复：把原本独立的 SESSION 区块合并成 TURNS 顶部的 Σ 汇总行,
        // section 名统一为 TURNS,信息层级更紧凑,避免两段重复强调 token/cost。
        push_section(&mut lines, "TURNS", w);
        let max_turns = 5usize;
        let total_turns = app.session.turn_stats.len();

        // 顶部汇总行:Σ 全部 N turns / total_tokens / total_cost。
        let total_cost_str = if app.session.session_total_cost >= 0.01 {
            format!("¥{:.3}", app.session.session_total_cost)
        } else {
            format!("¥{:.4}", app.session.session_total_cost)
        };
        let total_tok_str = fmt_num(app.session.session_total_tokens);
        // 缓存命中率 = cache_hit / (cache_hit + cache_miss)。
        // 修复：原实现一边用 session_total_cache_hit(全 session 累计)、
        // 一边用 turn_stats.iter().sum()(只算最近 N 条,被截断后窗口会偏小),
        // 一旦 session 超过 MAX_TURN_STATS(100) 命中率就失真。
        // 现在 miss 也走 session 累计字段,两个分母都跟随 session 真实状态。
        let total_prompt =
            app.session.session_total_cache_hit + app.session.session_total_cache_miss;
        let hit_str = if total_prompt > 0 {
            let pct = (app.session.session_total_cache_hit as f64 / total_prompt as f64) * 100.0;
            // 命中率高→绿色,低→红色,中等→琥珀色,一眼看出 prompt cache 工作情况。
            let color = if pct >= 70.0 {
                diff_added()
            } else if pct >= 30.0 {
                amber()
            } else {
                ds_red()
            };
            Some((
                format!(
                    "  {}H {}%",
                    fmt_num(app.session.session_total_cache_hit),
                    pct as u32
                ),
                color,
            ))
        } else {
            None
        };
        let mut summary_spans = vec![
            // 修复:T 是整段 TURNS 的"根"节点,用 `└─` 表示其后是同组兄弟(#N turn 行)。
            // 视觉上 T 与下方 `#N` 形成统一的树形,`#N` 行用 ├─ / └─ 区分中间 / 最末节点。
            Span::styled(
                "  └─ T ",
                Style::default().fg(ds_sky()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{}  ", total_tok_str),
                Style::default().fg(text_muted()),
            ),
            Span::styled(total_cost_str, Style::default().fg(amber())),
        ];
        if let Some((s, c)) = hit_str {
            summary_spans.push(Span::styled(s, Style::default().fg(c)));
        }
        lines.push(Line::from(summary_spans));

        let scroll = app
            .view
            .sidebar_turn_scroll
            .min(total_turns.saturating_sub(max_turns));
        let start_idx = total_turns.saturating_sub(max_turns).saturating_sub(scroll);
        let turns_slice: Vec<_> = app
            .session
            .turn_stats
            .iter()
            .skip(start_idx)
            .take(max_turns)
            .collect();
        let turn_count = turns_slice.len();
        for (i, ts) in turns_slice.into_iter().enumerate() {
            let is_last = i == turn_count - 1;
            let branch = if is_last { " └─" } else { " ├─" };
            let status_mark = match ts.status {
                TurnStatus::Done => "✓",
                TurnStatus::Failed => "✗",
            };
            let status_color = match ts.status {
                TurnStatus::Done => diff_added(),
                TurnStatus::Failed => ds_red(),
            };
            let tokens_str = if ts.stats.total_tokens >= 1_000_000 {
                format!("{:.1}M", ts.stats.total_tokens as f64 / 1_000_000.0)
            } else if ts.stats.total_tokens >= 10_000 {
                format!("{:.1}K", ts.stats.total_tokens as f64 / 1_000.0)
            } else {
                ts.stats.total_tokens.to_string()
            };
            let cost_str = if ts.cost_cny >= 0.01 {
                format!("¥{:.3}", ts.cost_cny)
            } else {
                format!("¥{:.4}", ts.cost_cny)
            };
            // 每轮缓存命中率,与 Σ 行采用同色阶,便于一眼比对。
            let prompt_total = ts.stats.cache_hit_tokens + ts.stats.cache_miss_tokens;
            let (hit_label, hit_color) = if prompt_total > 0 {
                let pct = (ts.stats.cache_hit_tokens as f64 / prompt_total as f64) * 100.0;
                let pct_u = pct as u32;
                let color = if pct >= 70.0 {
                    diff_added()
                } else if pct >= 30.0 {
                    amber()
                } else {
                    ds_red()
                };
                (
                    format!("{}H {}%", fmt_num(ts.stats.cache_hit_tokens), pct_u),
                    color,
                )
            } else {
                ("-".to_string(), text_dim())
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", branch), Style::default().fg(rgb(50, 60, 85))),
                Span::styled(format!("#{} ", ts.turn), Style::default().fg(ds_sky())),
                Span::styled(
                    format!("T{} ", tokens_str),
                    Style::default().fg(text_muted()),
                ),
                Span::styled(format!("{} ", cost_str), Style::default().fg(amber())),
                Span::styled(format!("{} ", hit_label), Style::default().fg(hit_color)),
                Span::styled(
                    status_mark,
                    Style::default()
                        .fg(status_color)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        lines.push(Line::from(""));

        if total_turns > max_turns {
            let hidden = total_turns
                .saturating_sub(start_idx)
                .saturating_sub(max_turns);
            if hidden > 0 {
                lines.push(Line::from(Span::styled(
                    format!("  ... +{} {}", hidden, app.s.sidebar_more),
                    Style::default().fg(text_dim()),
                )));
            }
        }
    }

    // 原本独立的 SESSION 区块已合并到上方 TURNS section 的 Σ 汇总行,这里不再重复展示。

    f.render_widget(
        Paragraph::new(ratatui::text::Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

pub(crate) fn push_section(lines: &mut Vec<Line>, label: &str, width: usize) {
    let dash_count = width.saturating_sub(label.len() + 3);
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            label.to_string(),
            Style::default().fg(text_dim()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {}", "─".repeat(dash_count)),
            Style::default().fg(rgb(40, 50, 75)),
        ),
    ]));
}

pub(crate) fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let max_w = 60u16;
    let max_h = 42u16;
    let help_w = max_w.min(area.width.saturating_sub(4));
    let help_h = max_h.min(area.height.saturating_sub(2));
    let x = area.width.saturating_sub(help_w) / 2;
    let y = area.height.saturating_sub(help_h) / 2;
    let help_area = Rect::new(area.x + x, area.y + y, help_w, help_h);

    f.render_widget(Clear, help_area);
    let zh = matches!(app.lang, Lang::Zh);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(focus_sidebar()))
        .style(Style::default().bg(rgb(12, 16, 28)));
    let inner = block.inner(help_area);
    f.render_widget(block, help_area);

    let mut lines = Vec::new();
    let title_badge = if zh {
        "  键盘快捷键与导航指令  "
    } else {
        "  KEYBOARD SHORTCUTS & COMMANDS  "
    };
    lines.push(Line::from(vec![
        Span::styled("   ▐", Style::default().fg(rgb(30, 41, 59))),
        Span::styled(
            title_badge,
            Style::default()
                .fg(focus_sidebar())
                .bg(rgb(30, 41, 59))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("▌", Style::default().fg(rgb(30, 41, 59))),
    ]));
    lines.push(Line::from(""));

    // 修复(Bug #21):数据驱动的 help 渲染,消除 60 行完全结构相同的 zh/en if/else。
    render_help_sections(&mut lines, zh);
    render_help_entries(&mut lines, zh);

    // "所有命令"最后追加,中英文一致。
    let all_label = if zh { "所有命令" } else { "All Commands" };
    lines.push(help_section_header(all_label));
    let all_cmds = app.s.get_commands();
    for (i, (cmd, desc)) in all_cmds.iter().enumerate() {
        lines.push(help_item_line(cmd, desc, i == all_cmds.len() - 1));
    }

    f.render_widget(Paragraph::new(ratatui::text::Text::from(lines)), inner);
}

fn help_section_header(label: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ✦ ", Style::default().fg(focus_sidebar())),
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(text_body())
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn help_item_line(key: &str, desc: &str, is_last: bool) -> Line<'static> {
    let branch = if is_last {
        "    └─ "
    } else {
        "    ├─ "
    };
    Line::from(vec![
        Span::styled(branch, Style::default().fg(rgb(50, 60, 85))),
        Span::styled(
            format!("{:<20}", key),
            Style::default().fg(ds_blue()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ➔ ", Style::default().fg(rgb(80, 90, 110))),
        Span::styled(desc.to_string(), Style::default().fg(text_muted())),
    ])
}

type HelpSection<'a> = (&'a str, &'a [(&'a str, &'a str)]);

fn render_help_sections(lines: &mut Vec<Line<'static>>, zh: bool) {
    let sections: &[HelpSection] = if zh {
        &[
            (
                "导航",
                &[
                    ("↑↓ / PgUp / PgDn", "滚动对话历史"),
                    ("End / g (空输入)", "直达最新消息"),
                    ("Ctrl+Tab", "循环切换区域焦点"),
                    ("Esc", "关闭帮助弹窗"),
                ],
            ),
            (
                "输入",
                &[
                    ("Enter", "发送消息 / 触发执行"),
                    ("Ctrl+J / Alt+Enter", "换行输入"),
                    ("Ctrl+← / →", "按单词快速移动"),
                    ("Esc", "停止运行/清空输入"),
                ],
            ),
        ]
    } else {
        &[
            (
                "Navigation",
                &[
                    ("Up/Down / PgUp/PgDn", "Scroll chat history"),
                    ("End / g (empty in)", "Jump to latest"),
                    ("Ctrl+Tab", "Cycle focus zones"),
                    ("Esc", "Close this help menu"),
                ],
            ),
            (
                "Input",
                &[
                    ("Enter", "Send message / run tools"),
                    ("Ctrl+J / Alt+Enter", "Insert newline character"),
                    ("Ctrl+Left / Right", "Move cursor by word"),
                    ("Esc", "Stop running / Clear input"),
                ],
            ),
        ]
    };
    for (title, items) in sections {
        lines.push(help_section_header(title));
        for (i, (key, desc)) in items.iter().enumerate() {
            lines.push(help_item_line(key, desc, i == items.len() - 1));
        }
        lines.push(Line::from(""));
    }
}

fn render_help_entries(lines: &mut Vec<Line<'static>>, zh: bool) {
    let title = if zh {
        "指令快捷切换"
    } else {
        "Commands & Session"
    };
    lines.push(help_section_header(title));
    let entries: &[(&str, &str)] = if zh {
        &[
            ("Tab", "循环切换 Plan→Agent→Auto→YOLO"),
            ("Ctrl+T", "切换模型 Pro ↔ Flash"),
            ("/model pro|flash", "切换推理模型"),
            ("Shift+Tab", "切换思考 off→auto→high→max"),
            ("Ctrl+O", "展开或折叠推理步骤"),
            ("/multi on|off|status", "多Agent协作控制"),
            ("/decompose <任务>", "分解任务查看子步骤"),
            ("/skill list|info|use|\u{2026}", "技能管理"),
            ("/cost", "查看本会话累计费用(¥)"),
            ("/pricing", "从 DeepSeek 官方文档同步最新定价"),
            ("/quit", "安全退出终端"),
        ]
    } else {
        &[
            ("/mode plan|agent|yolo", "Switch running mode"),
            ("Tab", "Cycle Plan→Agent→Auto→YOLO"),
            ("Ctrl+T", "Toggle Pro ↔ Flash"),
            ("/model pro|flash", "Switch model"),
            ("Shift+Tab", "Cycle off→auto→high→max"),
            ("/think auto|high|max", "Toggle reasoning effort"),
            ("Ctrl+O", "Expand/Collapse reasoning"),
            ("/multi on|off|status", "Multi-agent collaboration"),
            ("/decompose <task>", "Decompose task into sub-steps"),
            ("/skill list|info|use|...", "Skill management"),
            ("/cost", "Show session cost so far (¥)"),
            ("/pricing", "Sync latest pricing from DeepSeek docs"),
            ("/quit", "Quit terminal"),
        ]
    };
    for (i, (key, desc)) in entries.iter().enumerate() {
        lines.push(help_item_line(key, desc, i == entries.len() - 1));
    }
    lines.push(Line::from(""));
}

/// 修复(Bug #21):合并工具图标和标签查询,避免两张平行 match 表。
fn tool_meta(name: &str, zh: bool) -> (&str, &str) {
    match name {
        "write_file" => ("📝", if zh { "写入文件" } else { "Write File" }),
        "patch_file" => ("🔧", if zh { "修改文件" } else { "Patch File" }),
        "run_command" => ("⚡", if zh { "执行命令" } else { "Run Command" }),
        "read_file" => ("📖", if zh { "读取文件" } else { "Read File" }),
        "grep" | "search_code" => ("🔍", if zh { "搜索内容" } else { "Search Content" }),
        _ => ("⚙️", name),
    }
}

/// 统一计算弹窗 Rect(渲染 & 命中测试共用)
/// 优先上方(左对齐),不够则下方兜底
fn palette_dropdown_rect(area: Rect, input_area: Rect, total: usize) -> Option<Rect> {
    let term_w = area.width;
    let visible = (total.max(1) as u16).min(PALETTE_MAX_VISIBLE as u16);
    let dropdown_h = visible.saturating_add(3).min(area.height); // +3: title + hint + border
    let max_w = term_w.saturating_sub(2).min(PALETTE_MAX_WIDTH);
    let dropdown_w = input_area.width.min(max_w).max(8);
    let above_y = input_area.y.saturating_sub(dropdown_h);
    if above_y >= area.y {
        return Some(Rect::new(input_area.x, above_y, dropdown_w, dropdown_h));
    }
    let below_y = input_area.y.saturating_add(input_area.height);
    if below_y.saturating_add(dropdown_h) <= area.y.saturating_add(area.height) {
        return Some(Rect::new(input_area.x, below_y, dropdown_w, dropdown_h));
    }
    None
}

pub(crate) fn draw_command_suggestions(f: &mut Frame, area: Rect, input_area: Rect, app: &App) {
    let commands = app.filtered_commands();
    let total = commands.len();
    let scroll = app.view.palette_scroll.min(total.saturating_sub(1));
    let visible_end = (scroll + PALETTE_MAX_VISIBLE).min(total);
    let window: Vec<_> = if total == 0 {
        Vec::new()
    } else {
        commands[scroll..visible_end].to_vec()
    };
    let visible_count = if total == 0 { 1 } else { window.len() };
    let Some(dropdown_area) = palette_dropdown_rect(area, input_area, visible_count) else {
        return;
    };

    let palette_focused = app.view.focus == FocusZone::Palette;
    let border_color = if palette_focused {
        focus_palette()
    } else {
        ds_blue()
    };
    let title_text = if palette_focused {
        " COMMAND PALETTE "
    } else {
        " palette "
    };

    f.render_widget(Clear, dropdown_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(title_text)
        .title_style(
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(rgb(12, 20, 38)));
    let inner_full = block.inner(dropdown_area);
    f.render_widget(block, dropdown_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner_full);
    let list_area = chunks[0];
    let hint_area = chunks[1];

    let palette_bg = rgb(12, 20, 38);
    let max_cmd_w = window
        .iter()
        .map(|(cmd, _)| unicode_display_width(cmd))
        .max()
        .unwrap_or(0);
    let inner_w = list_area.width as usize;
    let prefix_w = 3usize;
    let desc_budget = inner_w.saturating_sub(prefix_w + max_cmd_w + 1);

    let mut lines: Vec<Line> = Vec::new();
    if window.is_empty() {
        let t = " unknown command";
        let pad = " ".repeat((list_area.width as usize).saturating_sub(unicode_display_width(t)));
        lines.push(Line::from(vec![
            Span::styled(t, Style::default().fg(text_dim()).bg(palette_bg)),
            Span::styled(pad, Style::default().bg(palette_bg)),
        ]));
    } else {
        for (i, (cmd, desc)) in window.iter().enumerate() {
            let logical = scroll + i;
            let sel = palette_focused && logical == app.view.palette_index;
            let bg = if sel { rgb(30, 50, 80) } else { palette_bg };
            let fg = if sel { text_body() } else { ds_sky() };
            let df = if sel { text_muted() } else { text_dim() };
            let pfx = if sel { " ▸ " } else { "   " };
            let cw = unicode_display_width(cmd);
            let left = format!(
                "{}{}{} ",
                pfx,
                cmd,
                " ".repeat(max_cmd_w.saturating_sub(cw))
            );
            let left_w = unicode_display_width(&left);
            // 描述截断
            let desc_clip = if desc_budget == 0 {
                String::new()
            } else if unicode_display_width(desc) < desc_budget {
                format!("{} ", desc)
            } else {
                let mut out = String::new();
                let mut used = 0usize;
                let b = desc_budget.saturating_sub(1);
                for ch in desc.chars() {
                    let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                    if used + w > b {
                        break;
                    }
                    out.push(ch);
                    used += w;
                }
                out.push('…');
                out
            };
            let dw = unicode_display_width(&desc_clip);
            let final_pad = inner_w.saturating_sub(left_w + dw);
            lines.push(Line::from(vec![
                Span::styled(
                    left,
                    Style::default().fg(fg).add_modifier(Modifier::BOLD).bg(bg),
                ),
                Span::styled(desc_clip, Style::default().fg(df).bg(bg)),
                Span::styled(" ".repeat(final_pad), Style::default().bg(bg)),
            ]));
        }
    }
    f.render_widget(Paragraph::new(ratatui::text::Text::from(lines)), list_area);

    // 底部 hint
    let mut hs: Vec<Span> = Vec::new();
    if total > PALETTE_MAX_VISIBLE {
        let lower = visible_end;
        hs.push(Span::styled(
            format!(" {}/{} ", lower, total),
            Style::default()
                .fg(amber())
                .add_modifier(Modifier::BOLD)
                .bg(palette_bg),
        ));
    }
    let hint = if palette_focused {
        " ↑↓ select  PgUp/PgDn page  Tab complete  Enter run  Esc cancel "
    } else {
        " ↑↓ select  Enter insert "
    };
    hs.push(Span::styled(
        hint,
        Style::default().fg(text_dim()).bg(palette_bg),
    ));
    let hw: usize = hs.iter().map(|s| unicode_display_width(&s.content)).sum();
    let hp = (hint_area.width as usize).saturating_sub(hw);
    if hp > 0 {
        hs.insert(
            0,
            Span::styled(" ".repeat(hp), Style::default().bg(palette_bg)),
        );
    }
    f.render_widget(Paragraph::new(Line::from(hs)), hint_area);
}

pub(crate) fn command_suggestion_area(app: &App, area: Rect, input_area: Rect) -> Option<Rect> {
    let total = app.filtered_commands().len();
    let visible = if total == 0 {
        1
    } else {
        total.min(PALETTE_MAX_VISIBLE)
    };
    palette_dropdown_rect(area, input_area, visible)
}

pub(crate) fn command_at(
    app: &App,
    area: Rect,
    input_area: Rect,
    x: u16,
    y: u16,
) -> Option<String> {
    let da = command_suggestion_area(app, area, input_area)?;
    if !contains(da, x, y) {
        return None;
    }
    let inner = Rect::new(
        da.x + 1,
        da.y + 1,
        da.width.saturating_sub(2),
        da.height.saturating_sub(3),
    );
    if !contains(inner, x, y) {
        return None;
    }
    let cmds = app.filtered_commands();
    if cmds.is_empty() {
        return None;
    }
    let scroll = app.view.palette_scroll.min(cmds.len() - 1);
    let idx = scroll.saturating_add((y.saturating_sub(inner.y)) as usize);
    cmds.get(idx).map(|(c, _)| (*c).to_string())
}

pub(crate) fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && y >= area.y
        && x < area.x.saturating_add(area.width)
        && y < area.y.saturating_add(area.height)
}

/// 从聊天区域提取选中文本
/// 根据选择起止坐标，从缓存的聊天行中提取对应文本
pub(crate) fn extract_selected_text(app: &App) -> Option<String> {
    let (sx, sy) = app.view.select_start?;
    let (ex, ey) = app.view.select_end?;
    let (min_row, max_row) = if sy <= ey { (sy, ey) } else { (ey, sy) };
    let (min_col, max_col) = if sy < ey || (sy == ey && sx <= ex) {
        (sx, ex)
    } else {
        (ex, sx)
    };

    let lines = &app.view.cached_chat_lines;
    if lines.is_empty() {
        return None;
    }

    let total_lines = lines.len();
    let scroll_start = if total_lines > 0 {
        let offset = app.view.scroll_offset.min(total_lines - 1);
        total_lines.saturating_sub(offset + 1)
    } else {
        0
    };

    let mut result = String::new();
    for (i, line) in lines.iter().enumerate() {
        let screen_row = (i + scroll_start) as u16;
        if screen_row < min_row || screen_row > max_row {
            continue;
        }
        let line_text: String = line.spans.iter().map(|s| &*s.content).collect();
        if min_row == max_row {
            let start_col = usize::from(min_col);
            let end_col = usize::from(max_col);
            let (byte_start, byte_end) = col_range_to_byte_range(&line_text, start_col, end_col);
            if byte_start < byte_end {
                result.push_str(&line_text[byte_start..byte_end]);
            }
        } else if screen_row == min_row {
            let start_col = usize::from(min_col);
            let (byte_start, _) =
                col_range_to_byte_range(&line_text, start_col, line_text.len() + 100);
            result.push_str(&line_text[byte_start..]);
            result.push('\n');
        } else if screen_row == max_row {
            let end_col = usize::from(max_col);
            let (_, byte_end) = col_range_to_byte_range(&line_text, 0, end_col);
            result.push_str(&line_text[..byte_end]);
        } else {
            result.push_str(&line_text);
            result.push('\n');
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// 根据显示列范围计算字节范围
pub(crate) fn col_range_to_byte_range(
    text: &str,
    start_col: usize,
    end_col: usize,
) -> (usize, usize) {
    let char_indices: Vec<(usize, char)> = text.char_indices().collect();
    let mut widths = Vec::new();
    let mut acc: usize = 0;
    for &(idx, ch) in &char_indices {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
        widths.push((acc, idx));
        acc += w;
    }
    let byte_start = widths
        .iter()
        .find(|&&(col, _)| col >= start_col)
        .map(|&(_, idx)| idx)
        .unwrap_or(text.len());
    let byte_end = widths
        .iter()
        .find(|&&(col, _)| col >= end_col)
        .map(|&(_, idx)| idx)
        .unwrap_or(text.len());
    (byte_start, byte_end)
}

/// 复制文本到系统剪贴板
pub(crate) fn copy_to_clipboard(text: &str) {
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        let _ = clipboard.set_text(text.to_string());
    }
}

/// 根据鼠标点击位置设置光标在 input_buf 中的字节位置
/// 考虑视觉换行和 Unicode 显示宽度
pub(crate) fn set_cursor_from_mouse(app: &mut App, input_area: Rect, x: u16, y: u16) {
    let inner = Rect::new(
        input_area.x + 1,
        input_area.y + 1,
        input_area.width.saturating_sub(2),
        input_area.height.saturating_sub(2),
    );
    if !contains(inner, x, y) {
        return;
    }

    if app.input.input_buf.is_empty() {
        app.input.cursor_pos = 0;
        return;
    }

    let avail_w = inner.width as usize;
    let target_visual_row = y.saturating_sub(inner.y) as usize;
    let target_col = x.saturating_sub(inner.x) as usize;

    let mut visual_row: usize = 0;
    let mut byte_pos: usize = 0;
    let chars: Vec<char> = app.input.input_buf.chars().collect();

    let mut i = 0;
    while i < chars.len() {
        if visual_row > target_visual_row {
            break;
        }
        if visual_row == target_visual_row {
            let mut col_width: usize = 0;
            while i < chars.len() && chars[i] != '\n' {
                let w = unicode_width::UnicodeWidthChar::width(chars[i]).unwrap_or(1);
                // 折行边界或到达点击列时停止
                if col_width + w > avail_w || col_width + w > target_col + 1 {
                    break;
                }
                col_width += w;
                byte_pos += chars[i].len_utf8();
                i += 1;
            }
            app.input.cursor_pos = byte_pos.min(app.input.input_buf.len());
            // 同步输入框滚动偏移，确保光标可见
            if inner.height > 0 {
                app.clamp_input_scroll(inner.height, avail_w);
            }
            return;
        }
        // 跳过当前视觉行
        if chars[i] == '\n' {
            visual_row += 1;
            byte_pos += 1;
            i += 1;
        } else {
            let mut row_width: usize = 0;
            while i < chars.len() && chars[i] != '\n' {
                let cw = unicode_width::UnicodeWidthChar::width(chars[i]).unwrap_or(1);
                if row_width > 0 && row_width + cw > avail_w {
                    // 自动换行：行首字符进入下一视觉行
                    visual_row += 1;
                    if visual_row == target_visual_row {
                        // 当前字符是下一折行的第一个字符，光标置于行首
                        app.input.cursor_pos = byte_pos.min(app.input.input_buf.len());
                        if inner.height > 0 {
                            app.clamp_input_scroll(inner.height, avail_w);
                        }
                        return;
                    }
                    row_width = 0;
                }
                row_width += cw;
                byte_pos += chars[i].len_utf8();
                i += 1;
            }
            visual_row += 1;
        }
    }
    app.input.cursor_pos = byte_pos.min(app.input.input_buf.len());
    if inner.height > 0 {
        app.clamp_input_scroll(inner.height, avail_w);
    }
}

/// 根据光标在 input_buf 中的字节位置，计算其在 inner 区域内的屏幕坐标
/// 考虑 Unicode 字符显示宽度（如中文占2列）和视觉换行
pub(crate) fn input_cursor_position(app: &App, inner: Rect) -> (u16, u16) {
    let pos = app.input.cursor_pos.min(app.input.input_buf.len());
    let before = &app.input.input_buf[..pos];
    let avail_w = inner.width as usize;
    if avail_w == 0 {
        return (inner.x, inner.y);
    }

    let mut visual_line: u16 = 0;
    let mut visual_col: usize = 0;

    for ch in before.chars() {
        if ch == '\n' {
            visual_line += 1;
            visual_col = 0;
        } else {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
            if visual_col + w > avail_w {
                visual_line += 1;
                visual_col = 0;
            }
            visual_col += w;
        }
    }

    let visible_y = visual_line.saturating_sub(app.input.input_scroll_y);
    let y = inner.y + visible_y.min(inner.height.saturating_sub(1));
    let x = (inner.x + visual_col as u16).min(inner.x + inner.width.saturating_sub(1));
    (x, y)
}
