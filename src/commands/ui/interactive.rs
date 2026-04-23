use anyhow::Result;
use crate::core::executor::discover_tests;
use crate::core::tree::{build_flat_tree, TreeNode};
use arboard::Clipboard;
use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::manual_watch::{start_manual_watch, ManualWatchHandle};

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};

use super::config::{OutputMode, RunConfig, Verbosity};
use super::filter::{build_filter, sync_parents};
use super::layout::{centered_rect, format_elapsed};
use super::output::{kill_process, OutputEvent, spawn_test_run};

#[derive(Clone, Debug, Default)]
struct FailedTestInfo {
    name: String,
    details: Vec<String>,
}

fn is_status_result_line(trimmed: &str) -> bool {
    trimmed.starts_with("Passed ")
        || trimmed.starts_with("Failed ")
        || trimmed.starts_with("Skipped ")
        || trimmed.starts_with('✓')
        || trimmed.starts_with('✗')
        || trimmed.starts_with('⚠')
}

fn extract_failed_tests(lines: &[String]) -> Vec<FailedTestInfo> {
    let mut failed: Vec<FailedTestInfo> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.starts_with("Failed ") {
            let after = trimmed.trim_start_matches("Failed ").trim();
            let name = after.split(" [").next().unwrap_or(after).trim().to_string();
            if name.is_empty() {
                i += 1;
                continue;
            }

            let mut details = Vec::new();
            let mut j = i + 1;
            while j < lines.len() {
                let next_trimmed = lines[j].trim();
                let lower = next_trimmed.to_lowercase();
                if is_status_result_line(next_trimmed)
                    || lower.starts_with("total tests:")
                    || lower.starts_with("passed:")
                    || lower.starts_with("failed:")
                    || lower.starts_with("skipped:")
                {
                    break;
                }
                details.push(lines[j].clone());
                j += 1;
            }

            if let Some(existing) = failed.iter_mut().find(|f| f.name == name) {
                if existing.details.is_empty() && !details.is_empty() {
                    existing.details = details;
                }
            } else {
                failed.push(FailedTestInfo { name, details });
            }
            i = j;
            continue;
        }
        i += 1;
    }
    failed
}

/// Strip VSTest parameter tail so `Name(a,b)` can be used in `FullyQualifiedName~` filters.
fn filter_key_for_vstest(name: &str) -> String {
    name.split('(').next().unwrap_or(name).trim().to_string()
}

fn build_filter_for_display_names(names: &[String]) -> String {
    names
        .iter()
        .map(|n| {
            let k = filter_key_for_vstest(n);
            format!("FullyQualifiedName~{k}")
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn apply_manual_watch_config(
    root: &Path,
    run_config: &RunConfig,
    handle: &mut Option<ManualWatchHandle>,
) {
    if let Some(h) = handle.take() {
        h.stop();
    }
    if !run_config.manual_watch_enabled {
        return;
    }
    let delay = Duration::from_millis(run_config.manual_watch_delay_ms as u64);
    match start_manual_watch(root.to_path_buf(), delay) {
        Ok(h) => *handle = Some(h),
        Err(e) => {
            eprintln!("Could not start manual watch: {e}");
        }
    }
}

/// Shared by Enter, manual watch, and failed-test reruns. `filter` is the
/// `FullyQualifiedName~…` string from `build_filter` (empty = run all).
#[allow(clippy::too_many_arguments)]
fn launch_filtered_test_run(
    filter: String,
    heading: &str,
    run_config: &RunConfig,
    output_lines: &mut Vec<String>,
    output_rx: &mut Option<mpsc::Receiver<OutputEvent>>,
    output_scroll: &mut u16,
    output_follow_tail: &mut bool,
    run_pid: &mut Option<u32>,
    run_start: &mut Option<Instant>,
    run_passed: &mut usize,
    run_failed: &mut usize,
    run_skipped: &mut usize,
    failed_tests: &mut Vec<FailedTestInfo>,
    show_failure_summary: &mut bool,
    failed_selection: &mut usize,
    failed_detail_scroll: &mut u16,
    is_running: &mut bool,
    show_output_fullscreen: &mut bool,
) {
    *show_output_fullscreen = run_config.output_mode == OutputMode::Fullscreen;
    output_lines.clear();
    *output_scroll = 0;
    *output_follow_tail = true;
    output_lines.push(heading.to_string());
    if filter.is_empty() {
        output_lines.push("  (all selected tests, no name filter)".to_string());
    }
    output_lines.push(String::new());
    match spawn_test_run(Some(filter), run_config) {
        Ok((rx, pid)) => {
            *output_rx = Some(rx);
            *run_pid = Some(pid);
            *is_running = true;
            *run_start = Some(Instant::now());
            *run_passed = 0;
            *run_failed = 0;
            *run_skipped = 0;
            failed_tests.clear();
            *show_failure_summary = false;
            *failed_selection = 0;
            *failed_detail_scroll = 0;
        }
        Err(e) => {
            output_lines.push(format!("Error: {e}"));
        }
    }
}

pub(super) fn run_interactive_loop(tree: &mut Vec<TreeNode>, mut run_config: RunConfig) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = ListState::default();
    state.select(Some(0));
    let mut search_query = String::new();

    let mut output_lines: Vec<String> = Vec::new();
    let mut output_rx: Option<mpsc::Receiver<OutputEvent>> = None;
    let mut is_running = false;
    let mut output_scroll: u16 = 0;
    let mut output_follow_tail = true;
    let mut run_pid: Option<u32> = None;
    let mut run_start: Option<Instant> = None;
    let mut run_passed = 0;
    let mut run_failed = 0;
    let mut run_skipped = 0;
    let mut failed_tests: Vec<FailedTestInfo> = Vec::new();
    let mut show_failure_summary = false;
    let mut failed_selection: usize = 0;
    let mut failed_detail_scroll: u16 = 0;

    let mut show_config = false;
    // 0: skip build, 1: verbosity, 2: cache, 3: output, 4: manual watch, 5: debounce
    let mut config_cursor: usize = 0;
    let mut show_help = false;
    let mut show_output_fullscreen = false;

    let root_dir = std::env::current_dir()?;
    let mut manual_watch_handle: Option<ManualWatchHandle> = None;
    apply_manual_watch_config(&root_dir, &run_config, &mut manual_watch_handle);

    loop {
        if let Some(ref rx) = output_rx {
            loop {
                match rx.try_recv() {
                    Ok(OutputEvent::Line(line)) => {
                        let trimmed = line.trim();

                        if trimmed.starts_with("Passed ") || trimmed.starts_with('✓') {
                            run_passed += 1;
                        } else if trimmed.starts_with("Failed ") || trimmed.starts_with('✗') {
                            run_failed += 1;
                        } else if trimmed.starts_with("Skipped ") || trimmed.starts_with('⚠') {
                            run_skipped += 1;
                        }

                        let line_lower = trimmed.to_lowercase();
                        if let Some(pos) = line_lower.find("passed:") {
                            let rest = line_lower[pos + 7..].trim_start();
                            let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                            if let Ok(n) = num_str.parse::<usize>() { run_passed = n; }
                        }
                        if let Some(pos) = line_lower.find("failed:") {
                            let rest = line_lower[pos + 7..].trim_start();
                            let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                            if let Ok(n) = num_str.parse::<usize>() { run_failed = n; }
                        }
                        if let Some(pos) = line_lower.find("skipped:") {
                            let rest = line_lower[pos + 8..].trim_start();
                            let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                            if let Ok(n) = num_str.parse::<usize>() { run_skipped = n; }
                        }

                        output_lines.push(line);
                    }
                    Ok(OutputEvent::Finished(code)) => {
                        is_running = false;
                        let elapsed = run_start.map(|s| format_elapsed(s.elapsed())).unwrap_or_default();

                        output_lines.push(String::new());
                        output_lines.push("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━".to_string());

                        let total = run_passed + run_failed + run_skipped;
                        let mut summary = format!("  Test Run Summary ({} total)", total);
                        if run_passed > 0 { summary.push_str(&format!("  |  ✓ {} Passed", run_passed)); }
                        if run_failed > 0 { summary.push_str(&format!("  |  ✗ {} Failed", run_failed)); }
                        if run_skipped > 0 { summary.push_str(&format!("  |  ⚠ {} Skipped", run_skipped)); }
                        output_lines.push(summary);

                        let msg = match code {
                            Some(0) => format!("  Finished successfully in {}", elapsed),
                            Some(c) => format!("  Finished with exit code {} in {}", c, elapsed),
                            None    => format!("  Process terminated after {}", elapsed),
                        };
                        output_lines.push(msg);
                        output_lines.push("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━".to_string());
                        failed_tests = extract_failed_tests(&output_lines);
                        if run_failed > 0 && !failed_tests.is_empty() {
                            show_failure_summary = true;
                            failed_selection = 0;
                            failed_detail_scroll = 0;
                        }
                        run_pid = None;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if is_running {
                            is_running = false;
                            let elapsed = run_start.map(|s| format_elapsed(s.elapsed())).unwrap_or_default();
                            output_lines.push(format!("✓ Process finished ({})", elapsed));
                            run_pid = None;
                        }
                        break;
                    }
                }
            }
        }

        // Manual watch: after debounced `.cs` changes, re-run the same set as if you pressed Enter
        if let Some(ref h) = manual_watch_handle {
            let mut fired = false;
            while h.rx.try_recv().is_ok() {
                fired = true;
            }
            if fired
                && run_config.manual_watch_enabled
                && !is_running
                && !show_config
                && !show_help
                && !show_failure_summary
            {
                let filter = build_filter(tree);
                match filter {
                    None => {
                        output_lines.push(
                            "👀 Manual watch: a `.cs` file changed, but no tests are checked. \
                             Use Space to check tests, or turn off Manual watch in Settings (Ctrl+P)."
                                .to_string(),
                        );
                    }
                    Some(filter_str) => {
                        let sel_count: usize = tree
                            .iter()
                            .filter(|n| n.is_leaf && n.is_selected)
                            .map(|n| n.test_count)
                            .sum();
                        let heading = format!("━━━ Manual watch: re-running {sel_count} checked test(s)… ━━━");
                        launch_filtered_test_run(
                            filter_str,
                            &heading,
                            &run_config,
                            &mut output_lines,
                            &mut output_rx,
                            &mut output_scroll,
                            &mut output_follow_tail,
                            &mut run_pid,
                            &mut run_start,
                            &mut run_passed,
                            &mut run_failed,
                            &mut run_skipped,
                            &mut failed_tests,
                            &mut show_failure_summary,
                            &mut failed_selection,
                            &mut failed_detail_scroll,
                            &mut is_running,
                            &mut show_output_fullscreen,
                        );
                    }
                }
            }
        }

        let query = search_query.to_lowercase();
        let mut matches_query = vec![false; tree.len()];
        if query.is_empty() {
            matches_query.fill(true);
        } else {
            for i in (0..tree.len()).rev() {
                if tree[i].label.to_lowercase().contains(&query) {
                    matches_query[i] = true;
                    let mut curr = tree[i].parent_idx;
                    while let Some(p) = curr {
                        matches_query[p] = true;
                        curr = tree[p].parent_idx;
                    }
                }
            }
        }

        let mut visible_indices = Vec::new();
        for i in 0..tree.len() {
            if !matches_query[i] { continue; }
            let mut hidden = false;
            if query.is_empty() {
                let mut curr = tree[i].parent_idx;
                while let Some(p) = curr {
                    if !tree[p].is_expanded { hidden = true; break; }
                    curr = tree[p].parent_idx;
                }
            }
            if !hidden { visible_indices.push(i); }
        }

        if let Some(sel) = state.selected() {
            if sel >= visible_indices.len() {
                state.select(if visible_indices.is_empty() { None } else { Some(visible_indices.len() - 1) });
            }
        } else if !visible_indices.is_empty() {
            state.select(Some(0));
        }

        let selected_count: usize = tree.iter().filter(|n| n.is_leaf && n.is_selected).map(|n| n.test_count).sum();
        let total_count: usize = tree.iter().filter(|n| n.is_leaf).map(|n| n.test_count).sum();

        let has_output = !output_lines.is_empty();
        let show_output_panel = has_output && (run_config.output_mode == OutputMode::Split || show_output_fullscreen);
        let area = terminal.size()?;
        let output_scroll_max = if show_output_panel {
            let constraints = if show_output_fullscreen {
                vec![Constraint::Min(0), Constraint::Length(3)]
            } else {
                vec![Constraint::Percentage(22), Constraint::Percentage(75), Constraint::Length(3)]
            };
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);
            let output_chunk_idx = if show_output_fullscreen { 0 } else { 1 };
            let output_height = chunks[output_chunk_idx].height.saturating_sub(2) as usize;
            output_lines.len().saturating_sub(output_height) as u16
        } else {
            0
        };

        if output_follow_tail {
            output_scroll = output_scroll_max;
        } else if output_scroll > output_scroll_max {
            output_scroll = output_scroll_max;
        }

        terminal.draw(|f| {
            let area = f.size();

            let constraints = if show_output_fullscreen {
                vec![Constraint::Min(0), Constraint::Length(3)]
            } else if show_output_panel {
                vec![Constraint::Percentage(22), Constraint::Percentage(75), Constraint::Length(3)]
            } else {
                vec![Constraint::Min(0), Constraint::Length(0), Constraint::Length(3)]
            };

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);

            let mut items = Vec::new();
            for (display_idx, &real_idx) in visible_indices.iter().enumerate() {
                let node = &tree[real_idx];
                let prefix = if !node.is_leaf {
                    if node.is_expanded { "▼ " } else { "▶ " }
                } else {
                    "  "
                };
                let indent = "  ".repeat(node.depth);
                let check = if node.is_selected { "[x] " } else { "[ ] " };
                let display_str = format!("{}{}{}{}", indent, prefix, check, node.label);

                let style = if Some(display_idx) == state.selected() {
                    Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD)
                } else if !node.is_leaf && node.depth == 0 {
                    Style::default().fg(Color::LightMagenta).add_modifier(Modifier::BOLD)
                } else if !node.is_leaf {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else if node.is_selected {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::DarkGray)
                };

                items.push(ListItem::new(Line::from(Span::styled(display_str, style))));
            }

            if !show_output_fullscreen {
                let title = format!(" Tests ({}/{}) ", selected_count, total_count);
                let list = List::new(items)
                    .block(Block::default().title(title).borders(Borders::ALL));
                f.render_stateful_widget(list, chunks[0], &mut state);
            }

            if show_output_panel {
                let output_text: Vec<Line> = output_lines.iter().map(|l| {
                    let style = if l.contains("Passed") || l.starts_with('✓') {
                        Style::default().fg(Color::Green)
                    } else if l.contains("Failed") || l.starts_with('✗') {
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                    } else if l.contains("warning") {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    Line::from(Span::styled(l.as_str(), style))
                }).collect();

                let output_title = if is_running {
                    let elapsed = run_start.map(|s| format_elapsed(s.elapsed())).unwrap_or_default();
                    if output_follow_tail {
                        format!(" Output (Running... {}) [follow]  |  ✓:{}  ✗:{}  ⚠:{} ", elapsed, run_passed, run_failed, run_skipped)
                    } else {
                        format!(" Output (Running... {}) [scroll]  |  ✓:{}  ✗:{}  ⚠:{} ", elapsed, run_passed, run_failed, run_skipped)
                    }
                } else {
                    let total = run_passed + run_failed + run_skipped;
                    if output_follow_tail {
                        format!(" Output (Done - {} total) [follow]  |  ✓:{}  ✗:{}  ⚠:{} ", total, run_passed, run_failed, run_skipped)
                    } else {
                        format!(" Output (Done - {} total) [scroll]  |  ✓:{}  ✗:{}  ⚠:{} ", total, run_passed, run_failed, run_skipped)
                    }
                };

                let output_chunk_idx = if show_output_fullscreen { 0 } else { 1 };
                let output_widget = Paragraph::new(output_text)
                    .block(Block::default()
                        .title(output_title)
                        .borders(Borders::ALL)
                        .border_style(if is_running {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        }))
                    .wrap(Wrap { trim: false })
                    .scroll((output_scroll, 0));
                f.render_widget(output_widget, chunks[output_chunk_idx]);
            }

            let help_text = if show_output_fullscreen && is_running {
                let elapsed = run_start.map(|s| format_elapsed(s.elapsed())).unwrap_or_default();
                format!(" Fullscreen output... {}  |  PgUp/PgDn/Home/End/mouse: scroll  Esc: cancel run ", elapsed)
            } else if show_output_fullscreen {
                " Fullscreen output  |  PgUp/PgDn/Home/End/mouse: scroll  Esc: back to tree ".to_string()
            } else if !search_query.is_empty() {
                format!(" Search: {}  |  Esc: clear  Enter: run  ?: help ", search_query)
            } else if is_running {
                let elapsed = run_start.map(|s| format_elapsed(s.elapsed())).unwrap_or_default();
                format!(" Running... {}  |  PgUp/PgDn/Home/End: output scroll  Esc: cancel ", elapsed)
            } else {
                let mut text = " Arrows: nav  Space: toggle  Enter: run  PgUp/PgDn/Home/End: output scroll ".to_string();
                if run_config.manual_watch_enabled {
                    text.push_str("  [manual watch] ");
                }
                if !failed_tests.is_empty() && run_failed > 0 {
                    text.push_str("  Ctrl+E: failed summary ");
                }
                text.push_str(" ?: help  Esc: quit ");
                text
            };

            let help = Paragraph::new(help_text)
                .style(Style::default().fg(
                    if is_running { Color::Yellow }
                    else if !search_query.is_empty() { Color::Yellow }
                    else { Color::DarkGray }
                ))
                .block(Block::default().borders(Borders::ALL));
            let help_chunk_idx = if show_output_fullscreen { 1 } else { 2 };
            f.render_widget(help, chunks[help_chunk_idx]);

            if show_config {
                let popup = centered_rect(64, 23, area);
                f.render_widget(Clear, popup);

                let v_label = match run_config.verbosity {
                    Verbosity::Normal => "Normal",
                    Verbosity::Detailed => "Detailed",
                    Verbosity::Minimal => "Minimal",
                };
                let out_label = match run_config.output_mode {
                    OutputMode::Split => "Split (tree + output)",
                    OutputMode::Fullscreen => "Fullscreen when running",
                };
                let mw = if run_config.manual_watch_enabled { "on " } else { "off" };
                let d = run_config.manual_watch_delay_ms;

                let mut config_lines: Vec<Line> = vec![
                    Line::from(""),
                    Line::from(Span::styled(" Build & output ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                ];
                let line_strings = vec![
                    format!(
                        "  {}  Skip build  (--no-build)",
                        if run_config.no_build { "[x]" } else { "[ ]" }
                    ),
                    format!(
                        "  {}  Skip restore  (--no-restore)",
                        if run_config.no_restore { "[x]" } else { "[ ]" }
                    ),
                    format!("  [∙]  Log verbosity:  {v_label}  (Space: cycle)"),
                    format!(
                        "  {}  Cache discovered tests  (F5 refresh)",
                        if run_config.cache_tests { "[x]" } else { "[ ]" }
                    ),
                    format!("  [∙]  Output:  {out_label}  (Space: toggle)"),
                    format!(
                        "  {}  Manual watch:  {mw} — re-runs only checked tests on `.cs` changes",
                        if run_config.manual_watch_enabled { "[x]" } else { "[ ]" }
                    ),
                    format!("  [∙]  Watch debounce:  {d} ms   ←/→: ±200  (applies to manual watch)"),
                ];
                for (i, line) in line_strings.iter().enumerate() {
                    let style = if i == config_cursor {
                        Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    config_lines.push(Line::from(Span::styled(line.as_str(), style)));
                }
                config_lines.push(Line::from(""));
                config_lines.push(Line::from(Span::styled(
                    "  ↑/↓: move   Space: change row   ←/→: debounce 200 ms (row 6)   Esc / Enter: save & close",
                    Style::default().fg(Color::DarkGray),
                )));
                config_lines.push(Line::from(""));

                let config_widget = Paragraph::new(config_lines)
                    .block(Block::default()
                        .title(" Settings (Ctrl+P) ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan)));
                f.render_widget(config_widget, popup);
            }

            if show_help {
                let popup = centered_rect(60, 20, area);
                f.render_widget(Clear, popup);

                let help_lines = vec![
                    Line::from(Span::styled(" Navigation", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                    Line::from("  ↑/↓       : Move selection"),
                    Line::from("  ←/→       : Expand/Collapse directories"),
                    Line::from("  a-z/0-9   : Type to search"),
                    Line::from("  Backspace : Delete search character"),
                    Line::from("  Esc       : Clear search"),
                    Line::from(""),
                    Line::from(Span::styled(" Execution & Toggles", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                    Line::from("  Space     : Toggle selection for hovered test/folder"),
                    Line::from("  Ctrl+A    : Toggle entirely all visible tests"),
                    Line::from("  Ctrl+E    : Open failed tests summary (after a failed run)"),
                    Line::from("  Ctrl+P    : Settings: manual watch, debounce, output, etc."),
                    Line::from("  Enter     : Run selected (checked) tests"),
                    Line::from("  Esc       : Cancel a running test execution"),
                    Line::from("  Esc       : Exit fullscreen output when run is finished"),
                    Line::from(""),
                    Line::from(Span::styled(" Tool Options", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                    Line::from("  F5        : Rediscover and refresh test list"),
                    Line::from(""),
                    Line::from(Span::styled(
                        " Manual watch: enable in Settings. When on, checked tests re-run on `.cs` saves.",
                        Style::default().fg(Color::DarkGray),
                    )),
                    Line::from(""),
                    Line::from(Span::styled("  Esc/Enter to close this help window", Style::default().fg(Color::DarkGray))),
                ];

                let help_widget = Paragraph::new(help_lines)
                    .block(Block::default()
                        .title(" Help Actions Mode ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow)));
                f.render_widget(help_widget, popup);
            }

            if show_failure_summary {
                let popup = centered_rect(88, 76, area);
                f.render_widget(Clear, popup);
                f.render_widget(
                    Block::default()
                        .title(" Failed Tests Summary ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Red)),
                    popup,
                );

                let inner = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(vec![Constraint::Min(0), Constraint::Length(2)])
                    .split(popup);

                let body = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints(vec![Constraint::Percentage(40), Constraint::Percentage(60)])
                    .split(inner[0]);

                let mut failed_items: Vec<ListItem> = Vec::new();
                if failed_tests.is_empty() {
                    failed_items.push(ListItem::new(Line::from(Span::styled(
                        "No failed tests captured.",
                        Style::default().fg(Color::DarkGray),
                    ))));
                } else {
                    for (idx, failed) in failed_tests.iter().enumerate() {
                        let style = if idx == failed_selection {
                            Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Red)
                        };
                        failed_items.push(ListItem::new(Line::from(Span::styled(
                            failed.name.clone(),
                            style,
                        ))));
                    }
                }

                let mut failed_state = ListState::default();
                if !failed_tests.is_empty() {
                    failed_state.select(Some(failed_selection.min(failed_tests.len().saturating_sub(1))));
                }

                let failed_list = List::new(failed_items).block(
                    Block::default()
                        .title(" Failed Tests ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Red)),
                );
                f.render_stateful_widget(failed_list, body[0], &mut failed_state);

                let details: Vec<Line> = if failed_tests.is_empty() {
                    vec![Line::from(Span::styled(
                        "No details available.",
                        Style::default().fg(Color::DarkGray),
                    ))]
                } else {
                    let selected = &failed_tests[failed_selection.min(failed_tests.len() - 1)];
                    if selected.details.is_empty() {
                        vec![Line::from(Span::styled(
                            "(No details captured for this test.)",
                            Style::default().fg(Color::DarkGray),
                        ))]
                    } else {
                        selected
                            .details
                            .iter()
                            .map(|line| {
                                let style = if line.contains("Error Message:") {
                                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                                } else if line.contains("Stack Trace:") {
                                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default().fg(Color::White)
                                };
                                Line::from(Span::styled(line.clone(), style))
                            })
                            .collect()
                    }
                };

                let detail_widget = Paragraph::new(details)
                    .block(
                        Block::default()
                            .title(" Error Details ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Red)),
                    )
                    .wrap(Wrap { trim: false })
                    .scroll((failed_detail_scroll, 0));
                f.render_widget(detail_widget, body[1]);

                let footer = Paragraph::new(
                    " ↑/↓: select  PgUp-Dn/scroll  c: copy names  d: copy error  r: re-run 1  R: re-run all  Esc: close ",
                )
                .style(Style::default().fg(Color::Red));
                f.render_widget(footer, inner[1]);
            }
        })?;

        if event::poll(std::time::Duration::from_millis(50))? {
            match event::read()? {
                Event::Mouse(mouse) => {
                    if show_failure_summary {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                failed_detail_scroll = failed_detail_scroll.saturating_sub(3);
                            }
                            MouseEventKind::ScrollDown => {
                                failed_detail_scroll = failed_detail_scroll.saturating_add(3);
                            }
                            _ => {}
                        }
                        continue;
                    }
                    if show_output_panel {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                output_follow_tail = false;
                                output_scroll = output_scroll.saturating_sub(3);
                            }
                            MouseEventKind::ScrollDown => {
                                output_scroll = output_scroll.saturating_add(3).min(output_scroll_max);
                                output_follow_tail = output_scroll >= output_scroll_max;
                            }
                            _ => {}
                        }
                    }
                    continue;
                }
                Event::Key(key) => {
                if key.kind != KeyEventKind::Press { continue; }

                if show_help {
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter => { show_help = false; }
                        _ => {}
                    }
                    continue;
                }

                if show_failure_summary {
                    match key.code {
                        KeyCode::Esc => {
                            show_failure_summary = false;
                        }
                        KeyCode::Up => {
                            if !failed_tests.is_empty() {
                                failed_selection = failed_selection.saturating_sub(1);
                                failed_detail_scroll = 0;
                            }
                        }
                        KeyCode::Down => {
                            if !failed_tests.is_empty() {
                                failed_selection = (failed_selection + 1).min(failed_tests.len() - 1);
                                failed_detail_scroll = 0;
                            }
                        }
                        KeyCode::PageUp => {
                            failed_detail_scroll = failed_detail_scroll.saturating_sub(5);
                        }
                        KeyCode::PageDown => {
                            failed_detail_scroll = failed_detail_scroll.saturating_add(5);
                        }
                        KeyCode::Home => {
                            failed_detail_scroll = 0;
                        }
                        KeyCode::End => {
                            failed_detail_scroll = u16::MAX;
                        }
                        KeyCode::Char('c') => {
                            if !failed_tests.is_empty() {
                                let names = failed_tests
                                    .iter()
                                    .map(|f| f.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                match Clipboard::new().and_then(|mut cb| cb.set_text(names)) {
                                    Ok(_) => output_lines.push("✓ Copied failed test names to clipboard.".to_string()),
                                    Err(_) => output_lines.push("✗ Could not copy failed test names to clipboard.".to_string()),
                                }
                            }
                        }
                        KeyCode::Char('d') | KeyCode::Char('m') => {
                            if !failed_tests.is_empty() {
                                let f = &failed_tests[failed_selection.min(failed_tests.len().saturating_sub(1))];
                                let mut s = f.name.clone();
                                if !f.details.is_empty() {
                                    s.push('\n');
                                    s.push_str(&f.details.join("\n"));
                                }
                                match Clipboard::new().and_then(|mut c| c.set_text(s)) {
                                    Ok(()) => output_lines
                                        .push("✓ Copied selected failure (name + message) to clipboard.".to_string()),
                                    Err(_) => output_lines
                                        .push("✗ Could not copy to clipboard.".to_string()),
                                }
                            }
                        }
                        KeyCode::Char('r') => {
                            if !is_running && !failed_tests.is_empty() {
                                let f = &failed_tests[failed_selection.min(failed_tests.len().saturating_sub(1))];
                                let fk = build_filter_for_display_names(&[filter_key_for_vstest(&f.name)]);
                                show_failure_summary = false;
                                launch_filtered_test_run(
                                    fk,
                                    "━━━ Re-running 1 failed test… ━━━",
                                    &run_config,
                                    &mut output_lines,
                                    &mut output_rx,
                                    &mut output_scroll,
                                    &mut output_follow_tail,
                                    &mut run_pid,
                                    &mut run_start,
                                    &mut run_passed,
                                    &mut run_failed,
                                    &mut run_skipped,
                                    &mut failed_tests,
                                    &mut show_failure_summary,
                                    &mut failed_selection,
                                    &mut failed_detail_scroll,
                                    &mut is_running,
                                    &mut show_output_fullscreen,
                                );
                            }
                        }
                        KeyCode::Char('R') => {
                            if !is_running && !failed_tests.is_empty() {
                                let names: Vec<String> = failed_tests.iter().map(|f| f.name.clone()).collect();
                                let n = names.len();
                                let fk = build_filter_for_display_names(&names);
                                show_failure_summary = false;
                                launch_filtered_test_run(
                                    fk,
                                    &format!("━━━ Re-running {n} failed test(s)… ━━━"),
                                    &run_config,
                                    &mut output_lines,
                                    &mut output_rx,
                                    &mut output_scroll,
                                    &mut output_follow_tail,
                                    &mut run_pid,
                                    &mut run_start,
                                    &mut run_passed,
                                    &mut run_failed,
                                    &mut run_skipped,
                                    &mut failed_tests,
                                    &mut show_failure_summary,
                                    &mut failed_selection,
                                    &mut failed_detail_scroll,
                                    &mut is_running,
                                    &mut show_output_fullscreen,
                                );
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                if show_config {
                    let debounce_clamp = |v: u32| v.clamp(200, 20_000);
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter => {
                            show_config = false;
                            run_config.manual_watch_delay_ms = debounce_clamp(run_config.manual_watch_delay_ms);
                            run_config.save();
                            apply_manual_watch_config(&root_dir, &run_config, &mut manual_watch_handle);
                        }
                        KeyCode::Up => {
                            if config_cursor > 0 {
                                config_cursor -= 1;
                            }
                        }
                        KeyCode::Down => {
                            if config_cursor < 6 {
                                config_cursor += 1;
                            }
                        }
                        KeyCode::Left => {
                            if config_cursor == 6 {
                                run_config.manual_watch_delay_ms = debounce_clamp(
                                    run_config.manual_watch_delay_ms.saturating_sub(200),
                                );
                            }
                        }
                        KeyCode::Right => {
                            if config_cursor == 6 {
                                run_config.manual_watch_delay_ms = debounce_clamp(
                                    (run_config.manual_watch_delay_ms + 200).min(20_000),
                                );
                            }
                        }
                        KeyCode::Char(' ') => {
                            match config_cursor {
                                0 => run_config.no_build = !run_config.no_build,
                                1 => run_config.no_restore = !run_config.no_restore,
                                2 => {
                                    run_config.verbosity = match run_config.verbosity {
                                        Verbosity::Normal => Verbosity::Detailed,
                                        Verbosity::Detailed => Verbosity::Minimal,
                                        Verbosity::Minimal => Verbosity::Normal,
                                    };
                                }
                                3 => {
                                    run_config.cache_tests = !run_config.cache_tests;
                                    if !run_config.cache_tests {
                                        let _ = std::fs::remove_file(".dotest_cache.json");
                                    } else {
                                        super::discover_and_cache(run_config.no_restore).ok();
                                    }
                                }
                                4 => {
                                    run_config.output_mode = if run_config.output_mode == OutputMode::Split {
                                        OutputMode::Fullscreen
                                    } else {
                                        OutputMode::Split
                                    };
                                }
                                5 => {
                                    run_config.manual_watch_enabled = !run_config.manual_watch_enabled;
                                    run_config.manual_watch_delay_ms = debounce_clamp(run_config.manual_watch_delay_ms);
                                    apply_manual_watch_config(
                                        &root_dir,
                                        &run_config,
                                        &mut manual_watch_handle,
                                    );
                                }
                                6 => {}
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                if is_running {
                    match key.code {
                        KeyCode::PageUp => {
                            if show_output_panel {
                                output_follow_tail = false;
                                output_scroll = output_scroll.saturating_sub(5);
                            }
                        }
                        KeyCode::PageDown => {
                            if show_output_panel {
                                output_scroll = output_scroll.saturating_add(5).min(output_scroll_max);
                                output_follow_tail = output_scroll >= output_scroll_max;
                            }
                        }
                        KeyCode::Home => {
                            if show_output_panel {
                                output_follow_tail = false;
                                output_scroll = 0;
                            }
                        }
                        KeyCode::End => {
                            if show_output_panel {
                                output_follow_tail = true;
                                output_scroll = output_scroll_max;
                            }
                        }
                        KeyCode::Esc => {
                            if let Some(pid) = run_pid.take() {
                                kill_process(pid);
                            }
                            is_running = false;
                            let elapsed = run_start.map(|s| format_elapsed(s.elapsed())).unwrap_or_default();
                            output_lines.push(String::new());
                            output_lines.push(format!("⚠ Cancelled ({})", elapsed));
                            output_rx = None;
                        }
                        _ => {}
                    }
                    continue;
                }

                if show_output_fullscreen {
                    match key.code {
                        KeyCode::PageUp => {
                            if show_output_panel {
                                output_follow_tail = false;
                                output_scroll = output_scroll.saturating_sub(5);
                            }
                        }
                        KeyCode::PageDown => {
                            if show_output_panel {
                                output_scroll = output_scroll.saturating_add(5).min(output_scroll_max);
                                output_follow_tail = output_scroll >= output_scroll_max;
                            }
                        }
                        KeyCode::Home => {
                            if show_output_panel {
                                output_follow_tail = false;
                                output_scroll = 0;
                            }
                        }
                        KeyCode::End => {
                            if show_output_panel {
                                output_follow_tail = true;
                                output_scroll = output_scroll_max;
                            }
                        }
                        KeyCode::Esc => {
                            show_output_fullscreen = false;
                        }
                        _ => {}
                    }
                    continue;
                }

                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
                    let any_leaf_selected = tree.iter().any(|n| n.is_leaf && n.is_selected);
                    let to_state = !any_leaf_selected;
                    for node in tree.iter_mut() { node.is_selected = to_state; }
                    continue;
                }

                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
                    if !failed_tests.is_empty() && run_failed > 0 {
                        show_failure_summary = true;
                        failed_selection = failed_selection.min(failed_tests.len() - 1);
                        failed_detail_scroll = 0;
                    }
                    continue;
                }

                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
                    show_config = true;
                    config_cursor = 0;
                    continue;
                }

                if key.code == KeyCode::F(5) {
                    output_lines.push("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━".to_string());
                    output_lines.push("🔄 Rediscovering tests... please wait.".to_string());

                    if let Ok(tests) = discover_tests(true, run_config.no_restore) {
                        if run_config.cache_tests {
                            if let Ok(s) = serde_json::to_string(&tests) {
                                let _ = std::fs::write(".dotest_cache.json", s);
                            }
                        }
                        *tree = build_flat_tree(&tests);
                        state.select(Some(0));
                        search_query.clear();
                        let total: usize = tests.iter().map(|(_, _, c)| c).sum();
                        output_lines.push(format!("✓ Found {} tests ({} methods).", total, tests.len()));
                    } else {
                        output_lines.push("✗ Failed to discover tests.".to_string());
                    }
                    continue;
                }

                if key.code == KeyCode::Char('?') || key.code == KeyCode::F(1) {
                    show_help = true;
                    continue;
                }

                match key.code {
                    KeyCode::PageUp => {
                        if show_output_panel {
                            output_follow_tail = false;
                            output_scroll = output_scroll.saturating_sub(5);
                        }
                    }
                    KeyCode::PageDown => {
                        if show_output_panel {
                            output_scroll = output_scroll.saturating_add(5).min(output_scroll_max);
                            output_follow_tail = output_scroll >= output_scroll_max;
                        }
                    }
                    KeyCode::Home => {
                        if show_output_panel {
                            output_follow_tail = false;
                            output_scroll = 0;
                        }
                    }
                    KeyCode::End => {
                        if show_output_panel {
                            output_follow_tail = true;
                            output_scroll = output_scroll_max;
                        }
                    }
                    KeyCode::Esc => {
                        if !search_query.is_empty() {
                            search_query.clear();
                            state.select(Some(0));
                        } else {
                            break;
                        }
                    }

                    KeyCode::Enter => {
                        let filter = build_filter(tree);
                        if let Some(filter_str) = filter {
                            let sel_count: usize = tree
                                .iter()
                                .filter(|n| n.is_leaf && n.is_selected)
                                .map(|n| n.test_count)
                                .sum();
                            let heading = format!("━━━ Running {sel_count} selected test(s)… ━━━");
                            launch_filtered_test_run(
                                filter_str,
                                &heading,
                                &run_config,
                                &mut output_lines,
                                &mut output_rx,
                                &mut output_scroll,
                                &mut output_follow_tail,
                                &mut run_pid,
                                &mut run_start,
                                &mut run_passed,
                                &mut run_failed,
                                &mut run_skipped,
                                &mut failed_tests,
                                &mut show_failure_summary,
                                &mut failed_selection,
                                &mut failed_detail_scroll,
                                &mut is_running,
                                &mut show_output_fullscreen,
                            );
                        }
                    }

                    KeyCode::Up => {
                        if !visible_indices.is_empty() {
                            let i = match state.selected() {
                                Some(0) | None => visible_indices.len() - 1,
                                Some(i) => i - 1,
                            };
                            state.select(Some(i));
                        }
                    }
                    KeyCode::Down => {
                        if !visible_indices.is_empty() {
                            let i = match state.selected() {
                                Some(i) if i >= visible_indices.len() - 1 => 0,
                                Some(i) => i + 1,
                                None => 0,
                            };
                            state.select(Some(i));
                        }
                    }
                    KeyCode::Left => {
                        if let Some(di) = state.selected() {
                            if di < visible_indices.len() {
                                let ri = visible_indices[di];
                                if !tree[ri].is_leaf && tree[ri].is_expanded {
                                    tree[ri].is_expanded = false;
                                } else if let Some(pi) = tree[ri].parent_idx {
                                    if let Some(pdi) = visible_indices.iter().position(|&r| r == pi) {
                                        state.select(Some(pdi));
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Right => {
                        if let Some(di) = state.selected() {
                            if di < visible_indices.len() {
                                let ri = visible_indices[di];
                                if !tree[ri].is_leaf && !tree[ri].is_expanded {
                                    tree[ri].is_expanded = true;
                                }
                            }
                        }
                    }
                    KeyCode::Char(' ') => {
                        if let Some(di) = state.selected() {
                            if di < visible_indices.len() {
                                let ri = visible_indices[di];
                                let new_state = !tree[ri].is_selected;
                                tree[ri].is_selected = new_state;
                                let mut j = ri + 1;
                                while j < tree.len() && tree[j].depth > tree[ri].depth {
                                    tree[j].is_selected = new_state;
                                    j += 1;
                                }
                                sync_parents(tree);
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        search_query.pop();
                        state.select(Some(0));
                    }
                    KeyCode::Char(c) => {
                        if c.is_alphanumeric() || c.is_ascii_punctuation() || c == ' ' {
                            search_query.push(c);
                            state.select(Some(0));
                        }
                    }
                    _ => {}
                }
            }
                _ => {}
            }
        }
    }

    if let Some(h) = manual_watch_handle {
        h.stop();
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(())
}
