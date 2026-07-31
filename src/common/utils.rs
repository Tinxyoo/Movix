use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Format a byte count as a human-readable size.
pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    format!("{:.1} {}", size, UNITS[unit_idx])
}

/// Join a user path to the workspace while preventing traversal and symlink escape.
///
/// Existing path components are canonicalized before the final path is accepted. This
/// matters for writes to new files under a symlinked directory.
///
/// # TOCTOU 注意
/// 在检查路径存在性（canonicalize）和后续使用之间存在理论上的 TOCTOU 竞态：
/// 攻击者可在检查后、使用前创建指向工作区外的符号链接。实际利用需并发文件系统访问，
/// 在单 Agent 场景下风险极低。若需完全消除，需在文件系统层面（如 openat2 + RESOLVE_BENEATH）
/// 做路径解析，当前 Rust 标准库和 Windows 平台暂不支持。
pub fn safe_join_path(workspace: &str, relative: &str) -> Result<PathBuf, String> {
    let base = PathBuf::from(workspace);
    let base_canon = base
        .canonicalize()
        .map_err(|e| format!("workspace does not exist or is not accessible: {}", e))?;

    let input = PathBuf::from(relative);
    let lexical = if input.is_absolute() {
        normalize_path(&input)?
    } else {
        normalize_path(&base_canon.join(input))?
    };

    if !lexical.starts_with(&base_canon) {
        return Err(format!("path escapes workspace: {}", relative));
    }

    let mut existing = lexical.clone();
    let mut missing = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(|s| s.to_os_string()) else {
            return Err(format!("path escapes workspace: {}", relative));
        };
        missing.push(name);
        if !existing.pop() {
            return Err(format!("path escapes workspace: {}", relative));
        }
    }

    let existing_canon = existing
        .canonicalize()
        .map_err(|e| format!("failed to resolve existing path: {}", e))?;
    if !existing_canon.starts_with(&base_canon) {
        return Err(format!(
            "path escapes workspace through symlink: {}",
            relative
        ));
    }

    // 修复(C4,关键):原实现把 `missing`(尚未存在的路径段)直接 push 到已 canonicalize
    // 的 existing_canon 之后,不再校验。攻击链:模型先用 write_file 在工作区创建符号链接
    // `link -> /etc`,再 write_file `link/passwd` —— resolve 时 existing_canon=工作区根(存在)
    // → 返回 `<工作区>/link/passwd` → 实际写入穿过符号链接到 /etc/passwd。
    //
    // 正确做法:逐段向下推进,每推进一段就 canonicalize 当前已存在的部分;一旦遇到符号链接,
    // canonicalize 会跟随它,得到真实路径,再做 workspace confinement 检查。对"不存在"的
    // 末尾段(真正要新建的文件),无法 canonicalize,但其父链已被逐段校验,符号链接无法藏身。
    let mut resolved = existing_canon;
    for part in missing.iter().rev() {
        resolved.push(part);
        // 如果当前路径已存在(可能是符号链接指向工作区外),必须 canonicalize 重新校验。
        // 不存在的末尾段会跳过——但此时其所有已存在的祖先都已被校验过。
        if let Ok(canon) = resolved.canonicalize() {
            if !canon.starts_with(&base_canon) {
                return Err(format!(
                    "path escapes workspace through symlink in missing segment: {}",
                    relative
                ));
            }
            resolved = canon;
        }
    }
    Ok(resolved)
}

fn normalize_path(path: &Path) -> Result<PathBuf, String> {
    let mut out = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                if !out.pop() {
                    return Err(format!("path escapes workspace: {}", path.display()));
                }
            }
        }
    }

    Ok(out)
}

/// Return whether this binary was compiled for Windows.
pub fn is_windows() -> bool {
    cfg!(target_os = "windows")
}

/// 截断字符串到指定最大字符数，保证在 UTF-8 字符边界处截断。
/// 如果截断发生，会在末尾追加省略号 `…`。
pub fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars || max_chars == 0 {
        // 修复(边界):max_chars=0 不能输出纯省略号,返回 "-" 表示无内容。
        return if max_chars == 0 {
            "-".into()
        } else {
            s.to_string()
        };
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{}…", truncated)
}

/// 向前回退到最近的 UTF-8 字符边界。
pub fn previous_char_boundary(s: &str, mut index: usize) -> usize {
    index = index.min(s.len());
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// 写入文件并限制权限(Unix 上 0600)。用于含密钥/敏感内容的文件
/// (`.env`、会话历史等),避免多用户机器上明文可读。
///
/// 修复(审查):`fs::write` 使用默认 umask(通常 0644),会话历史含 agent 看过的
/// 文件内容,同机其他用户可读;`.env` 先 0644 再 chmod 也存在竞态窗口。
/// 这里创建时直接设 0600,无中间状态。
pub fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true).mode(0o600);
        let mut f = opts.open(path)?;
        f.write_all(contents.as_bytes())?;
        f.flush()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

/// 原子写入文件(同步版本):写到同目录临时文件 + fsync + rename。
///
/// 修复(Critical #C10):memory.rs / skills.rs 此前用裸 `fs::write`,进程在写入中途
/// 崩溃(SIGKILL/掉电/OOM)会留下截断的半截损坏文件,下次解析得到残缺数据。
/// rename 在同目录、同文件系统上是原子的,保证目标文件要么是完整旧内容、要么是
/// 完整新内容,不会出现中间态。
///
/// 临时文件名带 PID + 纳秒时间戳,避免并发写时互相覆盖。
pub fn atomic_write(target: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target has no parent")
    })?;
    // 确保父目录存在(memory/skill 的父目录通常已存在,这里兜底)。
    std::fs::create_dir_all(parent)?;

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

    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.flush()?;
        // 尽力 fsync;不支持时忽略。
        let _ = f.sync_all();
    }
    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// 获取用户 HOME 目录。
/// 修复(P1.3):使用 `dirs` crate 提供的标准实现,替代手动 env var + 静默降级到 "."。
/// 在容器/CI 环境中,手动 fallback 到 "." 会导致配置文件写到工作目录。
pub fn home_dir() -> std::path::PathBuf {
    dirs::home_dir().unwrap_or_else(|| {
        tracing::warn!("无法检测 HOME 目录,回退到当前目录。请设置 HOME 环境变量。");
        std::path::PathBuf::from(".")
    })
}

/// 估算字符串的 token 数。
///
/// 修复(P1.1):当 `accurate-tokens` feature 启用时,使用 tiktoken-rs 进行
/// 精确 BPE tokenization(误差 <1%);否则回退到字符级启发式估算(误差 ±15%)。
///
/// 对于 1M token 上下文窗口,±15% 误差意味着 ±150K tokens 的偏差,
/// 可能导致 API 请求被截断或过早压缩上下文。生产环境建议启用 accurate-tokens。
///
/// ⚠️ 已知偏差(G-C3):`accurate-tokens` 用的是 OpenAI `cl100k_base`,而 DeepSeek V4
/// 用自己的 BPE tokenizer,词表与切分策略不同。对中文/代码的系统性偏差可能远超 15%
/// (DeepSeek tokenizer 对中文通常产出更少 token)。这会影响压缩触发时机与成本估算。
/// 缓解:API 返回的 `usage` 字段是真实 token 计数,`cached_stats` 用它校正;
/// 本地估算仅作"下次请求前"的预测。未来应接入 DeepSeek 官方 tokenizer。
pub fn estimate_tokens_str(s: &str) -> usize {
    #[cfg(feature = "accurate-tokens")]
    {
        use tiktoken_rs::cl100k_base;
        if let Ok(bpe) = cl100k_base() {
            return bpe.encode_with_special_tokens(s).len();
        }
        // tiktoken 初始化失败时回退到启发式
        tracing::warn_once!("tiktoken 初始化失败,回退到启发式估算");
    }

    heuristic_estimate_tokens(s)
}

/// 字符级启发式 token 估算,按 CJK/ASCII/标点细分。
/// 误差约 ±15%,仅作为 tiktoken 不可用时的 fallback。
fn heuristic_estimate_tokens(s: &str) -> usize {
    const TOKENS_PER_CJK_CHAR: f64 = 0.70;
    const TOKENS_PER_ASCII_WORD_CHAR: f64 = 0.25;
    const TOKENS_PER_PUNCT_CHAR: f64 = 0.25;

    let mut cjk: usize = 0;
    let mut word: usize = 0;
    let mut punct: usize = 0;
    for ch in s.chars() {
        let code = ch as u32;
        if (0x2E80..=0x9FFF).contains(&code)
            || (0x3400..=0x4DBF).contains(&code)
            || (0xF900..=0xFAFF).contains(&code)
        {
            cjk += 1;
        } else if ch.is_alphanumeric() {
            word += 1;
        } else if ch.is_whitespace() {
            // whitespace merged by BPE
        } else {
            punct += 1;
        }
    }
    (cjk as f64 * TOKENS_PER_CJK_CHAR
        + word as f64 * TOKENS_PER_ASCII_WORD_CHAR
        + punct as f64 * TOKENS_PER_PUNCT_CHAR) as usize
}

/// 同步运行命令并等待完成，超时后强制终止。
///
/// 修复(Critical #C7 管道死锁):原实现 spawn 后只 `try_wait` 轮询,**从不读取
/// stdout/stderr**。当子进程输出超过 OS 管道缓冲(~64KB)时,write 阻塞,而本函数
/// 在 try_wait 循环里等它退出 → 经典管道死锁。中大型 Rust 项目的 `cargo check`
/// 输出极易超 64KB,导致永久挂起直到超时,而 verify_loop 又把超时当"通过",
/// 编译错误被静默放行。
///
/// 正确做法:用两个独立线程并发 drain stdout/stderr 管道,主线程 wait。
/// 这样子进程的 write 不会因管道满而阻塞。
pub fn run_command_with_timeout(
    mut command: Command,
    timeout_secs: u64,
) -> std::io::Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;

    // 取出管道,交给独立线程 drain,避免主线程 wait 时管道写阻塞。
    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");

    let stdout_handle = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut s = stdout_pipe;
        let _ = s.read_to_end(&mut buf);
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut s = stderr_pipe;
        let _ = s.read_to_end(&mut buf);
        buf
    });

    let timeout = Duration::from_secs(timeout_secs.max(1));
    let started = Instant::now();

    // 轮询子进程退出;期间管道已被 drain 线程持续消费,不会死锁。
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            // 等 drain 线程结束(管道关闭后它们会退出)。
            let _ = child.wait();
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("command timed out after {} seconds", timeout_secs.max(1)),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// 清理工作区内残留的 atomic_write 临时文件。
///
/// 修复(G-M3):atomic_write 创建 `.{name}.movix.tmp.{pid}.{nanos}` 临时文件,
/// 若进程在 rename 前 SIGKILL/掉电,临时文件永久残留,污染 list_dir/grep 结果。
/// 启动时调用本函数扫描工作区,删除孤儿 `.movix.tmp.*` 文件。
pub fn cleanup_stale_atomic_tmp(workspace: &Path) {
    let mut visited = 0u32;
    let mut cleaned = 0u32;
    fn walk(dir: &Path, visited: &mut u32, cleaned: &mut u32) {
        if *visited > 50_000 {
            return; // 安全上限,防恶意深目录
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            *visited += 1;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // 匹配 atomic_write 的临时文件命名:`.<file>.movix.tmp.<pid>.<nanos>`
            if name.starts_with('.') && name.contains(".movix.tmp.") {
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::debug!(target: "utils", "无法清理残留 tmp {}: {}", path.display(), e);
                } else {
                    *cleaned += 1;
                }
                continue;
            }
            // 跳过常见的非源码目录,加速扫描
            if matches!(
                name,
                "target"
                    | "node_modules"
                    | ".git"
                    | ".movix-snapshots"
                    | "__pycache__"
                    | "dist"
                    | "build"
            ) {
                continue;
            }
            if path.is_dir() {
                walk(&path, visited, cleaned);
            }
        }
    }
    walk(workspace, &mut visited, &mut cleaned);
    if cleaned > 0 {
        tracing::info!(
            target: "utils",
            "启动清理:删除 {} 个残留 atomic_write 临时文件(扫描 {} 项)",
            cleaned,
            visited
        );
    }
}

/// 通用拓扑排序分层算法。
/// 给定节点 ID 列表和依赖关系，返回按层级排列的节点 ID 列表（同一层级可并行执行）。
/// `dep_map`: 每个节点 ID 到其依赖的节点 ID 列表的映射。
pub fn topological_layers<T: Clone + std::hash::Hash + Eq + std::fmt::Debug>(
    nodes: &[T],
    dep_map: &HashMap<T, Vec<T>>,
) -> Vec<Vec<T>> {
    let mut layers: Vec<Vec<T>> = Vec::new();
    let mut completed: HashSet<T> = HashSet::new();
    let mut remaining: HashSet<T> = nodes.iter().cloned().collect();

    while !remaining.is_empty() {
        let mut layer = Vec::new();
        let mut to_remove = Vec::new();

        for node in &remaining {
            let deps = dep_map.get(node).cloned().unwrap_or_default();
            if deps.iter().all(|d| completed.contains(d)) {
                layer.push(node.clone());
                to_remove.push(node.clone());
            }
        }

        // 防止死循环：如果没有节点可以推进（存在循环依赖或悬空依赖），
        // 把剩余全部放入当前层（降级为并行执行）。
        // 修复(Medium #M10):原实现静默吞掉循环依赖,LLM 生成的环依赖无任何提示。
        // 这里记录 warning 列出卡住的节点,便于排查。
        if layer.is_empty() {
            tracing::warn!(
                target: "planning",
                "检测到循环/悬空依赖,{} 个节点无法按拓扑序推进,降级为并行执行: {:?}",
                remaining.len(),
                remaining.iter().collect::<Vec<_>>()
            );
            layer.extend(remaining.iter().cloned());
            to_remove.extend(remaining.iter().cloned());
        }

        for node in &to_remove {
            completed.insert(node.clone());
            remaining.remove(node);
        }

        layers.push(layer);
    }

    layers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0.0 B");
        assert_eq!(format_size(1023), "1023.0 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1048576), "1.0 MB");
    }

    #[test]
    fn test_safe_join_path_rejects_traversal() {
        let cwd = std::env::current_dir().unwrap();
        let result = safe_join_path(&cwd.to_string_lossy(), "../../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn test_safe_join_path_accepts_normal() {
        let result = safe_join_path(".", "src/main.rs");
        assert!(result.is_ok());
    }

    #[test]
    fn test_safe_join_path_accepts_new_file_under_workspace() {
        let result = safe_join_path(".", "src/new_file_that_does_not_exist.rs");
        assert!(result.is_ok());
        let workspace = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert!(result.unwrap().starts_with(workspace));
    }

    #[test]
    fn test_truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_str_long() {
        let result = truncate_str("hello world", 5);
        assert_eq!(result, "hello…");
    }

    #[test]
    fn test_previous_char_boundary() {
        let s = "你好世界";
        assert_eq!(previous_char_boundary(s, 5), 3);
        assert_eq!(previous_char_boundary(s, 0), 0);
        assert_eq!(previous_char_boundary(s, 100), s.len());
    }

    #[test]
    fn test_home_dir() {
        let home = home_dir();
        assert!(!home.as_os_str().is_empty());
    }

    #[test]
    fn test_estimate_tokens_str() {
        // ASCII text: ~0.25 tokens per char
        let ascii = "hello world";
        let tokens = estimate_tokens_str(ascii);
        assert!(tokens > 0);
        // CJK text: ~0.7 tokens per char, more tokens per char than ASCII
        let cjk = "你好世界你好世界你好世界"; // 12 CJK chars
        let cjk_tokens = estimate_tokens_str(cjk);
        let ascii_long = "a".repeat(12);
        let ascii_tokens_long = estimate_tokens_str(&ascii_long);
        assert!(cjk_tokens > ascii_tokens_long);
    }
}
