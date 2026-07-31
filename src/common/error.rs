use thiserror::Error;

#[derive(Error, Debug)]
#[non_exhaustive] // 修复：阻止下游 match 穷尽,未来加新错误变体不破坏兼容
pub enum MovixError {
    #[error("API 调用失败: {0}")]
    ApiError(String),

    #[error("工具执行失败 [{tool}]: {message}")]
    ToolError { tool: String, message: String },

    // 修复(S7,关键):原 `#[error("IO 错误: {0}")]` 直接暴露 std::io::Error 的 Display,
    // 它通常含完整文件路径。改为存脱敏后的 String(通过自定义 From 转换),Display 不再
    // 接触原始 io::Error,避免经 ? 自动转换路径旁路 redact_secrets 防线。
    #[error("IO 错误: {0}")]
    IoError(String),

    #[error("JSON 解析失败: {0}")]
    JsonError(#[from] serde_json::Error),

    // 修复(S7):reqwest::Error 的 Display 含请求 URL,脱敏后存 String。
    #[error("HTTP 请求失败: {0}")]
    HttpError(String),

    #[error("环境变量未设置: {0}")]
    EnvError(String),

    #[error("达到最大迭代次数 ({0})，任务未完成")]
    MaxIterationsReached(u32),

    #[error("沙箱安全拦截: {0}")]
    SandboxViolation(String),

    #[error("取消: {0}")]
    Cancelled(String),

    #[error("MCP 错误: {0}")]
    McpError(String),

    #[error("{0}")]
    Other(String),
}

/// 修复(S7):对错误文案做脱敏,剥离疑似 API key / Bearer token 子串。
/// 与 deepseek.rs 的 redact_secrets 同语义,但 error.rs 不依赖 deepseek 模块,
/// 故此处独立实现一份(正则用 OnceLock 缓存,避免每次重编译)。
fn redact_error_text(s: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // 修复(R5/H16):正则字符类需与 config.rs 的 key 校验一致(允许 + /)。
        // 原正则不含 + /,base64 风格 key(sk-AB.CD+EF/GH)在 . / 处截断,半泄露。
        // config.rs:113 允许 - _ . + /,这里同步。
        regex::Regex::new(r"(?i)(sk-[A-Za-z0-9_\-\.+/]{8,}|Bearer\s+[A-Za-z0-9_\-\.+/]{8,})")
            .expect("redact regex")
    });
    re.replace_all(s, "[REDACTED]").to_string()
}

// 修复(S7):自定义 From,把原始 io/reqwest 错误转成脱敏后的 String 再装入 MovixError,
// 这样 ? 自动转换也会脱敏,不会旁路 redact_secrets。
impl From<std::io::Error> for MovixError {
    fn from(e: std::io::Error) -> Self {
        MovixError::IoError(redact_error_text(&e.to_string()))
    }
}

impl From<reqwest::Error> for MovixError {
    fn from(e: reqwest::Error) -> Self {
        MovixError::HttpError(redact_error_text(&e.to_string()))
    }
}

/// `Result` 别名。`MovixError` 内部由 `thiserror` 自动加 `#[must_use]`,
/// 不可在此处重复声明,否则在更新的 rustc 下会变成 hard error。
pub type Result<T> = std::result::Result<T, MovixError>;
