use crate::common::deepseek::ChatMessage;
use crate::common::utils::estimate_tokens_str;

const KEEP_RECENT_MESSAGES: usize = 4;
const MIN_SUMMARIZE_MESSAGES: usize = 6;
const TURN_END_RESULT_CAP_TOKENS: usize = 3000;

pub struct TurnCompaction {
    enabled: bool,
    token_threshold: usize,
    last_compact_tokens: usize,
}

impl TurnCompaction {
    pub fn new() -> Self {
        Self {
            enabled: true,
            token_threshold: 800_000,
            last_compact_tokens: 0,
        }
    }

    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn with_threshold(mut self, threshold: usize) -> Self {
        self.token_threshold = threshold;
        self
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn should_compact(&self, message_count: usize, total_tokens: usize) -> bool {
        // 修复:原条件 `total_tokens >= 500_000 && ... && total_tokens > self.token_threshold(800_000)`
        // 中 `>= 500_000` 永远被 `> 800_000` 蕴含,是冗余条件。简化为单一阈值判断。
        self.enabled
            && message_count >= MIN_SUMMARIZE_MESSAGES
            && total_tokens > self.token_threshold
    }

    /// 压缩消息。`provided_summary` 若为 Some 则直接使用（通常是 LLM 生成的语义摘要），
    /// 否则回退到统计字符串摘要。
    pub fn compact(&self, messages: &mut Vec<ChatMessage>) -> CompactionResult {
        self.compact_with_summary(messages, None)
    }

    /// 带（可选）外部摘要的压缩。调用方可在 drain 前用 SemanticCompressor 生成
    /// LLM 语义摘要传入；None 时回退到 build_stat_summary 的统计字符串。
    pub fn compact_with_summary(
        &self,
        messages: &mut Vec<ChatMessage>,
        provided_summary: Option<String>,
    ) -> CompactionResult {
        if messages.len() < MIN_SUMMARIZE_MESSAGES {
            return CompactionResult {
                removed_count: 0,
                summary: String::new(),
            };
        }

        let keep_count = KEEP_RECENT_MESSAGES.min(messages.len());
        let remove_count = messages.len() - keep_count;

        if remove_count == 0 {
            return CompactionResult {
                removed_count: 0,
                summary: String::new(),
            };
        }

        // 修复:原实现硬假设 `messages[0]` 是 system message,然后用
        // `drain(1..)` 删中间、insert(1, summary)。当上游有:
        //   - 多个 system(系统提示 + 内存注入 + 技能注入)
        //   - 或者 system 不在 [0] 位置
        // 时会破坏消息结构(把 user/assistant 当成 system 删掉,或把
        // summary 插到错误位置)。
        // 改为:
        //   1) 找到前导 system 段的结尾索引(连续 system 的最后一个);
        //   2) 保留前导 system 段;
        //   3) 在前导 system 段与 keep_count 之间的区间统计并删除;
        //   4) summary 插入到前导 system 段末尾。
        let prefix_end = messages
            .iter()
            .position(|m| !matches!(m.role.as_str(), "system" | "developer"))
            .unwrap_or(messages.len());

        // 要保留的最早用户消息起点(可能是 prefix_end,也可能就是 prefix_end)
        let cut_start = prefix_end.min(messages.len().saturating_sub(keep_count));
        // 真要删的范围
        let drain_end = messages.len() - keep_count;

        if drain_end <= cut_start {
            return CompactionResult {
                removed_count: 0,
                summary: String::new(),
            };
        }

        // 统计信息无论是否使用 LLM 摘要都需要（用于 CompactionResult 和 fallback）
        let removed_range = cut_start..drain_end;
        let stat_summary = self.build_stat_summary(messages, &removed_range);

        // 优先用外部 LLM 摘要；为空或未提供则回退统计字符串
        let summary = match provided_summary {
            Some(s) if !s.trim().is_empty() => s,
            _ => stat_summary,
        };

        let summary_msg = ChatMessage::system(&summary);

        // 使用 drain + insert 替代 split_off + truncate + push + append 四步操作
        let drained_count = drain_end - cut_start;
        let _removed: Vec<_> = messages.drain(cut_start..drain_end).collect();
        messages.insert(cut_start, summary_msg);

        // 修复(孤立 tool,双向):
        // (a) 前导孤立 tool:drain 后窗口第一条非 system 消息可能是孤立的 tool 结果
        //     (其对应的 assistant(tool_calls) 被 drain 掉了)→ 删除。
        // (b) 尾部孤立 tool_call:drain 区间若包含某条 tool 结果,而其对应的
        //     assistant(tool_calls) 在保留区(成为保留区较旧一端),则该 assistant
        //     后面不再有它的 tool_result → 同样孤立,需补占位或删除。
        //     这里删除尾部无结果的 assistant(tool_calls)。
        let mut extra_removed: usize = 0;
        // (a) 前导孤立 tool
        while messages.get(prefix_end).is_some_and(|m| m.role == "tool") {
            messages.remove(prefix_end);
            extra_removed += 1;
        }
        // (b) 尾部孤立 tool_call:遍历保留区,若 assistant(tool_calls) 的下一条
        //     不是 tool 结果(可能被 drain),删除该 assistant 以免 API 400。
        let mut i = prefix_end;
        while i < messages.len() {
            let is_caller = messages[i].role == "assistant"
                && messages[i]
                    .tool_calls
                    .as_ref()
                    .is_some_and(|c| !c.is_empty());
            if is_caller {
                let next_is_tool = messages.get(i + 1).is_some_and(|m| m.role == "tool");
                if !next_is_tool {
                    messages.remove(i);
                    extra_removed += 1;
                    continue; // 不递增 i,重新检查新位置
                }
            }
            i += 1;
        }

        CompactionResult {
            removed_count: drained_count + extra_removed,
            summary,
        }
    }

    /// 对被删除区间的消息构建统计摘要（非 LLM）。作为 LLM 摘要不可用时的回退。
    fn build_stat_summary(
        &self,
        messages: &[ChatMessage],
        range: &std::ops::Range<usize>,
    ) -> String {
        let mut tool_call_count = 0usize;
        let mut reasoning_count = 0usize;
        let mut file_paths: Vec<String> = Vec::new();
        let mut user_requests: Vec<String> = Vec::new();

        for msg in messages.iter().take(range.end).skip(range.start) {
            if let Some(calls) = &msg.tool_calls {
                tool_call_count += 1;
                for tc in calls {
                    if let Ok(args) =
                        serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                    {
                        for key in ["path", "file_path", "cwd"] {
                            if let Some(p) = args.get(key).and_then(|v| v.as_str()) {
                                if !p.is_empty() && !file_paths.contains(&p.to_string()) {
                                    file_paths.push(p.to_string());
                                }
                            }
                        }
                    }
                }
            }
            if msg.reasoning_content.is_some() {
                reasoning_count += 1;
            }
            // 保留用户请求摘要
            // 修复(G-L2):原 take(80) 太短,任务关键约束可能在后半段丢失。提到 200 字符。
            if msg.role == "user" {
                if let Some(content) = &msg.content {
                    let preview: String = content.chars().take(200).collect();
                    if !preview.is_empty() && user_requests.len() < 5 {
                        user_requests.push(preview);
                    }
                }
            }
        }

        let removed_count = range.end - range.start;
        let mut summary_parts = vec![format!(
            "已压缩 {} 条早期消息 | 含 {} 次工具调用, {} 次推理",
            removed_count, tool_call_count, reasoning_count
        )];

        if !file_paths.is_empty() {
            summary_parts.push(format!("涉及文件: {}", file_paths.join(", ")));
        }
        if !user_requests.is_empty() {
            summary_parts.push(format!("用户请求: {}", user_requests.join("; ")));
        }

        format!("[{}]", summary_parts.join(" | "))
    }

    pub fn truncate_tool_result(&self, result: &str, max_tokens: usize) -> String {
        // 修复(R5/H11,关键):原 max_chars = max_tokens * 4 假设 ~4 chars/token(英文)。
        // 但 should_truncate_result 用 estimate_tokens_str(CJK 0.7 tokens/char ≈ 1.4
        // chars/token)。CJK 内容下:should_truncate 说"该截"(>3000 token),但
        // truncate_tool_result 看 total_chars(5000) <= max_chars(12000) → 返回原文,
        // 截断是 no-op,LLM 仍收到超限输出,还误记 ToolCallTruncated 失败信号。
        //
        // 修复:用与 estimate_tokens_str 同源的 CJK 感知启发式反推 char budget。
        // 直接调 estimate_tokens_str 判断整体是否超限;若超限,按 token 比例切 head/tail。
        let total_tokens = estimate_tokens_str(result);
        if total_tokens <= max_tokens {
            return result.to_string();
        }

        let total_chars = result.chars().count();
        // 估算每个 char 平均摊多少 token,反推 max_tokens 对应的 char 上限。
        let tokens_per_char = if total_chars > 0 {
            total_tokens as f64 / total_chars as f64
        } else {
            0.25
        };
        let max_chars = ((max_tokens as f64 / tokens_per_char).ceil() as usize).max(1);

        let head_chars = (max_chars * 2 / 3).min(total_chars);
        let tail_chars = (max_chars / 3).min(total_chars.saturating_sub(head_chars));

        // 修复(Bug #8):用 char_indices().nth() + map_or 显式处理边界,
        // 不再依赖 head_byte_end == 0 兜底(会把 head 误扩到整串)。
        let head_byte_end = result
            .char_indices()
            .nth(head_chars)
            .map_or(result.len(), |(b, _)| b);
        let head: &str = &result[..head_byte_end];

        let skip = total_chars.saturating_sub(tail_chars);
        let tail_byte_start = result
            .char_indices()
            .nth(skip)
            .map_or(result.len(), |(b, _)| b);
        let tail: &str = &result[tail_byte_start..];

        let omitted = result.len().saturating_sub(head.len() + tail.len());
        format!(
            "{}\n\n... [已截断，原长度 {} chars，省略 {} chars] ...\n\n{}",
            head,
            result.len(),
            omitted,
            tail
        )
    }

    pub fn should_truncate_result(&self, result: &str) -> bool {
        // 修复:用 estimate_tokens_str 代替 /4,CJK 场景不会严重低估。
        self.enabled && estimate_tokens_str(result) > TURN_END_RESULT_CAP_TOKENS
    }

    pub fn last_compact_tokens(&self) -> usize {
        self.last_compact_tokens
    }

    pub fn set_last_compact_tokens(&mut self, tokens: usize) {
        self.last_compact_tokens = tokens;
    }

    pub fn token_threshold(&self) -> usize {
        self.token_threshold
    }
}

impl Default for TurnCompaction {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub removed_count: usize,
    pub summary: String,
}
