//! Per-test durations parsed from `dotnet test` console lines, ranked slowest first.
//!
//! The ranking is always available in the TUI after a run finishes (Ctrl+T).
//! Writing `bin/dotest/test-metrics.txt` is optional and controlled from Settings.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

/// Stable path so other tools can read the latest ranking without parsing the TUI.
pub(crate) const TEST_METRICS_PATH: &str = "bin/dotest/test-metrics.txt";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TestTiming {
    pub name: String,
    pub status: String,
    pub duration_ms: u64,
}

#[derive(Default)]
pub(crate) struct TestMetrics {
    by_name: HashMap<String, TestTiming>,
    /// Set when the current run has stopped (success, failure, or cancel).
    finished: bool,
}

impl TestMetrics {
    pub(crate) fn clear(&mut self) {
        self.by_name.clear();
        self.finished = false;
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }

    /// Returns true the first time this run is marked finished.
    pub(crate) fn mark_finished(&mut self) -> bool {
        let first = !self.finished;
        self.finished = true;
        first
    }

    pub(crate) fn ingest_line(&mut self, line: &str) {
        let Some(timing) = parse_timing_line(line) else {
            return;
        };
        self.by_name.insert(timing.name.clone(), timing);
    }

    pub(crate) fn len(&self) -> usize {
        self.by_name.len()
    }

    pub(crate) fn total_ms(&self) -> u64 {
        self.by_name.values().map(|t| t.duration_ms).sum()
    }

    /// Slowest first. Equal durations keep a stable name order.
    pub(crate) fn ranked(&self) -> Vec<TestTiming> {
        let mut rows: Vec<TestTiming> = self.by_name.values().cloned().collect();
        rows.sort_by(|a, b| {
            b.duration_ms
                .cmp(&a.duration_ms)
                .then_with(|| a.name.cmp(&b.name))
        });
        rows
    }

    /// Tab-separated ranking. Comment lines start with `#`.
    pub(crate) fn report_text(&self) -> String {
        let mut out = String::new();
        out.push_str("# dotest test metrics\n");
        out.push_str("# ranked slowest to fastest\n");
        out.push_str("# duration_ms is integer milliseconds; 0 means under 1 ms\n");
        out.push_str("# columns: duration_ms, status, name (tab-separated)\n");
        out.push_str(&format!("# count: {}\n", self.len()));
        for row in self.ranked() {
            out.push_str(&format!(
                "{}\t{}\t{}\n",
                row.duration_ms, row.status, row.name
            ));
        }
        out
    }

    pub(crate) fn save(&self) -> io::Result<()> {
        let path = Path::new(TEST_METRICS_PATH);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, self.report_text())
    }
}

pub(crate) fn format_duration_ms(ms: u64) -> String {
    if ms == 0 {
        return "< 1 ms".to_string();
    }
    if ms < 1_000 {
        return format!("{ms} ms");
    }
    if ms < 60_000 {
        return format!("{:.3} s", ms as f64 / 1000.0);
    }
    let mins = ms / 60_000;
    let rem_ms = ms % 60_000;
    if mins < 60 {
        return format!("{mins} m {:.1} s", rem_ms as f64 / 1000.0);
    }
    let hours = mins / 60;
    let mins = mins % 60;
    format!("{hours} h {mins} m")
}

fn parse_timing_line(line: &str) -> Option<TestTiming> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (status, after) = split_outcome(trimmed)?;
    if after.is_empty() || after.starts_with('(') {
        return None;
    }
    let (name, duration_ms) = split_name_and_duration(after)?;
    Some(TestTiming {
        name: name.to_string(),
        status: status.to_string(),
        duration_ms,
    })
}

fn split_outcome(trimmed: &str) -> Option<(&'static str, &str)> {
    let rest = trimmed.trim_start_matches(['✓', '✗', '⚠']).trim();
    if let Some(after) = rest.strip_prefix("Passed ") {
        return Some(("Passed", after));
    }
    if let Some(after) = rest.strip_prefix("Failed ") {
        return Some(("Failed", after));
    }
    if let Some(after) = rest.strip_prefix("Skipped ") {
        return Some(("Skipped", after));
    }
    None
}

fn split_name_and_duration(after: &str) -> Option<(&str, u64)> {
    let open = after.rfind(" [")?;
    if !after.ends_with(']') || open + 2 >= after.len() {
        return None;
    }
    let name = after[..open].trim();
    if name.is_empty() {
        return None;
    }
    let duration_ms = parse_duration_ms(&after[open + 2..after.len() - 1])?;
    Some((name, duration_ms))
}

/// Parse VSTest console durations such as `< 1 ms`, `12 ms`, `1.2 s`, `1 m 2 s`.
fn parse_duration_ms(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.starts_with('<') {
        return Some(0);
    }

    let mut total: f64 = 0.0;
    let mut rest = text;
    let mut saw = false;
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let num_end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        if num_end == 0 {
            return None;
        }
        let num: f64 = rest[..num_end].parse().ok()?;
        if !num.is_finite() || num < 0.0 {
            return None;
        }
        rest = rest[num_end..].trim_start();
        let unit_end = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        if unit_end == 0 {
            return None;
        }
        let unit = &rest[..unit_end];
        rest = &rest[unit_end..];
        let factor = match unit {
            "ms" => 1.0,
            "s" | "sec" | "secs" => 1_000.0,
            "m" | "min" | "mins" => 60_000.0,
            "h" | "hr" | "hrs" => 3_600_000.0,
            "d" => 86_400_000.0,
            _ => return None,
        };
        total += num * factor;
        saw = true;
    }
    if !saw {
        return None;
    }
    Some(total.round().clamp(0.0, u64::MAX as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_console_durations() {
        let row = parse_timing_line("Passed Namespace.Class.Method [12 ms]").unwrap();
        assert_eq!(row.status, "Passed");
        assert_eq!(row.name, "Namespace.Class.Method");
        assert_eq!(row.duration_ms, 12);

        let row = parse_timing_line("  Failed Foo.Bar [1 m 2 s]").unwrap();
        assert_eq!(row.status, "Failed");
        assert_eq!(row.duration_ms, 62_000);

        let row = parse_timing_line("Skipped Foo.Baz [< 1 ms]").unwrap();
        assert_eq!(row.duration_ms, 0);

        let row = parse_timing_line("✓ Passed Fast.One [1.2 s]").unwrap();
        assert_eq!(row.duration_ms, 1_200);
        assert_eq!(row.name, "Fast.One");
    }

    #[test]
    fn ignores_churn_banners_and_summaries() {
        assert!(parse_timing_line("Iteration 3   ✓ Passed (1.2s)").is_none());
        assert!(parse_timing_line("  Finished successfully in 0:12").is_none());
        assert!(parse_timing_line("Passed: 4").is_none());
    }

    #[test]
    fn ranks_slowest_first_and_formats_report() {
        let mut metrics = TestMetrics::default();
        metrics.ingest_line("Passed Fast [5 ms]");
        metrics.ingest_line("Failed Slow [2 s]");
        metrics.ingest_line("Passed Mid [400 ms]");
        // Same test reported again: keep the latest duration.
        metrics.ingest_line("Passed Fast [9 ms]");

        let ranked = metrics.ranked();
        assert_eq!(
            ranked.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["Slow", "Mid", "Fast"]
        );
        assert_eq!(ranked[2].duration_ms, 9);

        let report = metrics.report_text();
        let data: Vec<&str> = report
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();
        assert_eq!(
            data,
            vec!["2000\tFailed\tSlow", "400\tPassed\tMid", "9\tPassed\tFast"]
        );
    }
}
