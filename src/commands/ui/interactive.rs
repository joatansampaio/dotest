use anyhow::Result;
use crate::core::executor::discover_tests;
use crate::core::tree::{build_flat_tree, TreeNode};
use arboard::Clipboard;
use std::io;
use std::sync::mpsc;
use std::time::Instant;

use super::failed_tests::{
    build_filter_for_display_names, extract_failed_tests, filter_key_for_vstest, FailedTestInfo,
};
use super::failure_summary::{
    clicked_detail_index, compute_failure_detail_link_hover, failed_detail_styled_line_with_hover,
    failed_summary_detail_rect, failed_summary_list_rect, open_path_in_default_editor,
    parse_stack_trace_target,
};
use super::manual_watch::{apply_manual_watch_config, ManualWatchHandle};
use super::test_run::launch_filtered_test_run;

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind},
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
use super::layout::{centered_rect, format_elapsed, output_wrapped_scroll_max, styled_output_lines};
use super::output::{kill_process, OutputEvent};

/// Interactive TUI: test tree, run output, settings, and failure summary.
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
    // Detail line index for stack links while the pointer is over that line in Error Details.
    let mut failure_detail_hover: Option<usize> = None;

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
                        failed_tests = extract_failed_tests(&output_lines);
                        if failed_tests.is_empty() {
                            failed_selection = 0;
                            failed_detail_scroll = 0;
                        } else {
                            failed_selection = failed_selection.min(failed_tests.len() - 1);
                        }
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
                            failure_detail_hover = None;
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
            {
                if show_failure_summary {
                    show_failure_summary = false;
                    failure_detail_hover = None;
                }
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
                        failure_detail_hover = None;
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
            let inner_w = chunks[output_chunk_idx].width.saturating_sub(2);
            let inner_h = chunks[output_chunk_idx].height.saturating_sub(2);
            output_wrapped_scroll_max(&output_lines, inner_w, inner_h)
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
                let output_text = styled_output_lines(&output_lines);

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

            let watch_hint = if run_config.manual_watch_enabled {
                "  ● WATCH ON "
            } else {
                ""
            };
            let help_text = if show_output_fullscreen && is_running {
                let elapsed = run_start.map(|s| format_elapsed(s.elapsed())).unwrap_or_default();
                format!(
                    " Fullscreen output... {}  |  PgUp/PgDn/Home/End/mouse: scroll  Ctrl+E: failed summary  Esc: cancel run{}",
                    elapsed, watch_hint
                )
            } else if show_output_fullscreen {
                format!(
                    " Fullscreen output  |  PgUp/PgDn/Home/End/mouse: scroll  Esc: back to tree{}",
                    watch_hint
                )
            } else if !search_query.is_empty() {
                format!(
                    " Search: {}  |  Esc: clear  Enter: run  ?: help{}",
                    search_query, watch_hint
                )
            } else if is_running {
                let elapsed = run_start.map(|s| format_elapsed(s.elapsed())).unwrap_or_default();
                format!(
                    " Running... {}  |  PgUp/PgDn/Home/End: output scroll  Ctrl+E: failed summary  Esc: cancel{}",
                    elapsed, watch_hint
                )
            } else {
                let mut text = " Arrows: nav  Space: toggle  Enter: run  PgUp/PgDn/Home/End: output scroll ".to_string();
                if run_config.manual_watch_enabled {
                    text.push_str(watch_hint);
                }
                text.push_str("  Ctrl+E: failed summary ");
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
                let popup = centered_rect(64, 22, area);
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
                    "  ↑/↓: move   Space: change row   ←/→: debounce 200 ms (row 5)   Esc / Enter: save & close",
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
                let popup = centered_rect(62, 26, area);
                f.render_widget(Clear, popup);

                let help_lines = vec![
                    Line::from(Span::styled(" Navigation", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                    Line::from("  ↑/↓       : Move selection in the test tree"),
                    Line::from("  ←/→       : Collapse / expand folders"),
                    Line::from("  a-z/0-9   : Type to filter (search) the tree"),
                    Line::from("  Backspace : Delete last search character"),
                    Line::from("  Esc       : Clear search, or quit if search is empty"),
                    Line::from(""),
                    Line::from(Span::styled(" Running & output", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                    Line::from("  Enter     : Run all checked tests"),
                    Line::from("  Esc       : Cancel a run in progress, or leave fullscreen output"),
                    Line::from("  PgUp/Dn, Home, End : Scroll the output pane (when visible)"),
                    Line::from("  Mouse wheel : Scroll output when the output panel is focused"),
                    Line::from("  Output title shows [follow] (tail) vs [scroll] (manual)"),
                    Line::from(""),
                    Line::from(Span::styled(" Toggles & shortcuts", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                    Line::from("  Space     : Toggle checkmark on test or folder branch"),
                    Line::from("  Ctrl+A    : Toggle all visible tests (select all or clear all)"),
                    Line::from("  Ctrl+W    : Toggle manual watch on/off (saved). ● WATCH ON in status when active"),
                    Line::from("  Ctrl+P    : Settings (verbosity, output mode, watch debounce, …)"),
                    Line::from("  Ctrl+E    : Failed tests summary (opens immediately and fills as failures arrive)"),
                    Line::from(""),
                    Line::from(Span::styled(" Tool & discovery", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                    Line::from("  F1 / ?    : Open this help"),
                    Line::from("  F5        : Rediscover tests and refresh the tree (updates the on-disk list)"),
                    Line::from("  Startup   : Skips discovery when `.dotest_cache.json` matches repo/file fingerprint"),
                    Line::from(""),
                    Line::from(Span::styled(" Manual watch", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                    Line::from("  When ON, saving a `.cs` file re-runs only the tests you have checked."),
                    Line::from("  Debounce delay is adjusted in Settings (Ctrl+P)."),
                    Line::from(""),
                    Line::from(Span::styled(" Failed summary (Ctrl+E)", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                    Line::from("  Shift+↑/↓: Select failed test  |  ↑/↓/PgUp/Dn: Scroll error details (not the list)"),
                    Line::from("  r: Re-run one  |  R: Re-run all  |  c/d: copy  |  click list: pick  |  Esc: close"),
                    Line::from(""),
                    Line::from(Span::styled("  Esc or Enter closes this help window.", Style::default().fg(Color::DarkGray))),
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
                            .enumerate()
                            .map(|(i, line)| {
                                failed_detail_styled_line_with_hover(line, i, failure_detail_hover)
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
                    " Shift+↑/↓: pick test  |  ↑/↓/Pg: scroll  |  link highlights under pointer  |  click to open  |  c d r R Esc ",
                )
                .style(Style::default().fg(Color::Red));
                f.render_widget(footer, inner[1]);
            }
        })?;

        if event::poll(std::time::Duration::from_millis(50))? {
            match event::read()? {
                Event::Mouse(mouse) => {
                    if show_failure_summary {
                        let area = terminal.size()?;
                        let list_rect = failed_summary_list_rect(area);
                        let list_inner_x = list_rect.x.saturating_add(1);
                        let list_inner_y = list_rect.y.saturating_add(1);
                        let list_inner_width = list_rect.width.saturating_sub(2);
                        let list_inner_height = list_rect.height.saturating_sub(2);
                        let mouse_in_list_pane = list_inner_width > 0
                            && list_inner_height > 0
                            && mouse.column >= list_inner_x
                            && mouse.column < list_inner_x.saturating_add(list_inner_width)
                            && mouse.row >= list_inner_y
                            && mouse.row < list_inner_y.saturating_add(list_inner_height);
                        let detail_rect = failed_summary_detail_rect(area);
                        let detail_inner_x = detail_rect.x.saturating_add(1);
                        let detail_inner_y = detail_rect.y.saturating_add(1);
                        let detail_inner_width = detail_rect.width.saturating_sub(2);
                        let detail_inner_height = detail_rect.height.saturating_sub(2);
                        let mouse_in_detail_pane = detail_inner_width > 0
                            && detail_inner_height > 0
                            && mouse.column >= detail_inner_x
                            && mouse.column < detail_inner_x.saturating_add(detail_inner_width)
                            && mouse.row >= detail_inner_y
                            && mouse.row < detail_inner_y.saturating_add(detail_inner_height);

                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                if !failed_tests.is_empty() {
                                    if mouse_in_list_pane {
                                        let rel = mouse
                                            .row
                                            .saturating_sub(list_inner_y) as usize;
                                        if rel < failed_tests.len() {
                                            failed_selection = rel;
                                            failed_detail_scroll = 0;
                                            failure_detail_hover = None;
                                        }
                                    } else if mouse_in_detail_pane {
                                        let selected = &failed_tests[failed_selection.min(failed_tests.len() - 1)];
                                        if let Some(detail_index) = clicked_detail_index(
                                            &selected.details,
                                            detail_inner_width,
                                            failed_detail_scroll,
                                            mouse.row.saturating_sub(detail_inner_y),
                                        ) {
                                            if let Some(target) = parse_stack_trace_target(&selected.details[detail_index]) {
                                                match open_path_in_default_editor(&target.path) {
                                                    Ok(()) => {
                                                        let message = if let Some(line_number) = target.line_number {
                                                            format!("✓ Opened {} (line {}).", target.path, line_number)
                                                        } else {
                                                            format!("✓ Opened {}.", target.path)
                                                        };
                                                        output_lines.push(message);
                                                    }
                                                    Err(e) => output_lines.push(format!(
                                                        "✗ Could not open {}: {}",
                                                        target.path, e
                                                    )),
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            MouseEventKind::ScrollUp => {
                                if mouse_in_detail_pane {
                                    failed_detail_scroll = failed_detail_scroll.saturating_sub(3);
                                }
                                failure_detail_hover = compute_failure_detail_link_hover(
                                    &failed_tests,
                                    failed_selection,
                                    detail_inner_width,
                                    detail_inner_y,
                                    failed_detail_scroll,
                                    mouse_in_detail_pane,
                                    mouse.row,
                                );
                            }
                            MouseEventKind::ScrollDown => {
                                if mouse_in_detail_pane {
                                    failed_detail_scroll = failed_detail_scroll.saturating_add(3);
                                }
                                failure_detail_hover = compute_failure_detail_link_hover(
                                    &failed_tests,
                                    failed_selection,
                                    detail_inner_width,
                                    detail_inner_y,
                                    failed_detail_scroll,
                                    mouse_in_detail_pane,
                                    mouse.row,
                                );
                            }
                            MouseEventKind::Moved => {
                                failure_detail_hover = compute_failure_detail_link_hover(
                                    &failed_tests,
                                    failed_selection,
                                    detail_inner_width,
                                    detail_inner_y,
                                    failed_detail_scroll,
                                    mouse_in_detail_pane,
                                    mouse.row,
                                );
                            }
                            MouseEventKind::Drag(_) => {
                                failure_detail_hover = compute_failure_detail_link_hover(
                                    &failed_tests,
                                    failed_selection,
                                    detail_inner_width,
                                    detail_inner_y,
                                    failed_detail_scroll,
                                    mouse_in_detail_pane,
                                    mouse.row,
                                );
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
                    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                    match key.code {
                        KeyCode::Esc => {
                            show_failure_summary = false;
                            failure_detail_hover = None;
                        }
                        KeyCode::Up => {
                            if !failed_tests.is_empty() {
                                if shift {
                                    failed_selection = failed_selection.saturating_sub(1);
                                    failed_detail_scroll = 0;
                                    failure_detail_hover = None;
                                } else {
                                    failed_detail_scroll = failed_detail_scroll.saturating_sub(1);
                                    failure_detail_hover = None;
                                }
                            }
                        }
                        KeyCode::Down => {
                            if !failed_tests.is_empty() {
                                if shift {
                                    failed_selection = (failed_selection + 1).min(failed_tests.len() - 1);
                                    failed_detail_scroll = 0;
                                    failure_detail_hover = None;
                                } else {
                                    failed_detail_scroll = failed_detail_scroll.saturating_add(1);
                                    failure_detail_hover = None;
                                }
                            }
                        }
                        KeyCode::PageUp => {
                            failed_detail_scroll = failed_detail_scroll.saturating_sub(5);
                            failure_detail_hover = None;
                        }
                        KeyCode::PageDown => {
                            failed_detail_scroll = failed_detail_scroll.saturating_add(5);
                            failure_detail_hover = None;
                        }
                        KeyCode::Home => {
                            if shift {
                                if !failed_tests.is_empty() {
                                    failed_selection = 0;
                                    failed_detail_scroll = 0;
                                    failure_detail_hover = None;
                                }
                            } else {
                                failed_detail_scroll = 0;
                                failure_detail_hover = None;
                            }
                        }
                        KeyCode::End => {
                            if shift {
                                if !failed_tests.is_empty() {
                                    failed_selection = failed_tests.len().saturating_sub(1);
                                    failed_detail_scroll = 0;
                                    failure_detail_hover = None;
                                }
                            } else {
                                failed_detail_scroll = u16::MAX;
                                failure_detail_hover = None;
                            }
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
                                failure_detail_hover = None;
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
                                failure_detail_hover = None;
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
                            if config_cursor < 5 {
                                config_cursor += 1;
                            }
                        }
                        KeyCode::Left => {
                            if config_cursor == 5 {
                                run_config.manual_watch_delay_ms = debounce_clamp(
                                    run_config.manual_watch_delay_ms.saturating_sub(200),
                                );
                            }
                        }
                        KeyCode::Right => {
                            if config_cursor == 5 {
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
                                    run_config.output_mode = if run_config.output_mode == OutputMode::Split {
                                        OutputMode::Fullscreen
                                    } else {
                                        OutputMode::Split
                                    };
                                }
                                4 => {
                                    run_config.manual_watch_enabled = !run_config.manual_watch_enabled;
                                    run_config.manual_watch_delay_ms = debounce_clamp(run_config.manual_watch_delay_ms);
                                    apply_manual_watch_config(
                                        &root_dir,
                                        &run_config,
                                        &mut manual_watch_handle,
                                    );
                                }
                                5 => {}
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                if is_running {
                    match key.code {
                        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            show_failure_summary = true;
                            if failed_tests.is_empty() {
                                
                                failed_selection = 0;
                            } else {
                                failed_selection = failed_selection.min(failed_tests.len() - 1);
                            }
                            failed_detail_scroll = 0;
                            failure_detail_hover = None;
                        }
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
                    show_failure_summary = true;
                    if failed_tests.is_empty() {
                        failed_selection = 0;
                    } else {
                        failed_selection = failed_selection.min(failed_tests.len() - 1);
                    }
                    failed_detail_scroll = 0;
                    failure_detail_hover = None;
                    continue;
                }

                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
                    show_config = true;
                    config_cursor = 0;
                    continue;
                }

                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('w' | 'W'))
                {
                    run_config.manual_watch_enabled = !run_config.manual_watch_enabled;
                    run_config.manual_watch_delay_ms = run_config.manual_watch_delay_ms.clamp(200, 20_000);
                    apply_manual_watch_config(&root_dir, &run_config, &mut manual_watch_handle);
                    run_config.save();
                    if run_config.manual_watch_enabled {
                        output_lines.push("✓ Manual watch ON — checked tests re-run when you save `.cs` files.".to_string());
                    } else {
                        output_lines.push("○ Manual watch OFF.".to_string());
                    }
                    continue;
                }

                if key.code == KeyCode::F(5) {
                    output_lines.push("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━".to_string());
                    output_lines.push("🔄 Rediscovering tests... please wait.".to_string());

                    if let Ok(tests) = discover_tests(true, run_config.no_restore) {
                        let _ = super::discovery_cache::save_discovery_cache(&tests);
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
                            failure_detail_hover = None;
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
