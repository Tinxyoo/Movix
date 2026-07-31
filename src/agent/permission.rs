//! P3:权限审批辅助函数。
//!
//! 提供 `await_approval` 供 agent 主循环等待用户审批决策。
//! 权限决策逻辑已直接由 [`ModeConfig`] 提供（`decide_for_tool` /
//! `should_execute_with_detail` / `record_auto_execution`），无需额外包装层。

use crate::agent::modes::ApprovalDecision;

/// 工具调用的"鉴权快照"——一次性决策,后续可被重放审计。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthzSnapshot {
    pub tool_name: String,
    pub decision: crate::agent::modes::ModeDecision,
}

/// 等待用户审批决策（从 channel 阻塞等）。
/// 传入一个 `tokio::sync::mpsc::Receiver<ApprovalDecision>`,返回用户最终决策。
/// 仅当 `ModeConfig` 返回 [`ModeDecision::NeedsApproval`] 时调用。
pub async fn await_approval(
    rx: &mut tokio::sync::mpsc::Receiver<ApprovalDecision>,
) -> ApprovalDecision {
    match rx.recv().await {
        Some(d) => d,
        None => ApprovalDecision::Denied, // channel 关闭 = 视为拒绝
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::modes::{AppMode, ModeConfig, ModeDecision};
    use crate::tools::Tool;
    use async_trait::async_trait;
    use serde_json::json;

    /// 仿真 WriteFileTool,标记 Medium 风险。
    struct FakeWriteTool;
    #[async_trait]
    impl Tool for FakeWriteTool {
        fn name(&self) -> &str {
            "write_file"
        }
        fn description(&self) -> &str {
            "fake"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({})
        }
        async fn execute(
            &self,
            _args: &serde_json::Value,
        ) -> crate::common::error::Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
        fn risk_level(&self, _args: &serde_json::Value) -> crate::tools::RiskLevel {
            crate::tools::RiskLevel::Medium
        }
    }

    struct FakeReadTool;
    #[async_trait]
    impl Tool for FakeReadTool {
        fn name(&self) -> &str {
            "read_file"
        }
        fn description(&self) -> &str {
            "fake"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({})
        }
        async fn execute(
            &self,
            _args: &serde_json::Value,
        ) -> crate::common::error::Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn decide_routes_to_mode_policy() {
        let cfg = ModeConfig::new(AppMode::Plan);
        let d = cfg.decide_for_tool(&FakeWriteTool, &json!({"path": "x.rs"}));
        // Plan 模式:Blocked
        assert!(matches!(d, ModeDecision::Blocked(_)));
    }

    #[test]
    fn decide_by_name_falls_back_to_string_path() {
        // 字符串入口:即便没有 Tool 实例,也能走 ModeConfig 决策
        let cfg = ModeConfig::new(AppMode::Agent);
        // Agent 模式:write_file 需要审批
        assert!(matches!(
            cfg.should_execute_with_detail("write_file", "src/main.rs"),
            ModeDecision::NeedsApproval
        ));
        // Agent 模式:read_file Proceed
        assert!(matches!(
            cfg.should_execute_with_detail("read_file", "src/main.rs"),
            ModeDecision::Proceed
        ));
    }

    #[test]
    fn record_only_charges_budget_on_proceed() {
        let mut cfg = ModeConfig::new(AppMode::Auto);
        cfg.set_max_auto_writes(3);
        // Proceed + 真的执行 → 消耗一次预算
        {
            // 只有 Proceed + was_executed=true 才消耗预算
            if matches!(ModeDecision::Proceed, ModeDecision::Proceed) {
                cfg.record_auto_execution();
            }
        }
        assert_eq!(cfg.auto_writes_remaining(), 2);

        // Proceed + 没真执行(被上游拦截)→ 不消耗 (跳过 record)
        assert_eq!(cfg.auto_writes_remaining(), 2);

        // NeedsApproval → 不消耗 (不调用 record)
        assert_eq!(cfg.auto_writes_remaining(), 2);

        // Blocked → 不消耗 (不调用 record)
        assert_eq!(cfg.auto_writes_remaining(), 2);
    }

    #[test]
    fn plan_mode_blocks_even_readonly_proceeds() {
        // Plan 模式:读工具 Proceed,写工具 Blocked
        let cfg = ModeConfig::new(AppMode::Plan);
        assert!(matches!(
            cfg.decide_for_tool(&FakeReadTool, &json!({"path": "x.rs"})),
            ModeDecision::Proceed
        ));
        assert!(matches!(
            cfg.decide_for_tool(&FakeWriteTool, &json!({"path": "x.rs"})),
            ModeDecision::Blocked(_)
        ));
    }

    #[tokio::test]
    async fn await_approval_returns_denied_when_channel_closes() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        drop(tx); // 立即关闭
        let d = await_approval(&mut rx).await;
        assert_eq!(d, ApprovalDecision::Denied);
    }

    #[tokio::test]
    async fn await_approval_returns_user_decision() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let _ = tx.send(ApprovalDecision::Approved).await;
        let d = await_approval(&mut rx).await;
        assert_eq!(d, ApprovalDecision::Approved);
    }
}
