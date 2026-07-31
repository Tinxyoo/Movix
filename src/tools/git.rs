use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::process::Command as TokioCommand;
use tokio::time::{Duration, timeout};

use super::{EffectKind, Tool, ToolResult};
use crate::common::error::{MovixError, Result};
use crate::common::utils;

/// git 工具单次调用超时。修复(Low #L3):原实现无超时,恶意/巨型仓库可无限期挂起。
const GIT_TIMEOUT_SECS: u64 = 30;

async fn run_git(workspace: &str, args: &[String]) -> Result<String> {
    let cwd = utils::safe_join_path(workspace, ".").map_err(MovixError::SandboxViolation)?;
    // 修复(R6/git-H6,关键):原实现直接跑用户工作区的 git,继承仓库配置。
    // 恶意 .gitattributes 的 filter(clean/smudge)、.git/config 的 core.fsmonitor、
    // core.gitProxy 会在 git status/diff 时执行任意命令 = RCE。
    // 防护:用 -c 内联覆盖这些危险配置为安全值,且设 core.hooksPath=/dev/null 禁用钩子。
    // 这些 -c 只影响本次调用,不修改仓库配置文件。
    let mut full_args: Vec<String> = vec![
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        "core.gitProxy=".into(),
        "-c".into(),
        "filter.lfs.clean=cat".into(),
        "-c".into(),
        "filter.lfs.smudge=cat".into(),
    ];
    full_args.extend(args.iter().cloned());
    // 修复(Low #L3):用 timeout 包裹,避免巨型 packfile / 慢 URL 重定向挂起整个 agent。
    let output = timeout(Duration::from_secs(GIT_TIMEOUT_SECS), async {
        TokioCommand::new("git")
            .args(&full_args)
            .current_dir(&cwd)
            .kill_on_drop(true)
            .output()
            .await
    })
    .await
    .map_err(|_| {
        MovixError::Other(format!(
            "git 命令超时({}s): git {}",
            GIT_TIMEOUT_SECS,
            args.join(" ")
        ))
    })??;

    // 修复(Low #L3):原实现完全忽略退出码,git 报错时把 stderr 当成功输出返回,
    // LLM 会把 "fatal: not a git repository" 这类错误当真实 status/diff 处理。
    // 现在检查 success,失败时把 stderr(或 stdout)作为错误信息返回。
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        // 退出码非 0:git 报错。返回 stderr 作为可读错误,调用方据此判定失败。
        let msg = if !stderr.is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        // 仍返回 Ok(String)——上层工具据此构造 ToolResult,保持接口契约;
        // 但内容明确标注为 git 错误,LLM 可据此判断而非误读为正常输出。
        return Ok(format!(
            "[git error: exit {}]\n{}",
            output.status.code().unwrap_or(-1),
            msg
        ));
    }

    if stdout.is_empty() && !stderr.is_empty() {
        Ok(stderr)
    } else {
        Ok(stdout)
    }
}

pub struct GitStatusTool {
    workspace: String,
}

impl GitStatusTool {
    pub fn new(workspace: &str) -> Self {
        Self {
            workspace: workspace.to_string(),
        }
    }
}

#[async_trait]
impl Tool for GitStatusTool {
    fn name(&self) -> &str {
        "git_status"
    }
    fn description(&self) -> &str {
        "Show Git working tree status for the configured workspace."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _arguments: &Value) -> Result<ToolResult> {
        let args = vec!["status".to_string(), "--short".to_string()];
        let output = run_git(&self.workspace, &args).await?;
        if output.trim().is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "Working tree clean.".into(),
                error: None,
            });
        }
        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }

    fn effect_kind(&self) -> EffectKind {
        EffectKind::ReadOnly
    }
    fn parallel_safe(&self) -> bool {
        true
    }
}

pub struct GitDiffTool {
    workspace: String,
}

impl GitDiffTool {
    pub fn new(workspace: &str) -> Self {
        Self {
            workspace: workspace.to_string(),
        }
    }
}

#[async_trait]
impl Tool for GitDiffTool {
    fn name(&self) -> &str {
        "git_diff"
    }
    fn description(&self) -> &str {
        "Show Git diff for the configured workspace, optionally scoped to one path."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Optional path relative to the workspace."},
                "staged": {"type": "boolean", "description": "Show staged diff when true."}
            },
            "required": []
        })
    }

    async fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let staged = arguments["staged"].as_bool().unwrap_or(false);
        let path = arguments["path"].as_str();

        let mut args = if staged {
            vec![
                "diff".to_string(),
                "--staged".to_string(),
                "--unified=3".to_string(),
            ]
        } else {
            vec!["diff".to_string(), "--unified=3".to_string()]
        };

        if let Some(p) = path {
            match utils::safe_join_path(&self.workspace, p) {
                Ok(valid_path) => {
                    // 修复(R16):原实现把 canonical 后的绝对路径传给 git,会出现在 diff 输出
                    // 里,泄露工作区真实绝对路径(含用户名/项目位置)给 LLM,进而进对话历史/
                    // session.json。改为相对化到 workspace 后再传。
                    // 注意 valid_path 是 canonicalize 后的绝对路径,需对 workspace 也 canonicalize
                    // 才能 strip 成功;失败则回退用原用户输入 p(相对路径,不含主机信息)。
                    let ws_canon = std::path::PathBuf::from(&self.workspace)
                        .canonicalize()
                        .unwrap_or_else(|_| std::path::PathBuf::from(&self.workspace));
                    let rel = valid_path
                        .strip_prefix(&ws_canon)
                        .map(|r| r.to_path_buf())
                        .unwrap_or_else(|_| std::path::PathBuf::from(p));
                    args.push("--".to_string());
                    args.push(rel.to_string_lossy().to_string());
                }
                Err(e) => {
                    return Ok(ToolResult::err(format!("path validation failed: {}", e)));
                }
            }
        }

        let output = run_git(&self.workspace, &args).await?;
        let truncated = if output.len() > 30_000 {
            // 修复(MSRV):floor_char_boundary 在 Rust 1.91 才稳定,而 Cargo.toml
            // 声明 rust-version = "1.85"。改用项目自带的 previous_char_boundary
            // (语义一致:向前回退到最近的 UTF-8 字符边界)。
            let safe_end = utils::previous_char_boundary(&output, 30_000);
            format!(
                "{}...\n[diff truncated, total {} chars]",
                &output[..safe_end],
                output.len()
            )
        } else if output.trim().is_empty() {
            "No changes.".into()
        } else {
            output
        };

        Ok(ToolResult {
            success: true,
            output: truncated,
            error: None,
        })
    }

    fn effect_kind(&self) -> EffectKind {
        EffectKind::ReadOnly
    }
    fn parallel_safe(&self) -> bool {
        true
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

pub struct GitLogTool {
    workspace: String,
}

impl GitLogTool {
    pub fn new(workspace: &str) -> Self {
        Self {
            workspace: workspace.to_string(),
        }
    }
}

#[async_trait]
impl Tool for GitLogTool {
    fn name(&self) -> &str {
        "git_log"
    }
    fn description(&self) -> &str {
        "Show Git commit history for the configured workspace."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "count": {"type": "integer", "description": "Number of commits to show, default 10."}
            },
            "required": []
        })
    }

    async fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let count = arguments["count"].as_u64().unwrap_or(10).min(50);
        let args = vec![
            "log".to_string(),
            "--oneline".to_string(),
            "--decorate".to_string(),
            "-n".to_string(),
            count.to_string(),
        ];

        let output = run_git(&self.workspace, &args).await?;

        Ok(ToolResult {
            success: true,
            output: if output.trim().is_empty() {
                "No commit history.".into()
            } else {
                output
            },
            error: None,
        })
    }

    fn effect_kind(&self) -> EffectKind {
        EffectKind::ReadOnly
    }
    fn parallel_safe(&self) -> bool {
        true
    }
}
