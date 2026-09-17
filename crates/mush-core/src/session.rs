//! Conversation persistence.
//!
//! Everything mush writes lives in `<root>/.mush/`, which ignores itself via a
//! one-line `.gitignore`. Opening a folder is therefore the only setup step.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::message::Message;

pub const MUSH_DIR: &str = ".mush";
pub const SESSION_FILE: &str = "session.json";

/// A `.gitignore` that ignores everything, itself included.
const SELF_IGNORE: &str = "*\n";

pub fn mushroom_dir(root: &Path) -> PathBuf {
    root.join(MUSH_DIR)
}

pub fn session_path(root: &Path) -> PathBuf {
    mushroom_dir(root).join(SESSION_FILE)
}

/// Create `.mush/` and make it invisible to git.
pub fn ensure_mush_dir(root: &Path) -> std::io::Result<()> {
    let dir = mushroom_dir(root);
    fs::create_dir_all(&dir)?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        fs::write(&ignore, SELF_IGNORE)?;
    }
    Ok(())
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A stored conversation. The system message is regenerated on load, so only
/// the human/assistant/tool messages are persisted, along with the endpoint
/// selection (provider and base URL) so it survives a restart. The API key is
/// deliberately NOT stored — it comes from the environment or `/key` each run.
#[derive(Default, Serialize, Deserialize)]
pub struct Session {
    pub root: String,
    pub model: String,
    /// Provider name as given by `Provider::name` (e.g. "deepseek").
    #[serde(default)]
    pub provider: String,
    /// Base URL of the endpoint in use; empty when never customized.
    #[serde(default)]
    pub base_url: String,
    pub updated: u64,
    pub messages: Vec<Message>,
}

impl Session {
    pub fn load(root: &Path) -> Option<Self> {
        Self::load_from(&session_path(root))
    }

    pub fn load_from(path: &Path) -> Option<Self> {
        let bytes = fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let path = session_path(root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self).unwrap_or_else(|_| b"{}".to_vec());
        crate::workspace::atomic_write(&path, &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_creates_self_ignoring_dir() {
        let root = std::env::temp_dir().join(format!("mush-session-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        ensure_mush_dir(&root).unwrap();
        let ignore = mushroom_dir(&root).join(".gitignore");
        assert_eq!(fs::read_to_string(ignore).unwrap(), "*\n");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn session_roundtrips() {
        let root = std::env::temp_dir().join(format!("mush-session2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        ensure_mush_dir(&root).unwrap();

        let session = Session {
            root: root.display().to_string(),
            model: "test".into(),
            provider: "custom".into(),
            base_url: "http://localhost:9".into(),
            updated: now_secs(),
            messages: vec![Message::user("hello"), Message::assistant("hi")],
        };
        session.save(&root).unwrap();

        let loaded = Session::load(&root).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].text(), "hello");
        let _ = fs::remove_dir_all(&root);
    }
}
