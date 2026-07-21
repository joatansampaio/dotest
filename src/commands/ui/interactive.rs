use crate::core::discovery::DiscoveredTest;
use crate::core::executor::{compose_test_filter, discover_tests};
use crate::core::tree::{build_flat_tree, sync_parents, TreeNode, TreeState};
use anyhow::Result;
use arboard::Clipboard;
use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::failed_tests::{
    build_filter_for_display_names, extract_failed_tests, filter_key_for_vstest, FailedTestInfo,
};
use super::failure_summary::{
    clamp_failed_summary_list_pane_cols, clicked_detail_index,
    compute_failure_summary_list_pane_cols, compute_failure_detail_link_hover,
    failed_detail_styled_line_with_hover, failed_summary_detail_rect, failed_summary_list_rect,
    open_path_in_default_editor, parse_stack_trace_target,
};
use super::manual_watch::{apply_manual_watch_config, ManualWatchHandle};
use super::test_run::launch_filtered_test_run;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        block::{Position, Title},
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
    },
    Terminal,
};

use super::config::{OutputMode, RunConfig, Verbosity, ViewMode};
use super::filter::{build_filter, build_selected_run_request};
use super::layout::{
    centered_rect, format_elapsed, output_wrapped_scroll_max, styled_output_lines,
};
use super::output::{
    kill_process, spawn_churn_sidecar, spawn_test_run_for_target, ChurnSidecarRequest,
    OutputEvent,
};
use super::presets::{apply_preset_selection, collect_selected_tests, save_preset};
use super::run_results::{LeafStatus, RunResultsState};

type DiscoveryEntries = Vec<DiscoveredTest>;
type RediscoveryResult = std::result::Result<DiscoveryEntries, String>;

const DEFAULT_TESTS_PANE_PERCENT: u16 = 22;
const PANE_RESIZE_STEP_ROWS: u16 = 1;
const MIN_TESTS_PANE_ROWS: u16 = 3;
const MIN_OUTPUT_PANE_ROWS: u16 = 1;
const STATUS_PANE_ROWS: u16 = 3;
const QUICK_CHURN_LIMIT: usize = 100;
const CHURN_OUTPUT_TAIL_LINES: usize = 400;
const CHURN_OUTPUT_OMITTED_MARKER: &str = "  ... older churn output omitted ...";

fn churn_duration_stats_line(durations: &[Duration]) -> Option<String> {
    if durations.is_empty() {
        return None;
    }

    let min = durations.iter().min().copied().unwrap_or_default();
    let max = durations.iter().max().copied().unwrap_or_default();
    let total_nanos: u128 = durations.iter().map(|d| d.as_nanos()).sum();
    let avg_nanos = total_nanos / durations.len() as u128;
    let avg_nanos_u64 = avg_nanos.min(u64::MAX as u128) as u64;
    let avg = Duration::from_nanos(avg_nanos_u64);

    Some(format!(
        "  Duration stats: avg {}  |  min {}  |  max {}",
        format_elapsed(avg),
        format_elapsed(min),
        format_elapsed(max)
    ))
}

fn parse_churn_iteration_line(line: &str, marker: &str) -> Option<usize> {
    let rest = line.strip_prefix("Iteration ")?;
    let (iteration, suffix) = rest.split_once(' ')?;
    if !suffix.contains(marker) {
        return None;
    }
    iteration.parse().ok()
}

fn parse_churn_iteration_start_line(line: &str) -> Option<usize> {
    parse_churn_iteration_line(line, "↻ Starting")
}

fn parse_churn_iteration_passed_line(line: &str) -> Option<usize> {
    parse_churn_iteration_line(line, "✓ Passed")
}

fn parse_churn_iteration_failed_line(line: &str) -> Option<usize> {
    parse_churn_iteration_line(line, "✗ Failed")
}

fn churn_output_prefix_lines(output_lines: &[String]) -> usize {
    match output_lines.get(1) {
        Some(line) if line.starts_with("  Iteration limit:") => output_lines.len().min(2),
        _ => output_lines.len().min(1),
    }
}

fn trim_churn_output_lines(output_lines: &mut Vec<String>) {
    let prefix_len = churn_output_prefix_lines(output_lines);
    if output_lines.len() <= prefix_len + CHURN_OUTPUT_TAIL_LINES {
        return;
    }

    let tail_start = output_lines.len().saturating_sub(CHURN_OUTPUT_TAIL_LINES);
    let mut tail = output_lines.split_off(tail_start);
    output_lines.truncate(prefix_len);
    output_lines.push(CHURN_OUTPUT_OMITTED_MARKER.to_string());
    output_lines.append(&mut tail);
}

fn clamp_tests_pane_rows(rows: u16, terminal_height: u16) -> u16 {
    let max_tests_rows =
        terminal_height.saturating_sub(STATUS_PANE_ROWS + MIN_OUTPUT_PANE_ROWS);
    let min_tests_rows = MIN_TESTS_PANE_ROWS.min(max_tests_rows);

    rows.clamp(min_tests_rows, max_tests_rows)
}

fn default_tests_pane_rows(terminal_height: u16) -> u16 {
    let resizable_height = terminal_height.saturating_sub(STATUS_PANE_ROWS);
    let default_rows = resizable_height.saturating_mul(DEFAULT_TESTS_PANE_PERCENT) / 100;

    clamp_tests_pane_rows(default_rows, terminal_height)
}

fn split_output_constraints(
    tests_pane_rows: &mut Option<u16>,
    terminal_height: u16,
) -> Vec<Constraint> {
    let rows = tests_pane_rows
        .unwrap_or_else(|| default_tests_pane_rows(terminal_height));
    let rows = clamp_tests_pane_rows(rows, terminal_height);
    *tests_pane_rows = Some(rows);

    vec![
        Constraint::Length(rows),
        Constraint::Min(MIN_OUTPUT_PANE_ROWS),
        Constraint::Length(STATUS_PANE_ROWS),
    ]
}

/// Interactive TUI: test tree, run output, settings, and failure summary.
pub(super) fn run_interactive_loop(
    tree: &mut Vec<TreeNode>,
    mut run_config: RunConfig,
) -> Result<()> {
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
    let mut rediscovery_rx: Option<mpsc::Receiver<RediscoveryResult>> = None;
    let mut rediscovery_sel: Option<TreeState> = None;
    let mut is_running = false;
    let mut is_rediscovering = false;
    let mut output_scroll: u16 = 0;
    let mut output_follow_tail = true;
    let mut tests_pane_rows: Option<u16> = run_config.tests_pane_rows;
    let mut run_pid: Option<u32> = None;
    let mut run_start: Option<Instant> = None;
    let mut rediscovery_start: Option<Instant> = None;
    let mut run_passed = 0;
    let mut run_failed = 0;
    let mut run_skipped = 0;
    let mut is_churning = false;
    let mut churn_iteration: usize = 0;
    let mut churn_limit: Option<usize> = None;
    let mut churn_filter: Option<String> = None;
    let mut churn_target_path: Option<String> = None;
    let mut churn_using_sidecar = false;
    let mut churn_successes_before_failure: usize = 0;
    let mut churn_durations: Vec<Duration> = Vec::new();
    let mut failed_tests: Vec<FailedTestInfo> = Vec::new();
    let mut show_failure_summary = false;
    let mut show_failure_summary_help = false;
    let mut failed_selection: usize = 0;
    let mut failed_detail_scroll: u16 = 0;
    let mut failed_summary_list_pane_cols: Option<u16> =
        run_config.failed_summary_list_pane_cols;
    // Detail line index for stack links while the pointer is over that line in Error Details.
    let mut failure_detail_hover: Option<usize> = None;

    let mut show_config = false;
    // 0: skip build, 1: skip restore, 2: verbosity, 3: output,
    // 4: view mode, 5: manual watch, 6: debounce, 7: confirm exit on Esc
    let mut config_cursor: usize = 0;
    let mut show_exit_confirm = false;
    let mut show_help = false;
    let mut show_output_fullscreen = false;
    let mut show_save_preset = false;
    let mut preset_name_input = String::new();
    let mut preset_tag_input = String::new();
    let mut preset_input_cursor: usize = 0;
    let mut show_presets = false;
    let mut preset_list_cursor: usize = 0;

    let mut run_results = RunResultsState::load();
    run_results.recompute_rollups(tree);
    // When true, Results mode hides the leaf output panel until selection changes.
    let mut results_panel_hidden = false;
    let mut last_results_focus_fqn: Option<String> = None;

    let root_dir = std::env::current_dir()?;
    let mut manual_watch_handle: Option<ManualWatchHandle> = None;
    apply_manual_watch_config(&root_dir, &run_config, &mut manual_watch_handle);

    loop {
        if output_rx.is_some() {
            loop {
                let recv_result = match output_rx.as_ref() {
                    Some(rx) => rx.try_recv(),
                    None => break,
                };

                match recv_result {
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
                            let num_str: String =
                                rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                            if let Ok(n) = num_str.parse::<usize>() {
                                run_passed = n;
                            }
                        }
                        if let Some(pos) = line_lower.find("failed:") {
                            let rest = line_lower[pos + 7..].trim_start();
                            let num_str: String =
                                rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                            if let Ok(n) = num_str.parse::<usize>() {
                                run_failed = n;
                            }
                        }
                        if let Some(pos) = line_lower.find("skipped:") {
                            let rest = line_lower[pos + 8..].trim_start();
                            let num_str: String =
                                rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                            if let Ok(n) = num_str.parse::<usize>() {
                                run_skipped = n;
                            }
                        }

                        if is_churning && churn_using_sidecar {
                            if let Some(iteration) = parse_churn_iteration_start_line(trimmed) {
                                churn_iteration = iteration;
                                run_passed = 0;
                                run_failed = 0;
                                run_skipped = 0;
                                run_start = Some(Instant::now());
                            } else if let Some(iteration) =
                                parse_churn_iteration_passed_line(trimmed)
                            {
                                churn_successes_before_failure = iteration;
                            } else if let Some(iteration) =
                                parse_churn_iteration_failed_line(trimmed)
                            {
                                churn_iteration = iteration;
                            }
                        }

                        // Attribute Passed/Failed/Skipped to tree leaves (results mode).
                        // Skip churn iteration banner lines; still attribute per-test lines.
                        if parse_churn_iteration_start_line(trimmed).is_none()
                            && parse_churn_iteration_passed_line(trimmed).is_none()
                            && parse_churn_iteration_failed_line(trimmed).is_none()
                        {
                            run_results.ingest_line(&line, tree);
                        }

                        output_lines.push(line);
                        if is_churning {
                            trim_churn_output_lines(&mut output_lines);
                        } else {
                            // Regular runs: keep failed_tests fresh for Ctrl+E mid-run.
                            failed_tests = extract_failed_tests(&output_lines);
                        }
                    }
                    Ok(OutputEvent::Finished(code)) => {
                        if is_churning {
                            if churn_using_sidecar {
                                is_running = false;
                                is_churning = false;
                                churn_using_sidecar = false;
                                run_pid = None;
                                output_rx = None;

                                failed_tests = extract_failed_tests(&output_lines);
                                if code != Some(0) && !failed_tests.is_empty() {
                                    show_failure_summary = true;
                                    failed_selection = 0;
                                    failed_detail_scroll = 0;
                                    failure_detail_hover = None;
                                }

                                churn_filter = None;
                                churn_limit = None;
                                churn_target_path = None;
                                continue;
                            }

                            let elapsed_duration = run_start.map(|s| s.elapsed());
                            let elapsed = elapsed_duration
                                .map(format_elapsed)
                                .unwrap_or_default();

                            if let Some(d) = elapsed_duration {
                                churn_durations.push(d);
                            }

                            if code == Some(0) {
                                churn_successes_before_failure += 1;
                                output_lines.push(format!(
                                    "Iteration {}   ✓ Passed ({})",
                                    churn_iteration, elapsed
                                ));

                                let reached_limit = churn_limit
                                    .map(|limit| churn_successes_before_failure >= limit)
                                    .unwrap_or(false);

                                if reached_limit {
                                    is_running = false;
                                    is_churning = false;
                                    run_pid = None;
                                    output_rx = None;

                                    output_lines.push(String::new());
                                    output_lines.push(
                                        "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
                                            .to_string(),
                                    );
                                    output_lines.push(format!(
                                        "  Churn completed: reached iteration limit {} with no failures.",
                                        churn_limit.unwrap_or_default()
                                    ));
                                    output_lines.push(format!(
                                        "  Successful iterations: {}",
                                        churn_successes_before_failure
                                    ));
                                    if let Some(stats) = churn_duration_stats_line(&churn_durations)
                                    {
                                        output_lines.push(stats);
                                    }
                                    output_lines.push(
                                        "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
                                            .to_string(),
                                    );

                                    churn_filter = None;
                                    churn_limit = None;
                                    churn_target_path = None;
                                    churn_using_sidecar = false;
                                } else if let Some(filter) = churn_filter.clone() {
                                    churn_iteration += 1;
                                    run_passed = 0;
                                    run_failed = 0;
                                    run_skipped = 0;
                                    output_lines
                                        .push(format!("Iteration {}   ↻ Starting", churn_iteration));
                                    trim_churn_output_lines(&mut output_lines);

                                    let mut churn_run_config = run_config.clone();
                                    churn_run_config.no_build = true;
                                    churn_run_config.no_restore = true;
                                    // Churn favors throughput; keep log volume low per iteration.
                                    churn_run_config.verbosity = Verbosity::Minimal;

                                    match spawn_test_run_for_target(
                                        Some(filter),
                                        churn_target_path.as_deref(),
                                        &churn_run_config,
                                    ) {
                                        Ok((rx, pid)) => {
                                            output_rx = Some(rx);
                                            run_pid = Some(pid);
                                            run_start = Some(Instant::now());
                                            is_running = true;
                                        }
                                        Err(e) => {
                                            is_running = false;
                                            is_churning = false;
                                            run_pid = None;
                                            output_rx = None;
                                            output_lines.push(format!(
                                                "✗ Could not start churn iteration {}: {}",
                                                churn_iteration, e
                                            ));
                                            churn_filter = None;
                                            churn_limit = None;
                                            churn_target_path = None;
                                            churn_using_sidecar = false;
                                        }
                                    }
                                } else {
                                    is_running = false;
                                    is_churning = false;
                                    run_pid = None;
                                    output_rx = None;
                                    output_lines.push(
                                        "✗ Churn stopped: missing test filter for next iteration."
                                            .to_string(),
                                    );
                                    churn_target_path = None;
                                    churn_limit = None;
                                    churn_using_sidecar = false;
                                }
                            } else {
                                is_running = false;
                                is_churning = false;
                                run_pid = None;

                                output_lines.push(format!(
                                    "Iteration {}   ✗ Failed ({})",
                                    churn_iteration, elapsed
                                ));
                                output_lines.push(String::new());
                                output_lines.push(
                                    "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
                                        .to_string(),
                                );
                                output_lines.push(format!(
                                    "  Churn stopped on failure at iteration {}.",
                                    churn_iteration
                                ));
                                output_lines.push(format!(
                                    "  Successful iterations before failure: {}",
                                    churn_successes_before_failure
                                ));
                                if let Some(stats) = churn_duration_stats_line(&churn_durations) {
                                    output_lines.push(stats);
                                }

                                let msg = match code {
                                    Some(c) => format!("  Last run exit code: {}", c),
                                    None => "  Last run terminated without an exit code".to_string(),
                                };
                                output_lines.push(msg);
                                output_lines.push(
                                    "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
                                        .to_string(),
                                );

                                failed_tests = extract_failed_tests(&output_lines);
                                if !failed_tests.is_empty() {
                                    show_failure_summary = true;
                                    failed_selection = 0;
                                    failed_detail_scroll = 0;
                                    failure_detail_hover = None;
                                }

                                churn_filter = None;
                                churn_limit = None;
                                churn_target_path = None;
                                churn_using_sidecar = false;
                            }
                        } else {
                            is_running = false;
                            let elapsed = run_start
                                .map(|s| format_elapsed(s.elapsed()))
                                .unwrap_or_default();

                            output_lines.push(String::new());
                            output_lines.push(
                                "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
                                    .to_string(),
                            );

                            let total = run_passed + run_failed + run_skipped;
                            let mut summary = format!("  Test Run Summary ({} total)", total);
                            if run_passed > 0 {
                                summary.push_str(&format!("  |  ✓ {} Passed", run_passed));
                            }
                            if run_failed > 0 {
                                summary.push_str(&format!("  |  ✗ {} Failed", run_failed));
                            }
                            if run_skipped > 0 {
                                summary.push_str(&format!("  |  ⚠ {} Skipped", run_skipped));
                            }
                            output_lines.push(summary);

                            let msg = match code {
                                Some(0) => format!("  Finished successfully in {}", elapsed),
                                Some(c) => {
                                    format!("  Finished with exit code {} in {}", c, elapsed)
                                }
                                None => format!("  Process terminated after {}", elapsed),
                            };
                            output_lines.push(msg);
                            output_lines.push(
                                "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
                                    .to_string(),
                            );
                            failed_tests = extract_failed_tests(&output_lines);
                            if run_failed > 0 && !failed_tests.is_empty() {
                                show_failure_summary = true;
                                failed_selection = 0;
                                failed_detail_scroll = 0;
                                failure_detail_hover = None;
                            }
                            run_results.finalize_pending(tree);
                            run_results.save();
                            results_panel_hidden = false;
                            run_pid = None;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if is_running {
                            is_running = false;
                            let elapsed = run_start
                                .map(|s| format_elapsed(s.elapsed()))
                                .unwrap_or_default();
                            output_lines.push(format!("✓ Process finished ({})", elapsed));
                            run_pid = None;
                        }
                        break;
                    }
                }
            }
        }

        if let Some(ref rx) = rediscovery_rx {
            match rx.try_recv() {
                Ok(Ok(tests)) => {
                    let mut new_tree = build_flat_tree(&tests);
                    if let Some(sel) = rediscovery_sel.take() {
                        sel.restore(&mut new_tree);
                    }
                    *tree = new_tree;
                    state.select(Some(0));
                    search_query.clear();
                    run_results.prune_to_tree(tree);
                    let total: usize = tests.iter().map(|test| test.test_count).sum();
                    output_lines.push(format!(
                        "✓ Found {} tests ({} methods).",
                        total,
                        tests.len()
                    ));
                    is_rediscovering = false;
                    rediscovery_start = None;
                    rediscovery_rx = None;
                }
                Ok(Err(error)) => {
                    output_lines.push(format!("✗ Failed to discover tests: {error}"));
                    is_rediscovering = false;
                    rediscovery_start = None;
                    rediscovery_rx = None;
                    rediscovery_sel = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    output_lines.push("✗ Failed to discover tests: worker stopped.".to_string());
                    is_rediscovering = false;
                    rediscovery_start = None;
                    rediscovery_rx = None;
                    rediscovery_sel = None;
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
                && !show_save_preset
                && !show_presets
            {
                if show_failure_summary {
                    show_failure_summary = false;
                    show_failure_summary_help = false;
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
                        let heading = format!(
                            "━━━ Manual watch: re-running {sel_count} checked test(s)… ━━━"
                        );
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
                            &mut run_results,
                            &mut results_panel_hidden,
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
                let mut matched = tree[i].label.to_lowercase().contains(&query) || match_abbrev(&search_query, &tree[i].label);
                if !matched && tree[i].fqn.is_some() {
                    let fqn = tree[i].fqn.as_ref().unwrap();
                    matched = fqn.to_lowercase().contains(&query) || match_abbrev(&search_query, fqn);
                }
                if matched {
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
            if !matches_query[i] {
                continue;
            }
            let mut hidden = false;
            if query.is_empty() {
                let mut curr = tree[i].parent_idx;
                while let Some(p) = curr {
                    if !tree[p].is_expanded {
                        hidden = true;
                        break;
                    }
                    curr = tree[p].parent_idx;
                }
            }
            if !hidden {
                visible_indices.push(i);
            }
        }

        if let Some(sel) = state.selected() {
            if sel >= visible_indices.len() {
                state.select(if visible_indices.is_empty() {
                    None
                } else {
                    Some(visible_indices.len() - 1)
                });
            }
        } else if !visible_indices.is_empty() {
            state.select(Some(0));
        }

        let selected_count: usize = tree
            .iter()
            .filter(|n| n.is_leaf && n.is_selected)
            .map(|n| n.test_count)
            .sum();
        let total_count: usize = tree
            .iter()
            .filter(|n| n.is_leaf)
            .map(|n| n.test_count)
            .sum();

        let focused_leaf_fqn: Option<String> = state
            .selected()
            .and_then(|di| visible_indices.get(di).copied())
            .and_then(|ri| {
                let node = &tree[ri];
                if node.is_leaf {
                    node.fqn.clone()
                } else {
                    None
                }
            });
        if focused_leaf_fqn != last_results_focus_fqn {
            last_results_focus_fqn = focused_leaf_fqn.clone();
            results_panel_hidden = false;
        }

        let results_leaf_lines: Option<Vec<String>> =
            if run_config.view_mode == ViewMode::Results {
                focused_leaf_fqn
                    .as_deref()
                    .filter(|fqn| run_results.get(fqn).is_some())
                    .map(|fqn| run_results.leaf_panel_lines(fqn))
            } else {
                None
            };

        let has_output = !output_lines.is_empty();
        let show_output_panel = match run_config.view_mode {
            ViewMode::LiveOutput => {
                has_output
                    && (run_config.output_mode == OutputMode::Split || show_output_fullscreen)
            }
            ViewMode::Results => {
                // Per-leaf details for the focused test (live during the run once that
                // leaf has an outcome, and after the run finishes).
                !results_panel_hidden
                    && results_leaf_lines
                        .as_ref()
                        .map(|l| !l.is_empty())
                        .unwrap_or(false)
            }
        };
        let panel_lines: &[String] = match run_config.view_mode {
            ViewMode::Results => results_leaf_lines.as_deref().unwrap_or(&[]),
            ViewMode::LiveOutput => &output_lines,
        };
        let area = terminal.size()?;
        let split_constraints = if show_output_panel && !show_output_fullscreen {
            Some(split_output_constraints(&mut tests_pane_rows, area.height))
        } else {
            None
        };
        let output_scroll_max = if show_output_panel {
            let constraints = if show_output_fullscreen {
                vec![Constraint::Min(0), Constraint::Length(STATUS_PANE_ROWS)]
            } else {
                split_constraints.clone().unwrap_or_else(|| {
                    split_output_constraints(&mut tests_pane_rows, area.height)
                })
            };
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);
            let output_chunk_idx = if show_output_fullscreen { 0 } else { 1 };
            let inner_w = chunks[output_chunk_idx].width.saturating_sub(2);
            let inner_h = chunks[output_chunk_idx].height.saturating_sub(2);
            output_wrapped_scroll_max(panel_lines, inner_w, inner_h)
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
                vec![Constraint::Min(0), Constraint::Length(STATUS_PANE_ROWS)]
            } else if show_output_panel {
                split_constraints.clone().unwrap_or_else(|| {
                    vec![
                        Constraint::Length(default_tests_pane_rows(area.height)),
                        Constraint::Min(MIN_OUTPUT_PANE_ROWS),
                        Constraint::Length(STATUS_PANE_ROWS),
                    ]
                })
            } else {
                vec![
                    Constraint::Min(0),
                    Constraint::Length(0),
                    Constraint::Length(STATUS_PANE_ROWS),
                ]
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
                let check = if node.is_selected { "[x] " } else if node.is_partial { "[~] " } else { "[ ] " };

                let count_suffix = if run_config.view_mode == ViewMode::Results {
                    if node.is_leaf {
                        match node.fqn.as_deref().and_then(|f| run_results.get(f)) {
                            Some(r) => match r.status {
                                LeafStatus::Passed => "  ✓".to_string(),
                                LeafStatus::Failed => "  ✗".to_string(),
                                LeafStatus::Skipped => "  ⚠".to_string(),
                            },
                            None => String::new(),
                        }
                    } else if let Some(counts) = run_results.parent_counts.get(&real_idx) {
                        counts.format_suffix()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                let display_str =
                    format!("{}{}{}{}{}", indent, prefix, check, node.label, count_suffix);

                let style = if Some(display_idx) == state.selected() {
                    Style::default()
                        .bg(Color::DarkGray)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else if run_config.view_mode == ViewMode::Results && !run_results.is_empty() {
                    if node.is_leaf {
                        match node.fqn.as_deref().and_then(|f| run_results.get(f)) {
                            Some(r) => match r.status {
                                LeafStatus::Passed => Style::default().fg(Color::Green),
                                LeafStatus::Failed => Style::default().fg(Color::Red),
                                LeafStatus::Skipped => Style::default().fg(Color::Yellow),
                            },
                            None => {
                                if node.is_selected {
                                    Style::default().fg(Color::DarkGray)
                                } else {
                                    Style::default().fg(Color::DarkGray)
                                }
                            }
                        }
                    } else if let Some(counts) = run_results.parent_counts.get(&real_idx) {
                        if counts.failed > 0 {
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                        } else if counts.passed > 0 {
                            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                        } else if counts.skipped > 0 {
                            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                        } else if node.depth == 0 {
                            Style::default()
                                .fg(Color::LightMagenta)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                        }
                    } else if node.depth == 0 {
                        Style::default()
                            .fg(Color::LightMagenta)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    }
                } else if !node.is_leaf && node.depth == 0 {
                    Style::default().fg(Color::LightMagenta).add_modifier(Modifier::BOLD)
                } else if !node.is_leaf {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else if node.is_selected {
                    // Soft rose — readable, and distinct from success green / folder magenta.
                    Style::default().fg(Color::Rgb(210, 155, 180))
                } else {
                    Style::default().fg(Color::DarkGray)
                };

                items.push(ListItem::new(Line::from(Span::styled(display_str, style))));
            }

            if !show_output_fullscreen {
                let title = format!(" Tests ({}/{}) ", selected_count, total_count);
                let mut block = Block::default().title(title).borders(Borders::ALL);
                if show_output_panel {
                    block = block.title(
                        Title::from(" Ctrl+I/O ")
                            .alignment(Alignment::Right)
                            .position(Position::Bottom),
                    );
                }
                let list = List::new(items).block(block);
                f.render_stateful_widget(list, chunks[0], &mut state);
            }

            if show_output_panel {
                let output_text = styled_output_lines(panel_lines);

                let output_title = if run_config.view_mode == ViewMode::Results {
                    let name = focused_leaf_fqn.as_deref().unwrap_or("test");
                    let status = focused_leaf_fqn
                        .as_deref()
                        .and_then(|f| run_results.get(f))
                        .map(|r| match r.status {
                            LeafStatus::Passed => "Passed",
                            LeafStatus::Failed => "Failed",
                            LeafStatus::Skipped => "Skipped",
                        })
                        .unwrap_or("Result");
                    format!(" Result ({status}) — {name} ")
                } else if is_rediscovering {
                    let elapsed = rediscovery_start
                        .map(|s| format_elapsed(s.elapsed()))
                        .unwrap_or_default();
                    format!(" Output (Rediscovering... {}) [follow] ", elapsed)
                } else if is_running && is_churning {
                    let elapsed = run_start.map(|s| format_elapsed(s.elapsed())).unwrap_or_default();
                    let limit_suffix = churn_limit
                        .map(|limit| format!("/{}", limit))
                        .unwrap_or_default();
                    if output_follow_tail {
                        format!(
                            " Output (Churning {}{}... {}) [follow]  |  ✓:{}  ✗:{}  ⚠:{} ",
                            churn_iteration, limit_suffix, elapsed, run_passed, run_failed, run_skipped
                        )
                    } else {
                        format!(
                            " Output (Churning {}{}... {}) [scroll]  |  ✓:{}  ✗:{}  ⚠:{} ",
                            churn_iteration, limit_suffix, elapsed, run_passed, run_failed, run_skipped
                        )
                    }
                } else if is_running {
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
                        .border_style(if is_running || is_rediscovering {
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
            let view_hint = match run_config.view_mode {
                ViewMode::Results => "  View: Results",
                ViewMode::LiveOutput => "  View: Live",
            };
            let help_text = if show_output_fullscreen && is_running {
                let elapsed = run_start.map(|s| format_elapsed(s.elapsed())).unwrap_or_default();
                format!(
                    " Fullscreen output... {}  |  PgUp/PgDn/Home/End/mouse: scroll  Ctrl+E: failed summary  Esc: cancel run{}{}",
                    elapsed, watch_hint, view_hint
                )
            } else if show_output_fullscreen {
                format!(
                    " Fullscreen output  |  PgUp/PgDn/Home/End/mouse: scroll  Esc: back to tree{}{}",
                    watch_hint, view_hint
                )
            } else if !search_query.is_empty() {
                format!(
                    " Search: {}  |  Esc: clear  Enter: run  ?: help{}{}",
                    search_query, watch_hint, view_hint
                )
            } else if is_running && is_churning {
                let elapsed = run_start.map(|s| format_elapsed(s.elapsed())).unwrap_or_default();
                let limit_suffix = churn_limit
                    .map(|limit| format!("/{}", limit))
                    .unwrap_or_else(|| "/∞".to_string());
                let tree_nav = if run_config.view_mode == ViewMode::Results {
                    "  ←/→: expand"
                } else {
                    ""
                };
                format!(
                    " Churning iteration {}{}... {}  |  ✓:{} ✗:{} ⚠:{}{}  Ctrl+E: failed summary  Esc: stop{}{}",
                    churn_iteration, limit_suffix, elapsed, run_passed, run_failed, run_skipped, tree_nav, watch_hint, view_hint
                )
            } else if is_running {
                let elapsed = run_start.map(|s| format_elapsed(s.elapsed())).unwrap_or_default();
                let tree_nav = if run_config.view_mode == ViewMode::Results {
                    "  ←/→: expand"
                } else {
                    ""
                };
                format!(
                    " Running... {}  |  ✓:{} ✗:{} ⚠:{}{}  Ctrl+E: failed summary  Esc: cancel{}{}",
                    elapsed, run_passed, run_failed, run_skipped, tree_nav, watch_hint, view_hint
                )
            } else if is_rediscovering {
                let elapsed = rediscovery_start
                    .map(|s| format_elapsed(s.elapsed()))
                    .unwrap_or_default();
                format!(
                    " Rediscovering tests... {}  |  UI remains responsive{}{}",
                    elapsed, watch_hint, view_hint
                )
            } else {
                let mut text = " Arrows: nav  Space: toggle  Enter: run  Ctrl+U: churn  Ctrl+V: view  Ctrl+X: clear results ".to_string();
                if run_config.manual_watch_enabled {
                    text.push_str(watch_hint);
                }
                text.push_str(view_hint);
                text.push_str("  Ctrl+S: preset  Ctrl+L: presets  Ctrl+E: failed ");
                text.push_str(" ?: help  Esc: quit ");
                text
            };

            let help = Paragraph::new(help_text)
                .style(Style::default().fg(
                    if is_running || is_rediscovering { Color::Yellow }
                    else if !search_query.is_empty() { Color::Yellow }
                    else { Color::DarkGray }
                ))
                .block(Block::default().borders(Borders::ALL));
            let help_chunk_idx = if show_output_fullscreen { 1 } else { 2 };
            f.render_widget(help, chunks[help_chunk_idx]);

            if show_config {
                let popup = centered_rect(78, 28, area);
                f.render_widget(Clear, popup);

                let v_label = match run_config.verbosity {
                    Verbosity::Normal => "Normal",
                    Verbosity::Detailed => "Detailed",
                    Verbosity::Minimal => "Minimal",
                };
                let out_label = match run_config.output_mode {
                    OutputMode::Split => "Split (tree + output)",
                    OutputMode::Fullscreen => "Fullscreen when running (Live)",
                };
                let view_label = match run_config.view_mode {
                    ViewMode::Results => "Results (tree colors + leaf panel)",
                    ViewMode::LiveOutput => "Live (streaming output panel)",
                };
                let mw = if run_config.manual_watch_enabled { "on " } else { "off" };
                let d = run_config.manual_watch_delay_ms;
                let mut config_lines: Vec<Line> = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        " Build & discovery ",
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )),
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
                    format!("  [∙]  Output layout:  {out_label}  (Space: toggle)"),
                    format!("  [∙]  View mode:  {view_label}  (Space: toggle)"),
                    format!(
                        "  {}  Manual watch:  {mw} — re-runs only checked tests on `.cs` save",
                        if run_config.manual_watch_enabled { "[x]" } else { "[ ]" }
                    ),
                    format!("  [∙]  Watch debounce:  {d} ms   ←/→: ±200 ms"),
                    format!(
                        "  {}  Confirm exit on Esc",
                        if run_config.confirm_exit_on_esc { "[x]" } else { "[ ]" }
                    ),
                ];
                for (i, line) in line_strings.iter().enumerate() {
                    let style = if i == config_cursor {
                        Style::default()
                            .bg(Color::DarkGray)
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    config_lines.push(Line::from(Span::styled(line.as_str(), style)));
                }
                config_lines.push(Line::from(""));
                config_lines.push(Line::from(Span::styled(
                    "  Results = tree pass/fail colors + focused-leaf panel (default).",
                    Style::default().fg(Color::DarkGray),
                )));
                config_lines.push(Line::from(Span::styled(
                    "  Live = classic streaming log. Output layout mainly applies to Live.",
                    Style::default().fg(Color::DarkGray),
                )));
                config_lines.push(Line::from(Span::styled(
                    "  ↑/↓ move  ·  Space change  ·  ←/→ debounce  ·  Esc / Enter save & close",
                    Style::default().fg(Color::DarkGray),
                )));
                config_lines.push(Line::from(""));

                let config_widget = Paragraph::new(config_lines)
                    .block(
                        Block::default()
                            .title(" Settings (Ctrl+P) ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Cyan)),
                    )
                    .wrap(Wrap { trim: false });
                f.render_widget(config_widget, popup);
            }

            if show_help {
                let popup = centered_rect(88, 48, area);
                f.render_widget(Clear, popup);

                let help_lines = vec![
                    Line::from(Span::styled(
                        " Navigation",
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )),
                    Line::from("  ↑/↓         : Move selection in the test tree"),
                    Line::from("  ←/→         : Collapse / expand folders and classes"),
                    Line::from("  a-z / 0-9   : Type to search/filter the tree"),
                    Line::from("  Backspace   : Delete last search character"),
                    Line::from("  Esc         : Clear search, hide leaf result panel, or quit"),
                    Line::from(""),
                    Line::from(Span::styled(
                        " Running tests",
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )),
                    Line::from("  Space       : Check / uncheck a test or folder"),
                    Line::from("  Ctrl+A      : Toggle all visible tests"),
                    Line::from("  Enter       : Run all checked tests"),
                    Line::from("  Ctrl+U      : Churn checked tests until failure (Esc to stop)"),
                    Line::from("  Ctrl+Shift+U: Quick churn (max 100 iterations)"),
                    Line::from("  Esc         : Cancel a run, or leave fullscreen Live output"),
                    Line::from(""),
                    Line::from(Span::styled(
                        " View modes (Ctrl+V to toggle)",
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )),
                    Line::from("  Results     : Default. Tree shows pass (green) / fail (red) / skip (yellow)."),
                    Line::from("                Checked leaves use a soft rose until they finish."),
                    Line::from("                Focus a leaf to open its result panel (also mid-run once done)."),
                    Line::from("                While running: ↑/↓/←/→ still navigate and expand the tree."),
                    Line::from("  Live        : Classic streaming output panel while tests run."),
                    Line::from("  Ctrl+X      : Clear last-run colors, leaf panel, and bin/dotest/last_run.json"),
                    Line::from(""),
                    Line::from(Span::styled(
                        " Output pane",
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )),
                    Line::from("  PgUp/PgDn, Home, End, mouse wheel : Scroll when the output panel is visible"),
                    Line::from("  Ctrl+I / Ctrl+O : Move the Tests/Output split up or down (split layout)"),
                    Line::from("  Title shows [follow] (tail) vs [scroll] (manual)"),
                    Line::from("  Click a leaf    : Focus it (Results shows that test's output)"),
                    Line::from(""),
                    Line::from(Span::styled(
                        " Shortcuts",
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )),
                    Line::from("  Ctrl+P      : Settings (build, verbosity, view mode, watch, …)"),
                    Line::from("  Ctrl+W      : Toggle manual watch (● WATCH ON in the status bar)"),
                    Line::from("  Ctrl+S      : Save checked tests as a preset"),
                    Line::from("  Ctrl+L      : Open presets and run one"),
                    Line::from("  Ctrl+E      : Failed-tests summary (fills as failures arrive)"),
                    Line::from("  F5          : Rediscover tests and refresh the tree/cache"),
                    Line::from("  F1 / ?      : Open this help"),
                    Line::from(""),
                    Line::from(Span::styled(
                        " Persistence",
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )),
                    Line::from("  Settings    : Saved in `.dotest.yml`"),
                    Line::from("  Discovery   : Cached in `.dotest_cache.json` (skipped when fingerprint matches)"),
                    Line::from("  Last run    : Results colors persist in `bin/dotest/last_run.json`"),
                    Line::from(""),
                    Line::from(Span::styled(
                        " Manual watch",
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )),
                    Line::from("  When ON, saving a `.cs` file re-runs only the tests you have checked."),
                    Line::from("  Debounce delay is set in Settings (Ctrl+P)."),
                    Line::from(""),
                    Line::from(Span::styled(
                        " Failed summary (Ctrl+E)",
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )),
                    Line::from("  Shift+↑/↓   : Select failed test  |  ↑/↓/PgUp/Dn: Scroll error details"),
                    Line::from("  r / R       : Re-run one / all  |  c/d: copy  |  click list: pick  |  Esc: close"),
                    Line::from("  ?           : Shortcuts for this overlay"),
                    Line::from(""),
                    Line::from(Span::styled(
                        "  Esc or Enter closes this help.",
                        Style::default().fg(Color::DarkGray),
                    )),
                ];

                let help_widget = Paragraph::new(help_lines)
                    .block(
                        Block::default()
                            .title(" Help (? / F1) ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Yellow)),
                    )
                    .wrap(Wrap { trim: false });
                f.render_widget(help_widget, popup);
            }

            if show_save_preset {
                let popup = centered_rect(76, 13, area);
                f.render_widget(Clear, popup);
                let name_style = if preset_input_cursor == 0 {
                    Style::default()
                        .bg(Color::DarkGray)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let tag_style = if preset_input_cursor == 1 {
                    Style::default()
                        .bg(Color::DarkGray)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let save_lines = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("  Name (required, unique): {}", preset_name_input),
                        name_style,
                    )),
                    Line::from(Span::styled(
                        format!("  Tag (optional): {}", preset_tag_input),
                        tag_style,
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "  ↑/↓ or Tab: field  |  Enter: save  |  Esc: cancel  |  Backspace: delete",
                        Style::default().fg(Color::DarkGray),
                    )),
                ];
                let save_widget = Paragraph::new(save_lines).block(
                    Block::default()
                        .title(" Save Preset (Ctrl+S) ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan)),
                );
                f.render_widget(save_widget, popup);
            }

            if show_presets {
                let popup = centered_rect(82, 24, area);
                f.render_widget(Clear, popup);
                let mut items: Vec<ListItem> = Vec::new();
                for (idx, preset) in run_config.presets.iter().enumerate() {
                    let tag = preset
                        .tag
                        .as_deref()
                        .map(|t| format!("  [tag: {}]", t))
                        .unwrap_or_default();
                    let line = format!("{} ({} tests){}", preset.name, preset.tests.len(), tag);
                    let style = if idx == preset_list_cursor {
                        Style::default()
                            .bg(Color::DarkGray)
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    items.push(ListItem::new(Line::from(Span::styled(line, style))));
                }
                let list = List::new(items).block(
                    Block::default()
                        .title(" Presets (Ctrl+L) ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan)),
                );
                f.render_widget(list, popup);
            }

            if show_failure_summary {
                let popup = area;
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
                    .constraints(vec![
                        Constraint::Length(compute_failure_summary_list_pane_cols(
                            failed_summary_list_pane_cols,
                            popup.width,
                        )),
                        Constraint::Min(0),
                    ])
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
                        .title(
                            Title::from(" Ctrl+I/O ")
                                .alignment(Alignment::Right)
                                .position(Position::Bottom),
                        )
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
                    " Shift+↑/↓: pick test  |  ?: shortcuts ",
                )
                .style(Style::default().fg(Color::Red));
                f.render_widget(footer, inner[1]);

                if show_failure_summary_help {
                    let help_popup = centered_rect(72, 24, area);
                    f.render_widget(Clear, help_popup);
                    let help_lines = vec![
                        Line::from(Span::styled(
                            " Navigation",
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        )),
                        Line::from("  Shift+↑/↓ : Select failed test"),
                        Line::from("  ↑/↓       : Scroll error details"),
                        Line::from("  PgUp/PgDn : Scroll details faster"),
                        Line::from("  Home/End  : Jump details to top/bottom"),
                        Line::from(""),
                        Line::from(Span::styled(
                            " Actions",
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        )),
                        Line::from("  c         : Copy failed test names"),
                        Line::from("  d or m    : Copy selected failure details"),
                        Line::from("  r         : Re-run selected failed test"),
                        Line::from("  R         : Re-run all failed tests"),
                        Line::from(""),
                        Line::from(Span::styled(
                            " Mouse",
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        )),
                        Line::from("  Click test list       : Select failed test"),
                        Line::from("  Click stack-trace link: Open file in editor"),
                        Line::from("  Wheel / drag in details: Scroll details"),
                        Line::from(""),
                        Line::from(Span::styled(
                            "  ? / Esc / Enter closes this shortcuts window.",
                            Style::default().fg(Color::DarkGray),
                        )),
                        Line::from(Span::styled(
                            "  Esc from the summary closes Failed Tests Summary.",
                            Style::default().fg(Color::DarkGray),
                        )),
                    ];

                    let help_widget = Paragraph::new(help_lines)
                        .block(
                            Block::default()
                                .title(" Failed Summary Shortcuts (?) ")
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::Yellow)),
                        )
                        .wrap(Wrap { trim: false });
                    f.render_widget(help_widget, help_popup);
                }
            }

            if show_exit_confirm {
                let popup = centered_rect(56, 11, area);
                f.render_widget(Clear, popup);
                let confirm_lines = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        "  Exit dotest?",
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from("  Enter / Y : Yes, quit"),
                    Line::from("  Esc / N   : No, stay"),
                ];
                let confirm_widget = Paragraph::new(confirm_lines).block(
                    Block::default()
                        .title(" Confirm Exit ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow)),
                );
                f.render_widget(confirm_widget, popup);
            }
        })?;

        if event::poll(std::time::Duration::from_millis(50))? {
            match event::read()? {
                Event::Mouse(mouse) => {
                    if show_failure_summary {
                        let area = terminal.size()?;
                        let failed_summary_list_cols = compute_failure_summary_list_pane_cols(
                            failed_summary_list_pane_cols,
                            area.width,
                        );
                        let list_rect = failed_summary_list_rect(area, failed_summary_list_cols);
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
                        let detail_rect =
                            failed_summary_detail_rect(area, failed_summary_list_cols);
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
                                        let rel = mouse.row.saturating_sub(list_inner_y) as usize;
                                        if rel < failed_tests.len() {
                                            failed_selection = rel;
                                            failed_detail_scroll = 0;
                                            failure_detail_hover = None;
                                        }
                                    } else if mouse_in_detail_pane {
                                        let selected = &failed_tests
                                            [failed_selection.min(failed_tests.len() - 1)];
                                        if let Some(detail_index) = clicked_detail_index(
                                            &selected.details,
                                            detail_inner_width,
                                            failed_detail_scroll,
                                            mouse.row.saturating_sub(detail_inner_y),
                                        ) {
                                            if let Some(target) = parse_stack_trace_target(
                                                &selected.details[detail_index],
                                            ) {
                                                match open_path_in_default_editor(&target.path) {
                                                    Ok(()) => {
                                                        let message = if let Some(line_number) =
                                                            target.line_number
                                                        {
                                                            format!(
                                                                "✓ Opened {} (line {}).",
                                                                target.path, line_number
                                                            )
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
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                        && !show_output_fullscreen
                        && !show_config
                        && !show_help
                        && !show_presets
                        && !show_save_preset
                        && !show_exit_confirm
                    {
                        let tree_constraints = if show_output_panel {
                            split_constraints.clone().unwrap_or_else(|| {
                                split_output_constraints(&mut tests_pane_rows, area.height)
                            })
                        } else {
                            vec![
                                Constraint::Min(0),
                                Constraint::Length(0),
                                Constraint::Length(STATUS_PANE_ROWS),
                            ]
                        };
                        let tree_chunks = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints(tree_constraints)
                            .split(area);
                        let tree_rect = tree_chunks[0];
                        let list_inner_x = tree_rect.x.saturating_add(1);
                        let list_inner_y = tree_rect.y.saturating_add(1);
                        let list_inner_width = tree_rect.width.saturating_sub(2);
                        let list_inner_height = tree_rect.height.saturating_sub(2);
                        let in_tree = list_inner_width > 0
                            && list_inner_height > 0
                            && mouse.column >= list_inner_x
                            && mouse.column < list_inner_x.saturating_add(list_inner_width)
                            && mouse.row >= list_inner_y
                            && mouse.row < list_inner_y.saturating_add(list_inner_height);
                        if in_tree {
                            let rel = mouse.row.saturating_sub(list_inner_y) as usize;
                            let offset = state.offset();
                            let display_idx = offset.saturating_add(rel);
                            if display_idx < visible_indices.len() {
                                state.select(Some(display_idx));
                                results_panel_hidden = false;
                            }
                        }
                    }
                    if show_output_panel {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                output_follow_tail = false;
                                output_scroll = output_scroll.saturating_sub(3);
                            }
                            MouseEventKind::ScrollDown => {
                                output_scroll =
                                    output_scroll.saturating_add(3).min(output_scroll_max);
                                output_follow_tail = output_scroll >= output_scroll_max;
                            }
                            _ => {}
                        }
                    }
                    continue;
                }
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }

                    if show_help {
                        match key.code {
                            KeyCode::Esc | KeyCode::Enter => {
                                show_help = false;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    if show_exit_confirm {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                                show_exit_confirm = false;
                            }
                            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                                break;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    if show_save_preset {
                        match key.code {
                            KeyCode::Esc => {
                                show_save_preset = false;
                            }
                            KeyCode::Enter => {
                                match save_preset(
                                    &mut run_config,
                                    tree,
                                    &preset_name_input,
                                    Some(preset_tag_input.clone()),
                                ) {
                                    Ok(total) => {
                                        run_config.save();
                                        output_lines.push(format!(
                                            "✓ Preset '{}' saved. Total presets: {}.",
                                            preset_name_input.trim(),
                                            total
                                        ));
                                        show_save_preset = false;
                                        preset_name_input.clear();
                                        preset_tag_input.clear();
                                        preset_input_cursor = 0;
                                    }
                                    Err(message) => output_lines.push(format!("✗ {message}")),
                                }
                            }
                            KeyCode::Up => {
                                preset_input_cursor = preset_input_cursor.saturating_sub(1);
                            }
                            KeyCode::Down | KeyCode::Tab => {
                                preset_input_cursor = (preset_input_cursor + 1).min(1);
                            }
                            KeyCode::Backspace => {
                                if preset_input_cursor == 0 {
                                    preset_name_input.pop();
                                } else {
                                    preset_tag_input.pop();
                                }
                            }
                            KeyCode::Char(c) => {
                                if c.is_alphanumeric() || c.is_ascii_punctuation() || c == ' ' {
                                    if preset_input_cursor == 0 {
                                        preset_name_input.push(c);
                                    } else {
                                        preset_tag_input.push(c);
                                    }
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    if show_presets {
                        match key.code {
                            KeyCode::Esc => {
                                show_presets = false;
                            }
                            KeyCode::Up => {
                                preset_list_cursor = preset_list_cursor.saturating_sub(1);
                            }
                            KeyCode::Down => {
                                if !run_config.presets.is_empty() {
                                    preset_list_cursor =
                                        (preset_list_cursor + 1).min(run_config.presets.len() - 1);
                                }
                            }
                            KeyCode::Enter => {
                                if let Some(preset) =
                                    run_config.presets.get(preset_list_cursor).cloned()
                                {
                                    let result = apply_preset_selection(tree, &preset);
                                    if result.applied == 0 {
                                        output_lines.push(format!(
                                            "⚠ Preset '{}' has no tests available in current discovery.",
                                            preset.name
                                        ));
                                        show_presets = false;
                                        continue;
                                    }
                                    if result.missing > 0 {
                                        output_lines.push(format!(
                                            "⚠ Preset '{}' skipped {} missing test(s) not available in current discovery.",
                                            preset.name, result.missing
                                        ));
                                    }
                                    if let Some(filter_str) = build_filter(tree) {
                                        let heading = format!(
                                            "━━━ Running preset '{}' ({} available test(s))… ━━━",
                                            preset.name, result.applied
                                        );
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
                                            &mut run_results,
                                            &mut results_panel_hidden,
                                        );
                                    }
                                    show_presets = false;
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    if show_failure_summary {
                        if show_failure_summary_help {
                            match key.code {
                                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Enter => {
                                    show_failure_summary_help = false;
                                }
                                _ => {}
                            }
                            continue;
                        }

                        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                        let resize_delta = match key.code {
                            KeyCode::Char('i' | 'I')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                Some(-(PANE_RESIZE_STEP_ROWS as i16))
                            }
                            KeyCode::Char('o' | 'O')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                Some(PANE_RESIZE_STEP_ROWS as i16)
                            }
                            _ => None,
                        };
                        if let Some(delta) = resize_delta {
                            let current = compute_failure_summary_list_pane_cols(
                                failed_summary_list_pane_cols,
                                area.width,
                            );
                            let next = if delta.is_negative() {
                                current.saturating_sub(delta.unsigned_abs())
                            } else {
                                current.saturating_add(delta as u16)
                            };
                            failed_summary_list_pane_cols = Some(
                                clamp_failed_summary_list_pane_cols(next, area.width),
                            );
                            continue;
                        }

                        match key.code {
                            KeyCode::Esc => {
                                show_failure_summary = false;
                                show_failure_summary_help = false;
                                failure_detail_hover = None;
                            }
                            KeyCode::Char('?') => {
                                show_failure_summary_help = true;
                            }
                            KeyCode::Up => {
                                if !failed_tests.is_empty() {
                                    if shift {
                                        failed_selection = failed_selection.saturating_sub(1);
                                        failed_detail_scroll = 0;
                                        failure_detail_hover = None;
                                    } else {
                                        failed_detail_scroll =
                                            failed_detail_scroll.saturating_sub(1);
                                        failure_detail_hover = None;
                                    }
                                }
                            }
                            KeyCode::Down => {
                                if !failed_tests.is_empty() {
                                    if shift {
                                        failed_selection =
                                            (failed_selection + 1).min(failed_tests.len() - 1);
                                        failed_detail_scroll = 0;
                                        failure_detail_hover = None;
                                    } else {
                                        failed_detail_scroll =
                                            failed_detail_scroll.saturating_add(1);
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
                                        Ok(_) => output_lines.push(
                                            "✓ Copied failed test names to clipboard.".to_string(),
                                        ),
                                        Err(_) => output_lines.push(
                                            "✗ Could not copy failed test names to clipboard."
                                                .to_string(),
                                        ),
                                    }
                                }
                            }
                            KeyCode::Char('d') | KeyCode::Char('m') => {
                                if !failed_tests.is_empty() {
                                    let f = &failed_tests[failed_selection
                                        .min(failed_tests.len().saturating_sub(1))];
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
                                    let f = &failed_tests[failed_selection
                                        .min(failed_tests.len().saturating_sub(1))];
                                    let fk =
                                        build_filter_for_display_names(&[filter_key_for_vstest(
                                            &f.name,
                                        )]);
                                    show_failure_summary = false;
                                    show_failure_summary_help = false;
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
                                        &mut run_results,
                                        &mut results_panel_hidden,
                                    );
                                }
                            }
                            KeyCode::Char('R') => {
                                if !is_running && !failed_tests.is_empty() {
                                    let names: Vec<String> =
                                        failed_tests.iter().map(|f| f.name.clone()).collect();
                                    let n = names.len();
                                    let fk = build_filter_for_display_names(&names);
                                    show_failure_summary = false;
                                    show_failure_summary_help = false;
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
                                        &mut run_results,
                                        &mut results_panel_hidden,
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
                                run_config.manual_watch_delay_ms =
                                    debounce_clamp(run_config.manual_watch_delay_ms);
                                run_config.save();
                                apply_manual_watch_config(
                                    &root_dir,
                                    &run_config,
                                    &mut manual_watch_handle,
                                );
                            }
                            KeyCode::Up => {
                                if config_cursor > 0 {
                                    config_cursor -= 1;
                                }
                            }
                            KeyCode::Down => {
                                if config_cursor < 7 {
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
                            KeyCode::Char(' ') => match config_cursor {
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
                                    run_config.output_mode =
                                        if run_config.output_mode == OutputMode::Split {
                                            OutputMode::Fullscreen
                                        } else {
                                            OutputMode::Split
                                        };
                                }
                                4 => {
                                    run_config.view_mode = match run_config.view_mode {
                                        ViewMode::Results => ViewMode::LiveOutput,
                                        ViewMode::LiveOutput => ViewMode::Results,
                                    };
                                    results_panel_hidden = false;
                                    if run_config.view_mode == ViewMode::Results {
                                        show_output_fullscreen = false;
                                    }
                                }
                                5 => {
                                    run_config.manual_watch_enabled =
                                        !run_config.manual_watch_enabled;
                                    run_config.manual_watch_delay_ms =
                                        debounce_clamp(run_config.manual_watch_delay_ms);
                                    apply_manual_watch_config(
                                        &root_dir,
                                        &run_config,
                                        &mut manual_watch_handle,
                                    );
                                }
                                6 => {}
                                7 => {
                                    run_config.confirm_exit_on_esc =
                                        !run_config.confirm_exit_on_esc;
                                }
                                _ => {}
                            },
                            _ => {}
                        }
                        continue;
                    }

                    if show_output_panel && !show_output_fullscreen {
                        let resize_delta = match key.code {
                            KeyCode::Char('i' | 'I')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                Some(-(PANE_RESIZE_STEP_ROWS as i16))
                            }
                            KeyCode::Char('o' | 'O')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                Some(PANE_RESIZE_STEP_ROWS as i16)
                            }
                            _ => None,
                        };

                        if let Some(delta) = resize_delta {
                            let current = tests_pane_rows
                                .unwrap_or_else(|| default_tests_pane_rows(area.height));
                            let next = if delta.is_negative() {
                                current.saturating_sub(delta.unsigned_abs())
                            } else {
                                current.saturating_add(delta as u16)
                            };
                            tests_pane_rows = Some(clamp_tests_pane_rows(next, area.height));
                            continue;
                        }
                    }

                    if is_running {
                        // Results mode keeps the tree as the main surface while tests run.
                        // Allow expand/collapse and selection movement so users can follow
                        // live pass/fail colors without waiting for the run to finish.
                        let allow_tree_nav = run_config.view_mode == ViewMode::Results
                            && !show_output_fullscreen;
                        match key.code {
                            KeyCode::Char('x' | 'X') | KeyCode::Char('\u{18}')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                run_results.clear();
                                RunResultsState::delete_file();
                                results_panel_hidden = true;
                                failed_tests.clear();
                                show_failure_summary = false;
                            }
                            KeyCode::Up if allow_tree_nav => {
                                if !visible_indices.is_empty() {
                                    let i = match state.selected() {
                                        Some(0) | None => visible_indices.len() - 1,
                                        Some(i) => i - 1,
                                    };
                                    state.select(Some(i));
                                }
                            }
                            KeyCode::Down if allow_tree_nav => {
                                if !visible_indices.is_empty() {
                                    let i = match state.selected() {
                                        Some(i) if i >= visible_indices.len() - 1 => 0,
                                        Some(i) => i + 1,
                                        None => 0,
                                    };
                                    state.select(Some(i));
                                }
                            }
                            KeyCode::Left if allow_tree_nav => {
                                if let Some(di) = state.selected() {
                                    if di < visible_indices.len() {
                                        let ri = visible_indices[di];
                                        if !tree[ri].is_leaf && tree[ri].is_expanded {
                                            tree[ri].is_expanded = false;
                                        } else if let Some(pi) = tree[ri].parent_idx {
                                            if let Some(pdi) =
                                                visible_indices.iter().position(|&r| r == pi)
                                            {
                                                state.select(Some(pdi));
                                            }
                                        }
                                    }
                                }
                            }
                            KeyCode::Right if allow_tree_nav => {
                                if let Some(di) = state.selected() {
                                    if di < visible_indices.len() {
                                        let ri = visible_indices[di];
                                        if !tree[ri].is_leaf && !tree[ri].is_expanded {
                                            tree[ri].is_expanded = true;
                                        }
                                    }
                                }
                            }
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
                                    output_scroll =
                                        output_scroll.saturating_add(5).min(output_scroll_max);
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
                                let elapsed = run_start
                                    .map(|s| format_elapsed(s.elapsed()))
                                    .unwrap_or_default();
                                output_lines.push(String::new());
                                if is_churning {
                                    output_lines.push(format!(
                                        "⚠ Churn stopped by user at iteration {} ({})",
                                        churn_iteration, elapsed
                                    ));
                                    output_lines.push(format!(
                                        "  Successful iterations before stop: {}",
                                        churn_successes_before_failure
                                    ));
                                    if let Some(stats) = churn_duration_stats_line(&churn_durations)
                                    {
                                        output_lines.push(stats);
                                    }
                                } else {
                                    output_lines.push(format!("⚠ Cancelled ({})", elapsed));
                                }
                                is_churning = false;
                                churn_filter = None;
                                churn_limit = None;
                                churn_target_path = None;
                                churn_using_sidecar = false;
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
                                    output_scroll =
                                        output_scroll.saturating_add(5).min(output_scroll_max);
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

                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('a')
                    {
                        let any_leaf_selected = tree.iter().any(|n| n.is_leaf && n.is_selected);
                        let to_state = !any_leaf_selected;
                        for node in tree.iter_mut() {
                            node.is_selected = to_state;
                            node.is_partial = false;
                        }
                        continue;
                    }

                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('v' | 'V'))
                    {
                        run_config.view_mode = match run_config.view_mode {
                            ViewMode::Results => ViewMode::LiveOutput,
                            ViewMode::LiveOutput => ViewMode::Results,
                        };
                        results_panel_hidden = false;
                        if run_config.view_mode == ViewMode::Results {
                            show_output_fullscreen = false;
                        }
                        run_config.save();
                        continue;
                    }

                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(
                            key.code,
                            // Ctrl+X (and Ctrl+Shift+X when the terminal delivers it).
                            // Avoid requiring Shift: VS Code/Cursor steals Ctrl+Shift+X for Extensions.
                            KeyCode::Char('x' | 'X') | KeyCode::Char('\u{18}')
                        )
                    {
                        run_results.clear();
                        RunResultsState::delete_file();
                        results_panel_hidden = true;
                        failed_tests.clear();
                        show_failure_summary = false;
                        continue;
                    }

                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('u' | 'U'))
                    {
                        if let Some(run_request) = build_selected_run_request(tree) {
                            let filter_str = run_request.filter;
                            let sidecar_filter = run_request.sidecar_filter;
                            let target_path = run_request.target_path;
                            let test_names = run_request.test_names;
                            let quick_limit = key.modifiers.contains(KeyModifiers::SHIFT)
                                || matches!(key.code, KeyCode::Char('U'));
                            let limit = if quick_limit {
                                Some(QUICK_CHURN_LIMIT)
                            } else {
                                None
                            };

                            let sel_count: usize = tree
                                .iter()
                                .filter(|n| n.is_leaf && n.is_selected)
                                .map(|n| n.test_count)
                                .sum();

                            output_lines.clear();
                            output_scroll = 0;
                            output_follow_tail = true;
                            failed_tests.clear();
                            show_failure_summary = false;
                            failed_selection = 0;
                            failed_detail_scroll = 0;
                            failure_detail_hover = None;

                            is_churning = true;
                            churn_iteration = 1;
                            churn_limit = limit;
                            churn_filter = None;
                            churn_target_path = target_path.clone();
                            churn_using_sidecar = target_path.is_some();
                            churn_successes_before_failure = 0;
                            churn_durations.clear();
                            run_results.clear();
                            results_panel_hidden = false;

                            let heading = format!(
                                "━━━ Churning {sel_count} selected test(s) until failure… ━━━"
                            );
                            output_lines.push(heading);
                            if let Some(limit) = churn_limit {
                                output_lines.push(format!(
                                    "  Iteration limit: {} (quick churn mode)",
                                    limit
                                ));
                            }
                            output_lines.push("Iteration 1   ↻ Starting".to_string());
                            output_lines.push(String::new());

                            let spawn_result = if let Some(target_path) = target_path.clone() {
                                let request = ChurnSidecarRequest {
                                    repo_root: root_dir.display().to_string(),
                                    target_path,
                                    filter: compose_test_filter(Some(sidecar_filter)),
                                    test_names,
                                    iteration_limit: limit,
                                    no_build: run_config.no_build,
                                    no_restore: run_config.no_restore,
                                };
                                spawn_churn_sidecar(&request)
                            } else {
                                churn_filter = Some(filter_str.clone());
                                churn_using_sidecar = false;

                                let mut first_churn_run_config = run_config.clone();
                                // Respect the user's run settings; forcing a build here adds
                                // unnecessary startup time before churn begins.
                                first_churn_run_config.no_build = run_config.no_build;
                                first_churn_run_config.no_restore = run_config.no_restore;
                                // Churn favors throughput; keep log volume low per iteration.
                                first_churn_run_config.verbosity = Verbosity::Minimal;

                                spawn_test_run_for_target(
                                    Some(filter_str),
                                    churn_target_path.as_deref(),
                                    &first_churn_run_config,
                                )
                            };

                            match spawn_result {
                                Ok((rx, pid)) => {
                                    output_rx = Some(rx);
                                    run_pid = Some(pid);
                                    run_start = Some(Instant::now());
                                    run_passed = 0;
                                    run_failed = 0;
                                    run_skipped = 0;
                                    is_running = true;
                                    show_output_fullscreen =
                                        run_config.view_mode == ViewMode::LiveOutput
                                            && run_config.output_mode == OutputMode::Fullscreen;
                                }
                                Err(e) => {
                                    is_churning = false;
                                    churn_filter = None;
                                    churn_limit = None;
                                    churn_target_path = None;
                                    churn_using_sidecar = false;
                                    output_lines.push(format!("Error: {e}"));
                                }
                            }
                        } else {
                            output_lines.push(
                                "⚠ Select at least one test before starting churn (Ctrl+U)."
                                    .to_string(),
                            );
                        }
                        continue;
                    }

                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('e')
                    {
                        failed_tests = extract_failed_tests(&output_lines);
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

                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('p')
                    {
                        show_config = true;
                        config_cursor = 0;
                        continue;
                    }

                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('s' | 'S'))
                    {
                        if collect_selected_tests(tree).is_empty() {
                            output_lines.push(
                                "⚠ Select at least one test before saving a preset.".to_string(),
                            );
                        } else {
                            show_save_preset = true;
                            preset_input_cursor = 0;
                            if preset_name_input.is_empty() {
                                preset_name_input =
                                    format!("Preset {}", run_config.presets.len() + 1);
                            }
                        }
                        continue;
                    }

                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('l' | 'L'))
                    {
                        if run_config.presets.is_empty() {
                            output_lines.push(
                                "⚠ No presets saved yet. Press Ctrl+S to save one.".to_string(),
                            );
                        } else {
                            show_presets = true;
                            preset_list_cursor =
                                preset_list_cursor.min(run_config.presets.len().saturating_sub(1));
                        }
                        continue;
                    }

                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('w' | 'W'))
                    {
                        run_config.manual_watch_enabled = !run_config.manual_watch_enabled;
                        run_config.manual_watch_delay_ms =
                            run_config.manual_watch_delay_ms.clamp(200, 20_000);
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
                        if is_rediscovering {
                            output_lines.push("Rediscovery is already running.".to_string());
                            continue;
                        }

                        output_lines.push(
                            "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
                                .to_string(),
                        );
                        output_lines.push(
                            "🔄 Rediscovering tests (building if needed)... please wait."
                                .to_string(),
                        );
                        output_scroll = 0;
                        output_follow_tail = true;
                        show_output_fullscreen = run_config.view_mode == ViewMode::LiveOutput
                            && run_config.output_mode == OutputMode::Fullscreen;
                        is_rediscovering = true;
                        rediscovery_start = Some(Instant::now());
                        rediscovery_sel = Some(TreeState::capture(tree));

                        let no_restore = run_config.no_restore;
                        let (tx, rx) = mpsc::channel();
                        rediscovery_rx = Some(rx);
                        std::thread::spawn(move || {
                            let result = discover_tests(false, no_restore)
                                .map(|tests| {
                                    let _ = super::discovery_cache::save_discovery_cache(&tests);
                                    tests
                                })
                                .map_err(|e| e.to_string());
                            let _ = tx.send(result);
                        });
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
                                output_scroll =
                                    output_scroll.saturating_add(5).min(output_scroll_max);
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
                            } else if run_config.view_mode == ViewMode::Results
                                && show_output_panel
                                && !results_panel_hidden
                            {
                                results_panel_hidden = true;
                            } else {
                                if run_config.confirm_exit_on_esc {
                                    show_exit_confirm = true;
                                } else {
                                    break;
                                }
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
                                let heading =
                                    format!("━━━ Running {sel_count} selected test(s)… ━━━");
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
                                    &mut run_results,
                                    &mut results_panel_hidden,
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
                                        if let Some(pdi) =
                                            visible_indices.iter().position(|&r| r == pi)
                                        {
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
    let mut should_save_config = false;
    if run_config.tests_pane_rows != tests_pane_rows {
        run_config.tests_pane_rows = tests_pane_rows;
        should_save_config = true;
    }
    if run_config.failed_summary_list_pane_cols != failed_summary_list_pane_cols {
        run_config.failed_summary_list_pane_cols = failed_summary_list_pane_cols;
        should_save_config = true;
    }
    if should_save_config {
        run_config.save();
    }
    super::discovery_cache::save_tree_state(TreeState::capture(tree));
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn is_word_boundary(c: char, prev: Option<char>) -> bool {
    let Some(p) = prev else { return true };
    if c.is_uppercase() && !p.is_uppercase() { return true; }
    if c.is_numeric() && !p.is_numeric() { return true; }
    if p == '_' || p == '.' || !p.is_alphanumeric() { return true; }
    false
}

fn match_abbrev(query: &str, target: &str) -> bool {
    if query.is_empty() { return true; }
    let q_chars: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    let t_chars: Vec<char> = target.chars().collect();
    
    let mut boundaries = Vec::new();
    let mut prev = None;
    for (i, &c) in t_chars.iter().enumerate() {
        if is_word_boundary(c, prev) {
            boundaries.push(i);
        }
        prev = Some(c);
    }
    
    fn dfs(q_idx: usize, b_idx: usize, q_chars: &[char], t_chars: &[char], boundaries: &[usize]) -> bool {
        if q_idx == q_chars.len() {
            return true;
        }
        if b_idx == boundaries.len() {
            return false;
        }
        
        let start = boundaries[b_idx];
        let mut matched = 0;
        while q_idx + matched < q_chars.len() && start + matched < t_chars.len() {
            if q_chars[q_idx + matched] == t_chars[start + matched].to_ascii_lowercase() {
                matched += 1;
                if dfs(q_idx + matched, b_idx + 1, q_chars, t_chars, boundaries) {
                    return true;
                }
            } else {
                break;
            }
        }
        
        dfs(q_idx, b_idx + 1, q_chars, t_chars, boundaries)
    }
    
    dfs(0, 0, &q_chars, &t_chars, &boundaries)
}

#[cfg(test)]
mod search_tests {
    use super::*;

    #[test]
    fn test_match_abbrev() {
        let target = "MoveUnfinalizedRowsToNewBatch_MovesAllRowsForPartiallyProcessedRollup";
        assert!(match_abbrev("MoveunfiRollup", target));
        assert!(match_abbrev("MUR", target));
        assert!(match_abbrev("muro", target));
        assert!(!match_abbrev("MoveunfiRollupZ", target));
        
        let target2 = "Namespace.Class.Method";
        assert!(match_abbrev("NCM", target2));
        assert!(match_abbrev("NaClMe", target2));
        assert!(!match_abbrev("NaClMex", target2));
    }
}
