//! Failed-test summary overlay: stack trace links, layout, and click hit-testing.

use std::io;
use std::process::{Command, Stdio};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use super::failed_tests::FailedTestInfo;

const DEFAULT_FAILED_SUMMARY_LIST_PANE_COLS: u16 = 32;
const MIN_FAILED_SUMMARY_LIST_PANE_COLS: u16 = 16;
const MIN_FAILED_SUMMARY_DETAIL_PANE_COLS: u16 = 20;

pub(crate) fn clamp_failed_summary_list_pane_cols(cols: u16, area_width: u16) -> u16 {
    let max_cols = area_width.saturating_sub(MIN_FAILED_SUMMARY_DETAIL_PANE_COLS);
    let min_cols = MIN_FAILED_SUMMARY_LIST_PANE_COLS.min(max_cols);
    cols.clamp(min_cols, max_cols)
}

pub(crate) fn compute_failure_summary_list_pane_cols(
    saved_cols: Option<u16>,
    area_width: u16,
) -> u16 {
    let cols = saved_cols.unwrap_or(DEFAULT_FAILED_SUMMARY_LIST_PANE_COLS);
    clamp_failed_summary_list_pane_cols(cols, area_width)
}

#[derive(Clone, Debug)]
pub(crate) struct StackTraceTarget {
    pub path: String,
    pub line_number: Option<u32>,
}

pub(crate) fn parse_stack_trace_target(line: &str) -> Option<StackTraceTarget> {
    let trimmed = line.trim();
    let in_pos = trimmed.rfind(" in ")?;
    let after_in = trimmed[in_pos + 4..].trim();
    if after_in.is_empty() {
        return None;
    }

    let (path, line_number) = if let Some(line_pos) = after_in.rfind(":line ") {
        let path = after_in[..line_pos].trim();
        let number = after_in[line_pos + 6..].trim().parse::<u32>().ok();
        (path, number)
    } else {
        (after_in, None)
    };

    if path.is_empty() {
        return None;
    }

    // Normalize: stack may use / or `file:///C:/...`; we open even if the file is not present (e.g. moved repo) so the OS can show the error.
    let mut s = path.trim();
    s = s.strip_prefix("file://").unwrap_or(s);
    if cfg!(windows) {
        if let Some(rest) = s.strip_prefix('/') {
            // `file:///C:/path` -> `/C:/path` after strip
            if rest.as_bytes().get(1).is_some_and(|b| *b == b':') {
                s = rest;
            }
        }
    }
    s = s.trim();
    if s.is_empty() {
        return None;
    }
    let path = if cfg!(windows) {
        s.replace('/', r"\")
    } else {
        s.to_string()
    };

    Some(StackTraceTarget { path, line_number })
}

/// Launch the OS default app for a path without blocking the TUI read loop. Blocking
/// `child.wait()` in the same thread as `event::read()` on Windows can interleave mouse
/// down/up in ways that make the next link click map to the wrong line or be ignored.
pub(crate) fn open_path_in_default_editor(path: &str) -> io::Result<()> {
    let path = path.to_string();
    #[cfg(target_os = "windows")]
    {
        let mut child = Command::new("cmd")
            .args(["/C", "start", "", &path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let _ = std::thread::Builder::new()
            .name("open-file".to_string())
            .spawn(move || {
                let _ = child.wait();
            })?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let mut child = Command::new("open")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let _ = std::thread::Builder::new()
            .name("open-file".to_string())
            .spawn(move || {
                let _ = child.wait();
            })?;
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut child = Command::new("xdg-open")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let _ = std::thread::Builder::new()
            .name("open-file".to_string())
            .spawn(move || {
                let _ = child.wait();
            })?;
        return Ok(());
    }

    #[allow(unreachable_code)]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "opening files is not supported on this platform",
    ))
}

fn failed_summary_popup_layout(area: Rect, list_pane_cols: u16) -> (Rect, Rect) {
    let popup = area;
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Min(0), Constraint::Length(2)])
        .split(popup);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![Constraint::Length(list_pane_cols), Constraint::Min(0)])
        .split(inner[0]);
    (body[0], body[1])
}

pub(crate) fn failed_summary_list_rect(area: Rect, list_pane_cols: u16) -> Rect {
    failed_summary_popup_layout(area, list_pane_cols).0
}

pub(crate) fn failed_summary_detail_rect(area: Rect, list_pane_cols: u16) -> Rect {
    failed_summary_popup_layout(area, list_pane_cols).1
}

/// Styling for one error-detail line. Use the same function for `Paragraph` rendering and for
/// row-count calculation so wrapped rows match click / hover hit-testing.
///
/// `line_index` + `hover_line` highlight the stack-trace link under the mouse (most terminals
/// cannot change the system pointer to a “hand” over a region; a background + brighter text is
/// the usual TUI approach).
pub(crate) fn failed_detail_styled_line_with_hover(
    line: &str,
    line_index: usize,
    hover_line: Option<usize>,
) -> Line<'static> {
    let is_link = parse_stack_trace_target(line).is_some();
    let style = if is_link && hover_line == Some(line_index) {
        Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::UNDERLINED)
    } else if is_link {
        Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::UNDERLINED)
    } else if line.contains("Error Message:") {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if line.contains("Stack Trace:") {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    Line::from(Span::styled(line.to_string(), style))
}

/// Index of a stack-trace line under the pointer, if that line is openable (`… in path:line`).
pub(crate) fn hovered_openable_detail_index(
    details: &[String],
    detail_inner_width: u16,
    failed_detail_scroll: u16,
    click_row_in_detail: u16,
) -> Option<usize> {
    let idx = clicked_detail_index(
        details,
        detail_inner_width,
        failed_detail_scroll,
        click_row_in_detail,
    )?;
    if parse_stack_trace_target(&details[idx]).is_some() {
        Some(idx)
    } else {
        None
    }
}

/// When the failed-summary panel is open, maps pointer position to a hover highlight for file links.
pub(crate) fn compute_failure_detail_link_hover(
    failed_tests: &[FailedTestInfo],
    failed_selection: usize,
    detail_inner_width: u16,
    detail_inner_y: u16,
    failed_detail_scroll: u16,
    mouse_in_detail_pane: bool,
    mouse_row: u16,
) -> Option<usize> {
    if !mouse_in_detail_pane || failed_tests.is_empty() {
        return None;
    }
    let selected = &failed_tests[failed_selection.min(failed_tests.len() - 1)];
    if selected.details.is_empty() {
        return None;
    }
    hovered_openable_detail_index(
        &selected.details,
        detail_inner_width,
        failed_detail_scroll,
        mouse_row.saturating_sub(detail_inner_y),
    )
}

fn rendered_rows_for_detail_line(line: &str, inner_width: u16) -> usize {
    if inner_width == 0 {
        return 1;
    }
    // Use base (non-hover) styling so row counts match whether or not a line is highlighted.
    Paragraph::new(failed_detail_styled_line_with_hover(line, 0, None))
        .wrap(Wrap { trim: false })
        .line_count(inner_width.max(1))
        .max(1)
}

pub(crate) fn clicked_detail_index(
    details: &[String],
    inner_width: u16,
    scroll: u16,
    click_row: u16,
) -> Option<usize> {
    let mut remaining_row = usize::from(scroll) + usize::from(click_row);
    for (index, line) in details.iter().enumerate() {
        let rows = rendered_rows_for_detail_line(line, inner_width);
        if remaining_row < rows {
            return Some(index);
        }
        remaining_row = remaining_row.saturating_sub(rows);
    }
    None
}
