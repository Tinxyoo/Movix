use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_SKILL_DEPTH: usize = 8;
const MAX_SKILL_SIZE: usize = 50 * 1024;

/// Skill类型，决定执行方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SkillType {
    /// 提示型Skill，注入到system prompt中增强Agent能力
    Prompt,
    /// 工具型Skill，注册为LLM可调用的工具
    Tool,
    /// 工作流型Skill，定义多步骤执行流程
    Workflow,
    /// 模板型Skill，提供代码/文档模板
    Template,
}

impl SkillType {
    /// 获取Skill类型的显示名称
    pub fn display_name(&self) -> &'static str {
        match self {
            SkillType::Prompt => "Prompt",
            SkillType::Tool => "Tool",
            SkillType::Workflow => "Workflow",
            SkillType::Template => "Template",
        }
    }

    /// 获取Skill类型的图标
    pub fn icon(&self) -> &'static str {
        match self {
            SkillType::Prompt => "💡",
            SkillType::Tool => "🔧",
            SkillType::Workflow => "🔄",
            SkillType::Template => "📝",
        }
    }
}

/// Skill定义，包含元数据和内容
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Skill唯一标识名
    pub name: String,
    /// 简短描述
    pub description: String,
    /// Skill类型
    pub skill_type: SkillType,
    /// Skill内容（提示文本/模板/工作流定义）
    pub content: String,
    /// 来源文件路径
    pub source: PathBuf,
    /// 触发关键词
    pub trigger: Option<String>,
    /// Skill版本
    pub version: String,
    /// 标签列表，用于分类和搜索
    pub tags: Vec<String>,
    /// 输入参数定义（JSON Schema格式）
    pub input_schema: Option<serde_json::Value>,
    /// 是否为内置Skill
    pub builtin: bool,
    /// 是否启用
    pub enabled: bool,
}

/// Skill匹配结果
#[derive(Debug, Clone)]
pub struct SkillMatch {
    /// 匹配到的Skill
    pub skill: Skill,
    /// 匹配置信度 (0.0-1.0)
    pub confidence: f64,
}

/// Skill执行结果
#[derive(Debug, Clone)]
pub struct SkillExecutionResult {
    /// 执行是否成功
    pub success: bool,
    /// 执行输出
    pub output: String,
    /// 错误信息
    pub error: Option<String>,
    /// 使用的Skill名称
    pub skill_name: String,
    /// 执行耗时（毫秒）
    pub elapsed_ms: u64,
}

/// Skill注册表，管理所有Skill的发现、注册、查询和匹配
#[derive(Clone)]
pub struct SkillRegistry {
    /// 已注册的Skill集合
    skills: HashMap<String, Skill>,
    /// Skill搜索路径
    search_paths: Vec<PathBuf>,
    /// 工作区路径
    workspace: PathBuf,
}

impl SkillRegistry {
    /// 创建新的Skill注册表，自动配置搜索路径
    pub fn new() -> Self {
        let workspace = std::env::current_dir().unwrap_or_default();
        Self::with_workspace(workspace)
    }

    /// 使用指定工作区和搜索路径创建注册表
    pub fn with_workspace(workspace: PathBuf) -> Self {
        let mut search_paths = Vec::new();

        search_paths.push(workspace.join(".movix").join("skills"));
        search_paths.push(workspace.join(".claude").join("skills"));

        let home = crate::common::utils::home_dir();
        search_paths.push(home.join(".movix").join("skills"));
        search_paths.push(home.join(".claude").join("skills"));

        Self {
            skills: HashMap::new(),
            search_paths,
            workspace,
        }
    }

    /// 从所有搜索路径发现Skill
    pub fn discover(&mut self) {
        let paths: Vec<PathBuf> = self.search_paths.clone();
        for search_path in &paths {
            if search_path.exists() {
                self.discover_recursive(search_path, 0);
            }
        }
        self.register_builtins();
    }

    /// 递归搜索目录中的Skill文件
    fn discover_recursive(&mut self, dir: &Path, depth: usize) {
        if depth > MAX_SKILL_DEPTH {
            return;
        }

        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();

            // 修复(Low-skills):原用 `path.is_dir()`(跟随符号链接),若 `.claude/skills/`
            // 下存在指向祖先目录的符号链接环,虽 depth 递增最终会停,但每层因环重复进入
            // 同一物理目录,造成重复 read_dir + parse_skill_file(读盘放大)。改用
            // symlink_metadata 判断:符号链接当文件处理(尝试解析,不递归),杜绝环。
            let is_symlink_dir = entry
                .path()
                .symlink_metadata()
                .map(|m| m.is_symlink())
                .unwrap_or(false);
            let is_real_dir = path.is_dir() && !is_symlink_dir;

            if is_real_dir {
                let skill_file = path.join("SKILL.md");
                if skill_file.exists()
                    && let Some(skill) = Self::parse_skill_file(&skill_file)
                {
                    self.skills.insert(skill.name.clone(), skill);
                }
                self.discover_recursive(&path, depth + 1);
            } else if path.file_name().is_some_and(|n| n == "SKILL.md") {
                if let Some(skill) = Self::parse_skill_file(&path) {
                    self.skills.insert(skill.name.clone(), skill);
                }
            } else if path.extension().is_some_and(|e| e == "skill")
                && let Some(skill) = Self::parse_skill_json(&path)
            {
                self.skills.insert(skill.name.clone(), skill);
            }
        }
    }

    /// 解析SKILL.md格式的Skill文件
    fn parse_skill_file(path: &Path) -> Option<Skill> {
        let content = fs::read_to_string(path).ok()?;
        if content.trim().is_empty() {
            return None;
        }
        if content.len() > MAX_SKILL_SIZE {
            return None;
        }

        let name = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let (description, trigger, skill_type, tags, version, input_schema) =
            Self::parse_frontmatter(&content);

        Some(Skill {
            name,
            description,
            skill_type,
            content,
            source: path.to_path_buf(),
            trigger,
            version,
            tags,
            input_schema,
            builtin: false,
            enabled: true,
        })
    }

    /// 解析JSON格式的Skill文件
    fn parse_skill_json(path: &Path) -> Option<Skill> {
        let content = fs::read_to_string(path).ok()?;
        if content.len() > MAX_SKILL_SIZE {
            return None;
        }

        let mut skill: Skill = serde_json::from_str(&content).ok()?;
        // 修复(审查,Blocker):`builtin` 与 `source` 此前直接由 .skill JSON 内容自证。
        // 恶意仓库可伪造 `"builtin": true` + `"source": "builtin://x"`,使不可信技能
        // 内容以无 `<untrusted>` 包裹的 `<skill>` 系统级块注入 prompt(绕过 prompt
        // injection 缓解)。工作区文件加载的技能强制为非内置、source 指向真实文件路径
        // ——"内置=可信"只能由代码内的 builtin_skills() 产生,不可由文件伪造。
        skill.builtin = false;
        skill.source = path.to_path_buf();
        Some(skill)
    }

    /// 解析Skill文件的frontmatter元数据
    fn parse_frontmatter(
        content: &str,
    ) -> (
        String,
        Option<String>,
        SkillType,
        Vec<String>,
        String,
        Option<serde_json::Value>,
    ) {
        let mut description = String::new();
        let mut trigger = None;
        let mut skill_type = SkillType::Prompt;
        let mut tags = Vec::new();
        let mut version = "1.0".to_string();
        let mut input_schema = None;

        if content.starts_with("---")
            && let Some(end) = content[3..].find("---")
        {
            let frontmatter = &content[3..end + 3];
            for line in frontmatter.lines() {
                let line = line.trim();
                if let Some(desc) = line.strip_prefix("description:") {
                    description = desc.trim().trim_matches('"').to_string();
                } else if let Some(trig) = line.strip_prefix("trigger:") {
                    trigger = Some(trig.trim().trim_matches('"').to_string());
                } else if let Some(st) = line.strip_prefix("type:") {
                    let st = st.trim().trim_matches('"').to_lowercase();
                    skill_type = match st.as_str() {
                        "tool" => SkillType::Tool,
                        "workflow" => SkillType::Workflow,
                        "template" => SkillType::Template,
                        _ => SkillType::Prompt,
                    };
                } else if let Some(t) = line.strip_prefix("tags:") {
                    let t = t.trim();
                    if t.starts_with('[') && t.ends_with(']') {
                        tags = t[1..t.len() - 1]
                            .split(',')
                            .map(|s| s.trim().trim_matches('"').to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                    }
                } else if let Some(v) = line.strip_prefix("version:") {
                    version = v.trim().trim_matches('"').to_string();
                } else if let Some(s) = line.strip_prefix("input_schema:")
                    && let Ok(schema) = serde_json::from_str(s.trim())
                {
                    input_schema = Some(schema);
                }
            }
        }

        if description.is_empty() {
            description = content
                .lines()
                .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
                .map(|l| l.trim().chars().take(100).collect())
                .unwrap_or_default();
        }

        (
            description,
            trigger,
            skill_type,
            tags,
            version,
            input_schema,
        )
    }

    /// 注册内置Skill
    fn register_builtins(&mut self) {
        let builtins = Self::builtin_skills();
        for skill in builtins {
            self.skills.entry(skill.name.clone()).or_insert(skill);
        }
    }

    /// 获取内置Skill列表
    fn builtin_skills() -> Vec<Skill> {
        vec![
            Skill {
                name: "code-review".into(),
                description: "代码审查：分析代码质量、安全漏洞、性能问题和最佳实践".into(),
                skill_type: SkillType::Prompt,
                content: "你是一位资深代码审查专家。请对提供的代码进行以下维度的审查：\n\
                    1. **安全性** - 是否存在注入、XSS、敏感信息泄露等安全风险\n\
                    2. **性能** - 是否有不必要的计算、内存泄漏、N+1查询等性能问题\n\
                    3. **可维护性** - 命名是否清晰、函数是否过长、是否有重复代码\n\
                    4. **正确性** - 逻辑是否正确、边界条件是否处理\n\
                    5. **最佳实践** - 是否遵循语言/框架的惯用写法\n\n\
                    请按严重程度（🔴严重 🟡警告 🟢建议）分级列出问题，并给出修复建议。"
                    .into(),
                source: PathBuf::from("builtin://code-review"),
                trigger: Some("review code".into()),
                version: "1.0".into(),
                tags: vec!["code-quality".into(), "security".into(), "review".into()],
                input_schema: None,
                builtin: true,
                enabled: true,
            },
            Skill {
                name: "refactor".into(),
                description: "代码重构：改善代码结构、提升可读性和可维护性".into(),
                skill_type: SkillType::Prompt,
                content: "你是一位代码重构专家。请对提供的代码进行重构，遵循以下原则：\n\
                    1. **单一职责** - 每个函数/类只做一件事\n\
                    2. **消除重复** - DRY原则，提取公共逻辑\n\
                    3. **命名优化** - 变量、函数、类名应自解释\n\
                    4. **简化逻辑** - 减少嵌套、使用早返回\n\
                    5. **类型安全** - 利用类型系统防止错误\n\n\
                    重构时请保持功能不变，逐步改进，每步说明改动原因。"
                    .into(),
                source: PathBuf::from("builtin://refactor"),
                trigger: Some("refactor".into()),
                version: "1.0".into(),
                tags: vec!["refactor".into(), "code-quality".into()],
                input_schema: None,
                builtin: true,
                enabled: true,
            },
            Skill {
                name: "test-gen".into(),
                description: "测试生成：为代码自动生成单元测试".into(),
                skill_type: SkillType::Prompt,
                content: "你是一位测试工程师。请为提供的代码生成全面的单元测试：\n\
                    1. **正常路径** - 测试主要功能的正确行为\n\
                    2. **边界条件** - 空输入、最大值、最小值、零值\n\
                    3. **错误路径** - 无效输入、异常情况\n\
                    4. **并发安全** - 如适用，测试并发场景\n\n\
                    使用项目已有的测试框架，遵循AAA模式（Arrange-Act-Assert），\n\
                    测试名称应清晰描述预期行为。"
                    .into(),
                source: PathBuf::from("builtin://test-gen"),
                trigger: Some("generate test".into()),
                version: "1.0".into(),
                tags: vec!["testing".into(), "generation".into()],
                input_schema: None,
                builtin: true,
                enabled: true,
            },
            Skill {
                name: "explain".into(),
                description: "代码解释：深入分析代码逻辑和工作原理".into(),
                skill_type: SkillType::Prompt,
                content: "你是一位技术导师。请深入浅出地解释提供的代码：\n\
                    1. **整体架构** - 代码的组织结构和设计思路\n\
                    2. **核心逻辑** - 关键算法和业务逻辑的逐步解析\n\
                    3. **数据流** - 数据如何在各组件间流转\n\
                    4. **设计模式** - 使用了哪些设计模式，为什么\n\
                    5. **关键细节** - 容易被忽略但重要的实现细节\n\n\
                    使用类比和图示（ASCII）帮助理解，适合不同水平的开发者。"
                    .into(),
                source: PathBuf::from("builtin://explain"),
                trigger: Some("explain code".into()),
                version: "1.0".into(),
                tags: vec!["explanation".into(), "learning".into()],
                input_schema: None,
                builtin: true,
                enabled: true,
            },
            Skill {
                name: "security-audit".into(),
                description: "安全审计：全面检查代码安全漏洞".into(),
                skill_type: SkillType::Prompt,
                content: "你是一位应用安全专家。请对代码进行安全审计：\n\
                    1. **注入攻击** - SQL注入、命令注入、XSS、SSRF\n\
                    2. **认证授权** - 身份验证绕过、权限提升、会话管理\n\
                    3. **数据保护** - 敏感数据泄露、加密不当、不安全存储\n\
                    4. **配置安全** - 默认配置、调试信息、CORS策略\n\
                    5. **依赖安全** - 已知漏洞的依赖、供应链攻击\n\n\
                    按OWASP Top 10分类，标注风险等级（🔴高危 🟡中危 🟢低危），给出修复方案。"
                    .into(),
                source: PathBuf::from("builtin://security-audit"),
                trigger: Some("security audit".into()),
                version: "1.0".into(),
                tags: vec!["security".into(), "audit".into()],
                input_schema: None,
                builtin: true,
                enabled: true,
            },
            Skill {
                name: "api-design".into(),
                description: "API设计：设计RESTful或GraphQL API".into(),
                skill_type: SkillType::Template,
                content: "你是一位API设计专家。请根据需求设计API：\n\
                    1. **资源建模** - 识别核心资源和关系\n\
                    2. **端点设计** - URL结构、HTTP方法、状态码\n\
                    3. **请求/响应** - 数据格式、分页、过滤、排序\n\
                    4. **版本管理** - API版本策略\n\
                    5. **错误处理** - 统一错误响应格式\n\
                    6. **认证鉴权** - API Key / OAuth2 / JWT\n\n\
                    输出OpenAPI 3.0规范的YAML格式。"
                    .into(),
                source: PathBuf::from("builtin://api-design"),
                trigger: Some("design api".into()),
                version: "1.0".into(),
                tags: vec!["api".into(), "design".into(), "template".into()],
                input_schema: None,
                builtin: true,
                enabled: true,
            },
            Skill {
                name: "debug".into(),
                description: "调试分析：系统化排查和定位Bug".into(),
                skill_type: SkillType::Workflow,
                content: "你是一位调试专家。请系统化地排查问题：\n\
                    **调试流程：**\n\
                    1. **复现问题** - 确认Bug的复现条件和步骤\n\
                    2. **缩小范围** - 二分法定位问题区域\n\
                    3. **假设验证** - 提出可能原因，逐一验证\n\
                    4. **根因分析** - 找到根本原因而非表面症状\n\
                    5. **修复验证** - 提出修复方案并验证\n\n\
                    **分析维度：**\n\
                    - 输入数据是否合法？\n\
                    - 边界条件是否处理？\n\
                    - 并发/竞态是否可能？\n\
                    - 外部依赖是否正常？\n\
                    - 最近变更是否引入？\n\n\
                    请使用搜索和读取工具收集信息，不要猜测。"
                    .into(),
                source: PathBuf::from("builtin://debug"),
                trigger: Some("debug".into()),
                version: "1.0".into(),
                tags: vec!["debug".into(), "troubleshoot".into()],
                input_schema: None,
                builtin: true,
                enabled: true,
            },
            Skill {
                name: "perf-optimize".into(),
                description: "性能优化：分析性能瓶颈并提出优化方案".into(),
                skill_type: SkillType::Workflow,
                content: "你是一位性能优化专家。请系统化地分析和优化性能：\n\
                    **优化流程：**\n\
                    1. **测量基线** - 确认当前性能指标\n\
                    2. **识别瓶颈** - CPU/内存/IO/网络，哪个是瓶颈\n\
                    3. **分析原因** - 算法复杂度、数据结构选择、系统调用\n\
                    4. **提出方案** - 多种优化方案及预期收益\n\
                    5. **实施验证** - 逐步优化并测量效果\n\n\
                    **常见优化方向：**\n\
                    - 算法优化：O(n²) → O(n log n) 或 O(n)\n\
                    - 缓存策略：计算缓存、查询缓存、CDN\n\
                    - 异步处理：IO密集型改异步\n\
                    - 批量操作：减少循环内单次操作\n\
                    - 惰性加载：按需计算和加载\n\n\
                    优先优化收益最大的部分，避免过早优化。"
                    .into(),
                source: PathBuf::from("builtin://perf-optimize"),
                trigger: Some("optimize performance".into()),
                version: "1.0".into(),
                tags: vec!["performance".into(), "optimization".into()],
                input_schema: None,
                builtin: true,
                enabled: true,
            },
        ]
    }

    /// 运行时注册一个Skill
    pub fn register(&mut self, skill: Skill) {
        self.skills.insert(skill.name.clone(), skill);
    }

    /// 注销一个Skill
    pub fn unregister(&mut self, name: &str) -> bool {
        self.skills.remove(name).is_some()
    }

    /// 启用/禁用Skill
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        if let Some(skill) = self.skills.get_mut(name) {
            skill.enabled = enabled;
            true
        } else {
            false
        }
    }

    /// 根据名称获取Skill
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// 获取所有已注册的Skill列表
    pub fn list(&self) -> Vec<&Skill> {
        let mut skills: Vec<&Skill> = self.skills.values().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// 获取已启用的Skill列表
    pub fn list_enabled(&self) -> Vec<&Skill> {
        let mut skills: Vec<&Skill> = self.skills.values().filter(|s| s.enabled).collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// 获取指定类型的Skill列表
    pub fn list_by_type(&self, skill_type: SkillType) -> Vec<&Skill> {
        let mut skills: Vec<&Skill> = self
            .skills
            .values()
            .filter(|s| s.skill_type == skill_type)
            .collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// 根据用户输入匹配最相关的Skill
    pub fn match_skill(&self, input: &str) -> Vec<SkillMatch> {
        let input_lower = input.to_lowercase();
        let mut matches: Vec<SkillMatch> = Vec::new();

        for skill in self.skills.values() {
            if !skill.enabled {
                continue;
            }

            let mut confidence = 0.0f64;

            if input_lower.contains(&skill.name.to_lowercase()) {
                confidence += 0.8;
            }

            if let Some(ref trigger) = skill.trigger
                && input_lower.contains(&trigger.to_lowercase())
            {
                confidence += 0.6;
            }

            for word in skill.description.to_lowercase().split_whitespace() {
                if word.len() > 3 && input_lower.contains(word) {
                    confidence += 0.15;
                }
            }

            for tag in &skill.tags {
                if input_lower.contains(&tag.to_lowercase()) {
                    confidence += 0.3;
                }
            }

            if confidence > 0.3 {
                matches.push(SkillMatch {
                    skill: skill.clone(),
                    confidence: confidence.min(1.0),
                });
            }
        }

        matches.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        matches
    }

    /// 将Skill格式化为system prompt注入块
    ///
    /// 修复(G-H8):来自工作区(非 builtin)的 skill 内容是不可信的——恶意仓库可
    /// 在 `.claude/skills/` 放置注入文本。对非 builtin 来源,用 <untrusted> 标签
    /// 包裹并加免责声明,降低 prompt injection 优先级。
    pub fn skill_as_system_block(&self, name: &str) -> Option<String> {
        let skill = self.skills.get(name)?;
        let is_builtin = skill
            .source
            .to_str()
            .map(|s| s.starts_with("builtin://"))
            .unwrap_or(false);
        if is_builtin {
            Some(format!(
                "<skill name=\"{}\" type=\"{}\">\n{}\n</skill>",
                skill.name,
                skill.skill_type.display_name(),
                skill.content
            ))
        } else {
            // 工作区来源:标记为不可信数据,内容不得当作系统指令执行。
            Some(format!(
                "<untrusted source=\"workspace-skill\" name=\"{}\">\n{}\n</untrusted>",
                skill.name, skill.content
            ))
        }
    }

    /// 获取所有已启用Skill的system prompt注入
    pub fn all_skills_system_block(&self) -> String {
        let enabled_skills: Vec<&Skill> = self.skills.values().filter(|s| s.enabled).collect();
        if enabled_skills.is_empty() {
            return String::new();
        }

        let mut blocks = Vec::new();
        blocks.push("<skills>".to_string());
        blocks.push(format!("可用技能（{}个）：", enabled_skills.len()));

        for skill in &enabled_skills {
            blocks.push(format!(
                "- {} {}: {}",
                skill.skill_type.icon(),
                skill.name,
                skill.description
            ));
        }

        blocks.push("当用户请求匹配某个技能时，请使用对应技能的方法论来处理。".to_string());
        blocks.push("</skills>".to_string());

        blocks.join("\n")
    }

    /// 获取Skill数量
    pub fn count(&self) -> usize {
        self.skills.len()
    }

    /// 获取已启用Skill数量
    pub fn count_enabled(&self) -> usize {
        self.skills.values().filter(|s| s.enabled).count()
    }

    /// 将Skill持久化到文件
    pub fn save_skill(&self, skill: &Skill) -> std::result::Result<(), String> {
        let skill_dir = self
            .workspace
            .join(".movix")
            .join("skills")
            .join(&skill.name);
        fs::create_dir_all(&skill_dir).map_err(|e| e.to_string())?;

        let skill_path = skill_dir.join("SKILL.md");
        let mut content = String::new();

        content.push_str("---\n");
        content.push_str(&format!("description: \"{}\"\n", skill.description));
        content.push_str(&format!(
            "type: {}\n",
            skill.skill_type.display_name().to_lowercase()
        ));
        if let Some(ref trigger) = skill.trigger {
            content.push_str(&format!("trigger: \"{}\"\n", trigger));
        }
        if !skill.tags.is_empty() {
            let tags_str = skill
                .tags
                .iter()
                .map(|t| format!("\"{}\"", t))
                .collect::<Vec<_>>()
                .join(", ");
            content.push_str(&format!("tags: [{}]\n", tags_str));
        }
        content.push_str(&format!("version: \"{}\"\n", skill.version));
        content.push_str("---\n\n");
        content.push_str(&skill.content);

        // 修复(Critical #C10):原子写,崩溃不留半截损坏的 SKILL.md。
        crate::common::utils::atomic_write(&skill_path, &content).map_err(|e| e.to_string())
    }

    /// 重新发现Skill（刷新注册表）
    pub fn refresh(&mut self) {
        self.skills.clear();
        self.discover();
    }

    /// 获取Skill的统计摘要
    pub fn stats(&self) -> SkillStats {
        let total = self.skills.len();
        let enabled = self.skills.values().filter(|s| s.enabled).count();
        let builtin = self.skills.values().filter(|s| s.builtin).count();
        let prompt_count = self
            .skills
            .values()
            .filter(|s| s.skill_type == SkillType::Prompt)
            .count();
        let tool_count = self
            .skills
            .values()
            .filter(|s| s.skill_type == SkillType::Tool)
            .count();
        let workflow_count = self
            .skills
            .values()
            .filter(|s| s.skill_type == SkillType::Workflow)
            .count();
        let template_count = self
            .skills
            .values()
            .filter(|s| s.skill_type == SkillType::Template)
            .count();

        SkillStats {
            total,
            enabled,
            builtin,
            custom: total - builtin,
            prompt_count,
            tool_count,
            workflow_count,
            template_count,
        }
    }
}

/// Skill统计信息
#[derive(Debug, Clone)]
pub struct SkillStats {
    pub total: usize,
    pub enabled: usize,
    pub builtin: usize,
    pub custom: usize,
    pub prompt_count: usize,
    pub tool_count: usize,
    pub workflow_count: usize,
    pub template_count: usize,
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter() {
        let content = "---\ndescription: \"Test skill\"\ntrigger: \"test\"\ntype: tool\ntags: [\"a\", \"b\"]\nversion: \"2.0\"\n---\n# Test";
        let (desc, trigger, skill_type, tags, version, _schema) =
            SkillRegistry::parse_frontmatter(content);
        assert_eq!(desc, "Test skill");
        assert_eq!(trigger, Some("test".to_string()));
        assert_eq!(skill_type, SkillType::Tool);
        assert_eq!(tags, vec!["a", "b"]);
        assert_eq!(version, "2.0");
    }

    #[test]
    fn test_empty_frontmatter() {
        let content = "# Just a heading\nSome content";
        let (desc, _, skill_type, _, _, _) = SkillRegistry::parse_frontmatter(content);
        assert!(!desc.is_empty());
        assert_eq!(skill_type, SkillType::Prompt);
    }

    #[test]
    fn test_builtin_skills() {
        let skills = SkillRegistry::builtin_skills();
        assert!(!skills.is_empty());
        assert!(skills.iter().any(|s| s.name == "code-review"));
        assert!(skills.iter().any(|s| s.name == "debug"));
    }

    #[test]
    fn test_skill_match() {
        let mut registry = SkillRegistry::new();
        registry.register_builtins();

        let matches = registry.match_skill("review code for security issues");
        assert!(!matches.is_empty());
    }

    #[test]
    fn test_enable_disable() {
        let mut registry = SkillRegistry::new();
        registry.register_builtins();

        assert!(registry.set_enabled("code-review", false));
        assert!(!registry.get("code-review").unwrap().enabled);

        assert!(registry.set_enabled("code-review", true));
        assert!(registry.get("code-review").unwrap().enabled);
    }
}
