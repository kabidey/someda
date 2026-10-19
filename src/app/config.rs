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
        use std::io::Write;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {}", dir.display(), e))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("permissions on {}: {}", dir.display(), e))?;
            }
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        // Written to a fresh owner-only file, then renamed over the old one:
        // the key is never readable by others, not even for an instant.
        let tmp = path.with_extension("json.tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600).custom_flags(libc_o_nofollow());
        }
        let mut f = opts.open(&tmp).map_err(|e| format!("create {}: {}", tmp.display(), e))?;
        f.write_all(text.as_bytes()).map_err(|e| format!("write {}: {}", tmp.display(), e))?;
        f.sync_all().ok();
        drop(f);
        std::fs::rename(&tmp, path).map_err(|e| format!("rename {} → {}: {}", tmp.display(), path.display(), e))?;
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

#[cfg(unix)]
fn libc_o_nofollow() -> i32 {
    // O_NOFOLLOW: never write through a symlink planted at the config path
    #[cfg(target_os = "linux")]
    {
        0o400000
    }
    #[cfg(not(target_os = "linux"))]
    {
        0x0100
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
