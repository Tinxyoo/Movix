use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::fs;

use super::{EffectKind, Tool, ToolResult};
use crate::common::error::Result;
use crate::common::utils;

/// Upper bound for search results. Balances context budget vs completeness;
/// exceeding this would bloat LLM context window.
const MAX_SEARCH_CODE_RESULTS: usize = 500;
const MAX_GREP_RESULTS: usize = 500;
/// Skip files larger than this in grep to avoid reading huge files into memory.
const MAX_GREP_FILE_BYTES: u64 = 1_000_000;

/// 代码文件搜索工具，支持 glob 模式匹配
pub struct SearchCodeTool {
    workspace: String,
}

impl SearchCodeTool {
    pub fn new(workspace: &str) -> Self {
        Self {
            workspace: workspace.to_string(),
        }
    }
}

#[async_trait]
impl Tool for SearchCodeTool {
    fn name(&self) -> &str {
        "search_code"
    }
    fn description(&self) -> &str {
        "在代码库中搜索文件或文件名。支持 glob 模式匹配，按修改时间排序返回"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob 模式，如 **/*.rs 或 src/**/*.ts"},
                "path": {"type": "string", "description": "搜索起始目录，默认为 workspace"}
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let pattern = arguments["pattern"].as_str().unwrap_or("**/*");
        let search_path = arguments["path"].as_str().unwrap_or(&self.workspace);

        let search_dir = if search_path.is_empty() || search_path == "." {
            PathBuf::from(&self.workspace)
        } else {
            match utils::safe_join_path(&self.workspace, search_path) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult::err(e));
                }
            }
        };

        let pattern_path = search_dir.join(pattern);
        let pattern_str = pattern_path.to_string_lossy().to_string();

        let output = tokio::task::spawn_blocking(move || {
            let mut results = Vec::new();
            let glob_pattern = glob::Pattern::new(&pattern_str).ok();

            walk_dir_sync(
                &search_dir,
                &glob_pattern,
                &mut results,
                0,
                3,
                MAX_SEARCH_CODE_RESULTS,
            );
            results
        })
        .await
        .unwrap_or_default();

        if output.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("未找到匹配 '{}' 的文件", pattern),
                error: None,
            });
        }

        let result = output.join("\n");
        Ok(ToolResult {
            success: true,
            output: format!(
                "找到 {} 个匹配 '{}' 的文件:\n{}",
                output.len(),
                pattern,
                result
            ),
            error: None,
        })
    }

    fn parallel_safe(&self) -> bool {
        true
    }
    fn effect_kind(&self) -> EffectKind {
        EffectKind::ReadOnly
    }
    fn affected_paths(&self, args: &Value) -> Vec<PathBuf> {
        if let Some(p) = args.get("path").and_then(|v| v.as_str())
            && !p.is_empty()
        {
            return vec![PathBuf::from(p)];
        }
        Vec::new()
    }
}

/// 同步递归遍历目录，匹配 glob 模式
fn walk_dir_sync(
    dir: &std::path::Path,
    glob_pattern: &Option<glob::Pattern>,
    results: &mut Vec<String>,
    depth: usize,
    max_depth: usize,
    max_results: usize,
) {
    if depth > max_depth || !dir.is_dir() || results.len() >= max_results {
        return;
    }

    let ignore_dirs = [
        "target",
        "node_modules",
        ".git",
        "__pycache__",
        ".venv",
        "dist",
        "build",
    ];

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();

            if ignore_dirs.contains(&name.as_ref()) {
                continue;
            }

            if path.is_dir() {
                walk_dir_sync(
                    &path,
                    glob_pattern,
                    results,
                    depth + 1,
                    max_depth,
                    max_results,
                );
                continue;
            }

            if let Some(pattern) = glob_pattern {
                let path_str = path.to_string_lossy();
                if pattern.matches(&path_str)
                    && let Ok(meta) = path.metadata()
                {
                    let size = meta.len();
                    results.push(format!("  {} ({})", path_str, utils::format_size(size)));
                    if results.len() >= max_results {
                        return;
                    }
                }
            } else {
                if let Ok(meta) = path.metadata() {
                    let size = meta.len();
                    results.push(format!(
                        "  {} ({})",
                        path.display(),
                        utils::format_size(size)
                    ));
                    if results.len() >= max_results {
                        return;
                    }
                }
            }
        }
    }
}

/// 文本内容搜索工具，支持正则表达式和大小写控制
pub struct GrepTool {
    workspace: String,
}

impl GrepTool {
    pub fn new(workspace: &str) -> Self {
        Self {
            workspace: workspace.to_string(),
        }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "在代码文件中搜索文本内容（正则表达式）。返回匹配行及文件路径和行号"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "搜索关键词或正则表达式"},
                "path": {"type": "string", "description": "搜索目录，默认为 workspace"},
                "file_types": {"type": "string", "description": "文件类型过滤，如 .rs,.toml"},
                "case_sensitive": {"type": "boolean", "description": "是否区分大小写，默认 false"},
                "max_results": {"type": "integer", "description": "最大结果数，默认 50"}
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let pattern = arguments["pattern"].as_str().unwrap_or("");
        let search_path = arguments["path"].as_str().unwrap_or(&self.workspace);
        let file_types = arguments["file_types"].as_str();
        let case_sensitive = arguments["case_sensitive"].as_bool().unwrap_or(false);
        let max_results = arguments["max_results"]
            .as_u64()
            .unwrap_or(50)
            .min(MAX_GREP_RESULTS as u64) as usize;

        let regex_pattern = if case_sensitive {
            pattern.to_string()
        } else {
            format!("(?i){}", pattern)
        };

        let regex = match Regex::new(&regex_pattern) {
            Ok(r) => r,
            Err(_) => {
                let escaped = regex::escape(pattern);
                let escaped_pattern = if case_sensitive {
                    escaped
                } else {
                    format!("(?i){}", escaped)
                };
                Regex::new(&escaped_pattern).map_err(|e| {
                    crate::common::error::MovixError::Other(format!("正则解析失败: {}", e))
                })?
            }
        };

        let search_dir = if search_path.is_empty() || search_path == "." {
            PathBuf::from(&self.workspace)
        } else {
            match utils::safe_join_path(&self.workspace, search_path) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult::err(e));
                }
            }
        };

        let extensions: Vec<String> = file_types
            .map(|ft| {
                ft.split(',')
                    .map(|s| s.trim().trim_start_matches('.').to_string())
                    .collect()
            })
            .unwrap_or_default();

        let mut results: Vec<String> = Vec::new();
        let base = PathBuf::from(&self.workspace);
        let config = GrepConfig {
            regex: &regex,
            extensions: &extensions,
            base: &base,
            max_results,
            max_depth: 4,
        };
        // 修复(R8,关键):原实现无超时也无总扫描字节上限。恶意仓库(深嵌套 + 大量接近
        // MAX_GREP_FILE_BYTES 的文件)可让 grep 跑数分钟,挂死 agent 一整个 turn。
        // 这里用 30s 软超时包裹,超时返回已收集的部分结果(而非永久挂起)。
        let _timeout_result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            grep_dir(&search_dir, &config, &mut results, 0),
        )
        .await;
        if _timeout_result.is_err() {
            results.push(format!(
                "[grep 超过 30s 时间限制,已停止,以上为部分结果(共 {} 条)]",
                results.len()
            ));
        }

        if results.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("未找到匹配 '{}' 的内容", pattern),
                error: None,
            });
        }

        let count = results.len();
        let output = results.join("\n");

        Ok(ToolResult {
            success: true,
            output: format!("找到 {} 处匹配:\n{}", count, output),
            error: None,
        })
    }

    fn parallel_safe(&self) -> bool {
        true
    }
    fn effect_kind(&self) -> EffectKind {
        EffectKind::ReadOnly
    }
    fn affected_paths(&self, args: &Value) -> Vec<PathBuf> {
        if let Some(p) = args.get("path").and_then(|v| v.as_str())
            && !p.is_empty()
        {
            return vec![PathBuf::from(p)];
        }
        Vec::new()
    }
}

/// 搜索配置
struct GrepConfig<'a> {
    regex: &'a Regex,
    extensions: &'a [String],
    base: &'a std::path::Path,
    max_results: usize,
    max_depth: usize,
}

/// 修复(R17/S11):判断文件名是否敏感(供 grep 跳过),避免把 API key/私钥读进对话上下文。
/// 修复(S11):原用 `lower.contains(s)` 子串匹配会误伤 `grid_rsa.rs`(含 id_rsa)、
/// `rsa_identity_utils.rs`(含 identity)等合法源文件,让 grep 永远搜不到它们。
/// 改为**精确文件名相等**或**扩展名后缀**匹配。
/// 注:点文件(.env/.npmrc 等)已由 grep_dir 的 `starts_with('.')` 预过滤跳过,
/// 这里只处理非点文件的敏感名(SSH 私钥、credentials.json 等)。
fn is_grep_sensitive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // 精确文件名匹配(SSH 私钥 / 公钥 / 凭证文件)。这些是完整文件名,非子串。
    const SENSITIVE_NAMES: &[&str] = &[
        "id_rsa",
        "id_ecdsa",
        "id_ed25519",
        "id_dsa",
        "identity",
        "authorized_keys",
        "known_hosts",
        "credentials",
        "credentials.json",
    ];
    if SENSITIVE_NAMES.iter().any(|s| *s == lower) {
        return true;
    }
    // 密钥/证书扩展名后缀。
    const SENSITIVE_EXT: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".keystore", ".jks", ".kdbx"];
    if SENSITIVE_EXT.iter().any(|e| lower.ends_with(e)) {
        return true;
    }
    false
}

/// 递归搜索目录中的文件内容
async fn grep_dir(
    dir: &std::path::Path,
    config: &GrepConfig<'_>,
    results: &mut Vec<String>,
    depth: usize,
) -> Result<()> {
    if depth > config.max_depth || results.len() >= config.max_results {
        return Ok(());
    }

    let ignore_dirs = [
        "target",
        "node_modules",
        ".git",
        "__pycache__",
        ".venv",
        "dist",
        "build",
    ];

    let mut entries = fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if results.len() >= config.max_results {
            break;
        }

        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with('.') {
            continue;
        }

        let file_type = entry.file_type().await?;

        if file_type.is_dir() {
            if ignore_dirs.contains(&name_str.as_ref()) {
                continue;
            }
            Box::pin(grep_dir(&path, config, results, depth + 1)).await?;
            continue;
        }

        if file_type.is_file() {
            // 修复(R17):跳过敏感文件(.env / 密钥 / 证书),避免 grep 把 API key/私钥读进
            // 对话上下文(进而进 session.json/下次 prompt,可能被 prompt injection 外泄)。
            // file.rs 的 is_sensitive_path 用于写入校验,grep 是读取路径此前无对应防护。
            if is_grep_sensitive(&name_str) {
                continue;
            }
            if !config.extensions.is_empty() {
                let ext = path
                    .extension()
                    .map(|e| e.to_string_lossy().to_string())
                    .unwrap_or_default();
                if !config.extensions.contains(&ext) {
                    continue;
                }
            }

            if path.to_string_lossy().len() > 1000 {
                continue;
            }

            if entry
                .metadata()
                .await
                .map(|m| m.len() > MAX_GREP_FILE_BYTES)
                .unwrap_or(true)
            {
                continue;
            }

            match fs::read_to_string(&path).await {
                Ok(content) => {
                    for (line_no, line) in content.lines().enumerate() {
                        if results.len() >= config.max_results {
                            break;
                        }
                        // 修复(R8):原顺序是先 is_match 再判长度。对超长行(>500 字节)应
                        // 先跳过再匹配,避免对超长行做正则匹配(虽然 regex crate 是线性时间,
                        // 但超长行 + Unicode 属性类仍可能慢)。先 len 检查再 match。
                        if line.len() > 500 {
                            continue;
                        }
                        if config.regex.is_match(line) {
                            let rel_path = path.strip_prefix(config.base).unwrap_or(&path);
                            results.push(format!(
                                "{}:{}: {}",
                                rel_path.display(),
                                line_no + 1,
                                line.trim()
                            ));
                        }
                    }
                }
                Err(_) => continue,
            }
        }
    }

    Ok(())
}
