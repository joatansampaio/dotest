use serde::{Deserialize, Serialize};
use directories::UserDirs;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::fs;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Settings {
    #[serde(default)]
    pub excluded_categories: Vec<String>,
}

pub struct Config {
    settings_path: PathBuf,
}

impl Config {
    pub fn new() -> Result<Self> {
        let user_dirs = UserDirs::new().context("Could not find user directories")?;
        let dotest_dir = user_dirs.home_dir().join(".dotest");
        
        if !dotest_dir.exists() {
            fs::create_dir_all(&dotest_dir).context("Failed to create ~/.dotest directory")?;
        }
        
        let settings_path = dotest_dir.join("settings.json");
        
        Ok(Self { settings_path })
    }

    pub fn load_settings(&self) -> Result<Settings> {
        if !self.settings_path.exists() {
            return Ok(Settings::default());
        }

        let content = fs::read_to_string(&self.settings_path)
            .context("Failed to read settings file")?;
            
        let settings: Settings = serde_json::from_str(&content)
            .unwrap_or_else(|_| Settings::default());
            
        Ok(settings)
    }
}
