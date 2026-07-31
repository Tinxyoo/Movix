use std::collections::HashMap;
use std::time::Instant;

pub const FAILURE_ESCALATION_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureSignal {
    SearchNotFound,
    ToolCallScavenged,
    ToolCallTruncated,
    ToolCallStorm,
    ApiError,
    LoopRecovery,
    ToolExecutionFailed,
    JsonParseFailed,
}

impl FailureSignal {
    pub fn as_str(&self) -> &'static str {
        match self {
            FailureSignal::SearchNotFound => "search-mismatch",
            FailureSignal::ToolCallScavenged => "scavenged",
            FailureSignal::ToolCallTruncated => "truncated",
            FailureSignal::ToolCallStorm => "repeat-loop",
            FailureSignal::ApiError => "api-error",
            FailureSignal::LoopRecovery => "loop-recovery",
            FailureSignal::ToolExecutionFailed => "tool-exec-failed",
            FailureSignal::JsonParseFailed => "json-parse-failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FailureRecord {
    pub signal: FailureSignal,
    pub timestamp: Instant,
    pub detail: Option<String>,
}

#[derive(Debug)]
pub struct FailureTracker {
    turn_failures: u32,
    consecutive_failures: u32,
    escalated: bool,
    threshold: u32,
    signals: Vec<FailureRecord>,
    signal_counts: HashMap<String, u32>,
    last_failure: Option<Instant>,
    total_failures: u64,
}

impl FailureTracker {
    pub fn new() -> Self {
        Self {
            turn_failures: 0,
            consecutive_failures: 0,
            escalated: false,
            threshold: FAILURE_ESCALATION_THRESHOLD,
            signals: Vec::new(),
            signal_counts: HashMap::new(),
            last_failure: None,
            total_failures: 0,
        }
    }

    pub fn with_threshold(threshold: u32) -> Self {
        Self {
            threshold,
            ..Self::new()
        }
    }

    pub fn record(&mut self, signal: FailureSignal) {
        self.record_with_detail(signal, None);
    }

    pub fn record_with_detail(&mut self, signal: FailureSignal, detail: Option<String>) {
        let key = signal.as_str().to_string();
        *self.signal_counts.entry(key.clone()).or_insert(0) += 1;

        self.signals.push(FailureRecord {
            signal,
            timestamp: Instant::now(),
            detail,
        });
        // 修复(M3-failure,无界增长):signals: Vec 只增不清(reset_turn 不清它),
        // 长会话(几千次失败)下内存线性增长。加容量上限,超限 FIFO 丢弃最旧。
        const MAX_SIGNALS: usize = 500;
        if self.signals.len() > MAX_SIGNALS {
            let drop_n = self.signals.len() - MAX_SIGNALS;
            self.signals.drain(..drop_n);
        }

        self.turn_failures += 1;
        self.consecutive_failures += 1;
        self.last_failure = Some(Instant::now());
        self.total_failures += 1;
    }

    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
    }

    pub fn should_escalate(&self) -> bool {
        // 修复(M3-failure,状态机不一致):原条件 `turn_failures >= threshold && !escalated`
        // 只看**单 turn 内**的失败次数,而 `reset_turn` 每个 turn 重置 turn_failures + escalated。
        // 结果:每 turn 失败 2 次(threshold=3)的持续失败**永远不会**升级,因为单 turn 从不
        // 达到阈值。`consecutive_failures`(跨 turn 累计)字段虽存在却是 dead field。
        // 正确语义:"单 turn 内爆发"或"跨 turn 持续失败"任一满足即应升级。
        let turn_burst = self.turn_failures >= self.threshold;
        // 跨 turn 持续失败:连续 2×threshold 个 turn 都有失败(用 consecutive_failures 累计,
        // record_success 清零)。这覆盖"慢性故障"场景。
        let sustained = self.consecutive_failures >= self.threshold.saturating_mul(2);
        (turn_burst || sustained) && !self.escalated
    }

    pub fn mark_escalated(&mut self) {
        self.escalated = true;
    }

    pub fn is_escalated(&self) -> bool {
        self.escalated
    }

    pub fn reset_turn(&mut self) {
        // 每个 turn 开始时调用:清零本 turn 计数,但**保留** consecutive_failures
        // (它跨 turn 累计,由 record_success 清零)。escalated 也清零以允许新 turn 重新判定。
        // 修复(M3-failure):原实现 reset_turn 同时清了 consecutive_failures,使其变成 dead field,
        // 跨 turn 持续失败检测失效。现在保留它。
        self.turn_failures = 0;
        self.escalated = false;
    }

    pub fn reset_all(&mut self) {
        self.turn_failures = 0;
        self.consecutive_failures = 0;
        self.escalated = false;
        self.signals.clear();
        self.signal_counts.clear();
        self.last_failure = None;
        self.total_failures = 0;
    }

    pub fn turn_failures(&self) -> u32 {
        self.turn_failures
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    pub fn total_failures(&self) -> u64 {
        self.total_failures
    }

    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    pub fn format_breakdown(&self) -> String {
        let parts: Vec<String> = self
            .signal_counts
            .iter()
            .filter(|(_, n)| **n > 0)
            .map(|(kind, n)| format!("{}× {}", n, kind))
            .collect();

        if parts.is_empty() {
            format!("{} failure signal(s)", self.turn_failures)
        } else {
            parts.join(", ")
        }
    }

    pub fn recent_signals(&self, limit: usize) -> Vec<&FailureRecord> {
        self.signals.iter().rev().take(limit).collect()
    }
}

impl Default for FailureTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_escalate() {
        let mut tracker = FailureTracker::with_threshold(3);
        assert!(!tracker.should_escalate());

        tracker.record(FailureSignal::ApiError);
        tracker.record(FailureSignal::ApiError);
        assert!(!tracker.should_escalate());

        tracker.record(FailureSignal::ApiError);
        assert!(tracker.should_escalate());

        tracker.mark_escalated();
        assert!(!tracker.should_escalate());
    }

    #[test]
    fn test_reset_turn() {
        let mut tracker = FailureTracker::with_threshold(3);
        tracker.record(FailureSignal::ApiError);
        tracker.record(FailureSignal::ApiError);
        tracker.reset_turn();
        assert_eq!(tracker.turn_failures(), 0);
        assert!(!tracker.should_escalate());
    }

    #[test]
    fn test_format_breakdown() {
        let mut tracker = FailureTracker::new();
        tracker.record(FailureSignal::ApiError);
        tracker.record(FailureSignal::SearchNotFound);
        let breakdown = tracker.format_breakdown();
        assert!(breakdown.contains("api-error"));
        assert!(breakdown.contains("search-mismatch"));
    }
}
