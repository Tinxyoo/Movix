use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;
use tokio::time::{Duration, timeout};

use super::sandbox::Sandbox;
use super::{EffectKind, RiskLevel, Tool, ToolResult};
use crate::common::error::Result;
use crate::common::utils;

const OUTPUT_LIMIT: usize = 50_000;
/// 修复(Bug #9):subprocess 输出硬上限,防止 `yes`/`cat /dev/urandom` 撑爆内存。
/// 比 OUTPUT_LIMIT 大 4 倍,允许后续 `format_output` 截断展示但不会让进程
/// 在被 kill 之前先把 movix 自己 OOM 掉。
const PROC_OUTPUT_BYTES_HARD_CAP: usize = OUTPUT_LIMIT * 4;
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Executes shell commands after workspace and risk checks.
///
/// This is not an OS-level sandbox. It applies path confinement, timeout,
/// output limits, and dangerous-command screening before invoking the shell.
pub struct ShellTool {
    workspace: String,
    sandbox: Sandbox,
}

impl ShellTool {
    pub fn new(workspace: &str) -> Self {
        // 修复:之前用 `Sandbox::default()`,workspace_root 为空,导致沙箱
        // 无法识别 `cd /etc && cat passwd` 这类"切 cwd 到 workspace 外"的逃逸。
        // 改为绑定到 workspace,启用 check_cwd_escape。
        let workspace_root = match std::path::Path::new(workspace).canonicalize() {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => workspace.to_string(),
        };
        Self {
            workspace: workspace.to_string(),
            sandbox: Sandbox::with_workspace(workspace_root),
        }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn description(&self) -> &str {
        "Run a shell command in the workspace after risk checks. This is not a hard OS sandbox."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to run."},
                "cwd": {"type": "string", "description": "Working directory relative to the workspace; defaults to workspace."}
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let command = arguments["command"].as_str().unwrap_or("");
        let raw_cwd = arguments["cwd"].as_str().unwrap_or(&self.workspace);

        let cwd = match utils::safe_join_path(&self.workspace, raw_cwd) {
            Ok(path) => path.to_string_lossy().to_string(),
            Err(e) => {
                return Ok(ToolResult::err(format!(
                    "working directory validation failed: {}",
                    e
                )));
            }
        };

        if let Err(e) = self.sandbox.validate_command(command) {
            return Ok(ToolResult::err(format!(
                "command blocked by risk checks: {}",
                e
            )));
        }

        let result = if utils::is_windows() {
            self.execute_windows(command, &cwd).await
        } else {
            self.execute_unix(command, &cwd).await
        };

        match result {
            Ok(output) => Ok(output),
            Err(_elapsed) => Ok(ToolResult::err(format!(
                "command timed out after {} seconds",
                DEFAULT_TIMEOUT_SECS
            ))),
        }
    }

    fn effect_kind(&self) -> EffectKind {
        // shell 命令是"副作用"最不可预测的一类:进程可读文件、起网络、
        // 调子 shell——一律按 Command 处理,让 ModePolicy 走最严策略。
        EffectKind::Command
    }

    fn risk_level(&self, _args: &Value) -> RiskLevel {
        // 即便 sandbox 已经过滤了一部分高危命令,ModePolicy 仍应按 High 处理:
        // - Plan 模式直接 Blocked
        // - Agent 模式强制审批
        // - Auto 模式根据具体命令细节再判断
        RiskLevel::High
    }

    fn requires_approval_hint(&self, _args: &Value) -> bool {
        true
    }

    fn affected_paths(&self, args: &Value) -> Vec<PathBuf> {
        // shell 实际触及的路径只能事后才知道(命令内容决定),这里只标 cwd 用于快照范围。
        if let Some(c) = args.get("cwd").and_then(|v| v.as_str())
            && !c.is_empty()
        {
            return vec![PathBuf::from(c)];
        }
        Vec::new()
    }
}

impl ShellTool {
    async fn execute_unix(
        &self,
        command: &str,
        cwd: &str,
    ) -> std::result::Result<ToolResult, tokio::time::error::Elapsed> {
        timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS), async {
            let mut cmd = TokioCommand::new("sh");
            cmd.arg("-c")
                .arg(command)
                .current_dir(cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            // 修复(审查):进程组隔离。此前超时/溢出只 kill `sh` 本身,`sh -c 'rm -rf ~ &'`
            // 这类后台子进程成为孤儿继续执行,超时 kill 形同虚设。设为组首领后,
            // KillGroupOnDrop 能对整个进程组(含所有后代)发 SIGKILL。
            #[cfg(unix)]
            cmd.process_group(0);
            Self::sanitize_env(&mut cmd);
            Self::run_with_caps(cmd).await
        })
        .await
    }

    async fn execute_windows(
        &self,
        command: &str,
        cwd: &str,
    ) -> std::result::Result<ToolResult, tokio::time::error::Elapsed> {
        timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS), async {
            let mut cmd = TokioCommand::new("cmd.exe");
            cmd.arg("/C")
                .arg(command)
                .current_dir(cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            Self::sanitize_env(&mut cmd);
            Self::run_with_caps(cmd).await
        })
        .await
    }

    /// 修复(R5/C1,关键):原实现继承父进程全部环境变量。LLM(prompt 注入)只需
    /// 一句 `env` 即可把 `AWS_SECRET_ACCESS_KEY` / `GITHUB_TOKEN` / `DEEPSEEK_API_KEY`
    /// 等所有密钥 dump 到 stdout → 进入 LLM 上下文 → 上云泄露。
    ///
    /// 与 mcp/client.rs 的 H2 修复保持一致:先 `env_clear()` 再按白名单重建最小环境。
    /// 白名单只含进程运行必需的路径/ locale / 系统变量,任何 `*_TOKEN` / `*_KEY` /
    /// `*_SECRET` / 凭证类变量都不会进入子进程。
    fn sanitize_env(cmd: &mut TokioCommand) {
        cmd.env_clear();
        const SAFE_INHERIT: &[&str] = &[
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "TMPDIR",
            "TMP",
            "TEMP",
            "SYSTEMROOT",
            "WINDIR",
            "COMSPEC",
            "USERPROFILE",
            "PATHEXT",
            "APPDATA",
            "LOCALAPPDATA",
            "PROGRAMFILES",
            "HOMEDRIVE",
            "HOMEPATH",
            "PROCESSOR_ARCHITECTURE",
            "NUMBER_OF_PROCESSORS",
        ];
        for key in SAFE_INHERIT {
            if let Ok(val) = std::env::var(key) {
                cmd.env(key, val);
            }
        }
    }

    /// 修复(Bug #9):流式读取 stdout/stderr 并应用硬上限。
    /// 一旦任一流累计字节数超过 `PROC_OUTPUT_BYTES_HARD_CAP`,立即 kill 子进程,
    /// 避免 `yes` / `cat /dev/urandom` / `dd` 把 movix 进程 OOM 掉。
    async fn run_with_caps(mut cmd: TokioCommand) -> ToolResult {
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("spawn 失败: {e}")),
        };
        // 修复(审查):drop 时对整个进程组发 SIGKILL。覆盖超时(timeout abort 导致
        // 本 future 被 drop)与输出溢出(kill 分支)两条路径,确保 `cmd &` 产生的
        // 后台子进程不会在 shell 被杀后成为孤儿继续执行危险操作。
        #[cfg(unix)]
        let _group_guard = child.id().map(|pid| KillGroupOnDrop(pid as i32));
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();

        let mut stdout_buf = Vec::with_capacity(8 * 1024);
        let mut stderr_buf = Vec::with_capacity(4 * 1024);
        let mut chunk_out = [0u8; 8192];
        let mut chunk_err = [0u8; 8192];
        let mut overflowed = false;

        loop {
            tokio::select! {
                biased;
                read_out = read_into(&mut stdout_pipe, &mut chunk_out) => {
                    match read_out {
                        ReadResult::Eof => stdout_pipe = None,
                        ReadResult::Bytes(n) => {
                            if stdout_buf.len() + n > PROC_OUTPUT_BYTES_HARD_CAP {
                                overflowed = true;
                                break;
                            }
                            stdout_buf.extend_from_slice(&chunk_out[..n]);
                        }
                        ReadResult::Err => stdout_pipe = None,
                        ReadResult::None => {}
                    }
                }
                read_err = read_into(&mut stderr_pipe, &mut chunk_err) => {
                    match read_err {
                        ReadResult::Eof => stderr_pipe = None,
                        ReadResult::Bytes(n) => {
                            if stderr_buf.len() + n > PROC_OUTPUT_BYTES_HARD_CAP {
                                overflowed = true;
                                break;
                            }
                            stderr_buf.extend_from_slice(&chunk_err[..n]);
                        }
                        ReadResult::Err => stderr_pipe = None,
                        ReadResult::None => {}
                    }
                }
                else => break,
            }
            if stdout_pipe.is_none() && stderr_pipe.is_none() {
                break;
            }
        }

        let status = if overflowed {
            let _ = child.kill().await;
            child.wait().await.ok()
        } else {
            child.wait().await.ok()
        };

        let stdout = String::from_utf8_lossy(&stdout_buf).into_owned();
        let stderr = String::from_utf8_lossy(&stderr_buf).into_owned();
        let mut result = String::new();
        if !stdout.is_empty() {
            result.push_str(&stdout);
        }
        if !stderr.is_empty() {
            result.push_str("\n[stderr]: ");
            result.push_str(&stderr);
        }
        if overflowed {
            result.push_str(&format!(
                "\n[output exceeded {PROC_OUTPUT_BYTES_HARD_CAP} bytes; subprocess killed]"
            ));
        }
        let success = !overflowed && status.as_ref().map(|s| s.success()).unwrap_or(false);
        if let Some(s) = &status
            && !s.success()
        {
            result.push_str(&format!("\n[exit code: {}]", s.code().unwrap_or(-1)));
        }

        if result.len() > OUTPUT_LIMIT {
            // 修复(MSRV):floor_char_boundary 在 Rust 1.91 才稳定,而 Cargo.toml
            // 声明 rust-version = "1.85"。改用项目自带的 previous_char_boundary
            // (语义一致:向前回退到最近的 UTF-8 字符边界)。
            let safe_end = utils::previous_char_boundary(&result, OUTPUT_LIMIT);
            result = format!(
                "{}...\n[output truncated, total {} chars]",
                &result[..safe_end],
                result.len()
            );
        }

        if success {
            ToolResult::ok(result)
        } else {
            ToolResult {
                success: false,
                output: result,
                error: Some("命令执行失败或被截断".into()),
            }
        }
    }

    // 修复(Bug #9):format_output 被 run_with_caps 替代,已删除。
}

enum ReadResult {
    Eof,
    Bytes(usize),
    Err,
    None,
}

/// 进程组击杀守卫(Unix)。
///
/// 修复(审查):`sh -c` 的执行方式让危险命令可以 `&` 后台化(`nohup rm -rf ~ &`),
/// 超时只 kill `sh` 本身,后台子进程成为孤儿继续执行。将子进程设为进程组首领后,
/// 本守卫在 drop 时对整组发 SIGKILL,确保无孤儿子进程存活。
#[cfg(unix)]
struct KillGroupOnDrop(i32);

#[cfg(unix)]
impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        // 负 pid 表示发往进程组(组首 pid == 组 pgid)。
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
    }
}

async fn read_into(pipe: &mut Option<impl AsyncReadExt + Unpin>, buf: &mut [u8]) -> ReadResult {
    match pipe {
        Some(p) => match p.read(buf).await {
            Ok(0) => ReadResult::Eof,
            Ok(n) => ReadResult::Bytes(n),
            Err(_) => ReadResult::Err,
        },
        None => {
            // 永不就绪,让 select 偏向另一个 arm
            std::future::pending::<()>().await;
            ReadResult::None
        }
    }
}
