use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredTest {
    pub tree_path: String,
    pub filter_key: String,
    pub test_count: usize,
    #[serde(default)]
    pub target_path: Option<String>,
}

impl DiscoveredTest {
    pub fn new(
        tree_path: impl Into<String>,
        filter_key: impl Into<String>,
        test_count: usize,
    ) -> Self {
        Self {
            tree_path: tree_path.into(),
            filter_key: filter_key.into(),
            test_count,
            target_path: None,
        }
    }

    pub fn with_target(mut self, target_path: impl Into<String>) -> Self {
        self.target_path = Some(target_path.into());
        self
    }
}

/// When false, timing lines are written only to `.dotest_discovery.log` (if enabled).
/// The TUI disables stderr logging so F5 rediscovery cannot corrupt the alternate screen.
static DISCOVERY_LOG_STDERR: AtomicBool = AtomicBool::new(true);

pub fn set_discovery_log_stderr(enabled: bool) {
    DISCOVERY_LOG_STDERR.store(enabled, Ordering::Relaxed);
}

fn discovery_timing_enabled() -> bool {
    match std::env::var("DOTEST_DISCOVERY_TIMING") {
        Ok(v) => !(v.is_empty() || v == "0"),
        Err(_) => false,
    }
}

/// Discovery timing. Enable with `DOTEST_DISCOVERY_TIMING=1`.
/// Writes to `.dotest_discovery.log`; also stderr only while the TUI is not active.
pub fn discovery_log(msg: impl AsRef<str>) {
    if !discovery_timing_enabled() {
        return;
    }
    let line = format!("[discovery] {}", msg.as_ref());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(".dotest_discovery.log")
    {
        let _ = writeln!(file, "{line}");
    }
    if DISCOVERY_LOG_STDERR.load(Ordering::Relaxed) {
        eprintln!("{line}");
    }
}
