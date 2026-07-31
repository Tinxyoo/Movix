use crate::common::deepseek::ChatMessage;
use serde::{Deserialize, Serialize};

// 修复(M1):删除 ProMax 第三档,使 ModelType 与 switch_model 白名单
// (deepseek-v4-pro / deepseek-v4-flash / deepseek-chat)一致。ProMax 此前仅由
// selector 内部推荐,从未接入 API 客户端(switch_model 会拒绝),属未接线的死档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ModelType {
    Flash,
    #[default]
    Pro,
}

impl ModelType {
    pub fn name(&self) -> &'static str {
        match self {
            ModelType::Flash => "deepseek-v4-flash",
            ModelType::Pro => "deepseek-v4-pro",
        }
    }

    pub fn is_pro(&self) -> bool {
        matches!(self, ModelType::Pro)
    }

    pub fn cost_efficiency(&self) -> f64 {
        match self {
            ModelType::Flash => 1.0,
            ModelType::Pro => 0.5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskComplexity {
    Low,
    Medium,
    High,
    Extreme,
}

impl TaskComplexity {
    pub fn recommended_model(&self) -> ModelType {
        match self {
            TaskComplexity::Low => ModelType::Flash,
            TaskComplexity::Medium => ModelType::Pro,
            TaskComplexity::High => ModelType::Pro,
            // 修复(M1):Extreme 原推荐 ProMax(未接线),现回退到 Pro。
            TaskComplexity::Extreme => ModelType::Pro,
        }
    }

    pub fn recommended_effort(&self) -> &'static str {
        match self {
            TaskComplexity::Low => "low",
            TaskComplexity::Medium => "medium",
            TaskComplexity::High => "high",
            TaskComplexity::Extreme => "max",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub model_type: ModelType,
    pub reasoning_effort: String,
    pub thinking_budget: usize,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            model_type: ModelType::Pro,
            reasoning_effort: "medium".to_string(),
            thinking_budget: 16000,
        }
    }
}

impl ModelConfig {
    pub fn for_task(task: TaskComplexity) -> Self {
        Self {
            model_type: task.recommended_model(),
            reasoning_effort: task.recommended_effort().to_string(),
            thinking_budget: match task {
                TaskComplexity::Low => 8000,
                TaskComplexity::Medium => 16000,
                TaskComplexity::High => 32000,
                TaskComplexity::Extreme => 64000,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskAnalysis {
    pub complexity: TaskComplexity,
    pub is_coding_task: bool,
    pub is_long_context: bool,
    pub is_multi_step: bool,
    pub keywords: Vec<String>,
    pub confidence: f64,
}

impl TaskAnalysis {
    pub fn analyze(task: &str, history_size: usize, has_tool_calls: bool) -> Self {
        let keywords = extract_keywords(task);
        let complexity = estimate_complexity(task, &keywords, history_size, has_tool_calls);
        let is_coding_task = detect_coding_task(task, &keywords);
        let is_long_context = history_size > 50 || task.len() > 5000;
        let is_multi_step = detect_multi_step(task, &keywords);

        let confidence = calculate_confidence(&complexity, is_coding_task, is_long_context);

        Self {
            complexity,
            is_coding_task,
            is_long_context,
            is_multi_step,
            keywords,
            confidence,
        }
    }

    pub fn recommended_model(&self) -> ModelType {
        if self.confidence < 0.5 {
            return ModelType::Pro;
        }

        match self.complexity {
            TaskComplexity::Low if !self.is_multi_step => ModelType::Flash,
            TaskComplexity::Medium | TaskComplexity::High => ModelType::Pro,
            // 修复(M1):Extreme 原推荐 ProMax(未接线),现回退到 Pro。
            TaskComplexity::Extreme => ModelType::Pro,
            _ => ModelType::Pro,
        }
    }
}

/// 统一关键词权重表：同时用于 extract_keywords、estimate_complexity 和 detect_coding_task
/// 权重: 3=复杂, 2=中等, 1=简单; is_coding=true 表示编程类关键词
const KEYWORD_TABLE: &[(&str, i32, bool)] = &[
    // 复杂 (权重3)
    ("重构", 3, true),
    ("refactor", 3, true),
    ("重写", 3, true),
    ("rewrite", 3, true),
    ("迁移", 3, true),
    ("migrate", 3, true),
    ("架构", 3, true),
    ("architecture", 3, true),
    // 中等 (权重2)
    ("优化", 2, true),
    ("optimize", 2, true),
    ("实现", 2, true),
    ("implement", 2, true),
    ("设计", 2, true),
    ("design", 2, true),
    // 简单 (权重1)
    ("修复", 1, true),
    ("fix", 1, true),
    ("创建", 1, true),
    ("create", 1, true),
    // 无权重/仅检测
    ("bug", 0, true),
    ("测试", 0, true),
    ("test", 0, true),
    ("审查", 0, true),
    ("review", 0, true),
    ("分析", 0, true),
    ("analyze", 0, true),
    ("评估", 0, true),
    ("evaluate", 0, true),
    ("构建", 0, true),
    ("build", 0, true),
    ("调试", 0, true),
    ("debug", 0, true),
    ("排错", 0, true),
    ("troubleshoot", 0, true),
    // 编程类关键词
    ("代码", 0, true),
    ("code", 0, true),
    ("函数", 0, true),
    ("function", 0, true),
    ("类", 0, true),
    ("class", 0, true),
    ("文件", 0, true),
    ("file", 0, true),
    ("模块", 0, true),
    ("module", 0, true),
    ("接口", 0, true),
    ("api", 0, true),
    ("变量", 0, true),
    ("variable", 0, true),
    ("算法", 0, true),
    ("algorithm", 0, true),
    ("编译", 0, true),
    ("compile", 0, true),
    ("rust", 0, true),
    ("python", 0, true),
    ("javascript", 0, true),
    ("typescript", 0, true),
    ("go", 0, true),
    ("java", 0, true),
    ("查看", 0, false),
    ("view", 0, false),
];

/// 多步骤关键词
const MULTI_STEP_KEYWORDS: &[&str] = &[
    "首先",
    "then",
    "之后",
    "after",
    "最后",
    "finally",
    "下一步",
    "next",
    "接着",
    "多个",
    "multiple",
    "一系列",
    "series",
];

fn extract_keywords(task: &str) -> Vec<String> {
    let task_lower = task.to_lowercase();
    KEYWORD_TABLE
        .iter()
        .filter(|(m, _, _)| task_lower.contains(*m))
        .map(|(s, _, _)| s.to_string())
        .collect()
}

fn estimate_complexity(
    task: &str,
    keywords: &[String],
    history_size: usize,
    has_tool_calls: bool,
) -> TaskComplexity {
    // 用权重表直接查表，一次遍历 keywords 累加分数
    let keyword_set: std::collections::HashSet<&str> =
        keywords.iter().map(|s| s.as_str()).collect();
    let score: i32 = KEYWORD_TABLE
        .iter()
        .filter(|(m, _, _)| keyword_set.contains(*m))
        .map(|(_, w, _)| *w)
        .sum();

    let mut score = score;

    if task.len() > 1000 {
        score += 2;
    } else if task.len() > 500 {
        score += 1;
    }

    if history_size > 30 {
        score += 2;
    } else if history_size > 10 {
        score += 1;
    }

    if has_tool_calls {
        score += 2;
    }

    match score {
        0..=2 => TaskComplexity::Low,
        3..=5 => TaskComplexity::Medium,
        6..=8 => TaskComplexity::High,
        _ => TaskComplexity::Extreme,
    }
}

fn detect_coding_task(task: &str, keywords: &[String]) -> bool {
    let task_lower = task.to_lowercase();
    // 先检查 task 文本本身
    if KEYWORD_TABLE
        .iter()
        .any(|(m, _, is_coding)| *is_coding && task_lower.contains(*m))
    {
        return true;
    }
    // 再检查提取出的 keywords
    let keyword_set: std::collections::HashSet<&str> =
        keywords.iter().map(|s| s.as_str()).collect();
    KEYWORD_TABLE
        .iter()
        .any(|(m, _, is_coding)| *is_coding && keyword_set.contains(*m))
}

fn detect_multi_step(task: &str, keywords: &[String]) -> bool {
    let task_lower = task.to_lowercase();
    MULTI_STEP_KEYWORDS.iter().any(|m| task_lower.contains(*m)) || keywords.len() >= 3
}

fn calculate_confidence(complexity: &TaskComplexity, is_coding: bool, is_long: bool) -> f64 {
    let mut confidence: f64 = 0.5;

    match complexity {
        TaskComplexity::Low => confidence -= 0.1,
        TaskComplexity::Medium => confidence += 0.1,
        TaskComplexity::High => confidence += 0.2,
        TaskComplexity::Extreme => confidence += 0.3,
    }

    if is_coding {
        confidence += 0.1;
    }

    if is_long {
        confidence += 0.1;
    }

    confidence.clamp(0.0, 1.0)
}

#[derive(Debug, Clone)]
pub struct AdaptiveModelSelector;

impl AdaptiveModelSelector {
    pub fn new() -> Self {
        Self
    }

    pub fn select(&self, analysis: &TaskAnalysis) -> ModelConfig {
        let mut config = ModelConfig::for_task(analysis.complexity);

        // 长上下文场景:Flash 升级到 Pro,并放宽思考预算
        if analysis.is_long_context {
            if config.model_type == ModelType::Flash {
                config.model_type = ModelType::Pro;
            }
            // 修复(S9):原 `(budget * 1.5) as usize` 无上限,Extreme 的 64000×1.5=96000
            // 可能超过模型 reasoning 上限导致 400,或被当"无限制"放大计费。clamp 到
            // 一个合理上限(DeepSeek V4 单次 reasoning 上限约 64K)。
            let scaled = (config.thinking_budget as f64 * 1.5) as usize;
            config.thinking_budget = scaled.min(64_000);
        }

        config
    }

    pub fn select_for_message(&self, messages: &[ChatMessage]) -> Option<ModelConfig> {
        let last_msg = messages.last()?;
        let history_size = messages.len();
        let has_tool_calls = messages.iter().any(|m| m.tool_calls.is_some());

        let analysis = TaskAnalysis::analyze(
            last_msg.content.as_deref().unwrap_or(""),
            history_size,
            has_tool_calls,
        );

        Some(self.select(&analysis))
    }
}

impl Default for AdaptiveModelSelector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_complexity_detection() {
        let task = "重构 src/agent.rs 中的错误处理逻辑，需要修改多个模块并添加测试";
        let keywords = extract_keywords(task);
        let complexity = estimate_complexity(task, &keywords, 5, false);

        assert!(matches!(
            complexity,
            TaskComplexity::Medium | TaskComplexity::High | TaskComplexity::Extreme
        ));
    }

    #[test]
    fn test_model_selection() {
        let selector = AdaptiveModelSelector::new();

        let simple_task = TaskAnalysis::analyze("查看当前目录", 2, false);
        let config = selector.select(&simple_task);
        // 简单任务 + 无 builder 偏好 → Flash(Low 复杂度推荐 Flash)
        assert_eq!(config.model_type, ModelType::Flash);

        let complex_task = TaskAnalysis::analyze("重构整个项目的错误处理", 20, true);
        let config = selector.select(&complex_task);
        assert!(config.model_type.is_pro());
    }

    #[test]
    fn test_long_context_upgrades_to_pro() {
        let selector = AdaptiveModelSelector::new();

        // 长上下文(>50 历史)下,即使 Low 复杂度也会升级到 Pro
        let task = TaskAnalysis::analyze("查看当前目录", 60, false);
        let config = selector.select(&task);
        assert!(config.model_type.is_pro());
    }
}
