use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::common::deepseek::ToolDefinition;
use crate::common::error::Result;

pub mod execution;
pub mod file;
pub mod git;
pub mod sandbox;
pub mod search;
pub mod shell;
pub mod skill_tool;
pub mod web;

impl ToolResult {
    /// 成功结果
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            success: true,
            output: output.into(),
            error: None,
        }
    }

    /// 错误结果（不分配无用的空 output）
    pub fn err(msg: String) -> Self {
        Self {
            success: false,
            output: String::new(),
            error: Some(msg),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
}

/// 工具副作用的分类,供 ModePolicy / Pipeline / Snapshot 等模块消费。
/// 不再依赖工具名字符串去判断是否"会改东西"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EffectKind {
    /// 只读:不修改工作区、不派生子进程、不出网。
    ReadOnly,
    /// 写工作区文件(创建/修改/打补丁)。
    WorkspaceWrite,
    /// 派生子进程执行 shell 命令。
    Command,
    /// 出站网络请求(搜索、抓取)。
    Network,
    /// 副作用是上述多种的组合(如 use_skill 内部可能执行任意工具),
    /// 消费者应保守处理(走最严的策略)。
    Composite,
}

impl EffectKind {
    /// 是否会在工作区留下可观察变更。
    pub fn mutates_workspace(&self) -> bool {
        matches!(self, Self::WorkspaceWrite | Self::Command | Self::Composite)
    }
}

/// 风险等级,数值越大越危险。`Ord` 让 `max(a, b)` 在多工具并行时取最严的等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

impl RiskLevel {
    // max_with 已删除——调用方请直接使用 risk1.max(risk2)（RiskLevel derive 了 Ord）
}

/// 从 arguments 里"宽容地"提取可能包含路径的字段,容错处理缺失/非字符串。
/// 用于 `affected_paths` 的默认实现,工具可以更精确地覆盖。
pub fn extract_string_paths(args: &Value) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for key in ["path", "file_path", "cwd", "dir"] {
        if let Some(p) = args.get(key).and_then(|v| v.as_str())
            && !p.is_empty()
        {
            out.push(PathBuf::from(p));
        }
    }
    out
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    async fn execute(&self, arguments: &Value) -> Result<ToolResult>;

    // ---------- 声明性元数据 ----------
    // 全部带默认实现,工具可以选择性覆盖。消费者(ModePolicy / Pipeline /
    // Snapshot)应该只读这些方法,不再去字符串里猜。

    /// 副作用分类。默认 `ReadOnly`(最保守),写工具应显式声明。
    fn effect_kind(&self) -> EffectKind {
        EffectKind::ReadOnly
    }

    /// 风险等级。可以基于 `args` 判断(比如写到敏感路径时升级到 High)。
    /// 默认 `Low`,写工具 / shell 工具应覆盖。
    fn risk_level(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Low
    }

    /// 是否建议走人工审批。ModePolicy 会把这个 hint 与 mode 策略合成最终决策。
    /// 默认 `false`;mutating 工具应返回 `true`,Composite 工具也应返回 `true`。
    fn requires_approval_hint(&self, _args: &Value) -> bool {
        false
    }

    /// 此次调用会触及的工作区路径(相对或绝对)。用于快照范围、影响预览。
    /// 默认实现从 `path` / `file_path` / `cwd` / `dir` 字段抽取。
    fn affected_paths(&self, args: &Value) -> Vec<PathBuf> {
        extract_string_paths(args)
    }

    /// 是否可以与其他 `parallel_safe` 工具并发执行。
    /// 注意:并发安全 ≠ `effect_kind` 是 ReadOnly。
    /// 例如多个 read_file 并发安全;两个 write_file 写到同一文件就不安全。
    /// 默认 `false`(最保守),只读工具应覆盖为 `true`。
    fn parallel_safe(&self) -> bool {
        false
    }

    fn to_definition(&self) -> ToolDefinition {
        ToolDefinition::new(self.name(), self.description(), self.parameters())
    }
}

pub struct ToolRegistry {
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
}

/// 内置工具保留名集合。MCP 等外部工具不得占用这些名字,
/// 防止恶意/配置错误的 MCP server 遮蔽内置工具(如注册名为 read_file 的 MCP 工具
/// 让所有读取走外部服务,可能外发数据)。修复(High #H7)。
pub fn builtin_tool_names() -> &'static [&'static str] {
    &[
        "read_file",
        "write_file",
        "patch_file",
        "list_dir",
        "run_command",
        "search_code",
        "grep",
        "git_status",
        "git_diff",
        "git_log",
        "web_search",
        "web_fetch",
        "list_skills",
        "use_skill",
    ]
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
        }
    }

    // 修复(S14):RwLock 中毒(某线程持锁时 panic)时,原实现用 unwrap_or_else(into_inner)
    // 静默继续用脏数据。若 panic 发生在 retain/insert 中途,HashMap 可能半更新,
    // 后续 get() 可能返回指向已移除工具的 Arc(角色隔离失效)。改为:中毒时记录 error
    // 日志(至少让问题可观测),仍返回锁内数据(保持运行,但不再静默)。
    fn write_tools(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Arc<dyn Tool>>> {
        self.tools.write().unwrap_or_else(|e| {
            tracing::error!(
                target: "tools",
                "ToolRegistry RwLock 中毒(前一个持有者 panic),可能使用半更新的 HashMap。"
            );
            e.into_inner()
        })
    }

    fn read_tools(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Arc<dyn Tool>>> {
        self.tools.read().unwrap_or_else(|e| {
            tracing::error!(
                target: "tools",
                "ToolRegistry RwLock 中毒(前一个持有者 panic),可能读到半更新的 HashMap。"
            );
            e.into_inner()
        })
    }

    pub fn register<T: Tool + 'static>(&self, tool: T) -> &Self {
        let name = tool.name().to_string();
        let mut tools = self.write_tools();
        // 修复：原实现静默覆盖同名工具,可能让 MCP 工具意外遮蔽内置工具。
        // 现在重名时记录 warning 并保留旧条目。
        match tools.entry(name.clone()) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(Arc::new(tool));
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                tracing::warn!(target: "tools", "工具 '{}' 已存在, 跳过重复注册", name);
            }
        }
        drop(tools);
        self
    }

    /// 注册外部(MCP)工具。与 [`register`](Self::register) 不同:
    /// 若工具名与内置保留名冲突,**拒绝注册**(硬错误,返回 false)而非仅 warn。
    /// 修复(High #H7):避免恶意 MCP server 用 `read_file` 等名字遮蔽内置工具,
    /// 让所有调用走外部服务造成数据外泄。
    pub fn register_external<T: Tool + 'static>(&self, tool: T) -> bool {
        let name = tool.name().to_string();
        if builtin_tool_names().contains(&name.as_str()) {
            tracing::warn!(
                target: "tools",
                "外部工具 '{}' 与内置工具重名,已拒绝注册以防遮蔽",
                name
            );
            return false;
        }
        let mut tools = self.write_tools();
        if tools.contains_key(&name) {
            tracing::warn!(target: "tools", "外部工具 '{}' 已存在, 跳过重复注册", name);
            return false;
        }
        tools.insert(name, Arc::new(tool));
        drop(tools);
        true
    }

    /// 显式覆盖已注册的工具。仅用于测试或通过用户确认的工具覆盖。
    pub fn replace<T: Tool + 'static>(&self, tool: T) -> &Self {
        let name = tool.name().to_string();
        let mut tools = self.write_tools();
        tools.insert(name, Arc::new(tool));
        drop(tools);
        self
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.read_tools().get(name).cloned()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.read_tools()
            .values()
            .map(|t| t.to_definition())
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.read_tools().keys().cloned().collect()
    }

    /// 修复(Bug #7):仅保留白名单中的工具,用于 SubAgent 角色隔离。
    /// 删除任何不在 `allowed` 列表里的工具(MCP 桥工具也会被清掉,
    /// 因为它们对 readonly 角色不安全)。
    pub fn retain_only(&self, allowed: &[&str]) {
        let mut map = self.write_tools();
        map.retain(|name, _| allowed.iter().any(|a| a == name));
    }

    pub fn is_empty(&self) -> bool {
        self.read_tools().is_empty()
    }

    pub fn len(&self) -> usize {
        self.read_tools().len()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub fn create_default_registry(workspace: &str) -> ToolRegistry {
    let registry = ToolRegistry::new();
    registry
        .register(file::ReadFileTool::new(workspace))
        .register(file::WriteFileTool::new(workspace))
        .register(file::PatchFileTool::new(workspace))
        .register(file::ListDirTool::new(workspace))
        .register(shell::ShellTool::new(workspace))
        .register(search::SearchCodeTool::new(workspace))
        .register(search::GrepTool::new(workspace))
        .register(git::GitStatusTool::new(workspace))
        .register(git::GitDiffTool::new(workspace))
        .register(git::GitLogTool::new(workspace))
        .register(web::WebSearchTool)
        .register(web::WebFetchTool);
    registry
}

/// 把 Skill 相关工具(list_skills / use_skill)注册到已有的 registry。
///
/// 这两个工具需要 `SkillRegistry` 实例,而后者依赖 workspace 才能发现技能文件,
/// 因此不放在 [`create_default_registry`] 里(那会让 `movix tools` 命令、
/// 单元测试等无 SkillRegistry 的调用方被迫构造一个空 registry)。
///
/// Agent 主循环在初始化 SkillRegistry 之后调用本函数完成注册。
pub fn register_skill_tools(
    registry: &ToolRegistry,
    skill_registry: crate::context::skills::SkillRegistry,
) {
    registry.register(skill_tool::ListSkillsTool::new(skill_registry.clone()));
    registry.register(skill_tool::UseSkillTool::new(skill_registry));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_string_paths_picks_known_keys() {
        let args = json!({ "path": "src/main.rs", "other": 42 });
        let paths = extract_string_paths(&args);
        assert_eq!(paths, vec![PathBuf::from("src/main.rs")]);
    }

    #[test]
    fn extract_string_paths_collects_multiple_keys() {
        let args = json!({ "path": "a.rs", "cwd": "subdir", "file_path": "b.rs" });
        let mut paths = extract_string_paths(&args);
        paths.sort();
        let mut expected = vec![
            PathBuf::from("a.rs"),
            PathBuf::from("b.rs"),
            PathBuf::from("subdir"),
        ];
        expected.sort();
        assert_eq!(paths, expected);
    }

    #[test]
    fn extract_string_paths_skips_missing_and_empty() {
        let args = json!({ "path": "", "cwd": null });
        assert!(extract_string_paths(&args).is_empty());
    }

    #[test]
    fn risk_level_ord_works_for_aggregation() {
        // 验证 Ord 让"取最严"语义成立,后续 pipeline 聚合用得到。
        assert!(RiskLevel::High > RiskLevel::Medium);
        assert!(RiskLevel::Medium > RiskLevel::Low);
        assert_eq!(RiskLevel::Low.max(RiskLevel::High), RiskLevel::High);
    }

    #[test]
    fn effect_kind_marks_workspace_mutation() {
        assert!(!EffectKind::ReadOnly.mutates_workspace());
        assert!(EffectKind::WorkspaceWrite.mutates_workspace());
        assert!(EffectKind::Command.mutates_workspace());
        assert!(!EffectKind::Network.mutates_workspace());
        // Composite 保守视为会改东西。
        assert!(EffectKind::Composite.mutates_workspace());
    }

    /// 验证 trait 默认值正确:未实现 effect_kind 的工具,默认 ReadOnly 且 risk_level=Low。
    /// 一旦这个测试失败,说明 trait 默认值被误改,会污染所有未迁移的工具。
    struct DummyReadTool;
    #[async_trait]
    impl Tool for DummyReadTool {
        fn name(&self) -> &str {
            "dummy_read"
        }
        fn description(&self) -> &str {
            "for test"
        }
        fn parameters(&self) -> Value {
            json!({})
        }
        async fn execute(&self, _args: &Value) -> Result<ToolResult> {
            Ok(ToolResult::ok("ok"))
        }
    }

    #[tokio::test]
    async fn trait_defaults_are_safe() {
        let t = DummyReadTool;
        assert_eq!(t.effect_kind(), EffectKind::ReadOnly);
        assert_eq!(t.risk_level(&json!({})), RiskLevel::Low);
        assert!(!t.requires_approval_hint(&json!({})));
        assert!(!t.parallel_safe());
        // affected_paths 默认从 args 抽 path 字段
        assert_eq!(
            t.affected_paths(&json!({ "path": "x.rs" })),
            vec![PathBuf::from("x.rs")]
        );
    }
}
