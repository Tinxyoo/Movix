//! P5: TUI 主题(颜色与样式常量) + 终端真彩色检测。
//!
//! 集中所有 ratatui 颜色/样式常量，并根据终端是否支持真彩色自动降级。
//! macOS 自带 Terminal.app 不支持真彩色，会降级为 ANSI 256 色。
//! iTerm2 / WezTerm / Hyper / Alacritty 等现代终端支持真彩色。

use ratatui::style::Color;
use std::io::IsTerminal;
use std::sync::OnceLock;

// ── 颜色能力统一检测（colored + ratatui 共用） ──────────────

/// 颜色支持等级。`colored` 的固定 16 色调色板在任何等级都会输出，
/// 区别在于是否能用 256 色 / 24-bit RGB。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorLevel {
    /// 无颜色（管道/重定向/被 NO_COLOR 禁用）
    None,
    /// 仅 16 色（兼容性最好）
    Ansi16,
    /// 256 色（xterm-256color）
    Ansi256,
    /// 24-bit 真彩色
    TrueColor,
}

/// 启动时初始化 `colored` crate 的颜色输出。
///
/// **必须**在 `main` 函数最早阶段（`init_tracing` 之前或之后、`println!` 之前）
/// 调用一次。作用：
/// 1. 让 **debug / release** 构建颜色一致（`colored` 默认 release 关闭颜色）
/// 2. 跨 **Windows / macOS / Linux** 行为一致
/// 3. 尊重业界标准 `NO_COLOR` 环境变量
/// 4. 尊重 `CLICOLOR_FORCE=1` 强制开启（哪怕不是 TTY）
pub fn init_color_support() {
    let level = detect_color_level();
    let enabled = level != ColorLevel::None;
    // 强制覆盖 colored 内部的所有启发式判断（debug/release、TTY 检测等）
    colored::control::set_override(enabled);
}

/// 检测当前终端的颜色能力。结果会缓存，重复调用零成本。
pub fn color_level() -> ColorLevel {
    static CACHE: OnceLock<ColorLevel> = OnceLock::new();
    *CACHE.get_or_init(detect_color_level)
}

/// 实际执行颜色能力检测的纯函数，便于测试。
fn detect_color_level() -> ColorLevel {
    // 1) NO_COLOR 行业规范：只要设置了就禁用（值非空也禁用，参见 no-color.org）
    if let Ok(val) = std::env::var("NO_COLOR") {
        if !val.is_empty() {
            return ColorLevel::None;
        }
    }

    // 2) 非 TTY 场景（管道 / 重定向 / CI）默认禁用，避免 ANSI 污染日志
    //    例外：CLICOLOR_FORCE 强制开启
    let stdout_is_tty = std::io::stdout().is_terminal();
    let force = matches!(std::env::var("CLICOLOR_FORCE").as_deref(), Ok("1" | "true"));
    if !stdout_is_tty && !force {
        return ColorLevel::None;
    }

    // 3) 真彩色检测：与 ratatui 部分的 has_truecolor() 共享策略
    if has_truecolor() {
        return ColorLevel::TrueColor;
    }

    // 4) 256 色检测：常见于 xterm-256color、iTerm2（关闭 truecolor 时）
    if let Ok(term) = std::env::var("TERM") {
        if term.contains("256color") {
            return ColorLevel::Ansi256;
        }
    }

    // 5) 其余 TTY 默认至少 16 色
    ColorLevel::Ansi16
}

/// 检测当前终端是否支持真彩色（24-bit RGB）。
///
/// 检测策略（按优先级）：
/// 1. `COLORTERM=truecolor` / `COLORTERM=24bit` → 支持（最可靠）
/// 2. `TERM_PROGRAM` 命中 iTerm.app / WezTerm / Hyper / Alacritty / tmux / ghostty / rio
///    或 IDE 集成终端（vscode / cursor / windsurf / Trae）→ 支持
/// 3. `WT_SESSION` 或 `WT_PROFILE_ID` 存在 → Windows Terminal 支持
///    （注意：Windows Terminal 默认不设置 `COLORTERM`，必须用这个兜底）
/// 4. `TERM` 包含 256color / truecolor / xterm-kitty / xterm-ghostty / st-256color → 支持
/// 5. 显式不支持：`TERM_PROGRAM=Apple_Terminal`（macOS 自带终端只支持 256 色）
/// 6. 其他 → 不支持，降级为 256 色
fn has_truecolor() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        // 1) COLORTERM 是最可靠的真彩色信号
        if let Ok(val) = std::env::var("COLORTERM") {
            if val.eq_ignore_ascii_case("truecolor") || val.eq_ignore_ascii_case("24bit") {
                return true;
            }
        }

        // 2) TERM_PROGRAM 命中现代终端
        if let Ok(val) = std::env::var("TERM_PROGRAM") {
            const TRUE: &[&str] = &[
                "iTerm.app",
                "WezTerm",
                "Hyper",
                "Alacritty",
                "tmux",
                "ghostty",
                "rio",
                "vscode",
                "cursor",
                "windsurf",
                "Trae", // IDE 集成终端
            ];
            if TRUE.iter().any(|t| val.eq_ignore_ascii_case(t)) {
                return true;
            }
            // 明确不支持（早期 Terminal.app / mintty）
            if val == "Apple_Terminal" {
                return false;
            }
        }

        // 3) Windows Terminal 关键特征：WT_SESSION（Windows Terminal 默认不设 COLORTERM！）
        if std::env::var("WT_SESSION").is_ok() || std::env::var("WT_PROFILE_ID").is_ok() {
            return true;
        }

        // 4) TERM 命中现代终端类型
        if let Ok(term) = std::env::var("TERM") {
            if term.contains("256color")
                || term.contains("truecolor")
                || term == "xterm-kitty"
                || term == "xterm-ghostty"
                || term == "st-256color"
            {
                return true;
            }
        }

        // 5) Windows 平台兜底：Windows 10 1607+ 的所有终端（conhost / Windows Terminal / VS Code）
        //    都支持 24-bit 颜色。conhost 默认不设 COLORTERM，但 `colored` 在 Windows 上
        //    会自动调用 `SetConsoleMode(ENABLE_VIRTUAL_TERMINAL_PROCESSING)` 启用 VT 转义。
        #[cfg(windows)]
        {
            if std::io::stdout().is_terminal() {
                return true;
            }
        }

        false
    })
}

/// 创建颜色：真彩色终端返回 `Color::Rgb`，否则降级为最接近的 ANSI 256 色。
pub fn rgb(r: u8, g: u8, b: u8) -> Color {
    if has_truecolor() {
        Color::Rgb(r, g, b)
    } else {
        Color::Indexed(rgb_to_ansi256(r, g, b))
    }
}

/// 将 24-bit RGB 映射到最接近的 ANSI 256 色索引。
fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    let system_colors: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];

    let (rf, gf, bf) = (r as f32, g as f32, b as f32);
    let mut best: u8 = 0;
    let mut best_dist = f32::MAX;

    for (i, &(sr, sg, sb)) in system_colors.iter().enumerate() {
        let d = (rf - sr as f32).powi(2) + (gf - sg as f32).powi(2) + (bf - sb as f32).powi(2);
        if d < best_dist {
            best_dist = d;
            best = i as u8;
        }
    }

    let ri = (r as f32 / 255.0 * 5.0).round() as u8;
    let gi = (g as f32 / 255.0 * 5.0).round() as u8;
    let bi = (b as f32 / 255.0 * 5.0).round() as u8;

    let cr = ((ri as f32) * 255.0 / 5.0).round();
    let cg = ((gi as f32) * 255.0 / 5.0).round();
    let cb = ((bi as f32) * 255.0 / 5.0).round();
    let d_cube = (rf - cr).powi(2) + (gf - cg).powi(2) + (bf - cb).powi(2);

    if d_cube < best_dist {
        best = 16 + ri * 36 + gi * 6 + bi;
    }

    if r == g && g == b {
        for gray in 0..24u8 {
            let v = 8 + gray * 10;
            let d = (rf - v as f32).powi(2) * 3.0;
            if d < best_dist {
                best_dist = d;
                best = 232 + gray;
            }
        }
    }

    best
}

// ── 主题颜色常量 ────────────────────────────────────────────
// 注意：这些 const 使用 Color::Rgb，在非真彩色终端上不会自动降级为 256 色。
// 终端会自行近似处理，效果通常可接受。如需精确降级，在渲染代码中使用
// `theme::rgb(r, g, b)` 代替这些常量。新增内联颜色请优先用 `rgb()`。

pub const DS_BLUE: Color = Color::Rgb(147, 197, 253); // Muted Ice-Blue
pub const DS_SKY: Color = Color::Rgb(203, 213, 225); // Clean Silver/Slate
pub const DS_RED: Color = Color::Rgb(248, 113, 113); // Soft Muted Red
pub const TEXT_BODY: Color = Color::Rgb(226, 232, 240); // Crisp Slate-200
pub const TEXT_MUTED: Color = Color::Rgb(148, 163, 184); // Balanced Slate-400
pub const TEXT_DIM: Color = Color::Rgb(100, 116, 139); // Subdued Slate-500
pub const TEXT_REASONING: Color = Color::Rgb(148, 163, 184); // Muted Slate
pub const DIFF_ADDED: Color = Color::Rgb(110, 231, 183); // Soft Mint Green
pub const AMBER: Color = Color::Rgb(203, 213, 225); // Clean Silver-Gray
pub const SEP_COLOR: Color = Color::Rgb(30, 41, 59); // Deep Slate-800

pub const FOCUS_CHAT: Color = DS_BLUE;
pub const FOCUS_SIDEBAR: Color = Color::Rgb(100, 140, 190);
pub const FOCUS_INPUT: Color = Color::Rgb(100, 140, 190);
pub const FOCUS_PALETTE: Color = DS_BLUE;

pub const PLAN_ACCENT: Color = Color::Rgb(148, 163, 184);
pub const PLAN_BG: Color = Color::Rgb(15, 23, 42);
pub const AGENT_ACCENT: Color = Color::Rgb(226, 232, 240);
pub const AGENT_BG: Color = Color::Rgb(30, 41, 59);
pub const AUTO_ACCENT: Color = Color::Rgb(56, 189, 248);
pub const AUTO_BG: Color = Color::Rgb(12, 74, 110);
pub const YOLO_ACCENT: Color = Color::Rgb(203, 213, 225);
pub const YOLO_BG: Color = Color::Rgb(30, 41, 59);
pub const UNFOCUS_BORDER: Color = SEP_COLOR;
