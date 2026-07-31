use crate::common::deepseek::ChatMessage;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageTier {
    Critical,
    Important,
    Normal,
    Summary,
}

impl MessageTier {
    pub fn priority(&self) -> u8 {
        match self {
            MessageTier::Critical => 3,
            MessageTier::Important => 2,
            MessageTier::Normal => 1,
            MessageTier::Summary => 0,
        }
    }
}

/// 统一的 `MessageTier -> tiers 数组下标` 转换函数,避免别处
/// 直接 `tier as usize` 引入与 `priority()` 顺序不一致的 bug。
fn tier_to_index(tier: MessageTier) -> usize {
    match tier {
        MessageTier::Critical => 0,
        MessageTier::Important => 1,
        MessageTier::Normal => 2,
        MessageTier::Summary => 3,
    }
}

#[derive(Debug, Clone)]
pub struct TieredMessage {
    pub message: ChatMessage,
    pub tier: MessageTier,
    pub tokens: usize,
    /// 全局单调递增的插入序号,用于 `messages()` 时按时间顺序还原,
    /// 而非按层输出——后者会打散 tool_call ↔ tool_result 配对导致 API 400。
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierConfig {
    pub critical_keep: usize,
    /// Normal/Important/Summary 在 evict 时保留的最少**消息条数**下限。
    pub important_keep: usize,
    /// Normal tier 保留的最少消息条数(以前是字符预算 normal_truncate_chars,
    /// 被 evict_lowest_priority 误用为消息条数下限,导致 Normal 永不驱逐)。
    pub normal_keep: usize,
    pub summary_keep: usize,
    /// 单条 reasoning_content 的字符上限,用于 evict_reasoning_content 截断。
    pub thinking_budget_chars: usize,
    /// Normal tier 单条消息的字符截断预算(原 normal_truncate_chars 的真实语义)。
    pub normal_truncate_chars: usize,
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            critical_keep: 50,
            important_keep: 20,
            normal_keep: 30,
            summary_keep: 10,
            thinking_budget_chars: 6000,
            normal_truncate_chars: 8000,
        }
    }
}

impl TierConfig {
    pub fn high_effort() -> Self {
        Self {
            critical_keep: 100,
            important_keep: 40,
            normal_keep: 60,
            summary_keep: 20,
            thinking_budget_chars: 10000,
            normal_truncate_chars: 12000,
        }
    }

    pub fn low_effort() -> Self {
        Self {
            critical_keep: 20,
            important_keep: 10,
            normal_keep: 15,
            summary_keep: 5,
            thinking_budget_chars: 3000,
            normal_truncate_chars: 4000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TieredContextWindow {
    system_prompt: ChatMessage,
    tiers: [VecDeque<TieredMessage>; 4],
    estimated_tokens: usize,
    max_tokens: usize,
    config: TierConfig,
    /// 全局单调递增序号,保证 tool_call 与其 tool_result 在 `messages()` 中顺序不变。
    next_seq: u64,
}

impl TieredContextWindow {
    pub fn new(system_prompt: String, max_tokens: usize) -> Self {
        let estimated = Self::estimate_tokens_str(&system_prompt);
        Self {
            system_prompt: ChatMessage::system(system_prompt),
            tiers: [
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::new(),
            ],
            estimated_tokens: estimated,
            max_tokens,
            config: TierConfig::default(),
            next_seq: 0,
        }
    }

    pub fn with_config(system_prompt: String, max_tokens: usize, config: TierConfig) -> Self {
        let estimated = Self::estimate_tokens_str(&system_prompt);
        Self {
            system_prompt: ChatMessage::system(system_prompt),
            tiers: [
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::new(),
            ],
            estimated_tokens: estimated,
            max_tokens,
            config,
            next_seq: 0,
        }
    }

    pub fn push(&mut self, message: ChatMessage, tier: MessageTier) {
        let tokens = Self::estimate_tokens_message(&message);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.tiers[tier_to_index(tier)].push_back(TieredMessage {
            message,
            tier,
            tokens,
            seq,
        });
        self.estimated_tokens += tokens;
        self.compress_if_needed();
    }

    pub fn push_critical(&mut self, message: ChatMessage) {
        self.push(message, MessageTier::Critical);
    }

    pub fn push_important(&mut self, message: ChatMessage) {
        self.push(message, MessageTier::Important);
    }

    pub fn push_normal(&mut self, message: ChatMessage) {
        self.push(message, MessageTier::Normal);
    }

    pub fn push_summary(&mut self, message: ChatMessage) {
        self.push(message, MessageTier::Summary);
    }

    pub fn messages(&self) -> Vec<ChatMessage> {
        // 修复(Critical #C3):此前按层(Critical→Important→Normal→Summary)顺序输出,
        // 但 auto_tier_message 把 assistant(tool_calls) 归入 Critical、tool 结果归入
        // Normal,于是 tool_call 与其 tool_result 被拆散到不同层,中间隔着整层消息。
        // 发给 API 时 tool 结果不再紧跟 tool_call → 400 报错。
        // 现在按插入时的全局 seq 排序,恢复时间顺序,tier 仅用于驱逐优先级。
        let mut all = Vec::new();
        all.push(self.system_prompt.clone());

        let mut buffered: Vec<&TieredMessage> = Vec::new();
        for tier in &self.tiers {
            for tm in tier {
                buffered.push(tm);
            }
        }
        buffered.sort_by_key(|tm| tm.seq);
        for tm in buffered {
            all.push(tm.message.clone());
        }

        all
    }

    pub fn messages_with_metadata(&self) -> Vec<(ChatMessage, MessageTier)> {
        let mut all = Vec::new();
        all.push((self.system_prompt.clone(), MessageTier::Critical));

        let mut buffered: Vec<&TieredMessage> = Vec::new();
        for tier in &self.tiers {
            for tm in tier {
                buffered.push(tm);
            }
        }
        buffered.sort_by_key(|tm| tm.seq);
        for tm in buffered {
            all.push((tm.message.clone(), tm.tier));
        }

        all
    }

    pub fn update_config(&mut self, config: TierConfig) {
        self.config = config;
        self.compress_if_needed();
    }

    pub fn update_system(&mut self, new_prompt: String) {
        // 修复(Medium):此前只减 content 的 token,不减 reasoning/tool_calls,
        // 与 estimate_tokens_message 的统计口径不一致,长期会漂移。
        let old_tokens = Self::estimate_tokens_message(&self.system_prompt);
        self.estimated_tokens = self.estimated_tokens.saturating_sub(old_tokens);
        self.system_prompt = ChatMessage::system(new_prompt);
        self.estimated_tokens += Self::estimate_tokens_message(&self.system_prompt);
    }

    pub fn set_reasoning_effort(&mut self, effort: &str) {
        self.config = match effort {
            "max" => TierConfig::high_effort(),
            "high" => TierConfig::high_effort(),
            "medium" | "default" => TierConfig::default(),
            "low" => TierConfig::low_effort(),
            _ => TierConfig::default(),
        };
        self.compress_if_needed();
    }

    pub fn token_count(&self) -> usize {
        self.estimated_tokens
    }

    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    pub fn message_count(&self) -> usize {
        self.tiers.iter().map(|t| t.len()).sum()
    }

    pub fn tier_counts(&self) -> [usize; 4] {
        [
            self.tiers[0].len(),
            self.tiers[1].len(),
            self.tiers[2].len(),
            self.tiers[3].len(),
        ]
    }

    pub fn clear_history(&mut self) {
        for tier in &mut self.tiers {
            tier.clear();
        }
        self.estimated_tokens =
            Self::estimate_tokens_str(self.system_prompt.content.as_deref().unwrap_or(""));
    }

    fn compress_if_needed(&mut self) {
        while self.estimated_tokens > self.max_tokens {
            let removed = self.evict_lowest_priority();
            if !removed {
                break;
            }
        }
    }

    fn evict_lowest_priority(&mut self) -> bool {
        // 按 priority 升序遍历（Summary 先被驱逐），不需要运行时排序
        for tier in [
            MessageTier::Summary,
            MessageTier::Normal,
            MessageTier::Important,
            MessageTier::Critical,
        ] {
            let tier_idx = tier_to_index(tier);
            if self.tiers[tier_idx].len() > self.get_keep_limit(tier)
                && let Some(front) = self.pop_front_with_partner(tier_idx)
            {
                self.estimated_tokens = self.estimated_tokens.saturating_sub(front.tokens);
                return true;
            }
        }

        for tier in [
            MessageTier::Summary,
            MessageTier::Normal,
            MessageTier::Important,
            MessageTier::Critical,
        ] {
            let tier_idx = tier_to_index(tier);
            if !self.tiers[tier_idx].is_empty()
                && let Some(front) = self.pop_front_with_partner(tier_idx)
            {
                self.estimated_tokens = self.estimated_tokens.saturating_sub(front.tokens);
                self.evict_reasoning_content(tier_idx);
                return true;
            }
        }

        false
    }

    /// 从层首弹出一条消息。若它是 tool_call 消息,同时移除其 tool_result 消息,
    /// 避免驱逐后留下孤立 tool 结果 → API 校验 tool_call_id 失败(400)。
    /// 被连带移除的 tool 结果同样从 estimated_tokens 中扣除。
    fn pop_front_with_partner(&mut self, tier_idx: usize) -> Option<TieredMessage> {
        let front = self.tiers[tier_idx].pop_front()?;
        if let Some(ref calls) = front.message.tool_calls {
            let call_ids: Vec<String> = calls.iter().map(|c| c.id.clone()).collect();
            if !call_ids.is_empty() {
                let removed = {
                    let tier = &mut self.tiers[tier_idx];
                    let mut removed = Vec::new();
                    tier.retain(|tm| {
                        if tm.message.role == "tool"
                            && tm
                                .message
                                .tool_call_id
                                .as_ref()
                                .is_some_and(|id| call_ids.contains(id))
                        {
                            removed.push(tm.clone());
                            false
                        } else {
                            true
                        }
                    });
                    removed
                };
                for r in &removed {
                    self.estimated_tokens = self.estimated_tokens.saturating_sub(r.tokens);
                }
            }
        }
        Some(front)
    }

    fn get_keep_limit(&self, tier: MessageTier) -> usize {
        // 修复(Bug #1):Normal tier 现在使用 normal_keep(消息条数),
        // 原先误用 normal_truncate_chars(字符预算 8000)导致永不驱逐。
        match tier {
            MessageTier::Critical => self.config.critical_keep,
            MessageTier::Important => self.config.important_keep,
            MessageTier::Normal => self.config.normal_keep,
            MessageTier::Summary => self.config.summary_keep,
        }
    }

    fn evict_reasoning_content(&mut self, _tier_idx: usize) {
        let budget = self.config.thinking_budget_chars;
        let mut token_savings: usize = 0;
        for tier in &mut self.tiers {
            for tm in tier.iter_mut() {
                if let Some(ref reasoning) = tm.message.reasoning_content
                    && reasoning.chars().count() > budget
                {
                    // 修复(Medium):此前 `reasoning.len()`(字节)与 `budget`
                    // (名为 chars) 比较,CJK 在阈值附近截断行为不确定。
                    // 统一用字符数判断。
                    let old_tokens = tm.tokens;
                    let boundary = reasoning
                        .char_indices()
                        .nth(budget)
                        .map(|(i, _)| i)
                        .unwrap_or(reasoning.len());
                    tm.message.reasoning_content = Some(format!(
                        "{}...\n[思考链已截断，保留 {} 字符]",
                        &reasoning[..boundary],
                        budget
                    ));
                    // 修复(审查):截断后必须重算 token 估算并从 estimated_tokens 扣除,
                    // 否则估算持续虚高,窗口会把所有消息驱逐光(模型失忆)。
                    tm.tokens = Self::estimate_tokens_message(&tm.message);
                    token_savings += old_tokens.saturating_sub(tm.tokens);
                }
            }
        }
        self.estimated_tokens = self.estimated_tokens.saturating_sub(token_savings);
    }

    pub fn auto_tier_message(&self, message: &ChatMessage) -> MessageTier {
        // tool 结果必须与其 tool_call 配对,二者同属 Critical 以保证在驱逐时
        // 也尽量成对保留(且 messages() 已按 seq 还原顺序,不会拆散)。
        if message.tool_calls.is_some() || message.role == "tool" {
            return MessageTier::Critical;
        }

        if let Some(ref content) = message.content {
            let lower = content.to_lowercase();
            if lower.contains("错误") || lower.contains("失败") || lower.contains("critical") {
                return MessageTier::Critical;
            }
            if lower.contains("重要") || lower.contains("关键") || lower.contains("必须") {
                return MessageTier::Important;
            }
            if lower.contains("总结") || lower.contains("摘要") {
                return MessageTier::Summary;
            }
        }

        if message.reasoning_content.is_some()
            && let Some(ref reasoning) = message.reasoning_content
            // 修复(M7):原用 `reasoning.len()`(字节)与 2000 比较,而同文件
            // evict_reasoning_content 已统一改用字符数。CJK reasoning 约 667 字符即触发
            // Important 分层(字节=3×字符),口径不一致。改为字符数。
            && reasoning.chars().count() > 2000
        {
            return MessageTier::Important;
        }

        MessageTier::Normal
    }

    fn estimate_tokens_str(s: &str) -> usize {
        crate::common::utils::estimate_tokens_str(s)
    }

    fn estimate_tokens_message(msg: &ChatMessage) -> usize {
        let mut tokens = 4;
        if let Some(ref content) = msg.content {
            tokens += Self::estimate_tokens_str(content);
        }
        if let Some(ref reasoning) = msg.reasoning_content {
            tokens += Self::estimate_tokens_str(reasoning);
        }
        if let Some(ref tool_calls) = msg.tool_calls {
            for tc in tool_calls {
                tokens += Self::estimate_tokens_str(&tc.function.name);
                tokens += Self::estimate_tokens_str(&tc.function.arguments);
            }
        }
        tokens
    }
}

impl Default for TieredContextWindow {
    fn default() -> Self {
        Self::new("你是 Movix Agent。".into(), 900_000)
    }
}

pub struct TieredContextBuilder {
    max_tokens: usize,
    config: TierConfig,
    system_prompt: Option<String>,
}

impl TieredContextBuilder {
    pub fn new() -> Self {
        Self {
            max_tokens: crate::common::deepseek::MAX_OUTPUT_TOKENS_USIZE,
            config: TierConfig::default(),
            system_prompt: None,
        }
    }

    pub fn max_tokens(mut self, tokens: usize) -> Self {
        self.max_tokens = tokens;
        self
    }

    pub fn config(mut self, config: TierConfig) -> Self {
        self.config = config;
        self
    }

    pub fn reasoning_effort(mut self, effort: &str) -> Self {
        self.config = match effort {
            "max" => TierConfig::high_effort(),
            "low" | "high" => TierConfig::low_effort(),
            _ => TierConfig::default(),
        };
        self
    }

    pub fn system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = Some(prompt);
        self
    }

    pub fn build(self) -> TieredContextWindow {
        let prompt = self
            .system_prompt
            .unwrap_or_else(|| "你是 Movix AI 编码助手。".into());
        TieredContextWindow::with_config(prompt, self.max_tokens, self.config)
    }
}

impl Default for TieredContextBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tier_priority() {
        assert!(MessageTier::Critical.priority() > MessageTier::Important.priority());
        assert!(MessageTier::Important.priority() > MessageTier::Normal.priority());
        assert!(MessageTier::Normal.priority() > MessageTier::Summary.priority());
    }

    #[test]
    fn test_auto_tier() {
        let builder = TieredContextBuilder::new();
        let context = builder.build();

        let tool_msg = ChatMessage::assistant(Some("Calling tool".into()), None, Some(vec![]));
        assert_eq!(context.auto_tier_message(&tool_msg), MessageTier::Critical);

        let normal_msg = ChatMessage::user("普通对话");
        assert_eq!(context.auto_tier_message(&normal_msg), MessageTier::Normal);
    }

    #[test]
    fn test_tier_config_presets() {
        let default = TierConfig::default();
        let high = TierConfig::high_effort();
        let low = TierConfig::low_effort();

        assert!(high.critical_keep > default.critical_keep);
        assert!(low.critical_keep < default.critical_keep);
    }

    /// 回归(Critical #C3):messages() 必须按插入时间顺序输出,不能因 tier 分层
    /// 把 assistant(tool_calls) 与其 tool 结果的先后关系颠倒,导致 tool 结果
    /// 出现在对应的 tool_call 之前(孤立 tool 结果)→ API 400。
    #[test]
    fn messages_preserves_tool_call_result_pairing() {
        use crate::common::deepseek::{FunctionCall, ToolCall};
        let mut ctx = TieredContextWindow::new("sys".into(), 1_000_000);
        let tc = ToolCall {
            id: "call_1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
            },
        };
        let caller = ChatMessage::assistant(None, None, Some(vec![tc]));
        let tool_result = ChatMessage::tool("call_1".into(), "file content".into());
        // caller 在 Critical 层、tool_result 也归 Critical,中间穿插 Normal 消息。
        // 修复前 messages() 按层输出会让 tool_result(后插入)出现在 caller 之前的
        // 层序里;修复后按 seq 排序,caller 必在 tool_result 之前。
        ctx.push(caller, MessageTier::Critical);
        ctx.push(
            ChatMessage::user(String::from("padding")),
            MessageTier::Normal,
        );
        ctx.push(tool_result, MessageTier::Critical);

        let msgs = ctx.messages();
        let caller_idx = msgs
            .iter()
            .position(|m| {
                m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|c| !c.is_empty())
            })
            .expect("应存在 tool_call 消息");
        let tool_idx = msgs
            .iter()
            .position(|m| m.role == "tool")
            .expect("应存在 tool 结果");
        assert!(
            caller_idx < tool_idx,
            "tool_call(位置 {}) 必须在其 tool 结果(位置 {}) 之前,否则 API 400",
            caller_idx,
            tool_idx
        );
    }

    /// 回归:auto_tier_message 把 tool 结果归入 Critical,与 tool_call 同层。
    #[test]
    fn auto_tier_groups_tool_result_with_caller() {
        let ctx = TieredContextWindow::new("sys".into(), 1000);
        let tool_result = ChatMessage::tool("c1".into(), "x".into());
        assert_eq!(
            ctx.auto_tier_message(&tool_result),
            MessageTier::Critical,
            "tool 结果应归 Critical 以与 tool_call 配对"
        );
    }
}
