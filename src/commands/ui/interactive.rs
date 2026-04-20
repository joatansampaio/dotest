use anyhow::Result;
use crate::core::executor::discover_tests;
use crate::core::tree::{build_flat_tree, TreeNode};
use std::io;
use std::sync::mpsc;
use std::time::Instant;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
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

use super::config::{RunConfig, Verbosity};
use super::filter::{build_filter, sync_parents};
use super::layout::{centered_rect, format_elapsed};
use super::output::{kill_process, OutputEvent, spawn_test_run};

pub(super) fn run_interactive_loop(tree: &mut Vec<TreeNode>, mut run_config: RunConfig) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = ListState::default();
    state.select(Some(0));
    let mut search_query = String::new();

    let mut output_lines: Vec<String> = Vec::new();
    let mut output_rx: Option<mpsc::Receiver<OutputEvent>> = None;
    let mut is_running = false;
    let mut output_scroll: u16 = 0;
    let mut run_pid: Option<u32> = None;
    let mut run_start: Option<Instant> = None;
    let mut run_passed = 0;
    let mut run_failed = 0;
    let mut run_skipped = 0;

    let mut show_config = false;
    let mut config_cursor: usize = 0;
    let mut show_help = false;

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
            let mut curr = tree[i].parent_idx;
            while let Some(p) = curr {
                if !tree[p].is_expanded { hidden = true; break; }
                curr = tree[p].parent_idx;
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

        terminal.draw(|f| {
            let area = f.size();

            let constraints = if has_output {
                vec![Constraint::Percentage(45), Constraint::Percentage(52), Constraint::Length(3)]
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

            let title = format!(" Tests ({}/{}) ", selected_count, total_count);
            let list = List::new(items)
                .block(Block::default().title(title).borders(Borders::ALL));
            f.render_stateful_widget(list, chunks[0], &mut state);

            if has_output {
                let output_height = chunks[1].height.saturating_sub(2) as usize;
                output_scroll = output_lines.len().saturating_sub(output_height) as u16;

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
                    format!(" Output (Running... {})  |  ✓:{}  ✗:{}  ⚠:{} ", elapsed, run_passed, run_failed, run_skipped)
                } else {
                    let total = run_passed + run_failed + run_skipped;
                    format!(" Output (Done - {} total)  |  ✓:{}  ✗:{}  ⚠:{} ", total, run_passed, run_failed, run_skipped)
                };

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
                f.render_widget(output_widget, chunks[1]);
            }

            let help_text = if !search_query.is_empty() {
                format!(" Search: {}  |  Esc: clear  Enter: run  ?: help ", search_query)
            } else if is_running {
                let elapsed = run_start.map(|s| format_elapsed(s.elapsed())).unwrap_or_default();
                format!(" Running... {}  |  Esc: cancel ", elapsed)
            } else {
                " Arrows: nav  Space: toggle  Enter: run  ?: help  Esc: quit ".to_string()
            };

            let help = Paragraph::new(help_text)
                .style(Style::default().fg(
                    if is_running { Color::Yellow }
                    else if !search_query.is_empty() { Color::Yellow }
                    else { Color::DarkGray }
                ))
                .block(Block::default().borders(Borders::ALL));
            f.render_widget(help, chunks[2]);

            if show_config {
                let popup = centered_rect(54, 10, area);
                f.render_widget(Clear, popup);

                let config_items = vec![
                    format!("  {} Skip build (--no-build)", if run_config.no_build { "[x]" } else { "[ ]" }),
                    format!("  {} Verbose (Normal)", if run_config.verbosity == Verbosity::Normal { "(*)" } else { "( )" }),
                    format!("  {} Verbose (Detailed)", if run_config.verbosity == Verbosity::Detailed { "(*)" } else { "( )" }),
                    format!("  {} Verbose (Minimal)", if run_config.verbosity == Verbosity::Minimal { "(*)" } else { "( )" }),
                    format!("  {} Cache discovered tests", if run_config.cache_tests { "[x]" } else { "[ ]" }),
                ];

                let mut config_lines: Vec<Line> = Vec::new();
                config_lines.push(Line::from(""));
                for (i, item) in config_items.iter().enumerate() {
                    let style = if i == config_cursor {
                        Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    config_lines.push(Line::from(Span::styled(item.as_str(), style)));
                }
                config_lines.push(Line::from(""));
                config_lines.push(Line::from(Span::styled(
                    "  Space: toggle  |  Esc/Enter: close & save",
                    Style::default().fg(Color::DarkGray),
                )));

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
                    Line::from("  Enter     : Run selected tests"),
                    Line::from("  Esc       : Cancel a running test execution"),
                    Line::from(""),
                    Line::from(Span::styled(" Tool Options", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                    Line::from("  Ctrl+P    : Open Settings/Configuration"),
                    Line::from("  F5        : Rediscover and refresh test list"),
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
        })?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press { continue; }

                if show_help {
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter => { show_help = false; }
                        _ => {}
                    }
                    continue;
                }

                if show_config {
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter => {
                            show_config = false;
                            run_config.save();
                        }
                        KeyCode::Up => { if config_cursor > 0 { config_cursor -= 1; } }
                        KeyCode::Down => { if config_cursor < 4 { config_cursor += 1; } }
                        KeyCode::Char(' ') => {
                            match config_cursor {
                                0 => run_config.no_build = !run_config.no_build,
                                1 => run_config.verbosity = Verbosity::Normal,
                                2 => run_config.verbosity = Verbosity::Detailed,
                                3 => run_config.verbosity = Verbosity::Minimal,
                                4 => {
                                    run_config.cache_tests = !run_config.cache_tests;
                                    if !run_config.cache_tests {
                                        let _ = std::fs::remove_file(".dotest_cache.json");
                                    } else {
                                        super::discover_and_cache().ok();
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                if is_running {
                    if key.code == KeyCode::Esc {
                        if let Some(pid) = run_pid.take() {
                            kill_process(pid);
                        }
                        is_running = false;
                        let elapsed = run_start.map(|s| format_elapsed(s.elapsed())).unwrap_or_default();
                        output_lines.push(String::new());
                        output_lines.push(format!("⚠ Cancelled ({})", elapsed));
                        output_rx = None;
                    }
                    continue;
                }

                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
                    let any_leaf_selected = tree.iter().any(|n| n.is_leaf && n.is_selected);
                    let to_state = !any_leaf_selected;
                    for node in tree.iter_mut() { node.is_selected = to_state; }
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

                    if let Ok(tests) = discover_tests(true) {
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
                            output_lines.clear();
                            let sel_count: usize = tree.iter().filter(|n| n.is_leaf && n.is_selected).map(|n| n.test_count).sum();
                            output_lines.push(format!("━━━ Running {} selected tests... ━━━", sel_count));
                            if filter_str.is_empty() {
                                output_lines.push("  (all tests, no filter)".to_string());
                            }
                            output_lines.push(String::new());
                            match spawn_test_run(Some(filter_str), &run_config) {
                                Ok((rx, pid)) => {
                                    output_rx = Some(rx);
                                    run_pid = Some(pid);
                                    is_running = true;
                                    run_start = Some(Instant::now());
                                    run_passed = 0;
                                    run_failed = 0;
                                    run_skipped = 0;
                                }
                                Err(e) => {
                                    output_lines.push(format!("Error: {}", e));
                                }
                            }
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
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
