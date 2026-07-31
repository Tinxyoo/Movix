//! P5+: TUI 共享类型。
//!
//! 跨子模块(`app` / `events` / `render` / `commands`)共用的"非 App 状态"
//! 数据类型。

/// Streaming 后台 → TUI 主循环的单向消息。
///
/// 设计上**只承载数据**——不做任何处理逻辑;`events.rs` 拿到它后做模式匹配。
/// 变体:
/// - `Content(String)` — 普通正文 chunk
/// - `Thinking(String)` — 思考/推理 chunk
/// - `ToolAction` — 工具调用结果(可含 diff 上下文)
/// - `Done` — 流结束
/// - `Error(String)` — 流失败
#[derive(Debug, Clone)]
pub enum StreamUpdate {
    Content(String),
    Thinking(String),
    ToolAction {
        tool: String,
        detail: String,
        file: Option<String>,
        output: String,
    },
    /// 上下文窗口使用量更新（used / max tokens），用于实时进度条
    Context {
        used: usize,
        max: usize,
    },
    /// 修复(Bug #2):token 统计更新,直接把后台 agent 的真实 stats 推给 UI;
    /// 主循环 swap agent 期间,UI 不再读占位 agent 的零值。
    Stats {
        total_tokens: u64,
        prompt_tokens: u64,
        completion_tokens: u64,
        cache_hit_tokens: u64,
        cache_miss_tokens: u64,
        reasoning_tokens: u64,
        iteration: u32,
    },
    Done,
    Error(String),
}

/// TUI 内部消息角色(仅用于 TUI 显示层,不参与序列化)。
///
/// 与 `crate::common::deepseek::MessageRole` 不同——后者是 OpenAI API 兼容的
/// `system`/`user`/`assistant`/`tool` 字符串;本枚举只关心"谁说的",
/// 在渲染时映射到不同颜色和图标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageRole {
    User,
    Assistant,
    ToolResult,
}

/// TUI 内部聊天消息(仅用于本地滚动显示)。
///
/// 字段类型与 `crate::common::deepseek::ChatMessage` 不同——后者需要序列化为
/// OpenAI 协议 JSON,本 struct 只在 TUI 内部记录,字段是 `String` 不是 `Option`。
pub(crate) struct ChatMessage {
    pub(crate) role: MessageRole,
    pub(crate) content: String,
}
