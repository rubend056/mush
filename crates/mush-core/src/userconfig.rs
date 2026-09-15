//! User-level configuration stored in the home directory.
//!
//! Unlike the per-workspace session (`.mush/session.json`), this file is
//! machine-global: it is where the API key lives (the session deliberately
//! never stores secrets) and it can hold default provider / endpoint / model
//! choices. Resolution order on startup is
//! `CLI flags > environment > saved session > this file > built-in defaults`.
//!
//! Path: `$MUSH_CONFIG`, else `$XDG_CONFIG_HOME/mush/config.json`,
//! else `~/.config/mush/config.json`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::workspace::atomic_write;

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct UserConfig {
    /// API key for the provider. Stored in plain text on your own machine;
    /// never written to the workspace.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Provider name as given by `Provider::name` (e.g. "deepseek").
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
}

/// Where the user config lives. `MUSH_CONFIG` overrides the path for tests and
/// unusual setups.
pub fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("MUSH_CONFIG") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("mush/config.json");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".config/mush/config.json");
        }
    }
    PathBuf::from(".mush-user-config.json")
}

impl UserConfig {
    pub fn load() -> Self {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&config_path())
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self).unwrap_or_else(|_| b"{}".to_vec());
        atomic_write(path, &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_via_explicit_path() {
        let dir = std::env::temp_dir().join(format!("mush-userconfig-{}", std::process::id()));
        let path = dir.join("config.json");
        let user = UserConfig {
            api_key: Some("sk-test-1234".into()),
            provider: "deepseek".into(),
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-flash".into(),
        };
        user.save_to(&path).unwrap();
        let loaded = UserConfig::load_from(&path);
        assert_eq!(loaded.api_key.as_deref(), Some("sk-test-1234"));
        assert_eq!(loaded.provider, "deepseek");
        assert_eq!(loaded.model, "deepseek-flash");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_defaults() {
        let user = UserConfig::load_from(Path::new("/nonexistent/mush/config.json"));
        assert!(user.api_key.is_none());
        assert!(user.provider.is_empty());
    }
}