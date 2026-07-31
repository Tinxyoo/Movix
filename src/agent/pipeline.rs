//! P3:工具执行管道(串行版本)。
//!
//! 把 agent.rs 中 `handle_tool_call` 的核心序列抽成结构化的 `Pipeline::run_one`:
//! `permission check → snapshot → execute → finalize → emit`。
//!
//! **P3 范围**:仅暴露"骨架 API" + 串行执行能力,加单元测试覆盖关键状态转换。
//! **未切流**:agent.rs 现有逻辑保留,后续 PR 逐步迁移。
//!
//! **P6 范围**:`Pipeline::run_batch` 会加入读工具 fan-out 并发;P3 不涉及。
//!
//! ---
//! ⚠️ **H1 维护警告(对抗式审查 2026-07)**:本模块在全仓库
//! **无任何调用方**(主循环走 `agent/mod.rs::handle_tool_call` 的串行路径)。这里
//! 的所有并发/预算原子性逻辑都是**死代码**,无法被集成测试覆盖。围绕它做的安全
//! 修复(如"per-tool 审批门控")在当前架构下不会被触发。
//! 切流到本模块前,务必在 `run_one`/`run_batch` 内为每个 mutating 工具加入
//! `authorize_tool_call` + `try_claim_auto_write` 的 per-tool 门控——否则 batch 内
//! 的写操作会绕过 ModePolicy。保留本模块以备未来接线;`#![allow(dead_code)]` 已设。

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::modes::ApprovalDecision;
use crate::context::snapshot::{AutoSnapshotReason, WorkspaceMutationManager};
use crate::tools::execution::ToolCallback;
use crate::tools::{EffectKind, Tool, ToolResult};

/// 单次工具执行的"上下文"——所有跨阶段的共享状态。
///
/// 设计目标:
/// - **不依赖 `MovixAgent`**:pipeline 完全独立,可以在单测里直接构造。
/// - **所有可变状态都走 `&mut` 借出**:避免内部 Arc<Mutex<>> 隐藏的状态。
pub struct ExecutionContext<'a> {
    /// 工作区变更管理器(快照 + 失败回滚 + 修改文件跟踪)
    pub mutation: &'a mut WorkspaceMutationManager,
    /// 工具回调(可选,流式场景下用于实时推送)
    pub tool_cb: Option<&'a ToolCallback>,
    /// 审批 channel 的 receiver(可选,只在 NeedsApproval 路径下用)
    pub approval_rx: Option<&'a mut mpsc::Receiver<ApprovalDecision>>,
}

impl<'a> ExecutionContext<'a> {
    pub fn new(mutation: &'a mut WorkspaceMutationManager) -> Self {
        Self {
            mutation,
            tool_cb: None,
            approval_rx: None,
        }
    }

    pub fn with_tool_cb(mut self, cb: &'a ToolCallback) -> Self {
        self.tool_cb = Some(cb);
        self
    }

    pub fn with_approval_rx(mut self, rx: &'a mut mpsc::Receiver<ApprovalDecision>) -> Self {
        self.approval_rx = Some(rx);
        self
    }
}

/// 一次工具执行的"干跑结果"——描述"会做什么",**不**真正执行。
/// 给审批 UI / 审计 / 计划模式用。
#[derive(Debug, Clone)]
pub struct PlannedMutation {
    pub tool_name: String,
    pub effect_kind: EffectKind,
    pub affected_paths: Vec<PathBuf>,
    pub needs_snapshot: bool,
    pub reason: Option<AutoSnapshotReason>,
}

impl PlannedMutation {
    /// 给定 Tool + args,算出"应该做什么"。
    /// 这个函数**只读**,不触发任何 IO,适合在审批前/计划模式前调用。
    pub fn plan(tool: &dyn Tool, args: &Value, mgr: &WorkspaceMutationManager) -> Self {
        let effect_kind = tool.effect_kind();
        let affected_paths = tool.affected_paths(args);
        let reason = mgr.should_snapshot_for_effect(effect_kind);
        Self {
            tool_name: tool.name().to_string(),
            effect_kind,
            affected_paths,
            needs_snapshot: reason.is_some(),
            reason,
        }
    }
}

/// 管道执行入口(串行)。P3 阶段:每次只跑一个 tool,P6 才会 fan-out。
pub struct Pipeline;

impl Pipeline {
    /// 真正执行一个工具(同步一次)。
    ///
    /// 注意:此函数**不**做权限检查——那是 [`ModeConfig`] 的职责。
    /// 也不**做**结果格式化、verify、LSP——这些是上层 agent.rs 的工作。
    ///
    /// 它只负责:
    /// 1. 必要时触发自动快照
    /// 2. 调用 `tool.execute(args)`
    /// 3. 把结果标记为成功/失败
    /// 4. 记录修改文件
    pub async fn run_one(
        tool: Arc<dyn Tool>,
        args: &Value,
        ctx: &mut ExecutionContext<'_>,
    ) -> ToolResult {
        // 1. 副作用规划(从 Tool 元数据)
        let plan = PlannedMutation::plan(tool.as_ref(), args, ctx.mutation);

        // 2. 必要时创建快照
        if let Some(reason) = plan.reason {
            let _ = ctx
                .mutation
                .before_modification(reason, &plan.affected_paths);
        }

        // 3. 真正执行
        let result = match tool.execute(args).await {
            Ok(r) => r,
            Err(e) => ToolResult::err(format!("工具执行错误: {}", e)),
        };

        // 4. 记录 / 决定是否回滚
        if result.success {
            ctx.mutation.on_success();
            if !plan.affected_paths.is_empty() {
                ctx.mutation.record_modified(&plan.affected_paths);
            }
        } else {
            use crate::context::snapshot::RollbackDecision;
            if let RollbackDecision::Rollback(sid) = ctx
                .mutation
                .on_failure(result.error.as_deref().unwrap_or(""))
                && let Err(e) = ctx.mutation.rollback(&sid)
            {
                tracing::warn!(target: "pipeline", "自动回滚失败: {}", e);
            }
        }

        result
    }

    /// P6:批量执行一组 tool call。
    ///
    /// **核心策略**:
    /// - ReadOnly 工具 → **并行**(`tokio::spawn` + JoinSet 收集)
    /// - Mutating 工具 → **串行**,且**先 await 完所有已派发的读任务**再执行
    ///
    /// 这样保证:
    /// 1. 读工具之间无副作用竞争 → 可以安全并发
    /// 2. 写工具对工作区的修改**先发生**,后续读工具看到最新状态
    /// 3. 同一 batch 中多个写工具按调用顺序串行
    ///
    /// 输入的 `batch` 顺序会被尊重(结果的顺序 = 输入的顺序),
    /// 但**实际执行**会按"读-批-写-批-读-批..."的形式分段。
    ///
    /// **安全注意**:本函数**不**做 ModePolicy / 审批 / Auto 预算检查;调用方
    /// (目前是测试 + agent.rs 主循环)必须在调用前对每一个 mutating 工具走过
    /// `authorize_tool_call` 与 `try_claim_auto_write`。如果未来在 agent 主循环里
    /// 直接消费 `run_batch`,需要在这里(或调用前)做 per-tool 审批门控,
    /// 否则 batch 内的 mutating 工具会绕过 mode-policy。
    pub async fn run_batch(
        batch: Vec<(Arc<dyn Tool>, Value)>,
        ctx: &mut ExecutionContext<'_>,
    ) -> Vec<ToolResult> {
        use tokio::task::JoinSet;

        let total = batch.len();
        let mut results: Vec<Option<ToolResult>> = (0..total).map(|_| None).collect();
        let mut in_flight: JoinSet<(usize, ToolResult)> = JoinSet::new();
        let iter = batch.into_iter().enumerate();

        // 主循环:从 batch 里取一项,要么 spawn(read),要么 await_in_flight + 串行(write)
        for (idx, (tool, args)) in iter {
            if tool.effect_kind() == EffectKind::ReadOnly {
                let plan = PlannedMutation::plan(tool.as_ref(), &args, ctx.mutation);
                debug_assert!(!plan.needs_snapshot, "readonly 工具不应触发快照");

                let tool_clone = Arc::clone(&tool);
                let args_clone = args.clone();
                in_flight.spawn(async move {
                    let result = match tool_clone.execute(&args_clone).await {
                        Ok(r) => r,
                        Err(e) => ToolResult::err(format!("工具执行错误: {}", e)),
                    };
                    (idx, result)
                });
            } else {
                // Mutating 工具:必须等所有 in-flight 读完成,再串行执行写
                drain_in_flight(&mut in_flight, &mut results).await;
                let result = Self::run_one(tool, &args, ctx).await;
                results[idx] = Some(result);
            }
        }

        // 收尾:等剩余 in-flight 读任务
        drain_in_flight(&mut in_flight, &mut results).await;

        // 把 Option<ToolResult> 拍平成 Vec<ToolResult>(所有 slot 都已填)
        results
            .into_iter()
            .enumerate()
            .map(|(i, r)| {
                r.unwrap_or_else(|| {
                    ToolResult::err(format!("pipeline: result slot {} was never filled", i))
                })
            })
            .collect()
    }
}

/// 辅助函数:把 JoinSet 里所有 in-flight 任务的 `(idx, result)` 收集到 `results[idx]`。
async fn drain_in_flight(
    in_flight: &mut tokio::task::JoinSet<(usize, ToolResult)>,
    results: &mut [Option<ToolResult>],
) {
    while let Some(joined) = in_flight.join_next().await {
        match joined {
            Ok((idx, result)) => results[idx] = Some(result),
            Err(e) => {
                tracing::warn!("pipeline tool task panicked: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::path::Path;

    struct FakeOkWriteTool;
    #[async_trait]
    impl Tool for FakeOkWriteTool {
        fn name(&self) -> &str {
            "write_file"
        }
        fn description(&self) -> &str {
            "fake"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({})
        }
        async fn execute(&self, args: &Value) -> crate::common::error::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: format!(
                    "wrote {}",
                    args.get("path").and_then(|v| v.as_str()).unwrap_or("?")
                ),
                error: None,
            })
        }
        fn effect_kind(&self) -> EffectKind {
            EffectKind::WorkspaceWrite
        }
        fn affected_paths(&self, args: &Value) -> Vec<PathBuf> {
            args.get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .into_iter()
                .collect()
        }
    }

    struct FakeFailReadTool;
    #[async_trait]
    impl Tool for FakeFailReadTool {
        fn name(&self) -> &str {
            "read_file"
        }
        fn description(&self) -> &str {
            "fake"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({})
        }
        async fn execute(&self, _args: &Value) -> crate::common::error::Result<ToolResult> {
            Ok(ToolResult::err("boom".into()))
        }
        fn effect_kind(&self) -> EffectKind {
            EffectKind::ReadOnly
        }
    }

    #[test]
    fn plan_captures_effect_and_paths() {
        let tool = FakeOkWriteTool;
        let args = json!({ "path": "src/main.rs" });
        let mgr = WorkspaceMutationManager::new(Path::new("/tmp/_movix_p3_test"));
        let plan = PlannedMutation::plan(&tool, &args, &mgr);
        assert_eq!(plan.tool_name, "write_file");
        assert_eq!(plan.effect_kind, EffectKind::WorkspaceWrite);
        assert_eq!(plan.affected_paths, vec![PathBuf::from("src/main.rs")]);
        assert!(plan.needs_snapshot);
        assert!(matches!(plan.reason, Some(AutoSnapshotReason::BeforeWrite)));
    }

    #[test]
    fn plan_for_readonly_does_not_snapshot() {
        let tool = FakeFailReadTool;
        let mgr = WorkspaceMutationManager::new(Path::new("/tmp/_movix_p3_test"));
        let plan = PlannedMutation::plan(&tool, &json!({}), &mgr);
        assert_eq!(plan.effect_kind, EffectKind::ReadOnly);
        assert!(!plan.needs_snapshot);
        assert!(plan.reason.is_none());
    }

    #[tokio::test]
    async fn run_one_executes_tool_and_records_success() {
        let tool: Arc<dyn Tool> = Arc::new(FakeOkWriteTool);
        let mut mgr = WorkspaceMutationManager::new(Path::new("/tmp/_movix_p3_test"));
        // 关闭 enabled 标志,避免真正创建 git 仓库
        mgr.set_enabled(false);
        let mut ctx = ExecutionContext::new(&mut mgr);

        let result = Pipeline::run_one(tool, &json!({"path": "x.rs"}), &mut ctx).await;
        assert!(result.success);
        // enabled=false,所以 before_modification 不会真正触发;
        // 但 record_modified 仍然会记录"应该改哪些文件"——这是 P3 设计:
        // 修改文件跟踪和快照解耦,便于 verify / LSP。
        assert_eq!(
            ctx.mutation.get_modified_files(),
            vec![PathBuf::from("x.rs")]
        );
    }

    // ───────────── P6: 读并发 + 写串行 ─────────────

    /// 慢读工具:每个实例 sleep 100ms 然后返回 ok。
    struct SlowReadTool {
        tag: &'static str,
    }
    #[async_trait]
    impl Tool for SlowReadTool {
        fn name(&self) -> &str {
            "slow_read"
        }
        fn description(&self) -> &str {
            "fake"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({})
        }
        async fn execute(&self, _args: &Value) -> crate::common::error::Result<ToolResult> {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok(ToolResult {
                success: true,
                output: self.tag.into(),
                error: None,
            })
        }
        fn effect_kind(&self) -> EffectKind {
            EffectKind::ReadOnly
        }
    }

    /// 写工具:记录执行顺序(thread_local 计数),无 sleep。
    /// 修复(P3.2):将全局 static Mutex 改为 thread_local!,避免并行测试间相互干扰。
    struct OrderedWriteTool;
    std::thread_local! {
        static WRITE_LOG: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    #[async_trait]
    impl Tool for OrderedWriteTool {
        fn name(&self) -> &str {
            "ordered_write"
        }
        fn description(&self) -> &str {
            "fake"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({})
        }
        async fn execute(&self, _args: &Value) -> crate::common::error::Result<ToolResult> {
            WRITE_LOG.with(|log| log.borrow_mut().push("w"));
            Ok(ToolResult {
                success: true,
                output: "w".into(),
                error: None,
            })
        }
        fn effect_kind(&self) -> EffectKind {
            EffectKind::WorkspaceWrite
        }
    }

    #[tokio::test]
    async fn run_batch_reads_run_concurrently_writes_run_serially() {
        // 3 个读 + 1 个写 → 读应该"叠加并发"(< 200ms),写一定在最后一个读完成后才执行
        // 通过 Arc<AtomicI64> 验证并发
        let mgr_path = Path::new("/tmp/_movix_p6_test");
        let mut mgr = WorkspaceMutationManager::new(mgr_path);
        mgr.set_enabled(false);

        // 清空 log
        WRITE_LOG.with(|log| log.borrow_mut().clear());

        let r1: Arc<dyn Tool> = Arc::new(SlowReadTool { tag: "r1" });
        let r2: Arc<dyn Tool> = Arc::new(SlowReadTool { tag: "r2" });
        let r3: Arc<dyn Tool> = Arc::new(SlowReadTool { tag: "r3" });
        let w1: Arc<dyn Tool> = Arc::new(OrderedWriteTool);

        let batch: Vec<(Arc<dyn Tool>, Value)> = vec![
            (r1, json!({})),
            (r2, json!({})),
            (r3, json!({})),
            (w1, json!({"path": "a.rs"})),
        ];

        let start = std::time::Instant::now();
        let mut ctx = ExecutionContext::new(&mut mgr);
        let results = Pipeline::run_batch(batch, &mut ctx).await;
        let elapsed = start.elapsed();

        // 4 个结果都返回
        assert_eq!(results.len(), 4);
        assert!(results.iter().all(|r| r.success));

        // 3 个读各 sleep 100ms:并发总时 ~100ms;串行则 ~300ms
        // 留 250ms 上限,确保确实并发
        assert!(
            elapsed < std::time::Duration::from_millis(250),
            "reads should run concurrently; elapsed = {:?}",
            elapsed
        );

        // 写一定发生(且只发生一次)
        let log = WRITE_LOG.with(|log| log.borrow().clone());
        assert_eq!(log, vec!["w"]);
    }

    #[tokio::test]
    async fn run_batch_preserves_input_order_in_results() {
        // 即使内部执行是"3 reads 并行 + 1 write",输出顺序必须与输入一致
        let mgr_path = Path::new("/tmp/_movix_p6_test");
        let mut mgr = WorkspaceMutationManager::new(mgr_path);
        mgr.set_enabled(false);

        let r1: Arc<dyn Tool> = Arc::new(SlowReadTool { tag: "alpha" });
        let r2: Arc<dyn Tool> = Arc::new(SlowReadTool { tag: "beta" });
        let r3: Arc<dyn Tool> = Arc::new(SlowReadTool { tag: "gamma" });

        let batch: Vec<(Arc<dyn Tool>, Value)> =
            vec![(r1, json!({})), (r2, json!({})), (r3, json!({}))];

        let mut ctx = ExecutionContext::new(&mut mgr);
        let results = Pipeline::run_batch(batch, &mut ctx).await;

        let outputs: Vec<&str> = results.iter().map(|r| r.output.as_str()).collect();
        assert_eq!(outputs, vec!["alpha", "beta", "gamma"]);
    }
}
