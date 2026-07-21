use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
pub(crate) enum Verbosity {
    Minimal,
    Normal,
    Detailed,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
pub(crate) enum OutputMode {
    Split,
    Fullscreen,
}

/// How run results are shown in the TUI.
///
/// - [`ViewMode::Results`]: tree colors + pass/fail counts; output only for the focused leaf.
/// - [`ViewMode::LiveOutput`]: classic streaming output panel (previous default behavior).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
pub(crate) enum ViewMode {
    Results,
    LiveOutput,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct TestPreset {
    pub name: String,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub tests: Vec<String>,
}

fn default_output_mode() -> OutputMode {
    OutputMode::Split
}

fn default_view_mode() -> ViewMode {
    ViewMode::Results
}

fn default_manual_watch_delay_ms() -> u32 {
    2000
}

fn default_no_restore() -> bool {
    true
}

fn default_confirm_exit_on_esc() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct RunConfig {
    pub no_build: bool,
    /// When true (default), passes `--no-restore` to `dotnet test` (faster; skips implicit restore).
    #[serde(default = "default_no_restore")]
    pub no_restore: bool,
    pub verbosity: Verbosity,
    /// Ignored; discovery list is cached automatically via workspace fingerprint. Kept so older `.dotest.yml` still deserialize.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    cache_tests: bool,
    #[serde(default = "default_output_mode")]
    pub output_mode: OutputMode,
    /// Results tree (default) vs classic live output panel.
    #[serde(default = "default_view_mode")]
    pub view_mode: ViewMode,
    /// Remembered height of the Tests pane when Output is shown in split mode.
    #[serde(default)]
    pub tests_pane_rows: Option<u16>,
    /// Remembered width of the Failed Tests list pane in the failure summary overlay.
    #[serde(default)]
    pub failed_summary_list_pane_cols: Option<u16>,
    /// When set, a background watcher re-runs **only the tests you have checked in the tree**
    /// when `.cs` files change (debounced). For this option, you choose the scope.
    /// In the future, maybe add an automatic scope based on impact analysis.
    /// I tried, but it didn't work well. Halting for now.
    #[serde(default)]
    pub manual_watch_enabled: bool,
    #[serde(default = "default_manual_watch_delay_ms")]
    pub manual_watch_delay_ms: u32,
    /// When true (default), pressing Esc to quit the app first opens a confirmation dialog.
    #[serde(default = "default_confirm_exit_on_esc")]
    pub confirm_exit_on_esc: bool,
    #[serde(default)]
    pub presets: Vec<TestPreset>,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            no_build: true,
            no_restore: true,
            verbosity: Verbosity::Normal,
            cache_tests: false,
            output_mode: OutputMode::Split,
            view_mode: ViewMode::Results,
            tests_pane_rows: None,
            failed_summary_list_pane_cols: None,
            manual_watch_enabled: false,
            manual_watch_delay_ms: 2000,
            confirm_exit_on_esc: true,
            presets: Vec::new(),
        }
    }
}

impl RunConfig {
    pub(crate) fn load() -> Self {
        if let Ok(s) = std::fs::read_to_string(".dotest.yml") {
            if let Ok(cfg) = serde_yaml::from_str(&s) {
                return cfg;
            }
        }
        RunConfig::default()
    }

    pub(crate) fn save(&self) {
        if let Ok(s) = serde_yaml::to_string(self) {
            let _ = std::fs::write(".dotest.yml", s);
        }
    }
}
