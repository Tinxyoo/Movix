use serde::{Deserialize, Serialize};

/// 子任务建议的执行角色（原 sub_agent 模块内联，多智能体已移除）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubAgentRole {
    Searcher,
    Coder,
    Reviewer,
    Tester,
    Planner,
    General,
}

impl SubAgentRole {
    pub fn display_name(&self) -> &str {
        match self {
            SubAgentRole::Searcher => "搜索",
            SubAgentRole::Coder => "编码",
            SubAgentRole::Reviewer => "审查",
            SubAgentRole::Tester => "测试",
            SubAgentRole::Planner => "规划",
            SubAgentRole::General => "通用",
        }
    }

    pub fn icon(&self) -> &str {
        match self {
            SubAgentRole::Searcher => "🔍",
            SubAgentRole::Coder => "💻",
            SubAgentRole::Reviewer => "👀",
            SubAgentRole::Tester => "🧪",
            SubAgentRole::Planner => "📋",
            SubAgentRole::General => "⚙️",
        }
    }
}

/// 子任务优先级（原 sub_agent 模块内联）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskPriority {
    High,
    Normal,
    Low,
}

/// 任务分解策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecompositionStrategy {
    /// 子任务可完全并行执行
    Parallel,
    /// 子任务必须按顺序执行
    Sequential,
    /// 子任务有依赖关系，形成有向无环图
    DAG,
    /// 子任务形成流水线，前一个的输出是后一个的输入
    Pipeline,
}

impl DecompositionStrategy {
    /// 获取策略显示名称
    pub fn display_name(&self) -> &str {
        match self {
            DecompositionStrategy::Parallel => "并行",
            DecompositionStrategy::Sequential => "顺序",
            DecompositionStrategy::DAG => "依赖图",
            DecompositionStrategy::Pipeline => "流水线",
        }
    }
}

/// 分解后的子任务描述
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecomposedSubTask {
    /// 子任务序号
    pub step: u32,
    /// 子任务描述
    pub description: String,
    /// 建议的Agent角色
    pub suggested_role: SubAgentRole,
    /// 依赖的子任务序号列表
    pub depends_on: Vec<u32>,
    /// 优先级
    pub priority: TaskPriority,
}

/// 任务分解结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecomposedTask {
    /// 原始任务描述
    pub original_task: String,
    /// 分解策略
    pub strategy: DecompositionStrategy,
    /// 子任务列表
    pub sub_tasks: Vec<DecomposedSubTask>,
    /// 结果聚合提示
    pub aggregation_hint: String,
}

impl DecomposedTask {
    /// 获取执行层级（用于并行调度，同一层级的任务可并行执行）
    pub fn execution_layers(&self) -> Vec<Vec<u32>> {
        let nodes: Vec<u32> = self.sub_tasks.iter().map(|st| st.step).collect();
        let dep_map: std::collections::HashMap<u32, Vec<u32>> = self
            .sub_tasks
            .iter()
            .map(|st| (st.step, st.depends_on.clone()))
            .collect();
        crate::common::utils::topological_layers(&nodes, &dep_map)
    }

    /// 获取子任务数量
    pub fn task_count(&self) -> usize {
        self.sub_tasks.len()
    }

    /// 判断是否需要并行执行
    pub fn has_parallelism(&self) -> bool {
        let layers = self.execution_layers();
        layers.iter().any(|layer| layer.len() > 1)
    }
}

/// LLM返回的任务分解JSON结构
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LLMDecompositionResponse {
    task: String,
    strategy: String,
    sub_tasks: Vec<LLMSubTask>,
    aggregation_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LLMSubTask {
    step: u32,
    description: String,
    role: String,
    depends_on: Vec<u32>,
    priority: Option<String>,
}

/// 任务分解器，使用LLM将复杂任务分解为子任务
pub struct TaskDecomposer {
    /// 最大子任务数量
    max_sub_tasks: usize,
    /// 是否启用任务分解
    enabled: bool,
    /// 触发分解的最小任务复杂度阈值
    complexity_threshold: f64,
}

impl TaskDecomposer {
    /// 创建新的任务分解器
    pub fn new() -> Self {
        Self {
            max_sub_tasks: 6,
            enabled: true,
            complexity_threshold: 0.6,
        }
    }

    /// 设置是否启用
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// 查询是否已启用
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// 设置最大子任务数量
    pub fn with_max_sub_tasks(mut self, max: usize) -> Self {
        self.max_sub_tasks = max.clamp(2, 10);
        self
    }

    /// 设置复杂度阈值
    pub fn with_complexity_threshold(mut self, threshold: f64) -> Self {
        self.complexity_threshold = threshold.clamp(0.0, 1.0);
        self
    }

    /// 判断任务是否需要分解
    pub fn should_decompose(&self, task: &str, complexity: f64) -> bool {
        if !self.enabled {
            return false;
        }
        if complexity < self.complexity_threshold {
            return false;
        }
        let multi_step_markers = [
            "同时",
            "并且",
            "以及",
            "另外",
            "然后",
            "多个",
            "一系列",
            "全部",
            "所有",
            "重构",
            "迁移",
            "重写",
            "重新设计",
            "and",
            "also",
            "additionally",
            "then",
            "after",
            "refactor",
            "migrate",
            "rewrite",
            "redesign",
        ];
        let task_lower = task.to_lowercase();
        let marker_count = multi_step_markers
            .iter()
            .filter(|m| task_lower.contains(*m))
            .count();
        marker_count >= 1 || task.len() > 200
    }

    /// 生成任务分解提示词，供LLM分析
    pub fn build_decomposition_prompt(task: &str, max_sub_tasks: usize) -> String {
        format!(
            r#"分析以下编码任务，将其分解为可独立执行的子任务。

任务：{task}

请按以下 JSON 格式输出分解结果：
{{
    "task": "任务简述",
    "strategy": "parallel 或 sequential 或 dag 或 pipeline",
    "sub_tasks": [
        {{
            "step": 1,
            "description": "子任务的具体描述，要足够详细让子Agent独立执行",
            "role": "searcher 或 coder 或 reviewer 或 tester 或 planner 或 general",
            "depends_on": [],
            "priority": "normal 或 high 或 low"
        }}
    ],
    "aggregation_hint": "如何聚合各子任务的结果"
}}

分解原则：
- 每个子任务应该明确、可独立执行
- 先调查再修改（先搜索/阅读，再编码/修改）
- 搜索类任务用 searcher 角色，编码类用 coder，审查用 reviewer，测试用 tester
- 没有依赖关系的子任务应标记为可并行（depends_on 为空数组）
- 子任务数量控制在 2-{max_sub_tasks} 个
- strategy 选择：无依赖用 parallel，全依赖用 sequential，部分依赖用 dag，链式传递用 pipeline
- 只输出 JSON，不要其他内容"#
        )
    }

    /// 解析LLM返回的分解结果JSON
    pub fn parse_decomposition(json_str: &str, original_task: &str) -> Option<DecomposedTask> {
        let cleaned = json_str
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();

        let llm_response: LLMDecompositionResponse = match serde_json::from_str(cleaned) {
            Ok(r) => r,
            Err(e) => {
                // 修复(Medium #M9):`&cleaned[..200]` 按字节切,落在 UTF-8 多字节字符
                // 中间会 panic(debug)或产生无效 UTF-8。改用 previous_char_boundary。
                let boundary = crate::common::utils::previous_char_boundary(cleaned, 200);
                tracing::warn!(
                    "LLM decomposition JSON 解析失败: {}; 原文前200: {}",
                    e,
                    &cleaned[..boundary]
                );
                return None;
            }
        };

        let strategy = match llm_response.strategy.to_lowercase().as_str() {
            "parallel" => DecompositionStrategy::Parallel,
            "sequential" => DecompositionStrategy::Sequential,
            "pipeline" => DecompositionStrategy::Pipeline,
            _ => DecompositionStrategy::DAG,
        };

        let sub_tasks: Vec<DecomposedSubTask> = llm_response
            .sub_tasks
            .into_iter()
            .map(|st| {
                let role = parse_role(&st.role);
                let priority = parse_priority(st.priority.as_deref());
                // 修复(S16):description 限长,防 LLM 注入超长文本撑爆上下文。
                let description = if st.description.len() > 2000 {
                    let b = crate::common::utils::previous_char_boundary(&st.description, 2000);
                    st.description[..b].to_string()
                } else {
                    st.description
                };
                DecomposedSubTask {
                    step: st.step,
                    description,
                    suggested_role: role,
                    depends_on: st.depends_on,
                    priority,
                }
            })
            .collect();

        if sub_tasks.is_empty() {
            return None;
        }

        // 修复(S16,关键):LLM 返回的 sub_tasks 此前无任何校验:step 可重复/跳号,
        // depends_on 可指向不存在的 step(悬空依赖),会让下游 topological_layers
        // 行为不可预期(可能死循环或 panic)。这里做最小校验:
        // ① 收集所有合法 step 编号集合;② 过滤掉 depends_on 中指向不存在 step 的项。
        let valid_steps: std::collections::HashSet<u32> =
            sub_tasks.iter().map(|t| t.step).collect();
        let sub_tasks: Vec<DecomposedSubTask> = sub_tasks
            .into_iter()
            .map(|mut t| {
                t.depends_on.retain(|d| valid_steps.contains(d));
                t
            })
            .collect();

        Some(DecomposedTask {
            original_task: original_task.to_string(),
            strategy,
            sub_tasks,
            aggregation_hint: llm_response.aggregation_hint,
        })
    }

    /// 基于规则的任务分解（不依赖LLM，用于快速分解简单任务）
    pub fn rule_based_decompose(task: &str) -> Option<DecomposedTask> {
        let task_lower = task.to_lowercase();

        let has_search = task_lower.contains("搜索")
            || task_lower.contains("查找")
            || task_lower.contains("search")
            || task_lower.contains("find");
        let has_code = task_lower.contains("修改")
            || task_lower.contains("实现")
            || task_lower.contains("编写")
            || task_lower.contains("fix")
            || task_lower.contains("implement")
            || task_lower.contains("write");
        let has_review = task_lower.contains("审查")
            || task_lower.contains("检查")
            || task_lower.contains("review")
            || task_lower.contains("check");
        let has_test = task_lower.contains("测试")
            || task_lower.contains("验证")
            || task_lower.contains("test")
            || task_lower.contains("verify");

        let mut sub_tasks = Vec::new();
        let mut step = 0u32;

        if has_search {
            step += 1;
            sub_tasks.push(DecomposedSubTask {
                step,
                description: format!("搜索和分析相关代码：{}", task),
                suggested_role: SubAgentRole::Searcher,
                depends_on: Vec::new(),
                priority: TaskPriority::High,
            });
        }

        if has_code {
            step += 1;
            let deps: Vec<u32> = if has_search { vec![1] } else { Vec::new() };
            sub_tasks.push(DecomposedSubTask {
                step,
                description: format!("编写/修改代码：{}", task),
                suggested_role: SubAgentRole::Coder,
                depends_on: deps,
                priority: TaskPriority::High,
            });
        }

        if has_review {
            step += 1;
            let deps: Vec<u32> = if has_code {
                vec![step - 1]
            } else if has_search {
                vec![1]
            } else {
                Vec::new()
            };
            sub_tasks.push(DecomposedSubTask {
                step,
                description: format!("审查代码变更：{}", task),
                suggested_role: SubAgentRole::Reviewer,
                depends_on: deps,
                priority: TaskPriority::Normal,
            });
        }

        if has_test {
            step += 1;
            let deps: Vec<u32> = if has_code { vec![step - 1] } else { Vec::new() };
            sub_tasks.push(DecomposedSubTask {
                step,
                description: format!("测试验证：{}", task),
                suggested_role: SubAgentRole::Tester,
                depends_on: deps,
                priority: TaskPriority::Normal,
            });
        }

        if sub_tasks.is_empty() {
            return None;
        }

        if sub_tasks.len() == 1 {
            return None;
        }

        let strategy = if sub_tasks.iter().all(|st| st.depends_on.is_empty()) {
            DecompositionStrategy::Parallel
        } else if sub_tasks.iter().all(|st| st.depends_on.len() <= 1) {
            DecompositionStrategy::DAG
        } else {
            DecompositionStrategy::Sequential
        };

        Some(DecomposedTask {
            original_task: task.to_string(),
            strategy,
            sub_tasks,
            aggregation_hint: "合并所有子Agent的结果，按步骤顺序组织最终回复".to_string(),
        })
    }

    /// 获取最大子任务数
    pub fn max_sub_tasks(&self) -> usize {
        self.max_sub_tasks
    }
}

impl Default for TaskDecomposer {
    fn default() -> Self {
        Self::new()
    }
}

/// 解析LLM返回的角色字符串
fn parse_role(role_str: &str) -> SubAgentRole {
    match role_str.to_lowercase().as_str() {
        "searcher" | "search" | "搜索" => SubAgentRole::Searcher,
        "coder" | "code" | "编码" | "编写" => SubAgentRole::Coder,
        "reviewer" | "review" | "审查" => SubAgentRole::Reviewer,
        "tester" | "test" | "测试" => SubAgentRole::Tester,
        "planner" | "plan" | "规划" => SubAgentRole::Planner,
        _ => SubAgentRole::General,
    }
}

/// 解析LLM返回的优先级字符串
fn parse_priority(priority_str: Option<&str>) -> TaskPriority {
    match priority_str.map(|s| s.to_lowercase()).as_deref() {
        Some("high") | Some("critical") => TaskPriority::High,
        Some("low") => TaskPriority::Low,
        _ => TaskPriority::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rule_based_decompose() {
        let task = "搜索并修改 agent.rs 中的错误处理逻辑，然后测试验证";
        let result = TaskDecomposer::rule_based_decompose(task);
        assert!(result.is_some());

        let decomposed = result.unwrap();
        assert!(decomposed.sub_tasks.len() >= 2);
        assert!(decomposed.has_parallelism() || !decomposed.has_parallelism());
    }

    #[test]
    fn test_execution_layers() {
        let task = DecomposedTask {
            original_task: "test".to_string(),
            strategy: DecompositionStrategy::DAG,
            sub_tasks: vec![
                DecomposedSubTask {
                    step: 1,
                    description: "搜索".to_string(),
                    suggested_role: SubAgentRole::Searcher,
                    depends_on: vec![],
                    priority: TaskPriority::High,
                },
                DecomposedSubTask {
                    step: 2,
                    description: "编码".to_string(),
                    suggested_role: SubAgentRole::Coder,
                    depends_on: vec![1],
                    priority: TaskPriority::High,
                },
                DecomposedSubTask {
                    step: 3,
                    description: "测试".to_string(),
                    suggested_role: SubAgentRole::Tester,
                    depends_on: vec![2],
                    priority: TaskPriority::Normal,
                },
            ],
            aggregation_hint: "合并结果".to_string(),
        };

        let layers = task.execution_layers();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0], vec![1]);
        assert_eq!(layers[1], vec![2]);
        assert_eq!(layers[2], vec![3]);
    }

    #[test]
    fn test_parallel_layers() {
        let task = DecomposedTask {
            original_task: "test".to_string(),
            strategy: DecompositionStrategy::Parallel,
            sub_tasks: vec![
                DecomposedSubTask {
                    step: 1,
                    description: "搜索A".to_string(),
                    suggested_role: SubAgentRole::Searcher,
                    depends_on: vec![],
                    priority: TaskPriority::Normal,
                },
                DecomposedSubTask {
                    step: 2,
                    description: "搜索B".to_string(),
                    suggested_role: SubAgentRole::Searcher,
                    depends_on: vec![],
                    priority: TaskPriority::Normal,
                },
                DecomposedSubTask {
                    step: 3,
                    description: "编码".to_string(),
                    suggested_role: SubAgentRole::Coder,
                    depends_on: vec![1, 2],
                    priority: TaskPriority::High,
                },
            ],
            aggregation_hint: "合并结果".to_string(),
        };

        let layers = task.execution_layers();
        assert_eq!(layers.len(), 2);
        assert!(layers[0].contains(&1) && layers[0].contains(&2));
        assert_eq!(layers[1], vec![3]);
        assert!(task.has_parallelism());
    }

    #[test]
    fn test_should_decompose() {
        let decomposer = TaskDecomposer::new();

        assert!(!decomposer.should_decompose("查看当前目录", 0.3));
        assert!(decomposer.should_decompose("重构整个项目的错误处理并添加测试", 0.8));
    }

    #[test]
    fn test_parse_decomposition() {
        let json = r#"{
            "task": "重构错误处理",
            "strategy": "dag",
            "sub_tasks": [
                {
                    "step": 1,
                    "description": "搜索现有错误处理代码",
                    "role": "searcher",
                    "depends_on": [],
                    "priority": "high"
                },
                {
                    "step": 2,
                    "description": "修改错误处理逻辑",
                    "role": "coder",
                    "depends_on": [1],
                    "priority": "high"
                }
            ],
            "aggregation_hint": "合并搜索和修改结果"
        }"#;

        let result = TaskDecomposer::parse_decomposition(json, "重构错误处理");
        assert!(result.is_some());

        let decomposed = result.unwrap();
        assert_eq!(decomposed.sub_tasks.len(), 2);
        assert_eq!(decomposed.strategy, DecompositionStrategy::DAG);
    }
}
