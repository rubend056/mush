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
///
/// Five values for four endings, because one of them is not an ending:
/// `Running` is written while a run is *in flight* and no run ever writes it as
/// its own result — so a file read back with `Running` in it is a file whose
/// process went away with the run still going. That is the one cut-off a
/// restart can prove, and it is why `Running` exists at all: the old writer
/// flattened a mid-run agent to `Idle`, and a killed agent came back looking
/// exactly like one that had never been asked to do anything (`docs/findings.md`
/// H2).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredStatus {
    /// Never ran, or its run ended without a result the row kept.
    #[default]
    Idle,
    /// A run was in flight when this status was written. It is never an
    /// ending: `Done`, `Stopped` and `Failed` are what a run that ends writes.
    /// A file that still says `running` is the record of a run nobody finished
    /// — the process, the terminal, or the harness went away first.
    Running,
    /// The run never ended. Distinct from `Stopped` (the human's Ctrl-C: the
    /// actor is alive and a message resumes it) and from `Failed` (the model or
    /// the endpoint said no): nothing was committed by that run, and the agent
    /// is not waiting for anything.
    CutOff,
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
    /// The name the caller gave this agent (`spawn_agent`'s `title`), so a
    /// restored row keeps it; absent means the row falls back to the handle it
    /// derives from the brief.
    #[serde(default)]
    pub title: Option<String>,
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
    /// The parent had not read this agent's result when the file was written.
    /// The ✉ mark is a delivery fact, not a phase: it survives a restart, so a
    /// human who reopens the workspace still sees whose work is waiting to be
    /// read (finding H1).
    #[serde(default)]
    pub result_unread: bool,
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

/// What a workspace's `.mush/session.json` holds, as read.
///
/// `Option<Session>` could not tell "there is no session here" apart from
/// "there is one and mush cannot use it": both came back `None`, the app opened
/// empty either way, and the first save wrote a fresh conversation over the only
/// copy of the old one — no warning, no backup. This is the distinction, so the
/// caller can say which of the two it found.
pub enum Stored {
    /// There is no session file. A fresh workspace, and nothing to say about
    /// it: silence is the honest answer here, and the one `Unusable` must not
    /// be mistaken for.
    Absent,
    /// The conversation, as it was left.
    Loaded(Session),
    /// The file is there and cannot be used. The string is *why* — what serde
    /// objected to, or the IO error — for the human who has to decide what to
    /// do with the copy that [`keep_unreadable`] sets aside.
    Unusable(String),
}

/// Spelled by hand because a [`Session`] is a whole conversation and printing
/// one in a test failure would bury the fact being asserted.
impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stored::Absent => write!(f, "Absent"),
            Stored::Loaded(_) => write!(f, "Loaded(<session>)"),
            Stored::Unusable(reason) => write!(f, "Unusable({reason:?})"),
        }
    }
}
/// How many backup names mush will try beside an unreadable session before it
/// gives up looking. A workspace that has been hand-broken a hundred times has
/// a problem that no file name solves.
const BACKUP_TRIES: u32 = 100;

/// Where an unreadable session is kept, before the caller starts numbering:
/// `.mush/session.json.bak`.
fn first_backup(root: &Path) -> PathBuf {
    mushroom_dir(root).join(format!("{SESSION_FILE}.bak"))
}

/// The sentence a session mush could not set aside is told: the file the human
/// has to go and find, then why it could not be moved.
///
/// One shape for every failure of that move, so a third reason cannot be told
/// without naming the file (refactor R18): the name is the whole point of the
/// line — the human reads it to find the copy — and the reason is what lets
/// them fix it.
fn cannot_keep(from: &Path, why: impl std::fmt::Display) -> String {
    format!(
        "cannot keep {} — {why}",
        from.file_name().unwrap_or_default().to_string_lossy()
    )
}

/// Move a session file mush cannot read beside itself, so the save that
/// follows cannot destroy the only copy of the human's conversation.
///
/// A rename in the same directory: the bytes are never rewritten and never
/// leave the workspace. A backup that is already there is *not* overwritten —
/// the next free name (`.bak.2`, `.bak.3`, …) is used instead, so hand-breaking
/// the file twice does not lose the first copy either. Returns where it went.
///
/// This is deliberately not called for an [`Stored::Absent`] workspace: there
/// is nothing to keep, and creating a backup of nothing would be a file a human
/// has to wonder about.
pub fn keep_unreadable(root: &Path) -> Result<PathBuf, String> {
    let from = session_path(root);
    let base = first_backup(root);
    for step in 1..=BACKUP_TRIES {
        let to = if step == 1 {
            base.clone()
        } else {
            PathBuf::from(format!("{}.{step}", base.display()))
        };
        if to.exists() {
            continue;
        }
        return fs::rename(&from, &to)
            .map(|()| to)
            .map_err(|error| cannot_keep(&from, error));
    }
    Err(cannot_keep(&from, "every backup name beside it is taken"))
}

impl Session {
    /// Read what the workspace stores, distinguishing *absent* from
    /// *unreadable* (see [`Stored`]). Callers that only need the conversation —
    /// the config precedence, a test — want [`Self::load`].
    pub fn read(root: &Path) -> Stored {
        Self::read_from(&session_path(root))
    }

    pub fn read_from(path: &Path) -> Stored {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            // No file is not a file mush cannot use: a workspace nobody has
            // opened yet must stay silent.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Stored::Absent,
            Err(error) => return Stored::Unusable(format!("cannot read the file — {error}")),
        };
        match serde_json::from_slice(&bytes) {
            Ok(session) => Stored::Loaded(session),
            // The position serde names is what makes a hand edit findable, so
            // it is carried through rather than flattened into "bad json".
            Err(error) => Stored::Unusable(error.to_string()),
        }
    }

    /// The conversation, or `None` when there is none *or* when the one there
    /// is cannot be used. `None` is the truth for the layers that only want the
    /// stored endpoint selection, and it is why [`Self::read`] exists: the two
    /// cases must not be indistinguishable to the caller that overwrites the
    /// file.
    pub fn load(root: &Path) -> Option<Self> {
        Self::load_from(&session_path(root))
    }

    pub fn load_from(path: &Path) -> Option<Self> {
        match Self::read_from(path) {
            Stored::Loaded(session) => Some(session),
            Stored::Absent | Stored::Unusable(_) => None,
        }
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

    /// An `absent` session and one mush *cannot read* are two different facts,
    /// and telling them apart is what stops the second from being silently
    /// overwritten: the conversation the human left here — plus one agent, in
    /// the finding — has to survive the first save of the fresh one.
    #[test]
    fn an_unreadable_session_is_told_apart_from_an_absent_one() {
        let root = std::env::temp_dir().join(format!("mush-session4-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        ensure_mush_dir(&root).unwrap();

        // Nothing stored: silence, and no backup of nothing.
        assert!(matches!(Session::read(&root), Stored::Absent));
        assert!(Session::load(&root).is_none(), "absent is still `None`");

        // A session a schema change (or a hand edit) made unreadable. The one
        // finding S3 bisected to: `"status": "failed"` where the real encoding
        // is `{"failed": "…"}`.
        let broken = r#"{
          "root": "/tmp/ws",
          "model": "deepseek-chat",
          "updated": 7,
          "messages": [
            {"role": "user", "content": "a conversation worth keeping"},
            {"role": "assistant", "content": "and the reply to it"}
          ],
          "agents": [
            {"id": 1, "brief": "port the parser", "status": "failed", "messages": []}
          ]
        }"#;
        fs::write(session_path(&root), broken).unwrap();

        let reason = match Session::read(&root) {
            Stored::Unusable(reason) => reason,
            other => panic!("expected Unusable, got {other:?}"),
        };
        assert!(
            reason.contains("status") || reason.contains("line"),
            "the reason must be something a human can act on: {reason}"
        );
        assert!(
            Session::load(&root).is_none(),
            "an unreadable session is still `None` for the layers that only want the conversation"
        );

        // Keep it, then save the fresh conversation over the path it had.
        let kept = keep_unreadable(&root).unwrap();
        assert_eq!(kept, root.join(".mush/session.json.bak"));
        assert!(
            !session_path(&root).exists(),
            "the unreadable file is not both kept and in the way"
        );
        saying("the new conversation").save(&root).unwrap();

        assert!(
            matches!(Session::read(&root), Stored::Loaded(_)),
            "the save wrote a session the next start can read"
        );
        assert_eq!(
            fs::read_to_string(&kept).unwrap(),
            broken,
            "and the only copy of the old conversation is byte for byte what it was"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Breaking the file twice must not lose the first copy: the backup name is
    /// searched, not reused.
    #[test]
    fn keeping_a_second_unreadable_session_does_not_overwrite_the_first() {
        let root = std::env::temp_dir().join(format!("mush-session5-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        ensure_mush_dir(&root).unwrap();

        fs::write(session_path(&root), "first").unwrap();
        let first = keep_unreadable(&root).unwrap();
        fs::write(session_path(&root), "second").unwrap();
        let second = keep_unreadable(&root).unwrap();

        assert_ne!(first, second);
        assert_eq!(fs::read_to_string(&first).unwrap(), "first");
        assert_eq!(fs::read_to_string(&second).unwrap(), "second");
        assert_eq!(second, root.join(".mush/session.json.bak.2"));
        let _ = fs::remove_dir_all(&root);
    }

    /// Both ways the copy can fail to be set aside are told the same way: the
    /// file the human must go and find, then why. Nothing covered an `Err` at
    /// all before this — the fixing wave's test only ever read the successful
    /// rename (refactor R18).
    #[test]
    fn a_session_that_cannot_be_kept_still_names_the_file() {
        let root = std::env::temp_dir().join(format!("mush-session6-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        ensure_mush_dir(&root).unwrap();

        // Nothing to move: the rename fails, and the sentence still names the
        // file rather than the OS's error alone.
        let error = keep_unreadable(&root).unwrap_err();
        assert!(error.starts_with("cannot keep session.json — "), "{error}");

        // Every name beside it taken — the other `Err`, and the one a workspace
        // that has been hand-broken a hundred times reaches.
        fs::write(session_path(&root), "{}").unwrap();
        for step in 1..=BACKUP_TRIES {
            let name = if step == 1 {
                format!("{SESSION_FILE}.bak")
            } else {
                format!("{SESSION_FILE}.bak.{step}")
            };
            fs::write(mushroom_dir(&root).join(name), "kept").unwrap();
        }
        let error = keep_unreadable(&root).unwrap_err();
        assert!(error.starts_with("cannot keep session.json — "), "{error}");
        assert!(
            error.contains("every backup name beside it is taken"),
            "and it says why: {error}"
        );
        assert!(
            session_path(&root).exists(),
            "nothing was moved, so nothing may claim it was"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The session a test writes, so the round trip below is about the file
    /// rather than about the fields.
    fn saying(text: &str) -> Session {
        Session {
            root: String::new(),
            model: "test".into(),
            provider: "custom".into(),
            base_url: String::new(),
            context: None,
            updated: 0,
            messages: vec![Message::user(text)],
            agents: Vec::new(),
            notices: Vec::new(),
        }
    }

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
                title: Some("parser port".into()),
                branch: Some("mush/3".into()),
                status: StoredStatus::Stopped,
                landed: Some(StoredLanded::Merged),
                leftover: true,
                summary: Some("did the thing".into()),
                result_unread: true,
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
        assert_eq!(child.title.as_deref(), Some("parser port"));
        assert_eq!(child.status, StoredStatus::Stopped);
        assert_eq!(child.landed, Some(StoredLanded::Merged));
        assert!(child.leftover);
        assert_eq!(child.summary.as_deref(), Some("did the thing"));
        assert!(
            child.result_unread,
            "an unread result is a fact about delivery, and it survives the file"
        );
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
