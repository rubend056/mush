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

/// How a stored agent's last run ended.
///
/// The UI's `Phase` is the live version of this and cannot be stored: a phase
/// carries an `Instant`, and an age frozen at shutdown would be a lie.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredStatus {
    /// Never ran, or its run was interrupted and it is idle again.
    #[default]
    Idle,
    Done,
    Stopped,
    Failed(String),
}

/// Where an agent's isolated work went, once the human landed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredLanded {
    Merged,
    Discarded,
}

/// One subagent of a stored conversation.
///
/// Its transcript is here so a follow-up survives a restart. Without it a
/// relaunch forgot every child's context, and "continue that agent" really meant
/// writing the brief again from scratch.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: u64,
    #[serde(default)]
    pub parent: Option<u64>,
    #[serde(default)]
    pub depth: usize,
    #[serde(default)]
    pub brief: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub status: StoredStatus,
    #[serde(default)]
    pub landed: Option<StoredLanded>,
    #[serde(default)]
    pub leftover: bool,
    /// The result the row showed, so a restored tree does not lose it.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub messages: Vec<Message>,
}

/// A line mush wrote about a conversation that is worth keeping: a run's
/// failure.
///
/// There is no kind field because only one kind is stored. A line that answered
/// a command the human typed (`/help`, the `git diff` a `/diff` printed, a run's
/// usage line) answered *that* moment; a restart has no such moment to answer,
/// so it is dropped rather than restored out of context. A failure belongs to
/// its run, not to the moment it was read, and the human coming back to a
/// workspace that broke is the one reader of this file who needs it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredNotice {
    /// The agent the line concerns. Tagged, never global: a root failure must
    /// not be painted into a child's pane (finding B19), and the tag is what
    /// survives the round trip.
    pub agent: u64,
    /// When it happened, in Unix seconds. The stored phase carries an `Instant`
    /// and cannot survive a restart; this is what lets a restored line say
    /// *when* rather than only *what*.
    #[serde(default)]
    pub at: u64,
    pub text: String,
}

/// A stored conversation. The system message is regenerated on load, so only
/// the human/assistant/tool messages are persisted, along with the endpoint
/// selection (provider and base URL) so it survives a restart. The API key is
/// deliberately NOT stored — it comes from the environment or `/key` each run.
#[derive(Default, Serialize, Deserialize)]
pub struct Session {
    pub root: String,
    pub model: String,
    /// Provider name as given by [`Provider::name`], i.e. a name
    /// `--provider` accepts (see `provider::PROVIDERS`).
    #[serde(default)]
    pub provider: String,
    /// Base URL of the endpoint in use; empty when never customized.
    #[serde(default)]
    pub base_url: String,
    /// A context window the human stated for this workspace (`/context`, or a
    /// `MUSH_CONTEXT` at the time). Derived windows are never stored: they are
    /// re-read from the endpoint, so a stale guess cannot outlive its cause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<usize>,
    pub updated: u64,
    pub messages: Vec<Message>,
    /// The subagents this conversation had, so their context outlives the
    /// process. Old sessions have none and still load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<AgentSession>,
    /// The failures mush wrote about this conversation, oldest first. Notices
    /// used to live only in memory, so returning to a workspace whose run had
    /// failed said nothing about it at all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<StoredNotice>,
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
            context: Some(123_456),
            updated: now_secs(),
            messages: vec![
                Message::user("hello"),
                Message {
                    reasoning_content: Some("weighing the greeting".into()),
                    ..Message::assistant("hi")
                },
            ],
            agents: vec![AgentSession {
                id: 3,
                parent: Some(0),
                depth: 1,
                brief: "port the parser".into(),
                branch: Some("mush/3".into()),
                status: StoredStatus::Stopped,
                landed: Some(StoredLanded::Merged),
                leftover: true,
                summary: Some("did the thing".into()),
                messages: vec![Message::user("do it"), Message::assistant("done")],
            }],
            notices: vec![StoredNotice {
                agent: 3,
                at: 1_700_000_000,
                text: "could not compact: the endpoint refused the request".into(),
            }],
        };
        session.save(&root).unwrap();

        let loaded = Session::load(&root).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].text(), "hello");
        // A thinking turn's reasoning has to survive the save: a resumed agent
        // replays this history, and the endpoint refuses the turn without it.
        assert_eq!(
            loaded.messages[1].reasoning_content.as_deref(),
            Some("weighing the greeting")
        );
        assert_eq!(loaded.context, Some(123_456));
        // The child's context is the point: it must survive the round trip.
        assert_eq!(loaded.agents.len(), 1);
        let child = &loaded.agents[0];
        assert_eq!(child.id, 3);
        assert_eq!(child.brief, "port the parser");
        assert_eq!(child.status, StoredStatus::Stopped);
        assert_eq!(child.landed, Some(StoredLanded::Merged));
        assert!(child.leftover);
        assert_eq!(child.summary.as_deref(), Some("did the thing"));
        assert_eq!(child.messages.len(), 2);
        assert_eq!(child.messages[0].text(), "do it");
        // A failure outlives the run and the process: this is the one line a
        // human comes back to a broken workspace for.
        assert_eq!(loaded.notices.len(), 1);
        assert_eq!(loaded.notices[0].agent, 3);
        assert_eq!(loaded.notices[0].at, 1_700_000_000);
        assert!(loaded.notices[0].text.contains("could not compact"));
        let _ = fs::remove_dir_all(&root);
    }

    /// Old sessions have no context field; they must still load.
    #[test]
    fn a_session_without_a_context_loads() {
        let root = std::env::temp_dir().join(format!("mush-session3-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        ensure_mush_dir(&root).unwrap();
        fs::write(
            session_path(&root),
            r#"{"root":"/tmp","model":"m","provider":"custom","base_url":"","updated":1,"messages":[]}"#,
        )
        .unwrap();

        let loaded = Session::load(&root).unwrap();
        assert_eq!(loaded.context, None);
        assert!(loaded.agents.is_empty(), "no agents, not a parse failure");
        assert!(
            loaded.notices.is_empty(),
            "a session written before notices existed still loads"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
