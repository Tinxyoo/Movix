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
///
/// 第一性原理(Terminal.app 颜色异常根因链):
/// 1. 6³ cube 亮度分级 0,95,135,175,215,255——最低非零=95 太亮,深色 RGB
///    映射过去会变成"亮蓝/亮青",深色背景变亮色背景(刺眼 PPT 风)。
/// 2. 单纯跳过 cube 走系统色黑也不行——所有 max<64 的深色全部映射到纯黑,
///    跟全屏背景同色,色块(USER/AI/REASONING/TOOL)完全消失,看不出结构。
///
/// 修复策略:
/// - cube:仍禁用对深色(max<64)的应用,避免变亮
/// - 灰阶:深色优先用灰阶 234-238(灰度 28-48),比纯黑亮一档,让色块"浮"
///   在全屏背景(0/232)上仍可见;原 RGB 色相信息以微亮灰阶表达
/// - 系统色:仅当原色"明显有色相"(r/g/b 差异>20)且非纯黑时使用,避免
///   任何深色都变暗蓝暗红
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
    let max_val = r.max(g).max(b);
    let min_val = r.min(g).min(b);
    let is_gray = (r as i16 - g as i16).abs() <= 12 && (g as i16 - b as i16).abs() <= 12;

    // 深色(max<64)走专用路径:不映射到纯黑,用比全屏背景亮一档的灰阶,
    // 让色块(USER/AI/REASONING/TOOL)"浮"在全屏背景(0/232)上仍可见。
    if max_val < 64 {
        // 灰阶索引按亮度分档:max=0→232(灰度 8,接近黑),max=63→237(灰度 48)。
        let gray_idx = ((max_val as f32 / 64.0 * 5.0).round() as u8).min(5);
        // 色相饱和度 = max-min;饱和度高(>=30)说明原色有明显色相(如暗蓝暗红),
        // 用最近暗色系统色(index 0-7)保留色相信息,视觉上仍偏中性。
        let sat = max_val - min_val;
        if !is_gray && sat >= 30 && max_val >= 20 {
            let dark_sys: [(u8, u8, u8); 8] = [
                (0, 0, 0),
                (128, 0, 0),
                (0, 128, 0),
                (128, 128, 0),
                (0, 0, 128),
                (128, 0, 128),
                (0, 128, 128),
                (128, 128, 128),
            ];
            let mut best: u8 = 0;
            let mut best_d = f32::MAX;
            for (i, &(sr, sg, sb)) in dark_sys.iter().enumerate() {
                let d =
                    (rf - sr as f32).powi(2) + (gf - sg as f32).powi(2) + (bf - sb as f32).powi(2);
                if d < best_d {
                    best_d = d;
                    best = i as u8;
                }
            }
            return best;
        }
        // 灰色或弱色相:用灰阶 232-237(灰度 8-48),色块"浮"出。
        return 232 + gray_idx;
    }

    // 中亮色(max>=64)走完整搜索:系统色 + 灰阶 + cube
    let mut best: u8 = 0;
    let mut best_dist = f32::MAX;

    // 1. 系统色
    for (i, &(sr, sg, sb)) in system_colors.iter().enumerate() {
        let d = (rf - sr as f32).powi(2) + (gf - sg as f32).powi(2) + (bf - sb as f32).powi(2);
        if d < best_dist {
            best_dist = d;
            best = i as u8;
        }
    }

    // 2. 灰阶
    if is_gray {
        for gray in 0..24u8 {
            let v = 8 + gray * 10;
            let d = (rf - v as f32).powi(2) * 3.0;
            if d < best_dist {
                best_dist = d;
                best = 232 + gray;
            }
        }
    }

    // 3. cube(中亮色亮度足够,映射精度好)
    if !is_gray {
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
    }

    best
}

// ── 主题颜色函数 ────────────────────────────────────────────
// 所有函数统一走 rgb(),由 rgb_to_ansi256 在 256 色终端下自动降级。
// 降级策略:深色(max<64)跳过 cube 用系统色+灰阶(暗端覆盖好),
// 亮色走 cube 保持精度。这样深色背景不会变成亮色背景。

pub fn ds_blue() -> Color {
    rgb(147, 197, 253)
} // Muted Ice-Blue
pub fn ds_sky() -> Color {
    rgb(203, 213, 225)
} // Clean Silver/Slate
pub fn ds_red() -> Color {
    rgb(248, 113, 113)
} // Soft Muted Red
pub fn text_body() -> Color {
    rgb(226, 232, 240)
} // Crisp Slate-200
pub fn text_muted() -> Color {
    rgb(148, 163, 184)
} // Balanced Slate-400
pub fn text_dim() -> Color {
    rgb(100, 116, 139)
} // Subdued Slate-500
pub fn text_reasoning() -> Color {
    rgb(148, 163, 184)
} // Muted Slate
pub fn diff_added() -> Color {
    rgb(110, 231, 183)
} // Soft Mint Green
pub fn amber() -> Color {
    rgb(203, 213, 225)
} // Clean Silver-Gray
pub fn sep_color() -> Color {
    rgb(30, 41, 59)
} // Deep Slate-800

pub fn focus_chat() -> Color {
    ds_blue()
}
pub fn focus_sidebar() -> Color {
    rgb(100, 140, 190)
}
pub fn focus_input() -> Color {
    rgb(100, 140, 190)
}
pub fn focus_palette() -> Color {
    ds_blue()
}

pub fn plan_accent() -> Color {
    rgb(148, 163, 184)
}
pub fn plan_bg() -> Color {
    rgb(15, 23, 42)
}
pub fn agent_accent() -> Color {
    rgb(226, 232, 240)
}
pub fn agent_bg() -> Color {
    rgb(30, 41, 59)
}
pub fn auto_accent() -> Color {
    rgb(56, 189, 248)
}
pub fn auto_bg() -> Color {
    rgb(12, 74, 110)
}
pub fn yolo_accent() -> Color {
    rgb(203, 213, 225)
}
pub fn yolo_bg() -> Color {
    rgb(30, 41, 59)
}
pub fn unfocus_border() -> Color {
    sep_color()
}

/// 思考块文本颜色(浅金黄 #D3AA70)
pub fn thinking_text() -> Color {
    rgb(211, 170, 112)
}
