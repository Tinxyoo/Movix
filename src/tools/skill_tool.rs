use async_trait::async_trait;
use serde_json::{Value, json};

use super::{EffectKind, RiskLevel, Tool, ToolResult};
use crate::common::error::Result;
use crate::common::skill_executor;
use crate::context::skills::SkillRegistry;

/// 列出所有可用Skill的工具
pub struct ListSkillsTool {
    skill_registry: SkillRegistry,
}

impl ListSkillsTool {
    /// 创建列出Skill工具
    pub fn new(skill_registry: SkillRegistry) -> Self {
        Self { skill_registry }
    }
}

#[async_trait]
impl Tool for ListSkillsTool {
    fn name(&self) -> &str {
        "list_skills"
    }
    fn description(&self) -> &str {
        "列出所有可用的技能（Skills），包括内置和自定义技能"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "type_filter": {
                    "type": "string",
                    "description": "按类型过滤: prompt/tool/workflow/template",
                    "enum": ["prompt", "tool", "workflow", "template"]
                }
            }
        })
    }

    async fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let type_filter = arguments["type_filter"].as_str();

        let skills = if let Some(filter) = type_filter {
            let st = match filter {
                "tool" => crate::context::skills::SkillType::Tool,
                "workflow" => crate::context::skills::SkillType::Workflow,
                "template" => crate::context::skills::SkillType::Template,
                _ => crate::context::skills::SkillType::Prompt,
            };
            self.skill_registry.list_by_type(st)
        } else {
            self.skill_registry.list_enabled()
        };

        if skills.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "没有找到匹配的技能".to_string(),
                error: None,
            });
        }

        let mut output = String::new();
        output.push_str(&format!("可用技能 ({}个):\n", skills.len()));

        for skill in skills {
            output.push_str(&format!(
                "- {} {} [{}]: {}\n",
                skill.skill_type.icon(),
                skill.name,
                skill.skill_type.display_name(),
                skill.description
            ));
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }

    fn parallel_safe(&self) -> bool {
        true
    }
    fn effect_kind(&self) -> EffectKind {
        EffectKind::ReadOnly
    }
}

/// 使用指定Skill的工具
pub struct UseSkillTool {
    skill_registry: SkillRegistry,
}

impl UseSkillTool {
    /// 创建使用Skill工具
    pub fn new(skill_registry: SkillRegistry) -> Self {
        Self { skill_registry }
    }
}

#[async_trait]
impl Tool for UseSkillTool {
    fn name(&self) -> &str {
        "use_skill"
    }
    fn description(&self) -> &str {
        "使用指定名称的技能来处理任务。技能会提供专业的方法论和指导"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "技能名称，如 code-review, refactor, test-gen, explain, debug, security-audit, api-design, perf-optimize"
                },
                "task": {
                    "type": "string",
                    "description": "要使用该技能处理的任务描述"
                }
            },
            "required": ["skill_name", "task"]
        })
    }

    async fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let skill_name = arguments["skill_name"].as_str().unwrap_or("");
        let task = arguments["task"].as_str().unwrap_or("");

        if skill_name.is_empty() {
            return Ok(ToolResult::err("必须指定技能名称".to_string()));
        }

        let skill = match self.skill_registry.get(skill_name) {
            Some(s) => s,
            None => {
                let suggestions = self.skill_registry.match_skill(skill_name);
                let hint = if !suggestions.is_empty() {
                    let names: Vec<String> = suggestions
                        .iter()
                        .take(3)
                        .map(|m| m.skill.name.clone())
                        .collect();
                    format!("。相似的技能: {}", names.join(", "))
                } else {
                    String::new()
                };
                return Ok(ToolResult::err(format!(
                    "未找到技能: {}{}",
                    skill_name, hint
                )));
            }
        };

        if !skill.enabled {
            return Ok(ToolResult::err(format!(
                "技能 {} 已被禁用，请先启用",
                skill_name
            )));
        }

        let result = skill_executor::execute(skill, task);

        Ok(ToolResult {
            success: result.success,
            output: result.output,
            error: result.error,
        })
    }

    fn effect_kind(&self) -> EffectKind {
        // Skill 内部可能执行任意工具(读、写、shell、网络)——保守标 Composite,
        // 让 ModePolicy 走最严策略,而不是按 ReadOnly 漏判。
        EffectKind::Composite
    }

    fn risk_level(&self, _args: &Value) -> RiskLevel {
        // 同上:Skill 副作用不可预知,默认 Medium 起,具体由 ModePolicy 加权。
        RiskLevel::Medium
    }

    fn requires_approval_hint(&self, _args: &Value) -> bool {
        // Composite 默认建议审批——只有 ModePolicy 明确放行才会跳过。
        true
    }

    fn parallel_safe(&self) -> bool {
        // Skill 执行可能涉及任意工具链,并发安全需 Skill 自己声明,这里给 false。
        false
    }
}
