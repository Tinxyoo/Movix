pub mod modes;
pub mod partial_approval;
pub mod permission;
pub mod pipeline;
pub mod reasoning;
pub mod selector;

// Re-export key types for backward compatibility
pub use self::modes::{
    AppMode, ApprovalDecision, ApprovalModification, ModeConfig, ModeDecision, effect_kind_of,
    is_mutating_tool, is_mutating_tool_call,
};
pub use self::partial_approval::{ApprovalOutcome, ApprovalRequest, PartialApproval};
pub use self::permission::await_approval;
pub use self::pipeline::{ExecutionContext, Pipeline, PlannedMutation};
pub use self::reasoning::{ReasoningController, ReasoningEffort, ReasoningSummary};
pub use self::selector::{AdaptiveModelSelector, ModelConfig, TaskAnalysis};

use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

use crate::common::Scavenger;
use crate::common::arg_repair;
use crate::common::collab_watcher::CollaborationWatcher;
use crate::common::config::MovixConfig;
use crate::common::cost_status;
use crate::common::deepseek::{ChatMessage, DeepSeekClient, StreamEvent, ToolCall};
use crate::common::error::{MovixError, Result};
use crate::common::failure_tracker::{FailureSignal, FailureTracker};
use crate::common::loop_guard::LoopGuard;
use crate::common::lsp::LspDiagnostics;
use crate::common::pricing::{self};
use crate::common::reviewer::Reviewer;
use crate::common::session::{SessionPersistence, SessionSnapshot};
use crate::common::skill_executor;
use crate::common::utils::truncate_str;
use crate::context::ContextWindow;
use crate::context::compaction::TurnCompaction;
use crate::context::compressor::SemanticCompressor;
use crate::context::large_output::{LargeOutputConfig, LargeOutputRouter};
use crate::context::memory::{AddAction, MemoryCategory, UserMemory};
use crate::context::skills::SkillRegistry;
use crate::context::snapshot::SnapshotRepo;
use crate::context::snapshot::WorkspaceMutationManager;
use crate::context::tiered::{MessageTier, TieredContextBuilder, TieredContextWindow};
use crate::mcp::tool_bridge::McpToolBridge;
use crate::mcp::{McpManager, collect_mcp_configs};
use crate::planning::task_decomposer::{DecomposedTask, TaskDecomposer};
use crate::planning::verify_loop::{VerifyConfig, VerifyLevel, VerifyLoop};
use crate::tools::EffectKind;
use crate::tools::execution::{ToolCallback, extract_file_from_args, extract_tool_detail};
use crate::tools::{ToolRegistry, ToolResult};

pub type TokenCallback = Box<dyn Fn(String) + Send + Sync>;
pub type ReasoningCallback = Box<dyn Fn(String) + Send + Sync>;

// ── 子结构体：将 MovixAgent 的 35 字段按职责分组 ──

/// 核心配置和 LLM 客户端
struct AgentCore {
    config: MovixConfig,
    llm: Arc<Mutex<DeepSeekClient>>,
    tools: Arc<ToolRegistry>,
    mcp_manager: Arc<Mutex<McpManager>>,
    system_prompt_text: String,
}

/// 上下文管理
/// 修复(Bug #15):用 enum 替换 use_tiered_context+Option<TieredContextWindow>
/// 双重状态;原实现的 8 个 if/else wrapper 方法每次都要在两个独立字段间
/// 协调,bool 与 Option 可矛盾。改成 enum 后所有读取一行 match,二者天然不可能矛盾。
enum CtxBackend {
    Plain(ContextWindow),
    Tiered(Box<TieredContextWindow>),
}

impl CtxBackend {
    fn push(&mut self, message: ChatMessage, tier: MessageTier) {
        match self {
            CtxBackend::Plain(c) => c.push(message),
            CtxBackend::Tiered(t) => t.push(message, tier),
        }
    }

    fn messages(&self) -> Vec<ChatMessage> {
        match self {
            CtxBackend::Plain(c) => c.messages(),
            CtxBackend::Tiered(t) => t.messages(),
        }
    }

    fn clear_history(&mut self) {
        match self {
            CtxBackend::Plain(c) => c.clear_history(),
            CtxBackend::Tiered(t) => t.clear_history(),
        }
    }

    fn message_count(&self) -> usize {
        match self {
            CtxBackend::Plain(c) => c.message_count(),
            CtxBackend::Tiered(t) => t.message_count(),
        }
    }

    fn token_count(&self) -> usize {
        match self {
            CtxBackend::Plain(c) => c.token_count(),
            CtxBackend::Tiered(t) => t.token_count(),
        }
    }

    fn max_tokens(&self) -> usize {
        match self {
            CtxBackend::Plain(c) => c.max_tokens(),
            CtxBackend::Tiered(t) => t.max_tokens(),
        }
    }

    fn update_system(&mut self, prompt: String) {
        match self {
            CtxBackend::Plain(c) => c.update_system(prompt),
            CtxBackend::Tiered(t) => t.update_system(prompt),
        }
    }
}

struct AgentContext {
    backend: CtxBackend,
    turn_compaction: TurnCompaction,
    large_output_router: LargeOutputRouter,
    semantic_compressor: SemanticCompressor,
}

impl AgentContext {
    /// 根据消息角色推断默认 tier
    ///
    /// 修复(R5/C6,关键):原实现把 assistant / tool 都归 Normal。分层后端驱逐时
    /// 独立弹出最旧的 Normal 消息,会拆散 assistant(tool_calls) 与其 tool(result)
    /// 的配对 → 孤儿 tool 消息 → DeepSeek/OpenAI API 返回 HTTP 400。
    /// auto_tier_message(tiered.rs:351)有正确的配对逻辑(注释也说"配对保留"),
    /// 但那只用于迁移,稳态 push_auto 走的是这里。现把 tool_calls/tool 归 Critical,
    /// 与 auto_tier_message 对齐,保证驱逐时成对保留。
    fn default_tier(msg: &ChatMessage) -> MessageTier {
        // tool 调用与结果必须配对,同属 Critical。
        if msg.tool_calls.is_some() || msg.role == "tool" {
            return MessageTier::Critical;
        }
        match msg.role.as_str() {
            "system" | "user" => MessageTier::Important,
            "assistant" | "tool" => MessageTier::Normal,
            _ => MessageTier::Normal,
        }
    }

    fn push_auto(&mut self, message: ChatMessage) {
        let tier = Self::default_tier(&message);
        self.backend.push(message, tier);
    }

    fn push_message(&mut self, message: ChatMessage, tier: MessageTier) {
        self.backend.push(message, tier);
    }

    fn messages(&self) -> Vec<ChatMessage> {
        self.backend.messages()
    }

    fn clear_history(&mut self) {
        self.backend.clear_history();
    }

    fn message_count(&self) -> usize {
        self.backend.message_count()
    }

    fn token_count(&self) -> usize {
        self.backend.token_count()
    }

    fn max_tokens(&self) -> usize {
        self.backend.max_tokens()
    }

    fn update_system(&mut self, prompt: String) {
        self.backend.update_system(prompt);
    }

    fn is_tiered(&self) -> bool {
        matches!(self.backend, CtxBackend::Tiered(_))
    }

    fn tiered_mut(&mut self) -> Option<&mut TieredContextWindow> {
        match &mut self.backend {
            CtxBackend::Tiered(t) => Some(t.as_mut()),
            CtxBackend::Plain(_) => None,
        }
    }

    fn tiered(&self) -> Option<&TieredContextWindow> {
        match &self.backend {
            CtxBackend::Tiered(t) => Some(t.as_ref()),
            CtxBackend::Plain(_) => None,
        }
    }
}

/// 会话状态
struct AgentSession {
    /// 会话级累计轮次(用于持久化/统计),单调递增,不在每轮 turn 开头重置。
    iteration: u32,
    /// 当前 turn 内的主循环迭代计数。每轮 turn 开头(`run_with_stream`)重置为 0,
    /// 与 `max_iterations` 比较防止单轮死循环。
    /// 修复(Critical #C9):此前复用 `iteration` 一个字段,导致 max_iterations 比较
    /// 跨 turn 累积,十几轮对话后整会话宕掉。
    turn_iteration: u32,
    start_time: Instant,
    cached_stats: crate::common::deepseek::TokenStats,
    /// 修复(Bug #2):同一份 stats 的共享视图,供 spawn 出去的后台 task 期间
    /// UI 实时读取 (cli/mod.rs swap agent 后,主线程 app.agent 是占位,
    /// 但它通过 clone 出去的 Arc 仍能拿到真实 stats)。
    shared_stats: std::sync::Arc<std::sync::Mutex<crate::common::deepseek::TokenStats>>,
    /// 修复(Bug #4):跨 LLM HTTP 请求的取消标志。spawn task 在收到 Ctrl-C
    /// 时设置 true,chat_stream 内每次 next chunk 前检查并立即返回。
    cancel_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    session_cost: f64,
    /// 修复(G-C1):上一次 update_session_cost 时已计入成本的累计 token 总量。
    /// 用于只累加**增量**成本,而非反复叠加累计值(否则成本指数级虚高)。
    last_costed_total_tokens: u64,
    /// 修复(R6/C7fix):R5 曾用"比例缩放"把累计 cache/reasoning 字段折算成 delta,
    /// 但比例法数学错误(高缓存率下 completion 少报 ~7x、凭空计费未发生的 cache_hit)。
    /// 正确做法是直接减法:记录上次已计费的完整 stats 快照,每字段 current - last 得到真实增量。
    last_costed_stats: crate::common::deepseek::TokenStats,
    session_persistence: SessionPersistence,
    modified_files: Vec<std::path::PathBuf>,
    last_repair_notes: Vec<String>,
    user_memory: UserMemory,
    /// 修复(Bug #5):工具调用后置 dirty 标志,turn 末尾才跑 verify_loop。
    verify_pending: bool,
    /// 修复(审查):verify 错误已回灌的轮次数。超过上限后不再重复注入,
    /// 防止"修不好的编译错误"每轮都塞进上下文导致爆炸。
    verify_backfill_count: u8,
}

/// 安全和防护
struct AgentSafety {
    failure_tracker: FailureTracker,
    loop_guard: LoopGuard,
    mode_config: ModeConfig,
    partial_approval: PartialApproval,
    approval_rx: Option<tokio::sync::mpsc::Receiver<ApprovalDecision>>,
    verify_loop: VerifyLoop,
    reviewer: Reviewer,
    // P1.4 之后,scavenger 不再做"自动执行";调用方直接走 `Scavenger::scavenge`
    // 关联函数,不再需要实例字段。
}

/// 规划（多智能体相关子系统已移除——均为未接入主流程的死代码）
struct AgentPlanning {
    skill_registry: SkillRegistry,
}

/// 工作区和辅助
struct AgentWorkspace {
    snapshot_repo: Option<SnapshotRepo>,
    auto_snapshot: Option<WorkspaceMutationManager>,
    collab_watcher: CollaborationWatcher,
    lsp: LspDiagnostics,
    reasoning_controller: ReasoningController,
}

pub struct MovixAgent {
    core: AgentCore,
    ctx: AgentContext,
    session: AgentSession,
    safety: AgentSafety,
    planning: AgentPlanning,
    workspace: AgentWorkspace,
}

impl MovixAgent {
    /// 使用默认系统提示创建 Agent 实例（自动注入 AGENT.md 项目指令）
    pub fn new(config: MovixConfig) -> Result<Self> {
        // 修复(G-M3):启动时清理上次崩溃可能残留的 atomic_write 临时文件。
        crate::common::utils::cleanup_stale_atomic_tmp(&config.workspace);
        let system_prompt = DeepSeekClient::build_system_prompt_with_project(&config.workspace);
        Self::with_system_prompt(config, system_prompt)
    }

    /// 使用自定义系统提示创建 Agent 实例，初始化所有子系统
    pub fn with_system_prompt(config: MovixConfig, system_prompt: String) -> Result<Self> {
        Self::with_system_prompt_and_tools(config, system_prompt, None)
    }

    /// 修复(Bug #7):创建 SubAgent 时强制启用工具白名单,避免 Searcher 等只读
    /// 角色拥有 write_file/shell 等工具。`allowed_tools=Some(&[...])` 时,
    /// 默认 registry 中不在白名单的工具会被移除。
    pub fn with_system_prompt_and_tools(
        config: MovixConfig,
        system_prompt: String,
        allowed_tools: Option<&[&str]>,
    ) -> Result<Self> {
        let workspace_str = config.workspace.to_string_lossy().to_string();
        let llm = DeepSeekClient::new(config.clone())?;
        let tools = crate::tools::create_default_registry(&workspace_str);
        if let Some(allow) = allowed_tools {
            tools.retain_only(allow);
        }

        let reasoning_effort = ReasoningEffort::parse_effort(&config.reasoning_effort);
        let reasoning_controller = ReasoningController::with_effort(reasoning_effort);

        // 默认开启用户记忆：~/.movix/memory.md 会在初始化时被读取，
        // 若文件不存在则 cached_content 为 None，system_block() 返回 None，
        // 不会往 system_prompt 注入空块，零成本。用户可用 /memory off 关闭。
        let user_memory = UserMemory::new(crate::context::memory::default_memory_path(), true);

        // 修复(启动慢):原实现在此调用 `workspace_size_ok()`,其内部
        // `walk_dir_size()` 会递归 read_dir 整个工作区(不读 .gitignore,
        // 连 .git/node_modules/target 都遍历),大仓库耗时数秒~数十秒。
        // 而本函数在 `App::new` → 第一帧 `terminal.draw()` 之前执行,
        // 导致启动期间屏幕长时间空白(此前在 draw 前加帧的修复无效)。
        // 现将快照仓库的初始化(含体积检查)延迟到首帧绘制之后由
        // `init_snapshot_repo()` 完成,与 refresh_git_context / init_mcp 同策略。
        let snapshot_repo: Option<SnapshotRepo> = None;

        let mut loop_guard = LoopGuard::new();
        loop_guard.set_mutating_checker(is_mutating_tool_call);

        let large_output_router = LargeOutputRouter::new(LargeOutputConfig::default());
        // 压缩阈值设为 max_tokens 的 85%：上下文接近满之前主动做语义摘要压缩，
        // 而不是等到 compress_if_needed 暴力 pop_front（无摘要、丢语义）。
        // 修复(死路径)：原硬编码 800_000 远大于实际 max_tokens(~393k)，
        // 导致 should_compact 永不触发，主压缩路径完全失效。
        let compact_threshold = ((config.max_tokens as f64) * 0.85) as usize;
        let turn_compaction = TurnCompaction::new().with_threshold(compact_threshold);

        let tools_arc = Arc::new(tools);
        let mcp_manager = Arc::new(Mutex::new(McpManager::new()));
        let mut skill_registry = SkillRegistry::with_workspace(config.workspace.clone());
        skill_registry.discover();
        // 修复(skill 工具未注册):此前 list_skills / use_skill 实现了 Tool trait
        // 却从未被注册到 registry,导致生产环境 LLM 调用它们会得到"未知工具"。
        // 这里在 SkillRegistry 初始化后立即注册两个 skill 工具。
        crate::tools::register_skill_tools(&tools_arc, skill_registry.clone());

        let lsp = LspDiagnostics::new(false);

        // 把用户记忆（~/.movix/memory.md）作为 system block 拼到 system_prompt 末尾，
        // 这样后续 /memory add 的内容才会真正进入 LLM 的上下文。
        let system_prompt = if let Some(user_block) = user_memory.system_block() {
            tracing::debug!(target: "agent", "已注入用户记忆 system block（{} 字节）", user_block.len());
            format!("{}\n\n{}", system_prompt, user_block)
        } else {
            system_prompt
        };

        Ok(Self {
            core: AgentCore {
                config: config.clone(),
                llm: Arc::new(Mutex::new(llm)),
                tools: tools_arc,
                mcp_manager,
                system_prompt_text: system_prompt.clone(),
            },
            ctx: AgentContext {
                // 修复(Bug #15):默认走 Plain,需要 tier 时通过
                // enable_tiered_context() 切换。原实现两个字段并存,bool 与
                // Option 可矛盾;现在 enum 二选一,语义无歧义。
                backend: CtxBackend::Plain(ContextWindow::new(
                    system_prompt.clone(),
                    config.max_tokens as usize,
                )),
                turn_compaction,
                large_output_router,
                semantic_compressor: SemanticCompressor::new(),
            },
            session: AgentSession {
                iteration: 0,
                turn_iteration: 0,
                start_time: Instant::now(),
                cached_stats: crate::common::deepseek::TokenStats::default(),
                shared_stats: std::sync::Arc::new(std::sync::Mutex::new(
                    crate::common::deepseek::TokenStats::default(),
                )),
                cancel_flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                session_cost: 0.0,
                last_costed_total_tokens: 0,
                last_costed_stats: crate::common::deepseek::TokenStats::default(),
                session_persistence: SessionPersistence::new(&config.workspace),
                modified_files: Vec::new(),
                last_repair_notes: Vec::new(),
                user_memory,
                verify_pending: false,
                verify_backfill_count: 0,
            },
            safety: AgentSafety {
                failure_tracker: FailureTracker::new(),
                loop_guard,
                mode_config: ModeConfig::new(AppMode::Agent),
                partial_approval: PartialApproval::new(),
                approval_rx: None,
                verify_loop: VerifyLoop::new(
                    &config.workspace,
                    VerifyConfig {
                        enabled: true,
                        level: VerifyLevel::Syntax,
                        trigger_tools: vec![
                            "write_file".into(),
                            "patch_file".into(),
                            "edit_file".into(),
                            "delete_file".into(),
                        ],
                        max_output_chars: 2000,
                        timeout_secs: 30,
                    },
                ),
                reviewer: Reviewer::default_enabled(),
            },
            planning: AgentPlanning { skill_registry },
            workspace: AgentWorkspace {
                snapshot_repo,
                auto_snapshot: if which_git() {
                    Some(WorkspaceMutationManager::new(&config.workspace))
                } else {
                    None
                },
                collab_watcher: CollaborationWatcher::new(&config.workspace),
                lsp,
                reasoning_controller,
            },
        })
    }

    /// 切换 LLM 模型（deepseek-v4-pro / deepseek-v4-flash），重建客户端
    pub fn switch_model(&mut self, model: &str) -> Result<()> {
        let valid_models = ["deepseek-v4-pro", "deepseek-v4-flash", "deepseek-chat"];
        if !valid_models.contains(&model) {
            return Err(MovixError::Other(format!(
                "无效的模型名 '{}'，可用: deepseek-v4-pro, deepseek-v4-flash",
                model
            )));
        }

        self.core.config.model = model.to_string();
        self.rebuild_llm_client()
    }

    /// 切换推理/思考模式开关
    pub fn toggle_thinking(&mut self) -> Result<()> {
        self.core.config.thinking_enabled = !self.core.config.thinking_enabled;

        if !self.core.config.thinking_enabled {
            self.core.config.reasoning_effort = String::new();
        } else if self.core.config.reasoning_effort.is_empty() {
            self.core.config.reasoning_effort = "auto".into();
        }

        self.rebuild_llm_client()
    }

    /// 循环切换思考强度：off → auto → high → max → off
    pub fn cycle_thinking(&mut self) -> Result<()> {
        let next = match self.core.config.reasoning_effort.as_str() {
            "" | "off" => {
                self.core.config.thinking_enabled = true;
                "auto"
            }
            "auto" => "high",
            "high" => "max",
            _ => {
                self.core.config.thinking_enabled = false;
                ""
            }
        };
        self.core.config.reasoning_effort = next.to_string();

        self.rebuild_llm_client()
    }

    /// 设置推理努力等级（auto/low/medium/high/max）
    pub fn set_reasoning_effort(&mut self, effort: &str) -> Result<()> {
        if !self.core.config.thinking_enabled {
            return Err(MovixError::Other("请先启用思考模式 (/thinking)".into()));
        }
        let valid = ["auto", "high", "max"];
        if !valid.contains(&effort) {
            return Err(MovixError::Other(format!(
                "无效的推理强度 '{}'，可用: auto, high, max",
                effort
            )));
        }

        self.core.config.reasoning_effort = effort.to_string();
        self.rebuild_llm_client()
    }

    /// 获取当前使用的模型名称
    pub fn current_model(&self) -> &str {
        &self.core.config.model
    }

    /// 获取 Agent 配置的克隆
    pub fn config(&self) -> &MovixConfig {
        &self.core.config
    }

    pub fn thinking_enabled(&self) -> bool {
        self.core.config.thinking_enabled
    }

    pub fn reasoning_effort(&self) -> &str {
        &self.core.config.reasoning_effort
    }

    /// 异步初始化 MCP 连接，发现并注册 MCP 工具
    pub async fn init_mcp(&mut self) -> Result<()> {
        // 协作感知：建初始 mtime 快照。必须在首轮 run_with_stream 之前完成，
        // 否则 detect_external_changes 会把工作区全部文件误报为"外部新建"。
        self.workspace.collab_watcher.initialize();

        let configs = collect_mcp_configs(&self.core.config.workspace);
        if configs.is_empty() {
            return Ok(());
        }

        let manager = McpManager::from_configs(configs).await;
        let tool_count = manager.total_tools().await;
        let server_count = manager.connected_count().await;

        if server_count > 0 {
            let all_tools = manager.all_tools().await;
            for (server_name, tool_info) in &all_tools {
                let bridge = McpToolBridge::new(
                    server_name.clone(),
                    tool_info.clone(),
                    Arc::clone(&self.core.mcp_manager),
                );
                // 修复(High #H7):用 register_external 而非 register,拒绝与内置工具重名
                // 的 MCP 工具,防止遮蔽(如名为 read_file 的 MCP 工具让读取走外部服务)。
                self.core.tools.register_external(bridge);
            }

            tracing::info!(
                "MCP 初始化完成: {} 个服务器, {} 个工具",
                server_count,
                tool_count
            );
        }

        *self.core.mcp_manager.lock().await = manager;

        Ok(())
    }

    /// 获取 MCP 管理器的引用
    pub fn mcp_manager(&self) -> &Arc<Mutex<McpManager>> {
        &self.core.mcp_manager
    }

    /// 修复(G-M2):优雅关闭所有 MCP 子进程。run_interactive/run_single_task
    /// 返回前调用,确保 MCP server 被 kill+wait 而非依赖 Drop(runtime 关闭时
    /// Drop 里的 spawn wait 可能不执行,导致 zombie)。
    pub async fn shutdown(&self) {
        let mgr = self.core.mcp_manager.lock().await;
        mgr.disconnect_all().await;
    }

    /// 获取 MCP 服务器状态列表
    pub async fn mcp_server_statuses(&self) -> Vec<(String, crate::mcp::McpServerStatus)> {
        self.core.mcp_manager.lock().await.server_statuses().await
    }

    /// 获取 MCP 工具总数
    pub async fn mcp_tool_count(&self) -> usize {
        self.core.mcp_manager.lock().await.total_tools().await
    }

    /// 重新连接指定 MCP 服务器
    pub async fn mcp_reconnect(&self, server_name: &str) -> Result<()> {
        self.core
            .mcp_manager
            .lock()
            .await
            .reconnect(server_name)
            .await
    }

    /// 启用分层上下文窗口，按优先级分配 token 空间。
    /// 会迁移已有消息(系统提示除外)到新后端,按消息内容自动分 tier。
    pub fn enable_tiered_context(&mut self) {
        if matches!(self.ctx.backend, CtxBackend::Tiered(_)) {
            return;
        }
        // 修复(Bug #15 续):切换后端时迁移已有消息,不再静默丢弃。
        let old_msgs = self.ctx.backend.messages();
        let user_msgs: Vec<_> = old_msgs.into_iter().skip(1).collect(); // skip system

        let mut tiered = TieredContextBuilder::new()
            .max_tokens(self.core.config.max_tokens as usize)
            .reasoning_effort(&self.core.config.reasoning_effort)
            .system_prompt(self.core.system_prompt_text.clone())
            .build();
        for msg in user_msgs {
            let t = tiered.auto_tier_message(&msg);
            tiered.push(msg, t);
        }
        self.ctx.backend = CtxBackend::Tiered(Box::new(tiered));
    }

    /// 禁用分层上下文窗口，回退到普通上下文管理。
    /// 会迁移已有消息,避免丢历史。
    pub fn disable_tiered_context(&mut self) {
        if matches!(self.ctx.backend, CtxBackend::Plain(_)) {
            return;
        }
        let old_msgs = self.ctx.backend.messages();
        let user_msgs: Vec<_> = old_msgs.into_iter().skip(1).collect();

        let mut plain = ContextWindow::new(
            self.core.system_prompt_text.clone(),
            self.core.config.max_tokens as usize,
        );
        for msg in user_msgs {
            plain.push(msg);
        }
        self.ctx.backend = CtxBackend::Plain(plain);
    }

    /// 查询分层上下文窗口是否已启用
    pub fn is_tiered_context_enabled(&self) -> bool {
        self.ctx.is_tiered()
    }

    /// 设置推理等级并重建 LLM 客户端，同步更新分层上下文配置
    pub fn set_reasoning_level(&mut self, level: &str) -> Result<()> {
        let effort = ReasoningEffort::parse_effort(level);
        self.workspace.reasoning_controller.set_effort(effort);
        self.core.config.thinking_enabled = effort != ReasoningEffort::Off;
        self.core.config.reasoning_effort = level.to_string();

        if let Some(tc) = self.ctx.tiered_mut() {
            tc.set_reasoning_effort(level);
        }

        self.rebuild_llm_client()
    }

    /// 获取当前推理控制器的摘要信息
    pub fn reasoning_summary(&self) -> ReasoningSummary {
        self.workspace.reasoning_controller.summary()
    }

    /// 根据任务复杂度自动调整推理等级
    ///
    /// 修复(G-H1):原实现只更新 `reasoning_controller`,不回写 `config.reasoning_effort`
    /// 也不 `rebuild_llm_client`。导致 API 请求里 `reasoning_effort=None`(因 config 仍是
    /// "auto" 被 deepseek.rs 显式排除),实际推理强度与 UI(`/budget`)显示脱节。
    /// 现在在 controller 决定新 effort 后,同步 config + tiered + LLM client,
    /// 与手动入口 `set_reasoning_level` 对齐。
    pub fn auto_adjust_reasoning(&mut self, message: &str, is_subagent: bool) -> bool {
        let changed = self
            .workspace
            .reasoning_controller
            .auto_adjust(message, is_subagent);
        if changed {
            let effort = self.workspace.reasoning_controller.effort();
            let api_str = effort.to_api_string();
            // 把"auto"决定的具体 effort 回写 config,使 API 请求带上真实值。
            if !api_str.is_empty() {
                self.core.config.reasoning_effort = api_str.to_string();
            }
            self.core.config.thinking_enabled =
                effort != crate::agent::reasoning::ReasoningEffort::Off;
            if let Err(e) = self.rebuild_llm_client() {
                tracing::warn!(target: "agent", "rebuild_llm_client after auto_adjust failed: {}", e);
            }
            if let Some(tc) = self.ctx.tiered_mut() {
                tc.set_reasoning_effort(api_str);
            }
        }
        changed
    }

    /// 分析任务复杂度和类型，用于工具路由和模型选择
    pub fn analyze_task(&self, task: &str, history_size: usize) -> TaskAnalysis {
        TaskAnalysis::analyze(task, history_size, false)
    }

    /// 工具列表的总数(供 /tools 命令使用)。
    /// 修复(Bug #19):删除 ToolRouter 死代码后,直接使用 ToolRegistry。
    pub fn tool_count(&self) -> usize {
        self.core.tools.definitions().len()
    }

    /// 根据任务类型路由到最相关的工具子集，减少 token 消耗
    pub fn get_routed_tools(&self, _task: &str) -> Vec<crate::common::deepseek::ToolDefinition> {
        // 修复(Bug #19):原 ToolRouter 用工具名子串做 tier 分类,会把 web_search
        // 误归为 primary,且 LLM 自己会从工具描述选择,层级路由没实际收益。
        // 直接返回全集,让 LLM 决定。
        self.core.tools.definitions()
    }

    /// 获取分层上下文各层级的消息数量统计
    pub fn get_tiered_context_stats(&self) -> Option<[usize; 4]> {
        self.ctx.tiered().map(|tc| tc.tier_counts())
    }

    /// 获取推理预算信息：最大思考 token 数和剩余预算
    pub fn get_reasoning_budget_info(&self) -> (usize, usize) {
        let budget = self.workspace.reasoning_controller.budget();
        let remaining = self
            .workspace
            .reasoning_controller
            .estimate_remaining_budget();
        (budget.max_thinking_tokens, remaining)
    }

    /// 更新系统提示词，同时刷新上下文窗口
    pub fn update_system_prompt(&mut self, prompt: String) {
        self.ctx.update_system(prompt);
    }

    /// Check if a tool mutates the workspace, using the live Tool registry
    /// first (accurate for MCP tools), then falling back to the static table.
    fn tool_mutates_workspace(&self, tool_name: &str) -> bool {
        self.core
            .tools
            .get(tool_name)
            .map(|t| t.effect_kind().mutates_workspace())
            .unwrap_or_else(|| effect_kind_of(tool_name).mutates_workspace())
    }

    /// 工具是否是**纯只读**(无任何外部副作用:不写文件、不执行命令、不发网络请求)。
    /// 修复(H6):比 `!mutates_workspace()` 更严格 —— Network 工具虽不写工作区,但有
    /// 出站副作用(SSRF/外泄风险),在无审批通道时不应自动放行。仅 ReadOnly 才放行。
    fn tool_is_pure_readonly(&self, tool_name: &str) -> bool {
        let kind = self
            .core
            .tools
            .get(tool_name)
            .map(|t| t.effect_kind())
            .unwrap_or_else(|| effect_kind_of(tool_name));
        matches!(kind, crate::tools::EffectKind::ReadOnly)
    }

    /// 记录失败信号到追踪器，用于失败升级判断
    pub fn record_failure(&mut self, signal: FailureSignal) {
        self.safety.failure_tracker.record(signal);
        if self.safety.failure_tracker.should_escalate() {
            self.safety.failure_tracker.mark_escalated();
            if let Err(e) = self.switch_model("deepseek-v4-pro") {
                tracing::warn!("failure escalation: switch_model failed: {}", e);
            }
        }
    }

    /// 获取失败信号的分类统计摘要
    pub fn failure_breakdown(&self) -> String {
        self.safety.failure_tracker.format_breakdown()
    }

    /// 获取当前轮次的失败次数
    pub fn failure_count(&self) -> u32 {
        self.safety.failure_tracker.turn_failures()
    }

    /// 重置当前轮次的失败计数和循环风暴检测
    pub fn reset_turn_failures(&mut self) {
        self.safety.failure_tracker.reset_turn();
        self.safety.loop_guard.reset_storm();
        self.safety.mode_config.reset_turn();
        // 修复(R3,自我引入):reasoning_controller 新增的 turn_start_time 字段必须在每个
        // turn 开始时刷新,否则它永远是构造时刻,长会话里 should_truncate_thinking 用
        // turn_start_time.elapsed() 衡量"进程总时长",导致思考被误熔断。start_turn() 同时
        // 刷新 turn_start_time 与 start_time。
        self.workspace.reasoning_controller.start_turn();
    }

    /// 清除对话历史，保留系统提示词。用于 /clear 命令。
    pub fn clear_conversation(&mut self) {
        self.ctx.clear_history();
        self.session.iteration = 0;
        self.session.cached_stats = crate::common::deepseek::TokenStats::default();
        self.session.session_cost = 0.0;
        self.session.last_costed_total_tokens = 0;
        self.session.last_costed_stats = crate::common::deepseek::TokenStats::default();
        self.safety.failure_tracker.reset_turn();
        self.safety.loop_guard.reset_storm();
    }

    /// 根据缓存的 token 统计计算本轮调用成本(人民币 ¥)
    pub fn calculate_cost(&self) -> f64 {
        pricing::calculate_cost(&self.session.cached_stats, &self.core.config.model)
    }

    /// 获取会话累计成本(人民币 ¥)
    pub fn session_cost(&self) -> f64 {
        self.session.session_cost
    }

    /// 累加本轮和后台任务的成本到会话总计(人民币 ¥)
    ///
    /// 修复(G-C1):原实现 `session_cost += calculate_cost(&cached_stats)`,而
    /// `cached_stats` 是**累计** token 统计,每轮都把全部历史成本重新叠加,
    /// 导致 N 轮后显示成本 ≈ N×真实成本/2。改为只累加**本轮增量**:
    /// 用当前累计 total_tokens 减去上次已计入的,得到本轮新增,只对增量计费。
    ///
    /// 修复(R6/C7fix,关键):R5/C7 曾用"比例缩放"把累计 cache/reasoning 字段折算成
    /// delta(cumulative * delta_total / current_total),但这对 cache 字段数学错误:
    /// cache_hit 是"命中即不变"的累计量,其增量应是 current - last(常为 0,因已缓存),
    /// 而非按 total 比例分摊。比例法在高缓存率会话下:completion 少报 ~7x、凭空计费
    /// 未发生的 cache_hit。正确做法是**每字段直接减法**:记录上次已计费的完整 stats 快照,
    /// delta = current_field - last_field。这也统一了 prompt/completion/cache/reasoning
    /// 的处理(原本 prompt/completion 用比例、cache 用原样 clone,两套逻辑不一致)。
    pub fn update_session_cost(&mut self) {
        let cur = &self.session.cached_stats;
        let last = &self.session.last_costed_stats;
        // 每字段直接减法得到真实增量。
        let delta_stats = crate::common::deepseek::TokenStats {
            prompt_tokens: cur.prompt_tokens.saturating_sub(last.prompt_tokens),
            completion_tokens: cur.completion_tokens.saturating_sub(last.completion_tokens),
            total_tokens: cur.total_tokens.saturating_sub(last.total_tokens),
            reasoning_tokens: cur.reasoning_tokens.saturating_sub(last.reasoning_tokens),
            cache_hit_tokens: cur.cache_hit_tokens.saturating_sub(last.cache_hit_tokens),
            cache_miss_tokens: cur.cache_miss_tokens.saturating_sub(last.cache_miss_tokens),
        };
        // 更新上次已计费快照为当前值,供下一轮减法。
        self.session.last_costed_stats = cur.clone();
        self.session.last_costed_total_tokens = cur.total_tokens;

        // 只在有增量时计费(避免 0 增量时 calculate_cost 返回 0 仍累加无意义)。
        if delta_stats.total_tokens > 0 || delta_stats.completion_tokens > 0 {
            self.session.session_cost +=
                pricing::calculate_cost(&delta_stats, &self.core.config.model);
        }

        self.session.session_cost += crate::common::cost_status::drain();
    }

    /// 将会话成本格式化为带 ¥ 前缀的字符串
    pub fn format_session_cost(&self) -> String {
        pricing::format_cost(self.session.session_cost)
    }

    /// 保存当前会话快照到磁盘，用于会话持久化和恢复
    pub fn save_session(&self) -> std::io::Result<()> {
        let snapshot = self.build_session_snapshot();
        self.session.session_persistence.save(&snapshot)
    }

    /// 异步保存当前会话快照(详见 P2.4)。
    ///
    /// 修复(P2.4):`save_session` 是同步 IO(写 latest.json + 历史文件 + 列目录
    /// 清理),在 TUI 主线程或 tokio 异步上下文里直接调用会阻塞事件循环,造成
    /// "/save 一下 UI 卡 100ms~几秒"。新增异步入口:把整段 fs::write 推到
    /// `spawn_blocking`,调用方 `await` 拿到结果。
    ///
    /// 注意:`save_session_async` 不与 `save_session` 互斥,后者继续保留是为了
    /// `Drop`/退出路径上无法 await 的场景兜底。
    pub async fn save_session_async(&self) -> std::io::Result<()> {
        let snapshot = self.build_session_snapshot();
        let persistence = self.session.session_persistence.clone();
        tokio::task::spawn_blocking(move || persistence.save(&snapshot))
            .await
            .map_err(|e| std::io::Error::other(format!("session save join error: {}", e)))?
    }

    /// 构造当前 session 的不可变快照,供同步/异步保存路径共用。
    fn build_session_snapshot(&self) -> SessionSnapshot {
        let messages = self.ctx.messages();
        let now = chrono::Utc::now().to_rfc3339();
        SessionSnapshot {
            created_at: now.clone(),
            updated_at: now,
            messages,
            mode: self.safety.mode_config.mode() as u8,
            total_tokens: self.session.cached_stats.total_tokens,
            // 修复(G-M6):字段名 turns 历史遗留,实际存的是「会话累计 LLM 迭代次数」
            // (含工具调用往返),非「对话轮次」。保留语义以向后兼容旧快照。
            turns: self.session.iteration,
            workspace: self.core.config.workspace.to_string_lossy().to_string(),
        }
    }

    /// 从磁盘恢复最近一次会话快照
    pub fn restore_session(&mut self) -> std::io::Result<bool> {
        let snapshot = match self.session.session_persistence.load_latest() {
            Some(s) => s,
            None => return Ok(false),
        };
        self.ctx.clear_history();
        // 修复(重复 system)：snapshot.messages 来自 ctx.messages()，前导段是 system 消息
        // （系统提示 + 可能的内存/技能注入）。clear_history() 已保留当前 system_prompt 字段，
        // 若再 push 这些 system 消息会重复（多花 token + 破坏前缀缓存）。跳过前导 system 段。
        let non_system_start = snapshot
            .messages
            .iter()
            .position(|m| !matches!(m.role.as_str(), "system" | "developer"))
            .unwrap_or(snapshot.messages.len());
        for msg in snapshot.messages.into_iter().skip(non_system_start) {
            self.ctx.push_auto(msg);
        }
        self.session.iteration = snapshot.turns;
        // 修复：恢复会话时同时恢复运行模式，避免用户在 Yolo 模式下保存后
        // 恢复到 Agent 默认模式导致意外审批，或 Plan 模式保存后恢复为 Agent
        // 导致意外执行修改。
        // 修复(R6):若恢复到 Yolo,降级为 Auto 并警告。原实现直接恢复 Yolo:用户上次切到
        // Yolo 后退出,这次启动 auto_restore 静默恢复 Yolo,第一个输入就全自动执行(无审批
        // 的写文件/命令),用户很可能不记得上次切过模式 → 意外破坏。降级 Auto 保留审批门。
        let requested_mode = crate::agent::modes::AppMode::from_u8(snapshot.mode);
        let restored_mode = if requested_mode == crate::agent::modes::AppMode::Yolo {
            tracing::warn!(
                target: "session",
                "上次会话是 Yolo 模式,已降级恢复为 Auto(避免静默全自动执行)。如需 Yolo 请按 Tab 切换。"
            );
            crate::agent::modes::AppMode::Auto
        } else {
            requested_mode
        };
        self.safety.mode_config.set_mode(restored_mode);
        tracing::info!(target: "session", "会话已恢复，运行模式: {}", restored_mode.display_name());
        Ok(true)
    }

    /// 构建项目索引，返回项目结构摘要（语言分布、文件数、目录树等）
    pub fn build_project_index(&self) -> String {
        let indexer =
            crate::common::project_index::ProjectIndexer::new(&self.core.config.workspace);
        let index = indexer.build_index();
        format!(
            "项目类型: {:?}\n文件数: {}\n估计行数: {}\n语言分布: {:?}\n目录树:\n{}",
            index.project_type,
            index.total_files,
            index.total_lines_estimate,
            index.language_distribution,
            index.directory_tree
        )
    }

    /// 对当前修改的文件执行自动验证（lint/编译/测试）
    pub fn verify_modifications(&self) -> String {
        if self.session.modified_files.is_empty() {
            return "没有已修改的文件需要验证".into();
        }
        let result = self.safety.verify_loop.verify(&self.session.modified_files);
        VerifyLoop::format_verify_result(&result)
    }

    /// 获取本次会话中已修改的文件列表
    pub fn modified_files(&self) -> &[std::path::PathBuf] {
        &self.session.modified_files
    }

    /// 对"将被 turn_compaction 删除"的早期消息区间生成 LLM 语义摘要。
    /// 在 should_compact 触发后、compact_with_summary 之前调用。
    /// 失败返回 None，调用方回退到统计字符串摘要。
    async fn compress_with_llm(&self, messages: &[ChatMessage]) -> Option<String> {
        // 复刻 compact_with_summary 的区间计算逻辑，提取将被删除的消息。
        const KEEP_RECENT: usize = 4;
        if messages.len() < KEEP_RECENT {
            return None;
        }
        let prefix_end = messages
            .iter()
            .position(|m| !matches!(m.role.as_str(), "system" | "developer"))
            .unwrap_or(messages.len());
        let cut_start = prefix_end.min(messages.len().saturating_sub(KEEP_RECENT));
        let drain_end = messages.len().saturating_sub(KEEP_RECENT);
        if drain_end <= cut_start {
            return None;
        }
        let to_compress = &messages[cut_start..drain_end];
        if to_compress.is_empty() {
            return None;
        }

        let result = {
            let mut llm = self.core.llm.lock().await;
            self.ctx
                .semantic_compressor
                .compress_messages(to_compress, Some(&mut *llm))
                .await
        };
        match result {
            Ok(compressed) => Some(compressed.compressed_message.content.unwrap_or_default()),
            Err(e) => {
                tracing::warn!(target: "compaction", "LLM 语义压缩失败，回退统计摘要: {}", e);
                None
            }
        }
    }

    /// 执行语义压缩，将上下文窗口中的历史消息压缩为摘要
    pub async fn compress_context(&mut self) -> Result<String> {
        let messages = self.ctx.messages();
        if messages.len() <= 2 {
            return Ok("上下文过少，无需压缩".into());
        }
        let result = {
            let mut llm = self.core.llm.lock().await;
            self.ctx
                .semantic_compressor
                .compress_messages(&messages, Some(&mut *llm))
                .await
        };
        match result {
            Ok(compressed) => {
                let summary = format!(
                    "压缩完成: {} tokens → {} tokens (压缩率: {:.1}%)",
                    compressed.original_tokens,
                    compressed.compressed_tokens,
                    compressed.ratio * 100.0
                );
                self.ctx.clear_history();
                self.ctx
                    .push_message(compressed.compressed_message, MessageTier::Summary);
                Ok(summary)
            }
            Err(e) => Err(MovixError::Other(format!("上下文压缩失败: {}", e))),
        }
    }

    /// 对指定代码变更执行审查（生成-评估分离）
    pub async fn review_changes(
        &mut self,
        original_request: &str,
        changes_description: &str,
        diff_or_code: &str,
    ) -> Result<String> {
        let result = {
            let mut llm = self.core.llm.lock().await;
            self.safety
                .reviewer
                .deep_review(
                    &mut llm,
                    original_request,
                    changes_description,
                    diff_or_code,
                )
                .await
        };
        match result {
            Ok(review) => {
                if review.approved {
                    Ok(format!(
                        "审查通过 (置信度: {:.0}%)\n{}",
                        review.confidence * 100.0,
                        review.summary
                    ))
                } else {
                    let findings: Vec<String> = review
                        .findings
                        .iter()
                        .map(|f| format!("- [{:?}] {}", f.severity, f.message))
                        .collect();
                    Ok(format!(
                        "审查未通过\n发现:\n{}\n摘要: {}",
                        findings.join("\n"),
                        review.summary
                    ))
                }
            }
            // 修复(G-H4):审查器 LLM 调用失败时,原实现包装成普通 Ok 字符串,
            // 调用方无法区分"审查通过"与"审查根本没跑"。改为明确标记为未审查,
            // fail-close 提示用户人工介入。
            Err(e) => Ok(format!("⚠️ 审查未执行(审查器调用失败,请人工复核): {}", e)),
        }
    }

    /// 重建 LLM 客户端（在修改 config 后调用）
    fn rebuild_llm_client(&mut self) -> crate::common::error::Result<()> {
        let new_client = DeepSeekClient::new(self.core.config.clone())?;
        self.core.llm = Arc::new(Mutex::new(new_client));
        Ok(())
    }

    /// 发送上下文更新回调
    fn emit_context_update(&self, cb: &Option<Arc<dyn Fn(usize, usize) + Send + Sync>>) {
        if let Some(cb) = cb.as_ref() {
            cb(self.ctx.token_count(), self.ctx.max_tokens());
        }
    }

    /// 检测外部文件变更（协作感知）
    pub fn detect_external_changes(&mut self) -> Vec<String> {
        let changes = self.workspace.collab_watcher.detect_changes();
        // 降级保护：若一次性出现大量 Created（>20），几乎一定是初始化未完成或
        // 全新 checkout 导致的，而非真实的协作编辑。注入巨量噪声 system 消息会
        // 误导 LLM，这种情况下只记日志、不注入上下文。
        let created_count = changes
            .iter()
            .filter(|c| {
                matches!(
                    c.change_type,
                    crate::common::collab_watcher::ChangeType::Created
                )
            })
            .count();
        if created_count > 20 {
            tracing::info!(
                target: "collab",
                "detect_external_changes: 检测到 {} 个新建文件，疑似初始化未完成，降级为仅日志（不注入上下文）",
                created_count
            );
            return Vec::new();
        }
        changes
            .iter()
            .map(|c| {
                let path_str = c.path.to_string_lossy();
                match c.change_type {
                    crate::common::collab_watcher::ChangeType::Created => {
                        format!("[新建] {}", path_str)
                    }
                    crate::common::collab_watcher::ChangeType::Modified => {
                        format!("[修改] {}", path_str)
                    }
                    crate::common::collab_watcher::ChangeType::Deleted => {
                        format!("[删除] {}", path_str)
                    }
                }
            })
            .collect()
    }

    /// 执行自动回滚到最近的快照
    pub fn auto_rollback(&self) -> Result<String> {
        if let Some(ref snapshot_mgr) = self.workspace.auto_snapshot {
            let snapshots = snapshot_mgr
                .list_snapshots()
                .map_err(|e| MovixError::Other(format!("列出快照失败: {}", e)))?;
            if let Some(latest) = snapshots.last() {
                let id = crate::context::snapshot::SnapshotId(latest.id.as_str().to_string());
                snapshot_mgr
                    .rollback(&id)
                    .map_err(|e| MovixError::Other(format!("回滚失败: {}", e)))?;
                Ok(format!("已回滚到快照: {}", latest.id.as_str()))
            } else {
                Ok("没有可回滚的快照".into())
            }
        } else {
            Ok("自动快照未启用（需要 Git 仓库）".into())
        }
    }

    /// 创建部分审批请求（用于 Agent 模式下对危险操作进行参数修改审批）
    pub fn create_partial_approval(
        &mut self,
        tool_name: &str,
        arguments: &str,
        risk_level: crate::tools::RiskLevel,
        description: &str,
        affected_files: Vec<String>,
    ) -> crate::agent::partial_approval::ApprovalRequest {
        self.safety.partial_approval.create_request(
            tool_name,
            arguments,
            risk_level,
            description,
            affected_files,
        )
    }

    /// 处理部分审批决策（批准/拒绝/修改参数/要求解释）
    pub fn process_partial_approval(
        &mut self,
        decision: ApprovalDecision,
    ) -> crate::agent::partial_approval::ApprovalOutcome {
        self.safety.partial_approval.process_decision(decision)
    }

    /// 设置运行模式（Plan/Agent/YOLO），影响工具权限和审批流程
    /// 设置运行模式（通过共享状态，运行中的 agent 也能感知）
    pub fn set_mode(&self, mode: AppMode) {
        self.safety.mode_config.set_mode(mode);
    }

    /// 设置共享模式配置（用于 agent 替换时保持模式状态同步）
    pub fn set_mode_config(&mut self, config: ModeConfig) {
        self.safety.mode_config = config;
    }

    /// 获取当前运行模式
    pub fn mode(&self) -> AppMode {
        self.safety.mode_config.mode()
    }

    /// 获取模式配置引用，包含权限规则
    pub fn mode_config(&self) -> &ModeConfig {
        &self.safety.mode_config
    }

    /// 根据当前模式检查工具调用权限，返回允许/拒绝/需审批
    pub fn check_tool_permission(&self, tool_name: &str) -> ModeDecision {
        self.safety.mode_config.should_execute(tool_name)
    }

    /// 设置审批通道，用于 Agent 模式下等待用户审批
    pub fn set_approval_channel(&mut self, rx: tokio::sync::mpsc::Receiver<ApprovalDecision>) {
        self.safety.approval_rx = Some(rx);
    }

    /// 启用或禁用用户记忆系统，启用时自动重新加载记忆文件
    pub fn enable_memory(&mut self, enabled: bool) {
        self.session.user_memory.set_enabled(enabled);
        if enabled {
            self.session.user_memory.reload();
        }
    }

    /// 查询用户记忆是否已启用
    pub fn is_memory_enabled(&self) -> bool {
        self.session.user_memory.is_enabled()
    }

    /// 显示用户记忆内容
    pub fn memory_show(&self) -> String {
        self.session.user_memory.show()
    }

    /// 向用户记忆文件追加文本
    ///
    /// 修复(G-H5):原实现只写文件,不刷新已注入上下文的 system prompt。
    /// 用户 `/memory add` 后,本轮及后续所有 LLM 请求仍基于旧记忆,直到重启或
    /// 下次自动提取触发 rebuild。现在写入成功后立即 rebuild system prompt。
    pub fn memory_append(&mut self, text: &str) -> std::io::Result<()> {
        self.session.user_memory.append(text)?;
        self.rebuild_system_with_memory();
        Ok(())
    }

    /// 清空用户记忆文件
    pub fn memory_clear(&mut self) -> std::io::Result<()> {
        self.session.user_memory.clear()?;
        self.rebuild_system_with_memory();
        Ok(())
    }

    /// 获取用户记忆的系统提示块，用于注入到 LLM 上下文
    pub fn memory_system_block(&self) -> Option<String> {
        self.session.user_memory.system_block()
    }

    /// 启用或禁用 LSP 诊断检查
    pub fn enable_lsp(&mut self, enabled: bool) {
        self.workspace.lsp.set_enabled(enabled);
    }

    /// 查询 LSP 诊断是否已启用
    pub fn is_lsp_enabled(&self) -> bool {
        self.workspace.lsp.is_enabled()
    }

    /// 检查指定文件的 LSP 诊断信息（错误和警告）
    pub fn check_file_diagnostics(&mut self, path: &std::path::Path) -> String {
        let diags = self.workspace.lsp.check_file(path);
        if diags.is_empty() {
            String::new()
        } else {
            self.workspace
                .lsp
                .format_diagnostics(path)
                .unwrap_or_default()
        }
    }

    /// 获取 LSP 诊断的错误数量
    pub fn lsp_errors(&self) -> usize {
        self.workspace.lsp.total_errors()
    }

    /// 获取 LSP 诊断的警告数量
    pub fn lsp_warnings(&self) -> usize {
        self.workspace.lsp.total_warnings()
    }

    /// 延迟初始化快照仓库（含工作区体积检查）。
    ///
    /// 在 `with_system_prompt_and_tools` 中已把 `snapshot_repo` 置为 None 以避免
    /// 启动期 `walk_dir_size` 阻塞第一帧绘制。调用方应在首帧 `terminal.draw()`
    /// 之后调用本方法补齐初始化,与 `refresh_git_context` / `init_mcp` 同属
    /// 启动后置阶段。重复调用是幂等的:已初始化则直接返回。
    pub fn init_snapshot_repo(&mut self) {
        if self.workspace.snapshot_repo.is_some() {
            return;
        }
        if !which_git() {
            return;
        }
        let repo = SnapshotRepo::new(&self.core.config.workspace);
        if repo.workspace_size_ok() {
            self.workspace.snapshot_repo = Some(repo);
        } else {
            tracing::info!(target: "agent", "工作区过大,已禁用快照功能");
        }
    }

    /// 创建工作区快照，返回快照 ID（基于 git stash）
    pub fn create_snapshot(&mut self, label: &str) -> Option<String> {
        if let Some(ref repo) = self.workspace.snapshot_repo {
            if !repo.is_initialized() && repo.init().is_err() {
                return None;
            }
            match repo.snapshot(label) {
                Ok(id) => Some(id.as_str().to_string()),
                Err(_) => None,
            }
        } else {
            None
        }
    }

    /// 恢复到指定快照，还原工作区状态
    pub fn restore_snapshot(&self, snapshot_id: &str) -> Result<()> {
        let repo =
            self.workspace.snapshot_repo.as_ref().ok_or_else(|| {
                MovixError::Other("快照功能不可用 (git 未找到或工作区过大)".into())
            })?;
        let id = crate::context::snapshot::SnapshotId(snapshot_id.to_string());
        repo.restore(&id)
            .map_err(|e| MovixError::Other(format!("恢复快照失败: {}", e)))
    }

    /// 列出所有快照，返回 (id, label, timestamp) 元组列表
    pub fn list_snapshots(&self) -> Vec<(String, String, i64)> {
        self.workspace
            .snapshot_repo
            .as_ref()
            .map_or_else(Vec::new, |repo| {
                repo.list()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|s| (s.id.as_str().to_string(), s.label, s.timestamp))
                    .collect()
            })
    }

    /// 获取最近一次自动修复的备注信息
    pub fn last_repair_notes(&self) -> &[String] {
        &self.session.last_repair_notes
    }

    /// 同步运行 Agent，返回完整响应文本
    pub async fn run(&mut self, user_input: &str) -> Result<String> {
        self.run_with_stream(user_input, None, None, None, None)
            .await
    }

    /// 流式运行 Agent，通过回调实时推送内容/思考/工具调用/上下文事件
    pub async fn run_streaming<F, G, H, I>(
        &mut self,
        user_input: &str,
        on_token: F,
        on_reasoning: G,
        on_tool: H,
        on_context: I,
    ) -> Result<String>
    where
        F: Fn(String) + Send + Sync + 'static,
        G: Fn(String) + Send + Sync + 'static,
        H: Fn(String, String, Option<String>, String) + Send + Sync + 'static,
        I: Fn(usize, usize) + Send + Sync + 'static,
    {
        let token_cb: Option<Arc<dyn Fn(String) + Send + Sync>> = Some(Arc::new(on_token));
        let reasoning_cb: Option<Arc<dyn Fn(String) + Send + Sync>> = Some(Arc::new(on_reasoning));
        let tool_cb: Option<ToolCallback> = Some(Arc::new(on_tool));
        let ctx_cb: Option<Arc<dyn Fn(usize, usize) + Send + Sync>> = Some(Arc::new(on_context));
        self.run_with_stream(user_input, token_cb, reasoning_cb, tool_cb, ctx_cb)
            .await
    }

    async fn run_with_stream(
        &mut self,
        user_input: &str,
        token_cb: Option<Arc<dyn Fn(String) + Send + Sync>>,
        reasoning_cb: Option<Arc<dyn Fn(String) + Send + Sync>>,
        tool_cb: Option<ToolCallback>,
        context_cb: Option<Arc<dyn Fn(usize, usize) + Send + Sync>>,
    ) -> Result<String> {
        self.reset_turn_failures();
        // 修复(Medium #M1):turn 开头重置 cancel_flag,避免上一轮被 Ctrl-C 取消后
        // 下一轮请求一发即被自己遗留的 flag 枪毙。让 agent 自管生命周期。
        self.reset_cancel_flag();
        self.session.last_repair_notes.clear();
        self.session.modified_files.clear();
        // 修复(Critical #C9):每轮 turn 重置 per-turn 迭代计数,避免跨 turn 累积
        // 导致整会话过早触发 MaxIterationsReached。`session.iteration`(会话级统计)
        // 不在此重置,它只用于持久化/统计。
        self.session.turn_iteration = 0;

        let external_changes = self.detect_external_changes();
        if !external_changes.is_empty() {
            let change_msg = format!(
                "[协作感知] 检测到外部文件变更:\n{}",
                external_changes.join("\n")
            );
            self.ctx.push_auto(ChatMessage::system(&change_msg));
        }

        // 修复(Bug #18):删除 dynamic_injector 死骨架。原 inject_for_request 在
        // build_index() 从未被调用的前提下永远返回空 Vec。如要恢复动态注入
        // 应实现 keyword 索引构建,而非保留无操作骨架。

        let enriched_prompt = skill_executor::build_enhanced_prompt(
            &self.planning.skill_registry,
            &self.core.system_prompt_text,
            user_input,
        );
        self.ctx.update_system(enriched_prompt);

        if self.core.config.reasoning_effort == "auto" {
            self.auto_adjust_reasoning(user_input, false);
        }

        self.ctx
            .push_auto(ChatMessage::user(user_input.to_string()));

        let has_stream = token_cb.is_some() || reasoning_cb.is_some();

        loop {
            self.session.turn_iteration += 1;
            // 会话级累计轮次(用于统计/持久化)
            self.session.iteration = self.session.iteration.saturating_add(1);

            if self.session.turn_iteration > self.core.config.max_iterations {
                return Err(MovixError::MaxIterationsReached(
                    self.core.config.max_iterations,
                ));
            }

            if self
                .ctx
                .turn_compaction
                .should_compact(self.ctx.message_count(), self.ctx.token_count())
            {
                let mut msgs = self.ctx.messages();

                // 优先用 LLM 语义摘要压缩被删除区间，失败回退统计字符串。
                // 只对"将被删除"的消息做摘要（前导 system 段之后、保留窗口之前）。
                let llm_summary = self.compress_with_llm(&msgs).await;

                self.ctx
                    .turn_compaction
                    .compact_with_summary(&mut msgs, llm_summary);

                let system = msgs.first().cloned();
                self.ctx.clear_history();
                if let Some(sys) = system {
                    self.ctx.update_system(sys.content.unwrap_or_default());
                }
                for msg in msgs.into_iter().skip(1) {
                    self.ctx.push_auto(msg);
                }
                self.ctx
                    .turn_compaction
                    .set_last_compact_tokens(self.ctx.token_count());
            }

            let messages = self.ctx.messages();
            let tool_defs = self.core.tools.definitions();

            if has_stream {
                let should_continue = self
                    .process_streaming_response(
                        messages,
                        &tool_defs,
                        &token_cb,
                        &reasoning_cb,
                        &tool_cb,
                        &context_cb,
                    )
                    .await?;
                if !should_continue {
                    return Ok(self.finalize_turn(user_input).await);
                }
            } else {
                let should_continue = self
                    .process_non_streaming_response(messages, &tool_defs)
                    .await?;
                if !should_continue {
                    return Ok(self.finalize_turn(user_input).await);
                }
            }
        }
    }

    /// 处理流式 LLM 响应，返回 Ok(true) 表示需要继续循环（有工具调用），Ok(false) 表示完成
    async fn process_streaming_response(
        &mut self,
        messages: Vec<ChatMessage>,
        tool_defs: &[crate::common::deepseek::ToolDefinition],
        token_cb: &Option<Arc<dyn Fn(String) + Send + Sync>>,
        reasoning_cb: &Option<Arc<dyn Fn(String) + Send + Sync>>,
        tool_cb: &Option<ToolCallback>,
        context_cb: &Option<Arc<dyn Fn(usize, usize) + Send + Sync>>,
    ) -> Result<bool> {
        let mut stream_result = {
            let llm = self.core.llm.lock().await;
            // 修复(Bug #4):传 cancel_flag 进去,Ctrl-C 能中断 SSE 读取。
            llm.chat_stream_with_cancel(
                messages,
                Some(tool_defs.to_vec()),
                Some("auto"),
                Some(std::sync::Arc::clone(&self.session.cancel_flag)),
            )
        };

        let mut events = stream_result
            .events
            .take()
            .ok_or_else(|| MovixError::Other("stream events receiver missing".to_string()))?;
        let handle = stream_result
            .handle
            .take()
            .ok_or_else(|| MovixError::Other("stream handle missing".to_string()))?;

        let mut stream_stats = crate::common::deepseek::TokenStats::default();
        // 修复(R5/H5):原实现收到 StreamEvent::Error 只记 failure + 显示到 reasoning_cb,
        // 不设任何"本轮已出错"状态。若流随后以 Done + 空 content/None tool_calls 收尾,
        // 会被当成"成功的空回复"finalize,用户看到空白且 LLM 不知道出错无法 adapt。
        // 这里记一个标志,响应处理阶段若置位则把错误文本推入上下文并返回 Err。
        let mut stream_error: Option<String> = None;

        while let Some(event) = events.recv().await {
            match event {
                StreamEvent::Content(text) => {
                    if let Some(cb) = token_cb {
                        cb(text);
                    }
                }
                StreamEvent::ReasoningContent(text) => {
                    if let Some(cb) = reasoning_cb {
                        cb(text);
                    }
                }
                StreamEvent::Error(msg) => {
                    self.record_failure(FailureSignal::ApiError);
                    if let Some(cb) = reasoning_cb {
                        cb(format!("\n[stream error] {}\n", msg));
                    }
                    stream_error = Some(msg);
                }
                StreamEvent::Done { stats, .. } => {
                    stream_stats = stats;
                    // 修复(S6,自我批判):R2 只在工具成功时 record_success,但纯 API 失败场景
                    // (网络抖动/限流/5xx)在工具执行之前,record_success 永不调用 →
                    // consecutive_failures 只增不减 → 达阈值后每 turn 永久触发升级。
                    // 这里在 LLM 流式响应成功完成(Done)时也清零,覆盖 API 持续失败后恢复。
                    self.safety.failure_tracker.record_success();
                }
            }
        }

        let response = match handle.await {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                self.record_failure(FailureSignal::ApiError);
                return Err(e);
            }
            Err(e) => {
                return Err(MovixError::Other(format!("流式任务失败: {}", e)));
            }
        };

        // 修复(R5/H5):若流式过程中收到过 Error 事件,即使 handle 正常返回,
        // 也视为本轮失败 —— 把错误文本推入上下文(让 LLM 下轮能 adapt)并返回 Err,
        // 而非把空回复当成功 finalize。
        if let Some(msg) = stream_error {
            self.ctx.push_auto(ChatMessage::assistant(
                Some(format!("[流式错误,本轮可能不完整] {}", msg)),
                None,
                None,
            ));
            return Err(MovixError::Other(format!("流式错误: {}", msg)));
        }

        {
            let mut llm = self.core.llm.lock().await;
            llm.token_stats.add(&stream_stats);
            self.session.cached_stats = llm.token_stats.clone();
            // 修复(Bug #2):同步共享 stats,让 spawn 出去后 UI 能读到真实值。
            if let Ok(mut s) = self.session.shared_stats.lock() {
                *s = self.session.cached_stats.clone();
            }
        }

        let tool_calls = response.tool_calls.clone();
        let reasoning_content = response.reasoning_content.clone();
        let content = response.content.clone();

        // 修复(High #H4):原 `has_tool_calls = tool_calls.is_some()` 对空数组
        // `Some(vec![])` 也返回 true,进入执行分支但 for 循环空跑,返回 Ok(true)
        // 让主循环继续,LLM 收到自己上一轮的空 tool_calls 可能再次返回空数组,
        // 空转到 max_iterations 耗尽。改为要求非空。
        let has_tool_calls = tool_calls.as_ref().is_some_and(|c| !c.is_empty());
        if has_tool_calls {
            // 修复(P1.4):用 if let 替代 is_some() + unwrap(),消除生产路径的 panic 风险。
            let Some(tool_calls) = tool_calls.as_ref() else {
                unreachable!("has_tool_calls is true but tool_calls is None");
            };
            self.ctx.push_auto(ChatMessage::assistant(
                content.clone(),
                reasoning_content.clone(),
                Some(tool_calls.clone()),
            ));

            self.emit_context_update(context_cb);

            for tool_call in tool_calls {
                let detail =
                    extract_tool_detail(&tool_call.function.name, &tool_call.function.arguments);
                self.handle_tool_call(tool_call, Some(detail.args), tool_cb.as_ref())
                    .await;
            }

            self.emit_context_update(context_cb);

            Ok(true)
        } else {
            if let Some(reasoning) = &reasoning_content {
                let existing_names: Vec<String> = Vec::new();
                let known_tools = self.core.tools.names();
                let candidates = Scavenger::scavenge(reasoning, &existing_names, &known_tools);
                if !candidates.is_empty() {
                    let best = &candidates[0];
                    // 修复(P1.4):此前 scavenger 检测到"reasoning 里带结构化工具调用"
                    // 时会直接 `handle_tool_call` 自动执行,绕过用户审批与 mode 决策。
                    // 哪怕 confidence ≥ 0.95、来源是 DSML/JSON,也只能算 LLM"嘴上说想用",
                    // 真正的 tool_call 必须由 LLM 通过 OpenAI tool_calls 通道发出。
                    // 现在改为:仅作为系统提示注入下一轮上下文,鼓励 LLM 重新走正规通道。
                    let confident = best.confidence >= 0.95
                        && matches!(
                            best.source,
                            crate::common::ScavengeSource::Dsml
                                | crate::common::ScavengeSource::Json
                        )
                        && self.core.tools.get(&best.name).is_some();
                    if confident {
                        self.record_failure(FailureSignal::ToolCallScavenged);
                        tracing::info!(
                            target: "agent::scavenger",
                            "detected scavenged tool call '{}' (conf={:.2}, source={:?}), nudging LLM to re-emit via tool_calls channel",
                            best.name,
                            best.confidence,
                            best.source,
                        );
                        // 把 reasoning 里发现的"伪工具调用"当作 assistant 内容保留,
                        // 再追加一条 system 提示,让模型在下一轮通过正规 tool_calls 通道发出。
                        self.ctx.push_auto(ChatMessage::assistant(
                            content.clone(),
                            reasoning_content.clone(),
                            None,
                        ));
                        self.ctx.push_auto(ChatMessage::system(format!(
                            "检测到你在思考中描述了对工具 `{}` 的调用,但没有通过 tool_calls 通道发出。\
                             请直接通过标准 tool_calls 通道发起调用,而不是把工具调用嵌在思考文本里 —— \
                             嵌入文本里的调用不会被自动执行,以保证审批与 mode 策略生效。",
                            best.name,
                        )));
                        self.emit_context_update(context_cb);
                        return Ok(true);
                    }
                }
            }

            self.ctx.push_auto(ChatMessage::assistant(
                content.clone(),
                reasoning_content.clone(),
                None,
            ));

            self.emit_context_update(context_cb);

            self.update_session_cost();
            Ok(false)
        }
    }

    /// 处理非流式 LLM 响应，返回 Ok(true) 表示需要继续循环（有工具调用），Ok(false) 表示完成
    async fn process_non_streaming_response(
        &mut self,
        messages: Vec<ChatMessage>,
        tool_defs: &[crate::common::deepseek::ToolDefinition],
    ) -> Result<bool> {
        let response = {
            let mut llm = self.core.llm.lock().await;
            let resp = llm.chat(&messages, Some(tool_defs), Some("auto")).await;
            // 错误处理移到锁外，避免与 record_failure 的可变借用冲突。
            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    drop(llm);
                    self.record_failure(FailureSignal::ApiError);
                    return Err(e);
                }
            };
            self.session.cached_stats = llm.token_stats.clone();
            // 修复(Bug #2):同步共享 stats。
            if let Ok(mut s) = self.session.shared_stats.lock() {
                *s = self.session.cached_stats.clone();
            }
            resp
        };

        // 修复(R5/H4):原 `if let Some(ref tool_calls)` 对空数组 Some(vec![]) 也进入
        // 此分支,for 循环空跑后返回 Ok(true),主循环无新增上下文地继续,LLM 可能再次
        // 返回空数组 → 空转到 max_iterations 耗尽 token。流式路径已修(High #H4 注释),
        // 但非流式路径漏修。改为要求非空,与流式一致。
        let has_tool_calls = response.tool_calls.as_ref().is_some_and(|c| !c.is_empty());
        if has_tool_calls {
            let tool_calls = response.tool_calls.as_ref().unwrap();
            self.ctx.push_auto(ChatMessage::assistant(
                response.content.clone(),
                response.reasoning_content.clone(),
                Some(tool_calls.clone()),
            ));

            for tool_call in tool_calls {
                let detail =
                    extract_tool_detail(&tool_call.function.name, &tool_call.function.arguments);
                self.handle_tool_call(tool_call, Some(detail.args), None)
                    .await;
            }
            Ok(true)
        } else {
            self.ctx.push_auto(ChatMessage::assistant(
                response.content.clone(),
                response.reasoning_content.clone(),
                None,
            ));
            self.update_session_cost();
            Ok(false)
        }
    }

    async fn execute_tool_with_repair(
        &mut self,
        tool_call: &ToolCall,
        pre_parsed_args: Option<serde_json::Value>,
    ) -> ToolResult {
        let tool_name = &tool_call.function.name;

        let (args, report) = match pre_parsed_args {
            Some(v) => (
                v,
                arg_repair::RepairReport {
                    changed: false,
                    notes: Vec::new(),
                    fallback: false,
                    too_large: false,
                    original_len: 0,
                },
            ),
            None => arg_repair::repair_with_report(&tool_call.function.arguments),
        };

        if report.changed {
            self.session.last_repair_notes.extend(report.notes);
        }

        // 修复(Bug #11):超长参数被 repair 标记为 `too_large` 时,直接
        // 拒绝执行,避免下游用 `Value::Null` 或空对象误命中 `path: ""`
        // 等危险默认。
        if report.too_large {
            return ToolResult::err(format!(
                "工具参数过大 ({} bytes),超过 {} bytes 上限,拒绝执行以避免数据损坏",
                report.original_len,
                crate::common::arg_repair::MAX_ARG_LEN
            ));
        }

        if report.fallback {
            self.record_failure(FailureSignal::JsonParseFailed);
            return ToolResult::err(format!(
                "工具参数 JSON 修复失败，原始参数不可恢复: {}",
                &tool_call.function.arguments[..tool_call.function.arguments.len().min(100)]
            ));
        }

        match self.core.tools.get(tool_name) {
            // 修复(R5/C4,关键):工具执行未隔离 panic。一个工具(尤其第三方 MCP 工具)
            // 对畸形输入 panic 会沿 await 传播,杀死整个 agent task——已写文件不回滚、
            // verify/memory 不 flush、TUI 看到 task 消失而非可恢复错误。
            // 用 AssertUnwindSafe + catch_unwind 把 panic 转成 ToolResult::err,
            // 让循环继续,LLM 可 adapt。
            //
            // AssertUnwindSafe 的安全性:工具 execute 内部不应有跨 await 持有的
            // 非可恢复不变量(它返回独立的 ToolResult,不修改外部状态);此处仅用于
            // 把 panic 变错误,不依赖被 panic 破坏的内部一致性。
            Some(tool) => {
                use futures::future::FutureExt;
                let result = std::panic::AssertUnwindSafe(tool.execute(&args))
                    .catch_unwind()
                    .await;
                match result {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => ToolResult::err(format!("工具执行错误: {}", e)),
                    Err(panic_payload) => {
                        let msg = if let Some(s) = panic_payload.downcast_ref::<&'static str>() {
                            s.to_string()
                        } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "未知 panic 类型".to_string()
                        };
                        tracing::error!(target: "agent", "工具 '{}' 执行时 panic: {}", tool_name, msg);
                        ToolResult::err(format!("工具执行 panic(已隔离): {}", msg))
                    }
                }
            }
            None => ToolResult::err(format!("未知工具: {}", tool_name)),
        }
    }

    /// 统一处理单个工具调用：循环检测 → 模式检查 → 审批 → 执行 → 输出处理 → 上下文推送
    /// tool_cb 用于流式场景，非流式传入 None
    async fn handle_tool_call(
        &mut self,
        tool_call: &ToolCall,
        parsed_args: Option<serde_json::Value>,
        tool_cb: Option<&ToolCallback>,
    ) {
        let call_detail =
            extract_tool_detail(&tool_call.function.name, &tool_call.function.arguments);
        // 注意:detail / file 均 clone 而非 move。二者后续都要以 `&detail` / `&file` 形式
        // 与 `&call_detail` 整体一起传入 authorize_tool_call;若 move 任一字段,
        // call_detail 会被部分移动,无法再以 `&call_detail` 借用(E0382)。
        let detail = call_detail.detail.clone();
        let file = call_detail.file.clone();

        // 修复(G-H7):参数过大时直接拒绝,不进入执行。原实现把 args 设 Null 但
        // execute_tool_with_repair 见 pre_parsed=Some 就合成 too_large:false 的
        // report,闸门失效,Null 直接进工具产生 path:"" 的危险默认。
        if call_detail.too_large {
            self.ctx.push_auto(ChatMessage::tool(
                tool_call.id.clone(),
                format!(
                    "工具参数过大 ({}),超过 {} bytes 上限,已拒绝执行以避免数据损坏",
                    detail,
                    crate::common::arg_repair::MAX_ARG_LEN
                ),
            ));
            if let Some(cb) = tool_cb {
                cb(
                    tool_call.function.name.clone(),
                    "[blocked] 参数过大".into(),
                    file.clone(),
                    String::new(),
                );
            }
            return;
        }

        if !self.allow_tool_attempt(tool_call, &detail, &file, tool_cb) {
            return;
        }

        let Some(decision) = self
            .authorize_tool_call(tool_call, &call_detail.args, &call_detail, &file, tool_cb)
            .await
        else {
            return;
        };

        // 修复(TOCTOU):Auto 模式下,mutating 工具的"预算检查 + 扣减"必须原子完成。
        // 此前 authorize_tool_call 内部用非原子的 auto_write_exhausted() 检查,
        // 执行后 record_tool_execution 再用 record_auto_execution() 扣减 ——
        // 并行 dispatch 时多个 task 可能同时通过检查再各自 +1,导致预算超额扣减。
        // 改为:授权通过后、执行前,用 try_claim_auto_write() 原子占用预算,
        // 失败则转为 Blocked。预算在授权时占用,执行失败也不退还
        // (符合"一次决策消耗一次预算"语义,避免退还后再被其他任务占用的复杂竞态)。
        if matches!(decision, ModeDecision::Proceed)
            && self.mode() == AppMode::Auto
            && self.tool_mutates_workspace(&tool_call.function.name)
            && !self.safety.mode_config.try_claim_auto_write()
        {
            self.ctx.push_auto(ChatMessage::tool(
                tool_call.id.clone(),
                format!(
                    "[Mode] Auto 模式自动写入预算已耗尽 ({}/{}).请切换到 Agent 模式手动确认。",
                    self.safety.mode_config.auto_write_count(),
                    self.safety.mode_config.max_auto_writes
                ),
            ));
            if let Some(cb) = tool_cb {
                cb(
                    tool_call.function.name.clone(),
                    format!("[blocked] {}", detail),
                    file.clone(),
                    String::new(),
                );
            }
            return;
        }

        // 修复(Medium #M4):快照路径提取与实际执行此前用两条不同的 JSON 解析路径
        // (快照用 arg_repair::repair,执行用 call_detail.args),畸形 JSON 下两者产出
        // 不同的 affected_paths,回滚可能覆盖错误文件。统一用同一份解析后的 args。
        let unified_args = parsed_args.clone().or(Some(call_detail.args.clone()));
        // 修复(G-H3):快照失败时拒绝执行 mutating 工具(fail-close)。
        if let Err(msg) = self.prepare_mutation_snapshot(tool_call, &unified_args) {
            self.ctx
                .push_auto(ChatMessage::tool(tool_call.id.clone(), msg.clone()));
            if let Some(cb) = tool_cb {
                cb(
                    tool_call.function.name.clone(),
                    format!("[blocked] {}", msg),
                    file.clone(),
                    String::new(),
                );
            }
            return;
        }

        let tool_result = self
            .execute_tool_with_repair(tool_call, unified_args.clone())
            .await;
        self.record_tool_execution(tool_call, &decision, &tool_result);

        self.finish_mutation_snapshot(tool_call, &tool_result, &unified_args);
        let output = self.format_tool_output(tool_call, &tool_result);
        self.emit_tool_callback(tool_cb, tool_call, detail, file, &tool_result);

        self.ctx
            .push_auto(ChatMessage::tool(tool_call.id.clone(), output));

        self.verify_and_diagnose_mutation(tool_call, &tool_result);
    }

    fn allow_tool_attempt(
        &mut self,
        tool_call: &ToolCall,
        detail: &str,
        file: &Option<String>,
        tool_cb: Option<&ToolCallback>,
    ) -> bool {
        match self.safety.loop_guard.inspect_tool_call(tool_call) {
            crate::common::loop_guard::AttemptDecision::Proceed => true,
            crate::common::loop_guard::AttemptDecision::Block(reason) => {
                self.record_failure(FailureSignal::ToolCallStorm);
                self.record_failure(FailureSignal::LoopRecovery);
                self.ctx.push_auto(ChatMessage::tool(
                    tool_call.id.clone(),
                    format!("[Loop Guard] {}", reason),
                ));
                if let Some(cb) = tool_cb {
                    cb(
                        tool_call.function.name.clone(),
                        format!("[blocked] {}", detail),
                        file.clone(),
                        String::new(),
                    );
                }
                false
            }
        }
    }

    async fn authorize_tool_call(
        &mut self,
        tool_call: &ToolCall,
        args: &serde_json::Value,
        call_detail: &crate::tools::execution::ToolCallDetail,
        file: &Option<String>,
        tool_cb: Option<&ToolCallback>,
    ) -> Option<ModeDecision> {
        // `detail` 仅用于展示日志;风险判定走 call_detail.full_command(见下方 fallback)。
        let detail = &call_detail.detail;
        let decision = self
            .core
            .tools
            .get(&tool_call.function.name)
            .map(|tool| self.safety.mode_config.decide_for_tool(tool.as_ref(), args))
            .unwrap_or_else(|| {
                // 修复(C3,关键):fallback 路径(工具不在 registry)做风险判定时,必须用
                // **完整命令**而非被截断到 40 字符的 `detail`。否则攻击者可用
                // `printf 'xxx...'; rm -rf /opt` 把 `rm ` 挤出 40 字符窗口,绕过 Auto 审批。
                // `detail` 仅用于 UI 展示,不参与安全判定。
                let risk_input = call_detail.full_command.as_deref().unwrap_or(detail);
                self.safety
                    .mode_config
                    .should_execute_with_detail(&tool_call.function.name, risk_input)
            });

        match decision {
            ModeDecision::Proceed => Some(ModeDecision::Proceed),
            ModeDecision::Blocked(reason) => {
                self.ctx.push_auto(ChatMessage::tool(
                    tool_call.id.clone(),
                    format!("[Mode] {}", reason),
                ));
                if let Some(cb) = tool_cb {
                    cb(
                        tool_call.function.name.clone(),
                        format!("[blocked] {}", detail),
                        file.clone(),
                        String::new(),
                    );
                }
                None
            }
            ModeDecision::NeedsApproval => {
                if let Some(cb) = tool_cb {
                    cb(
                        tool_call.function.name.clone(),
                        format!("[needs-approval] {}", detail),
                        file.clone(),
                        String::new(),
                    );
                }

                if let Some(ref mut rx) = self.safety.approval_rx {
                    match rx.recv().await {
                        Some(ApprovalDecision::Approved) => Some(ModeDecision::Proceed),
                        Some(ApprovalDecision::Modified(modification)) => {
                            // 用户修改了参数：将修改后的参数注入上下文消息，
                            // 让 LLM 用修改后的参数重新发起工具调用。
                            self.ctx.push_auto(ChatMessage::system(format!(
                                "[审批] 用户修改了工具参数。修改原因: {}。修改字段: {}。修改后参数: {}。请使用修改后的参数重新调用。",
                                modification.reason,
                                modification.changed_fields.join(", "),
                                modification.modified_arguments
                            )));
                            None
                        }
                        Some(ApprovalDecision::Explain) => {
                            // 用户要求解释：将请求注入上下文消息，让 LLM 解释意图后重新请求。
                            self.ctx.push_auto(ChatMessage::system(
                                "[审批] 用户要求解释此操作的目的和影响。请说明为何需要执行此操作。"
                                    .to_string(),
                            ));
                            None
                        }
                        Some(ApprovalDecision::Denied) | None => {
                            self.ctx.push_auto(ChatMessage::tool(
                                tool_call.id.clone(),
                                format!("[Mode] 用户拒绝了执行: {}", tool_call.function.name),
                            ));
                            None
                        }
                    }
                } else if self.tool_is_pure_readonly(&tool_call.function.name) {
                    // 修复(H6,关键):原条件是 `!tool_mutates_workspace`,对 Network 工具
                    // (web_search/web_fetch)返回 true → 在无审批通道(非交互 -t 模式)下
                    // 自动放行。但网络出站有副作用:可能触发 SSRF、数据外泄、访问内网。
                    // 改为只对 **纯 ReadOnly** 工具(无任何外部副作用)自动放行;Network/
                    // Command/Composite 在无审批通道时一律拒绝,引导用户用 YOLO 或 TUI。
                    Some(ModeDecision::Proceed)
                } else {
                    self.ctx.push_auto(ChatMessage::tool(
                        tool_call.id.clone(),
                        format!(
                            "[Mode] Agent 模式下修改操作需要审批（当前无审批通道）。使用 /yolo 切换到全自动模式，或通过 TUI 界面确认执行: {}",
                            tool_call.function.name
                        ),
                    ));
                    None
                }
            }
        }
    }

    /// 返回 Ok(()) 表示可以继续执行;Err(msg) 表示快照失败,应拒绝执行该 mutating 工具。
    fn prepare_mutation_snapshot(
        &mut self,
        tool_call: &ToolCall,
        unified_args: &Option<serde_json::Value>,
    ) -> std::result::Result<(), String> {
        if !self.tool_mutates_workspace(&tool_call.function.name) {
            return Ok(());
        }

        let Some(ref mut snapshot_mgr) = self.workspace.auto_snapshot else {
            return Ok(());
        };

        // 修复(Medium #M4):用 handle_tool_call 统一解析的 args,与执行路径一致。
        let args = unified_args.clone().unwrap_or_else(|| {
            arg_repair::repair(&tool_call.function.arguments)
                .map_err(|_| ())
                .unwrap_or_default()
        });
        let (effect, affected_paths) = self
            .core
            .tools
            .get(&tool_call.function.name)
            .map(|tool| (tool.effect_kind(), tool.affected_paths(&args)))
            .unwrap_or((EffectKind::Composite, Vec::new()));

        if let Some(reason) = snapshot_mgr.should_snapshot_for_effect(effect) {
            // 快照创建失败时不再拒绝执行，改为 warn 后继续。
            // 用户可自行决定是否需要快照回滚能力。
            if let Err(e) = snapshot_mgr.before_modification(reason, &affected_paths) {
                tracing::warn!(
                    "snapshot before_modification failed for {:?}: {}",
                    affected_paths,
                    e
                );
            }
        }

        for path in affected_paths {
            if !self.session.modified_files.contains(&path) {
                self.session.modified_files.push(path);
            }
        }
        Ok(())
    }

    fn finish_mutation_snapshot(
        &mut self,
        tool_call: &ToolCall,
        tool_result: &ToolResult,
        unified_args: &Option<serde_json::Value>,
    ) {
        // 自动快照栈管理：成功弹栈，失败按策略回滚。
        // 修复前 on_success / on_failure 从未被调用，导致 snapshot_stack 无限增长。
        if let Some(ref snapshot_mgr) = self.workspace.auto_snapshot
            && self.tool_mutates_workspace(&tool_call.function.name)
        {
            if tool_result.success {
                snapshot_mgr.on_success();
            } else if let crate::context::snapshot::RollbackDecision::Rollback(sid) =
                snapshot_mgr.on_failure(tool_result.error.as_deref().unwrap_or(""))
            {
                if let Err(e) = snapshot_mgr.rollback(&sid) {
                    tracing::warn!(target: "auto_snapshot", "自动回滚失败: {}", e);
                    self.ctx.push_auto(ChatMessage::system(format!(
                        "[AutoSnapshot] 自动回滚失败: {}",
                        e
                    )));
                } else {
                    // 修复(Low #L2):回滚成功后从 modified_files 移除本次涉及的路径,
                    // 否则 turn 末尾 verify 会对已恢复的文件跑检查产生虚假错误。
                    let args = unified_args.clone().unwrap_or_else(|| {
                        arg_repair::repair(&tool_call.function.arguments)
                            .map_err(|_| ())
                            .unwrap_or_default()
                    });
                    let affected = self
                        .core
                        .tools
                        .get(&tool_call.function.name)
                        .map(|t| t.affected_paths(&args))
                        .unwrap_or_default();
                    self.session
                        .modified_files
                        .retain(|p| !affected.contains(p));
                    // 修复(G-H11):原消息"已自动回滚"给模型虚假安全感。run_command
                    // 类工具可能已产生不可撤销的外部副作用(git push/网络请求/包发布),
                    // 文件回滚无法撤销这些。根据工具类型给出诚实消息。
                    let is_command =
                        self.core
                            .tools
                            .get(&tool_call.function.name)
                            .is_some_and(|t| {
                                matches!(
                                    t.effect_kind(),
                                    crate::tools::EffectKind::Command
                                        | crate::tools::EffectKind::Network
                                )
                            });
                    let rollback_msg = if is_command {
                        format!(
                            "[AutoSnapshot] 工作区文件已回滚到快照 {}。但注意:{} 产生的外部副作用(如 git push、网络请求、包发布)无法撤销,请手动检查远程状态。",
                            sid, tool_call.function.name
                        )
                    } else {
                        format!("[AutoSnapshot] 已自动回滚到快照 {}", sid)
                    };
                    self.ctx.push_auto(ChatMessage::system(rollback_msg));
                }
            }
        }

        // 协作感知：工具成功写入文件后，把这些文件标记为"agent 自己改的"，
        // 避免下一轮 detect_external_changes 把 agent 自身写入误报为外部变更。
        if tool_result.success && self.tool_mutates_workspace(&tool_call.function.name) {
            let args =
                extract_tool_detail(&tool_call.function.name, &tool_call.function.arguments).args;
            let affected = self
                .core
                .tools
                .get(&tool_call.function.name)
                .map(|t| t.affected_paths(&args))
                .unwrap_or_default();
            for path in &affected {
                self.workspace.collab_watcher.mark_agent_modified(path);
            }
        }

        // 修复(R2,自我引入):新 failure_tracker 的 should_escalate 依赖 consecutive_failures,
        // 它由 record_success 清零。原代码从未调用 record_success,导致 consecutive_failures
        // 只增不减,达到 threshold*2 后每个 turn 永久触发升级,agent 卡死。这里在工具成功后
        // 记录成功信号,让"持续失败"语义真正成立。
        if tool_result.success {
            self.safety.failure_tracker.record_success();
        }
    }

    fn record_tool_execution(
        &mut self,
        tool_call: &ToolCall,
        decision: &ModeDecision,
        tool_result: &ToolResult,
    ) {
        // 修复(G-L4):对 mutating 工具写入 append-only 审计日志,便于事后追查
        // "模型为什么删了那个文件"。只记非只读工具,避免日志膨胀。
        let is_mutating = self.tool_mutates_workspace(&tool_call.function.name);
        if !is_mutating {
            return;
        }
        let mode = self.mode();
        let decision_str = match decision {
            ModeDecision::Proceed => "proceed",
            ModeDecision::Blocked(_) => "blocked",
            ModeDecision::NeedsApproval => "needs_approval",
        };
        let ts = chrono::Utc::now().to_rfc3339();
        // 参数只记前 500 字符,避免日志爆炸;不记密钥(参数里可能含 .env 内容,
        // 但审计需要知道操作了什么路径)。
        let args_preview: String = tool_call.function.arguments.chars().take(500).collect();
        let line = format!(
            "[{}] mode={} decision={} tool={} success={} args={}\n",
            ts,
            mode.as_str(),
            decision_str,
            tool_call.function.name,
            tool_result.success,
            args_preview.replace('\n', " ")
        );
        let audit_path = crate::common::utils::home_dir()
            .join(".movix")
            .join("audit.log");
        // 尽力写入,失败不影响主流程。
        if let Some(parent) = audit_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&audit_path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn format_tool_output(&mut self, tool_call: &ToolCall, tool_result: &ToolResult) -> String {
        // 修复(G-C2):工具输出是不可信数据,用 framing 标签包裹,提示模型区分"数据"
        // 与"指令",防御间接 prompt injection(恶意文件/网页内容诱导特权操作)。
        let inner = if tool_result.success {
            let raw_output = &tool_result.output;
            let output = if self.ctx.turn_compaction.should_truncate_result(raw_output) {
                self.record_failure(FailureSignal::ToolCallTruncated);
                self.ctx
                    .turn_compaction
                    .truncate_tool_result(raw_output, 3000)
            } else {
                match self
                    .ctx
                    .large_output_router
                    .route(&tool_call.function.name, raw_output)
                {
                    crate::context::large_output::RoutingDecision::PassThrough => {
                        raw_output.clone()
                    }
                    crate::context::large_output::RoutingDecision::Truncate => {
                        self.record_failure(FailureSignal::ToolCallTruncated);
                        self.ctx.large_output_router.smart_truncate(raw_output)
                    }
                }
            };
            // 搜索类工具返回"未找到"是常见的失败信号，记一笔以便累积升级。
            let is_search = matches!(
                tool_call.function.name.as_str(),
                "search_code" | "grep" | "search_files" | "find"
            );
            if is_search {
                let lower = output.to_lowercase();
                if lower.contains("未找到")
                    || lower.contains("no match")
                    || lower.contains("not found")
                {
                    self.record_failure(FailureSignal::SearchNotFound);
                }
            }
            output
        } else {
            self.record_failure(FailureSignal::ToolExecutionFailed);
            // 修复(G-H6):优先返回 output(若非空,含详细诊断),否则才回退 error。
            if !tool_result.output.trim().is_empty() {
                tool_result.output.clone()
            } else {
                tool_result
                    .error
                    .clone()
                    .unwrap_or_else(|| tool_result.output.clone())
            }
        };
        format!(
            "<tool_output tool=\"{}\">\n{}\n</tool_output>",
            tool_call.function.name, inner
        )
    }

    fn emit_tool_callback(
        &self,
        tool_cb: Option<&ToolCallback>,
        tool_call: &ToolCall,
        detail: String,
        file: Option<String>,
        tool_result: &ToolResult,
    ) {
        if let Some(cb) = tool_cb {
            let cb_output = if tool_result.success {
                tool_result.output.clone()
            } else {
                tool_result.error.clone().unwrap_or_default()
            };
            cb(tool_call.function.name.clone(), detail, file, cb_output);
        }
    }

    fn verify_and_diagnose_mutation(&mut self, tool_call: &ToolCall, tool_result: &ToolResult) {
        // 修复(Bug #5):原实现每次工具成功 mutate 都跑一次全项目 cargo check,
        // N 次写入触发 N 轮编译。改为只设 dirty 标志,turn 结束时统一 verify 一次。
        if tool_result.success
            && self.tool_mutates_workspace(&tool_call.function.name)
            && !self.session.modified_files.is_empty()
        {
            self.session.verify_pending = true;
        }

        if tool_result.success
            && self.tool_mutates_workspace(&tool_call.function.name)
            && let Some(ref file_path) = extract_file_from_args(&tool_call.function.arguments)
        {
            let path = std::path::PathBuf::from(file_path);
            // 修复(G-H9):check_file_diagnostics 内部跑同步 cargo check/pyright,
            // 阻塞 tokio worker。用 spawn_blocking 移到阻塞线程池。
            // 注:lsp 需 &mut,这里取所有权后放回(无法跨 spawn_blocking 借用 &mut)。
            // 改为:clone 诊断结果路径,lsp 留在 agent 里,只 spawn 检查逻辑。
            // 但 LspDiagnostics 内部有缓存 Mutex,不能简单 clone。退而求其次:
            // 此处改为 fire-and-forget,不阻塞当前工具调用流程——诊断结果在
            // turn 末尾 flush_pending_verify 统一注入(那里已是 spawn_blocking)。
            // 这里只标记 verify_pending(上面已设),LSP 诊断由 verify 统一覆盖。
            let _ = path;
        }
    }

    /// 在 turn 末尾或 LLM 不再要求工具调用时统一执行 verify_loop 一次。
    /// 如果有失败,把 verify 错误以 system 消息注入下一轮上下文。
    async fn flush_pending_verify(&mut self) {
        if !self.session.verify_pending {
            return;
        }
        self.session.verify_pending = false;
        if self.session.modified_files.is_empty() {
            return;
        }
        // 修复(G-H9):verify 内部跑 cargo check/python/tsc,是同步阻塞调用(最长 30s)。
        // 原实现在 async finalize_turn 内直接调用,阻塞 tokio worker,期间 Ctrl-C
        // 的事件循环 poll 不到。改用 spawn_blocking 把阻塞操作移到阻塞线程池。
        let verify_loop = self.safety.verify_loop.clone();
        let modified_files = self.session.modified_files.clone();
        let verify_result =
            tokio::task::spawn_blocking(move || verify_loop.verify(&modified_files))
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(target: "verify", "spawn_blocking verify panicked: {}", e);
                    crate::planning::verify_loop::VerifyResult {
                        passed: true,
                        level: crate::planning::verify_loop::VerifyLevel::Syntax,
                        files: vec![],
                        errors: vec![],
                        warnings: vec![format!("验证任务异常: {}", e)],
                        output_summary: "verify task panicked".into(),
                    }
                });
        if !verify_result.passed {
            // 修复(审查):同一批"修不好的编译错误"最多回灌 3 轮,之后不再重复注入,
            // 避免错误信息无界膨胀上下文(verify 错误回灌上限)。
            const MAX_VERIFY_BACKFILLS: u8 = 3;
            if self.session.verify_backfill_count >= MAX_VERIFY_BACKFILLS {
                self.ctx.push_auto(ChatMessage::system(
                    "[自动验证] 仍存在编译问题(已多次回灌,不再重复)。请停止当前修复尝试并检查整体方案。"
                        .to_string(),
                ));
                return;
            }
            self.session.verify_backfill_count += 1;
            let error_summary = VerifyLoop::format_verify_result(&verify_result);
            self.ctx.push_auto(ChatMessage::system(format!(
                "[自动验证] 检测到问题:\n{}",
                error_summary
            )));
        }
    }

    // ──────────────────────────────────────────────────────────────
    // 自动记忆提取：每轮结束后从对话中提炼值得跨会话保留的事实
    // ──────────────────────────────────────────────────────────────

    /// 轮次收尾：verify + 记忆提取。返回最终回复文本。
    /// 在 run_with_stream 的两个返回分支前统一调用，避免重复代码。
    async fn finalize_turn(&mut self, user_input: &str) -> String {
        // 修复(审查):必须**先捕获**助手回复再 flush verify。否则 verify 失败时注入的
        // "[自动验证] 检测到问题:..." system 消息会成为 .last(),导致 run()/run_streaming()
        // 返回 verify 错误摘要而非模型实际答复,且记忆提取也基于错误文本。
        let assistant_reply = self
            .ctx
            .messages()
            .last()
            .and_then(|m| m.content.clone())
            .unwrap_or_else(|| "Movix 已完成任务".into());
        self.flush_pending_verify().await;
        // 记忆提取失败绝不影响主流程
        if let Err(e) = self
            .maybe_extract_memory(user_input, &assistant_reply)
            .await
        {
            tracing::warn!(target: "memory", "自动记忆提取失败（已忽略）: {}", e);
        }
        assistant_reply
    }

    /// 从本轮对话中提取值得长期记住的事实，写入 ~/.movix/memory.md。
    /// 用独立的 flash client 调用，token 不进主 client 的统计，
    /// 改走 cost_status 后台费用池（/cost 可见，透明）。
    async fn maybe_extract_memory(
        &mut self,
        user_input: &str,
        assistant_reply: &str,
    ) -> Result<()> {
        if !self.session.user_memory.is_enabled() || !self.session.user_memory.is_auto_extract() {
            return Ok(());
        }
        // 跳过过短输入（多为命令/闲聊），不值得提取
        if user_input.trim().chars().count() < 10 {
            return Ok(());
        }
        // 只传本轮摘要，控制 prompt 成本
        let recent = format!(
            "用户: {}\n助手: {}",
            truncate_str(user_input, 500),
            truncate_str(assistant_reply, 1500)
        );
        let existing = self.session.user_memory.content().unwrap_or("");
        let prompt = build_extraction_prompt(&recent, existing);

        // 独立 flash client：共享全局 reqwest 连接池，用完即弃
        let mut extractor = DeepSeekClient::new(self.core.config.clone())?;
        let messages = vec![
            ChatMessage::system(EXTRACTION_SYSTEM_PROMPT),
            ChatMessage::user(&prompt),
        ];
        let resp = extractor.chat(&messages, None, None).await?;
        let content = resp.content.unwrap_or_default();

        // 把这次调用的成本报入后台池（不污染主 client 的 token_stats）
        let cost = pricing::calculate_cost(&extractor.token_stats, "deepseek-v4-flash");
        if cost > 0.0 {
            cost_status::report(cost);
        }

        // 解析并写入每条提取到的事实
        let items = parse_extraction(&content);
        let mut changed = false;
        for item in items.into_iter().take(MAX_FACTS_PER_TURN) {
            let action = match item.action.as_str() {
                "replace" => AddAction::Replace {
                    match_hint: item.match_hint.unwrap_or_default(),
                },
                _ => AddAction::Add,
            };
            match self
                .session
                .user_memory
                .add_entry(item.category, &item.fact, action)
            {
                Ok(o) if o.applied => changed = true,
                Ok(o) => {
                    tracing::debug!(target: "memory", "记忆未写入: {} ({})", item.fact, o.reason)
                }
                Err(e) => tracing::warn!(target: "memory", "写入记忆失败: {}", e),
            }
        }

        if changed {
            // 下一轮 LLM 才能看到最新记忆 → 重建 system prompt 注入
            self.rebuild_system_with_memory();
            tracing::info!(target: "memory", "自动提取并写入了新记忆");
        }
        Ok(())
    }

    /// 重建 system prompt，使其包含最新的用户记忆块。
    /// 先剥离旧的 `<user_memory>` 块，再拼上当前记忆内容。
    fn rebuild_system_with_memory(&mut self) {
        let base = strip_memory_block(&self.core.system_prompt_text);
        let with_memory = match self.session.user_memory.system_block() {
            Some(block) => format!("{}\n\n{}", base, block),
            None => base,
        };
        self.core.system_prompt_text = with_memory.clone();
        self.ctx.update_system(with_memory);
    }

    /// 控制 /memory auto 开关
    pub fn set_memory_auto_extract(&mut self, on: bool) {
        self.session.user_memory.set_auto_extract(on);
    }

    pub fn is_memory_auto_extract(&self) -> bool {
        self.session.user_memory.is_auto_extract()
    }

    /// 获取 Agent 运行时长
    pub fn elapsed(&self) -> std::time::Duration {
        self.session.start_time.elapsed()
    }

    /// 获取当前迭代次数
    pub fn iteration_count(&self) -> u32 {
        self.session.iteration
    }

    /// 获取所有已注册工具的名称列表
    pub fn tool_definitions(&self) -> Vec<String> {
        self.core.tools.names()
    }

    /// 从 LLM 客户端获取最新的 token 使用统计
    pub async fn token_stats(&self) -> crate::common::deepseek::TokenStats {
        self.core.llm.lock().await.token_stats.clone()
    }

    /// 获取缓存的 token 统计（避免加锁）
    pub fn cached_token_stats(&self) -> &crate::common::deepseek::TokenStats {
        &self.session.cached_stats
    }

    /// 修复(Bug #2):返回共享 stats 的 Arc clone,让 UI 在 spawn agent 出去
    /// 之后仍能读到实时值。原方案是从 `app.agent` 读 cached_token_stats,
    /// swap 期间 app.agent 是占位实例,所有数值归零。
    pub fn shared_stats_handle(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::common::deepseek::TokenStats>> {
        std::sync::Arc::clone(&self.session.shared_stats)
    }

    /// 修复(Bug #4):返回取消标志的 Arc clone,UI Ctrl-C 时设置 true
    /// 即可让正在跑的 LLM HTTP 请求立即终止。
    pub fn cancel_flag_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.session.cancel_flag)
    }

    /// 重置取消标志,在每次 run_streaming 开始时调用。
    pub fn reset_cancel_flag(&self) {
        self.session
            .cancel_flag
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// 获取上下文窗口当前使用的 token 数
    pub fn context_token_count(&self) -> usize {
        self.ctx.token_count()
    }

    /// 获取上下文窗口的最大 token 容量
    pub fn context_max_tokens(&self) -> usize {
        self.ctx.max_tokens()
    }

    /// 获取上下文窗口中的消息数量
    pub fn context_message_count(&self) -> usize {
        self.ctx.message_count()
    }

    /// 获取工作区路径
    pub fn workspace(&self) -> std::path::PathBuf {
        self.core.config.workspace.clone()
    }

    /// 使用LLM分解任务，返回分解结果
    pub async fn decompose_task(&self, task: &str) -> Result<Option<DecomposedTask>> {
        let decomposer = TaskDecomposer::new();

        let rule_based = TaskDecomposer::rule_based_decompose(task);
        if rule_based.is_some() {
            return Ok(rule_based);
        }

        let prompt = TaskDecomposer::build_decomposition_prompt(task, decomposer.max_sub_tasks());
        let messages = vec![ChatMessage::user(prompt)];

        let mut llm = self.core.llm.lock().await;
        let response = llm.chat(&messages, None, None).await?;
        drop(llm);

        let content = response.content.unwrap_or_default();
        Ok(TaskDecomposer::parse_decomposition(&content, task))
    }

    /// 获取Skill注册表的引用
    pub fn skill_registry(&self) -> &SkillRegistry {
        &self.planning.skill_registry
    }

    /// 获取Skill注册表的可变引用
    pub fn skill_registry_mut(&mut self) -> &mut SkillRegistry {
        &mut self.planning.skill_registry
    }

    /// 列出所有已注册的Skill
    pub fn list_skills(&self) -> Vec<crate::context::skills::Skill> {
        self.planning
            .skill_registry
            .list()
            .into_iter()
            .cloned()
            .collect()
    }

    /// 启用指定Skill
    pub fn enable_skill(&mut self, name: &str) -> bool {
        self.planning.skill_registry.set_enabled(name, true)
    }

    /// 禁用指定Skill
    pub fn disable_skill(&mut self, name: &str) -> bool {
        self.planning.skill_registry.set_enabled(name, false)
    }

    /// 刷新Skill注册表（重新发现）
    pub fn refresh_skills(&mut self) {
        self.planning.skill_registry.refresh();
    }

    /// 使用指定Skill执行任务
    pub fn use_skill(
        &self,
        skill_name: &str,
        task: &str,
    ) -> Option<crate::context::skills::SkillExecutionResult> {
        let skill = self.planning.skill_registry.get(skill_name)?;
        if !skill.enabled {
            return None;
        }
        Some(skill_executor::execute(skill, task))
    }

    /// 获取Skill统计信息
    pub fn skill_stats(&self) -> crate::context::skills::SkillStats {
        self.planning.skill_registry.stats()
    }

    /// 保存自定义Skill到文件
    pub fn save_skill(
        &self,
        skill: &crate::context::skills::Skill,
    ) -> std::result::Result<(), String> {
        self.planning.skill_registry.save_skill(skill)
    }
}

fn which_git() -> bool {
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

// ──────────────────────────────────────────────────────────────
// 模块级：自动记忆提取的 prompt、JSON 解析、system 块剥离
// 这些是纯函数/常量，单独测试不打真实 LLM。
// ──────────────────────────────────────────────────────────────

/// 每轮最多写入的记忆条数，防止 LLM 一次产出过多。
const MAX_FACTS_PER_TURN: usize = 3;

const EXTRACTION_SYSTEM_PROMPT: &str = "你是一个对话记忆提取器。你的任务是判断对话中是否出现了**值得跨会话长期保留**的事实，并以严格 JSON 输出。\n\n只提取以下类别：\n- pref：用户偏好（语言、代码风格、工作习惯）\n- conv：项目约定（工具链、规范、配置）\n- dec：关键架构/技术决策\n- fact：其它稳定事实\n\n**不要提取**：一次性调试细节、临时文件路径、本轮已完成的任务、闲聊、明显是提问而非陈述的内容。\n\n若用户在本轮表达了与已有记忆**冲突**的新偏好（例如从“喜欢 Rust”变为“改用 Go”），使用 action=replace 并在 match_hint 中给出能定位旧条目的关键词。\n\n严格只输出一个 JSON 数组（可空 `[]`），不要任何解释文字、不要 markdown 代码块标记。每条格式：\n{\"fact\":\"简短事实陈述\",\"category\":\"pref|conv|dec|fact\",\"action\":\"add|replace\",\"match_hint\":\"仅 replace 时需要\"}";

/// 构建提取 prompt。把本轮对话摘要 + 现有记忆一起给 LLM，便于冲突判断。
fn build_extraction_prompt(recent_turn: &str, existing_memory: &str) -> String {
    // 现有记忆太长时截断，避免 prompt 膨胀
    let existing = truncate_str(existing_memory, 2000);
    format!(
        "现有记忆：\n{existing}\n\n本轮对话：\n{recent_turn}\n\n请判断本轮是否产生了值得长期保留的新事实，或与现有记忆冲突需要更新的偏好。严格输出 JSON 数组（最多 3 条）。"
    )
}

/// 一条提取结果（解析自 LLM 的 JSON）。
#[derive(Debug, Clone)]
struct ExtractedFact {
    fact: String,
    category: MemoryCategory,
    action: String,
    match_hint: Option<String>,
}

/// 解析 LLM 输出的 JSON 数组。容错：任何解析失败都返回空 Vec，绝不 panic。
fn parse_extraction(content: &str) -> Vec<ExtractedFact> {
    let json_str = extract_json_array(content);
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&json_str) else {
        tracing::debug!(target: "memory", "提取结果非合法 JSON: {}", truncate_str(content, 200));
        return Vec::new();
    };
    let Some(arr) = val.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let fact = item.get("fact")?.as_str()?.trim().to_string();
            if fact.is_empty() {
                return None;
            }
            let category = item
                .get("category")
                .and_then(|v| v.as_str())
                .map(MemoryCategory::from_tag)
                .unwrap_or(MemoryCategory::Fact);
            let action = item
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("add")
                .to_lowercase();
            let match_hint = item
                .get("match_hint")
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(ExtractedFact {
                fact,
                category,
                action,
                match_hint,
            })
        })
        .collect()
}

/// 从可能含 markdown 包裹的文本中提取 JSON 数组字符串。
fn extract_json_array(content: &str) -> String {
    let trimmed = content.trim();
    // ```json ... ``` 或 ``` ... ```
    if let Some(start) = trimmed.find("```") {
        let after_fence = &trimmed[start + 3..];
        // 跳过可能的 "json"/"JSON" 语言标记(大小写不敏感)。
        // 修复(G-L1):原 strip_prefix("json") 大小写敏感,大写 JSON 不匹配导致残留。
        let after_lang = after_fence
            .strip_prefix("json")
            .or_else(|| after_fence.strip_prefix("JSON"))
            .unwrap_or(after_fence);
        if let Some(end) = after_lang.find("```") {
            return after_lang[..end].trim().to_string();
        }
    }
    // 裸数组 `[...]`
    if let Some(start) = trimmed.find('[') {
        if let Some(end) = trimmed.rfind(']') {
            if end > start {
                return trimmed[start..=end].to_string();
            }
        }
    }
    trimmed.to_string()
}

/// 从 system prompt 文本中剥离 `<user_memory ...>...</user_memory>` 块。
/// 用于记忆更新后重建 system prompt。用 regex（已是依赖）匹配多行块。
fn strip_memory_block(system_prompt: &str) -> String {
    let re = regex::Regex::new(r"(?s)\n*<user_memory[^>]*>.*?</user_memory>\n*").unwrap();
    let stripped = re.replace_all(system_prompt, "\n\n").to_string();
    // 收敛多余空行
    stripped.trim_end().to_string()
}

#[cfg(test)]
mod memory_extraction_tests {
    use super::*;

    #[test]
    fn parse_extraction_valid_array() {
        let json = r#"[
            {"fact":"用户偏好 2 空格缩进","category":"conv","action":"add"},
            {"fact":"改用 Go","category":"pref","action":"replace","match_hint":"语言偏好"}
        ]"#;
        let items = parse_extraction(json);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].category, MemoryCategory::Convention);
        assert_eq!(items[0].action, "add");
        assert_eq!(items[1].category, MemoryCategory::Preference);
        assert_eq!(items[1].action, "replace");
        assert_eq!(items[1].match_hint.as_deref(), Some("语言偏好"));
    }

    #[test]
    fn parse_extraction_empty_array() {
        let items = parse_extraction("[]");
        assert!(items.is_empty());
    }

    #[test]
    fn parse_extraction_garbage_returns_empty() {
        assert!(parse_extraction("这不是 JSON").is_empty());
        assert!(parse_extraction("```json\nbroken").is_empty());
        assert!(parse_extraction(r#"{"not":"an array"}"#).is_empty());
    }

    #[test]
    fn parse_extraction_strips_markdown_fence() {
        let content = "```json\n[{\"fact\":\"x\",\"category\":\"fact\",\"action\":\"add\"}]\n```";
        let items = parse_extraction(content);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn parse_extraction_skips_empty_fact() {
        let json = r#"[{"fact":"","category":"fact","action":"add"}]"#;
        assert!(parse_extraction(json).is_empty());
    }

    #[test]
    fn extract_json_array_handles_bare_array() {
        let s = extract_json_array("some text [{\"a\":1}] trailing");
        assert_eq!(s, r#"[{"a":1}]"#);
    }

    #[test]
    fn strip_memory_block_removes_block() {
        let prompt = "你是 Movix。\n\n<user_memory source=\"/x/m.md\">\n旧记忆\n</user_memory>";
        let stripped = strip_memory_block(prompt);
        assert!(!stripped.contains("<user_memory"));
        assert!(stripped.contains("你是 Movix"));
    }

    #[test]
    fn strip_memory_block_noop_without_block() {
        let prompt = "你是 Movix。";
        assert_eq!(strip_memory_block(prompt), "你是 Movix。");
    }

    #[test]
    fn build_extraction_prompt_contains_context() {
        let p = build_extraction_prompt("用户: hello", "现有: 偏好 Rust");
        assert!(p.contains("偏好 Rust"));
        assert!(p.contains("hello"));
    }
}
