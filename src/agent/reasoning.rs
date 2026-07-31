use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ReasoningEffort {
    Off,
    Low,
    #[default]
    Medium,
    High,
    Max,
}

impl ReasoningEffort {
    pub fn parse_effort(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "off" | "disabled" => ReasoningEffort::Off,
            "low" | "auto" => ReasoningEffort::Low,
            "medium" | "default" => ReasoningEffort::Medium,
            "high" => ReasoningEffort::High,
            "max" | "ultra" => ReasoningEffort::Max,
            _ => ReasoningEffort::Medium,
        }
    }

    pub fn to_api_string(&self) -> &'static str {
        match self {
            ReasoningEffort::Off => "",
            ReasoningEffort::Low => "auto",
            ReasoningEffort::Medium => "auto",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "max",
        }
    }

    pub fn budget_multiplier(&self) -> f64 {
        match self {
            ReasoningEffort::Off => 0.0,
            ReasoningEffort::Low => 0.5,
            ReasoningEffort::Medium => 1.0,
            ReasoningEffort::High => 2.0,
            ReasoningEffort::Max => 4.0,
        }
    }

    pub fn thinking_token_limit(&self, base: usize) -> usize {
        match self {
            ReasoningEffort::Off => 0,
            ReasoningEffort::Low => base / 4,
            ReasoningEffort::Medium => base / 2,
            ReasoningEffort::High => base,
            ReasoningEffort::Max => base * 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningBudget {
    pub max_thinking_tokens: usize,
    pub max_thinking_seconds: u64,
    pub truncation_threshold: f64,
}

impl Default for ReasoningBudget {
    fn default() -> Self {
        Self {
            max_thinking_tokens: 32000,
            max_thinking_seconds: 120,
            truncation_threshold: 0.8,
        }
    }
}

impl ReasoningBudget {
    pub fn for_effort(effort: ReasoningEffort) -> Self {
        match effort {
            ReasoningEffort::Off => Self {
                max_thinking_tokens: 0,
                max_thinking_seconds: 0,
                truncation_threshold: 1.0,
            },
            ReasoningEffort::Low => Self {
                max_thinking_tokens: 8000,
                max_thinking_seconds: 30,
                truncation_threshold: 0.9,
            },
            ReasoningEffort::Medium => Self {
                max_thinking_tokens: 16000,
                max_thinking_seconds: 60,
                truncation_threshold: 0.8,
            },
            ReasoningEffort::High => Self {
                max_thinking_tokens: 32000,
                max_thinking_seconds: 120,
                truncation_threshold: 0.7,
            },
            ReasoningEffort::Max => Self {
                max_thinking_tokens: 64000,
                max_thinking_seconds: 240,
                truncation_threshold: 0.5,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReasoningStats {
    pub iterations: usize,
    pub total_thinking_chars: usize,
    pub total_response_chars: usize,
    pub tool_calls: usize,
    pub start_time: Instant,
    pub reasoning_time_ms: u64,
    /// 修复(Low-reasoning):turn 开始时刻,**不在 start_iteration 中重置**。
    /// 用于 should_truncate_thinking 判断整个 turn 的思考时长是否超限。
    /// 原 start_time 被 start_iteration 每次覆盖,导致 max_thinking_seconds 实际
    /// 衡量的是"单次迭代耗时"而非"整个 turn 思考耗时",长思考无法被正确熔断。
    pub turn_start_time: Instant,
}

impl Default for ReasoningStats {
    fn default() -> Self {
        Self {
            iterations: 0,
            total_thinking_chars: 0,
            total_response_chars: 0,
            tool_calls: 0,
            start_time: Instant::now(),
            reasoning_time_ms: 0,
            turn_start_time: Instant::now(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReasoningController {
    effort: ReasoningEffort,
    budget: ReasoningBudget,
    stats: ReasoningStats,
    is_active: bool,
}

impl ReasoningController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_effort(effort: ReasoningEffort) -> Self {
        Self {
            effort,
            budget: ReasoningBudget::for_effort(effort),
            stats: ReasoningStats::default(),
            is_active: effort != ReasoningEffort::Off,
        }
    }

    pub fn set_effort(&mut self, effort: ReasoningEffort) {
        self.effort = effort;
        self.budget = ReasoningBudget::for_effort(effort);
        self.is_active = effort != ReasoningEffort::Off;
    }

    pub fn effort(&self) -> ReasoningEffort {
        self.effort
    }

    pub fn budget(&self) -> &ReasoningBudget {
        &self.budget
    }

    pub fn is_active(&self) -> bool {
        self.is_active
    }

    pub fn start_iteration(&mut self) {
        self.stats.iterations += 1;
        // 修复(Low-reasoning):不再覆盖 start_time,否则 should_truncate_thinking
        // 用 start_time.elapsed() 判断的是"单次迭代耗时"而非整个 turn 思考时长。
        // turn_start_time 在 turn 开始时由调用方(reset_turn / new)设置,这里不动。
        self.stats.start_time = self.stats.turn_start_time;
    }

    /// 标记一个新 turn 开始:重置 turn_start_time,作为思考时长计时的基准。
    pub fn start_turn(&mut self) {
        let now = Instant::now();
        self.stats.turn_start_time = now;
        self.stats.start_time = now;
    }

    pub fn record_thinking(&mut self, chars: usize) {
        self.stats.total_thinking_chars += chars;
    }

    pub fn record_response(&mut self, chars: usize) {
        self.stats.total_response_chars += chars;
    }

    pub fn record_tool_call(&mut self) {
        self.stats.tool_calls += 1;
    }

    pub fn stats(&self) -> &ReasoningStats {
        &self.stats
    }

    pub fn stats_mut(&mut self) -> &mut ReasoningStats {
        &mut self.stats
    }

    pub fn should_truncate_thinking(&self) -> bool {
        if !self.is_active {
            return false;
        }
        // 修复(Bug #13):max_thinking_tokens=0 时除 0 得 inf/NaN,
        // NaN 比较恒为 false,但配置传 0 立刻坏。显式早返回。
        if self.budget.max_thinking_tokens == 0 {
            return false;
        }

        let elapsed = self.stats.turn_start_time.elapsed();
        let time_exceeded = elapsed.as_secs() > self.budget.max_thinking_seconds;
        // 注:char-based 阈值 `total_thinking_chars / (max_tokens/4)` 用 "4 char≈1 token"
        // 的 ASCII 假设,对 CJK 会略偏低(中文 1 字≈1.4 token),中文思考链可能略早触发
        // 截断。彻底修复需接入 DeepSeek 官方 tokenizer;当前与 time_exceeded 取或,
        // 时间维度对 CJK/ASCII 一致,作为主要熔断依据。
        let denom = (self.budget.max_thinking_tokens as f64 / 4.0).max(1.0);
        let threshold = self.stats.total_thinking_chars as f64 / denom;

        time_exceeded || threshold > self.budget.truncation_threshold
    }

    pub fn should_extend_thinking(&self) -> bool {
        if !self.is_active {
            return false;
        }

        let chars_per_iter = self
            .stats
            .total_thinking_chars
            .checked_div(self.stats.iterations)
            .unwrap_or(0);

        chars_per_iter < 500 && self.stats.tool_calls < 2
    }

    pub fn estimate_remaining_budget(&self) -> usize {
        let used = self.stats.total_thinking_chars / 4;
        self.budget.max_thinking_tokens.saturating_sub(used)
    }

    pub fn truncate_reasoning(&self, reasoning: &str) -> String {
        if !self.should_truncate_thinking() {
            return reasoning.to_string();
        }

        let limit =
            (self.budget.max_thinking_tokens as f64 * self.budget.truncation_threshold) as usize;

        // 先用 char_indices 找到截断点的字节偏移，避免收集整个 Vec<char>
        let boundary = reasoning
            .char_indices()
            .nth(limit)
            .map(|(i, _)| i)
            .unwrap_or(reasoning.len());

        if boundary == reasoning.len() {
            return reasoning.to_string();
        }

        format!(
            "{}...\n[思考链已达预算限制，已截断]",
            &reasoning[..boundary]
        )
    }

    pub fn summary(&self) -> ReasoningSummary {
        // 修复(Low-reasoning):用 turn_start_time 报告"整个 turn 的思考耗时",
        // 而非 start_time(被 start_iteration 覆盖,只反映最后一次迭代的时长)。
        let elapsed = self.stats.turn_start_time.elapsed();
        ReasoningSummary {
            effort: self.effort,
            iterations: self.stats.iterations,
            thinking_chars: self.stats.total_thinking_chars,
            response_chars: self.stats.total_response_chars,
            tool_calls: self.stats.tool_calls,
            elapsed_secs: elapsed.as_secs_f64(),
            thinking_token_budget: self.budget.max_thinking_tokens,
            budget_used_ratio: self.stats.total_thinking_chars as f64
                / (self.budget.max_thinking_tokens as f64 / 4.0).max(1.0),
        }
    }

    pub fn reset(&mut self) {
        self.stats = ReasoningStats::default();
    }
}

impl Default for ReasoningController {
    fn default() -> Self {
        Self::with_effort(ReasoningEffort::Medium)
    }
}

const HIGH_EFFORT_KEYWORDS: &[&str] = &[
    "debug",
    "error",
    "crash",
    "segfault",
    "panic",
    "stack overflow",
    "调试",
    "错误",
    "报错",
    "出错",
    "崩溃",
    "段错误",
    "死锁",
    "内存泄漏",
    "デバッグ",
    "エラー",
    "バグ",
    "クラッシュ",
];

const LOW_EFFORT_KEYWORDS: &[&str] = &[
    "search", "lookup", "find", "list", "show", "cat", "搜索", "查找", "查询", "列出", "显示",
    "查看", "検索", "一覧",
];

const MEDIUM_EFFORT_KEYWORDS: &[&str] = &[
    "refactor",
    "optimize",
    "implement",
    "design",
    "重构",
    "优化",
    "实现",
    "设计",
    "架构",
    "リファクタ",
    "最適化",
    "実装",
];

impl ReasoningController {
    pub fn select_for_message(&self, message: &str, is_subagent: bool) -> ReasoningEffort {
        if is_subagent {
            return ReasoningEffort::Low;
        }

        let lower = message.to_ascii_lowercase();

        // 修复(R5/C5,关键):原实现把整个 message 当作用户意图做关键词匹配。但 message
        // 可能是被注入的内容(粘贴的源码文件、web_fetch 拉回的网页、工具结果摘要),
        // 其中 "error"/"panic"/"debug"/"crash" 极常见——一次注入即可让每轮都升 Max
        // (64000 thinking token,~4x 成本),且通过 rebuild_llm_client 持久化。
        //
        // 防御:仅对**短消息**(用户实际输入的指令)做关键词升级;长内容(>500 字符,
        // 几乎一定是粘贴的文件/网页)不因关键词升级到 High/Max,避免注入烧钱。
        // 强信号(crash/段错误)阈值更严:短消息才认。
        const KEYWORD_UPGRADE_MAX_LEN: usize = 500;
        let do_keyword_upgrade = lower.len() <= KEYWORD_UPGRADE_MAX_LEN;

        // 修复(S12):原实现任一 HIGH_EFFORT_KEYWORDS 命中即跳 Max。但表里含 "error"/"bug"/
        // "错误"/"报错" 这类极常见词,用户随口一句"这里报错了"就触发 Max(64000 thinking
        // token,是 Medium 的 4 倍),可被恶意 prompt 用常见词诱导烧钱。
        //
        // 改为**分级评分**:把原 HIGH 表拆成"强信号"(crash/segfault/死锁/内存泄漏——单命中
        // 即 Max)与"弱信号"(error/bug/错误/报错/debug——需累计 ≥2 个,或与强信号叠加,
        // 才升 Max)。这样日常报错只升到 High,真正严重的崩溃才上 Max。
        const STRONG_SIGNALS: &[&str] = &[
            "crash",
            "segfault",
            "stack overflow",
            "崩溃",
            "段错误",
            "死锁",
            "内存泄漏",
            "クラッシュ",
        ];
        const WEAK_SIGNALS: &[&str] = &[
            "debug",
            "error",
            "panic",
            "调试",
            "错误",
            "报错",
            "出错",
            "デバッグ",
            "エラー",
            "バグ",
        ];

        if do_keyword_upgrade {
            let strong_hits = STRONG_SIGNALS
                .iter()
                .filter(|kw| lower.contains(**kw))
                .count();
            let weak_hits = WEAK_SIGNALS
                .iter()
                .filter(|kw| lower.contains(**kw))
                .count();

            // 强信号任一命中 → Max;或弱信号累计 ≥2 → Max(多个报错信号说明真复杂)。
            if strong_hits >= 1 || weak_hits >= 2 {
                return ReasoningEffort::Max;
            }
            // 单个弱信号 → High(不直接跳 Max)。
            if weak_hits >= 1 {
                return ReasoningEffort::High;
            }

            for kw in HIGH_EFFORT_KEYWORDS {
                if lower.contains(*kw) {
                    return ReasoningEffort::High;
                }
            }

            for kw in MEDIUM_EFFORT_KEYWORDS {
                if lower.contains(*kw) {
                    return ReasoningEffort::High;
                }
            }

            for kw in LOW_EFFORT_KEYWORDS {
                if lower.contains(*kw) {
                    return ReasoningEffort::Low;
                }
            }
        }

        // 修复(R5/C5):原 fallthrough 是 High(32000 thinking token)。"auto" 本意是
        // "sensible default"(见 README),但 neutral 消息(hello/解释项目)默认 High
        // 导致用户静默支付高推理成本。改为 Medium(16000),与文档语义一致。
        // (长消息/无关键词命中也走这里。)
        ReasoningEffort::Medium
    }

    pub fn auto_adjust(&mut self, message: &str, is_subagent: bool) -> bool {
        let new_effort = self.select_for_message(message, is_subagent);
        if new_effort != self.effort {
            self.set_effort(new_effort);
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ReasoningSummary {
    pub effort: ReasoningEffort,
    pub iterations: usize,
    pub thinking_chars: usize,
    pub response_chars: usize,
    pub tool_calls: usize,
    pub elapsed_secs: f64,
    pub thinking_token_budget: usize,
    pub budget_used_ratio: f64,
}

impl std::fmt::Display for ReasoningSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "推理摘要 [{}]: {}次迭代, {}思考字符, {}响应字符, {}工具调用, {:.1}s",
            self.effort.to_api_string(),
            self.iterations,
            self.thinking_chars,
            self.response_chars,
            self.tool_calls,
            self.elapsed_secs
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_effort_from_str() {
        assert_eq!(ReasoningEffort::parse_effort("max"), ReasoningEffort::Max);
        assert_eq!(ReasoningEffort::parse_effort("high"), ReasoningEffort::High);
        assert_eq!(ReasoningEffort::parse_effort("auto"), ReasoningEffort::Low);
        assert_eq!(ReasoningEffort::parse_effort("OFF"), ReasoningEffort::Off);
    }

    #[test]
    fn test_budget_for_effort() {
        let max_budget = ReasoningBudget::for_effort(ReasoningEffort::Max);
        assert_eq!(max_budget.max_thinking_tokens, 64000);

        let off_budget = ReasoningBudget::for_effort(ReasoningEffort::Off);
        assert_eq!(off_budget.max_thinking_tokens, 0);
    }

    #[test]
    fn test_truncation() {
        let mut controller = ReasoningController::with_effort(ReasoningEffort::Low);
        controller.stats_mut().total_thinking_chars = 20000;

        assert!(controller.should_truncate_thinking());
    }
}
