use clap::{Parser, Subcommand};
use colored::*;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use movix::cli;
use movix::common::config::MovixConfig;
use movix::common::error::Result;

/// 初始化 tracing,优先使用 RUST_LOG,未设置时给个静音默认值。
/// TUI 模式下仅记录到日志文件(若设置了 `MOVIX_LOG_FILE`),避免污染终端。
fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,movix=info,movix_lib=info"));

    // 若用户显式要求日志到文件(便于复现 bug),写到给定路径;否则 stderr。
    if let Ok(log_path) = std::env::var("MOVIX_LOG_FILE") {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok();
        if let Some(file) = file {
            let _ = tracing_subscriber::registry()
                .with(env_filter)
                .with(fmt::layer().with_writer(file).with_ansi(false))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_writer(std::io::stderr).with_target(true))
        .try_init();
}

#[derive(Parser)]
#[command(
    name = "movix",
    version = env!("CARGO_PKG_VERSION"),
    about = "Movix - DeepSeek coding agent",
    long_about = "A Rust-powered coding agent for DeepSeek-compatible chat APIs, with tool calling, reasoning controls, and workspace automation."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(
        short = 'm',
        long = "model",
        help = "Model name, for example deepseek-v4-pro or deepseek-v4-flash"
    )]
    model: Option<String>,

    #[arg(short = 'w', long = "workspace", help = "Workspace directory")]
    workspace: Option<String>,

    #[arg(
        short = 't',
        long = "task",
        help = "Task description for non-interactive mode"
    )]
    task: Option<String>,

    #[arg(long = "no-thinking", help = "Disable reasoning mode at startup")]
    no_thinking: bool,

    #[arg(
        long = "reasoning-effort",
        help = "Reasoning effort: off, auto, high, or max"
    )]
    reasoning_effort: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "List available tools")]
    Tools,
    #[command(about = "Show configuration")]
    Info,
}

fn apply_cli_overrides(mut config: MovixConfig, cli: &Cli) -> MovixConfig {
    if let Some(ref model) = cli.model {
        config.model = model.clone();
    }
    if let Some(ref workspace) = cli.workspace {
        config.workspace = workspace.into();
    }
    if cli.no_thinking {
        config.thinking_enabled = false;
    }
    if let Some(ref effort) = cli.reasoning_effort {
        config.reasoning_effort = effort.clone();
    }
    config
}

#[tokio::main]
async fn main() -> Result<()> {
    // 必须在 init_tracing 之前：让 colored 的 ANSI 输出在 debug/release 一致
    // 并在 Windows / macOS / Linux 上行为一致。详见 cli::theme::init_color_support。
    cli::theme::init_color_support();
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Tools) => {
            let config = apply_cli_overrides(MovixConfig::from_env_optional(), &cli);
            let workspace = config.workspace.to_string_lossy().to_string();
            let tools = movix::tools::create_default_registry(&workspace);
            println!("{}", "Movix available tools:".bright_white().bold());
            println!("{}", "-".repeat(50).dimmed());
            for name in tools.names() {
                if let Some(tool) = tools.get(&name) {
                    println!("  - {}", name.bright_cyan());
                    println!("    {}", tool.description());
                }
            }
            Ok(())
        }
        Some(Commands::Info) => {
            let config = apply_cli_overrides(MovixConfig::from_env_optional(), &cli);
            let has_key = !config.api_key.is_empty();
            println!("{}", "Movix configuration".bright_white().bold());
            println!("{}", "-".repeat(55).dimmed());
            println!(
                "  {:<22} {}",
                "Model:".dimmed(),
                match config.model.as_str() {
                    "deepseek-v4-pro" => "DeepSeek-V4-Pro".bright_cyan(),
                    "deepseek-v4-flash" => "DeepSeek-V4-Flash".yellow(),
                    other => other.normal(),
                }
            );
            println!(
                "  {:<22} {}",
                "Workspace:".dimmed(),
                config.workspace.display().to_string().bright_yellow()
            );
            println!(
                "  {:<22} {}",
                "API key:".dimmed(),
                if has_key {
                    "configured".bright_green()
                } else {
                    "missing".red()
                }
            );
            println!(
                "  {:<22} {}",
                "Reasoning:".dimmed(),
                if config.thinking_enabled {
                    format!("enabled ({})", config.reasoning_effort).bright_green()
                } else {
                    "disabled".red()
                }
            );
            println!(
                "  {:<22} {}",
                "Max iterations:".dimmed(),
                config.max_iterations.to_string().bright_yellow()
            );
            println!(
                "  {:<22} {}",
                "Context window:".dimmed(),
                "Tiered (~900K)".bright_yellow()
            );
            println!(
                "  {:<22} {}",
                "API base URL:".dimmed(),
                config.base_url.bright_yellow()
            );
            println!();
            println!(
                "{}",
                "常用命令: /help /model /pricing /verify /snapshot /review".dimmed()
            );
            Ok(())
        }
        None => {
            let config = apply_cli_overrides(MovixConfig::prompt_api_key(), &cli);
            if let Some(task) = cli.task {
                cli::run_single_task(config, task).await
            } else {
                cli::run_interactive(config).await
            }
        }
    }
}
