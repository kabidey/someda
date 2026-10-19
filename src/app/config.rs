//! The app's saved settings: provider, key, model, tool paths.
//!
//! Stored as JSON in the platform config directory
//! (`~/.config/someda/app.json`, `~/Library/Application Support/someda/app.json`,
//! `%APPDATA%\someda\app.json`). The API key is kept here in plain text with
//! owner-only permissions on Unix; it is handed to the engine subprocess
//! through the environment only and never written to any log.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AppConfig {
    /// Provider id from [`super::providers::PROVIDERS`] (e.g. "anthropic").
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    /// Path to kicad-cli when not on PATH.
    #[serde(default)]
    pub kicad_cli: Option<String>,
    /// Path to the someda CLI when not next to the app.
    #[serde(default)]
    pub someda_cli: Option<String>,
    /// Where projects are written (default: a `someda-projects` folder in the user's documents).
    #[serde(default)]
    pub projects_dir: Option<String>,
}

impl AppConfig {
    /// The config file path, if a platform config directory exists.
    pub fn path() -> Option<PathBuf> {
        config_dir().map(|d| d.join("someda").join("app.json"))
    }

    pub fn load() -> AppConfig {
        Self::path().and_then(|p| Self::load_from(&p).ok()).unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Result<AppConfig, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
        serde_json::from_str(&text).map_err(|e| format!("parse {}: {}", path.display(), e))
    }

    pub fn save(&self) -> Result<PathBuf, String> {
        let path = Self::path().ok_or("no config directory on this system")?;
        self.save_to(&path)?;
        Ok(path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {}", dir.display(), e))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| format!("write {}: {}", path.display(), e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// The key with everything but a hint masked, for display.
    pub fn masked_key(&self) -> String {
        let k = self.api_key.trim();
        if k.len() <= 8 {
            return "•".repeat(k.len());
        }
        format!("{}…{}", &k[..4], &k[k.len() - 4..])
    }
}

fn config_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("SOMEDA_CONFIG_DIR") {
        return Some(PathBuf::from(d));
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Application Support"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_masks_the_key() {
        let dir = std::env::temp_dir().join(format!("someda-cfg-{}", std::process::id()));
        let path = dir.join("app.json");
        let c = AppConfig { provider: "openai".into(), base_url: "https://api.openai.com/v1".into(), model: "gpt-4o-mini".into(), api_key: "sk-abcdefghijkl".into(), ..Default::default() };
        c.save_to(&path).unwrap();
        let back = AppConfig::load_from(&path).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.masked_key(), "sk-a…ijkl");
        let _ = std::fs::remove_dir_all(dir);
    }
}
