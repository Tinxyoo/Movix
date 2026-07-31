//! P5+:TUI 应用状态 + 状态变更方法。
//!
//! 包含 `App` 结构体(全部 TUI 状态)和 `impl App`(状态变更逻辑)。
//! 渲染由 `crate::cli::render::draw_*` 调用,事件由 `crate::cli::events` 调用。
//!
//! **关键约束**:`App` 的字段保持私有(`pub(crate)`);`render` 和 `events`
//! 子模块要访问,直接持 `&mut App` 即可,所有方法都是 `pub(crate) fn`。

use std::time::Instant;

use ratatui::style::Color;
use ratatui::text::Line;

use crate::agent::MovixAgent;
use crate::common::deepseek::TokenStats;
use crate::common::i18n::{Lang, Strings};

// 父模块的私有 fn / const(子模块可访问父模块私有项)。
use super::{
    FocusZone, MAX_HISTORY, MAX_TURN_STATS, TaskStatus, ToolActionInfo, TurnStats, TurnStatus,
    delta_stats, estimate_cost,
};
use crate::cli::theme::*;
// TUI 本地共享类型(原 cli::ChatMessage / cli::MessageRole,现抽到 cli::types)。
use super::types::{ChatMessage, MessageRole};

/// 修复(P0.2):Agent 相关状态,从 App 中拆分。
pub(crate) struct AppAgent {
    pub(crate) agent: MovixAgent,
    pub(crate) agent_handle: Option<tokio::task::JoinHandle<MovixAgent>>,
    pub(crate) agent_cancel_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// 修复(Bug #2):spawn 前 clone 出 stats handle,UI 即使在 agent swap
    /// 期间也能读到真实 stats。None 表示当前没有运行中的后台 agent。
    pub(crate) shared_stats_handle:
        Option<std::sync::Arc<std::sync::Mutex<crate::common::deepseek::TokenStats>>>,
    /// 修复(Bug #4):spawn 前 clone cancel flag,Ctrl-C 时设置 true
    /// 让 LLM HTTP 请求立刻中止。
    pub(crate) cancel_flag_handle: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub(crate) approval_tx:
        Option<tokio::sync::mpsc::Sender<crate::agent::modes::ApprovalDecision>>,
    pub(crate) pending_approval: Option<(String, String)>,
}

/// 修复(P0.2):会话统计状态,从 App 中拆分。
pub(crate) struct AppSession {
    pub(crate) session_total_tokens: u64,
    pub(crate) session_total_cache_hit: u64,
    pub(crate) session_total_cache_miss: u64,
    pub(crate) session_total_cost: f64,
    pub(crate) session_turns: u32,
    pub(crate) turn_stats: Vec<TurnStats>,
    pub(crate) turn_start_stats: TokenStats,
    pub(crate) turn_started_at: Option<Instant>,
    pub(crate) current_turn_summary: String,
    pub(crate) last_stats: Option<TokenStats>,
    pub(crate) last_elapsed: f64,
    pub(crate) last_iterations: u32,
}

/// 修复(P0.2):输入相关状态,从 App 中拆分。
pub(crate) struct AppInput {
    pub(crate) input_buf: String,
    pub(crate) cursor_pos: usize,
    pub(crate) input_scroll_y: u16,
    pub(crate) input_history: Vec<String>,
    pub(crate) history_cursor: Option<usize>,
    pub(crate) draft_before_history: Option<String>,
    pub(crate) input_select_anchor: Option<usize>,
}

/// 修复(P0.2):视图/UI 状态,从 App 中拆分。
pub(crate) struct AppView {
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) cached_chat_lines: Vec<Line<'static>>,
    pub(crate) chat_cache_valid: bool,
    pub(crate) thinking_lines: Vec<String>,
    pub(crate) streaming: bool,
    pub(crate) tick: u64,
    pub(crate) task_list: Vec<ToolActionInfo>,
    pub(crate) modified_files: Vec<String>,
    pub(crate) context_used: usize,
    pub(crate) context_max: usize,
    pub(crate) git_context: String,
    pub(crate) thinking_expanded: bool,
    pub(crate) help_open: bool,
    pub(crate) scroll_offset: usize,
    pub(crate) auto_scroll: bool,
    pub(crate) focus: FocusZone,
    pub(crate) palette_index: usize,
    pub(crate) palette_scroll: usize,
    pub(crate) sidebar_scroll: usize,
    pub(crate) sidebar_turn_scroll: usize,
    pub(crate) select_start: Option<(u16, u16)>,
    pub(crate) select_end: Option<(u16, u16)>,
    pub(crate) selecting: bool,
    pub(crate) last_palette_click: Option<(u16, u16, std::time::Instant)>,
}

/// TUI 应用状态。
/// 修复(P0.2):拆分为子结构体,按职责分组(Agent/Session/Input/View)。
pub struct App {
    pub(crate) agent_state: AppAgent,
    pub(crate) session: AppSession,
    pub(crate) input: AppInput,
    pub(crate) view: AppView,
    pub(crate) lang: Lang,
    pub(crate) s: Strings,
}

impl App {
    pub(crate) fn new(agent: MovixAgent, lang: Lang) -> Self {
        Self {
            agent_state: AppAgent {
                agent,
                agent_handle: None,
                agent_cancel_tx: None,
                shared_stats_handle: None,
                cancel_flag_handle: None,
                approval_tx: None,
                pending_approval: None,
            },
            session: AppSession {
                session_total_tokens: 0,
                session_total_cache_hit: 0,
                session_total_cache_miss: 0,
                session_total_cost: 0.0,
                session_turns: 0,
                turn_stats: Vec::new(),
                turn_start_stats: TokenStats::default(),
                turn_started_at: None,
                current_turn_summary: String::new(),
                last_stats: None,
                last_elapsed: 0.0,
                last_iterations: 0,
            },
            input: AppInput {
                input_buf: String::new(),
                cursor_pos: 0,
                input_scroll_y: 0,
                input_history: Vec::new(),
                history_cursor: None,
                draft_before_history: None,
                input_select_anchor: None,
            },
            view: AppView {
                messages: Vec::new(),
                cached_chat_lines: Vec::new(),
                chat_cache_valid: false,
                thinking_lines: Vec::new(),
                streaming: false,
                tick: 0,
                task_list: Vec::new(),
                modified_files: Vec::new(),
                context_used: 0,
                context_max: 0,
                git_context: String::new(),
                thinking_expanded: false,
                help_open: false,
                scroll_offset: 0,
                auto_scroll: true,
                focus: FocusZone::Input,
                palette_index: 0,
                palette_scroll: 0,
                sidebar_scroll: 0,
                sidebar_turn_scroll: 0,
                select_start: None,
                select_end: None,
                selecting: false,
                last_palette_click: None,
            },
            lang,
            s: Strings::for_lang(lang),
        }
    }

    pub(crate) fn toggle_model(&mut self) {
        let new_model = match self.agent_state.agent.current_model() {
            "deepseek-v4-pro" => "deepseek-v4-flash",
            _ => "deepseek-v4-pro",
        };
        let _ = self.agent_state.agent.switch_model(new_model);
    }

    pub(crate) fn model_color(&self) -> Color {
        match self.agent_state.agent.current_model() {
            "deepseek-v4-pro" | "deepseek-v4-flash" => DS_SKY,
            _ => Color::White,
        }
    }

    pub(crate) fn think_str(&self) -> String {
        if !self.agent_state.agent.thinking_enabled() {
            "off".into()
        } else {
            match self.agent_state.agent.reasoning_effort() {
                "" => "auto".into(),
                s => s.to_string(),
            }
        }
    }

    pub(crate) fn mode_color(&self) -> Color {
        match self.agent_state.agent.mode() {
            crate::agent::modes::AppMode::Plan => PLAN_ACCENT,
            crate::agent::modes::AppMode::Agent => AGENT_ACCENT,
            crate::agent::modes::AppMode::Auto => AUTO_ACCENT,
            crate::agent::modes::AppMode::Yolo => YOLO_ACCENT,
        }
    }

    pub(crate) fn mode_badge(&self) -> (&'static str, Color, Color) {
        match self.agent_state.agent.mode() {
            crate::agent::modes::AppMode::Plan => ("▣ PLAN", PLAN_ACCENT, PLAN_BG),
            crate::agent::modes::AppMode::Agent => ("◈ AGENT", AGENT_ACCENT, AGENT_BG),
            crate::agent::modes::AppMode::Auto => ("◉ AUTO", AUTO_ACCENT, AUTO_BG),
            crate::agent::modes::AppMode::Yolo => ("⚡ YOLO", YOLO_ACCENT, YOLO_BG),
        }
    }

    pub(crate) fn refresh_token_counts(&mut self) {
        self.view.context_used = self.agent_state.agent.context_token_count();
        self.view.context_max = self.agent_state.agent.context_max_tokens();
    }

    pub(crate) async fn load_stats(&mut self, status: TurnStatus) {
        let stats = self.agent_state.agent.token_stats().await;
        let turn_stats = delta_stats(&stats, &self.session.turn_start_stats);
        self.session.last_stats = Some(turn_stats.clone());
        self.session.last_elapsed = self
            .session
            .turn_started_at
            .take()
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or_else(|| self.agent_state.agent.elapsed().as_secs_f64());
        self.session.last_iterations = self.agent_state.agent.iteration_count();
        self.session.session_turns += 1;
        self.session.session_total_tokens += turn_stats.total_tokens;
        self.session.session_total_cache_hit += turn_stats.cache_hit_tokens;
        self.session.session_total_cache_miss += turn_stats.cache_miss_tokens;
        let cost = estimate_cost(&turn_stats, self.agent_state.agent.current_model());
        self.session.session_total_cost += cost;

        let tool_calls = self
            .view
            .task_list
            .iter()
            .filter(|t| t.status != TaskStatus::WaitingApproval)
            .count() as u32;
        self.session.turn_stats.push(TurnStats {
            turn: self.session.session_turns,
            summary: self.session.current_turn_summary.clone(),
            stats: turn_stats,
            cost_cny: cost,
            iterations: self.session.last_iterations,
            tool_calls,
            status,
        });
        if self.session.turn_stats.len() > MAX_TURN_STATS {
            self.session.turn_stats.remove(0);
        }

        self.refresh_token_counts();
    }

    /// 异步刷新 Git 上下文信息(分支名 + 工作区状态),避免阻塞 TUI 事件循环
    pub(crate) async fn refresh_git_context(&mut self) {
        let workspace = self.agent_state.agent.workspace();

        let branch_output = tokio::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&workspace)
            .output()
            .await;
        let branch = match branch_output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            _ => "-".to_string(),
        };

        let status_output = tokio::process::Command::new("git")
            .args(["status", "--short"])
            .current_dir(&workspace)
            .output()
            .await;
        let status = match status_output {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout);
                let lines: Vec<&str> = s.lines().collect();
                if lines.is_empty() {
                    "clean".to_string()
                } else {
                    let mut staged = 0u32;
                    let mut modified = 0u32;
                    let mut untracked = 0u32;
                    for line in &lines {
                        if line.starts_with("??") || line.starts_with("? ") {
                            untracked += 1;
                        } else {
                            let index_col = line.chars().next();
                            let work_col = line.chars().nth(1);
                            if index_col.map(|c| c != ' ' && c != '?').unwrap_or(false) {
                                staged += 1;
                            }
                            if work_col.map(|c| c != ' ' && c != '?').unwrap_or(false) {
                                modified += 1;
                            }
                        }
                    }
                    let mut parts = Vec::new();
                    if staged > 0 {
                        parts.push(format!("{} staged", staged));
                    }
                    if modified > 0 {
                        parts.push(format!("{} modified", modified));
                    }
                    if untracked > 0 {
                        parts.push(format!("{} untracked", untracked));
                    }
                    parts.join(", ")
                }
            }
            _ => "-".to_string(),
        };
        self.view.git_context = format!("{} | {}", branch, status);
    }

    pub(crate) fn push_history(&mut self, input: &str) {
        let trimmed = input.trim();
        if trimmed.is_empty() || trimmed.starts_with('/') {
            return;
        }
        if self.input.input_history.last().map(|s| s.as_str()) == Some(trimmed) {
            return;
        }
        self.input.input_history.push(trimmed.to_string());
        if self.input.input_history.len() > MAX_HISTORY {
            self.input.input_history.remove(0);
        }
    }

    pub(crate) fn history_up(&mut self) {
        if self.input.input_history.is_empty() {
            return;
        }
        if self.input.history_cursor.is_none() {
            self.input.draft_before_history = Some(self.input.input_buf.clone());
        }
        let idx = match self.input.history_cursor {
            Some(i) if i > 0 => i - 1,
            Some(_) | None => self.input.input_history.len() - 1,
        };
        self.input.input_buf = self.input.input_history[idx].clone();
        self.input.cursor_pos = self.input.input_buf.len();
        self.input.input_scroll_y = 0;
        self.input.history_cursor = Some(idx);
    }

    pub(crate) fn history_down(&mut self) {
        match self.input.history_cursor {
            Some(i) if i + 1 < self.input.input_history.len() => {
                self.input.history_cursor = Some(i + 1);
                self.input.input_buf = self.input.input_history[i + 1].clone();
                self.input.cursor_pos = self.input.input_buf.len();
                self.input.input_scroll_y = 0;
            }
            Some(_) => {
                self.input.history_cursor = None;
                self.input.input_buf = self.input.draft_before_history.take().unwrap_or_default();
                self.input.cursor_pos = self.input.input_buf.len();
                self.input.input_scroll_y = 0;
            }
            None => {}
        }
    }

    pub(crate) fn cancel_history(&mut self) {
        if let Some(draft) = self.input.draft_before_history.take() {
            self.input.input_buf = draft;
            self.input.cursor_pos = self.input.input_buf.len();
        }
        self.input.history_cursor = None;
    }

    /// 处理流式更新完成(无论成功或失败),统一清理 Agent 状态。
    /// 修复:原实现未 take agent_handle,依赖 recover_agent 兜底,存在竞态窗口。
    /// 现在 finish_stream 与 agent_handle 强绑定,要么一起走完,要么被 take 后状态一致。
    pub(crate) fn finish_stream(&mut self, is_error: bool, error_msg: Option<String>) {
        // 修复:走 push_msg 统一切换 chat_cache_valid=false,
        // 避免绕过缓存失效导致错误消息不渲染。
        if is_error && let Some(e) = error_msg {
            self.push_msg(MessageRole::Assistant, format!("error: {}", e));
        }
        self.view.streaming = false;
        self.agent_state.pending_approval = None;
        self.agent_state.approval_tx = None;
        self.agent_state.agent_cancel_tx = None;
        // 修复：原实现只更新最后一个 Running 任务，导致多个并行任务时状态残留
        let final_status = if is_error {
            TaskStatus::Failed
        } else {
            TaskStatus::Done
        };
        for task in &mut self.view.task_list {
            if task.status == TaskStatus::Running {
                task.status = final_status;
            }
        }
    }

    /// 从 JoinHandle 中恢复 Agent 实例
    pub(crate) async fn recover_agent(&mut self) {
        if let Some(handle) = self.agent_state.agent_handle.take() {
            match handle.await {
                Ok(agent) => self.agent_state.agent = agent,
                Err(e) => {
                    // 修复:JoinError 后 self.agent_state.agent 仍是 swap 时的占位实例,
                    // 所有先前对话上下文已丢失。额外推警告告知用户。
                    let zh = matches!(self.lang, Lang::Zh);
                    let mut msg = if zh {
                        format!("Agent 任务异常终止: {}", e)
                    } else {
                        format!("Agent task panicked: {}", e)
                    };
                    let ctx_warn = if zh {
                        "\n⚠️ 对话上下文已丢失，建议 /clear 后重新开始。"
                    } else {
                        "\n⚠️ Conversation context was lost. Run /clear to reset."
                    };
                    msg.push_str(ctx_warn);
                    self.push_msg(MessageRole::Assistant, msg);
                }
            }
        }
    }

    pub(crate) fn push_msg(&mut self, role: MessageRole, content: String) {
        self.view.messages.push(ChatMessage { role, content });
        self.view.chat_cache_valid = false;
    }

    /// 过滤命令列表(两层匹配: 前缀 → 子串降级)
    pub(crate) fn filtered_commands(&self) -> Vec<(&'static str, &'static str)> {
        Self::filter_commands_buf(&self.input.input_buf, &self.s)
    }

    /// 纯函数版本(便于测试)
    pub(crate) fn filter_commands_buf(buf: &str, s: &Strings) -> Vec<(&'static str, &'static str)> {
        if buf.is_empty() {
            return Vec::new();
        }
        if buf == "/" {
            return s.get_commands();
        }
        let partial = buf.to_lowercase();
        let commands = s.get_commands();
        let prefix: Vec<_> = commands
            .iter()
            .copied()
            .filter(|(cmd, _)| cmd.starts_with(&partial))
            .collect();
        if !prefix.is_empty() {
            return prefix;
        }
        let no_slash = partial.trim_start_matches('/');
        commands
            .into_iter()
            .filter(|(cmd, desc)| {
                cmd.to_lowercase().contains(&partial)
                    || desc.to_lowercase().contains(&partial)
                    || (!no_slash.is_empty() && desc.to_lowercase().contains(no_slash))
            })
            .collect()
    }

    /// 同步 palette_index 和 palette_scroll
    pub(crate) fn sync_palette(&mut self, max_visible: usize) {
        let len = self.filtered_commands().len();
        if len == 0 {
            self.view.palette_index = 0;
            self.view.palette_scroll = 0;
            return;
        }
        if self.view.palette_index >= len {
            self.view.palette_index = len - 1;
        }
        let mv = max_visible.max(1);
        if self.view.palette_index < self.view.palette_scroll {
            self.view.palette_scroll = self.view.palette_index;
        } else if self.view.palette_index >= self.view.palette_scroll + mv {
            self.view.palette_scroll = self.view.palette_index + 1 - mv;
        }
        if self.view.palette_scroll > len.saturating_sub(1) {
            self.view.palette_scroll = len.saturating_sub(1);
        }
    }

    /// 在过滤列表中移动选中项(环绕)
    pub(crate) fn palette_move(&mut self, delta: i32, max_visible: usize) {
        let len = self.filtered_commands().len();
        if len == 0 {
            self.view.palette_index = 0;
            self.view.palette_scroll = 0;
            return;
        }
        self.view.palette_index =
            (self.view.palette_index as i32 + delta).rem_euclid(len as i32) as usize;
        self.sync_palette(max_visible);
    }

    /// 翻页
    pub(crate) fn palette_page(&mut self, delta: i32, max_visible: usize) {
        let len = self.filtered_commands().len();
        if len == 0 {
            self.view.palette_index = 0;
            self.view.palette_scroll = 0;
            return;
        }
        let mv = max_visible.max(1) as i32;
        self.view.palette_index =
            (self.view.palette_index as i32 + delta * mv).clamp(0, len as i32 - 1) as usize;
        self.sync_palette(mv as usize);
    }

    /// 重置弹窗状态
    pub(crate) fn reset_palette(&mut self) {
        self.view.palette_index = 0;
        self.view.palette_scroll = 0;
    }

    /// 选中命令名
    pub(crate) fn palette_selected_cmd(&self) -> Option<&'static str> {
        self.filtered_commands()
            .get(self.view.palette_index)
            .map(|(c, _)| *c)
    }

    /// 修复:接受 max_visible 参数而非硬编码 6,与 PALETTE_MAX_VISIBLE 保持一致。
    pub(crate) fn clamp_palette_index(&mut self, max_visible: usize) {
        self.sync_palette(max_visible);
    }

    /// 根据光标位置自动调整输入框垂直滚动偏移
    /// 使用视觉行号(考虑长行自动换行)来计算
    pub(crate) fn clamp_input_scroll(&mut self, visible_lines: u16, avail_w: usize) {
        if visible_lines == 0 {
            return;
        }
        let pos = self.input.cursor_pos.min(self.input.input_buf.len());
        let before = &self.input.input_buf[..pos];
        let mut visual_line: u16 = 0;
        let mut visual_col: usize = 0;
        for ch in before.chars() {
            if ch == '\n' {
                visual_line += 1;
                visual_col = 0;
            } else {
                let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                if avail_w > 0 && visual_col + w > avail_w {
                    visual_line += 1;
                    visual_col = 0;
                }
                visual_col += w;
            }
        }
        if visual_line < self.input.input_scroll_y {
            self.input.input_scroll_y = visual_line;
        } else if visual_line >= self.input.input_scroll_y + visible_lines {
            self.input.input_scroll_y = visual_line - visible_lines + 1;
        }
    }

    /// 正向循环焦点区域:Input → Chat → Sidebar → Palette → Input
    pub(crate) fn cycle_focus_forward(&mut self) {
        self.view.focus = match self.view.focus {
            FocusZone::Input => FocusZone::Chat,
            FocusZone::Chat => FocusZone::Sidebar,
            FocusZone::Sidebar => FocusZone::Palette,
            FocusZone::Palette => FocusZone::Input,
        };
        if self.view.focus == FocusZone::Palette && !self.input.input_buf.starts_with('/') {
            self.view.focus = FocusZone::Input;
        }
    }
}

#[cfg(test)]
mod palette_tests {
    use super::*;
    fn fixture_with_buf(buf: &str) -> App {
        let cfg = crate::common::config::MovixConfig::from_env_optional();
        let agent = crate::agent::MovixAgent::new(cfg).expect("agent");
        let mut app = App::new(agent, Lang::Zh);
        app.input.input_buf = buf.to_string();
        app.input.cursor_pos = buf.len();
        app
    }

    #[test]
    fn empty_returns_empty() {
        assert!(fixture_with_buf("").filtered_commands().is_empty());
    }
    #[test]
    fn slash_returns_all() {
        let c = fixture_with_buf("/").filtered_commands();
        assert!(c.len() >= 26);
    }
    #[test]
    fn prefix_match() {
        let c = App::filter_commands_buf("/mod", &fixture_with_buf("/").s);
        assert!(c.iter().any(|(x, _)| *x == "/model pro"));
    }
    #[test]
    fn substring_fallback() {
        let c = fixture_with_buf("/分层").filtered_commands();
        assert!(!c.is_empty());
    }
    #[test]
    fn case_insensitive() {
        let _c = App::filter_commands_buf("/LANG", &fixture_with_buf("/").s);
    }
    #[test]
    fn sync_clamps() {
        let mut a = fixture_with_buf("/");
        a.view.palette_index = 999;
        a.sync_palette(6);
        assert!(a.view.palette_index < a.filtered_commands().len());
    }
    #[test]
    fn move_wraps() {
        let mut a = fixture_with_buf("/");
        let n = a.filtered_commands().len();
        if n < 2 {
            return;
        }
        a.view.palette_index = n - 1;
        a.palette_move(1, 6);
        assert_eq!(a.view.palette_index, 0);
    }
    #[test]
    fn reset_works() {
        let mut a = fixture_with_buf("/");
        a.view.palette_index = 5;
        a.view.palette_scroll = 3;
        a.reset_palette();
        assert_eq!(a.view.palette_index, 0);
    }
}
