//! P5: TUI 布局辅助(计算各区域高度,产出 Layout 切片)。
//!
//! 这些函数都是**无状态**的——纯函数 + 终端大小(通过 `crossterm::terminal::size()`)。
//! 抽出后 cli.rs 不再关心"这块多高"这种几何,只关心"画什么"。
//!
//! 注意:`input_height` 因为需要读 `App::input_buf` 私有字段,**仍留在 cli.rs**;
//! 它的"按宽度计算视觉行"算法可以在未来公开 `App::visual_line_count()` 方法后再搬。

use std::rc::Rc;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use unicode_width::UnicodeWidthStr;

/// 计算审批通知块高度(根据工具/详情/终端宽度算需要多少行)。
pub fn calc_approval_height(tool: &str, detail: &str, width: u16) -> u16 {
    let inner_w = width.saturating_sub(4) as usize;
    let detail_clean = detail.trim_start_matches("[needs-approval] ");
    let detail_lines = if inner_w > 4 {
        let detail_w = inner_w.saturating_sub(4);
        let display_w = UnicodeWidthStr::width(detail_clean);
        if display_w > detail_w {
            display_w.div_ceil(detail_w)
        } else {
            1
        }
    } else {
        1
    };
    let is_high_risk = tool == "run_command" || tool == "write_file" || tool == "patch_file";
    let risk_extra = if is_high_risk { 1 } else { 0 };
    let header_lines = 2;
    let tool_line = 1;
    let gap_lines = 2;
    let btn_lines = 2;
    (header_lines + tool_line + detail_lines + gap_lines + risk_extra + btn_lines + 2) as u16
}

/// 主布局行划分(返回 4 行或 5 行的 Rc<[Rect]>,取决于审批块是否存在)。
pub fn root_rows(area: Rect, input_h: u16, approval_h: u16) -> Rc<[Rect]> {
    if approval_h > 0 {
        // 有审批弹窗时减小 chat 最小高度，避免挤掉输入框。
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(approval_h),
                Constraint::Length(input_h),
                Constraint::Length(1),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(6),
                Constraint::Length(input_h),
                Constraint::Length(1),
            ])
            .split(area)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_height_includes_risk_extra() {
        // 详情短 + 宽度足够 → 默认 9 行;高风险工具多 1 行
        let w = 80;
        assert!(calc_approval_height("read_file", "x.rs", w) >= 9);
        let read_h = calc_approval_height("read_file", "x.rs", w);
        let write_h = calc_approval_height("write_file", "x.rs", w);
        let run_h = calc_approval_height("run_command", "ls", w);
        // 高风险工具审批块更高
        assert!(write_h > read_h);
        assert!(run_h > read_h);
    }

    #[test]
    fn approval_height_grows_with_long_detail() {
        let short = calc_approval_height("write_file", "x.rs", 80);
        let long = calc_approval_height("write_file", &"a".repeat(200), 80);
        assert!(long > short);
    }

    #[test]
    fn root_rows_returns_4_when_no_approval() {
        let area = Rect::new(0, 0, 80, 24);
        let rows = root_rows(area, 5, 0);
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn root_rows_returns_5_when_approval_present() {
        let area = Rect::new(0, 0, 80, 40);
        let rows = root_rows(area, 5, 10);
        assert_eq!(rows.len(), 5);
    }
}
