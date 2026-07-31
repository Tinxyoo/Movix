pub mod arg_repair;
pub mod collab_watcher;
pub mod config;
pub mod cost_status;
pub mod deepseek;
pub mod error;
pub mod failure_tracker;
pub mod i18n;
pub mod loop_guard;
pub mod lsp;
pub mod markdown;
pub mod pricing;
pub mod project_index;
pub mod reviewer;
pub mod scavenger;
pub mod session;
pub mod skill_executor;
pub mod utils;

// Re-export key types for backward compatibility
pub use self::arg_repair::{ArgRepairError, RepairReport, repair, repair_with_report};
// 修复(P4.3):Scavenger 拆分为独立模块,从 scavenger 导出而非 arg_repair。
pub use self::collab_watcher::{ChangeType, CollaborationWatcher, FileChange};
pub use self::config::MovixConfig;
pub use self::deepseek::{
    ChatMessage, DeepSeekClient, FunctionCall, FunctionDef, LlmResponse, StreamEvent, StreamResult,
    TokenStats, ToolCall, ToolDefinition,
};
pub use self::error::{MovixError, Result};
pub use self::failure_tracker::{FailureRecord, FailureSignal, FailureTracker};
pub use self::i18n::{Lang, Strings};
pub use self::loop_guard::{AttemptDecision, IsMutating, LoopGuard};
pub use self::lsp::{Diagnostic, DiagnosticSeverity, LspDiagnostics, LspKind};
pub use self::pricing::{calculate_cost, cost_badge, format_cost};
pub use self::project_index::{FileCategory, FileEntry, ProjectIndex, ProjectIndexer, ProjectType};
pub use self::reviewer::{
    ReviewConfig, ReviewDimension, ReviewFinding, ReviewResult, ReviewSeverity, Reviewer,
};
pub use self::scavenger::{ScavengeSource, Scavenger, ToolCallCandidate};
pub use self::session::{SessionPersistence, SessionSnapshot};
pub use self::skill_executor::{
    build_enhanced_prompt, execute, execute_prompt_skill, execute_template_skill,
    execute_workflow_skill, format_skill_list, match_and_execute,
};
