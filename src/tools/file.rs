use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, BufReader};

use super::{EffectKind, RiskLevel, Tool, ToolResult};
use crate::common::error::Result;
use crate::common::utils;

const MAX_READ_FILE_BYTES: u64 = 1_000_000;

/// 修复(Bug #10):原子写入。
/// 先写到 `<path>.movix.tmp.<pid>.<nanos>`,fsync,再 rename 替换原文件。
/// 中途崩溃 / Ctrl-C / 磁盘满最多留下临时文件,不会把原文件截断成残余前缀。
async fn atomic_write(target: &Path, contents: &str) -> std::io::Result<()> {
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target has no parent")
    })?;
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let file_name = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed");
    let tmp = parent.join(format!(".{file_name}.movix.tmp.{pid}.{nanos}"));

    // 写入临时文件 + fsync,然后 rename(在多数 fs 上是原子的)。
    {
        use tokio::io::AsyncWriteExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await?;
        f.write_all(contents.as_bytes()).await?;
        f.flush().await?;
        // 尽力 fsync;若不支持(罕见)忽略错误。
        let _ = f.sync_all().await;
    }
    if let Err(e) = fs::rename(&tmp, target).await {
        // rename 失败时清理临时文件,避免污染目录。
        let _ = fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

/// 安全拼接工作目录与相对路径，防止路径遍历攻击
fn safe_path(workspace: &str, path: &str) -> std::result::Result<PathBuf, String> {
    utils::safe_join_path(workspace, path)
}

/// 读取文件内容的工具
pub struct ReadFileTool {
    workspace: String,
}

impl ReadFileTool {
    pub fn new(workspace: &str) -> Self {
        Self {
            workspace: workspace.to_string(),
        }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "读取文件内容。可以指定起始行和行数限制来读取大文件的特定部分"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "文件路径（相对于工作目录）"},
                "offset": {"type": "integer", "description": "起始行号，默认为 1"},
                "limit": {"type": "integer", "description": "读取行数，默认最多 200 行"}
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let path = arguments["path"].as_str().unwrap_or("");
        let offset = arguments["offset"].as_u64().unwrap_or(1).max(1);
        let limit = arguments["limit"].as_u64().unwrap_or(200).min(500);

        let file_path = match safe_path(&self.workspace, path) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::err(e));
            }
        };

        // 修复(审查):write_file/patch_file 都检查 is_sensitive_path,唯独 read_file
        // 只做工作区边界校验,工作区内的 `.env`(含真实 API key)、`.ssh/*`、
        // `id_rsa` 等密钥可被直接读入上下文,再经 web_fetch/run_command 外泄。
        // 与写入侧对齐:读敏感文件同样拒绝。
        if is_sensitive_path(&file_path) {
            return Ok(ToolResult::err(format!(
                "读取被拒绝：{} 是敏感系统文件（如 .env、SSH 密钥、凭证等），不允许读取",
                file_path.display()
            )));
        }

        match fs::metadata(&file_path).await {
            Ok(meta) if meta.len() > MAX_READ_FILE_BYTES => {
                return Ok(ToolResult::err(format!(
                    "文件过大，拒绝一次性读取 {} ({} bytes, limit {} bytes)。请使用 shell 命令分段查看。",
                    file_path.display(),
                    meta.len(),
                    MAX_READ_FILE_BYTES
                )));
            }
            Ok(_) => {}
            Err(e) => {
                return Ok(ToolResult::err(format!(
                    "无法读取文件元数据 {}: {}",
                    file_path.display(),
                    e
                )));
            }
        };

        // 修复(性能/内存):原实现 `fs::read_to_string` 把整个文件读入内存再 `.lines().collect()`,
        // 对于 950KB 的单行 minified JSON / 大日志,会造成瞬时高内存峰值。
        // 改为流式 BufReader 逐行读取,只在窗口 [start_line, end_line) 内拷贝;
        // 一旦读到 end_line 就停止,避免无谓地继续拉取大文件尾部。
        let file = match fs::File::open(&file_path).await {
            Ok(f) => f,
            Err(e) => {
                return Ok(ToolResult::err(format!(
                    "无法读取文件 {}: {}",
                    file_path.display(),
                    e
                )));
            }
        };
        let mut reader = BufReader::new(file);

        // 防御:对单行长度做硬截断,防止"整文件就一行"的 minified JS/JSON 把 read_line 变成
        // 一次性吃完全部内存。这里给单行 1MB 的硬上限,与 MAX_READ_FILE_BYTES 持平。
        const MAX_LINE_BYTES: usize = MAX_READ_FILE_BYTES as usize;

        let start_line = (offset - 1) as usize;
        let end_line = start_line.saturating_add(limit as usize);

        let mut current_line: usize = 0;
        let mut total_lines: u64 = 0;
        let mut window: Vec<String> = Vec::with_capacity(limit as usize);
        let mut line_buf = String::new();

        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf).await {
                Ok(0) => break,
                Ok(_) => {
                    if line_buf.len() > MAX_LINE_BYTES {
                        return Ok(ToolResult::err(format!(
                            "文件 {} 单行长度超出 {} bytes 限制,拒绝读取。请用 shell 命令分段查看。",
                            file_path.display(),
                            MAX_LINE_BYTES
                        )));
                    }
                    if current_line >= start_line && current_line < end_line {
                        // 去掉行尾的 '\n' / "\r\n",与原实现 `.lines()` 行为一致
                        let trimmed_end = line_buf.trim_end_matches('\n').trim_end_matches('\r');
                        window.push(trimmed_end.to_string());
                    }
                    current_line += 1;
                    total_lines += 1;
                    // 已经读够窗口尾部时,继续读到 EOF 仅为统计 total_lines。
                    // 对超大文件这步仍要走流式 read_line,但不再 push,内存 O(window) 即可。
                }
                Err(e) => {
                    return Ok(ToolResult::err(format!(
                        "读取文件失败 {}: {}",
                        file_path.display(),
                        e
                    )));
                }
            }
        }

        let start = (start_line as u64).min(total_lines);
        let end = ((start_line + window.len()) as u64).min(total_lines);

        let partial: String = window
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>6}| {}", start_line + i + 1, line))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolResult {
            success: true,
            output: format!(
                "文件 {} (共{} 行, 显示第{}-{} 行):\n{}",
                file_path.display(),
                total_lines,
                start + 1,
                end,
                partial
            ),
            error: None,
        })
    }

    fn parallel_safe(&self) -> bool {
        true
    }
}

/// 写入文件内容的工具
pub struct WriteFileTool {
    workspace: String,
}

/// 检查路径是否为敏感系统文件，防止 Agent 修改关键配置
/// 修复(P2.2):支持从 ~/.movix/sensitive_patterns.toml 加载用户自定义敏感文件模式。
/// 修复(High #H6):扩展默认敏感文件名单,覆盖常见密钥/凭证文件与扩展名。
fn is_sensitive_path(path: &std::path::Path) -> bool {
    /// 文件名精确匹配(大小写不敏感)
    const SENSITIVE_EXACT: &[&str] = &[
        ".bashrc",
        ".bash_profile",
        ".zshrc",
        ".profile",
        ".env",
        ".env.local",
        ".env.production",
        ".env.development",
        ".env.staging",
        ".gitconfig",
        ".git-credentials",
        ".npmrc",
        ".pypirc",
        ".netrc",
        "hosts",
        "resolv.conf",
        "fstab",
        "sudoers",
        "passwd",
        "shadow",
        // 常见凭证文件名
        "credentials",
        "credentials.json",
        "secrets.yml",
        "secrets.yaml",
        "secret",
        "token",
        "token.json",
        "service-account.json",
        "terraform.tfvars",
        "terraform.tfstate",
        ".htpasswd",
        "authorized_keys",
        "known_hosts",
        ".aws/credentials",
    ];
    /// 文件名前缀匹配(用于 id_rsa / id_rsa.pub / id_ed25519 等)
    const SENSITIVE_PREFIX: &[&str] = &["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"];
    /// 整目录拦截(只要这个目录出现在路径里,就视为敏感)
    const SENSITIVE_DIRS: &[&str] = &[".ssh", ".gnupg", ".aws", ".kube", ".docker"];
    /// 敏感文件扩展名(密钥/证书)
    const SENSITIVE_SUFFIXES: &[&str] = &[
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        ".crt",
        ".cer",
        ".der",
        ".keystore",
        ".jks",
    ];

    for component in path.components() {
        let Some(name) = component.as_os_str().to_str() else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        if SENSITIVE_EXACT.contains(&lower.as_str())
            || SENSITIVE_PREFIX.iter().any(|p| lower.starts_with(p))
            || SENSITIVE_DIRS.contains(&lower.as_str())
            || SENSITIVE_SUFFIXES.iter().any(|s| lower.ends_with(s))
        {
            return true;
        }
    }

    // 检查用户自定义敏感模式
    is_user_sensitive_path(path)
}

/// 从 ~/.movix/sensitive_patterns.toml 加载用户自定义敏感文件模式。
/// 配置文件格式:
/// ```toml
/// exact = [".env.staging", "credentials.json", "terraform.tfvars"]
/// prefix = ["id_rsa", "service-account"]
/// dirs = [".aws", ".kube"]
/// glob = ["**/secrets*", "*.pem", "*.key"]
/// ```
fn load_user_sensitive_patterns() -> Option<UserSensitivePatterns> {
    static PATTERNS: std::sync::OnceLock<Option<UserSensitivePatterns>> =
        std::sync::OnceLock::new();
    PATTERNS
        .get_or_init(|| {
            let home = crate::common::utils::home_dir();
            let config_path = home.join(".movix").join("sensitive_patterns.toml");
            let content = std::fs::read_to_string(&config_path).ok()?;
            let doc = content.parse::<toml::Value>().ok()?;

            let exact: Vec<String> = doc
                .get("exact")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                        .collect()
                })
                .unwrap_or_default();

            let prefix: Vec<String> = doc
                .get("prefix")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                        .collect()
                })
                .unwrap_or_default();

            let dirs: Vec<String> = doc
                .get("dirs")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                        .collect()
                })
                .unwrap_or_default();

            let glob_patterns: Vec<String> = doc
                .get("glob")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            Some(UserSensitivePatterns {
                exact,
                prefix,
                dirs,
                glob_patterns,
            })
        })
        .clone()
}

#[derive(Clone)]
struct UserSensitivePatterns {
    exact: Vec<String>,
    prefix: Vec<String>,
    dirs: Vec<String>,
    glob_patterns: Vec<String>,
}

/// 检查路径是否匹配用户自定义敏感模式
fn is_user_sensitive_path(path: &std::path::Path) -> bool {
    let Some(patterns) = load_user_sensitive_patterns() else {
        return false;
    };

    let path_str = path.to_string_lossy();

    for component in path.components() {
        let Some(name) = component.as_os_str().to_str() else {
            continue;
        };
        let lower = name.to_ascii_lowercase();

        if patterns.exact.contains(&lower) {
            return true;
        }
        if patterns
            .prefix
            .iter()
            .any(|p| lower.starts_with(p.as_str()))
        {
            return true;
        }
        if patterns.dirs.contains(&lower) {
            return true;
        }
    }

    // glob 模式匹配(对完整路径)
    for pattern in &patterns.glob_patterns {
        if let Ok(glob) = glob::Pattern::new(pattern) {
            if glob.matches(&path_str) {
                return true;
            }
        }
    }

    false
}

impl WriteFileTool {
    pub fn new(workspace: &str) -> Self {
        Self {
            workspace: workspace.to_string(),
        }
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "写入或创建文件。如果文件已存在则覆盖，不存在则创建新文件（包括父目录）"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "文件路径（相对于工作目录）"},
                "content": {"type": "string", "description": "要写入的文件内容"}
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let path = arguments["path"].as_str().unwrap_or("");
        let content = arguments["content"].as_str().unwrap_or("");

        let file_path = match safe_path(&self.workspace, path) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::err(e));
            }
        };

        if is_sensitive_path(&file_path) {
            return Ok(ToolResult::err(format!(
                "写入被拒绝：{} 是敏感系统文件（如 shell 配置、SSH 密钥、环境变量等），不允许修改",
                file_path.display()
            )));
        }

        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        // 修复(High #H5):原实现把整个旧文件读入内存仅为生成 diff,500MB 日志会 OOM。
        // 改为先检查文件大小,超阈值则跳过 diff 生成(只写入)。
        const MAX_OLD_CONTENT_BYTES: u64 = 10 * 1024 * 1024; // 10MB
        let (old_content, is_new_file, diff_skipped) = match fs::metadata(&file_path).await {
            Ok(meta) => {
                if meta.len() > MAX_OLD_CONTENT_BYTES {
                    // 旧文件过大,跳过 diff,直接当"覆盖大文件"处理。
                    (String::new(), false, true)
                } else {
                    match fs::read_to_string(&file_path).await {
                        Ok(content) => (content, false, false),
                        Err(_) => (String::new(), false, true),
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), true, false),
            Err(e) => {
                return Ok(ToolResult::err(format!(
                    "无法读取现有文件 {}: {}",
                    file_path.display(),
                    e
                )));
            }
        };

        atomic_write(&file_path, content).await?;

        let lines = content.lines().count();
        let size = content.len();

        let diff = if diff_skipped {
            "\n[diff 已跳过:原文件较大(>10MB)]".to_string()
        } else if is_new_file {
            generate_new_file_diff(content)
        } else {
            generate_diff(&old_content, content)
        };

        Ok(ToolResult {
            success: true,
            output: format!(
                "已写入 {} ({} 行, {} 字节)\n{}",
                file_path.display(),
                lines,
                size,
                diff
            ),
            error: None,
        })
    }

    fn effect_kind(&self) -> EffectKind {
        EffectKind::WorkspaceWrite
    }

    fn risk_level(&self, args: &Value) -> RiskLevel {
        // 用原始字符串判断"用户是否打算写敏感路径"——即使 safe_path 拒绝,
        // 意图本身仍是高风险,ModePolicy 应该看到 High。
        if let Some(p) = args.get("path").and_then(|v| v.as_str())
            && is_sensitive_path(Path::new(p))
        {
            return RiskLevel::High;
        }
        RiskLevel::Medium
    }

    fn requires_approval_hint(&self, _args: &Value) -> bool {
        // 写操作永远建议审批:ModePolicy 在 Auto 模式才会用到这个 hint,
        // Agent/Yolo 模式走自己的 requires_approval 通道。
        true
    }
}
/// 列出目录内容的工具
pub struct ListDirTool {
    workspace: String,
}

impl ListDirTool {
    pub fn new(workspace: &str) -> Self {
        Self {
            workspace: workspace.to_string(),
        }
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "列出目录中的文件和子目录，支持递归和忽略模式"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "目录路径（相对路径或 . 表示当前目录）"},
                "recursive": {"type": "boolean", "description": "是否递归列出子目录"},
                "depth": {"type": "integer", "description": "递归深度限制，默认 2"}
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let path = arguments["path"].as_str().unwrap_or(".");
        let recursive = arguments["recursive"].as_bool().unwrap_or(false);
        let depth = arguments["depth"].as_u64().unwrap_or(2).min(5);

        let dir_path = match safe_path(&self.workspace, path) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::err(e));
            }
        };

        if recursive {
            list_recursive(&dir_path, depth as usize, 0).await
        } else {
            list_single(&dir_path).await
        }
    }

    fn parallel_safe(&self) -> bool {
        true
    }
}
/// 列出单层目录内容
async fn list_single(path: &std::path::Path) -> Result<ToolResult> {
    let mut entries = fs::read_dir(path).await?;
    let mut result = String::from("名称                          | 类型     | 大小\n");
    result.push_str(&"-".repeat(60));
    result.push('\n');

    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // 允许特定的有用配置目录显示
        let allowed_dot_dirs = [".github", ".vscode", ".idea", ".config"];
        if name_str.starts_with('.') && !allowed_dot_dirs.contains(&name_str.as_ref()) {
            continue;
        }
        let meta = entry.metadata().await?;
        let file_type = if meta.is_dir() { "目录" } else { "文件" };
        let size = if meta.is_dir() {
            "-".into()
        } else {
            utils::format_size(meta.len())
        };
        result.push_str(&format!("{:<30} | {:<8} | {}\n", name_str, file_type, size));
    }

    Ok(ToolResult {
        success: true,
        output: result,
        error: None,
    })
}

/// 递归列出目录内容
async fn list_recursive(
    path: &std::path::Path,
    max_depth: usize,
    current: usize,
) -> Result<ToolResult> {
    if current > max_depth {
        return Ok(ToolResult {
            success: true,
            output: String::new(),
            error: None,
        });
    }

    let mut result = String::new();
    let prefix = "  ".repeat(current);

    let mut entries = fs::read_dir(path).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // 跳过构建产物目录
        if name_str == "target"
            || name_str == "node_modules"
            || name_str == "build"
            || name_str == "dist"
            || name_str == ".git"
        {
            continue;
        }
        // 允许特定的有用配置目录
        let allowed_dot_dirs = [".github", ".vscode", ".idea", ".config"];
        if name_str.starts_with('.') && !allowed_dot_dirs.contains(&name_str.as_ref()) {
            continue;
        }

        let file_type = entry.file_type().await?;
        let display_path = entry.path();
        result.push_str(&format!("{}{}\n", prefix, display_path.display()));

        if file_type.is_dir() && current < max_depth {
            let sub = Box::pin(list_recursive(&display_path, max_depth, current + 1)).await?;
            result.push_str(&sub.output);
        }
    }

    Ok(ToolResult {
        success: true,
        output: result,
        error: None,
    })
}

/// 增量代码修补工具
pub struct PatchFileTool {
    workspace: String,
}

impl PatchFileTool {
    pub fn new(workspace: &str) -> Self {
        Self {
            workspace: workspace.to_string(),
        }
    }
}

#[async_trait]
impl Tool for PatchFileTool {
    fn name(&self) -> &str {
        "patch_file"
    }
    fn description(&self) -> &str {
        "在指定文件中查找特定代码段并替换为新代码段（增量修改）。特别适用于大文件的微调，可极大地节约 Token 消耗"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "要修改的文件路径（相对于工作目录）"},
                "patches": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "find": {"type": "string", "description": "要替换的精确现有代码段，必须完全匹配（含缩进和换行）"},
                            "replace": {"type": "string", "description": "替换后的新代码段"}
                        },
                        "required": ["find", "replace"]
                    },
                    "description": "要应用的查找替换块列表，将按顺序执行"
                }
            },
            "required": ["path", "patches"]
        })
    }

    async fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let path = arguments["path"].as_str().unwrap_or("");

        let patches_val = match arguments["patches"].as_array() {
            Some(arr) => arr,
            None => {
                return Ok(ToolResult::err(
                    "patches 参数必须是包含 find 和 replace 的数组".to_string(),
                ));
            }
        };

        let file_path = match safe_path(&self.workspace, path) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::err(e));
            }
        };

        if is_sensitive_path(&file_path) {
            return Ok(ToolResult::err(format!(
                "写入被拒绝：{} 是敏感系统文件（如 shell 配置、SSH 密钥、环境变量等），不允许修改",
                file_path.display()
            )));
        }

        let mut content = match fs::read_to_string(&file_path).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::err(format!(
                    "无法读取文件 {}: {}",
                    file_path.display(),
                    e
                )));
            }
        };

        let mut applied_count = 0;
        let mut diff_parts: Vec<String> = Vec::new();

        for (idx, patch_val) in patches_val.iter().enumerate() {
            let find = patch_val["find"].as_str().unwrap_or("");
            let replace = patch_val["replace"].as_str().unwrap_or("");

            if find.is_empty() {
                return Ok(ToolResult::err(format!(
                    "第 {} 个 patch 的 'find' 字段为空，不可进行空白查找",
                    idx + 1
                )));
            }

            let matches: Vec<_> = content.match_indices(find).collect();
            if matches.is_empty() {
                // 修复(顺序敏感):patches 是按顺序累积应用的,前一个 patch 可能恰好
                // 覆盖/抹掉后一个 patch 的 `find` 字段,导致看上去"原文里有"的
                // 段落变成"找不到"。
                //
                // 修复(High #H3):原措辞"前 N 个 patch 已应用,本次失败前的修改仍会被丢弃"
                // 是**错误的**——atomic_write 只在循环结束后调用,任一 patch 失败即整体
                // return,磁盘上一个 patch 都没应用(全有/全无语义)。原措辞让 LLM 误以为
                // 文件已部分修改,后续基于错误前提操作。改为如实说明:所有修改均未落盘。
                let progress = if applied_count > 0 {
                    format!(
                        "（前 {} 个 patch 虽已在内存中匹配成功,但因第 {} 个失败,**全部 patch 均未落盘**(原子性),文件保持原样）",
                        applied_count,
                        idx + 1
                    )
                } else {
                    String::new()
                };
                return Ok(ToolResult::err(format!(
                    "第 {} 个 patch 执行失败：未在文件中找到精确对应的现有代码段{}。\n\n【试图查找的段落】：\n\"{}\"\n\n请核实空格、换行、缩进是否完全与原文件一致;若 `find` 字段在前一个 patch 后被修改/删除,请调整 patch 顺序。",
                    idx + 1,
                    progress,
                    find
                )));
            }

            if matches.len() > 1 {
                return Ok(ToolResult::err(format!(
                    "第 {} 个 patch 执行失败：在文件中找到了多处（共 {} 处）完全相同的代码段。请提供更长或更具唯一性的 'find' 范围以避免误伤。",
                    idx + 1,
                    matches.len()
                )));
            }

            // 修复(自我覆盖陷阱):如果 `replace` 中再次包含 `find`,
            // 后续若有 patch 想再匹配同一片段,会出现重复匹配/无限递归
            // 的二义,显式提示而非默默替换。
            if !replace.is_empty() && replace.contains(find) {
                tracing::warn!(
                    target: "patch_file",
                    "第 {} 个 patch 的 replace 中再次包含 find,可能让后续 patch 出现重复匹配",
                    idx + 1
                );
            }

            let patch_diff = generate_patch_diff(find, replace, idx + 1);
            diff_parts.push(patch_diff);

            content = content.replacen(find, replace, 1);
            applied_count += 1;
        }

        atomic_write(&file_path, &content).await?;

        let lines = content.lines().count();
        let size = content.len();

        let diff_summary = diff_parts.join("\n");

        Ok(ToolResult {
            success: true,
            output: format!(
                "已成功应用 {} 个修补块到 {} (现存 {} 行, {} 字节)\n{}",
                applied_count,
                file_path.display(),
                lines,
                size,
                diff_summary
            ),
            error: None,
        })
    }

    fn effect_kind(&self) -> EffectKind {
        EffectKind::WorkspaceWrite
    }

    fn risk_level(&self, args: &Value) -> RiskLevel {
        if let Some(p) = args.get("path").and_then(|v| v.as_str())
            && is_sensitive_path(Path::new(p))
        {
            return RiskLevel::High;
        }
        RiskLevel::Medium
    }

    fn requires_approval_hint(&self, _args: &Value) -> bool {
        true
    }
}
/// 生成简单diff格式的变更对比（旧内容 vs 新内容）
#[allow(clippy::mut_range_bound)]
fn generate_diff(old_content: &str, new_content: &str) -> String {
    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();

    let mut diff_lines: Vec<String> = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;

    let max_old = old_lines.len();
    let max_new = new_lines.len();

    let mut i = 0usize;
    let mut j = 0usize;

    while i < max_old || j < max_new {
        let old_line = old_lines.get(i);
        let new_line = new_lines.get(j);

        match (old_line, new_line) {
            (Some(o), Some(n)) if o == n => {
                i += 1;
                j += 1;
            }
            (Some(_), None) => {
                diff_lines.push(format!("-{}", old_lines[i]));
                removed += 1;
                i += 1;
            }
            (None, Some(_)) => {
                diff_lines.push(format!("+{}", new_lines[j]));
                added += 1;
                j += 1;
            }
            (Some(_), Some(_)) => {
                let mut found_in_new = false;
                for k in j..max_new.min(j + 5) {
                    if old_lines[i] == new_lines[k] {
                        for m in &new_lines[j..k] {
                            diff_lines.push(format!("+{}", m));
                            added += 1;
                        }
                        j = k;
                        found_in_new = true;
                        break;
                    }
                }

                if !found_in_new {
                    let mut found_in_old = false;
                    for k in i..max_old.min(i + 5) {
                        if new_lines[j] == old_lines[k] {
                            for m in &old_lines[i..k] {
                                diff_lines.push(format!("-{}", m));
                                removed += 1;
                            }
                            i = k;
                            found_in_old = true;
                            break;
                        }
                    }

                    if !found_in_old {
                        diff_lines.push(format!("-{}", old_lines[i]));
                        removed += 1;
                        diff_lines.push(format!("+{}", new_lines[j]));
                        added += 1;
                        i += 1;
                        j += 1;
                    }
                }
            }
            (None, None) => break,
        }
    }

    let summary = if added + removed > 0 {
        format!("\n变更摘要: +{} / -{}", added, removed)
    } else {
        "\n无变更".to_string()
    };

    if diff_lines.is_empty() {
        summary
    } else {
        let diff_text = diff_lines.join("\n");
        format!("\n--- 变更详情 ---\n{}\n--- 结束 ---{}", diff_text, summary)
    }
}

/// 生成新文件的diff（全部为新增行）
fn generate_new_file_diff(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    if total == 0 {
        return "\n新建空文件".to_string();
    }

    let max_preview = 30usize;
    let preview_lines: Vec<String> = lines
        .iter()
        .take(max_preview)
        .enumerate()
        .map(|(i, line)| format!("+{:>4}| {}", i + 1, line))
        .collect();

    let preview = preview_lines.join("\n");
    let truncation = if total > max_preview {
        format!("\n... 共 {} 行（显示前 {} 行）", total, max_preview)
    } else {
        String::new()
    };

    format!(
        "\n--- 新建文件 (+{} 行) ---\n{}\n--- 结束 ---{}",
        total, preview, truncation
    )
}

/// 生成单个patch的diff（显示被替换的旧代码和新代码）
fn generate_patch_diff(find: &str, replace: &str, patch_num: usize) -> String {
    let find_lines: Vec<&str> = find.lines().collect();
    let replace_lines: Vec<&str> = replace.lines().collect();

    let mut diff_lines: Vec<String> = Vec::new();

    for line in &find_lines {
        diff_lines.push(format!("-{}", line));
    }
    for line in &replace_lines {
        diff_lines.push(format!("+{}", line));
    }

    let diff_text = diff_lines.join("\n");
    let removed = find_lines.len();
    let added = replace_lines.len();

    format!(
        "\n--- Patch #{} (-{} / +{}) ---\n{}\n--- 结束 ---",
        patch_num, removed, added, diff_text
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_patch_file_success() {
        let dir = tempdir().unwrap();
        let workspace_path = dir.path().to_string_lossy().to_string();
        let file_name = "test.txt";
        let file_path = dir.path().join(file_name);

        fs::write(&file_path, "line 1\nline 2\nline 3")
            .await
            .unwrap();

        let tool = PatchFileTool::new(&workspace_path);
        let args = json!({
            "path": file_name,
            "patches": [
                {
                    "find": "line 2",
                    "replace": "line 2 modified"
                }
            ]
        });

        let res = tool.execute(&args).await.unwrap();
        assert!(res.success);
        assert!(res.error.is_none());

        let new_content = fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(new_content, "line 1\nline 2 modified\nline 3");
    }

    #[tokio::test]
    async fn test_patch_file_sequential_success() {
        let dir = tempdir().unwrap();
        let workspace_path = dir.path().to_string_lossy().to_string();
        let file_name = "test.txt";
        let file_path = dir.path().join(file_name);

        fs::write(&file_path, "first block\nsecond block")
            .await
            .unwrap();

        let tool = PatchFileTool::new(&workspace_path);
        let args = json!({
            "path": file_name,
            "patches": [
                {
                    "find": "first block",
                    "replace": "alpha"
                },
                {
                    "find": "second block",
                    "replace": "beta"
                }
            ]
        });

        let res = tool.execute(&args).await.unwrap();
        assert!(res.success);

        let new_content = fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(new_content, "alpha\nbeta");
    }

    #[tokio::test]
    async fn test_patch_file_find_not_found() {
        let dir = tempdir().unwrap();
        let workspace_path = dir.path().to_string_lossy().to_string();
        let file_name = "test.txt";
        let file_path = dir.path().join(file_name);

        fs::write(&file_path, "line 1\nline 2").await.unwrap();

        let tool = PatchFileTool::new(&workspace_path);
        let args = json!({
            "path": file_name,
            "patches": [
                {
                    "find": "non-existent line",
                    "replace": "new line"
                }
            ]
        });

        let res = tool.execute(&args).await.unwrap();
        assert!(!res.success);
        assert!(
            res.error
                .unwrap()
                .contains("未在文件中找到精确对应的现有代码段")
        );
    }

    #[tokio::test]
    async fn test_patch_file_duplicate_ambiguity() {
        let dir = tempdir().unwrap();
        let workspace_path = dir.path().to_string_lossy().to_string();
        let file_name = "test.txt";
        let file_path = dir.path().join(file_name);

        fs::write(&file_path, "duplicate\nduplicate").await.unwrap();

        let tool = PatchFileTool::new(&workspace_path);
        let args = json!({
            "path": file_name,
            "patches": [
                {
                    "find": "duplicate",
                    "replace": "single"
                }
            ]
        });

        let res = tool.execute(&args).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("在文件中找到了多处"));
    }

    #[tokio::test]
    async fn test_patch_file_sensitive_blocked() {
        let dir = tempdir().unwrap();
        let workspace_path = dir.path().to_string_lossy().to_string();
        let tool = PatchFileTool::new(&workspace_path);
        let args = json!({
            "path": ".env",
            "patches": [
                {
                    "find": "API_KEY",
                    "replace": "BLOCKED"
                }
            ]
        });

        let res = tool.execute(&args).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("敏感系统文件"));
    }

    /// 回归(审查):read_file 此前不检查 is_sensitive_path,工作区内 .env 密钥
    /// 可直接读入上下文。修复后读取敏感文件必须被拒绝。
    #[tokio::test]
    async fn test_read_file_sensitive_blocked() {
        let dir = tempdir().unwrap();
        let workspace_path = dir.path().to_string_lossy().to_string();
        fs::write(dir.path().join(".env"), "SECRET=1")
            .await
            .unwrap();

        let tool = ReadFileTool::new(&workspace_path);
        let args = json!({ "path": ".env" });
        let res = tool.execute(&args).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("敏感"));

        // 普通文件不受影响
        fs::write(dir.path().join("ok.txt"), "hi").await.unwrap();
        let res2 = tool.execute(&json!({ "path": "ok.txt" })).await.unwrap();
        assert!(res2.success);
    }

    #[test]
    fn test_sensitive_path_detection() {
        use std::path::Path;

        // 真正的敏感文件应该被拦截
        assert!(is_sensitive_path(Path::new(".env")));
        assert!(is_sensitive_path(Path::new("/home/user/.ssh/id_rsa")));
        assert!(is_sensitive_path(Path::new("/home/user/.ssh/id_rsa.pub")));
        assert!(is_sensitive_path(Path::new(
            "/home/user/.ssh/authorized_keys"
        )));
        assert!(is_sensitive_path(Path::new("/home/user/.bashrc")));

        // 类似但不敏感的文件不应该被拦截
        assert!(!is_sensitive_path(Path::new("mydir.env")));
        assert!(!is_sensitive_path(Path::new("environment.json")));
        assert!(!is_sensitive_path(Path::new("ssh_config")));
        assert!(!is_sensitive_path(Path::new(".ssh_backup")));
    }

    #[test]
    fn test_sensitive_path_subdir() {
        use std::path::Path;

        // .ssh 目录下的任何文件都应该被拦截
        assert!(is_sensitive_path(Path::new(".ssh/config")));
        assert!(is_sensitive_path(Path::new("/a/.ssh/b/c")));

        // .gnupg 目录同样
        assert!(is_sensitive_path(Path::new(".gnupg/secring.gpg")));
    }
}
