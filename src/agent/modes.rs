use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::time::Instant;

use crate::tools::{EffectKind, RiskLevel, Tool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AppMode {
    Plan,
    #[default]
    Agent,
    Auto,
    Yolo,
}

impl AppMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Agent => "agent",
            Self::Auto => "auto",
            Self::Yolo => "yolo",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::Agent => "Agent",
            Self::Auto => "Auto",
            Self::Yolo => "YOLO",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Plan => "只读调查模式，不执行任何修改操作",
            Self::Agent => "交互式审批模式，修改操作需确认",
            Self::Auto => "智能审查模式，低风险自动执行，高风险需确认",
            Self::Yolo => "全自动执行模式，无需确认",
        }
    }

    pub fn allows_mutations(&self) -> bool {
        match self {
            Self::Plan => false,
            Self::Agent => true,
            Self::Auto => true,
            Self::Yolo => true,
        }
    }

    pub fn requires_approval(&self) -> bool {
        match self {
            Self::Plan => false,
            Self::Agent => true,
            Self::Auto => false,
            Self::Yolo => false,
        }
    }

    pub fn parse_mode(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "plan" | "p" => Some(Self::Plan),
            "agent" | "a" => Some(Self::Agent),
            "auto" | "au" => Some(Self::Auto),
            "yolo" | "y" => Some(Self::Yolo),
            _ => None,
        }
    }

    pub fn cycle(&self) -> Self {
        match self {
            Self::Plan => Self::Agent,
            Self::Agent => Self::Auto,
            Self::Auto => Self::Yolo,
            Self::Yolo => Self::Plan,
        }
    }
}

impl AppMode {
    fn to_u8(self) -> u8 {
        match self {
            Self::Plan => 0,
            Self::Agent => 1,
            Self::Auto => 2,
            Self::Yolo => 3,
        }
    }

    /// 把持久化的 u8 还原为枚举。
    /// 修复(P1.3):此前用 `v % 4` 把任意脏值"映射"到合法值,
    /// 等价于"未知值悄悄当成 Yolo",对一个会自动放行命令的危险模式
    /// 来说是降级到最危险默认值,违反 fail-safe。改为:
    ///   - 仅 0/1/2/3 接受
    ///   - 其它值视为损坏快照,降级到最安全的 Plan,并记一条 warn
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Plan,
            1 => Self::Agent,
            2 => Self::Auto,
            3 => Self::Yolo,
            other => {
                tracing::warn!(
                    target: "agent::mode",
                    "unknown AppMode value {} in persisted state, falling back to Plan",
                    other,
                );
                Self::Plan
            }
        }
    }
}

impl std::fmt::Display for AppMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

/// 工具名 → 副作用分类 的静态表。
/// 这是 modes.rs 唯一允许"按字符串名判断"的入口:新增工具时在这里登记一次即可。
/// P0 已把 `Tool::effect_kind()` 作为权威来源,本表必须与之保持一致;
/// 测试 [`table_matches_tool_metadata`] 会守门。
pub const EFFECT_KIND_TABLE: &[(&str, EffectKind)] = &[
    // 读工具 — 默认 ReadOnly
    ("read_file", EffectKind::ReadOnly),
    ("list_dir", EffectKind::ReadOnly),
    ("search_code", EffectKind::ReadOnly),
    ("grep", EffectKind::ReadOnly),
    ("git_status", EffectKind::ReadOnly),
    ("git_diff", EffectKind::ReadOnly),
    ("git_log", EffectKind::ReadOnly),
    ("list_skills", EffectKind::ReadOnly),
    // 写工具
    ("write_file", EffectKind::WorkspaceWrite),
    ("patch_file", EffectKind::WorkspaceWrite),
    ("run_command", EffectKind::Command),
    // 网络
    ("web_search", EffectKind::Network),
    ("web_fetch", EffectKind::Network),
    // 复合(skill 可能执行任意工具)
    ("use_skill", EffectKind::Composite),
];

/// 字符串名 → EffectKind。未知工具按最严处理(Composite)。
///
/// **注意(修复 High #H2)**:此函数**只查静态表**,不查 `ToolRegistry`。
/// 因此对未登记的 MCP 工具一律返回 `Composite`(fail-safe 但过严)。
/// 调用方若需要精确判断,应优先用 `ToolRegistry::get(name).effect_kind()`
/// (即 `MovixAgent::tool_mutates_workspace` 走的路径),而非本函数。
/// 本函数保留给无 registry 的场景(单元测试、CLI 启动早期)。
pub fn effect_kind_of(name: &str) -> EffectKind {
    EFFECT_KIND_TABLE
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, k)| *k)
        .unwrap_or(EffectKind::Composite)
}

/// 是否是"会改东西"的工具。
/// 内部走 [`effect_kind_of`] + [`EffectKind::mutates_workspace`],不再有硬编码白名单。
pub fn is_mutating_tool(tool_name: &str) -> bool {
    effect_kind_of(tool_name).mutates_workspace()
}

/// Auto 模式下需要审批的高风险操作关键词。
/// 注意：这些关键词在 `run_command` 的 detail 中做 `contains` 匹配，
/// 所以需要避免过于宽泛的词（如 "force" 会误匹配 "echo force"）。
/// 优先用带空格/前缀的精确模式（如 "rm " 而非 "rm"）。
pub const HIGH_RISK_PATTERNS: &[&str] = &[
    "delete",
    "rm ",
    "rmdir",
    "truncate",
    "shutdown",
    "reboot",
    "chmod 777",
    "chown",
    "sudo ",
    "sudo\t",
    "purge",
    "wipe",
    "destroy",
    "mkfs.",
    "dd if=",
    "> /dev/",
    "format ", // "format" 作为命令动词时通常后跟空格，避免匹配 "reformat" 等
    "reset --hard",
    "push --force",
    "push -f",
];

/// Auto 模式下需要审批的文件路径模式（已预转为小写，避免运行时 to_lowercase）
pub const HIGH_RISK_PATHS: &[&str] = &[
    "/etc/",
    "/usr/bin/",
    "/usr/sbin/",
    "cargo.toml",
    "package.json",
    ".env",
    "docker-compose",
    "dockerfile",
    ".git/config",
    "makefile",
    ".bashrc",
    ".zshrc",
    ".profile",
];

/// 判断 Auto 模式下某个操作是否为高风险,需要审批确认。
///
/// `risk_level` 由 [`Tool::risk_level`] 提供(已知元数据),
/// `detail` 是命令/路径字符串(用于关键字匹配,因为部分工具的"危险"取决于参数内容)。
pub fn is_high_risk_action(tool_name: &str, detail: &str, risk_level: RiskLevel) -> bool {
    // 1. 结构化判断:任何 tool 自我声明为 High,直接高风险
    if risk_level >= RiskLevel::High {
        return true;
    }

    let detail_lower;
    let detail_lower =
        if tool_name == "run_command" || tool_name == "write_file" || tool_name == "patch_file" {
            detail_lower = detail.to_lowercase();
            &detail_lower
        } else {
            return false;
        };

    if tool_name == "run_command" {
        if HIGH_RISK_PATTERNS.iter().any(|p| detail_lower.contains(p)) {
            return true;
        }
        if detail_lower.contains("install")
            || detail_lower.contains("apt ")
            || detail_lower.contains("yum ")
            || detail_lower.contains("brew ")
        {
            return true;
        }
        if detail_lower.contains("curl")
            && (detail_lower.contains("| sh") || detail_lower.contains("| bash"))
        {
            return true;
        }
        // git push --force / push -f / reset --hard 已在 HIGH_RISK_PATTERNS 中
    }

    if tool_name == "write_file" || tool_name == "patch_file" {
        // 修复(Medium #M11):原用 `detail_lower.contains(p)`,`.env` 会误匹配
        // `.envrc`/`config.env.bak`,`cargo.toml` 会误匹配 `my_cargo.toml`。
        // 改为路径分量精确匹配:把 detail 当路径,对每个分量(component)与
        // 敏感文件名精确比较;对目录前缀(如 `/etc/`)仍用前缀匹配。
        if matches_high_risk_path(detail_lower) {
            return true;
        }
    }

    false
}

/// 判断路径是否命中高风险路径模式。
///
/// `HIGH_RISK_PATHS` 中,以 `/` 结尾或含 `/` 的视为目录前缀(前缀匹配),
/// 其余视为文件名(必须与某个路径分量精确相等)。
fn matches_high_risk_path(detail_lower: &str) -> bool {
    use std::path::Path;
    let path = Path::new(detail_lower);
    let components: Vec<String> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(|s| s.to_lowercase()))
        .collect();
    for pattern in HIGH_RISK_PATHS {
        if pattern.ends_with('/') || pattern.contains('/') {
            // 目录前缀模式:detail 以该前缀开头
            if detail_lower.starts_with(pattern) {
                return true;
            }
        } else if components.iter().any(|c| c == pattern) {
            return true;
        }
    }
    false
}

/// 适配器：将 is_mutating_tool 转为 LoopGuard 所需的 IsMutating 签名
pub fn is_mutating_tool_call(call: &crate::common::deepseek::ToolCall) -> bool {
    is_mutating_tool(&call.function.name)
}

#[derive(Debug, Clone)]
pub struct ModeConfig {
    mode_shared: Arc<AtomicU8>,
    pub show_plan_summary: bool,
    pub max_auto_writes: u32,
    /// 已自动执行的写工具次数。原子变量,支持并行 dispatch 下的 check-and-claim 语义。
    /// 之前是 `pub u32`,在并行 dispatch 多个 mutating 工具时存在 TOCTOU
    /// 漏洞 —— 两个工具可能同时通过预算检查再各自 +1,导致预算被超额扣减。
    /// 详见 `try_claim_auto_write` 的实现。
    auto_write_count: Arc<AtomicU32>,
    /// 上次预算重置时间,用于防止短时间内的频繁重置
    last_reset_at: Instant,
    /// 两次重置之间的最小间隔(毫秒),默认 3000ms
    pub min_reset_interval_ms: u64,
}

impl ModeConfig {
    pub fn new(mode: AppMode) -> Self {
        Self {
            mode_shared: Arc::new(AtomicU8::new(mode.to_u8())),
            show_plan_summary: mode == AppMode::Plan,
            max_auto_writes: match mode {
                // Yolo:无限制
                AppMode::Yolo => u32::MAX,
                // Auto:给一个合理的默认预算(智能审查模式应该能工作,只是受限)。
                // P4 真正接通预算后,超过 N 次会被 Blocked,迫使 agent 走更严策略或让用户介入。
                AppMode::Auto => 5,
                // Plan/Agent:不依赖此字段(Plan blocked,Agent 走 approval)
                AppMode::Plan | AppMode::Agent => 0,
            },
            auto_write_count: Arc::new(AtomicU32::new(0)),
            last_reset_at: Instant::now(),
            min_reset_interval_ms: 3_000,
        }
    }

    /// 获取当前运行模式（从共享状态读取最新值）
    pub fn mode(&self) -> AppMode {
        AppMode::from_u8(self.mode_shared.load(Ordering::Relaxed))
    }

    /// 设置运行模式（写入共享状态，运行中的 agent 也能感知）
    pub fn set_mode(&self, mode: AppMode) {
        self.mode_shared.store(mode.to_u8(), Ordering::Relaxed);
    }

    pub fn should_execute(&self, tool_name: &str) -> ModeDecision {
        self.should_execute_with_detail(tool_name, "")
    }

    /// 检查工具调用权限,支持 Auto 模式根据操作详情判断风险等级。
    /// `risk_level` 未知时传 [`RiskLevel::Medium`] 兜底(走关键字匹配路径)。
    pub fn should_execute_with_detail(&self, tool_name: &str, detail: &str) -> ModeDecision {
        self.should_execute_with_risk(tool_name, detail, RiskLevel::Medium)
    }

    /// 真正消费 [`Tool::risk_level`] 元数据的决策入口。
    /// P3 pipeline.rs 应当走这个接口,把工具实例 + args 传进来。
    pub fn should_execute_with_risk(
        &self,
        tool_name: &str,
        detail: &str,
        risk_level: RiskLevel,
    ) -> ModeDecision {
        let mode = self.mode();
        // Plan 模式下不允许网络操作,必须在 mutating 检查之前阻断
        if mode == AppMode::Plan && matches!(effect_kind_of(tool_name), EffectKind::Network) {
            return ModeDecision::Blocked(format!("Plan 模式下不允许网络操作: {}", tool_name));
        }
        // 修复(审查):Agent 模式此前只审批 mutating 工具,Network 工具(web_search/
        // web_fetch)的 mutates_workspace()==false → 直接 Proceed,可经"read_file 读
        // .env → web_fetch 发到攻击者 URL"形成零审批外泄链。网络出站有副作用(SSRF/
        // 数据外泄/内网访问),Agent 模式下同样需要审批。非交互模式(-t)无审批通道时,
        // 由 authorize_tool_call 的 H6 分支(tool_is_pure_readonly==false)拒绝放行。
        if mode == AppMode::Agent && matches!(effect_kind_of(tool_name), EffectKind::Network) {
            return ModeDecision::NeedsApproval;
        }
        if !mode.allows_mutations() && is_mutating_tool(tool_name) {
            return ModeDecision::Blocked(format!("Plan 模式下不允许执行修改操作: {}", tool_name));
        }

        if mode.requires_approval() && is_mutating_tool(tool_name) {
            return ModeDecision::NeedsApproval;
        }

        if mode == AppMode::Auto
            && is_mutating_tool(tool_name)
            && is_high_risk_action(tool_name, detail, risk_level)
        {
            return ModeDecision::NeedsApproval;
        }

        // P4:Auto 模式下,如果 mutating 工具的自执行预算耗尽,直接 Blocked。
        // Plan/Agent 模式不依赖此字段(Plan 走 allows_mutations 路径,Agent 走 approval 路径)。
        if mode == AppMode::Auto && is_mutating_tool(tool_name) && self.auto_write_exhausted() {
            return ModeDecision::Blocked(format!(
                "Auto 模式自动写入预算已耗尽 ({}/{}).请切换到 Agent 模式手动确认。",
                self.auto_write_count.load(Ordering::Relaxed),
                self.max_auto_writes
            ));
        }

        ModeDecision::Proceed
    }

    /// 一步到位:直接消费 `Tool` 实例 + args,内部算出 `risk_level`。
    /// P3 应当把 [self.should_execute_with_detail] 的所有调用换成这个。
    pub fn decide_for_tool(&self, tool: &dyn Tool, args: &serde_json::Value) -> ModeDecision {
        let risk_level = tool.risk_level(args);
        let detail = if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
            p.to_string()
        } else if let Some(c) = args.get("command").and_then(|v| v.as_str()) {
            c.to_string()
        } else {
            String::new()
        };
        self.should_execute_with_risk(tool.name(), &detail, risk_level)
    }

    /// 旧的非原子记账接口,保留以兼容已有调用点。新代码请用
    /// [`Self::try_claim_auto_write`] 做"检查 + 占用"原子操作。
    pub fn record_auto_execution(&self) {
        // saturating + 不超过 max:即便外部循环漏检,也不会让 count 越界。
        let _ = self
            .auto_write_count
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
                Some(c.saturating_add(1).min(self.max_auto_writes))
            });
    }

    /// 原子地"占用一次自动写预算"。返回 true 表示成功占用,false 表示预算已用尽。
    /// 修复:之前 `auto_write_exhausted()` 检查与 `record_auto_execution()` 累加是分开的,
    /// 并行 dispatch 多个 mutating 工具时存在 TOCTOU —— 两个 task 都读到 `count < max`
    /// 然后各自 +1,导致预算被超额扣减。改为基于 `compare_exchange` 的 CAS 循环,
    /// 保证"看到 < max 才会 +1"是不可分割的。
    pub fn try_claim_auto_write(&self) -> bool {
        let mut current = self.auto_write_count.load(Ordering::Relaxed);
        loop {
            if current >= self.max_auto_writes {
                return false;
            }
            match self.auto_write_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    pub fn reset_turn(&mut self) {
        // 修复(R5/H6):原有 3s 冷却(min_reset_interval_ms)会在快速连续交互时拒绝重置
        // auto_write_count。Auto 模式默认预算 5,第一轮耗尽后,3s 内的下一轮 reset_turn
        // no-op → 预算永久为 0 → 所有写工具 Blocked,agent 变只读,用户无任何提示。
        // 冷却的本意(防单轮内 write-storm)已由 LoopGuard::reset_storm 覆盖(同在
        // reset_turn_failures 调用),无需在 turn 边界再设壁钟冷却。改为无条件重置。
        self.last_reset_at = Instant::now();
        self.auto_write_count.store(0, Ordering::Relaxed);
    }

    /// 当前 Auto 模式的"自动写入"预算是否已用完。
    /// Yolo 模式 `max_auto_writes = u32::MAX`,永不为 true。
    /// 其他模式 `max_auto_writes = 0`,只要有一次 write 就耗尽。
    /// 用户可调 [`ModeConfig::set_max_auto_writes`] 改预算。
    pub fn auto_write_exhausted(&self) -> bool {
        self.auto_write_count.load(Ordering::Relaxed) >= self.max_auto_writes
    }

    /// 调整 Auto 模式可自动写入的次数上限。P3 起 UI 应暴露此开关。
    pub fn set_max_auto_writes(&mut self, n: u32) {
        self.max_auto_writes = n;
    }

    /// 仅供观察:当前还剩多少次"自动写入"配额。
    pub fn auto_writes_remaining(&self) -> u32 {
        self.max_auto_writes
            .saturating_sub(self.auto_write_count.load(Ordering::Relaxed))
    }

    /// 调试/UI 用:返回当前 auto_write 计数器的快照值。
    pub fn auto_write_count(&self) -> u32 {
        self.auto_write_count.load(Ordering::Relaxed)
    }
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self::new(AppMode::default())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeDecision {
    Proceed,
    NeedsApproval,
    Blocked(String),
}

/// Agent 模式审批决策，由用户通过 TUI 输入
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approved,
    Denied,
    /// 修改参数后批准
    Modified(ApprovalModification),
    /// 要求 Agent 解释
    Explain,
}

/// 参数修改内容（统一版本，供 modes::ApprovalDecision 和 partial_approval 共用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalModification {
    /// 修改后的工具参数（完整 JSON）
    pub modified_arguments: String,
    /// 修改原因
    pub reason: String,
    /// 修改的字段列表
    pub changed_fields: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_cycle() {
        assert_eq!(AppMode::Plan.cycle(), AppMode::Agent);
        assert_eq!(AppMode::Agent.cycle(), AppMode::Auto);
        assert_eq!(AppMode::Auto.cycle(), AppMode::Yolo);
        assert_eq!(AppMode::Yolo.cycle(), AppMode::Plan);
    }

    #[test]
    fn test_plan_blocks_mutations() {
        let config = ModeConfig::new(AppMode::Plan);
        assert!(matches!(
            config.should_execute("write_file"),
            ModeDecision::Blocked(_)
        ));
        assert!(matches!(
            config.should_execute("read_file"),
            ModeDecision::Proceed
        ));
    }

    #[test]
    fn test_agent_needs_approval() {
        let config = ModeConfig::new(AppMode::Agent);
        assert!(matches!(
            config.should_execute("write_file"),
            ModeDecision::NeedsApproval
        ));
        assert!(matches!(
            config.should_execute("read_file"),
            ModeDecision::Proceed
        ));
    }

    #[test]
    fn test_yolo_auto_approve() {
        let config = ModeConfig::new(AppMode::Yolo);
        assert!(matches!(
            config.should_execute("write_file"),
            ModeDecision::Proceed
        ));
    }

    #[test]
    fn test_from_str() {
        assert_eq!(AppMode::parse_mode("plan"), Some(AppMode::Plan));
        assert_eq!(AppMode::parse_mode("p"), Some(AppMode::Plan));
        assert_eq!(AppMode::parse_mode("auto"), Some(AppMode::Auto));
        assert_eq!(AppMode::parse_mode("au"), Some(AppMode::Auto));
        assert_eq!(AppMode::parse_mode("yolo"), Some(AppMode::Yolo));
        assert_eq!(AppMode::parse_mode("unknown"), None);
    }

    #[test]
    fn test_auto_low_risk_proceeds() {
        // P4 后:Auto 模式默认 max_auto_writes=0,所以"普通写"默认被 Blocked(预算耗尽)。
        // 调大预算后,低风险才 Proceed。
        let mut config = ModeConfig::new(AppMode::Auto);
        config.set_max_auto_writes(5);
        assert!(matches!(
            config.should_execute_with_detail("write_file", "src/main.rs"),
            ModeDecision::Proceed
        ));
        assert!(matches!(
            config.should_execute_with_detail("read_file", "src/main.rs"),
            ModeDecision::Proceed
        ));
    }

    #[test]
    fn test_auto_default_budget_blocks_writes() {
        // P4:把预算调到 0 后,Auto 模式普通写 Blocked(预算耗尽)。
        // 默认是 5,这里验证"显式 0"会让 budget 立即耗尽。
        let mut config = ModeConfig::new(AppMode::Auto);
        config.set_max_auto_writes(0);
        match config.should_execute_with_detail("write_file", "src/main.rs") {
            ModeDecision::Blocked(msg) => assert!(msg.contains("预算")),
            other => panic!("expected Blocked, got {:?}", other),
        }
    }

    #[test]
    fn test_auto_high_risk_needs_approval() {
        let config = ModeConfig::new(AppMode::Auto);
        assert!(matches!(
            config.should_execute_with_detail("run_command", "rm -rf /tmp/test"),
            ModeDecision::NeedsApproval
        ));
        assert!(matches!(
            config.should_execute_with_detail("write_file", "Cargo.toml"),
            ModeDecision::NeedsApproval
        ));
    }

    // ---------- P1:元数据驱动的 ModeConfig 决策 ----------

    #[test]
    fn effect_kind_of_returns_composite_for_unknown_tool() {
        // 未知工具按最严处理:Composite(避免漏判)
        assert_eq!(effect_kind_of("definitely_not_real"), EffectKind::Composite);
    }

    #[test]
    fn is_mutating_now_uses_effect_kind_table() {
        // P1 之前:硬编码白名单。
        // P1 之后:查表 + mutates_workspace()。新工具只要在 EFFECT_KIND_TABLE 加一行即可。
        assert!(is_mutating_tool("write_file"));
        assert!(is_mutating_tool("patch_file"));
        assert!(is_mutating_tool("run_command"));
        // 读工具不再"会改东西"
        assert!(!is_mutating_tool("read_file"));
        assert!(!is_mutating_tool("git_status"));
        // 网络不算 mutating
        assert!(!is_mutating_tool("web_search"));
        // use_skill 是 Composite,会被识别为 mutating(因为 Composite.mutates_workspace() == true)
        assert!(is_mutating_tool("use_skill"));
    }

    #[test]
    fn is_high_risk_action_accepts_risk_level() {
        // 工具自己声明 High → 无视 detail 直接高风险
        assert!(is_high_risk_action(
            "write_file",
            "src/main.rs",
            RiskLevel::High
        ));
        // Low 兜底:普通路径不触发
        assert!(!is_high_risk_action(
            "write_file",
            "src/main.rs",
            RiskLevel::Low
        ));
        // Medium 兜底 + 关键字匹配:Cargo.toml 仍然高风险
        assert!(is_high_risk_action(
            "write_file",
            "Cargo.toml",
            RiskLevel::Medium
        ));
    }

    use crate::tools::{Tool, ToolResult};
    /// 用一个 Dummy 工具验证 `decide_for_tool` 走的是元数据路径。
    /// 一旦 Tool trait 默认值被改坏,这个测试会立刻挂。
    use async_trait::async_trait;
    use serde_json::json;

    struct DummyMediumTool;
    #[async_trait]
    impl Tool for DummyMediumTool {
        fn name(&self) -> &str {
            "write_file"
        }
        fn description(&self) -> &str {
            "test stub"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({})
        }
        async fn execute(
            &self,
            _args: &serde_json::Value,
        ) -> std::result::Result<ToolResult, crate::common::error::MovixError> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
        fn risk_level(&self, _args: &serde_json::Value) -> RiskLevel {
            RiskLevel::High
        }
    }

    #[test]
    fn decide_for_tool_consumes_risk_level() {
        // Auto 模式 + High 风险 + 写入 src/main.rs → 走 NeedsApproval
        let config = ModeConfig::new(AppMode::Auto);
        let tool = DummyMediumTool;
        let args = json!({ "path": "src/main.rs" });
        assert!(matches!(
            config.decide_for_tool(&tool, &args),
            ModeDecision::NeedsApproval
        ));
    }

    // ---------- P4:Auto 模式自动写入预算 ----------

    #[test]
    fn yolo_budget_is_unlimited() {
        let config = ModeConfig::new(AppMode::Yolo);
        assert_eq!(config.max_auto_writes, u32::MAX);
        for _ in 0..1000 {
            config.record_auto_execution();
        }
        assert!(!config.auto_write_exhausted());
    }

    #[test]
    fn non_yolo_default_budgets() {
        // Plan/Agent 默认 0(不依赖此字段)。
        assert_eq!(ModeConfig::new(AppMode::Plan).max_auto_writes, 0);
        assert_eq!(ModeConfig::new(AppMode::Agent).max_auto_writes, 0);
        // Auto 默认 5(给一个合理的 UX 起点,用户可调)。
        assert_eq!(ModeConfig::new(AppMode::Auto).max_auto_writes, 5);
    }

    #[test]
    fn auto_budget_blocks_after_n_writes() {
        let mut config = ModeConfig::new(AppMode::Auto);
        config.set_max_auto_writes(3);

        // 前 3 次都没耗尽(写完才算)
        for _ in 0..3 {
            assert!(!config.auto_write_exhausted());
            config.record_auto_execution();
        }
        // 第 4 次:耗尽
        assert!(config.auto_write_exhausted());
        assert_eq!(config.auto_writes_remaining(), 0);
    }

    #[test]
    fn reset_turn_restores_budget() {
        let mut config = ModeConfig::new(AppMode::Auto);
        config.set_max_auto_writes(2);
        config.record_auto_execution();
        config.record_auto_execution();
        assert!(config.auto_write_exhausted());

        // 移除冷却期以立即测试重置逻辑
        config.min_reset_interval_ms = 0;
        config.reset_turn();
        assert!(!config.auto_write_exhausted());
        assert_eq!(config.auto_writes_remaining(), 2);
    }

    #[test]
    fn auto_budget_only_applies_to_mutating_tools() {
        // 读工具不被 budget 拦截
        let mut config = ModeConfig::new(AppMode::Auto);
        config.set_max_auto_writes(0);
        assert!(config.auto_write_exhausted());
        // 但 read_file 仍应 Proceed(因为 is_mutating_tool("read_file") == false)
        assert!(matches!(
            config.should_execute_with_risk("read_file", "", RiskLevel::Low),
            ModeDecision::Proceed
        ));
    }

    #[test]
    fn auto_budget_decision_returns_blocked() {
        let mut config = ModeConfig::new(AppMode::Auto);
        config.set_max_auto_writes(1);
        config.record_auto_execution();
        // 已用完 → 写工具应 Blocked
        match config.should_execute_with_risk("write_file", "src/main.rs", RiskLevel::Medium) {
            ModeDecision::Blocked(msg) => assert!(msg.contains("Auto 模式自动写入预算")),
            other => panic!("expected Blocked, got {:?}", other),
        }
    }

    /// 修复(P1.3):未知持久化 mode 值应 fail-safe 到 Plan,而不是 Yolo。
    #[test]
    fn from_u8_unknown_falls_back_to_plan() {
        // 已知合法值
        assert_eq!(AppMode::from_u8(0), AppMode::Plan);
        assert_eq!(AppMode::from_u8(1), AppMode::Agent);
        assert_eq!(AppMode::from_u8(2), AppMode::Auto);
        assert_eq!(AppMode::from_u8(3), AppMode::Yolo);
        // 任意未知值都应回到 Plan(以前会被 `% 4` 映射成 Yolo / Auto / 等等)
        for bad in [4u8, 5, 7, 99, 200, 255] {
            assert_eq!(
                AppMode::from_u8(bad),
                AppMode::Plan,
                "AppMode::from_u8({}) 必须 fail-safe 回 Plan",
                bad,
            );
        }
    }

    /// 修复(P4.4):验证 EFFECT_KIND_TABLE 与 ToolRegistry 中已注册工具的
    /// effect_kind() 返回值一致。如果新增工具忘记在表中添加条目,
    /// 或两处声明不匹配,此测试会失败。
    ///
    /// 注:list_skills / use_skill 是"按需注册"的工具(需要 SkillRegistry),
    /// 不在 create_default_registry 中。这里显式注册它们,使测试反映真实
    /// agent 初始化后的工具集。
    #[test]
    fn table_matches_tool_metadata() {
        use crate::context::skills::SkillRegistry;
        use crate::tools::{create_default_registry, register_skill_tools};

        let registry = create_default_registry(".");
        // 注册 skill 工具,与 agent::MovixAgent::with_system_prompt_and_tools 保持一致
        let skill_registry = SkillRegistry::with_workspace(std::path::PathBuf::from("."));
        register_skill_tools(&registry, skill_registry);

        for (name, expected_kind) in EFFECT_KIND_TABLE {
            let actual = registry
                .get(name)
                .map(|t| t.effect_kind())
                .unwrap_or(EffectKind::Composite);
            assert_eq!(
                actual, *expected_kind,
                "EFFECT_KIND_TABLE says '{}' is {:?}, but Tool::effect_kind() returns {:?}",
                name, expected_kind, actual
            );
        }
    }
}
