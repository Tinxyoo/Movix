use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::warn;

use crate::common::utils;

const MAX_DIAGNOSTICS_PER_FILE: usize = 50;
const LSP_CHECK_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

impl DiagnosticSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
            Self::Hint => "hint",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspKind {
    RustAnalyzer,
    Pyright,
    TypeScriptServer,
    Generic,
}

impl LspKind {
    pub fn detect_for_file(path: &Path) -> Option<Self> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext {
            "rs" => Some(Self::RustAnalyzer),
            "py" => Some(Self::Pyright),
            "ts" | "tsx" | "js" | "jsx" => Some(Self::TypeScriptServer),
            _ => None,
        }
    }

    pub fn command(&self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust-analyzer",
            Self::Pyright => "pyright",
            Self::TypeScriptServer => "tsserver",
            Self::Generic => "echo",
        }
    }
}

pub struct LspDiagnostics {
    diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
    enabled: bool,
}

impl LspDiagnostics {
    pub fn new(enabled: bool) -> Self {
        Self {
            diagnostics: HashMap::new(),
            enabled,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn check_file(&mut self, path: &Path) -> Vec<Diagnostic> {
        if !self.enabled {
            return Vec::new();
        }

        let Some(kind) = LspKind::detect_for_file(path) else {
            return Vec::new();
        };

        let diags = self.run_check(path, &kind);
        if !diags.is_empty() {
            // 修复(M4,关键):原实现 diagnostics HashMap 只增不减,无上限/LRU/失效。
            // 长会话检查过 N 个不同文件后,map 累积 N 个条目(每个最多 50 条 Diagnostic),
            // 永不释放,造成无界内存增长。这里加容量上限,超限时按"最近最少访问"启发式
            // 驱逐(简单策略:随机/首个,因 HashMap 无序;保留最近插入的一半)。
            const MAX_DIAGNOSTIC_FILES: usize = 200;
            if self.diagnostics.len() >= MAX_DIAGNOSTIC_FILES {
                // 保留最近插入的一半,丢弃另一半(HashMap 无序,近似 LRU)。
                let keep = MAX_DIAGNOSTIC_FILES / 2;
                let keys_to_remove: Vec<PathBuf> =
                    self.diagnostics.keys().skip(keep).cloned().collect();
                for k in keys_to_remove {
                    self.diagnostics.remove(&k);
                }
            }
            self.diagnostics.insert(path.to_path_buf(), diags.clone());
        } else {
            // 文件已无诊断时,移除其旧条目(避免 stale 数据)。
            self.diagnostics.remove(path);
        }
        diags
    }

    fn run_check(&self, path: &Path, kind: &LspKind) -> Vec<Diagnostic> {
        match kind {
            LspKind::RustAnalyzer => self.check_rust(path),
            LspKind::Pyright => self.check_python(path),
            LspKind::TypeScriptServer => self.check_typescript(path),
            LspKind::Generic => Vec::new(),
        }
    }

    fn check_rust(&self, path: &Path) -> Vec<Diagnostic> {
        // 修复(审查):原实现不设 current_dir,继承进程启动时 cwd —— `movix -w 工作区`
        // 启动时进程 cwd 是用户 shell 所在目录,cargo 在错误目录跑找不到 Cargo.toml。
        // 以被检查文件的父目录为工作目录(cargo 会向上搜索 manifest)。
        let cwd = path.parent().unwrap_or_else(|| Path::new("."));
        let output = if utils::is_windows() {
            let mut command = Command::new("cmd.exe");
            command
                .args(["/C", "cargo check --message-format=short"])
                .current_dir(cwd);
            utils::run_command_with_timeout(command, LSP_CHECK_TIMEOUT_SECS)
        } else {
            let mut command = Command::new("sh");
            command
                .args(["-c", "cargo check --message-format=short"])
                .current_dir(cwd);
            utils::run_command_with_timeout(command, LSP_CHECK_TIMEOUT_SECS)
        };

        let mut diags = Vec::new();
        match output {
            Ok(output) if !output.status.success() => {
                let combined = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                for line in combined.lines().take(MAX_DIAGNOSTICS_PER_FILE) {
                    if let Some(diag) = parse_rust_diagnostic(line) {
                        diags.push(diag);
                    }
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                warn!(target: "lsp", "cargo check timed out after {}s", LSP_CHECK_TIMEOUT_SECS);
            }
            Err(e) => {
                warn!(target: "lsp", "cargo check failed to start: {}", e);
            }
        }
        diags
    }

    fn check_python(&self, path: &Path) -> Vec<Diagnostic> {
        let mut command = Command::new("pyright");
        command.arg(path).args(["--outputjson"]);
        let output = utils::run_command_with_timeout(command, LSP_CHECK_TIMEOUT_SECS);

        let mut diags = Vec::new();
        match output {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout)
                    && let Some(diagnostics) =
                        json.get("generalDiagnostics").and_then(|d| d.as_array())
                {
                    for entry in diagnostics.iter().take(MAX_DIAGNOSTICS_PER_FILE) {
                        let severity = match entry.get("severity").and_then(|s| s.as_str()) {
                            Some("error") => DiagnosticSeverity::Error,
                            Some("warning") => DiagnosticSeverity::Warning,
                            Some("information") => DiagnosticSeverity::Info,
                            _ => DiagnosticSeverity::Hint,
                        };
                        diags.push(Diagnostic {
                            file: path.to_path_buf(),
                            line: entry
                                .get("range")
                                .and_then(|r| r.get("start"))
                                .and_then(|s| s.get("line"))
                                .and_then(|l| l.as_u64())
                                .unwrap_or(0) as u32
                                + 1,
                            column: entry
                                .get("range")
                                .and_then(|r| r.get("start"))
                                .and_then(|s| s.get("character"))
                                .and_then(|c| c.as_u64())
                                .unwrap_or(0) as u32
                                + 1,
                            severity,
                            message: entry
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("")
                                .to_string(),
                            source: "pyright".to_string(),
                        });
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                warn!(target: "lsp", "pyright timed out after {}s", LSP_CHECK_TIMEOUT_SECS);
            }
            Err(e) => {
                warn!(target: "lsp", "pyright failed to start: {}", e);
            }
        }
        diags
    }

    fn check_typescript(&self, path: &Path) -> Vec<Diagnostic> {
        let mut command = Command::new("npx");
        command
            .args(["tsc", "--noEmit", "--pretty", "false"])
            .arg(path);
        let output = utils::run_command_with_timeout(command, LSP_CHECK_TIMEOUT_SECS);

        let mut diags = Vec::new();
        match output {
            Ok(output) => {
                let combined = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                for line in combined.lines().take(MAX_DIAGNOSTICS_PER_FILE) {
                    if let Some(diag) = parse_tsc_diagnostic(line) {
                        diags.push(diag);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                warn!(target: "lsp", "tsc timed out after {}s", LSP_CHECK_TIMEOUT_SECS);
            }
            Err(e) => {
                warn!(target: "lsp", "tsc failed to start: {}", e);
            }
        }
        diags
    }

    pub fn get_diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        self.diagnostics.get(path).cloned().unwrap_or_default()
    }

    pub fn format_diagnostics(&self, path: &Path) -> Option<String> {
        let diags = self.diagnostics.get(path)?;
        if diags.is_empty() {
            return None;
        }

        let mut blocks = Vec::new();
        let display = path.display();

        let errors = diags
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Error)
            .count();
        let warnings = diags
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Warning)
            .count();

        blocks.push(format!(
            "<diagnostics file=\"{}\" errors=\"{}\" warnings=\"{}\">",
            display, errors, warnings
        ));

        for diag in diags.iter().take(MAX_DIAGNOSTICS_PER_FILE) {
            blocks.push(format!(
                "  <{} line=\"{}\" col=\"{}\" source=\"{}\">{}</{}>",
                diag.severity.as_str(),
                diag.line,
                diag.column,
                diag.source,
                diag.message,
                diag.severity.as_str(),
            ));
        }

        blocks.push("</diagnostics>".to_string());
        Some(blocks.join("\n"))
    }

    pub fn clear(&mut self, path: &Path) {
        self.diagnostics.remove(path);
    }

    pub fn clear_all(&mut self) {
        self.diagnostics.clear();
    }

    pub fn total_errors(&self) -> usize {
        self.diagnostics
            .values()
            .flat_map(|v| v.iter())
            .filter(|d| d.severity == DiagnosticSeverity::Error)
            .count()
    }

    pub fn total_warnings(&self) -> usize {
        self.diagnostics
            .values()
            .flat_map(|v| v.iter())
            .filter(|d| d.severity == DiagnosticSeverity::Warning)
            .count()
    }
}

fn parse_rust_diagnostic(line: &str) -> Option<Diagnostic> {
    // 修复: cargo check --message-format=short 输出格式为
    //   src/main.rs:10:5: error[E0308]: mismatched types
    // 原 splitn(3, ':') 把 "5: error[E0308]..." 粘在 parts[2],
    // 无法解析 col→u32,导致所有诊断静默丢弃。改为 splitn(4, ':')。
    let parts: Vec<&str> = line.splitn(4, ':').collect();
    if parts.len() < 4 {
        return None;
    }

    let file = PathBuf::from(parts.first()?.trim());
    let line_num: u32 = parts.get(1)?.trim().parse().ok()?;
    let col: u32 = parts.get(2)?.trim().parse().ok()?;

    let rest = parts.get(3)?;
    let (severity, message) = if rest.contains("error") {
        (DiagnosticSeverity::Error, rest.trim().to_string())
    } else if rest.contains("warning") {
        (DiagnosticSeverity::Warning, rest.trim().to_string())
    } else {
        return None;
    };

    Some(Diagnostic {
        file,
        line: line_num,
        column: col,
        severity,
        message: message.trim().to_string(),
        source: "rustc".to_string(),
    })
}

fn parse_tsc_diagnostic(line: &str) -> Option<Diagnostic> {
    // 修复(审查):tsc --pretty false 输出格式是 `path/to/file.ts(5,2): error TS2322: msg`
    // (行/列在**圆括号**里),原实现按 `:` 切分并假定 parts[1]/[2] 是行列 → 实际拿到
    // 的是 " error TS2322" 与 " Type ...",parse::<u32> 失败 → 所有 TS 诊断被静默丢弃。
    // 改为先提取 `(line,col)`。
    let open = line.find('(')?;
    let close = line[open..].find(')')? + open;
    let file = PathBuf::from(line[..open].trim());
    let loc = &line[open + 1..close];
    let (line_str, col_str) = loc.split_once(',')?;
    let line_num: u32 = line_str.trim().parse().ok()?;
    let col: u32 = col_str.trim().parse().ok()?;
    let rest = line[close + 1..].trim();

    let severity = if rest.contains("error") {
        DiagnosticSeverity::Error
    } else if rest.contains("warning") {
        DiagnosticSeverity::Warning
    } else {
        DiagnosticSeverity::Info
    };

    Some(Diagnostic {
        file,
        line: line_num,
        column: col,
        severity,
        message: rest.to_string(),
        source: "tsc".to_string(),
    })
}

impl Default for LspDiagnostics {
    fn default() -> Self {
        Self::new(false)
    }
}
