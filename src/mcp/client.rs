use crate::common::error::{MovixError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

// 修复(Bug #12):删除全局 JSONRPC_ID 与 atomic import,改用每 McpClient 的 next_id。
use std::time::Duration;
// 注意:不再使用 AsyncBufReadExt。原 read_line(无字节上限,可 OOM)已被
// read_capped_line(逐块 read + 累计字节检查)取代,后者只需 AsyncReadExt。
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::timeout;

// 修复(Bug #12):删除全局 JSONRPC_ID,改用每 McpClient 自己的 next_id。

/// MCP 单次启动/握手超时（10s）。可通过 `MOVIX_MCP_TIMEOUT_SECS` 调整。
fn mcp_timeout() -> Duration {
    let secs: u64 = std::env::var("MOVIX_MCP_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    Duration::from_secs(secs)
}

/// JSON-RPC 2.0 请求
#[derive(Debug, Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

/// JSON-RPC 2.0 响应
#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Option<u64>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

/// JSON-RPC 2.0 错误
#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

/// MCP 服务器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// 服务器名称（唯一标识）
    pub name: String,
    /// 启动命令
    pub command: String,
    /// 命令参数
    #[serde(default)]
    pub args: Vec<String>,
    /// 环境变量
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// 是否启用
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 是否受信任：仅受信任的服务器会自动连接。
    /// 从环境变量配置的服务器默认受信任（用户主动设置），
    /// 从工作区 .movix/mcp.json 文件加载的服务器默认不受信任
    /// （恶意仓库可能植入任意命令）。
    #[serde(default)]
    pub trusted: bool,
}

fn default_true() -> bool {
    true
}

/// MCP 工具定义（从服务器发现）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
}

/// MCP 工具调用结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolResult {
    pub content: Vec<McpContentBlock>,
    #[serde(default)]
    pub is_error: bool,
}

/// MCP 内容块
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpContentBlock {
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

/// MCP 服务器连接状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

/// MCP 客户端，管理与单个 MCP 服务器的通信
pub struct McpClient {
    config: McpServerConfig,
    status: McpServerStatus,
    child: Option<Child>,
    stdin: Option<tokio::process::ChildStdin>,
    stdout_reader: Option<BufReader<tokio::process::ChildStdout>>,
    tools: Vec<McpToolInfo>,
    server_info: Option<Value>,
    /// 修复(Bug #12):本客户端单调递增的 JSON-RPC id(替换原 static 全局),
    /// 让每个连接独立编号,调试日志能定位到 server,且重连后 id 可从 1 开始。
    next_id: u64,
    /// 修复(Bug #12):缓存收到的、与当前 send_request 不匹配的响应,
    /// 下次发同一 id 的请求(理论上不可能,因为每次 fetch_add)或后续
    /// 仍在等待这条 id 的调用方可立即取出。同时也防止 timeout 后旧响应
    /// 滞留 stdout buffer 让下一次请求 drain 时拿到错误数据。
    pending_responses: std::collections::HashMap<u64, JsonRpcResponse>,
}

/// `read_capped_line` 的返回结果。
enum ReadCappedOutcome {
    /// 读到一行(含末尾换行符,与 `read_line` 语义一致)。
    Line(String),
    /// 还没遇到换行符就超过字节上限。`n` 是已读字节数。
    TooLong(usize),
    /// 对端关闭连接(读到 EOF 且缓冲为空)。
    Eof,
}

/// 逐块读取一行,严格限制最大字节数,防止恶意 server 用无 `\n` 的大流触发 OOM。
///
/// 修复(C6,关键):原实现用 `reader.read_line()` 一次性填充 String 后才检查长度,
/// 顺序反了——`read_line` 无上限,内存会在检查触发前已爆炸。本函数用小块 `read`
/// 累积,每次检查累计长度,超限立即返回 `TooLong`。
///
/// - `max_bytes`:单行最大字节数(硬上限,内存占用 <= max_bytes + chunk)。
/// - `chunk`:每次 `read` 的缓冲大小。
/// - `line_timeout`:本行的总超时(覆盖所有 chunk),避免慢速 server 被误杀。
///
/// 修复(R10):原用固定 500ms 分块超时会误杀"逐字节慢速流式输出"的合法 MCP server
/// (CPU 密集型工具 chunk 间隔可能 >500ms)。改为整行总超时(由调用方按 mcp_timeout 传入),
/// 只要整行在总时限内读完即合法。
async fn read_capped_line<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    max_bytes: usize,
    chunk: usize,
    line_timeout: Duration,
) -> std::io::Result<ReadCappedOutcome> {
    use tokio::time::timeout;
    let mut buf: Vec<u8> = Vec::with_capacity(chunk.min(max_bytes));
    let mut tmp = vec![0u8; chunk];
    // 整行 deadline:覆盖所有 chunk 读累计时间,而非单 chunk 500ms。
    let line_deadline = tokio::time::Instant::now() + line_timeout;
    loop {
        // 检查上限:在 read 之前判断,确保内存不超 max_bytes。
        if buf.len() > max_bytes {
            return Ok(ReadCappedOutcome::TooLong(buf.len()));
        }
        let remaining = line_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "MCP read line timeout",
            ));
        }
        let n = match timeout(remaining, reader.read(&mut tmp[..])).await {
            Ok(Ok(0)) => {
                // EOF:若已读到内容,作为一行返回(无换行);否则 Eof。
                if buf.is_empty() {
                    return Ok(ReadCappedOutcome::Eof);
                }
                return Ok(ReadCappedOutcome::Line(
                    String::from_utf8_lossy(&buf).into_owned(),
                ));
            }
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e),
            // 整行总时限内未就绪,视为超时。
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "MCP read timeout",
                ));
            }
        };
        // 扫描本次读到的字节里有没有换行符。
        let segment = &tmp[..n];
        if let Some(pos) = segment.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&segment[..=pos]);
            // 已读到换行:返回。先检查是否超限(整行可能超)。
            if buf.len() > max_bytes {
                return Ok(ReadCappedOutcome::TooLong(buf.len()));
            }
            return Ok(ReadCappedOutcome::Line(
                String::from_utf8_lossy(&buf).into_owned(),
            ));
        }
        buf.extend_from_slice(segment);
    }
}

impl McpClient {
    /// 创建新的 MCP 客户端
    pub fn new(config: McpServerConfig) -> Self {
        Self {
            config,
            status: McpServerStatus::Disconnected,
            child: None,
            stdin: None,
            stdout_reader: None,
            tools: Vec::new(),
            server_info: None,
            next_id: 1,
            pending_responses: std::collections::HashMap::new(),
        }
    }

    /// 获取服务器名称
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// 获取当前连接状态
    pub fn status(&self) -> &McpServerStatus {
        &self.status
    }

    /// 获取已发现的工具列表
    pub fn tools(&self) -> &[McpToolInfo] {
        &self.tools
    }

    /// 获取服务器信息
    pub fn server_info(&self) -> Option<&Value> {
        self.server_info.as_ref()
    }

    /// 连接到 MCP 服务器并完成初始化握手
    pub async fn connect(&mut self) -> Result<()> {
        // 安全检查：验证 MCP 服务器命令和环境变量
        self.validate_config()?;

        self.status = McpServerStatus::Connecting;

        let mut cmd = Command::new(&self.config.command);
        // 修复(High #H8):原 stderr 设 Stdio::null(),MCP server 的崩溃堆栈/协议
        // 错误全部丢失,排障时只看到 stdout EOF,无法定位。改为 piped 并由独立 task
        // drain(避免管道满阻塞 server),尾部输出以 debug 日志记录。
        cmd.args(&self.config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // 修复(H2,关键):原实现不调 env_clear(),tokio::process::Command 默认继承父进程
        // **全部**环境变量。validate_config 的黑名单只过滤 config.env(用户显式配置的),
        // 看不到继承自父进程的 LD_PRELOAD/DEEPSEEK_API_KEY/MOVIX_* —— 这些会原样传给
        // MCP server 子进程。MCP server 本身无任何沙箱,等于把 API key 与可被劫持的
        // 动态库路径泄露给第三方进程。
        //
        // 正确做法:先 env_clear() 清空,再按白名单重建安全的最小环境:
        //   - PATH / HOME / USER / TMPDIR / LANG / LC_* :让 server 能找到二进制与临时目录。
        //   - SYSTEMROOT / TEMP / PATHEXT(Windows):Windows 上 spawn 必需。
        // 然后叠加 config.env(用户显式为该 server 配置的,已过 validate_config 黑名单)。
        cmd.env_clear();
        // 修复(R11):补充 Windows 进程必需与跨平台运行时变量,避免正常 MCP server
        // (Python/Node/Java 实现)因缺环境变量启动失败。
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
            // Windows 进程必需:SYSTEMROOT 缺失会让进程崩溃;USERPROFILE 是 HOME 的 Win 等价
            // (许多库读 ~/.config);COMSPEC 是 cmd.exe 路径(spawn 需要);WINDIR 与 SYSTEMROOT
            // 并存(某些库查 WINDIR)。
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
        // 显式配置的 env(已过 validate_config 黑名单)覆盖继承值。
        for (key, value) in &self.config.env {
            cmd.env(key, value);
        }

        // 修复：原实现 spawn/initialize/discover_tools 都没加超时，
        // 挂死的 MCP server 会让 agent 启动卡死。现在统一加 10s 超时。
        // 注：tokio::process::Command::spawn 本身是同步调用,这里通过 spawn_blocking + timeout 包裹,
        // 避免长时间阻塞在 fork/exec 上。
        //
        // 已知残余风险(R5/H8):若 timeout 触发时 spawn_blocking 内部的 cmd.spawn()
        // 仍在阻塞(罕见:NFS/慢 exec),该 blocking 任务无法取消,若它最终成功 spawn,
        // 子进程会变孤儿(无 Child 句柄可 kill)。完整修复需用 oneshot channel 把 Child
        // 回传并在 timeout 后 reap,复杂度高;此处接受该低概率残余。绝大多数 server 的
        // spawn() 是毫秒级,慢启动由后续 initialize-read 超时捕获(不会产生孤儿)。
        let server_name = self.config.name.clone();
        let spawn_handle = tokio::task::spawn_blocking(move || cmd.spawn());
        let spawn_result = timeout(mcp_timeout(), spawn_handle).await;

        let child = match spawn_result {
            Ok(Ok(child)) => child,
            Ok(Err(join_err)) => {
                return self.connect_fail(format!("spawn_blocking 失败: {}", join_err));
            }
            Err(_) => {
                return self.connect_fail(format!("MCP 服务器 '{}' 启动超时", server_name));
            }
        };

        let child = match child {
            Ok(c) => c,
            Err(e) => {
                return self
                    .connect_fail(format!("MCP 服务器 '{}' 启动失败: {}", self.config.name, e));
            }
        };

        self.child = Some(child);

        let child_ref = match self.child.as_mut() {
            Some(c) => c,
            None => return self.connect_fail("MCP 子进程未初始化".into()),
        };
        let stdin = match child_ref.stdin.take() {
            Some(s) => s,
            None => return self.connect_fail("无法获取 MCP 服务器 stdin".into()),
        };
        self.stdin = Some(stdin);

        let stdout = match child_ref.stdout.take() {
            Some(s) => s,
            None => return self.connect_fail("无法获取 MCP 服务器 stdout".into()),
        };
        self.stdout_reader = Some(BufReader::new(stdout));

        // 修复(High #H8):drain stderr 到 debug 日志,既保留诊断信息又避免管道满阻塞。
        if let Some(stderr) = child_ref.stderr.take() {
            let server_name_for_log = self.config.name.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut reader = BufReader::new(stderr).lines();
                let mut tail: Vec<String> = Vec::with_capacity(8);
                while let Ok(Some(line)) = reader.next_line().await {
                    tracing::debug!(
                        target: "mcp",
                        "[{}] stderr: {}",
                        server_name_for_log,
                        line
                    );
                    tail.push(line);
                    if tail.len() > 8 {
                        tail.remove(0);
                    }
                }
            });
        }

        // 修复(Bug #11):initialize/discover_tools 失败时如果只 ? 提前返回,
        // self.child 仍挂着,subprocess 泄漏直到 Drop 才回收。这里用闭包捕获,
        // 失败时主动 disconnect 清理。
        let init_result = self.initialize().await;
        if let Err(e) = init_result {
            self.disconnect().await;
            return self.connect_fail(format!("initialize 失败: {e}"));
        }

        let discover_result = self.discover_tools().await;
        if let Err(e) = discover_result {
            self.disconnect().await;
            return self.connect_fail(format!("discover_tools 失败: {e}"));
        }

        self.status = McpServerStatus::Connected;
        Ok(())
    }

    /// 验证 MCP 服务器配置的安全性。
    /// 1. 检查命令是否在危险命令黑名单中
    /// 2. 检查环境变量是否包含可提权的关键变量
    fn validate_config(&self) -> Result<()> {
        // 检查命令是否是 shell/下载工具 —— 这类命令几乎只可能用来"绕过 MCP 协议
        // 直接执行任意子进程",拒绝启动比 warn-only 更稳。
        // 修复:之前仅 `tracing::warn!` 后照常 spawn,意味着 `command="bash"` +
        // `args=["-c", "..."]` 这种组合只在 args 含 `-c` 时被拦,如果 LLM/恶意
        // 配置直接 `command="bash" args=["./evil.sh"]`,沙箱毫无作用。
        let command_lower = self.config.command.to_ascii_lowercase();
        let command_basename = command_lower
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&command_lower)
            .trim_end_matches(".exe");
        let dangerous_commands = [
            "bash",
            "sh",
            "zsh",
            "dash",
            "fish",
            "cmd",
            "powershell",
            "pwsh",
            "curl",
            "wget",
        ];
        if dangerous_commands.contains(&command_basename) {
            return Err(MovixError::Other(format!(
                "MCP 服务器 '{}' 使用 shell/下载工具 '{}' 作为入口命令,拒绝启动以防止任意命令执行",
                self.config.name, self.config.command
            )));
        }

        // 检查参数中是否包含 shell 执行标志
        let shell_flags = [
            "-c",
            "-e",
            "-Command",
            "-EncodedCommand",
            "-enc",
            "-NoProfile",
        ];
        for arg in &self.config.args {
            // 大小写不敏感比较(`-c` / `-C` 在 cmd.exe / pwsh 都生效)
            if shell_flags.iter().any(|&f| arg.eq_ignore_ascii_case(f)) {
                return Err(MovixError::Other(format!(
                    "MCP 服务器 '{}' 的参数包含 shell 执行标志 '{}'，拒绝执行以防止命令注入",
                    self.config.name, arg
                )));
            }
        }

        // 检查环境变量是否包含可提权/可影响动态加载的关键变量。
        // 修复：原列表把 `RUST_LOG` / `RUST_BACKTRACE` 也算进来,但这两者只影响子进程
        // (MCP server)自身的日志详尽度,既不会影响 movix 宿主,也不会改变可执行路径,
        // 反而把"用户希望开 verbose log 排查 MCP"这种合法用例堵死。这里只保留真正
        // 能影响动态链接、shebang、解释器查找的变量。
        // 修复:补齐常被滥用的"启动时执行任意代码"环境变量。
        // - PYTHONSTARTUP / PYTHONHOME / PYTHONUSERBASE:python 解释器启动钩子;
        // - RUBYOPT / RUBYLIB:ruby 启动参数;
        // - PERL5OPT / PERL5LIB / PERL5DB:perl 启动钩子;
        // - NODE_OPTIONS:node 启动参数(可注入 --require);
        // - LD_BIND_NOW / LD_AOUT_PRELOAD / LD_DEBUG_OUTPUT:动态链接相关;
        // - GIT_SSH_COMMAND:git 内部子进程钩子。
        let dangerous_env_keys = [
            "PATH",
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "LD_AUDIT",
            "LD_BIND_NOW",
            "LD_AOUT_PRELOAD",
            "LD_DEBUG_OUTPUT",
            "DYLD_INSERT_LIBRARIES",
            "DYLD_LIBRARY_PATH",
            "DYLD_FALLBACK_LIBRARY_PATH",
            "HOME",
            "USER",
            "SHELL",
            "IFS",
            "PYTHONPATH",
            "PYTHONSTARTUP",
            "PYTHONHOME",
            "PYTHONUSERBASE",
            "NODE_PATH",
            "NODE_OPTIONS",
            "RUBYOPT",
            "RUBYLIB",
            "PERL5OPT",
            "PERL5LIB",
            "PERL5DB",
            "GIT_SSH_COMMAND",
        ];
        for key in self.config.env.keys() {
            let key_upper = key.to_uppercase();
            if dangerous_env_keys.iter().any(|&dk| key_upper == dk) {
                return Err(MovixError::Other(format!(
                    "MCP 服务器 '{}' 的环境变量 '{}' 可能影响宿主进程安全，拒绝注入",
                    self.config.name, key
                )));
            }
        }

        Ok(())
    }

    /// 统一设置 Error 状态并返回 Err，用于 connect 各失败分支
    /// 修复(Bug #11):同时清理 child/stdin/stdout,避免 subprocess 泄漏。
    fn connect_fail(&mut self, msg: String) -> Result<()> {
        // 注意:这是同步函数,不能 await disconnect。但失败的 connect 通常
        // child 还没真正 spawn 成功(spawn 失败/超时分支),或上层调用方
        // 已经在 await disconnect()(initialize/discover 失败分支)。
        // 这里 best-effort 同步 kill 一次,真正的 reap 留给 Drop。
        if let Some(ref mut child) = self.child {
            let _ = child.start_kill();
        }
        self.child = None;
        self.stdin = None;
        self.stdout_reader = None;
        self.status = McpServerStatus::Error(msg.clone());
        Err(MovixError::Other(msg))
    }

    /// 发送 MCP 初始化请求
    async fn initialize(&mut self) -> Result<()> {
        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "clientInfo": {
                "name": "movix",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let response = self.send_request("initialize", Some(init_params)).await?;

        if let Some(error) = &response.error {
            self.status = McpServerStatus::Error(error.message.clone());
            return Err(MovixError::Other(format!(
                "MCP 初始化失败 [{}]: [code:{}] {}{}",
                self.config.name,
                error.code,
                error.message,
                error
                    .data
                    .as_ref()
                    .map_or(String::new(), |d| format!(" (data: {})", d))
            )));
        }

        self.server_info = response.result.clone();

        let _ = self
            .send_notification("notifications/initialized", None)
            .await;

        Ok(())
    }

    /// 发现服务器提供的工具
    async fn discover_tools(&mut self) -> Result<()> {
        let response = self
            .send_request("tools/list", Some(serde_json::json!({})))
            .await?;

        if let Some(error) = &response.error {
            return Err(MovixError::Other(format!(
                "MCP 工具发现失败 [{}]: [code:{}] {}",
                self.config.name, error.code, error.message
            )));
        }

        if let Some(result) = &response.result
            && let Some(tools_val) = result.get("tools")
        {
            let tools: Vec<McpToolInfo> =
                serde_json::from_value(tools_val.clone()).unwrap_or_else(|e| {
                    tracing::warn!(
                        "MCP {}: tools parse failed: {}; raw: {}",
                        self.config.name,
                        e,
                        tools_val
                    );
                    Vec::new()
                });
            self.tools = tools;
        }

        Ok(())
    }

    /// 调用 MCP 工具
    /// 修复(P2.3):添加指数退避重试机制。对瞬时错误(连接中断/超时)自动重试,
    /// 最多 3 次,退避间隔 1s → 2s → 4s。对业务错误(工具参数错误等)不重试。
    ///
    /// 修复(R5/H9,关键):tools/call 是 MCP 协议中唯一可能产生副作用的 RPC
    /// (initialize/list/ping 是幂等的)。若 server 已处理调用但在返回响应前超时,
    /// 重试会重复执行(发两封邮件、转两次账)。改为对 tools/call **不重试**:
    /// 首次失败即返回错误,由 agent 层决定是否让 LLM 重试(LLM 能看到首次的部分结果)。
    /// 重连逻辑仍保留给 initialize/list/ping(在各自方法内)。
    pub async fn call_tool(&mut self, tool_name: &str, arguments: Value) -> Result<McpToolResult> {
        match self.call_tool_inner(tool_name, arguments).await {
            Ok(result) => Ok(result),
            Err(e) => {
                // 不重试:tools/call 可能有副作用,重试不安全。
                // 仅在连接已断时尝试一次重连(为后续调用恢复),本次仍返回错误。
                if self.status != McpServerStatus::Connected {
                    tracing::info!(
                        "MCP 服务器 '{}' 调用 '{}' 失败且连接已断,尝试重连(本次调用不重试): {}",
                        self.config.name,
                        tool_name,
                        e
                    );
                    if let Err(reconn_err) = self.connect().await {
                        tracing::warn!("MCP 重连失败: {}", reconn_err);
                    }
                }
                Err(e)
            }
        }
    }

    /// 实际执行工具调用(不含重试逻辑)
    async fn call_tool_inner(
        &mut self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<McpToolResult> {
        let params = serde_json::json!({
            "name": tool_name,
            "arguments": arguments
        });

        let response = self.send_request("tools/call", Some(params)).await?;

        if let Some(error) = &response.error {
            return Err(MovixError::Other(format!(
                "MCP 工具调用失败 [{}.{}]: [code:{}] {}",
                self.config.name, tool_name, error.code, error.message
            )));
        }

        let result_val = response.result.unwrap_or(serde_json::json!({}));
        let is_error = result_val
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let content = result_val
            .get("content")
            .and_then(|v| serde_json::from_value::<Vec<McpContentBlock>>(v.clone()).ok())
            .unwrap_or_default();

        Ok(McpToolResult { content, is_error })
    }

    /// 发送 JSON-RPC 请求并等待响应
    async fn send_request(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<JsonRpcResponse> {
        // 修复(Bug #12):用每客户端 id 计数,不再用全局 AtomicU64;
        // 同时优先消费 pending_responses 缓存,避免上次 timeout 后的旧响应
        // 滞留 stdout buffer 被下一次请求误用。
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let id = self.next_id;

        // 先看缓存里是不是已经有这条 id 的响应(理论上不会,但做防御)
        if let Some(cached) = self.pending_responses.remove(&id) {
            return Ok(cached);
        }

        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };

        let mut request_str = serde_json::to_string(&request)
            .map_err(|e| MovixError::Other(format!("JSON 序列化失败: {}", e)))?;
        request_str.push('\n');

        // 修复(审查):写侧此前无超时。恶意/有 bug 的 MCP server 从不读 stdin 时,
        // 管道缓冲(~64KB)写满后 write_all 永久阻塞,`call_tool` 挂死且无取消路径。
        // 整个写 + flush 套上超时,对端不消费 stdin 时快速失败。
        let write_result = tokio::time::timeout(mcp_timeout(), async {
            let stdin = self
                .stdin
                .as_mut()
                .ok_or_else(|| MovixError::Other("MCP 服务器 stdin 不可用".into()))?;
            stdin
                .write_all(request_str.as_bytes())
                .await
                .map_err(|e| MovixError::Other(format!("写入 MCP 服务器失败: {}", e)))?;
            stdin
                .flush()
                .await
                .map_err(|e| MovixError::Other(format!("刷新 MCP stdin 失败: {}", e)))?;
            Ok::<(), MovixError>(())
        })
        .await;
        match write_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(MovixError::Other(
                    "写入 MCP 服务器超时:对端不读取 stdin(server 可能挂死)".into(),
                ));
            }
        }

        // 修复(E0596):reader 后续以 `&mut reader` 传入 read_capped_line。Rust 2024
        // edition 要求被可变借用的绑定必须显式声明为 `mut`(即使其本身已是 &mut 引用)。
        let mut reader = self
            .stdout_reader
            .as_mut()
            .ok_or_else(|| MovixError::Other("MCP 服务器 stdout 不可用".into()))?;

        let deadline = tokio::time::Instant::now() + mcp_timeout();

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(MovixError::Other(
                    "MCP read timeout waiting for matching response".into(),
                ));
            }

            // 修复(C6,关键):原实现用 `reader.read_line(&mut line)` 后才检查长度,顺序
            // 反了——`read_line` 无字节上限,恶意/有 bug 的 server 发一条无 `\n` 的大流,
            // String 在 8MB 检查触发**之前**已 OOM。注释声称防 OOM 实际是空壳。
            //
            // 正确做法:逐块 `read` 进缓冲,每块检查累计字节数,超限立即停止并丢弃该行。
            // 这样内存使用严格受限于 MAX_MCP_LINE_BYTES + 一个 chunk 的大小。
            const MAX_MCP_LINE_BYTES: usize = 8 * 1024 * 1024;
            const READ_CHUNK: usize = 16 * 1024;
            // 修复(R10):传整行总时限(本轮回合的剩余时间),替代旧的固定 500ms 分块超时,
            // 避免误杀慢速合法 server。
            let line = match read_capped_line(
                &mut reader,
                MAX_MCP_LINE_BYTES,
                READ_CHUNK,
                remaining,
            )
            .await
            {
                Ok(ReadCappedOutcome::Line(l)) => l,
                Ok(ReadCappedOutcome::TooLong(n)) => {
                    tracing::warn!(
                        target: "mcp",
                        "MCP 单行超过 {} 字节上限(读到 {} 字节仍无换行),丢弃以防 OOM",
                        MAX_MCP_LINE_BYTES, n
                    );
                    continue;
                }
                Ok(ReadCappedOutcome::Eof) => {
                    return Err(MovixError::Other("MCP server closed connection".into()));
                }
                Err(e) => {
                    // 超时被 read_capped_line 内部的 deadline 检测;其它 io 错误上抛。
                    if e.kind() == std::io::ErrorKind::TimedOut {
                        return Err(MovixError::Other(format!("MCP read timeout: {}", e)));
                    }
                    return Err(MovixError::Other(format!("MCP read error: {}", e)));
                }
            };

            let trimmed = line.trim();
            let response: JsonRpcResponse = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(_) => {
                    tracing::debug!(target: "mcp", "Skipping unparseable MCP line: {} chars", trimmed.len());
                    continue;
                }
            };

            if response.jsonrpc != "2.0" {
                return Err(MovixError::Other(format!(
                    "MCP protocol version mismatch: expected 2.0, got {}",
                    response.jsonrpc
                )));
            }

            match response.id {
                Some(resp_id) if resp_id == id => return Ok(response),
                Some(resp_id) => {
                    // 修复(Bug #12):暂存,而不是丢弃。下次有调用等这条 id
                    // 时可以立即返回(避免协议层"用旧响应应付新请求")。
                    // 缓存大小限制,避免对端 buggy 不停发不匹配响应导致内存膨胀。
                    if self.pending_responses.len() < 64 {
                        self.pending_responses.insert(resp_id, response);
                    } else {
                        tracing::warn!(target: "mcp", "pending_responses full, dropping id={}", resp_id);
                    }
                    continue;
                }
                None => {
                    tracing::debug!(target: "mcp", "Skipping MCP notification");
                    continue;
                }
            }
        }
    }

    /// 发送 JSON-RPC 通知（无响应）
    async fn send_notification(&mut self, method: &str, params: Option<Value>) -> Result<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        let mut notif_str = serde_json::to_string(&notification)
            .map_err(|e| MovixError::Other(format!("JSON 序列化失败: {}", e)))?;
        notif_str.push('\n');

        // 修复(审查):与 send_request 一致,写侧加超时,防止对端不读 stdin 时永久挂死。
        let write_result = tokio::time::timeout(mcp_timeout(), async {
            let stdin = self
                .stdin
                .as_mut()
                .ok_or_else(|| MovixError::Other("MCP 服务器 stdin 不可用".into()))?;
            stdin
                .write_all(notif_str.as_bytes())
                .await
                .map_err(|e| MovixError::Other(format!("写入 MCP 通知失败: {}", e)))?;
            stdin
                .flush()
                .await
                .map_err(|e| MovixError::Other(format!("刷新 MCP stdin 失败: {}", e)))?;
            Ok::<(), MovixError>(())
        })
        .await;
        match write_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(MovixError::Other(
                    "写入 MCP 通知超时:对端不读取 stdin(server 可能挂死)".into(),
                ));
            }
        }

        Ok(())
    }

    /// 断开与 MCP 服务器的连接
    pub async fn disconnect(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.child = None;
        self.stdin = None;
        self.stdout_reader = None;
        self.tools.clear();
        // 修复(Bug #12 续):重连后旧连接的残留响应必须清除,否则 send_request
        // 可能返回旧连接中缓存的陈旧响应。
        self.pending_responses.clear();
        // 修复(R5/H7,关键):原实现把 next_id 重置为 1。但重连后新请求从 id=1 重新开始,
        // 若旧连接在断开前曾向 pending_responses 缓存注入过低 id 的响应(或在 disconnect
        // 与 pending_responses.clear() 之间存在竞态),新请求 id=1 会命中旧缓存 →
        // 用旧响应顶替新请求。next_id 应在**客户端整个生命周期内单调递增**,不复位,
        // 从根本上避免跨连接的 id 复用。pending_responses.clear() 已保证无残留缓存。
        // (注:next_id 是 u64,实际不会溢出。)
        self.status = McpServerStatus::Disconnected;
    }

    /// 修复(P2.3):健康检查。发送 ping 请求检测服务器是否存活。
    /// 返回 true 表示服务器正常响应,false 表示需要重连。
    pub async fn health_check(&mut self) -> bool {
        // 先检查子进程是否还在运行
        if let Some(ref mut child) = self.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    tracing::warn!("MCP 服务器 '{}' 进程已退出: {}", self.config.name, status);
                    self.status = McpServerStatus::Error(format!("进程已退出: {}", status));
                    return false;
                }
                Ok(None) => {} // 进程仍在运行
                Err(e) => {
                    tracing::warn!("MCP 服务器 '{}' 进程状态检查失败: {}", self.config.name, e);
                    return false;
                }
            }
        } else {
            return false;
        }

        // 尝试发送 ping 请求
        match self.send_request("ping", Some(serde_json::json!({}))).await {
            Ok(response) => {
                if response.error.is_some() {
                    tracing::warn!(
                        "MCP 服务器 '{}' ping 返回错误: {:?}",
                        self.config.name,
                        response.error
                    );
                    false
                } else {
                    true
                }
            }
            Err(e) => {
                tracing::warn!("MCP 服务器 '{}' 健康检查失败: {}", self.config.name, e);
                self.status = McpServerStatus::Error(format!("健康检查失败: {}", e));
                false
            }
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // 修复(Bug #5 + Drop zombie):
        // - 原 Drop 仅 `start_kill()` 不 wait,zombie 进程依赖 movix 进程退出后
        //   交给 init 回收;在长生命周期 + 高频重连场景下,zombie 数会随时间增长。
        // - 现在:先关闭 stdin/stdout pipe,触发对端 EPIPE 让子进程主动退出;
        //   再 start_kill() 兜底;最后 spawn 一个一次性 tokio task 调用
        //   `child.wait()` 把 zombie 真正 reap 掉(若当前在 tokio runtime 中)。
        // 同步 Drop 不能 await,因此用 `tokio::spawn` 把 wait 推迟到 runtime 里。
        let _ = self.stdin.take();
        let _ = self.stdout_reader.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            // 修复(Low-mcp zombie):原实现在 runtime 关闭期间(tokio::spawn 的 task 不会执行)
            // 不 wait → 留 zombie。改进:先尝试 spawn 异步 wait;若无法 spawn(runtime 关闭/
            // 未绑定),用 `try_wait` 轮询最多 500ms 同步回收子进程,避免依赖 init。
            if tokio::runtime::Handle::try_current().is_ok() {
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
            } else {
                // 同步兜底:轮询 try_wait 短暂回收。子进程收到 SIGKILL 后通常很快退出。
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
                while std::time::Instant::now() < deadline {
                    match child.try_wait() {
                        Ok(Some(_)) => break, // 已回收
                        Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                        Err(_) => break, // wait 失败,放弃(进程已不在)
                    }
                }
            }
        }
    }
}

/// MCP 管理器，管理所有 MCP 服务器连接
///
/// 修复(P2.2):此前结构是 `Arc<Mutex<McpManager>>`,所有方法都需要拿
/// **整个** manager 的写锁。当一个 MCP 工具调用阻塞 30s 时(网络/IO),
/// 同一进程所有其他 MCP 服务器的 `call_tool` / `all_tools` /
/// `server_statuses` 全都被卡住,即使它们彼此互不相关。
///
/// 现在拆成两层:
///   - `clients`:`Arc<RwLock<HashMap<...>>>` —— 只在 `insert/remove`
///     (热更新配置时极少发生)需要写锁,读路径全部走读锁,瞬时完成。
///   - 单个 `McpClient` 自带 `AsyncMutex`,不同 server 的 stdin/stdout
///     管道天然隔离,互不阻塞。
///   - 同一 server 内串行(JSON-RPC 必须串行匹配 id),这一层用 client
///     自身的锁保证。
pub struct McpManager {
    clients: HashMap<String, Arc<AsyncMutex<McpClient>>>,
}

impl McpManager {
    /// 创建空的 MCP 管理器
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    /// 从配置列表创建并连接所有 MCP 服务器。
    /// 安全策略：仅自动连接 `trusted: true` 的服务器。
    /// 从工作区 .movix/mcp.json 文件加载的服务器默认 `trusted: false`，
    /// 需要用户在配置中显式设置 `"trusted": true` 才会自动连接。
    pub async fn from_configs(configs: Vec<McpServerConfig>) -> Self {
        let mut manager = Self::new();
        for config in configs {
            if !config.enabled {
                continue;
            }
            if !config.trusted {
                tracing::warn!(
                    "MCP 服务器 '{}' 未标记为受信任 (trusted: false)，跳过自动连接。\
                     如需连接，请在 .movix/mcp.json 中设置 \"trusted\": true",
                    config.name
                );
                continue;
            }
            let name = config.name.clone();
            let mut client = McpClient::new(config);
            match client.connect().await {
                Ok(()) => {
                    tracing::info!(
                        "MCP 服务器 '{}' 已连接，发现 {} 个工具",
                        name,
                        client.tools().len()
                    );
                    manager
                        .clients
                        .insert(name, Arc::new(AsyncMutex::new(client)));
                }
                Err(e) => {
                    tracing::warn!("MCP 服务器 '{}' 连接失败: {}", name, e);
                    // 连接失败的 client 不插入，避免后续 all_tools/call_tool 需要额外过滤
                }
            }
        }
        manager
    }

    /// 修复(P2.2):获取某个 server 的 client 句柄(Arc 克隆,无锁)。
    /// 调用方拿到后再各自 `.lock().await`,不会跨 server 串行。
    pub fn client_handle(&self, server_name: &str) -> Option<Arc<AsyncMutex<McpClient>>> {
        self.clients.get(server_name).cloned()
    }

    /// 获取所有已连接服务器的工具列表。
    /// 注意:为了不跨 await 持锁,先克隆所有 client 句柄,再逐一短锁取 tools。
    pub async fn all_tools(&self) -> Vec<(String, McpToolInfo)> {
        let handles: Vec<(String, Arc<AsyncMutex<McpClient>>)> = self
            .clients
            .iter()
            .map(|(name, c)| (name.clone(), Arc::clone(c)))
            .collect();
        let mut result = Vec::new();
        for (server_name, handle) in handles {
            let client = handle.lock().await;
            if matches!(client.status(), McpServerStatus::Connected) {
                for tool in client.tools() {
                    result.push((server_name.clone(), tool.clone()));
                }
            }
        }
        result
    }

    /// 调用指定服务器的工具。锁粒度为单个 client,其它 server 调用不受影响。
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<McpToolResult> {
        let handle = self
            .clients
            .get(server_name)
            .cloned()
            .ok_or_else(|| MovixError::Other(format!("MCP 服务器 '{}' 不存在", server_name)))?;
        let mut client = handle.lock().await;
        if !matches!(client.status(), McpServerStatus::Connected) {
            return Err(MovixError::Other(format!(
                "MCP 服务器 '{}' 未连接",
                server_name
            )));
        }
        client.call_tool(tool_name, arguments).await
    }

    /// 获取所有服务器的状态(短锁批量取快照,不阻塞 call_tool)。
    pub async fn server_statuses(&self) -> Vec<(String, McpServerStatus)> {
        let handles: Vec<(String, Arc<AsyncMutex<McpClient>>)> = self
            .clients
            .iter()
            .map(|(name, c)| (name.clone(), Arc::clone(c)))
            .collect();
        let mut out = Vec::with_capacity(handles.len());
        for (name, h) in handles {
            let c = h.lock().await;
            out.push((name, c.status().clone()));
        }
        out
    }

    /// 重新连接指定服务器(只锁该 server)。
    pub async fn reconnect(&self, server_name: &str) -> Result<()> {
        let handle = self
            .clients
            .get(server_name)
            .cloned()
            .ok_or_else(|| MovixError::Other(format!("MCP 服务器 '{}' 不存在", server_name)))?;
        let mut client = handle.lock().await;
        client.disconnect().await;
        client.connect().await
    }

    /// 断开所有服务器连接(逐个短锁,互不阻塞)。
    pub async fn disconnect_all(&self) {
        let handles: Vec<Arc<AsyncMutex<McpClient>>> = self.clients.values().cloned().collect();
        for h in handles {
            h.lock().await.disconnect().await;
        }
    }

    /// 获取已连接的服务器数量
    pub async fn connected_count(&self) -> usize {
        let mut count = 0;
        for h in self.clients.values() {
            if matches!(h.lock().await.status(), McpServerStatus::Connected) {
                count += 1;
            }
        }
        count
    }

    /// 获取服务器总数(无锁)
    pub fn total_count(&self) -> usize {
        self.clients.len()
    }

    /// 获取已发现工具的总数
    pub async fn total_tools(&self) -> usize {
        let mut total = 0;
        for h in self.clients.values() {
            let c = h.lock().await;
            if matches!(c.status(), McpServerStatus::Connected) {
                total += c.tools().len();
            }
        }
        total
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 从环境变量解析 MCP 服务器配置
/// 格式: MOVIX_MCP_SERVERS=name1:command:arg1,arg2;name2:command2:arg1
/// 或使用 JSON 格式: MOVIX_MCP_CONFIG=<json>
pub fn parse_mcp_configs_from_env() -> Vec<McpServerConfig> {
    if let Ok(json_str) = std::env::var("MOVIX_MCP_CONFIG") {
        match serde_json::from_str::<Vec<McpServerConfig>>(&json_str) {
            Ok(configs) => return configs,
            Err(e) => {
                tracing::warn!("MOVIX_MCP_CONFIG JSON 解析失败: {}", e);
            }
        }
    }

    if let Ok(servers_str) = std::env::var("MOVIX_MCP_SERVERS") {
        return parse_simple_mcp_config(&servers_str);
    }

    Vec::new()
}

/// 解析简单格式的 MCP 配置
/// 格式: name:command:arg1,arg2;name2:command2
fn parse_simple_mcp_config(s: &str) -> Vec<McpServerConfig> {
    let mut configs = Vec::new();

    for server_def in s.split(';') {
        let parts: Vec<&str> = server_def.splitn(3, ':').collect();
        if parts.len() < 2 {
            continue;
        }

        let name = parts[0].trim().to_string();
        let command = parts[1].trim().to_string();
        let args = if parts.len() > 2 {
            parts[2]
                .split(',')
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty())
                .collect()
        } else {
            Vec::new()
        };

        configs.push(McpServerConfig {
            name,
            command,
            args,
            env: HashMap::new(),
            enabled: true,
            trusted: true, // 环境变量配置默认受信任
        });
    }

    configs
}

/// 从配置文件目录读取 MCP 配置
pub fn load_mcp_config_from_file(workspace: &Path) -> Vec<McpServerConfig> {
    let config_path = workspace.join(".movix").join("mcp.json");

    if !config_path.exists() {
        return Vec::new();
    }

    match std::fs::read_to_string(&config_path) {
        Ok(content) => {
            let configs: Vec<McpServerConfig> = match serde_json::from_str(&content) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("MCP 配置文件解析失败 {}: {}", config_path.display(), e);
                    return Vec::new();
                }
            };
            configs
        }
        Err(e) => {
            tracing::warn!("读取 MCP 配置文件失败 {}: {}", config_path.display(), e);
            Vec::new()
        }
    }
}

/// 合并所有来源的 MCP 配置（文件 > 环境变量）
///
/// 修复(审查,Blocker):此前工作区 `.movix/mcp.json` 里的 `"trusted": true` 由文件
/// 自证,`from_configs` 直接静默 spawn 其 `command` → 克隆恶意仓库即任意代码执行,
/// 且同名文件配置还会**覆盖**用户环境变量里配好的受信服务器。
///
/// 现在:文件来源的配置一律视为不可信,**除非**用户显式设置 `MOVIX_TRUST_MCP_FILE=1`
/// (该变量由用户在自己的 shell 环境设置,恶意仓库无法伪造)。未授权时跳过文件配置,
/// 且不覆盖用户的环境变量配置。
pub fn collect_mcp_configs(workspace: &Path) -> Vec<McpServerConfig> {
    let mut configs = Vec::new();

    let env_configs = parse_mcp_configs_from_env();
    configs.extend(env_configs);

    let trust_file = std::env::var("MOVIX_TRUST_MCP_FILE")
        .map(|v| v == "1")
        .unwrap_or(false);
    let file_configs = load_mcp_config_from_file(workspace);
    for fc in file_configs {
        if !trust_file {
            tracing::warn!(
                "工作区 .movix/mcp.json 中的 MCP 服务器 '{}' 默认不受信,已跳过。\
                 该文件可被恶意仓库篡改以自证 trusted:true;如确认信任,请在 shell 环境设置 MOVIX_TRUST_MCP_FILE=1",
                fc.name
            );
            continue;
        }
        if let Some(existing) = configs.iter_mut().find(|c| c.name == fc.name) {
            *existing = fc;
        } else {
            configs.push(fc);
        }
    }

    configs
}
