//! Starting a filtered test run from the interactive UI (Enter, watch, re-run failed, etc.).

use std::sync::mpsc;
use std::time::Instant;

use super::config::{OutputMode, RunConfig, ViewMode};
use super::failed_tests::FailedTestInfo;
use super::output::{spawn_test_run, OutputEvent};
use super::run_results::RunResultsState;

/// Shared by Enter, manual watch, and failed-test reruns. `filter` is the
/// `FullyQualifiedName~…` string from `build_filter` (empty = run all).
#[allow(clippy::too_many_arguments)]
pub(crate) fn launch_filtered_test_run(
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
    run_results: &mut RunResultsState,
    results_panel_hidden: &mut bool,
) {
    // Live Output still wants fullscreen when configured; Results mode never auto-opens
    // the global stream panel.
    *show_output_fullscreen = run_config.view_mode == ViewMode::LiveOutput
        && run_config.output_mode == OutputMode::Fullscreen;
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
            // Clear stale colors; refill as lines arrive.
            run_results.clear();
            *results_panel_hidden = false;
        }
        Err(e) => {
            output_lines.push(format!("Error: {e}"));
        }
    }
}
