pub const DEFAULT_LARGE_OUTPUT_THRESHOLD_TOKENS: usize = 4_096;

#[derive(Debug, Clone)]
pub struct LargeOutputConfig {
    pub threshold_tokens: usize,
    pub max_summary_chars: usize,
}

impl Default for LargeOutputConfig {
    fn default() -> Self {
        Self {
            threshold_tokens: DEFAULT_LARGE_OUTPUT_THRESHOLD_TOKENS,
            max_summary_chars: 2000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingDecision {
    PassThrough,
    Truncate,
}

pub struct LargeOutputRouter {
    config: LargeOutputConfig,
}

impl LargeOutputRouter {
    pub fn new(config: LargeOutputConfig) -> Self {
        Self { config }
    }

    pub fn route(&self, _tool_name: &str, output: &str) -> RoutingDecision {
        let threshold = self.config.threshold_tokens;
        let estimated = estimate_tokens(output);

        if estimated <= threshold {
            RoutingDecision::PassThrough
        } else {
            RoutingDecision::Truncate
        }
    }

    fn truncate_output(&self, output: &str) -> String {
        let max_chars = self.config.max_summary_chars;
        // 修复(M6/M8):原实现三处口径混用——字节守卫(`output.len() <= max_chars`)、
        // 字符截断(`chars().take(max_chars)`)、字节差额(`output.len() - max_chars`)。
        // CJK 下:① 截断结果(最多 max_chars 字符)可达 max_chars×3 字节,超过"字节预算";
        // ② 报告的"省略 N 字符"用字节总数减字符预算,严重虚高。
        // 统一为**字符**口径:按字符数判断、截断、计算省略量。
        let total_chars = output.chars().count();
        if total_chars <= max_chars {
            return output.to_string();
        }

        let mut truncated: String = output.chars().take(max_chars).collect();
        let omitted = total_chars - max_chars;
        truncated.push_str(&format!(
            "\n\n... [输出已截断: 省略约 {} 字符, 原始输出约 {} tokens]",
            omitted,
            estimate_tokens(output),
        ));
        truncated
    }

    pub fn smart_truncate(&self, output: &str) -> String {
        let max_chars = self.config.max_summary_chars;
        // 修复(M6):与 truncate_output 对齐,用字符数守卫而非字节。
        if output.chars().count() <= max_chars {
            return output.to_string();
        }

        let lines: Vec<&str> = output.lines().collect();
        // 修复: ≤10 行时走纯截断在 11-20 行临界点输出形态突变(head+tail 只留
        // 10 行,而纯截断可能保留全部 ~2000 字符)。放宽到 ≤20 行,避免小输出反而丢信息。
        if lines.len() <= 20 {
            return self.truncate_output(output);
        }

        let head_count = 5.min(lines.len() / 3);
        let tail_count = 5.min(lines.len() / 3);

        let head: Vec<&str> = lines.iter().take(head_count).copied().collect();
        let tail: Vec<&str> = lines
            .iter()
            .rev()
            .take(tail_count)
            .copied()
            .collect::<Vec<&str>>()
            .into_iter()
            .rev()
            .collect();

        let omitted_lines = lines.len() - head_count - tail_count;
        let estimated_tokens = estimate_tokens(output);

        let mut result = head.join("\n");
        result.push_str(&format!(
            "\n\n... [省略 {} 行, 原始输出约 {} tokens]\n\n",
            omitted_lines, estimated_tokens
        ));
        result.push_str(&tail.join("\n"));

        result
    }
}

impl Default for LargeOutputRouter {
    fn default() -> Self {
        Self::new(LargeOutputConfig::default())
    }
}

pub fn estimate_tokens(text: &str) -> usize {
    crate::common::utils::estimate_tokens_str(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_passthrough() {
        let router = LargeOutputRouter::default();
        let short_output = "hello world";
        assert_eq!(
            router.route("read_file", short_output),
            RoutingDecision::PassThrough
        );
    }

    #[test]
    fn test_truncate_decision() {
        let router = LargeOutputRouter::default();
        let long_output = "x".repeat(20_000);
        assert!(matches!(
            router.route("execute_shell", &long_output),
            RoutingDecision::Truncate
        ));
    }

    #[test]
    fn test_smart_truncate() {
        let router = LargeOutputRouter::default();
        let lines: Vec<String> = (0..500)
            .map(|i| format!("line {} with some extra content to make it longer", i))
            .collect();
        let output = lines.join("\n");
        let truncated = router.smart_truncate(&output);
        assert!(truncated.len() < output.len());
        assert!(truncated.contains("省略"));
    }

    #[test]
    fn test_uniform_threshold_across_tools() {
        // per_tool_thresholds 死配置已移除：所有工具共用同一阈值。
        let router = LargeOutputRouter::default();
        let medium_output = "x".repeat(4_000);
        // read_file 和 execute_shell 用同一阈值，行为一致。
        assert!(matches!(
            router.route("read_file", &medium_output),
            RoutingDecision::PassThrough
        ));
        assert!(matches!(
            router.route("execute_shell", &medium_output),
            RoutingDecision::PassThrough
        ));
    }
}
