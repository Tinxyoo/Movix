use crate::common::deepseek::{ChatMessage, DeepSeekClient};
use crate::common::error::Result;
use crate::common::utils::{estimate_tokens_str, truncate_str};

const DEFAULT_COMPRESSION_RATIO: f32 = 0.15;

/// 压缩策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionStrategy {
    /// LLM 语义摘要（无 client 时回退到简单截断）
    SemanticSummary,
}

/// 压缩结果
#[derive(Debug, Clone)]
pub struct CompressionResult {
    /// 压缩后的消息
    pub compressed_message: ChatMessage,
    /// 原始 token 估计
    pub original_tokens: usize,
    /// 压缩后 token 估计
    pub compressed_tokens: usize,
    /// 压缩比
    pub ratio: f32,
    /// 使用的策略
    pub strategy: CompressionStrategy,
}

/// 语义摘要压缩器
pub struct SemanticCompressor {
    /// 目标压缩比
    target_ratio: f32,
}

impl SemanticCompressor {
    /// 创建语义压缩器
    pub fn new() -> Self {
        Self {
            target_ratio: DEFAULT_COMPRESSION_RATIO,
        }
    }

    /// 压缩一组消息为单条摘要消息
    pub async fn compress_messages(
        &self,
        messages: &[ChatMessage],
        client: Option<&mut DeepSeekClient>,
    ) -> Result<CompressionResult> {
        if messages.is_empty() {
            return Ok(CompressionResult {
                compressed_message: ChatMessage::system("[empty context]"),
                original_tokens: 0,
                compressed_tokens: 0,
                ratio: 0.0,
                strategy: CompressionStrategy::SemanticSummary,
            });
        }

        let original_tokens: usize = messages.iter().map(estimate_tokens_message).sum();

        let target_tokens = (original_tokens as f32 * self.target_ratio) as usize;
        // 最小 100 token 保证质量，最大不超过 original_tokens 的 50%
        let target_tokens = target_tokens.clamp(100, original_tokens.saturating_sub(1).max(100));

        if let Some(c) = client {
            self.compress_semantic(messages, c, target_tokens).await
        } else {
            self.compress_fallback(messages, target_tokens)
        }
    }

    /// 简单截断压缩（无 LLM client 时的回退方案）
    fn compress_fallback(
        &self,
        messages: &[ChatMessage],
        target_tokens: usize,
    ) -> Result<CompressionResult> {
        let original_tokens: usize = messages.iter().map(estimate_tokens_message).sum();

        let mut summary_parts = Vec::new();
        let mut current_tokens = 0;

        let messages_iter = messages.iter().peekable();
        let total = messages.len();
        for (processed, msg) in messages_iter.enumerate() {
            let role_label = match msg.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => "Tool",
                _ => &msg.role,
            };

            let content = msg.content.as_deref().unwrap_or("");
            let reasoning = msg.reasoning_content.as_deref().unwrap_or("");

            let mut entry = format!("[{}]", role_label);

            if !reasoning.is_empty() {
                let r_summary = truncate_str(reasoning, 200);
                entry.push_str(&format!(" (thought: {})", r_summary));
            }

            if !content.is_empty() {
                let max_content = 500;
                let c_summary = truncate_str(content, max_content);
                entry.push_str(&format!(" {}", c_summary));
            }

            let entry_tokens = estimate_tokens_str(&entry);
            if current_tokens + entry_tokens > target_tokens {
                let remaining = target_tokens.saturating_sub(current_tokens);
                if remaining > 50 {
                    // 修复(Medium #M6 + M8):原 `entry.truncate(remaining * 4)` 按字节截断,
                    // 切在多字节字符中间会 panic(String::truncate 在非字符边界 panic)。
                    // 已改用 char_indices 落在字符边界。但 `remaining*4` 作为字符预算对 CJK
                    // 虚高(中文 1 字≈1.4 token,×4 后预算偏大约 3 倍,回退摘要超 token 目标)。
                    // 用 estimate_tokens_str 反算"剩余 token 还能塞多少字符",对 CJK/ASCII
                    // 都更准确;兜底用 remaining*4 防止除零。
                    let char_budget = if entry_tokens > 0 {
                        // entry 的 token 密度 = entry_tokens / entry 字符数;用同密度估算剩余。
                        let entry_chars = entry.chars().count().max(1);
                        let tokens_per_char = entry_tokens as f64 / entry_chars as f64;
                        ((remaining as f64) / tokens_per_char.max(0.05)) as usize
                    } else {
                        remaining.saturating_mul(4)
                    };
                    let safe_end = entry
                        .char_indices()
                        .nth(char_budget)
                        .map(|(b, _)| b)
                        .unwrap_or(entry.len());
                    let entry = format!("{}...", &entry[..safe_end]);
                    summary_parts.push(entry);
                }
                // 修复(R5/H13):原 break 后静默丢弃剩余消息。若被丢弃的尾部含用户的关键
                // 修正(如"改用 PostgreSQL")或错误,摘要会 materially 不完整且无信号。
                // 此前从不含 [... N more omitted] 标记(与 format_for_summary 不同)。
                // 现补一个省略计数,让 LLM 至少知道有内容被略过。
                let omitted = total.saturating_sub(processed + 1);
                if omitted > 0 {
                    summary_parts.push(format!(
                        "[... 因 token 预算限制,后续 {} 条消息未纳入摘要,请参考完整历史 ...]",
                        omitted
                    ));
                }
                break;
            }

            current_tokens += entry_tokens;
            summary_parts.push(entry);
        }

        let compressed = format!("[Context Summary]\n{}", summary_parts.join("\n"));
        let compressed_tokens = estimate_tokens_str(&compressed);

        Ok(CompressionResult {
            compressed_message: ChatMessage::system(compressed),
            original_tokens,
            compressed_tokens,
            ratio: if original_tokens > 0 {
                compressed_tokens as f32 / original_tokens as f32
            } else {
                0.0
            },
            strategy: CompressionStrategy::SemanticSummary,
        })
    }

    /// LLM 语义摘要压缩
    async fn compress_semantic(
        &self,
        messages: &[ChatMessage],
        client: &mut DeepSeekClient,
        target_tokens: usize,
    ) -> Result<CompressionResult> {
        let original_tokens: usize = messages.iter().map(estimate_tokens_message).sum();

        let conversation_text = self.format_for_summary(messages);

        let prompt = format!(
            r#"请将以下对话历史压缩为简洁的摘要，保留关键信息：
- 用户的请求和意图
- 已完成的关键操作
- 重要的决策和原因
- 未解决的问题

目标长度：约 {} tokens（当前 {} tokens）

对话历史：
{}"#,
            target_tokens, original_tokens, conversation_text
        );

        let summary_messages = vec![
            ChatMessage::system("你是一个对话摘要专家。输出简洁但完整的摘要，保留所有关键信息。"),
            ChatMessage::user(&prompt),
        ];

        let response = client.chat(&summary_messages, None, None).await?;
        let summary = response
            .content
            .unwrap_or_else(|| "[Summary generation failed]".into());

        // 修复(G-M13):原 `summary.len()/3` 用字节长度/3 估算 token,中文场景严重虚高
        // (1 中文字≈3 字节但≈0.7 token,虚高约 4 倍)。改用项目标准的 estimate_tokens_str。
        let compressed_tokens = estimate_tokens_str(&summary);

        Ok(CompressionResult {
            compressed_message: ChatMessage::system(format!("[Context Summary]\n{}", summary)),
            original_tokens,
            compressed_tokens,
            ratio: if original_tokens > 0 {
                compressed_tokens as f32 / original_tokens as f32
            } else {
                0.0
            },
            strategy: CompressionStrategy::SemanticSummary,
        })
    }

    /// 格式化消息用于摘要
    fn format_for_summary(&self, messages: &[ChatMessage]) -> String {
        let mut parts = Vec::new();
        let mut total_chars = 0;
        const MAX_CHARS: usize = 50_000;

        for msg in messages {
            let role_label = match msg.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => "Tool",
                _ => &msg.role,
            };

            let content = msg.content.as_deref().unwrap_or("");
            let truncated = truncate_str(content, 2000);

            let entry = format!("[{}]: {}", role_label, truncated);
            // 修复(M8):原用 `entry.len()`(字节)累加到名为 total_chars 的计数器,
            // 字段名暗示"字符"。CJK 下 50000 字节约 16000 字符即触发截断,
            // 比注释意图(MAX_CHARS)早 ~3 倍。改为字符数。
            total_chars += entry.chars().count();

            if total_chars > MAX_CHARS {
                parts.push("[... remaining messages omitted]".into());
                break;
            }

            parts.push(entry);
        }

        parts.join("\n")
    }
}

impl Default for SemanticCompressor {
    fn default() -> Self {
        Self::new()
    }
}

/// 估计消息的 token 数
fn estimate_tokens_message(msg: &ChatMessage) -> usize {
    let mut total = 0;
    if let Some(ref c) = msg.content {
        total += estimate_tokens_str(c);
    }
    if let Some(ref r) = msg.reasoning_content {
        total += estimate_tokens_str(r);
    }
    if let Some(ref tc) = msg.tool_calls {
        for call in tc {
            total += estimate_tokens_str(&call.function.name);
            total += estimate_tokens_str(&call.function.arguments);
        }
    }
    total
}
