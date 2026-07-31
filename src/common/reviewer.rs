use crate::common::deepseek::{ChatMessage, DeepSeekClient};
use crate::common::error::Result;

/// 修复(S2):判断 reviewer endpoint 是否可信。默认信任官方 api.deepseek.com;
/// 用户可通过 MOVIX_REVIEWER_TRUST_HOSTS 环境变量追加(逗号分隔,支持自部署代理)。
/// 用 url::Url 解析后比对 host_str() 精确相等,杜绝 `api.deepseek.com.evil.com`
/// 这类子串前缀绕过。
fn is_reviewer_endpoint_trusted(base_url: &str) -> bool {
    let parsed = match url::Url::parse(base_url) {
        Ok(u) => u,
        Err(_) => return false,
    };
    let host = match parsed.host_str() {
        Some(h) => h,
        None => return false,
    };
    // 默认可信 host。
    let mut trusted: Vec<String> = vec!["api.deepseek.com".to_string()];
    // 用户追加的可信 host(逗号分隔)。
    if let Ok(extra) = std::env::var("MOVIX_REVIEWER_TRUST_HOSTS") {
        trusted.extend(
            extra
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty()),
        );
    }
    trusted.iter().any(|t| t.eq_ignore_ascii_case(host))
}

/// 审查严重级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReviewSeverity {
    /// 建议（不影响正确性）
    Suggestion,
    /// 警告（潜在问题）
    Warning,
    /// 错误（必须修复）
    Error,
    /// 严重（安全/数据风险）
    Critical,
}

/// 审查发现
#[derive(Debug, Clone)]
pub struct ReviewFinding {
    pub severity: ReviewSeverity,
    pub category: String,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub suggestion: Option<String>,
}

/// 审查结果
#[derive(Debug, Clone)]
pub struct ReviewResult {
    pub approved: bool,
    pub findings: Vec<ReviewFinding>,
    pub summary: String,
    pub confidence: f32,
}

/// 审查维度
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDimension {
    /// 代码正确性
    Correctness,
    /// 安全性
    Security,
    /// 性能
    Performance,
    /// 代码风格/可维护性
    Style,
    /// 完整性（是否遗漏修改）
    Completeness,
}

/// 审查配置
#[derive(Debug, Clone)]
pub struct ReviewConfig {
    /// 是否启用审查
    pub enabled: bool,
    /// 审查维度
    pub dimensions: Vec<ReviewDimension>,
    /// 阻断级别：该级别及以上的发现会阻止自动提交
    pub block_level: ReviewSeverity,
    /// 最大审查 token 数
    pub max_review_tokens: usize,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dimensions: vec![
                ReviewDimension::Correctness,
                ReviewDimension::Security,
                ReviewDimension::Completeness,
            ],
            block_level: ReviewSeverity::Error,
            max_review_tokens: 4096,
        }
    }
}

/// 生成-评估分离的审查器
pub struct Reviewer {
    config: ReviewConfig,
}

impl Reviewer {
    /// 创建审查器
    pub fn new(config: ReviewConfig) -> Self {
        Self { config }
    }

    /// 使用默认配置创建审查器
    pub fn default_enabled() -> Self {
        Self::new(ReviewConfig::default())
    }

    /// 构建审查提示词
    pub fn build_review_prompt(
        &self,
        original_request: &str,
        changes_description: &str,
        diff_or_code: &str,
    ) -> String {
        let dimensions: Vec<&str> = self
            .config
            .dimensions
            .iter()
            .map(|d| match d {
                ReviewDimension::Correctness => "正确性：逻辑是否正确，是否有 bug",
                ReviewDimension::Security => "安全性：是否有安全漏洞、密钥泄露、命令注入",
                ReviewDimension::Performance => "性能：是否有性能问题、不必要的计算",
                ReviewDimension::Style => "风格：是否符合项目代码风格，是否可维护",
                ReviewDimension::Completeness => {
                    "完整性：是否遗漏了必要的修改（如关联文件、测试、配置）"
                }
            })
            .collect();

        format!(
            r#"你是一个代码审查专家。请审查以下代码变更。

## 用户原始请求
{original_request}

## 变更描述
{changes_description}

## 代码变更内容
{diff_or_code}

## 审查维度
{dimensions}

## 输出格式
请以 JSON 格式输出审查结果：
```json
{{
  "approved": true/false,
  "confidence": 0.0-1.0,
  "summary": "审查总结",
  "findings": [
    {{
      "severity": "suggestion/warning/error/critical",
      "category": "类别",
      "message": "问题描述",
      "file": "文件路径（可选）",
      "line": 行号（可选），
      "suggestion": "修复建议（可选）"
    }}
  ]
}}
```

注意：
- 只有在发现 error 或 critical 级别问题时才标记 approved 为 false
- suggestion 和 warning 不阻止通过
- 重点关注安全性和正确性"#,
            original_request = original_request,
            changes_description = changes_description,
            diff_or_code = diff_or_code,
            dimensions = dimensions.join("\n"),
        )
    }

    /// 审查禁用时的默认返回值
    fn disabled_result() -> ReviewResult {
        ReviewResult {
            approved: true,
            findings: vec![],
            summary: "Review disabled".into(),
            confidence: 1.0,
        }
    }

    /// 使用 LLM 执行深度审查
    pub async fn deep_review(
        &self,
        client: &mut DeepSeekClient,
        original_request: &str,
        changes_description: &str,
        diff_or_code: &str,
    ) -> Result<ReviewResult> {
        if !self.config.enabled {
            return Ok(Self::disabled_result());
        }

        // 修复(R7 → S2,自我批判):第一轮 R7 用 `base.starts_with("https://api.deepseek.com")`
        // 做前缀匹配,被 `https://api.deepseek.com.evil.attacker.com` 这种子域绕过(字符串前缀
        // 但 host 完全不同),反而给攻击者一条"只要含前缀子串即通过信任检查"的明确路径。
        // 同时硬编码单一域名会让自部署 vLLM/ollama/LiteLLM 代理用户(README 宣称支持
        // OpenAI 兼容 API)的评审静默失效。
        //
        // 正确做法:用 url::Url 解析后比对 host_str() 精确相等(杜绝子串绕过),且允许用户
        // 通过 MOVIX_REVIEWER_TRUST_HOSTS 环境变量声明额外可信 host(逗号分隔,支持代理)。
        // 默认只信任官方 api.deepseek.com。
        let base = client.base_url();
        let is_trusted = is_reviewer_endpoint_trusted(base);
        if !is_trusted {
            tracing::warn!(
                target: "reviewer",
                "拒绝执行评审:base_url '{}' 的 host 不在可信列表。默认仅信任 api.deepseek.com。\
                 若你使用自部署/兼容代理,设置环境变量 MOVIX_REVIEWER_TRUST_HOSTS=host1,host2 后重试。",
                base
            );
            return Ok(ReviewResult {
                approved: false,
                findings: vec![],
                summary: format!(
                    "评审已跳过:当前 API endpoint({})host 不在可信列表,评审无法保证独立性。\
                     (自部署用户请设 MOVIX_REVIEWER_TRUST_HOSTS)",
                    base
                ),
                confidence: 0.0,
            });
        }

        let prompt = self.build_review_prompt(original_request, changes_description, diff_or_code);

        let messages = vec![
            ChatMessage::system("你是一个严格的代码审查专家。只输出 JSON 格式的审查结果。"),
            ChatMessage::user(&prompt),
        ];

        let response = client.chat(&messages, None, None).await?;
        let content = response.content.unwrap_or_default();

        self.parse_review_response(&content)
    }

    /// 解析 LLM 审查响应
    fn parse_review_response(&self, content: &str) -> Result<ReviewResult> {
        let json_str = extract_json_from_markdown(content);

        match serde_json::from_str::<serde_json::Value>(&json_str) {
            Ok(val) => {
                // 修复(审查):缺失 approved 字段时 fail-close(false),不再视为通过。
                let mut approved = val
                    .get("approved")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let confidence = val
                    .get("confidence")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.5) as f32;
                let summary = val
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("No summary")
                    .to_string();

                let findings = val
                    .get("findings")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|item| {
                                let severity_str = item
                                    .get("severity")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("suggestion");
                                let severity = match severity_str {
                                    "critical" => ReviewSeverity::Critical,
                                    "error" => ReviewSeverity::Error,
                                    "warning" => ReviewSeverity::Warning,
                                    _ => ReviewSeverity::Suggestion,
                                };
                                ReviewFinding {
                                    severity,
                                    category: item
                                        .get("category")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("unknown")
                                        .to_string(),
                                    message: item
                                        .get("message")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    file: item
                                        .get("file")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string()),
                                    line: item
                                        .get("line")
                                        .and_then(|v| v.as_u64())
                                        .map(|v| v as u32),
                                    suggestion: item
                                        .get("suggestion")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string()),
                                }
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                // 修复(审查):LLM 输出含 Critical/Error 级发现却仍标 approved(前后矛盾),
                // 按 fail-close 处理 —— 存在阻塞级发现即不得判通过,避免安全关键变更被放行。
                if findings
                    .iter()
                    .any(|f| matches!(f.severity, ReviewSeverity::Critical | ReviewSeverity::Error))
                {
                    approved = false;
                }

                Ok(ReviewResult {
                    approved,
                    findings,
                    summary,
                    confidence,
                })
            }
            Err(_) => {
                // 修复(G-H4):原实现解析失败时 approved:true,把"无法审查"伪装成
                // "审查通过",安全关键路径 fail-open。改为 fail-close:解析失败视为
                // "未通过审查",让调用方知道需要人工介入。
                let preview =
                    &json_str[..crate::common::utils::previous_char_boundary(&json_str, 200)];
                Ok(ReviewResult {
                    approved: false,
                    findings: vec![],
                    summary: format!("审查结果解析失败,无法判定是否通过(请人工复核): {}", preview),
                    confidence: 0.0,
                })
            }
        }
    }
}

/// 从 Markdown 代码块中提取 JSON
fn extract_json_from_markdown(content: &str) -> String {
    if let Some(start) = content.find("```json") {
        let after_start = start + 7;
        if let Some(end) = content[after_start..].find("```") {
            return content[after_start..after_start + end].trim().to_string();
        }
    }
    if let Some(start) = content.find("```") {
        let after_start = start + 3;
        if let Some(end) = content[after_start..].find("```") {
            return content[after_start..after_start + end].trim().to_string();
        }
    }
    if content.trim().starts_with('{')
        && let Some(end) = content.rfind('}')
    {
        return content[..=end].to_string();
    }
    content.trim().to_string()
}
