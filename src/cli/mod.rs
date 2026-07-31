use std::io;
use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind, PopKeyboardEnhancementFlags,
};
#[cfg(not(target_os = "windows"))]
use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::execute;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// 确保终端在函数退出时（正常返回或 panic）都能恢复到原始状态。
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), SetCursorStyle::DefaultUserShape);
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        let _ = execute!(io::stdout(), DisableMouseCapture);
        ratatui::restore();
    }
}
use tokio::sync::mpsc;

use crate::agent::MovixAgent;
use crate::cli::layout::root_rows;
use crate::cli::text::{
    fmt_num, next_char_boundary, next_word_boundary, prev_char_boundary, prev_word_boundary,
};
use crate::common::config::MovixConfig;
use crate::common::deepseek::TokenStats;
use crate::common::error::{MovixError, Result};
use crate::common::i18n::Lang;
use crate::common::skill_executor;

// P5+:TUI 子模块(本文件后续会被拆空,只保留 mod 声明 + 公共 re-export)。
pub mod app;
pub mod layout;
pub mod render;
pub mod text;
pub mod theme;
pub mod types;

// 在本文件 scope 中重新暴露 `App`,避免子模块迁移造成大面积改签名。
pub use self::app::App;

use self::render::{
    command_at, command_suggestion_area, contains, copy_to_clipboard, draw_ui,
    extract_selected_text, input_height, set_cursor_from_mouse,
};

const MAX_HISTORY: usize = 500;
const MAX_TASK_LIST: usize = 100;
const MAX_TURN_STATS: usize = 100;
const PALETTE_MAX_VISIBLE: usize = 6;
const PALETTE_MAX_WIDTH: u16 = 60;
const PALETTE_PAGE_SIZE: usize = PALETTE_MAX_VISIBLE;
// 修复：原值 36/30 偏窄,Σ 汇总行 + 5 列 turn 详情会折行。
// 调到 48/42,让 `15H 71%` 这类长 span 一行展示完,摘要预览也能给到 70+ 字符。
const SIDEBAR_WIDE: u16 = 48;
const SIDEBAR_MEDIUM: u16 = 42;
const SIDEBAR_WIDE_THRESHOLD: u16 = 108;
const SIDEBAR_MEDIUM_THRESHOLD: u16 = 86;
const MIN_TERM_WIDTH: u16 = 32;
const MIN_TERM_HEIGHT: u16 = 10;
const SPINNER: [char; 4] = ['⠋', '⠙', '⠹', '⠸'];

/// 无需额外参数的命令集合——弹窗选中后 Enter 应立即执行
fn should_auto_execute(cmd: &str) -> bool {
    matches!(
        cmd,
        "/help"
            | "/?"
            | "/quit"
            | "/clear"
            | "/stats"
            | "/tier"
            | "/budget"
            | "/tools"
            | "/lsp"
            | "/failures"
            | "/cost"
            | "/pricing"
            | "/model pro"
            | "/model flash"
            | "/skill"
            | "/memory"
            | "/snapshot"
            | "/mcp"
            | "/save"
            | "/restore"
            | "/index"
            | "/verify"
            | "/compress"
            | "/changes"
            | "/rollback"
    )
}

/// 有子命令的 base 命令——补全时补空格
fn has_subcommands(cmd: &str) -> bool {
    matches!(
        cmd,
        "/model"
            | "/memory"
            | "/snapshot"
            | "/skill"
            | "/mcp"
            | "/analyze"
            | "/decompose"
            | "/review"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusZone {
    Chat,
    Input,
    Sidebar,
    Palette,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskStatus {
    Running,
    Done,
    Failed,
    WaitingApproval,
}

// P5+:MessageRole / ChatMessage 已抽到 cli/types.rs。
use self::types::{ChatMessage, MessageRole};

pub(crate) struct ToolActionInfo {
    tool: String,
    detail: String,
    status: TaskStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnStatus {
    Done,
    Failed,
}

pub(crate) struct TurnStats {
    turn: u32,
    summary: String,
    stats: TokenStats,
    cost_cny: f64,
    iterations: u32,
    tool_calls: u32,
    status: TurnStatus,
}

// P5+:`App` 状态机已抽到 `cli::app`,见 src/cli/app.rs。
// 渲染函数保留在 `cli/mod.rs`(本文件),将随 P5+ 后续步骤继续拆到 `cli::render` / `cli::events`。

pub async fn run_interactive(config: MovixConfig) -> Result<()> {
    if !io::stdout().is_terminal() {
        return Err(MovixError::Other(
            "当前环境不是交互式终端，无法启动 TUI。请在真实 Terminal/iTerm 中运行，或使用 `movix info`、`movix tools`、`movix -t \"任务\"`。".into(),
        ));
    }

    // 修复(错误):在进入 raw mode 前注册 panic hook 作为 TerminalGuard 的
    // 第二道防线。TerminalGuard::Drop 在双重 panic 或 spawn task panic 时
    // 可能不会执行;hook 至少恢复终端并禁用 mouse capture。
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        let _ = execute!(io::stdout(), SetCursorStyle::DefaultUserShape);
        ratatui::restore();
        prev_hook(info);
    }));

    let mut terminal = ratatui::init();
    execute!(io::stdout(), EnableMouseCapture)?;
    // 在 Windows 上不推任何键盘增强标志,让 crossterm 使用原生 Console API 读取输入,
    // 以确保 Ctrl 修饰符能被正确检测(Ctrl+O 等快捷键才能正常工作)。
    #[cfg(not(target_os = "windows"))]
    let _ = execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );
    // 设置细竖线闪烁光标(比默认方块光标更细,适合编辑)
    let _ = execute!(io::stdout(), SetCursorStyle::BlinkingBar);
    let _guard = TerminalGuard;

    let lang = match config.language.as_str() {
        "en" => Lang::En,
        _ => Lang::Zh,
    };
    let mut app = App::new(MovixAgent::new(config.clone())?, lang);
    app.refresh_token_counts();

    // 先绘制第一帧,让用户立刻看到 UI 框架,避免 git/MCP/会话恢复等异步初始化
    // 期间屏幕空白(git status 在大仓库、MCP 服务器连接、collab_watcher 遍历工作区
    // 都可能耗时)。这些初始化完成后主循环会自动刷新。
    terminal.draw(|f| draw_ui(f, &mut app))?;

    // 修复(启动慢):快照仓库的工作区体积检查(walk_dir_size 递归遍历整个
    // 工作区,大仓库耗时数秒)已在 MovixAgent 构造时被跳过,这里在首帧绘制
    // 之后补齐,避免阻塞首屏显示。
    app.agent_state.agent.init_snapshot_repo();

    app.refresh_git_context().await;

    if let Err(e) = app.agent_state.agent.init_mcp().await {
        tracing::warn!("MCP 初始化失败: {}", e);
    }

    // 自动恢复上次会话（可由 MOVIX_AUTO_RESTORE=0 关闭）。
    if config.auto_restore {
        match app.agent_state.agent.restore_session() {
            Ok(true) => tracing::info!(target: "session", "已自动恢复上次会话"),
            Ok(false) => {} // 无可恢复的会话
            Err(e) => tracing::warn!(target: "session", "自动恢复会话失败: {}", e),
        }
    }

    // 修复(R5,关键):原为 unbounded_channel,生产者(spawn task 的回调)速度受 LLM 流速
    // 限制,消费者(TUI 主循环 try_recv + draw)受渲染速度限制。长 reasoning 流式回复下,
    // 回调以每秒数千 token 速度 send,而 TUI 渲染慢时更新在 channel 内无限堆积 → OOM。
    // 改为有界 channel(capacity 1024)。回调是同步闭包无法 .await,改用 try_send:
    // 满了直接丢弃该更新(背压降级)。对 Content/Thinking 增量,丢一两个中间 token 不影响
    // 最终展示(下一帧 try_recv 会拿到后续);Done/Error 每任务仅一条,1024 容量绝不会满。
    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamUpdate>(1024);

    let mut last_tick = Instant::now();
    let mut last_git_refresh = Instant::now();
    let mut draw_needed = true;

    loop {
        while let Ok(update) = stream_rx.try_recv() {
            match update {
                StreamUpdate::Content(text) => {
                    if let Some(last) = app.view.messages.last_mut() {
                        if last.role == MessageRole::Assistant {
                            last.content.push_str(&text);
                        } else {
                            app.view.messages.push(ChatMessage {
                                role: MessageRole::Assistant,
                                content: text,
                            });
                        }
                    } else {
                        app.view.messages.push(ChatMessage {
                            role: MessageRole::Assistant,
                            content: text,
                        });
                    }
                    app.view.chat_cache_valid = false;
                    if app.view.auto_scroll {
                        app.view.scroll_offset = 0;
                    }
                    draw_needed = true;
                }
                StreamUpdate::Thinking(text) => {
                    if let Some(last) = app.view.thinking_lines.last_mut() {
                        last.push_str(&text);
                    } else {
                        app.view.thinking_lines.push(text);
                    }
                    draw_needed = true;
                }
                StreamUpdate::ToolAction {
                    tool,
                    detail,
                    file,
                    output,
                } => {
                    if detail.starts_with("[needs-approval]") {
                        app.agent_state.pending_approval = Some((tool.clone(), detail.clone()));
                    }
                    if let Some(ref f) = file
                        && tool == "write_file"
                        && !app.view.modified_files.contains(f)
                    {
                        app.view.modified_files.push(f.clone());
                    }
                    if let Some(last) = app.view.task_list.last_mut()
                        && (last.status == TaskStatus::Running
                            || (last.status == TaskStatus::WaitingApproval
                                && !detail.starts_with("[needs-approval]")))
                    {
                        last.status = TaskStatus::Done;
                    }
                    let status = if detail.starts_with("[needs-approval]") {
                        TaskStatus::WaitingApproval
                    } else {
                        TaskStatus::Running
                    };
                    let detail_clone = detail.clone();
                    let tool_clone = tool.clone();
                    app.view.task_list.push(ToolActionInfo {
                        tool,
                        detail,
                        status,
                    });
                    if app.view.task_list.len() > MAX_TASK_LIST {
                        app.view.task_list.remove(0);
                    }
                    let msg_content = if output.is_empty() {
                        format!("{}: {}", tool_clone, detail_clone)
                    } else {
                        format!("{}: {}\n{}", tool_clone, detail_clone, output)
                    };
                    app.view.messages.push(ChatMessage {
                        role: MessageRole::ToolResult,
                        content: msg_content,
                    });
                    app.view.chat_cache_valid = false;
                    draw_needed = true;
                }
                StreamUpdate::Done => {
                    app.finish_stream(false, None);
                    app.recover_agent().await;
                    app.load_stats(TurnStatus::Done).await;
                    app.refresh_git_context().await;
                    draw_needed = true;
                }
                StreamUpdate::Error(e) => {
                    app.finish_stream(true, Some(e));
                    app.recover_agent().await;
                    app.load_stats(TurnStatus::Failed).await;
                    draw_needed = true;
                }
                StreamUpdate::Context { used, max } => {
                    app.view.context_used = used;
                    app.view.context_max = max;
                    draw_needed = true;
                }
                StreamUpdate::Stats { .. } => {
                    // 修复(Bug #2):stats 走 shared_stats_handle 共享内存,
                    // 这条 arm 当前不发送(预留扩展);UI tick 自己读 handle。
                }
            }
        }

        let tick_rate = if app.view.streaming {
            std::time::Duration::from_millis(50)
        } else {
            std::time::Duration::from_millis(250)
        };

        if last_tick.elapsed() >= tick_rate {
            app.view.tick = app.view.tick.wrapping_add(1);
            if app.view.streaming {
                // 修复(Bug #2):优先从共享 stats handle 读;swap 期间 app.agent 是占位。
                let stats = app
                    .agent_state
                    .shared_stats_handle
                    .as_ref()
                    .and_then(|h| h.lock().ok().map(|s| s.clone()))
                    .unwrap_or_else(|| app.agent_state.agent.cached_token_stats().clone());
                if stats.total_tokens > 0 {
                    app.session.last_stats =
                        Some(delta_stats(&stats, &app.session.turn_start_stats));
                }
                app.session.last_elapsed = app
                    .session
                    .turn_started_at
                    .map(|t| t.elapsed().as_secs_f64())
                    .unwrap_or_else(|| app.agent_state.agent.elapsed().as_secs_f64());
                app.session.last_iterations = app.agent_state.agent.iteration_count();
            }
            last_tick = Instant::now();
            draw_needed = true;
        }

        if !app.view.streaming && last_git_refresh.elapsed() >= std::time::Duration::from_secs(180)
        {
            last_git_refresh = Instant::now();
            app.refresh_git_context().await;
            draw_needed = true;
        }

        if draw_needed {
            let input_area_h = input_height(&app).saturating_sub(2);
            let term_w = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
            let input_inner_w = term_w.saturating_sub(2) as usize;
            app.clamp_input_scroll(input_area_h, input_inner_w);
            terminal.draw(|f| draw_ui(f, &mut app))?;
            draw_needed = false;
        }

        let remaining_time = tick_rate.saturating_sub(last_tick.elapsed());
        if !event::poll(remaining_time)? {
            continue;
        }

        let ev = event::read()?;

        if app.view.help_open {
            if let Event::Key(key) = ev
                && key.kind == KeyEventKind::Press
            {
                app.view.help_open = false;
                draw_needed = true;
            } else {
                continue;
            }
        }

        match ev {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                match key.code {
                    KeyCode::Char('c') if ctrl => {
                        if let Some(anchor) = app.input.input_select_anchor {
                            let start = anchor.min(app.input.cursor_pos);
                            let end = anchor.max(app.input.cursor_pos);
                            if start < end && end <= app.input.input_buf.len() {
                                let text = &app.input.input_buf[start..end];
                                copy_to_clipboard(text);
                            }
                            app.input.input_select_anchor = None;
                            draw_needed = true;
                        } else if app.view.select_start.is_some() && app.view.select_end.is_some() {
                            if let Some(text) = extract_selected_text(&app) {
                                copy_to_clipboard(&text);
                                app.view.select_start = None;
                                app.view.select_end = None;
                                draw_needed = true;
                            }
                        } else if let Some(handle) = app.agent_state.agent_handle.take() {
                            // 修复(Bug #4):先 set cancel_flag,让 LLM stream 立即中止;
                            // 再发 cancel_tx 让 spawn task 的 select! arm 触发收尾。
                            app.agent_state.agent_handle = Some(handle);
                            app.stop_running().await;
                            draw_needed = true;
                        } else if app.view.streaming {
                            // 修复(Bug #3):同步命令(/decompose, /save, /compress 等)
                            // 在主线程 await,没装 agent_handle。原实现走 else 分支
                            // `return Ok(())`,把 movix 直接退掉。改为发出取消信号但
                            // 不退出,让用户看到提示后等命令自然返回。
                            app.stop_running().await;
                            draw_needed = true;
                        } else {
                            // _guard 的 Drop 会自动恢复终端
                            app.agent_state.agent.shutdown().await;
                            return Ok(());
                        }
                    }
                    // 修复(Low):raw mode 下 Ctrl+Z 不再产生 SIGTSTP,而是变成
                    // Char('z') + CONTROL 事件,原实现落入 _ => {} 被忽略,用户无法
                    // 挂起 Movix(易误以为程序卡死)。这里明确提示挂起不被支持。
                    KeyCode::Char('z') if ctrl => {
                        app.push_msg(
                            MessageRole::Assistant,
                            if matches!(app.lang, Lang::Zh) {
                                "Ctrl+Z 挂起在 TUI 模式下不可用。用 Ctrl+C 中断任务,或退出后用 shell 的 Ctrl+Z。".to_string()
                            } else {
                                "Ctrl+Z suspend is not supported in TUI mode. Use Ctrl+C to interrupt, or exit first.".to_string()
                            },
                        );
                        draw_needed = true;
                    }
                    KeyCode::Enter if app.agent_state.pending_approval.is_some() => {
                        if let Some(ref tx) = app.agent_state.approval_tx {
                            let _ = tx.try_send(crate::agent::modes::ApprovalDecision::Approved);
                        }
                        app.agent_state.pending_approval = None;
                        draw_needed = true;
                    }
                    KeyCode::Char('y') | KeyCode::Char('Y')
                        if app.agent_state.pending_approval.is_some() =>
                    {
                        if let Some(ref tx) = app.agent_state.approval_tx {
                            let _ = tx.try_send(crate::agent::modes::ApprovalDecision::Approved);
                        }
                        app.agent_state.pending_approval = None;
                        draw_needed = true;
                    }
                    KeyCode::Char('n') | KeyCode::Char('N')
                        if app.agent_state.pending_approval.is_some() =>
                    {
                        if let Some(ref tx) = app.agent_state.approval_tx {
                            let _ = tx.try_send(crate::agent::modes::ApprovalDecision::Denied);
                        }
                        app.agent_state.pending_approval = None;
                        draw_needed = true;
                    }
                    KeyCode::Char('o') if ctrl => {
                        app.view.thinking_expanded = !app.view.thinking_expanded;
                        draw_needed = true;
                    }
                    KeyCode::Char('m') if ctrl => {
                        app.toggle_model();
                        draw_needed = true;
                    }
                    KeyCode::Enter if ctrl => {
                        app.toggle_model();
                        draw_needed = true;
                    }
                    KeyCode::Char('j') if ctrl => {
                        app.input.input_buf.insert(app.input.cursor_pos, '\n');
                        app.input.cursor_pos += 1;
                        draw_needed = true;
                    }
                    KeyCode::Char('a') if ctrl => {
                        let before = &app.input.input_buf[..app.input.cursor_pos];
                        let line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
                        app.input.cursor_pos = line_start;
                        draw_needed = true;
                    }
                    KeyCode::Char('e') if ctrl => {
                        let after = &app.input.input_buf[app.input.cursor_pos..];
                        let line_end = after
                            .find('\n')
                            .map(|p| app.input.cursor_pos + p)
                            .unwrap_or(app.input.input_buf.len());
                        app.input.cursor_pos = line_end;
                        draw_needed = true;
                    }
                    KeyCode::Char('w') if ctrl && app.input.cursor_pos > 0 => {
                        let new_pos =
                            prev_word_boundary(&app.input.input_buf, app.input.cursor_pos);
                        app.input.input_buf.drain(new_pos..app.input.cursor_pos);
                        app.input.cursor_pos = new_pos;
                        app.input.history_cursor = None;
                        if app.input.input_buf.starts_with('/') {
                            app.clamp_palette_index(PALETTE_MAX_VISIBLE);
                        } else {
                            app.view.focus = FocusZone::Input;
                        }
                        draw_needed = true;
                    }
                    KeyCode::Char('u') if ctrl && app.input.cursor_pos > 0 => {
                        let before = &app.input.input_buf[..app.input.cursor_pos];
                        let line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
                        app.input.input_buf.drain(line_start..app.input.cursor_pos);
                        app.input.cursor_pos = line_start;
                        app.input.history_cursor = None;
                        if app.input.input_buf.starts_with('/') {
                            app.clamp_palette_index(PALETTE_MAX_VISIBLE);
                        } else {
                            app.view.focus = FocusZone::Input;
                        }
                        draw_needed = true;
                    }
                    KeyCode::Char('k')
                        if ctrl && app.input.cursor_pos < app.input.input_buf.len() =>
                    {
                        let after = &app.input.input_buf[app.input.cursor_pos..];
                        let line_end = after
                            .find('\n')
                            .map(|p| app.input.cursor_pos + p)
                            .unwrap_or(app.input.input_buf.len());
                        app.input.input_buf.drain(app.input.cursor_pos..line_end);
                        app.input.history_cursor = None;
                        if app.input.input_buf.starts_with('/') {
                            app.clamp_palette_index(PALETTE_MAX_VISIBLE);
                        } else {
                            app.view.focus = FocusZone::Input;
                        }
                        draw_needed = true;
                    }
                    KeyCode::Char('?') if !ctrl => {
                        app.view.help_open = true;
                        draw_needed = true;
                    }
                    KeyCode::Char(c) if !ctrl && !alt => {
                        app.input.input_buf.insert(app.input.cursor_pos, c);
                        app.input.cursor_pos += c.len_utf8();
                        app.input.history_cursor = None;
                        app.input.draft_before_history = None;
                        app.input.input_select_anchor = None;
                        if app.input.input_buf.starts_with('/') {
                            app.view.focus = FocusZone::Palette;
                            app.clamp_palette_index(PALETTE_MAX_VISIBLE);
                        } else {
                            app.view.focus = FocusZone::Input;
                        }
                        draw_needed = true;
                    }
                    KeyCode::Enter => {
                        if app.view.focus == FocusZone::Palette
                            && app.input.input_buf.starts_with('/')
                        {
                            if let Some(cmd) = app.palette_selected_cmd() {
                                if should_auto_execute(cmd) {
                                    app.input.input_buf = cmd.to_string();
                                    app.input.cursor_pos = app.input.input_buf.len();
                                    app.view.focus = FocusZone::Input;
                                    app.reset_palette();
                                    // fall through 到下方主 Enter 逻辑
                                } else {
                                    let mut buf = cmd.to_string();
                                    if has_subcommands(cmd) {
                                        buf.push(' ');
                                    }
                                    app.input.input_buf = buf;
                                    app.input.cursor_pos = app.input.input_buf.len();
                                    app.view.focus = FocusZone::Input;
                                    app.reset_palette();
                                    draw_needed = true;
                                    continue;
                                }
                            } else {
                                draw_needed = true;
                                continue;
                            }
                        }
                        if alt {
                            app.input.input_buf.insert(app.input.cursor_pos, '\n');
                            app.input.cursor_pos += 1;
                            draw_needed = true;
                            continue;
                        }
                        if app.view.streaming {
                            continue;
                        }

                        let input = app.input.input_buf.trim().to_string();
                        let raw_input = app.input.input_buf.clone();
                        app.push_history(&raw_input);
                        app.input.input_buf.clear();
                        app.input.cursor_pos = 0;
                        app.input.input_scroll_y = 0;
                        app.input.history_cursor = None;
                        app.input.draft_before_history = None;

                        if input.is_empty() {
                            continue;
                        }

                        if input.starts_with('/') {
                            let lower = input.to_lowercase().trim().to_string();

                            let is_agent_cmd = lower == "/model" || lower.starts_with("/model ");
                            if is_agent_cmd && app.view.streaming {
                                app.push_msg(MessageRole::Assistant, app.s.agent_busy.to_string());
                                draw_needed = true;
                                continue;
                            }
                            if lower == "/help" || lower == "/?" {
                                app.view.help_open = true;
                            } else if lower == "/model" || lower.starts_with("/model pro") {
                                if lower == "/model" {
                                    let model = app.agent_state.agent.current_model().to_string();
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        format!("{}: {}", app.s.model_prefix, model),
                                    );
                                } else {
                                    if let Err(e) =
                                        app.agent_state.agent.switch_model("deepseek-v4-pro")
                                    {
                                        tracing::warn!("model switch failed: {}", e);
                                    }
                                }
                            } else if lower.starts_with("/model flash") {
                                if let Err(e) =
                                    app.agent_state.agent.switch_model("deepseek-v4-flash")
                                {
                                    tracing::warn!("model switch failed: {}", e);
                                }
                            } else if lower.starts_with("/model ") {
                                let model_name = &input[7..];
                                app.push_msg(
                                    MessageRole::Assistant,
                                    format!("Unknown model: {}", model_name),
                                );
                            } else if lower == "/clear" {
                                // 清除 Agent 内部对话历史
                                app.agent_state.agent.clear_conversation();
                                // 清除 TUI 状态
                                app.view.messages.clear();
                                app.view.chat_cache_valid = false;
                                app.view.thinking_lines.clear();
                                app.session.last_stats = None;
                                app.view.task_list.clear();
                                app.view.modified_files.clear();
                                app.session.session_turns = 0;
                                app.session.session_total_tokens = 0;
                                app.session.session_total_cache_hit = 0;
                                app.session.session_total_cache_miss = 0;
                                app.session.session_total_cost = 0.0;
                                app.session.turn_stats.clear();
                                app.session.turn_start_stats = TokenStats::default();
                                app.session.turn_started_at = None;
                                app.session.current_turn_summary.clear();
                                app.view.scroll_offset = 0;
                                app.view.auto_scroll = true;
                                draw_needed = true;
                                continue;
                            } else if lower == "/quit" {
                                // _guard 的 Drop 会自动恢复终端
                                return Ok(());
                            } else if lower == "/stats" {
                                app.push_msg(
                                    MessageRole::Assistant,
                                    format_turn_stats_report(&app),
                                );
                            } else if lower == "/tier" {
                                if app.agent_state.agent.is_tiered_context_enabled() {
                                    app.agent_state.agent.disable_tiered_context();
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        app.s.tier_disabled.to_string(),
                                    );
                                } else {
                                    app.agent_state.agent.enable_tiered_context();
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        app.s.tier_enabled.to_string(),
                                    );
                                }
                                if let Some(stats) =
                                    app.agent_state.agent.get_tiered_context_stats()
                                {
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        format!(
                                            "{}: Critical={}, Important={}, Normal={}, Summary={}",
                                            app.s.tier_stats,
                                            stats[0],
                                            stats[1],
                                            stats[2],
                                            stats[3]
                                        ),
                                    );
                                }
                            } else if lower.starts_with("/analyze ") {
                                let zh = matches!(app.lang, Lang::Zh);
                                let task = &input[9..];
                                let analysis = app.agent_state.agent.analyze_task(
                                    task,
                                    app.agent_state.agent.context_message_count(),
                                );
                                app.push_msg(
                                    MessageRole::Assistant,
                                    format!(
                                        "{}: {}={:?}, {}={}, {}={}, {}={}, {}={:.2}",
                                        app.s.task_analysis,
                                        if zh { "复杂度" } else { "complexity" },
                                        analysis.complexity,
                                        if zh { "编码" } else { "coding" },
                                        analysis.is_coding_task,
                                        if zh { "长上下文" } else { "long_ctx" },
                                        analysis.is_long_context,
                                        if zh { "多步骤" } else { "multi_step" },
                                        analysis.is_multi_step,
                                        if zh { "置信度" } else { "confidence" },
                                        analysis.confidence
                                    ),
                                );
                                app.push_msg(
                                    MessageRole::Assistant,
                                    format!(
                                        "{}: {}",
                                        app.s.recommended_model,
                                        analysis.recommended_model().name()
                                    ),
                                );
                                app.push_msg(
                                    MessageRole::Assistant,
                                    format!("{}: {:?}", app.s.keywords, analysis.keywords),
                                );
                            } else if lower == "/budget" {
                                let (total, remaining) =
                                    app.agent_state.agent.get_reasoning_budget_info();
                                let summary = app.agent_state.agent.reasoning_summary();
                                app.push_msg(
                                    MessageRole::Assistant,
                                    format!(
                                        "{}: {}/{} tokens",
                                        app.s.reasoning_budget,
                                        total.saturating_sub(remaining),
                                        total
                                    ),
                                );
                                app.push_msg(MessageRole::Assistant, format!("{}", summary));
                            } else if lower == "/tools" {
                                app.push_msg(
                                    MessageRole::Assistant,
                                    format!(
                                        "{}: {}",
                                        app.s.tool_routing_stats,
                                        app.agent_state.agent.tool_count()
                                    ),
                                );
                            } else if lower == "/memory" || lower == "/memory show" {
                                if app.agent_state.agent.is_memory_enabled() {
                                    let content = app.agent_state.agent.memory_show();
                                    if content.is_empty() {
                                        app.push_msg(
                                            MessageRole::Assistant,
                                            app.s.memory_empty.to_string(),
                                        );
                                    } else {
                                        app.push_msg(
                                            MessageRole::Assistant,
                                            format!("{}\n{}", app.s.memory_content_prefix, content),
                                        );
                                    }
                                } else {
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        app.s.memory_disabled.to_string(),
                                    );
                                }
                            } else if lower == "/memory on" {
                                app.agent_state.agent.enable_memory(true);
                                app.push_msg(
                                    MessageRole::Assistant,
                                    app.s.memory_enabled.to_string(),
                                );
                            } else if lower == "/memory off" {
                                app.agent_state.agent.enable_memory(false);
                                app.push_msg(
                                    MessageRole::Assistant,
                                    app.s.memory_disabled.to_string(),
                                );
                            } else if lower.starts_with("/memory add ") {
                                let text = &input[12..];
                                match app.agent_state.agent.memory_append(text) {
                                    Ok(()) => app.push_msg(
                                        MessageRole::Assistant,
                                        app.s.memory_added.to_string(),
                                    ),
                                    Err(e) => app.push_msg(
                                        MessageRole::Assistant,
                                        format!("{}: {}", app.s.memory_add_failed, e),
                                    ),
                                }
                            } else if lower == "/memory clear" {
                                match app.agent_state.agent.memory_clear() {
                                    Ok(()) => app.push_msg(
                                        MessageRole::Assistant,
                                        app.s.memory_cleared.to_string(),
                                    ),
                                    Err(e) => app.push_msg(
                                        MessageRole::Assistant,
                                        format!("{}: {}", app.s.memory_clear_failed, e),
                                    ),
                                }
                            } else if lower == "/memory auto on" {
                                app.agent_state.agent.set_memory_auto_extract(true);
                                app.push_msg(
                                    MessageRole::Assistant,
                                    app.s.memory_auto_on.to_string(),
                                );
                            } else if lower == "/memory auto off" {
                                app.agent_state.agent.set_memory_auto_extract(false);
                                app.push_msg(
                                    MessageRole::Assistant,
                                    app.s.memory_auto_off.to_string(),
                                );
                            } else if lower == "/memory auto" || lower.starts_with("/memory auto ")
                            {
                                app.push_msg(
                                    MessageRole::Assistant,
                                    app.s.memory_auto_invalid.to_string(),
                                );
                            // 修复(审查):restore/list 分支此前排在通用的 "/snapshot " 前缀分支
                            // 之后,被前缀分支吞掉变成"创建快照"(/snapshot restore abc 会新建一个
                            // label="restore abc" 的快照)。必须先检查更具体的子命令。
                            } else if lower.starts_with("/snapshot restore ") {
                                let id = &input[18..];
                                match app.agent_state.agent.restore_snapshot(id) {
                                    Ok(()) => app.push_msg(
                                        MessageRole::Assistant,
                                        format!("{}: {}", app.s.snapshot_restored, id),
                                    ),
                                    Err(e) => app.push_msg(
                                        MessageRole::Assistant,
                                        format!("{}: {}", app.s.snapshot_restore_failed, e),
                                    ),
                                }
                            } else if lower == "/snapshot list" {
                                let snapshots = app.agent_state.agent.list_snapshots();
                                if snapshots.is_empty() {
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        app.s.no_snapshots.to_string(),
                                    );
                                } else {
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        format!("{}:", app.s.snapshots_label),
                                    );
                                    for (id, label, ts) in &snapshots {
                                        let time = chrono::DateTime::from_timestamp(*ts, 0)
                                            .map(|t| t.format("%m-%d %H:%M").to_string())
                                            .unwrap_or_else(|| ts.to_string());
                                        app.push_msg(
                                            MessageRole::Assistant,
                                            format!("  {} [{}] {}", id, label, time),
                                        );
                                    }
                                }
                            } else if lower == "/snapshot" || lower.starts_with("/snapshot ") {
                                // 通用创建快照分支(restore/list 已在上面优先处理)。
                                let label = if lower.starts_with("/snapshot ") {
                                    &input[10..]
                                } else {
                                    "manual"
                                };
                                match app.agent_state.agent.create_snapshot(label) {
                                    Some(id) => app.push_msg(
                                        MessageRole::Assistant,
                                        format!(
                                            "{}: {} (label: {})",
                                            app.s.snapshot_created, id, label
                                        ),
                                    ),
                                    None => app.push_msg(
                                        MessageRole::Assistant,
                                        app.s.snapshot_failed.to_string(),
                                    ),
                                }
                            } else if lower == "/lsp" || lower == "/lsp on" {
                                app.agent_state.agent.enable_lsp(true);
                                app.push_msg(MessageRole::Assistant, app.s.lsp_enabled.to_string());
                            } else if lower == "/lsp off" {
                                app.agent_state.agent.enable_lsp(false);
                                app.push_msg(
                                    MessageRole::Assistant,
                                    app.s.lsp_disabled.to_string(),
                                );
                            } else if lower == "/failures" {
                                let breakdown = app.agent_state.agent.failure_breakdown();
                                let count = app.agent_state.agent.failure_count();
                                app.push_msg(
                                    MessageRole::Assistant,
                                    format!(
                                        "{}: {} failures\n{}",
                                        app.s.failure_tracking, count, breakdown
                                    ),
                                );
                            } else if lower == "/cost" {
                                let cost = app.agent_state.agent.format_session_cost();
                                app.push_msg(
                                    MessageRole::Assistant,
                                    format!("{}: {}", app.s.session_cost_label, cost),
                                );
                            } else if lower == "/pricing" {
                                let zh = matches!(app.lang, Lang::Zh);
                                app.push_msg(
                                    MessageRole::Assistant,
                                    if zh {
                                        "🔄 正在从 DeepSeek 官方文档获取最新定价 ...".to_string()
                                    } else {
                                        "🔄 Fetching latest pricing from DeepSeek docs ..."
                                            .to_string()
                                    },
                                );
                                // 修复(High #H13):refresh_from_official 用
                                // reqwest::blocking::Client(同步,最长 8s timeout),
                                // 直接在 async 事件循环里调用会冻结 TUI。
                                // 用 spawn_blocking 移到阻塞线程池,不阻塞事件循环。
                                let pricing_result = tokio::task::spawn_blocking(
                                    crate::common::pricing::refresh_from_official,
                                )
                                .await;
                                match pricing_result {
                                    Ok(Ok((pricing, path))) => {
                                        let body = format!(
                                            "{} {}\n\n{}",
                                            if zh { "✅ 已更新" } else { "✅ Updated" },
                                            path.display(),
                                            pricing.to_display_table(),
                                        );
                                        app.push_msg(MessageRole::Assistant, body);
                                    }
                                    Ok(Err(e)) => {
                                        app.push_msg(
                                            MessageRole::Assistant,
                                            if zh {
                                                format!(
                                                    "❌ 获取失败: {}\n可手动编辑 ~/.movix/pricing.toml 调整价格。",
                                                    e
                                                )
                                            } else {
                                                format!(
                                                    "❌ Failed to fetch pricing: {}\nYou can edit ~/.movix/pricing.toml manually.",
                                                    e
                                                )
                                            },
                                        );
                                    }
                                    Err(join_err) => {
                                        app.push_msg(
                                            MessageRole::Assistant,
                                            if zh {
                                                format!("❌ 后台任务异常: {}", join_err)
                                            } else {
                                                format!("❌ Background task failed: {}", join_err)
                                            },
                                        );
                                    }
                                }
                            } else if lower == "/skill" || lower == "/skill list" {
                                let output = skill_executor::format_skill_list(
                                    app.agent_state.agent.skill_registry(),
                                );
                                app.push_msg(MessageRole::Assistant, output);
                            } else if lower == "/skill refresh" {
                                let zh = matches!(app.lang, Lang::Zh);
                                app.agent_state.agent.refresh_skills();
                                let count = app.agent_state.agent.skill_registry().count();
                                app.push_msg(
                                    MessageRole::Assistant,
                                    if zh {
                                        format!("🔄 技能已刷新，共发现 {} 个技能", count)
                                    } else {
                                        format!("🔄 Skills refreshed, found {} skills", count)
                                    },
                                );
                            } else if lower.starts_with("/skill enable ") {
                                let zh = matches!(app.lang, Lang::Zh);
                                let name = &input[14..];
                                if app.agent_state.agent.enable_skill(name) {
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            format!("✅ 技能 {} 已启用", name)
                                        } else {
                                            format!("✅ Skill {} enabled", name)
                                        },
                                    );
                                } else {
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            format!("❌ 未找到技能: {}", name)
                                        } else {
                                            format!("❌ Skill not found: {}", name)
                                        },
                                    );
                                }
                            } else if lower.starts_with("/skill disable ") {
                                let zh = matches!(app.lang, Lang::Zh);
                                let name = &input[15..];
                                if app.agent_state.agent.disable_skill(name) {
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            format!("❌ 技能 {} 已禁用", name)
                                        } else {
                                            format!("❌ Skill {} disabled", name)
                                        },
                                    );
                                } else {
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            format!("❌ 未找到技能: {}", name)
                                        } else {
                                            format!("❌ Skill not found: {}", name)
                                        },
                                    );
                                }
                            } else if lower.starts_with("/skill use ") {
                                let zh = matches!(app.lang, Lang::Zh);
                                let rest = &input[11..];
                                let parts: Vec<&str> = rest.splitn(2, ' ').collect();
                                if parts.len() < 2 {
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            "用法: /skill use <技能名> <任务描述>".to_string()
                                        } else {
                                            "Usage: /skill use <skill_name> <task>".to_string()
                                        },
                                    );
                                } else {
                                    let skill_name = parts[0];
                                    let task = parts[1];
                                    match app.agent_state.agent.use_skill(skill_name, task) {
                                        Some(result) => {
                                            app.push_msg(MessageRole::Assistant, result.output);
                                        }
                                        None => {
                                            app.push_msg(MessageRole::Assistant, if zh {
                                                format!("❌ 无法使用技能: {}（未找到或已禁用）", skill_name)
                                            } else {
                                                format!("❌ Cannot use skill: {} (not found or disabled)", skill_name)
                                            });
                                        }
                                    }
                                }
                            } else if lower.starts_with("/skill info ") {
                                let zh = matches!(app.lang, Lang::Zh);
                                let name = &input[12..];
                                match app.agent_state.agent.skill_registry().get(name) {
                                    Some(skill) => {
                                        let tags = if skill.tags.is_empty() {
                                            if zh { "无" } else { "none" }.to_string()
                                        } else {
                                            skill.tags.join(", ")
                                        };
                                        let trigger = skill.trigger.as_deref().unwrap_or(if zh {
                                            "无"
                                        } else {
                                            "none"
                                        });
                                        let builtin = if skill.builtin {
                                            if zh { "内置" } else { "builtin" }
                                        } else {
                                            if zh { "自定义" } else { "custom" }
                                        };
                                        let status = if skill.enabled { "✅" } else { "❌" };
                                        app.push_msg(MessageRole::Assistant, format!(
                                            "{} {} [{}]\n  {}: {}\n  {}: {}\n  {}: {}\n  {}: {}\n  {}: {}",
                                            status,
                                            skill.name,
                                            skill.skill_type.display_name(),
                                            if zh { "描述" } else { "Description" },
                                            skill.description,
                                            if zh { "版本" } else { "Version" },
                                            skill.version,
                                            if zh { "标签" } else { "Tags" },
                                            tags,
                                            if zh { "触发词" } else { "Trigger" },
                                            trigger,
                                            if zh { "来源" } else { "Source" },
                                            builtin,
                                        ));
                                    }
                                    None => {
                                        app.push_msg(
                                            MessageRole::Assistant,
                                            if zh {
                                                format!("❌ 未找到技能: {}", name)
                                            } else {
                                                format!("❌ Skill not found: {}", name)
                                            },
                                        );
                                    }
                                }
                            } else if lower == "/mcp" || lower == "/mcp status" {
                                let zh = matches!(app.lang, Lang::Zh);
                                let statuses = app.agent_state.agent.mcp_server_statuses().await;
                                if statuses.is_empty() {
                                    app.push_msg(MessageRole::Assistant, if zh {
                                        "未配置 MCP 服务器\n在 .movix/mcp.json 或 MOVIX_MCP_SERVERS 环境变量中配置".to_string()
                                    } else {
                                        "No MCP servers configured\nConfigure in .movix/mcp.json or MOVIX_MCP_SERVERS env var".to_string()
                                    });
                                } else {
                                    let mut lines = Vec::new();
                                    for (name, status) in &statuses {
                                        let status_str = match status {
                                            crate::mcp::McpServerStatus::Connected => {
                                                "✅ Connected"
                                            }
                                            crate::mcp::McpServerStatus::Disconnected => {
                                                "⚪ Disconnected"
                                            }
                                            crate::mcp::McpServerStatus::Connecting => {
                                                "🔄 Connecting..."
                                            }
                                            crate::mcp::McpServerStatus::Error(e) => {
                                                &format!("❌ Error: {}", e)[..]
                                            }
                                        };
                                        lines.push(format!("  {} - {}", name, status_str));
                                    }
                                    let tool_count = app.agent_state.agent.mcp_tool_count().await;
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        format!(
                                            "{} ({} {}):\n{}",
                                            if zh { "MCP 服务器" } else { "MCP Servers" },
                                            tool_count,
                                            if zh { "个工具" } else { "tools" },
                                            lines.join("\n")
                                        ),
                                    );
                                }
                            } else if lower.starts_with("/mcp reconnect ") {
                                let zh = matches!(app.lang, Lang::Zh);
                                let server_name = &input[15..].trim();
                                match app.agent_state.agent.mcp_reconnect(server_name).await {
                                    Ok(()) => {
                                        app.push_msg(
                                            MessageRole::Assistant,
                                            if zh {
                                                format!("✅ MCP 服务器 {} 已重新连接", server_name)
                                            } else {
                                                format!("✅ MCP server {} reconnected", server_name)
                                            },
                                        );
                                    }
                                    Err(e) => {
                                        app.push_msg(
                                            MessageRole::Assistant,
                                            if zh {
                                                format!("❌ 重连失败: {}", e)
                                            } else {
                                                format!("❌ Reconnect failed: {}", e)
                                            },
                                        );
                                    }
                                }
                            } else if lower == "/mcp tools" {
                                let zh = matches!(app.lang, Lang::Zh);
                                let all_tools = {
                                    let manager = app.agent_state.agent.mcp_manager().lock().await;
                                    manager.all_tools().await
                                };
                                if all_tools.is_empty() {
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            "无 MCP 工具可用".to_string()
                                        } else {
                                            "No MCP tools available".to_string()
                                        },
                                    );
                                } else {
                                    let mut lines = Vec::new();
                                    for (server, tool) in &all_tools {
                                        let desc = tool.description.as_deref().unwrap_or("-");
                                        lines
                                            .push(format!("  {}.{} - {}", server, tool.name, desc));
                                    }
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        format!(
                                            "{} ({}):\n{}",
                                            if zh { "MCP 工具列表" } else { "MCP Tools" },
                                            all_tools.len(),
                                            lines.join("\n")
                                        ),
                                    );
                                }
                            } else if lower.starts_with("/decompose ") {
                                let zh = matches!(app.lang, Lang::Zh);
                                let task = &input[11..];
                                app.view.streaming = true;
                                match app.agent_state.agent.decompose_task(task).await {
                                    Ok(Some(decomposed)) => {
                                        app.push_msg(
                                            MessageRole::Assistant,
                                            format!(
                                                "{}: {} {} ({} {})",
                                                if zh {
                                                    "📋 任务分解结果"
                                                } else {
                                                    "📋 Task decomposition"
                                                },
                                                if zh { "策略" } else { "Strategy" },
                                                decomposed.strategy.display_name(),
                                                if zh { "子任务数" } else { "sub-tasks" },
                                                decomposed.task_count(),
                                            ),
                                        );
                                        for st in &decomposed.sub_tasks {
                                            let deps = if st.depends_on.is_empty() {
                                                if zh { "无" } else { "none" }.to_string()
                                            } else {
                                                st.depends_on
                                                    .iter()
                                                    .map(|d| format!("#{}", d))
                                                    .collect::<Vec<_>>()
                                                    .join(", ")
                                            };
                                            app.push_msg(
                                                MessageRole::Assistant,
                                                format!(
                                                    "  {}{}. {} [{}] {}→ {}",
                                                    st.suggested_role.icon(),
                                                    st.step,
                                                    st.description,
                                                    st.suggested_role.display_name(),
                                                    if zh { "依赖: " } else { "deps: " },
                                                    deps,
                                                ),
                                            );
                                        }
                                        let layers = decomposed.execution_layers();
                                        if layers.len() > 1 {
                                            app.push_msg(
                                                MessageRole::Assistant,
                                                format!(
                                                    "  {} {} {}",
                                                    if zh {
                                                        "执行层级"
                                                    } else {
                                                        "Execution layers"
                                                    },
                                                    layers.len(),
                                                    if zh {
                                                        "层（可并行）"
                                                    } else {
                                                        "(parallelizable)"
                                                    },
                                                ),
                                            );
                                        }
                                    }
                                    Ok(None) => {
                                        app.push_msg(
                                            MessageRole::Assistant,
                                            if zh {
                                                "无法分解此任务，可能任务过于简单"
                                            } else {
                                                "Cannot decompose this task, it may be too simple"
                                            }
                                            .to_string(),
                                        );
                                    }
                                    Err(e) => {
                                        app.push_msg(
                                            MessageRole::Assistant,
                                            if zh {
                                                format!("❌ 分解失败: {}", e)
                                            } else {
                                                format!("❌ Decomposition failed: {}", e)
                                            },
                                        );
                                    }
                                }
                                app.view.streaming = false;
                            } else if lower == "/save" {
                                let zh = matches!(app.lang, Lang::Zh);
                                // 修复(P2.4):走异步 save,主线程不再被磁盘 IO 阻塞。
                                match app.agent_state.agent.save_session_async().await {
                                    Ok(()) => app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            "✅ 会话已保存"
                                        } else {
                                            "✅ Session saved"
                                        }
                                        .to_string(),
                                    ),
                                    Err(e) => app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            format!("❌ 保存失败: {}", e)
                                        } else {
                                            format!("❌ Save failed: {}", e)
                                        },
                                    ),
                                }
                            } else if lower == "/restore" {
                                let zh = matches!(app.lang, Lang::Zh);
                                match app.agent_state.agent.restore_session() {
                                    Ok(true) => app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            "✅ 会话已恢复"
                                        } else {
                                            "✅ Session restored"
                                        }
                                        .to_string(),
                                    ),
                                    Ok(false) => app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            "⚪ 没有可恢复的会话"
                                        } else {
                                            "⚪ No session to restore"
                                        }
                                        .to_string(),
                                    ),
                                    Err(e) => app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            format!("❌ 恢复失败: {}", e)
                                        } else {
                                            format!("❌ Restore failed: {}", e)
                                        },
                                    ),
                                }
                            } else if lower == "/index" {
                                let zh = matches!(app.lang, Lang::Zh);
                                // 修复(审查):build_project_index 内部做两次全量递归遍历,
                                // 此前在 TUI 事件循环线程同步执行,大仓库会卡死 UI。移入
                                // spawn_blocking,索引构建期间事件循环不被阻塞。
                                let workspace = app.agent_state.agent.workspace();
                                let index_summary = tokio::task::spawn_blocking(move || {
                                    let indexer = crate::common::project_index::ProjectIndexer::new(
                                        &workspace,
                                    );
                                    let index = indexer.build_index();
                                    format!(
                                        "项目类型: {:?}\n文件数: {}\n估计行数: {}\n语言分布: {:?}\n目录树:\n{}",
                                        index.project_type,
                                        index.total_files,
                                        index.total_lines_estimate,
                                        index.language_distribution,
                                        index.directory_tree
                                    )
                                })
                                .await
                                .unwrap_or_else(|e| format!("索引构建失败: {}", e));
                                app.push_msg(
                                    MessageRole::Assistant,
                                    if zh {
                                        format!("📊 项目索引:\n{}", index_summary)
                                    } else {
                                        format!("📊 Project Index:\n{}", index_summary)
                                    },
                                );
                            } else if lower == "/verify" {
                                let result = app.agent_state.agent.verify_modifications();
                                app.push_msg(
                                    MessageRole::Assistant,
                                    format!("🔍 验证结果:\n{}", result),
                                );
                            } else if lower == "/compress" {
                                let zh = matches!(app.lang, Lang::Zh);
                                app.push_msg(
                                    MessageRole::Assistant,
                                    if zh {
                                        "⏳ 正在压缩上下文..."
                                    } else {
                                        "⏳ Compressing context..."
                                    }
                                    .to_string(),
                                );
                                match app.agent_state.agent.compress_context().await {
                                    Ok(summary) => app.push_msg(
                                        MessageRole::Assistant,
                                        format!("✅ {}", summary),
                                    ),
                                    Err(e) => app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            format!("❌ 压缩失败: {}", e)
                                        } else {
                                            format!("❌ Compression failed: {}", e)
                                        },
                                    ),
                                }
                            } else if lower == "/changes" {
                                let zh = matches!(app.lang, Lang::Zh);
                                let changes = app.agent_state.agent.detect_external_changes();
                                if changes.is_empty() {
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            "✅ 未检测到外部文件变更"
                                        } else {
                                            "✅ No external file changes detected"
                                        }
                                        .to_string(),
                                    );
                                } else {
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            format!("📁 外部文件变更:\n{}", changes.join("\n"))
                                        } else {
                                            format!("📁 External changes:\n{}", changes.join("\n"))
                                        },
                                    );
                                }
                            } else if lower == "/rollback" {
                                let zh = matches!(app.lang, Lang::Zh);
                                match app.agent_state.agent.auto_rollback() {
                                    Ok(msg) => {
                                        app.push_msg(MessageRole::Assistant, format!("⏪ {}", msg))
                                    }
                                    Err(e) => app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            format!("❌ 回滚失败: {}", e)
                                        } else {
                                            format!("❌ Rollback failed: {}", e)
                                        },
                                    ),
                                }
                            } else if lower.starts_with("/review ") {
                                let zh = matches!(app.lang, Lang::Zh);
                                let desc = &input[8..];
                                let modified = app.agent_state.agent.modified_files().to_vec();
                                if modified.is_empty() {
                                    app.push_msg(
                                        MessageRole::Assistant,
                                        if zh {
                                            "⚪ 没有已修改的文件需要审查"
                                        } else {
                                            "⚪ No modified files to review"
                                        }
                                        .to_string(),
                                    );
                                } else {
                                    // 修复(审查):此前第三个参数 diff_or_code 硬编码传 "",
                                    // 评审器只有文件名没有 diff,`/review` 是"空审"——安全关键变更
                                    // 被空审放行并给出虚假的"审查通过"。这里生成真实 diff 传下去。
                                    let workspace = app.agent_state.agent.workspace();
                                    let diff_strs: Vec<String> = modified
                                        .iter()
                                        .map(|p| p.to_string_lossy().to_string())
                                        .collect();
                                    let mut diff = tokio::process::Command::new("git")
                                        .arg("diff")
                                        .arg("--")
                                        .args(&diff_strs)
                                        .current_dir(&workspace)
                                        .output()
                                        .await
                                        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                                        .unwrap_or_default();
                                    if diff.trim().is_empty() {
                                        // 无已跟踪改动(多为新建文件):直接读内容作为"变更内容"。
                                        let mut parts: Vec<String> = Vec::new();
                                        for f in &diff_strs {
                                            if let Ok(content) =
                                                std::fs::read_to_string(workspace.join(f))
                                            {
                                                parts.push(format!("=== {} ===\n{}", f, content));
                                            }
                                        }
                                        diff = parts.join("\n\n");
                                    }
                                    const REVIEW_DIFF_CAP: usize = 60_000;
                                    if diff.len() > REVIEW_DIFF_CAP {
                                        diff.truncate(REVIEW_DIFF_CAP);
                                        diff.push_str("\n...[diff 过长已截断]");
                                    }
                                    match app
                                        .agent_state
                                        .agent
                                        .review_changes(desc, &diff_strs.join(", "), &diff)
                                        .await
                                    {
                                        Ok(result) => app.push_msg(MessageRole::Assistant, result),
                                        Err(e) => app.push_msg(
                                            MessageRole::Assistant,
                                            if zh {
                                                format!("❌ 审查失败: {}", e)
                                            } else {
                                                format!("❌ Review failed: {}", e)
                                            },
                                        ),
                                    }
                                }
                            } else {
                                app.push_msg(MessageRole::Assistant, app.s.unknown_cmd.to_string());
                            }
                            draw_needed = true;
                            continue;
                        }

                        app.view.messages.push(ChatMessage {
                            role: MessageRole::User,
                            content: input.clone(),
                        });
                        app.view.chat_cache_valid = false;
                        app.view.thinking_lines.clear();
                        app.session.last_stats = None;
                        app.view.task_list.clear();
                        app.view.thinking_expanded = true;
                        app.view.scroll_offset = 0;
                        app.view.auto_scroll = true;
                        app.view.streaming = true;
                        app.session.turn_start_stats =
                            app.agent_state.agent.cached_token_stats().clone();
                        app.session.turn_started_at = Some(Instant::now());
                        app.session.current_turn_summary = summarize_turn_input(&input);

                        let user_input = input.clone();
                        let tx = stream_tx.clone();

                        app.view.context_used = app.agent_state.agent.context_token_count();
                        app.view.context_max = app.agent_state.agent.context_max_tokens();

                        let current_config = app.agent_state.agent.config().clone();
                        let shared_mode_config = app.agent_state.agent.mode_config().clone();
                        // 修复(Bug #2):spawn 前 clone stats handle,UI 在
                        // app.agent 被 swap 成占位实例期间仍能读到真实数据。
                        app.agent_state.shared_stats_handle =
                            Some(app.agent_state.agent.shared_stats_handle());
                        // 修复(Bug #4):spawn 前 reset + clone cancel flag。
                        app.agent_state.agent.reset_cancel_flag();
                        app.agent_state.cancel_flag_handle =
                            Some(app.agent_state.agent.cancel_flag_handle());
                        let mut agent = std::mem::replace(
                            &mut app.agent_state.agent,
                            MovixAgent::new(current_config)?,
                        );
                        app.agent_state.agent.set_mode_config(shared_mode_config);

                        let (approval_tx, approval_rx) =
                            tokio::sync::mpsc::channel::<crate::agent::modes::ApprovalDecision>(1);
                        agent.set_approval_channel(approval_rx);
                        app.agent_state.approval_tx = Some(approval_tx);
                        app.agent_state.pending_approval = None;
                        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
                        app.agent_state.agent_cancel_tx = Some(cancel_tx);

                        let task_handle = tokio::spawn(async move {
                            let tx_content = tx.clone();
                            let tx_thinking = tx.clone();
                            let tx_tool = tx.clone();
                            let tx_done = tx.clone();
                            let tx_err = tx.clone();
                            let tx_ctx = tx.clone();

                            let result = {
                                let run_fut = agent.run_streaming(
                                    &user_input,
                                    move |text| {
                                        let _ = tx_content.try_send(StreamUpdate::Content(text));
                                    },
                                    move |text| {
                                        let _ = tx_thinking.try_send(StreamUpdate::Thinking(text));
                                    },
                                    move |tool, detail, file, output| {
                                        let _ = tx_tool.try_send(StreamUpdate::ToolAction {
                                            tool,
                                            detail,
                                            file,
                                            output,
                                        });
                                    },
                                    move |used, max| {
                                        let _ =
                                            tx_ctx.try_send(StreamUpdate::Context { used, max });
                                    },
                                );
                                tokio::pin!(run_fut);

                                tokio::select! {
                                    result = &mut run_fut => Some(result),
                                    _ = cancel_rx => None,
                                }
                            };

                            if let Some(result) = result {
                                match result {
                                    Ok(_) => {
                                        let _ = tx_done.try_send(StreamUpdate::Done);
                                    }
                                    Err(e) => {
                                        let _ = tx_err.try_send(StreamUpdate::Error(e.to_string()));
                                    }
                                }
                            }

                            agent
                        });

                        // 修复(并发#1):替换旧 agent 前先 cancel+abort,避免旧
                        // task 在后台继续消耗 API 配额。连续快速输入会叠加任务。
                        if let Some(old_handle) = app.agent_state.agent_handle.take() {
                            if let Some(ref flag) = app.agent_state.cancel_flag_handle {
                                flag.store(true, std::sync::atomic::Ordering::Release);
                            }
                            if let Some(old_cancel) = app.agent_state.agent_cancel_tx.take() {
                                let _ = old_cancel.send(());
                            }
                            old_handle.abort();
                        }
                        app.agent_state.agent_handle = Some(task_handle);
                        draw_needed = true;
                    }
                    KeyCode::Backspace if app.input.cursor_pos > 0 => {
                        app.input.input_select_anchor = None;
                        let prev = prev_char_boundary(&app.input.input_buf, app.input.cursor_pos);
                        app.input.input_buf.remove(prev);
                        app.input.cursor_pos = prev;
                        app.input.history_cursor = None;
                        if app.input.input_buf.starts_with('/') {
                            app.clamp_palette_index(PALETTE_MAX_VISIBLE);
                        }
                        draw_needed = true;
                    }
                    KeyCode::Delete if app.input.cursor_pos < app.input.input_buf.len() => {
                        app.input.input_select_anchor = None;
                        app.input.input_buf.remove(app.input.cursor_pos);
                        app.input.history_cursor = None;
                        if app.input.input_buf.starts_with('/') {
                            app.clamp_palette_index(PALETTE_MAX_VISIBLE);
                        }
                        draw_needed = true;
                    }
                    // ── 输入框文本选择（Shift+方向键） ──
                    KeyCode::Left if shift && app.view.focus == FocusZone::Input => {
                        if app.input.input_select_anchor.is_none() {
                            app.input.input_select_anchor = Some(app.input.cursor_pos);
                        }
                        if app.input.cursor_pos > 0 {
                            app.input.cursor_pos =
                                prev_char_boundary(&app.input.input_buf, app.input.cursor_pos);
                        }
                        draw_needed = true;
                    }
                    KeyCode::Right if shift && app.view.focus == FocusZone::Input => {
                        if app.input.input_select_anchor.is_none() {
                            app.input.input_select_anchor = Some(app.input.cursor_pos);
                        }
                        if app.input.cursor_pos < app.input.input_buf.len() {
                            app.input.cursor_pos =
                                next_char_boundary(&app.input.input_buf, app.input.cursor_pos);
                        }
                        draw_needed = true;
                    }
                    KeyCode::Home if shift && app.view.focus == FocusZone::Input => {
                        if app.input.input_select_anchor.is_none() {
                            app.input.input_select_anchor = Some(app.input.cursor_pos);
                        }
                        let before = &app.input.input_buf[..app.input.cursor_pos];
                        let line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
                        app.input.cursor_pos = line_start;
                        draw_needed = true;
                    }
                    KeyCode::End if shift && app.view.focus == FocusZone::Input => {
                        if app.input.input_select_anchor.is_none() {
                            app.input.input_select_anchor = Some(app.input.cursor_pos);
                        }
                        let after = &app.input.input_buf[app.input.cursor_pos..];
                        let line_end = after
                            .find('\n')
                            .map(|p| app.input.cursor_pos + p)
                            .unwrap_or(app.input.input_buf.len());
                        app.input.cursor_pos = line_end;
                        draw_needed = true;
                    }
                    KeyCode::Left if ctrl => {
                        app.input.input_select_anchor = None;
                        let pos = prev_word_boundary(&app.input.input_buf, app.input.cursor_pos);
                        app.input.cursor_pos = pos;
                        draw_needed = true;
                    }
                    KeyCode::Left if app.input.cursor_pos > 0 => {
                        app.input.input_select_anchor = None;
                        app.input.cursor_pos =
                            prev_char_boundary(&app.input.input_buf, app.input.cursor_pos);
                        draw_needed = true;
                    }
                    KeyCode::Right if ctrl => {
                        app.input.input_select_anchor = None;
                        let pos = next_word_boundary(&app.input.input_buf, app.input.cursor_pos);
                        app.input.cursor_pos = pos;
                        draw_needed = true;
                    }
                    KeyCode::Right if app.input.cursor_pos < app.input.input_buf.len() => {
                        app.input.input_select_anchor = None;
                        app.input.cursor_pos =
                            next_char_boundary(&app.input.input_buf, app.input.cursor_pos);
                        draw_needed = true;
                    }
                    KeyCode::Up => {
                        app.input.input_select_anchor = None;
                        if app.view.focus == FocusZone::Palette
                            && app.input.input_buf.starts_with('/')
                        {
                            app.palette_move(-1, PALETTE_PAGE_SIZE);
                            draw_needed = true;
                            continue;
                        }
                        if app.input.input_buf.trim().is_empty() {
                            if app.view.scroll_offset < 1000 {
                                app.view.scroll_offset += 3;
                                app.view.auto_scroll = false;
                                draw_needed = true;
                            }
                        } else if app.input.input_buf[..app.input.cursor_pos]
                            .chars()
                            .filter(|&c| c == '\n')
                            .count()
                            > 0
                        {
                            let before = &app.input.input_buf[..app.input.cursor_pos];
                            let prev_newline = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
                            let line_start = prev_char_boundary(&app.input.input_buf, prev_newline);
                            let col_in_line = before[line_start..].chars().count();
                            let prev_line_start = if line_start > 0 {
                                let p = prev_char_boundary(&app.input.input_buf, line_start - 1);
                                let before_prev = &app.input.input_buf[..p];
                                before_prev.rfind('\n').map(|n| n + 1).unwrap_or(0)
                            } else {
                                0
                            };
                            let prev_line =
                                &app.input.input_buf[prev_line_start..line_start.saturating_sub(1)];
                            let prev_chars: Vec<char> = prev_line.chars().collect();
                            let target_col = col_in_line.min(prev_chars.len());
                            let mut byte_pos = prev_line_start;
                            for (i, ch) in prev_line.char_indices() {
                                if i >= target_col {
                                    break;
                                }
                                byte_pos = prev_line_start + i + ch.len_utf8();
                            }
                            app.input.cursor_pos = byte_pos.min(app.input.input_buf.len());
                            draw_needed = true;
                        } else {
                            app.history_up();
                            draw_needed = true;
                        }
                    }
                    KeyCode::Down => {
                        app.input.input_select_anchor = None;
                        if app.view.focus == FocusZone::Palette
                            && app.input.input_buf.starts_with('/')
                        {
                            app.palette_move(1, PALETTE_PAGE_SIZE);
                            draw_needed = true;
                            continue;
                        }
                        if app.input.input_buf.trim().is_empty() && !app.view.auto_scroll {
                            app.view.scroll_offset = app.view.scroll_offset.saturating_sub(3);
                            if app.view.scroll_offset == 0 {
                                app.view.auto_scroll = true;
                            }
                            draw_needed = true;
                        } else if app.input.input_buf[..app.input.cursor_pos]
                            .chars()
                            .filter(|&c| c == '\n')
                            .count()
                            < app.input.input_buf.chars().filter(|&c| c == '\n').count()
                        {
                            let before = &app.input.input_buf[..app.input.cursor_pos];
                            let this_line_end = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
                            let col_in_line = before[this_line_end..].chars().count();
                            let next_newline = app.input.input_buf[app.input.cursor_pos..]
                                .find('\n')
                                .map(|p| app.input.cursor_pos + p);
                            if let Some(nl) = next_newline {
                                let next_line_start = nl + 1;
                                let end = app.input.input_buf[next_line_start..]
                                    .find('\n')
                                    .map(|p| next_line_start + p)
                                    .unwrap_or(app.input.input_buf.len());
                                let next_line = &app.input.input_buf[next_line_start..end];
                                let next_chars: Vec<char> = next_line.chars().collect();
                                let target_col = col_in_line.min(next_chars.len());
                                let mut byte_pos = next_line_start;
                                for (ci, (i, ch)) in next_line.char_indices().enumerate() {
                                    if ci >= target_col {
                                        break;
                                    }
                                    byte_pos = next_line_start + i + ch.len_utf8();
                                }
                                app.input.cursor_pos = byte_pos.min(app.input.input_buf.len());
                            }
                            draw_needed = true;
                        } else {
                            app.history_down();
                            draw_needed = true;
                        }
                    }
                    KeyCode::Home => {
                        app.input.input_select_anchor = None;
                        let before = &app.input.input_buf[..app.input.cursor_pos];
                        let line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
                        app.input.cursor_pos = line_start;
                        draw_needed = true;
                    }
                    KeyCode::End => {
                        app.input.input_select_anchor = None;
                        let after = &app.input.input_buf[app.input.cursor_pos..];
                        let line_end = after
                            .find('\n')
                            .map(|p| app.input.cursor_pos + p)
                            .unwrap_or(app.input.input_buf.len());
                        app.input.cursor_pos = line_end;
                        draw_needed = true;
                    }
                    KeyCode::PageUp => {
                        if app.view.focus == FocusZone::Palette
                            && app.input.input_buf.starts_with('/')
                        {
                            app.palette_page(-1, PALETTE_PAGE_SIZE);
                            draw_needed = true;
                            continue;
                        }
                        if app.view.focus == FocusZone::Sidebar {
                            let max_scroll = app.view.task_list.len().saturating_sub(6);
                            app.view.sidebar_scroll = (app.view.sidebar_scroll + 3).min(max_scroll);
                            let max_turn_scroll = app.session.turn_stats.len().saturating_sub(5);
                            app.view.sidebar_turn_scroll =
                                (app.view.sidebar_turn_scroll + 3).min(max_turn_scroll);
                        } else {
                            app.view.scroll_offset += 10;
                            app.view.auto_scroll = false;
                        }
                        draw_needed = true;
                    }
                    KeyCode::PageDown => {
                        if app.view.focus == FocusZone::Palette
                            && app.input.input_buf.starts_with('/')
                        {
                            app.palette_page(1, PALETTE_PAGE_SIZE);
                            draw_needed = true;
                            continue;
                        }
                        if app.view.focus == FocusZone::Sidebar {
                            app.view.sidebar_scroll = app.view.sidebar_scroll.saturating_sub(3);
                            app.view.sidebar_turn_scroll =
                                app.view.sidebar_turn_scroll.saturating_sub(3);
                        } else {
                            app.view.scroll_offset = app.view.scroll_offset.saturating_sub(10);
                            if app.view.scroll_offset == 0 {
                                app.view.auto_scroll = true;
                            }
                        }
                        draw_needed = true;
                    }
                    KeyCode::Char('g') if !ctrl && !alt => {
                        app.view.scroll_offset = 0;
                        app.view.auto_scroll = true;
                        draw_needed = true;
                    }
                    KeyCode::Tab if ctrl => {
                        app.cycle_focus_forward();
                        draw_needed = true;
                    }
                    KeyCode::Tab
                        if app.view.focus == FocusZone::Palette
                            && app.input.input_buf.starts_with('/') =>
                    {
                        if let Some(cmd) = app.palette_selected_cmd() {
                            let mut buf = cmd.to_string();
                            if has_subcommands(cmd) {
                                buf.push(' ');
                            }
                            app.input.input_buf = buf;
                            app.input.cursor_pos = app.input.input_buf.len();
                            app.view.focus = FocusZone::Input;
                            app.reset_palette();
                        }
                        draw_needed = true;
                    }
                    KeyCode::Tab => {
                        let new_mode = app.agent_state.agent.mode().cycle();
                        app.agent_state.agent.set_mode(new_mode);
                        draw_needed = true;
                    }
                    KeyCode::BackTab => {
                        let _ = app.agent_state.agent.cycle_thinking();
                        draw_needed = true;
                    }
                    KeyCode::Char('t') if ctrl => {
                        app.toggle_model();
                        draw_needed = true;
                    }
                    KeyCode::F(2) => {
                        if app.input.input_buf.starts_with('/') && !app.view.streaming {
                            if app.view.focus == FocusZone::Palette {
                                app.view.focus = FocusZone::Input;
                            } else {
                                app.view.focus = FocusZone::Palette;
                                app.clamp_palette_index(PALETTE_MAX_VISIBLE);
                            }
                        }
                        draw_needed = true;
                    }
                    KeyCode::Esc => {
                        app.input.input_select_anchor = None;
                        if app.agent_state.pending_approval.is_some() {
                            if let Some(ref tx) = app.agent_state.approval_tx {
                                let _ = tx.try_send(crate::agent::modes::ApprovalDecision::Denied);
                            }
                            app.agent_state.pending_approval = None;
                        } else if app.view.streaming {
                            app.stop_running().await;
                        } else if app.view.focus == FocusZone::Palette {
                            app.view.focus = FocusZone::Input;
                            app.reset_palette();
                            if app.input.input_buf.starts_with('/') {
                                app.input.input_buf.clear();
                                app.input.cursor_pos = 0;
                                app.input.input_scroll_y = 0;
                            }
                        } else if app.input.history_cursor.is_some() {
                            app.cancel_history();
                        } else {
                            app.input.input_buf.clear();
                            app.input.cursor_pos = 0;
                            app.input.input_scroll_y = 0;
                            app.view.focus = FocusZone::Input;
                        }
                        draw_needed = true;
                    }
                    _ => {}
                }
            }
            Event::Mouse(mouse) => {
                let size = crossterm::terminal::size()?;
                let root = Rect::new(0, 0, size.0, size.1);
                let rows = root_rows(root, input_height(&app), 0);

                let chat_area = rows[1];
                let sidebar_w = if size.0 >= SIDEBAR_WIDE_THRESHOLD {
                    SIDEBAR_WIDE
                } else if size.0 >= SIDEBAR_MEDIUM_THRESHOLD {
                    SIDEBAR_MEDIUM
                } else {
                    0
                };
                let chat_layout = if sidebar_w > 0 {
                    let rects = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Min(42), Constraint::Length(sidebar_w)])
                        .split(chat_area);
                    Some((rects[0], rects[1]))
                } else {
                    None
                };
                let chat_only = chat_layout.map(|(c, _)| c).unwrap_or(chat_area);
                let in_chat = contains(chat_only, mouse.column, mouse.row);

                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if app.input.input_buf.starts_with('/')
                            && !app.view.streaming
                            && let Some(palette_area) = command_suggestion_area(&app, root, rows[2])
                            && contains(palette_area, mouse.column, mouse.row)
                        {
                            let click_pos = (mouse.column, mouse.row);
                            let is_double = app
                                .view
                                .last_palette_click
                                .map(|(x, y, t)| {
                                    x == click_pos.0
                                        && y == click_pos.1
                                        && t.elapsed() < std::time::Duration::from_millis(500)
                                })
                                .unwrap_or(false);
                            app.view.last_palette_click =
                                Some((click_pos.0, click_pos.1, std::time::Instant::now()));
                            if let Some(cmd) =
                                command_at(&app, root, rows[2], mouse.column, mouse.row)
                            {
                                if is_double {
                                    // 双击: auto-exec 命令写入 input_buf 并 fall-through,
                                    // 有参命令仅插入
                                    app.input.input_buf = cmd.to_string();
                                    app.input.cursor_pos = app.input.input_buf.len();
                                    if should_auto_execute(&cmd) {
                                        // 标记: 下次 Enter 时立即执行
                                        app.view.focus = FocusZone::Input;
                                        app.reset_palette();
                                        // fall-through 不可能(这里是 mouse 事件),
                                        // 所以仅写入文本,用户需按 Enter
                                    } else {
                                        if has_subcommands(&cmd) {
                                            app.input.input_buf.push(' ');
                                            app.input.cursor_pos += 1;
                                        }
                                        app.view.focus = FocusZone::Input;
                                        app.reset_palette();
                                    }
                                } else {
                                    // 单击: 更新选中行
                                    let inner = Rect::new(
                                        palette_area.x + 1,
                                        palette_area.y + 1,
                                        palette_area.width.saturating_sub(2),
                                        palette_area.height.saturating_sub(3),
                                    );
                                    if contains(inner, mouse.column, mouse.row) {
                                        let row = (mouse.row.saturating_sub(inner.y)) as usize;
                                        let total = app.filtered_commands().len();
                                        if total > 0 {
                                            let idx = app
                                                .view
                                                .palette_scroll
                                                .min(total - 1)
                                                .saturating_add(row);
                                            if idx < total {
                                                app.view.palette_index = idx;
                                                app.view.focus = FocusZone::Palette;
                                            }
                                        }
                                    }
                                }
                            }
                            draw_needed = true;
                            continue;
                        }

                        if contains(rows[2], mouse.column, mouse.row) {
                            set_cursor_from_mouse(&mut app, rows[2], mouse.column, mouse.row);
                            app.view.focus = FocusZone::Input;
                            app.view.select_start = None;
                            app.view.select_end = None;
                            // 记录鼠标按下时的光标位置作为选择锚点，用于拖拽选择
                            app.input.input_select_anchor = Some(app.input.cursor_pos);
                            draw_needed = true;
                        } else if in_chat {
                            app.view.select_start = Some((mouse.column, mouse.row));
                            app.view.select_end = Some((mouse.column, mouse.row));
                            app.view.selecting = true;
                            app.view.focus = FocusZone::Chat;
                            draw_needed = true;
                        } else if contains(chat_area, mouse.column, mouse.row) {
                            if let Some((_, sidebar)) = chat_layout
                                && contains(sidebar, mouse.column, mouse.row)
                            {
                                app.view.focus = FocusZone::Sidebar;
                            }
                            app.view.select_start = None;
                            app.view.select_end = None;
                            draw_needed = true;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left)
                        if app.input.input_select_anchor.is_some()
                            && contains(rows[2], mouse.column, mouse.row) =>
                    {
                        set_cursor_from_mouse(&mut app, rows[2], mouse.column, mouse.row);
                        app.view.focus = FocusZone::Input;
                        draw_needed = true;
                    }
                    MouseEventKind::Drag(MouseButton::Left) if app.view.selecting && in_chat => {
                        app.view.select_end = Some((mouse.column, mouse.row));
                        draw_needed = true;
                    }
                    MouseEventKind::Up(MouseButton::Left)
                        if app.input.input_select_anchor.is_some() =>
                    {
                        if app.input.input_select_anchor == Some(app.input.cursor_pos) {
                            app.input.input_select_anchor = None;
                        }
                        draw_needed = true;
                    }
                    MouseEventKind::Up(MouseButton::Left) if app.view.selecting => {
                        app.view.selecting = false;
                        if app.view.select_start == app.view.select_end {
                            app.view.select_start = None;
                            app.view.select_end = None;
                        }
                        draw_needed = true;
                    }
                    MouseEventKind::ScrollUp => {
                        if sidebar_w > 0 {
                            if let Some((_, sidebar)) = chat_layout {
                                if contains(sidebar, mouse.column, mouse.row) {
                                    let max_scroll = app.view.task_list.len().saturating_sub(6);
                                    app.view.sidebar_scroll =
                                        (app.view.sidebar_scroll + 2).min(max_scroll);
                                    let max_turn_scroll =
                                        app.session.turn_stats.len().saturating_sub(5);
                                    app.view.sidebar_turn_scroll =
                                        (app.view.sidebar_turn_scroll + 2).min(max_turn_scroll);
                                } else if in_chat && app.view.scroll_offset < 1000 {
                                    app.view.scroll_offset += 2;
                                    app.view.auto_scroll = false;
                                }
                            }
                        } else if contains(rows[1], mouse.column, mouse.row)
                            && app.view.scroll_offset < 1000
                        {
                            app.view.scroll_offset += 2;
                            app.view.auto_scroll = false;
                        }
                        draw_needed = true;
                    }
                    MouseEventKind::ScrollDown => {
                        if sidebar_w > 0 {
                            if let Some((_, sidebar)) = chat_layout {
                                if contains(sidebar, mouse.column, mouse.row) {
                                    app.view.sidebar_scroll =
                                        app.view.sidebar_scroll.saturating_sub(2);
                                    app.view.sidebar_turn_scroll =
                                        app.view.sidebar_turn_scroll.saturating_sub(2);
                                } else if in_chat && !app.view.auto_scroll {
                                    app.view.scroll_offset =
                                        app.view.scroll_offset.saturating_sub(2);
                                    if app.view.scroll_offset == 0 {
                                        app.view.auto_scroll = true;
                                    }
                                }
                            }
                        } else if contains(rows[1], mouse.column, mouse.row)
                            && !app.view.auto_scroll
                        {
                            app.view.scroll_offset = app.view.scroll_offset.saturating_sub(2);
                            if app.view.scroll_offset == 0 {
                                app.view.auto_scroll = true;
                            }
                        }
                        draw_needed = true;
                    }
                    _ => {}
                }
            }
            Event::Resize(..) => {
                draw_needed = true;
            }
            Event::Paste(text) if !app.view.streaming => {
                app.input.input_buf.insert_str(app.input.cursor_pos, &text);
                app.input.cursor_pos += text.len();
                app.input.history_cursor = None;
                app.input.draft_before_history = None;
                let is_multiline = text.contains('\n');
                if app.input.input_buf.starts_with('/') && !is_multiline {
                    app.view.focus = FocusZone::Palette;
                    app.clamp_palette_index(PALETTE_MAX_VISIBLE);
                } else {
                    app.view.focus = FocusZone::Input;
                }
                draw_needed = true;
            }
            _ => {}
        }
    }
}

fn ui_label(app: &App, key: &str) -> &'static str {
    let zh = matches!(app.lang, Lang::Zh);
    match key {
        "model" => {
            if zh {
                "模型"
            } else {
                "model"
            }
        }
        "mode" => {
            if zh {
                "模式"
            } else {
                "mode"
            }
        }
        "think" => {
            if zh {
                "推理"
            } else {
                "think"
            }
        }
        "lang" => {
            if zh {
                "语言"
            } else {
                "lang"
            }
        }
        "ctx" => {
            if zh {
                "上下文"
            } else {
                "ctx"
            }
        }
        "git" => {
            if zh {
                "Git"
            } else {
                "git"
            }
        }
        "cost" => {
            if zh {
                "费用"
            } else {
                "cost"
            }
        }
        "conversation" => {
            if zh {
                "对话"
            } else {
                "conversation"
            }
        }
        "reasoning" => {
            if zh {
                "推理"
            } else {
                "reasoning"
            }
        }
        "active" => {
            if zh {
                "进行中"
            } else {
                "active"
            }
        }
        "complete" => {
            if zh {
                "完成"
            } else {
                "complete"
            }
        }
        "reasoning_collapsed" => {
            if zh {
                "  已折叠 - 按 Ctrl+O 展开"
            } else {
                "  collapsed - press Ctrl+O to expand"
            }
        }
        "approval_required" => {
            if zh {
                "需要批准"
            } else {
                "approval required"
            }
        }
        "approval_hint" => {
            if zh {
                "按 Y 批准，按 N 拒绝"
            } else {
                "press Y to approve or N to deny"
            }
        }
        "placeholder" => {
            if zh {
                "编写任务、/help 或 /mode ..."
            } else {
                "Type a task, /help, or /mode ..."
            }
        }
        "run" => {
            if zh {
                "运行"
            } else {
                "run"
            }
        }
        "state" => {
            if zh {
                "状态"
            } else {
                "state"
            }
        }
        "context" => {
            if zh {
                "上下文"
            } else {
                "context"
            }
        }
        "tasks" => {
            if zh {
                "任务"
            } else {
                "tasks"
            }
        }
        "files" => {
            if zh {
                "文件"
            } else {
                "files"
            }
        }
        "session" => {
            if zh {
                "会话"
            } else {
                "session"
            }
        }
        "idle" => "idle",
        "turns" => {
            if zh {
                "轮次"
            } else {
                "turns"
            }
        }
        "tokens" => "tokens",
        "total" => {
            if zh {
                "总计"
            } else {
                "total"
            }
        }
        "hit" => {
            if zh {
                "命中"
            } else {
                "hit"
            }
        }
        _ => "",
    }
}

fn delta_stats(current: &TokenStats, start: &TokenStats) -> TokenStats {
    TokenStats {
        prompt_tokens: current.prompt_tokens.saturating_sub(start.prompt_tokens),
        completion_tokens: current
            .completion_tokens
            .saturating_sub(start.completion_tokens),
        total_tokens: current.total_tokens.saturating_sub(start.total_tokens),
        reasoning_tokens: current
            .reasoning_tokens
            .saturating_sub(start.reasoning_tokens),
        cache_hit_tokens: current
            .cache_hit_tokens
            .saturating_sub(start.cache_hit_tokens),
        cache_miss_tokens: current
            .cache_miss_tokens
            .saturating_sub(start.cache_miss_tokens),
    }
}

fn summarize_turn_input(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.len() <= 40 {
        trimmed.to_string()
    } else {
        let end = trimmed
            .char_indices()
            .take(40)
            .last()
            .map(|(i, _)| i)
            .unwrap_or(40);
        format!("{}…", &trimmed[..end])
    }
}

fn format_turn_stats_report(app: &App) -> String {
    let zh = matches!(app.lang, Lang::Zh);
    let mut lines = Vec::new();
    if zh {
        lines.push("═════ 会话轮次统计 ═════".to_string());
    } else {
        lines.push("═════ Session Turn Stats ═════".to_string());
    }
    for ts in &app.session.turn_stats {
        let summary = ts.summary.trim();
        let summary_line = if summary.is_empty() {
            None
        } else {
            // 截断避免撑爆单行 / 终端
            let preview: String = summary.chars().take(60).collect();
            Some(if summary.chars().count() > 60 {
                format!("{}…", preview)
            } else {
                preview
            })
        };
        let summary_str = summary_line
            .map(|s| {
                if zh {
                    format!("  摘要: {}", s)
                } else {
                    format!("  summary: {}", s)
                }
            })
            .unwrap_or_default();
        let prompt_total = ts.stats.cache_hit_tokens + ts.stats.cache_miss_tokens;
        let hit_str = if prompt_total > 0 {
            let pct = (ts.stats.cache_hit_tokens as f64 / prompt_total as f64) * 100.0;
            format!("{}H {}%", fmt_num(ts.stats.cache_hit_tokens), pct as u32)
        } else {
            "-".to_string()
        };
        lines.push(format!(
            "#{}  T{}  ¥{:.4}  {} {} / {} {}  {}{}",
            ts.turn,
            fmt_num(ts.stats.total_tokens),
            ts.cost_cny,
            ts.iterations,
            if zh { "次迭代" } else { "iter" },
            ts.tool_calls,
            if zh { "工具" } else { "tools" },
            hit_str,
            summary_str,
        ));
    }
    if zh {
        lines.push(format!(
            "总计: {} 轮  {} tokens ({}命中)  ¥{:.4}",
            app.session.session_turns,
            fmt_num(app.session.session_total_tokens),
            fmt_num(app.session.session_total_cache_hit),
            app.session.session_total_cost,
        ));
    } else {
        lines.push(format!(
            "Total: {} turns  {} tokens ({} hit)  ¥{:.4}",
            app.session.session_turns,
            fmt_num(app.session.session_total_tokens),
            fmt_num(app.session.session_total_cache_hit),
            app.session.session_total_cost,
        ));
    }
    lines.join("\n")
}

fn estimate_cost(stats: &TokenStats, model: &str) -> f64 {
    crate::common::pricing::calculate_cost(stats, model)
}

// P5:fmt_num / unicode_display_width / is_emoji_wide / IsCjk / *_boundary / truncate_str
// 已抽到 crate::tui_text,见 src/tui_text.rs

// P5+:StreamUpdate 已抽到 cli/types.rs(本文件是 cli/mod.rs 的占位,
// 后续会拆分出 app.rs / events.rs / render.rs / commands.rs,只保留 mod 声明 + 公共 re-export)。
use self::types::StreamUpdate;

pub async fn run_single_task(config: MovixConfig, task: String) -> Result<()> {
    use colored::*;
    use std::io::Write;

    println!("{} {}", "task:".dimmed(), task.bright_white());
    println!(
        "model: {} | think: {}",
        match config.model.as_str() {
            "deepseek-v4-pro" => "Pro".bright_cyan(),
            "deepseek-v4-flash" => "Flash".yellow(),
            other => other.normal(),
        },
        if config.thinking_enabled {
            config.reasoning_effort.to_string().bright_green()
        } else {
            "off".red()
        }
    );
    println!("{}", "-".repeat(60).dimmed());

    let mut agent = MovixAgent::new(config)?;
    // 修复(R4,关键):原实现设 AppMode::Auto,但 -t 模式从不 set_approval_channel
    // (approval_rx=None)。结果:每个写工具(write_file/patch_file/run_command)落到
    // "NeedsApproval 且无通道"分支,被永久 blocked,工具调一个失败一个,LLM 反复重试
    // 烧光 token 直到 max_iterations,用户却看到"正常退出 0"以为成功了。
    //
    // -t 是用户显式选择的"非交互单任务"模式,语义上等价于"我授权你自动执行这个任务"。
    // 因此这里用 Yolo(全自动,无审批),并在 banner 明确告知用户:所有操作将自动执行。
    // 这与 README 宣称的"non-interactive mode"契约一致。
    agent.set_mode(crate::agent::modes::AppMode::Yolo);
    println!(
        "{}",
        "[单任务模式] 已启用 Yolo(全自动执行),所有工具调用(含写文件/命令)将无需确认直接执行。"
            .yellow()
    );
    let cancel_flag = agent.cancel_flag_handle();
    agent.reset_cancel_flag();
    let reasoning_shown = Arc::new(AtomicBool::new(false));
    let rs1 = reasoning_shown.clone();
    let rs2 = reasoning_shown.clone();

    // 修复(R4 → S1,自我批判):第一轮 R4 用 select! 包裹 ctrl_c,但 select! 选中 ctrl_c 分支后
    // **直接 drop run_streaming future**,cancel_flag 设了也没人读(持有它的 future 已析构)。
    // 注释声称"等待收尾"实际没等,与原问题(析构丢状态)等价。
    //
    // 正确做法:用 pin! 固定 future,ctrl_c 分支只 set flag,然后用带超时的第二层 select
    // **真正等待** run_streaming 在检查点看到 flag 后自然返回(最多 5s)。超时才强制放弃。
    //
    // 修复(R4 → S2,编译错误):run_streaming 借 &mut agent,tokio::pin! 后 run_fut 持有该
    // &mut 借用直到所在作用域结束。原代码把 run_fut 声明在与 shutdown() 同一作用域,导致
    // shutdown() 处报"不可变借用冲突"(E0502)。改用内层 block:run_fut 在 block 结束时离开
    // 作用域被 drop,**先于** shutdown() 释放 &mut agent。
    let result = {
        let run_fut = agent.run_streaming(
            &task,
            |text| {
                print!("{}", text);
                let _ = io::stdout().flush();
            },
            move |text| {
                if !rs1.load(Ordering::Relaxed) {
                    rs1.store(true, Ordering::Relaxed);
                    print!("\n{} ", "thinking...".truecolor(180, 180, 180));
                }
                print!("{}", text.truecolor(140, 140, 140));
                let _ = io::stdout().flush();
            },
            |tool, detail, _file, output| {
                println!(
                    "\n{} {} {}",
                    "tool:".dimmed(),
                    tool.bright_cyan(),
                    detail.dimmed()
                );
                if !output.trim().is_empty() {
                    println!("{}", output.dimmed());
                }
            },
            |_, _| {},
        );
        tokio::pin!(run_fut);
        tokio::select! {
            biased;
            r = &mut run_fut => r,
            _ = tokio::signal::ctrl_c() => {
                cancel_flag.store(true, std::sync::atomic::Ordering::Release);
                eprintln!("\n{} 已请求取消,等待当前步骤收尾(最多 5s)...", "中断:".red().bold());
                // 真正等待 run_streaming 在检查点看到 cancel_flag 后自然返回。
                match tokio::time::timeout(std::time::Duration::from_secs(5), &mut run_fut).await {
                    Ok(r) => r,
                    Err(_) => {
                        eprintln!("{} 取消超时,强制退出(可能留下未完成的 MCP 请求)。", "中断:".red().bold());
                        Err(crate::common::error::MovixError::Other("用户中断 (Ctrl+C),取消超时".into()))
                    }
                }
            }
        }
    };

    match result {
        Ok(_) => {
            println!();
            // 修复(G-M2):优雅关闭 MCP 子进程,避免 zombie。
            agent.shutdown().await;
            Ok(())
        }
        Err(e) => {
            if rs2.load(Ordering::Relaxed) {
                println!();
            }
            eprintln!("{} {}", "error:".red().bold(), e);
            agent.shutdown().await;
            Err(e)
        }
    }
}
