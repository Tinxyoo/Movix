use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::modes::{ApprovalDecision, ApprovalModification};
use crate::common::utils::truncate_str;

/// 审批请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// 请求 ID
    pub id: String,
    /// 工具名称
    pub tool_name: String,
    /// 原始参数
    pub original_arguments: String,
    /// 风险等级
    pub risk_level: RiskLevel,
    /// 工具描述
    pub description: String,
    /// 涉及的文件
    pub affected_files: Vec<String>,
}

/// 风险等级 — 与 tools::RiskLevel 统一
pub use crate::tools::RiskLevel;

/// 部分批准管理器
pub struct PartialApproval {
    /// 当前待审批请求
    pending: Option<ApprovalRequest>,
}

impl Default for PartialApproval {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialApproval {
    /// 创建部分批准管理器
    pub fn new() -> Self {
        Self { pending: None }
    }

    /// 创建审批请求
    pub fn create_request(
        &mut self,
        tool_name: &str,
        arguments: &str,
        risk_level: RiskLevel,
        description: &str,
        affected_files: Vec<String>,
    ) -> ApprovalRequest {
        let request = ApprovalRequest {
            id: format!(
                "approval_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
            ),
            tool_name: tool_name.to_string(),
            original_arguments: arguments.to_string(),
            risk_level,
            description: description.to_string(),
            affected_files,
        };
        self.pending = Some(request.clone());
        request
    }

    /// 处理审批决策
    /// 安全：如果 pending 为 None（没有待审批请求），返回 Rejected 而非空 Execute，
    /// 防止下游误判空 Execute 为合法执行指令。
    pub fn process_decision(&mut self, decision: ApprovalDecision) -> ApprovalOutcome {
        let request = self.pending.take();

        // 防御：无待审批请求时，任何决策都视为拒绝
        let request = match request {
            Some(r) => r,
            None => {
                return ApprovalOutcome::Rejected {
                    reason: "无待审批请求".into(),
                };
            }
        };

        match decision {
            ApprovalDecision::Approved => ApprovalOutcome::Execute {
                tool_name: request.tool_name.clone(),
                arguments: request.original_arguments.clone(),
            },
            ApprovalDecision::Denied => ApprovalOutcome::Rejected {
                reason: "User denied execution".into(),
            },
            ApprovalDecision::Modified(modification) => ApprovalOutcome::ExecuteWithModifications {
                tool_name: request.tool_name.clone(),
                original_arguments: request.original_arguments.clone(),
                // ⚠️ 安全警告(对抗式审查 H5/BB):`modified_arguments` 是用户输入经
                // parse_modification 合成的 JSON,key 无白名单。任何消费此变体的调用方
                // **绝不可直接执行**这些参数 —— 否则用户(或注入 UI 输入的攻击者)可把
                // `command` 改成 `rm -rf /` 或注入指向敏感文件的 `path`,绕过 ModePolicy
                // 二次校验。当前主循环(agent/mod.rs)正确地把修改后参数**回灌给 LLM**
                // 让其重新发起调用(走完整审批链),而非直接执行。新增消费方必须沿用该模式。
                modified_arguments: modification.modified_arguments,
                reason: modification.reason,
                changed_fields: modification.changed_fields,
            },
            ApprovalDecision::Explain => ApprovalOutcome::NeedExplanation {
                tool_name: request.tool_name.clone(),
                arguments: request.original_arguments.clone(),
            },
        }
    }

    /// 格式化审批请求为用户可读的文本
    pub fn format_request(request: &ApprovalRequest) -> String {
        let risk_icon = match request.risk_level {
            RiskLevel::Low => "🟢",
            RiskLevel::Medium => "🟡",
            RiskLevel::High => "🟠",
            RiskLevel::Critical => "🔴",
        };

        let mut text = format!(
            "{} [{}] {} wants to execute: {}\n",
            risk_icon,
            format!("{:?}", request.risk_level).to_uppercase(),
            request.tool_name,
            request.description
        );

        if !request.affected_files.is_empty() {
            text.push_str(&format!(
                "Affected files: {}\n",
                request.affected_files.join(", ")
            ));
        }

        text.push_str(&format!(
            "Arguments: {}\n",
            truncate_str(&request.original_arguments, 500)
        ));
        text.push_str("\nOptions: [A]pprove / [D]eny / [M]odify / [E]xplain");

        text
    }

    /// 解析用户输入的修改指令
    pub fn parse_modification(
        original_args: &str,
        user_input: &str,
    ) -> Result<ApprovalModification, String> {
        // 修复:循环前解析一次 JSON,循环内只修改 Value,循环后序列化一次。
        // 原实现每行都 from_str+to_string,O(N·parse+N·serialize)。
        let mut args_value = serde_json::from_str::<Value>(original_args)
            .map_err(|e| format!("Invalid JSON args: {}", e))?;
        let obj = args_value
            .as_object_mut()
            .ok_or_else(|| "Args is not a JSON object".to_string())?;
        let mut changed_fields = Vec::new();

        for line in user_input.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            if let Some(eq_pos) = trimmed.find('=') {
                let key = trimmed[..eq_pos].trim();
                let value = trimmed[eq_pos + 1..].trim();

                let json_value = if value.starts_with('"') && value.ends_with('"') {
                    Value::String(value[1..value.len() - 1].to_string())
                } else if value == "true" {
                    Value::Bool(true)
                } else if value == "false" {
                    Value::Bool(false)
                } else if let Ok(n) = value.parse::<i64>() {
                    Value::Number(n.into())
                } else if let Ok(n) = value.parse::<f64>() {
                    Value::Number(serde_json::Number::from_f64(n).unwrap_or_else(|| 0.into()))
                } else {
                    Value::String(value.to_string())
                };

                obj.insert(key.to_string(), json_value);
                changed_fields.push(key.to_string());
            }
        }

        let modified =
            serde_json::to_string(&args_value).unwrap_or_else(|_| original_args.to_string());

        if changed_fields.is_empty() {
            return Err("No valid modifications found. Use format: key=value".into());
        }

        Ok(ApprovalModification {
            modified_arguments: modified,
            reason: format!("User modified: {}", changed_fields.join(", ")),
            changed_fields,
        })
    }
}

/// 审批结果
#[derive(Debug, Clone)]
pub enum ApprovalOutcome {
    /// 执行原始请求
    Execute {
        tool_name: String,
        arguments: String,
    },
    /// 执行修改后的请求
    ExecuteWithModifications {
        tool_name: String,
        original_arguments: String,
        modified_arguments: String,
        reason: String,
        changed_fields: Vec<String>,
    },
    /// 拒绝
    Rejected { reason: String },
    /// 需要 Agent 解释
    NeedExplanation {
        tool_name: String,
        arguments: String,
    },
}
