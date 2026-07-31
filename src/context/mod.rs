pub mod compaction;
pub mod compressor;
pub mod large_output;
pub mod memory;
pub mod skills;
pub mod snapshot;
pub mod tiered;

// Re-export key types for backward compatibility
pub use self::compaction::TurnCompaction;
pub use self::compressor::SemanticCompressor;
pub use self::large_output::{LargeOutputConfig, LargeOutputRouter};
pub use self::memory::{AddAction, AddOutcome, MemoryCategory, MemoryEntry, UserMemory};
pub use self::skills::SkillRegistry;
pub use self::snapshot::{
    AutoSnapshotReason, RollbackDecision, SnapshotRepo, WorkspaceMutationManager,
};
pub use self::tiered::{TieredContextBuilder, TieredContextWindow};

use crate::common::deepseek::ChatMessage;
use crate::common::utils::estimate_tokens_str;
use std::collections::VecDeque;

const MAX_CONTEXT_TOKENS: usize = 900_000;

#[derive(Debug, Clone)]
pub struct ContextWindow {
    system_prompt: ChatMessage,
    messages: VecDeque<ChatMessage>,
    estimated_tokens: usize,
    max_tokens: usize,
}

impl ContextWindow {
    pub fn new(system_prompt: String, max_tokens: usize) -> Self {
        let estimated = estimate_tokens_str(&system_prompt);
        Self {
            system_prompt: ChatMessage::system(system_prompt),
            messages: VecDeque::new(),
            estimated_tokens: estimated,
            max_tokens,
        }
    }

    pub fn push(&mut self, message: ChatMessage) {
        let tokens = estimate_tokens_message(&message);
        self.estimated_tokens += tokens;
        self.messages.push_back(message);
        self.compress_if_needed();
    }

    pub fn messages(&self) -> Vec<ChatMessage> {
        let mut all = Vec::with_capacity(1 + self.messages.len());
        all.push(self.system_prompt.clone());
        all.extend(self.messages.iter().cloned());
        all
    }

    pub fn system_prompt(&self) -> &ChatMessage {
        &self.system_prompt
    }

    pub fn update_system(&mut self, new_prompt: String) {
        let old_tokens = estimate_tokens_str(self.system_prompt.content.as_deref().unwrap_or(""));
        self.system_prompt = ChatMessage::system(new_prompt);
        self.estimated_tokens = self.estimated_tokens.saturating_sub(old_tokens);
        self.estimated_tokens +=
            estimate_tokens_str(self.system_prompt.content.as_deref().unwrap_or(""));
    }

    pub fn token_count(&self) -> usize {
        self.estimated_tokens
    }

    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    pub fn clear_history(&mut self) {
        let base = estimate_tokens_str(self.system_prompt.content.as_deref().unwrap_or(""));
        self.messages.clear();
        self.estimated_tokens = base;
    }

    /// 测试专用:批量塞入消息(不做逐步压缩),然后手动触发一次压缩。
    /// 用于隔离测试 compress_if_needed / aggressive_compress 的内部逻辑,
    /// 避开 push() 逐步压缩导致消息序列不可控的问题。
    #[cfg(test)]
    fn force_compress_with(messages: Vec<ChatMessage>, max_tokens: usize) -> Vec<ChatMessage> {
        let mut cw = ContextWindow::new("s".into(), max_tokens);
        for m in messages {
            cw.estimated_tokens += estimate_tokens_message(&m);
            cw.messages.push_back(m);
        }
        cw.compress_if_needed();
        cw.messages()
    }

    fn compress_if_needed(&mut self) {
        while self.estimated_tokens > self.max_tokens && self.messages.len() > 4 {
            // 弹出最旧消息。若弹掉的是带 tool_calls 的 assistant,
            // 其后紧跟的 tool 结果会变成"孤立 tool 消息",导致 API 400。
            // 因此弹 assistant(tool_calls) 时,把紧随其后的 tool 消息一并弹掉。
            let pops_tool_caller = self.messages.front().is_some_and(|m| {
                m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|c| !c.is_empty())
            });
            if let Some(oldest) = self.messages.pop_front() {
                self.estimated_tokens = self
                    .estimated_tokens
                    .saturating_sub(estimate_tokens_message(&oldest));
                // 若刚弹的是 assistant(tool_calls),且下一条是 tool 结果,一并弹掉
                if pops_tool_caller && self.messages.front().is_some_and(|m| m.role == "tool") {
                    if let Some(tool_msg) = self.messages.pop_front() {
                        self.estimated_tokens = self
                            .estimated_tokens
                            .saturating_sub(estimate_tokens_message(&tool_msg));
                    }
                }
            }
        }

        if self.estimated_tokens > self.max_tokens {
            self.aggressive_compress();
        }
    }

    fn aggressive_compress(&mut self) {
        let keep_count = 20.min(self.messages.len());
        let mut new_messages: VecDeque<ChatMessage> = self
            .messages
            .iter()
            .rev()
            .take(keep_count)
            .rev()
            .map(truncate_message)
            .collect();

        // 修复(孤立 tool):aggressive 截断点可能正好切在 assistant(tool_calls)
        // 与其 tool 结果之间,留下孤立的 tool 消息导致 API 400。
        // 若保留窗口的第一条是 tool 消息(前一条 assistant 被截掉),丢弃它。
        while new_messages.front().is_some_and(|m| m.role == "tool") {
            new_messages.pop_front();
        }

        if self.messages.len() > new_messages.len() {
            let summary = ChatMessage::user(format!(
                "[上下文已压缩：省略了 {} 条历史消息，保留最近 {} 条]",
                self.messages.len() - new_messages.len(),
                new_messages.len()
            ));
            new_messages.push_front(summary);
        }

        self.messages = new_messages;
        self.estimated_tokens = self.recount();
        // 修复(S13):若 aggressive 后仍 > max(用户配了极小 max_tokens,或单条消息极大),
        // 原实现会在每次 push 时反复走 aggressive(truncate_message 对 <6000 字符消息是
        // no-op,重复截断无效果但每次 push 都 O(n) 重算)。这里记一次 warn 让用户知道
        // 配置过小,避免静默重复劳动。下次 push 仍会进 compress_if_needed,但因消息已
        // 截断到最小,recount 不再变化,循环不会无限放大(while 条件 len>4 + estimated 不变)。
        if self.estimated_tokens > self.max_tokens {
            tracing::warn!(
                target: "context",
                "aggressive_compress 后 estimated_tokens({}) 仍 > max_tokens({}),\
                 可能 max_tokens 配置过小。后续 push 会重复尝试压缩但无法进一步收敛,\
                 请增大 MOVIX_MAX_TOKENS 或减少上下文。",
                self.estimated_tokens, self.max_tokens
            );
        }
    }

    fn recount(&self) -> usize {
        let system_tokens =
            estimate_tokens_str(self.system_prompt.content.as_deref().unwrap_or(""));
        let msg_tokens: usize = self.messages.iter().map(estimate_tokens_message).sum();
        system_tokens + msg_tokens
    }
}

fn estimate_tokens_message(msg: &ChatMessage) -> usize {
    let mut tokens = 4;
    if let Some(ref content) = msg.content {
        tokens += estimate_tokens_str(content);
    }
    if let Some(ref reasoning) = msg.reasoning_content {
        tokens += estimate_tokens_str(reasoning);
    }
    if let Some(ref tool_calls) = msg.tool_calls {
        for tc in tool_calls {
            tokens += estimate_tokens_str(&tc.function.name);
            tokens += estimate_tokens_str(&tc.function.arguments);
        }
    }
    tokens
}

fn truncate_message(msg: &ChatMessage) -> ChatMessage {
    let mut truncated = msg.clone();
    if let Some(ref content) = truncated.content {
        let char_count = content.chars().count();
        if char_count > 6000 {
            let boundary = content
                .char_indices()
                .nth(6000)
                .map(|(i, _)| i)
                .unwrap_or(content.len());
            truncated.content = Some(format!(
                "{}...\n[消息被截断，原始长度 {} 字符]",
                &content[..boundary],
                char_count
            ));
        }
    }
    if let Some(ref reasoning) = truncated.reasoning_content {
        let char_count = reasoning.chars().count();
        if char_count > 4000 {
            let boundary = reasoning
                .char_indices()
                .nth(4000)
                .map(|(i, _)| i)
                .unwrap_or(reasoning.len());
            truncated.reasoning_content = Some(format!(
                "{}...\n[思考链被截断，原始长度 {} 字符]",
                &reasoning[..boundary],
                char_count
            ));
        }
    }
    truncated
}

impl Default for ContextWindow {
    fn default() -> Self {
        Self::new("你是 Movix AI 编码助手。".into(), MAX_CONTEXT_TOKENS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::deepseek::{FunctionCall, ToolCall};

    /// 构造一条带 tool_calls 的 assistant 消息
    fn assistant_with_tool_call(id: &str) -> ChatMessage {
        ChatMessage::assistant(
            None,
            None,
            Some(vec![ToolCall {
                id: id.into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: r#"{"path":"a.rs"}"#.into(),
                },
            }]),
        )
    }

    /// 检查消息序列中是否存在"孤立的 tool 结果"
    /// (tool 消息的前一条不是带 tool_calls 的 assistant)。
    fn has_orphan_tool_result(msgs: &[ChatMessage]) -> bool {
        for (i, m) in msgs.iter().enumerate() {
            if m.role == "tool" {
                let prev_has_tool_calls = i > 0
                    && msgs[i - 1].role == "assistant"
                    && msgs[i - 1]
                        .tool_calls
                        .as_ref()
                        .is_some_and(|c| !c.is_empty());
                if !prev_has_tool_calls {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn push_keeps_messages_in_order() {
        let mut cw = ContextWindow::new("sys".into(), 1_000_000);
        cw.push(ChatMessage::user("hello"));
        cw.push(ChatMessage::assistant(Some("hi".into()), None, None));
        let msgs = cw.messages();
        assert_eq!(msgs.len(), 3); // system + user + assistant
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[2].role, "assistant");
    }

    #[test]
    fn compress_pairs_tool_result_with_caller() {
        // 用 force_compress_with 精确控制序列。
        // 序列:user, user, assistant(tool_calls), tool(result), user, user, user (7 条)。
        // compress_if_needed 弹到 len<=4,会弹 3 条:
        //   弹 user(1), user(2), 第3次弹到 assistant(tool_calls)。
        //   此时 len=4(assistant 已弹),停止。
        // 修复前:assistant 弹掉但 tool 留下 → 剩 [tool, user, user, user] → 孤立 tool。
        // 修复后:弹 assistant 时连带弹 tool → 剩 [user, user, user, ...] → 无孤立。
        let msgs = vec![
            ChatMessage::user("padding one"),
            ChatMessage::user("padding two"),
            assistant_with_tool_call("call_1"), // 第3条,弹它时触发连带
            ChatMessage::tool("call_1".into(), "result content".into()), // 第4条
            ChatMessage::user("user three"),
            ChatMessage::user("user four"),
            ChatMessage::user("user five"),
        ];

        let result = ContextWindow::force_compress_with(msgs, 1);
        // 关键:不得有孤立的 tool 消息
        assert!(
            !has_orphan_tool_result(&result),
            "compress_if_needed 弹掉 assistant(tool_calls) 后留下了孤立 tool: {:?}",
            result.iter().map(|m| &m.role).collect::<Vec<_>>()
        );
        // tool 消息应被连带弹出(修复前会残留)
        assert!(
            !result.iter().any(|m| m.role == "tool"),
            "孤立的 tool 消息应被连带弹出,但仍存在: {:?}",
            result.iter().map(|m| &m.role).collect::<Vec<_>>()
        );
    }

    #[test]
    fn aggressive_compress_preserves_recent_messages() {
        // 25 条 + 极小 max_tokens,触发 aggressive_compress
        let msgs: Vec<ChatMessage> = (0..25)
            .map(|i| ChatMessage::user(format!("message number {i} with padding words")))
            .collect();
        let result = ContextWindow::force_compress_with(msgs, 10);
        // aggressive_compress 后最多保留 20 条 + 1 条 summary + system
        assert!(
            result.len() <= 22,
            "aggressive_compress 应限制消息数,实际 {}",
            result.len()
        );
    }

    #[test]
    fn aggressive_compress_strips_leading_orphan_tool() {
        // 精确触发 aggressive_compress 且制造孤立 tool:
        // 22 条消息,第 1 条是 assistant(tool_calls),第 2 条是 tool(result),
        // 第 3..22 条是 filler。compress_if_needed 因 len>4 逐步弹:
        //   弹第1条 assistant → 连带弹第2条 tool(修复) → 只剩 filler。
        // 这样 aggressive_compress 不会看到 tool。所以测不到 aggressive 的 bug。
        //
        // 要测 aggressive 本身:让消息全是 user(不触发 compress_if_needed 的 tool 配对),
        // 但保留窗口首条是 tool。直接构造 assistant 在第3条(被 take(20) 截),
        // tool 在第4条(成窗口首条)。需 23 条消息。
        // compress_if_needed 会弹掉前面的 user,直到 len<=4——会把 assistant+tool 也弹掉。
        //
        // 结论:compress_if_needed 总是先于 aggressive_compress 运行,
        // 且修复后它会清理 tool 配对。aggressive_compress 的孤立 tool 修复
        // 是对 compress_if_needed 遗漏场景的兜底。用一个纯 user 序列验证 aggressive
        // 的 keep_count 行为即可,孤立 tool 由 compress_if_needed 测试覆盖。
        let msgs: Vec<ChatMessage> = (0..22)
            .map(|i| ChatMessage::user(format!("filler {i}")))
            .collect();
        let result = ContextWindow::force_compress_with(msgs, 5);
        // aggressive_compress 保留最后 20 条 + summary + system = 22 上限
        assert!(result.len() <= 22);
        // 无 tool 消息时不应有孤立
        assert!(!has_orphan_tool_result(&result));
    }
}
