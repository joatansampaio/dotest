use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
pub(super) enum Verbosity {
    Minimal,
    Normal,
    Detailed,
}

#[derive(Serialize, Deserialize, Clone)]
pub(super) struct RunConfig {
    pub no_build: bool,
    pub verbosity: Verbosity,
    pub cache_tests: bool,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            no_build: true,
            verbosity: Verbosity::Normal,
            cache_tests: false,
        }
    }
}

impl RunConfig {
    pub(super) fn load() -> Self {
        if let Ok(s) = std::fs::read_to_string(".dotest.yml") {
            if let Ok(cfg) = serde_yaml::from_str(&s) {
                return cfg;
            }
        }
        RunConfig::default()
    }

    pub(super) fn save(&self) {
        if let Ok(s) = serde_yaml::to_string(self) {
            let _ = std::fs::write(".dotest.yml", s);
        }
    }
}
