//! Conversation persistence.
//!
//! Everything mush writes lives in `<root>/.mush/`, which ignores itself via a
//! one-line `.gitignore`. Opening a folder is therefore the only setup step.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::message::Message;
use crate::transcript::cap_transcript;
pub use crate::transcript::Dropped;

pub const MUSH_DIR: &str = ".mush";
pub const SESSION_FILE: &str = "session.json";

/// How much of one subagent's transcript a save may store, in serialized
/// bytes.
///
/// A child's transcript is the largest thing in the file that grows without
/// the human's hand on it — one measured child was 318.9 KiB after ~80
/// messages, and the root of a run with a few hundred children reached ~100 MB
/// — so this is what keeps a save's cost proportional to the conversation
/// instead of to the number of children ever spawned. The brief opens the
/// transcript and is the one message the cut keeps whatever the cap, and the
/// system prompt is regenerated on the way back in, so a child revived from
/// its newest 256 KiB still knows what it was asked to do and can be nudged
/// onwards.
///
/// Counted in what the file pays (see [`Dropped`]), not in `Message::weight`,
/// which is a token estimate for a different reader.
pub const SESSION_AGENT_BYTES: usize = 256 * 1024;

/// How much of the root transcript a save may store, in the same serialized
/// bytes. Much higher than a child's because the root is the human's own words
/// and mush's scrollback contract: this is a runaway guard, not a budget, and
/// when it trips the file says so — [`Session::truncated`] is the marker a
/// reader needs to tell a conversation that began at the beginning from one
/// that was cut.
pub const SESSION_ROOT_BYTES: usize = 32 * 1024 * 1024;

/// One save reads both caps, so the ordering between them is a fact about the
/// file, not a preference: a root guard anywhere near a child's budget would
/// cut the human's own conversation down to what one subagent is allowed. A
/// compile-time check because the numbers are constants and a runtime test
/// could only restate them.
const _: () = assert!(SESSION_ROOT_BYTES >= 64 * SESSION_AGENT_BYTES);

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
    /// What capping the root transcript cut off its oldest end, if anything.
    ///
    /// Absent — the field is not written — is a conversation stored whole;
    /// `Some` is a file that says on its face that what it holds is the newest
    /// part of one. Without it a reader (and the next launch) could not tell a
    /// conversation that began at the beginning from one cut at
    /// [`SESSION_ROOT_BYTES`], which is the one thing a cap on the human's own
    /// words must not do. Children carry no marker: their cap is what lets a
    /// whole tree of them survive a restart at all, and a child whose oldest
    /// turns are gone still has its brief and its newest work in hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<Dropped>,
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

    /// Write the conversation to `<root>/.mush/session.json`, bounding every
    /// stored transcript first ([`SESSION_ROOT_BYTES`],
    /// [`SESSION_AGENT_BYTES`]).
    ///
    /// Takes `self` by value because bounding drains what it writes: the
    /// caller handed over a snapshot it will not read again (see
    /// `session_save`'s writer), and cloning the newest part of every
    /// transcript just to serialize it would make a save cost more than the
    /// bytes it stores. The conversation in memory is untouched — the UI still
    /// scrolls back through all of it, and only the copy on disk is cut.
    pub fn save(mut self, root: &Path) -> std::io::Result<()> {
        self.bound_stored();
        let path = session_path(root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(&self).unwrap_or_else(|_| b"{}".to_vec());
        crate::workspace::atomic_write(&path, &json)
    }

    /// Cut every stored transcript to the cap it ships with.
    fn bound_stored(&mut self) {
        self.bound_stored_at(SESSION_ROOT_BYTES, SESSION_AGENT_BYTES);
    }

    /// [`Self::bound_stored`] with caps a test can afford: the real root cap is
    /// tens of MiB, and a test that had to build one to prove the rule would
    /// cost more than the rule is worth. The cuts themselves are the same
    /// function at either size.
    ///
    /// The root's marker accumulates, because the file it describes does: a
    /// transcript loaded already cut only ever loses more, so what this save
    /// cuts is counted on top of what the file already said. A marker that
    /// survives a save dropping nothing is what keeps a restored conversation
    /// marked as incomplete instead of letting one quiet write relabel it.
    fn bound_stored_at(&mut self, root_cap: usize, agent_cap: usize) {
        let cut = cap_transcript(&mut self.messages, root_cap);
        self.truncated = match (self.truncated, cut.messages) {
            (Some(earlier), _) => Some(Dropped {
                messages: earlier.messages + cut.messages,
                bytes: earlier.bytes + cut.bytes,
            }),
            (None, 0) => None,
            (None, _) => Some(cut),
        };
        for agent in &mut self.agents {
            cap_transcript(&mut agent.messages, agent_cap);
        }
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
        let rename_failed = keep_unreadable(&root).unwrap_err();
        assert!(
            rename_failed.starts_with("cannot keep session.json — "),
            "{rename_failed}"
        );

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
        assert_ne!(
            error, rename_failed,
            "the two ways a keep fails must read as two reasons: {rename_failed} vs {error}"
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
            truncated: None,
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
            truncated: None,
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
        assert!(
            loaded.truncated.is_none(),
            "and one written before the cap existed is a whole conversation"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A round for a stored transcript, sized so a cap can be aimed at whole
    /// segments instead of at whatever the JSON happens to weigh. The answer
    /// carries a body of its own, like a real one: a fixture of two-byte
    /// answers would spend a quarter of the pretty file on indentation and
    /// make the bound below a test of `serde_json`'s formatting.
    fn round(index: usize, bytes: usize) -> Vec<Message> {
        vec![
            Message::user(format!("ask {index} {}", "x".repeat(bytes))),
            Message::assistant(format!("ok {}", "y".repeat(bytes / 4))),
        ]
    }

    /// What a slice of a transcript costs the file, counted the way the cap
    /// counts: one compact serialization per message.
    fn cost(messages: &[Message]) -> usize {
        messages
            .iter()
            .map(|message| serde_json::to_vec(message).unwrap().len())
            .sum()
    }

    /// A child carrying `messages`, for tests that are about one transcript and
    /// not about the tree.
    fn child(messages: Vec<Message>) -> AgentSession {
        AgentSession {
            id: 7,
            parent: Some(0),
            depth: 1,
            brief: "port the parser".into(),
            title: None,
            branch: None,
            status: StoredStatus::Done,
            landed: None,
            leftover: false,
            summary: None,
            result_unread: false,
            messages,
        }
    }

    /// An assistant turn that asks for one call, so a fixture can carry the
    /// call/result pairs the cut must not separate.
    fn calling(id: &str, text: String) -> Message {
        let mut message = Message::assistant(text);
        message.tool_calls = Some(vec![crate::ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: crate::FunctionCall {
                name: "read_file".into(),
                arguments: "{}".into(),
            },
        }]);
        message
    }

    /// A directory and no leftovers, for the tests that go through a file.
    fn temp_root(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("mush-cap-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// A child's transcript is cut to the cap at write time: the newest work
    /// and the brief survive, the middle goes, and what is left is at most the
    /// cap plus the one message the cap never takes.
    #[test]
    fn a_child_over_the_cap_stores_the_newest_work_and_its_brief() {
        let cap = 8 * 1024;
        let mut session = saying("the root");
        let full: Vec<Message> = (0..40).flat_map(|i| round(i, 400)).collect();
        let full_len = full.len();
        session.agents.push(child(full));
        session.bound_stored_at(usize::MAX, cap);

        let stored = &session.agents[0].messages;
        let head = cost(&stored[..1]);
        assert!(cost(stored) <= cap + head, "{} bytes", cost(stored));
        assert!(
            cost(stored) > cap / 2,
            "the cap is used, not merely tripped: {} bytes",
            cost(stored)
        );
        assert!(stored[0].text().starts_with("ask 0 "));
        assert!(
            stored[1].text().starts_with("ok "),
            "whole segments, not half a round"
        );
        assert!(
            stored[stored.len() - 2].text().starts_with("ask 39 "),
            "the newest round is what a restart resumes from: {}",
            stored[stored.len() - 2].text()
        );
        assert!(stored.last().unwrap().text().starts_with("ok "));
        assert!(stored.len() < full_len, "something was actually cut");
        assert_eq!(
            session.truncated, None,
            "a child alone does not mark the root"
        );
    }

    /// The cap the file actually gets is [`SESSION_AGENT_BYTES`]: a child a
    /// little over it is stored a little under, with every other field of its
    /// row untouched.
    #[test]
    fn save_applies_the_shipped_child_cap() {
        let root = temp_root("child");
        let mut session = saying("the root");
        let full: Vec<Message> = (0..900).flat_map(|i| round(i, 400)).collect();
        let full_len = full.len();
        session.agents.push(child(full));
        session.save(&root).unwrap();

        let loaded = Session::load(&root).unwrap();
        let stored = &loaded.agents[0].messages;
        let head = cost(&stored[..1]);
        assert!(
            cost(stored) <= SESSION_AGENT_BYTES + head,
            "{} bytes for a {SESSION_AGENT_BYTES}-byte cap",
            cost(stored)
        );
        assert!(cost(stored) > SESSION_AGENT_BYTES / 2);
        assert!(stored.len() < full_len, "the fixture must actually be cut");
        assert!(stored[0].text().starts_with("ask 0 "), "the task stays");
        assert!(stored[stored.len() - 2].text().starts_with("ask 899 "));
        assert!(stored.last().unwrap().text().starts_with("ok "));
        assert_eq!(loaded.agents[0].brief, "port the parser");
        let _ = fs::remove_dir_all(&root);
    }

    /// An assistant's calls and the results that answer them cross the cut
    /// together: a stored child never comes back with a call whose answers are
    /// gone or an answer whose call is gone, because the endpoint would reject
    /// the next request and the child's history would lie about what it knows.
    #[test]
    fn a_stored_child_keeps_every_call_and_its_results_together() {
        let cap = 4 * 1024;
        let mut messages = vec![Message::user("the brief")];
        for i in 0..60 {
            messages.push(calling(
                &format!("call{i}"),
                format!("step {i} {}", "y".repeat(200)),
            ));
            messages.push(Message::tool(format!("call{i}"), format!("result {i}")));
        }
        let mut session = saying("the root");
        session.agents.push(child(messages));
        session.bound_stored_at(usize::MAX, cap);

        let stored = &session.agents[0].messages;
        assert!(stored.len() >= 3, "the fixture must actually be cut");
        assert_eq!(stored[0].text(), "the brief");
        let calls: Vec<&str> = stored
            .iter()
            .flat_map(|message| message.tool_calls().iter().map(|call| call.id.as_str()))
            .collect();
        let answered: Vec<&str> = stored
            .iter()
            .filter_map(|message| message.tool_call_id.as_deref())
            .collect();
        assert_eq!(
            calls, answered,
            "no call without its answer, no orphan answer"
        );
        assert!(!calls.is_empty(), "some work survives");
        assert!(
            stored
                .iter()
                .any(|message| message.text().starts_with("step 59 ")),
            "and the newest step is among it"
        );
    }

    /// Bounding is a cut into the stored copy, not a decay: saving the same
    /// session again writes the same bytes, so a crash and a restart cannot
    /// keep shrinking what is stored one write at a time.
    #[test]
    fn a_bounded_session_saves_the_same_bytes_twice() {
        let root = temp_root("idempotent");
        let mut session = saying("the root");
        session.messages = (0..80).flat_map(|i| round(i, 500)).collect();
        session
            .agents
            .push(child((0..900).flat_map(|i| round(i, 400)).collect()));
        session.bound_stored_at(4 * 1024, SESSION_AGENT_BYTES);
        session.save(&root).unwrap();
        let first = fs::read(session_path(&root)).unwrap();

        let loaded = Session::load(&root).unwrap();
        loaded.save(&root).unwrap();
        let second = fs::read(session_path(&root)).unwrap();

        assert_eq!(
            first, second,
            "a save of what a save stored changes nothing"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The root's cap is a guard, not a budget, and it is *recorded*: the file
    /// says how much of the human's conversation is not in it, keeps the newest
    /// messages, and keeps that marker through a save that stops cutting.
    #[test]
    fn a_root_over_its_cap_is_marked_and_truncated_from_the_middle() {
        let root_cap = 6 * 1024;
        let mut session = saying("the root");
        session.messages = (0..60).flat_map(|i| round(i, 500)).collect();
        let full_len = session.messages.len();
        session.bound_stored_at(root_cap, SESSION_AGENT_BYTES);

        let dropped = session.truncated.expect("the root was cut, and says so");
        assert_eq!(dropped.messages, full_len - session.messages.len());
        assert!(dropped.bytes > 0);
        // Never the newest messages, never the opening task: a saved
        // conversation still ends where the human left it and still says what
        // it was about.
        assert!(session.messages[0].text().starts_with("ask 0 "));
        assert!(session.messages.last().unwrap().text().starts_with("ok "));
        assert!(session.messages[session.messages.len() - 2]
            .text()
            .starts_with("ask 59 "));

        // A second save cannot unmark it: the conversation in the file is still
        // cut, even though this save had nothing left to drop.
        let stored_len = session.messages.len();
        session.bound_stored_at(root_cap, SESSION_AGENT_BYTES);
        assert_eq!(session.truncated, Some(dropped));
        assert_eq!(session.messages.len(), stored_len);

        // And the marker counts the whole conversation, not the last save: what
        // the file already said plus what this cut gave up.
        session
            .messages
            .extend((60..120).flat_map(|i| round(i, 500)));
        session.bound_stored_at(root_cap, SESSION_AGENT_BYTES);
        let more = session.truncated.unwrap();
        assert!(
            more.messages > dropped.messages,
            "{} <= {}",
            more.messages,
            dropped.messages
        );
        assert!(more.bytes > dropped.bytes);
        assert!(session.messages[0].text().starts_with("ask 0 "));
        assert!(session.messages.last().unwrap().text().starts_with("ok "));
    }

    /// The marker is what the next launch reads: a bounded session writes it to
    /// the file, and the loaded one carries the same counts. Without the round
    /// trip the first quiet save after a restart would relabel a cut
    /// conversation as whole.
    #[test]
    fn the_truncation_marker_round_trips_through_the_file() {
        let root = temp_root("marker");
        let mut session = saying("the root");
        session.messages = (0..40).flat_map(|i| round(i, 500)).collect();
        session.bound_stored_at(4 * 1024, SESSION_AGENT_BYTES);
        let marker = session.truncated.expect("the root was cut");
        session.save(&root).unwrap();

        let loaded = Session::load(&root).unwrap();
        assert_eq!(loaded.truncated, Some(marker));

        // And a whole conversation writes no marker at all: the field is
        // additive, so a file that predates the cap still reads and a file that
        // does not need one does not carry the shape of a cut one.
        loaded.save(&root).unwrap();
        let loaded_again = Session::load(&root).unwrap();
        assert_eq!(loaded_again.truncated, Some(marker));
        let _ = fs::remove_dir_all(&root);

        let whole = temp_root("whole");
        saying("a conversation that fits").save(&whole).unwrap();
        let json = fs::read_to_string(session_path(&whole)).unwrap();
        assert!(!json.contains("truncated"), "{json}");
        assert!(Session::load(&whole).unwrap().truncated.is_none());
        let _ = fs::remove_dir_all(&whole);
    }

    /// The shipped caps: the child's is the measured one, and the root's is
    /// much higher — a runaway guard for the human's own words, not a budget
    /// for them. The constants are checked directly because a test that built a
    /// 32 MiB transcript to watch the root trip would cost more than the rule it
    /// proves; that the same function bounds the root is what
    /// `a_root_over_its_cap_is_marked_and_truncated_from_the_middle` shows at a
    /// size a test can afford. (The ordering between them is a compile-time
    /// assertion beside the constants.)
    #[test]
    fn the_shipped_caps_are_the_measured_child_and_a_much_higher_root() {
        assert_eq!(SESSION_AGENT_BYTES, 256 * 1024);
        assert_eq!(SESSION_ROOT_BYTES, 32 * 1024 * 1024);
    }

    /// A session with N children is bounded by N times the child cap: this is
    /// the test that would have caught the ~100 MB session — a few hundred
    /// children each carrying their whole transcript — and the reason a save's
    /// cost is bounded by what a conversation last said and not by how many
    /// agents it ever spawned.
    #[test]
    fn a_session_of_many_children_is_bounded_by_their_cap() {
        let children = 4;
        let mut session = saying("the root");
        for _ in 0..children {
            // Each child a good deal over the cap, so the bound is the cap's
            // doing and not the fixture's. Body-sized rounds rather than many
            // tiny ones: the pretty file spends a fixed few dozen bytes per
            // message on indentation, and a fixture of two-line answers would
            // measure that instead of the cap.
            session
                .agents
                .push(child((0..400).flat_map(|i| round(i, 2_000)).collect()));
        }
        session.bound_stored();

        let head = cost(&session.agents[0].messages[..1]);
        for agent in &session.agents {
            assert!(
                cost(&agent.messages) <= SESSION_AGENT_BYTES + head,
                "{} bytes",
                cost(&agent.messages)
            );
        }
        let json = serde_json::to_vec_pretty(&session).unwrap();
        // The pretty array spends indentation and newlines per message on top
        // of the compact cap — 1.03x on a real transcript, and a tenth on this
        // fixture — so the bound carries a tenth for the file's own shape. A
        // session storing the whole transcripts would be several times over it.
        let bound = children * (SESSION_AGENT_BYTES + head);
        assert!(
            json.len() < bound + bound / 10,
            "{} bytes for {children} capped children, bound {bound}",
            json.len()
        );
        assert!(
            json.len() > bound / 2,
            "the caps must be doing the bounding, not an empty fixture"
        );
    }
}
