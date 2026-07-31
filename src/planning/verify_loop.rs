//! VerifyLoop:在 turn 末尾对 agent 改过的文件跑编译级检查(cargo check /
//! py_compile / 本地 tsc),错误回灌给模型自我修正(上限 3 轮,见 agent/mod.rs)。
//!
//! **接线状态**:已接入主循环 —— `MovixAgent::verify_modifications`
//! (`agent/mod.rs`)、`flush_pending_verify`(turn 末统一执行)、
//! `format_verify_result`(回灌格式化)均调用本模块。
//!
//! **已知残余风险**:`verify_rust` 用 `cargo check --offline` 防止联网拉依赖
//! (规避 build script 构建期 RCE);`verify_typescript` 仅用工作区本地
//! `node_modules/.bin/tsc`,不联网安装。但命令本身**无资源/沙箱边界**(无
//! cgroup/ulimit 限制 CPU),在不可信工作区上仍应配合外层容器使用。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::common::utils;

/// 验证级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VerifyLevel {
    /// 仅语法检查（编译/lint）
    Syntax,
    /// 语法 + 测试
    Test,
    /// 语法 + 测试 + 类型检查
    Full,
}

/// 验证结果
#[derive(Debug, Clone)]
pub struct VerifyResult {
    /// 是否通过
    pub passed: bool,
    /// 验证级别
    pub level: VerifyLevel,
    /// 检查的文件
    pub files: Vec<PathBuf>,
    /// 错误信息
    pub errors: Vec<VerifyError>,
    /// 警告信息
    pub warnings: Vec<String>,
    /// 输出摘要
    pub output_summary: String,
}

/// 验证错误
#[derive(Debug, Clone)]
pub struct VerifyError {
    pub file: PathBuf,
    pub line: Option<u32>,
    pub message: String,
    pub severity: ErrorSeverity,
}

/// 错误严重程度
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSeverity {
    Error,
    Warning,
    Info,
}

/// 验证配置
#[derive(Debug, Clone)]
pub struct VerifyConfig {
    /// 是否启用自动验证
    pub enabled: bool,
    /// 验证级别
    pub level: VerifyLevel,
    /// 需要触发验证的工具名列表
    pub trigger_tools: Vec<String>,
    /// 最大输出长度
    pub max_output_chars: usize,
    /// 验证超时（秒）
    pub timeout_secs: u64,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            level: VerifyLevel::Syntax,
            trigger_tools: vec![
                "write_file".into(),
                "create_file".into(),
                "search_and_replace".into(),
                "edit_file".into(),
            ],
            max_output_chars: 4000,
            timeout_secs: 60,
        }
    }
}

/// 自动验证循环管理器
/// 修复(G-H9):derive Clone 以支持 spawn_blocking(需把 verify_loop move 进闭包)。
#[derive(Clone)]
pub struct VerifyLoop {
    config: VerifyConfig,
    workspace: PathBuf,
}

impl VerifyLoop {
    /// 创建验证循环管理器
    pub fn new(workspace: &Path, config: VerifyConfig) -> Self {
        Self {
            config,
            workspace: workspace.to_path_buf(),
        }
    }

    /// 判断工具调用是否需要触发验证
    pub fn should_verify(&self, tool_name: &str) -> bool {
        self.config.enabled && self.config.trigger_tools.iter().any(|t| t == tool_name)
    }

    /// 对指定文件执行自动验证
    pub fn verify(&self, modified_files: &[PathBuf]) -> VerifyResult {
        if modified_files.is_empty() {
            return VerifyResult {
                passed: true,
                level: self.config.level,
                files: vec![],
                errors: vec![],
                warnings: vec![],
                output_summary: "No files to verify".into(),
            };
        }

        let mut all_errors = Vec::new();
        let mut all_warnings = Vec::new();
        let mut output_parts = Vec::new();

        // 按语言分组，同种语言只运行一次项目级检查（如 cargo check）
        let has_rust = modified_files
            .iter()
            .any(|f| f.extension().and_then(|e| e.to_str()) == Some("rs"));
        let has_python = modified_files
            .iter()
            .any(|f| f.extension().and_then(|e| e.to_str()) == Some("py"));
        let has_ts = modified_files.iter().any(|f| {
            f.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e, "ts" | "tsx" | "js" | "jsx"))
        });

        // Rust: cargo check 是项目级检查，只需运行一次
        if has_rust {
            let result = self.verify_rust_project();
            all_errors.extend(result.errors);
            all_warnings.extend(result.warnings);
            if !result.output_summary.is_empty() {
                output_parts.push(result.output_summary);
            }
        }

        if has_python {
            let result = self.verify_python_project(modified_files);
            all_errors.extend(result.errors);
            all_warnings.extend(result.warnings);
            if !result.output_summary.is_empty() {
                output_parts.push(result.output_summary);
            }
        }

        if has_ts {
            let result = self.verify_typescript_project(modified_files);
            all_errors.extend(result.errors);
            all_warnings.extend(result.warnings);
            if !result.output_summary.is_empty() {
                output_parts.push(result.output_summary);
            }
        }

        if self.config.level >= VerifyLevel::Test {
            let test_result = self.run_project_tests();
            all_errors.extend(test_result.errors);
            if !test_result.output_summary.is_empty() {
                output_parts.push(test_result.output_summary);
            }
        }

        let passed = all_errors
            .iter()
            .all(|e| e.severity != ErrorSeverity::Error);
        let output_summary = output_parts.join("\n---\n");
        let truncated = truncate_str(&output_summary, self.config.max_output_chars);

        VerifyResult {
            passed,
            level: self.config.level,
            files: modified_files.to_vec(),
            errors: all_errors,
            warnings: all_warnings,
            output_summary: truncated,
        }
    }

    /// Rust 项目级检查（cargo check 只需运行一次，无需对每个文件重复执行）
    fn verify_rust_project(&self) -> VerifyResult {
        self.verify_rust(Path::new("."))
    }

    /// Python 项目级检查
    fn verify_python_project(&self, modified_files: &[PathBuf]) -> VerifyResult {
        // 修复(Bug #22):find_file_with_ext 永远返回 None,导致 Python/TS
        // 验证从未真正运行。改为从 modified_files 取第一个匹配。
        let py_file = first_with_ext(modified_files, &["py"]);
        match py_file {
            Some(f) => self.verify_python(&f),
            None => VerifyResult {
                passed: true,
                level: self.config.level,
                files: vec![],
                errors: vec![],
                warnings: vec![],
                output_summary: "No Python files to verify".into(),
            },
        }
    }

    /// TypeScript 项目级检查
    fn verify_typescript_project(&self, modified_files: &[PathBuf]) -> VerifyResult {
        let ts_file = first_with_ext(modified_files, &["ts", "tsx", "js", "jsx"]);
        match ts_file {
            Some(f) => self.verify_typescript(&f),
            None => VerifyResult {
                passed: true,
                level: self.config.level,
                files: vec![],
                errors: vec![],
                warnings: vec![],
                output_summary: "No TypeScript files to verify".into(),
            },
        }
    }

    /// 验证 Rust 文件
    fn verify_rust(&self, file: &Path) -> VerifyResult {
        // 修复(审查):对"含 .rs 文件但非 Cargo 工作区"的目录,原实现无条件跑
        // `cargo check --offline` → "could not find Cargo.toml" 假编译错误回灌给
        // 模型,形成"反复修 Cargo 配置"死循环。仅当存在 Cargo.toml 时才真正检查。
        if !self.workspace.join("Cargo.toml").exists() {
            return VerifyResult {
                passed: true,
                level: VerifyLevel::Syntax,
                files: vec![file.to_path_buf()],
                errors: vec![],
                warnings: vec![],
                output_summary: "未检测到 Cargo.toml,跳过 cargo check".into(),
            };
        }
        let mut command = Command::new("cargo");
        command
            .args(["check", "--message-format=short", "--offline"])
            .current_dir(&self.workspace);
        // 修复(R6/verify-C2,关键):原 `passed` 仅依赖 parse_cargo_errors 的字符串匹配
        // (找 "error["/"error:"),若 cargo 因非匹配原因退出非零(ICE、locale、wrapper
        // 改写输出、"could not compile" 无 error[ 前缀),passed 错误为 true → 坏代码放行。
        // 现在以 exit status 为主、解析为辅:status 失败即 passed=false。
        // 同时加 --offline 防止 verify 触发联网拉依赖(构建期 RCE 面:恶意 build script)。
        let output = utils::run_command_with_timeout(command, self.config.timeout_secs);

        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let combined = format!("{}\n{}", stdout, stderr);
                let errors = parse_cargo_errors(&combined);
                // exit status 失败,或解析出 Error 级别,都判失败。
                let passed = out.status.success()
                    && errors.iter().all(|e| e.severity != ErrorSeverity::Error);
                VerifyResult {
                    passed,
                    level: VerifyLevel::Syntax,
                    files: vec![file.to_path_buf()],
                    errors,
                    warnings: vec![],
                    output_summary: truncate_str(&combined, 2000),
                }
            }
            Err(e) => {
                // 修复(Critical #C7):原实现所有 Err 都 passed:true,把超时/IO 错误
                // 当"检查通过",编译错误被静默放行。现在区分:
                // - NotFound(命令未安装)→ 合理跳过,passed:true;
                // - 超时/其他 IO 错误 → passed:false,不静默放行。
                let not_installed = matches!(e.kind(), std::io::ErrorKind::NotFound);
                VerifyResult {
                    passed: not_installed,
                    level: VerifyLevel::Syntax,
                    files: vec![file.to_path_buf()],
                    errors: if not_installed {
                        vec![]
                    } else {
                        vec![VerifyError {
                            file: file.to_path_buf(),
                            line: None,
                            message: format!("cargo check 执行失败: {}", e),
                            severity: ErrorSeverity::Error,
                        }]
                    },
                    warnings: if not_installed {
                        vec![format!("cargo check not available: {}", e)]
                    } else {
                        vec![]
                    },
                    output_summary: if not_installed {
                        "cargo check skipped".into()
                    } else {
                        format!("cargo check error: {}", e)
                    },
                }
            }
        }
    }

    /// 验证 Python 文件
    fn verify_python(&self, file: &Path) -> VerifyResult {
        let mut command = Command::new("python");
        command.args(["-m", "py_compile"]).arg(file);
        let output = utils::run_command_with_timeout(command, self.config.timeout_secs);

        match output {
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let passed = out.status.success();
                let errors = if passed {
                    vec![]
                } else {
                    vec![VerifyError {
                        file: file.to_path_buf(),
                        line: None,
                        message: stderr.trim().to_string(),
                        severity: ErrorSeverity::Error,
                    }]
                };
                VerifyResult {
                    passed,
                    level: VerifyLevel::Syntax,
                    files: vec![file.to_path_buf()],
                    errors,
                    warnings: vec![],
                    output_summary: if passed {
                        "OK".into()
                    } else {
                        truncate_str(&stderr, 2000)
                    },
                }
            }
            Err(e) => {
                // 修复(Critical #C7):同 verify_rust,区分未安装(跳过)与超时/IO(不放行)。
                let not_installed = matches!(e.kind(), std::io::ErrorKind::NotFound);
                VerifyResult {
                    passed: not_installed,
                    level: VerifyLevel::Syntax,
                    files: vec![file.to_path_buf()],
                    errors: if not_installed {
                        vec![]
                    } else {
                        vec![VerifyError {
                            file: file.to_path_buf(),
                            line: None,
                            message: format!("python check 执行失败: {}", e),
                            severity: ErrorSeverity::Error,
                        }]
                    },
                    warnings: if not_installed {
                        vec!["python not available".into()]
                    } else {
                        vec![]
                    },
                    output_summary: if not_installed {
                        "python check skipped".into()
                    } else {
                        format!("python check error: {}", e)
                    },
                }
            }
        }
    }

    /// 验证 TypeScript/JavaScript 文件
    fn verify_typescript(&self, file: &Path) -> VerifyResult {
        // 修复(R6/verify-C1,关键):原实现 `npx tsc` 会联网拉取 typescript 包,
        // 恶意 package.json 的 postinstall/prepare 钩子 = 构建期 RCE。改为只使用
        // 工作区本地已安装的 tsc(node_modules/.bin/tsc),不存在则跳过(不联网)。
        let local_tsc = self.workspace.join("node_modules").join(".bin").join("tsc");
        let (tsc_cmd, tsc_args): (&str, Vec<&str>) = if local_tsc.exists() {
            // 用本地 tsc 的路径(跨平台:Windows 上 .bin/tsc 是 .cmd,这里用 exists 判断)
            (
                local_tsc.to_str().unwrap_or("tsc"),
                vec!["--noEmit", "--pretty", "false"],
            )
        } else {
            // 本地无 tsc:跳过验证而非联网安装(防 RCE)。
            return VerifyResult {
                passed: true,
                level: self.config.level,
                files: vec![file.to_path_buf()],
                errors: vec![],
                warnings: vec![
                    "本地无 node_modules/.bin/tsc,跳过 TS 校验(不联网安装以防 RCE)".into(),
                ],
                output_summary: "Skipped: no local tsc".into(),
            };
        };
        let mut command = Command::new(tsc_cmd);
        command
            .args(&tsc_args)
            .arg(file)
            .current_dir(&self.workspace);
        let output = utils::run_command_with_timeout(command, self.config.timeout_secs);

        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let combined = format!("{}\n{}", stdout, stderr);
                let passed = out.status.success();
                let errors = if passed {
                    vec![]
                } else {
                    parse_tsc_errors(&combined)
                };
                VerifyResult {
                    passed,
                    level: VerifyLevel::Syntax,
                    files: vec![file.to_path_buf()],
                    errors,
                    warnings: vec![],
                    output_summary: if passed {
                        "OK".into()
                    } else {
                        truncate_str(&combined, 2000)
                    },
                }
            }
            Err(e) => {
                // 修复(Critical #C7):同 verify_rust,区分未安装(跳过)与超时/IO(不放行)。
                let not_installed = matches!(e.kind(), std::io::ErrorKind::NotFound);
                VerifyResult {
                    passed: not_installed,
                    level: VerifyLevel::Syntax,
                    files: vec![file.to_path_buf()],
                    errors: if not_installed {
                        vec![]
                    } else {
                        vec![VerifyError {
                            file: file.to_path_buf(),
                            line: None,
                            message: format!("tsc check 执行失败: {}", e),
                            severity: ErrorSeverity::Error,
                        }]
                    },
                    warnings: if not_installed {
                        vec!["tsc not available".into()]
                    } else {
                        vec![]
                    },
                    output_summary: if not_installed {
                        "typescript check skipped".into()
                    } else {
                        format!("tsc check error: {}", e)
                    },
                }
            }
        }
    }

    /// 运行项目测试
    fn run_project_tests(&self) -> VerifyResult {
        let has_cargo = self.workspace.join("Cargo.toml").exists();
        let has_npm = self.workspace.join("package.json").exists();
        let has_pytest = self.workspace.join("pytest.ini").exists()
            || self.workspace.join("pyproject.toml").exists();

        let output = if has_cargo {
            let mut command = Command::new("cargo");
            command
                .args(["test", "--no-run"])
                .current_dir(&self.workspace);
            utils::run_command_with_timeout(command, self.config.timeout_secs)
        } else if has_npm {
            let mut command = Command::new("npm");
            command.args(["test"]).current_dir(&self.workspace);
            utils::run_command_with_timeout(command, self.config.timeout_secs)
        } else if has_pytest {
            let mut command = Command::new("python");
            command
                .args(["-m", "pytest", "--collect-only", "-q"])
                .current_dir(&self.workspace);
            utils::run_command_with_timeout(command, self.config.timeout_secs)
        } else {
            return VerifyResult {
                passed: true,
                level: VerifyLevel::Test,
                files: vec![],
                errors: vec![],
                warnings: vec!["No test framework detected".into()],
                output_summary: "No tests to run".into(),
            };
        };

        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let combined = format!("{}\n{}", stdout, stderr);
                let passed = out.status.success();
                VerifyResult {
                    passed,
                    level: VerifyLevel::Test,
                    files: vec![],
                    errors: if passed {
                        vec![]
                    } else {
                        vec![VerifyError {
                            file: PathBuf::new(),
                            line: None,
                            message: "Tests failed".into(),
                            severity: ErrorSeverity::Error,
                        }]
                    },
                    warnings: vec![],
                    output_summary: truncate_str(&combined, 2000),
                }
            }
            Err(e) => {
                // 修复(Critical #C7):同上,区分未安装(跳过)与超时/IO(不放行)。
                let not_installed = matches!(e.kind(), std::io::ErrorKind::NotFound);
                VerifyResult {
                    passed: not_installed,
                    level: VerifyLevel::Test,
                    files: vec![],
                    errors: if not_installed {
                        vec![]
                    } else {
                        vec![VerifyError {
                            file: PathBuf::new(),
                            line: None,
                            message: format!("test runner 执行失败: {}", e),
                            severity: ErrorSeverity::Error,
                        }]
                    },
                    warnings: if not_installed {
                        vec!["Test runner not available".into()]
                    } else {
                        vec![]
                    },
                    output_summary: if not_installed {
                        "Tests skipped".into()
                    } else {
                        format!("test runner error: {}", e)
                    },
                }
            }
        }
    }

    /// 将验证结果格式化为上下文消息
    pub fn format_verify_result(result: &VerifyResult) -> String {
        if result.passed {
            // 区分"真正通过"与"因命令不可用而跳过"：跳过时 warnings 非空且 summary 含 skipped。
            let skipped = result.output_summary.to_lowercase().contains("skipped");
            let status = if skipped { "SKIPPED" } else { "PASSED" };
            if result.warnings.is_empty() {
                format!(
                    "<verification_result status=\"{status}\" level=\"{:?}\">\nAll checks passed for {} file(s).\n</verification_result>",
                    result.level,
                    result.files.len()
                )
            } else {
                format!(
                    "<verification_result status=\"{status}\" level=\"{:?}\">\nChecks for {} file(s) completed with warnings:\n{}\n</verification_result>",
                    result.level,
                    result.files.len(),
                    result
                        .warnings
                        .iter()
                        .map(|w| format!("  - {w}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            }
        } else {
            let all_error_lines: Vec<String> = result
                .errors
                .iter()
                .filter(|e| e.severity == ErrorSeverity::Error)
                .map(|e| match e.line {
                    Some(l) => format!("  - {}:{} {}", e.file.display(), l, e.message),
                    None => format!("  - {} {}", e.file.display(), e.message),
                })
                .collect();
            // 修复(审查):错误行数无上限,一个故意写烂的项目可产生上千条错误,
            // 每条失败 turn 都整体回灌 → 上下文被验证错误撑爆。上限 40 条,其余折叠。
            const MAX_ERROR_LINES: usize = 40;
            let (shown, remaining) = if all_error_lines.len() > MAX_ERROR_LINES {
                (
                    all_error_lines[..MAX_ERROR_LINES].to_vec(),
                    Some(all_error_lines.len() - MAX_ERROR_LINES),
                )
            } else {
                (all_error_lines, None)
            };
            let mut body = shown.join("\n");
            if let Some(n) = remaining {
                body.push_str(&format!("\n  ... 另有 {} 条错误已折叠", n));
            }
            format!(
                "<verification_result status=\"FAILED\" level=\"{:?}\">\n{} error(s) found:\n{}\nOutput:\n{}\n</verification_result>",
                result.level,
                result.errors.len(),
                body,
                result.output_summary
            )
        }
    }
}

/// 从 modified_files 中找到指定扩展名(任一)的第一个文件。
fn first_with_ext(files: &[PathBuf], exts: &[&str]) -> Option<PathBuf> {
    files
        .iter()
        .find(|f| {
            f.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| exts.contains(&e))
        })
        .cloned()
}

/// 解析 cargo check 输出中的错误
fn parse_cargo_errors(output: &str) -> Vec<VerifyError> {
    let mut errors = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.contains("error[") || trimmed.starts_with("error:") {
            errors.push(VerifyError {
                file: PathBuf::new(),
                line: None,
                message: trimmed.to_string(),
                severity: ErrorSeverity::Error,
            });
        } else if trimmed.contains("warning:") {
            errors.push(VerifyError {
                file: PathBuf::new(),
                line: None,
                message: trimmed.to_string(),
                severity: ErrorSeverity::Warning,
            });
        }
    }
    errors
}

/// 解析 tsc 输出中的错误
fn parse_tsc_errors(output: &str) -> Vec<VerifyError> {
    let mut errors = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.contains("error TS") {
            errors.push(VerifyError {
                file: PathBuf::new(),
                line: None,
                message: trimmed.to_string(),
                severity: ErrorSeverity::Error,
            });
        }
    }
    errors
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // 修复(边界):max < 20 时 max-20 会下溢为 usize::MAX;用 saturating 防下溢。
        let reserve = 20.min(max.saturating_sub(1));
        let cutoff = crate::common::utils::previous_char_boundary(s, max.saturating_sub(reserve));
        format!("{}...[truncated]", &s[..cutoff])
    }
}
