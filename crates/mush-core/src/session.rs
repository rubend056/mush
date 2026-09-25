//! Conversation persistence.
//!
//! Everything mush writes lives in `<root>/.mush/`, which ignores itself via a
//! one-line `.gitignore`. Opening a folder is therefore the only setup step.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::message::Message;

pub const MUSH_DIR: &str = ".mush";
pub const SESSION_FILE: &str = "session.json";

/// The largest `.mush/session.json` mush will read whole, in bytes.
///
/// The store's own size is bounded by `CHILD_HISTORY × the fold trigger + the
/// root` (`docs/findings.md` §8.28) — a number that moves with the model's
/// window, not with a constant: ~10 MiB at 128k tokens, ~140 MiB at a million.
/// This is that bound's outer margin rather than the bound itself. Past it the
/// name is not a conversation any endpoint's window can plausibly produce, and
/// reading one would be a memcpy whose only ending is the OOM killer — the read
/// runs before the terminal is entered, so nothing on screen would say why.
///
/// It is a cap on the *read*, not a rule about what mush writes: a store past
/// it reads as [`Stored::Unusable`], which is the road that sets the file aside
/// as a `.bak` copy and starts a new conversation with the human told (finding
/// IN3).
pub const SESSION_READ_CAP: u64 = 256 * 1024 * 1024;

/// The workspace lock's own name, under `.mush/`. The lock itself — the flock
/// on this file and the rule that one mush holds it at a time — lives in the
/// `mush` crate's `lock` module; the *name* lives here, with the store's other
/// paths, because it is one of the store's own files (see [`STORE_FILES`]).
pub const LOCK_FILE: &str = "lock";

/// The store's own files under `.mush/`, and what a whole-file write to each
/// costs, in one list beside the paths above.
///
/// A name is listed by its *prefix*, so the session's family — the
/// `session.json.bak`, `.bak.2`, … copies `keep_unreadable` sets aside — is
/// covered by the session's own entry rather than by a list that has to grow
/// with the copies. The window is a file *directly* under `.mush/`; a
/// subdirectory is the human's (a paste lives in `.mush/paste/`), and a file
/// under one is not one of these names. `mush`'s write doors ask
/// [`store_file`] before replacing a name, so a store file added here is
/// covered by construction rather than by somebody remembering.
pub const STORE_FILES: &[(&str, &str)] = &[
    (
        LOCK_FILE,
        "the workspace lock, which one mush holds for as long as it runs — replacing it would \
         let a second mush lock the new file and write over this conversation",
    ),
    (
        SESSION_FILE,
        "this workspace's conversation — replacing it would lose the chat",
    ),
];

/// Why `name` — a file directly under `.mush/` — is one of the store's own,
/// or `None` when it is not.
///
/// The answer is the *name*, not the file's shape: every store file is a
/// regular file by construction, so a shape check (finding B3's socket/FIFO
/// refusal) cannot see them, and the harm of replacing one is not the loss of
/// some bytes but of what the name means — the lock that keeps two mushes off
/// one store (finding E2), or the conversation itself.
pub fn store_file(name: &str) -> Option<&'static str> {
    STORE_FILES.iter().find_map(|(file, why)| {
        let family = name.strip_prefix(file)?;
        (family.is_empty() || family.starts_with('.')).then_some(*why)
    })
}

/// A `.gitignore` that ignores everything, itself included.
const SELF_IGNORE: &str = "*\n";

pub fn mushroom_dir(root: &Path) -> PathBuf {
    root.join(MUSH_DIR)
}

pub fn session_path(root: &Path) -> PathBuf {
    mushroom_dir(root).join(SESSION_FILE)
}

/// Create `.mush/` and make it invisible to git.
///
/// The `.gitignore` is mush's, not the human's: one line, `*`, which ignores
/// everything in the directory, itself included. It is written on every call
/// rather than only when absent (finding C5): a hand edit, another tool, or a
/// repository that ships its own `.mush/.gitignore` used to survive here, and
/// the whole conversation was then one `git add -A` from the index. The file is
/// one line and idempotent, so enforcing it costs one small write per call —
/// and a sticky wrong one is a leak, which is the more expensive of the two.
///
/// Every road that creates the directory comes through here, not
/// `create_dir_all`: a `.mush/` recreated mid-run — the human's `rm -rf .mush`,
/// a `git clean -xfd` — is mush's directory again, and a write that recreated
/// it without the ignore line would leave the conversation one `git add -A`
/// from the index (finding R18). `Session::save` and [`keep_previous`] are the
/// two writes that can find it missing.
pub fn ensure_mush_dir(root: &Path) -> std::io::Result<()> {
    let dir = mushroom_dir(root);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(".gitignore"), SELF_IGNORE)?;
    Ok(())
}

/// The name a conversation is kept under when a new chat clears it:
/// `.mush/session.json.previous`, beside the store it replaces.
pub const PREVIOUS_FILE: &str = "session.json.previous";

/// Where that copy is: `<root>/.mush/session.json.previous`.
pub fn previous_session_path(root: &Path) -> PathBuf {
    mushroom_dir(root).join(PREVIOUS_FILE)
}

/// Keep `session` beside the store, under the name a new chat's warning points
/// at, and answer where it landed.
///
/// One slot, not a numbered family like [`keep_unreadable`]'s: the promise is
/// that the conversation just cleared can be reclaimed, and the newest cleared
/// conversation is the one the human is looking for. A copy family would be an
/// archive of conversations, which the chat layer refuses to keep in so many
/// words (`Chat::forget`'s "No archive" rule).
///
/// Synchronous, and the same bytes [`Session::save`] writes — images shed, the
/// same serializer, the same atomic rename — because the caller clears the
/// store the moment this returns: a copy that is late or half-written is not
/// the copy the key promised. A failure is the caller's to refuse the clear
/// with, told by `cannot_keep`'s one sentence, so a workspace that cannot
/// take the copy keeps the conversation instead of losing it.
///
/// Private, like the store it sits beside ([`crate::workspace::Fresh::Private`]):
/// the copy holds the same conversation, so it gets the same `0600` — a new
/// chat must not be the moment the human's umask hands it to the group.
pub fn keep_previous(root: &Path, mut session: Session) -> Result<PathBuf, String> {
    let to = previous_session_path(root);
    session.shed_images();
    let write = (|| -> std::io::Result<()> {
        // Through `ensure_mush_dir`, not `create_dir_all`: the directory this
        // recreates has to come back with its ignore line, or the copy just
        // kept is untracked work the next `git add -A` stages (finding R18).
        ensure_mush_dir(root)?;
        let json = serde_json::to_vec_pretty(&session).map_err(std::io::Error::other)?;
        crate::workspace::atomic_write(&to, &json, crate::workspace::Fresh::Private)
    })();
    write.map_err(|error| cannot_keep(&to, error))?;
    Ok(to)
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

/// Where an agent's isolated work went, once mush's sweep settled it.
///
/// `NothingCommitted` is the third ending the sweep can prove: the branch never
/// gained a commit of its own, so there was nothing to land. It is stored for
/// the same reason the other two are — a restart paints `landed` on the row —
/// and without it a no-commit run would come back reading "merged" (the
/// disclosure in §8.23 of `docs/findings.md` that asks for this variant here).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredLanded {
    /// The branch's commits are in the base it was forked from.
    Merged,
    /// The branch never gained a commit of its own.
    NothingCommitted,
    /// Thrown away on purpose.
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
    /// The child's transcript, stored the way the root's is: each message in
    /// its wire form plus the flags a message's provenance needs
    /// ([`crate::message::serialize_stored_messages`]), so a note read back
    /// is read as the note rather than as the human's own line, and a line
    /// mush wrote is read as mush's.
    #[serde(default, serialize_with = "crate::message::serialize_stored_messages")]
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
    pub model: String,
    /// Provider name as given by [`Provider::name()`](crate::provider::Provider::name),
    /// i.e. a name `--provider` accepts (see [`provider::PROVIDERS`](crate::provider::PROVIDERS)).
    #[serde(default)]
    pub provider: String,
    /// Base URL of the endpoint in use; empty when never customized.
    #[serde(default)]
    pub base_url: String,
    /// A context window the human stated for this workspace (`/context`, or a
    /// `MUSH_CONTEXT` at the time). Derived windows are never stored: they are
    /// re-read from the endpoint, so a stale guess cannot outlive its cause.
    /// `/context auto` is the road back from a stored statement: the window
    /// re-derives from the model table, the next save writes this field away,
    /// and a restart reads no statement at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<usize>,
    /// The root conversation, stored in the shape that keeps each message's
    /// own facts: the wire form plus the flags a message's provenance needs
    /// ([`crate::message::serialize_stored_messages`]), because a transcript
    /// read back from here has to know which line is the note, and which lines
    /// mush itself wrote, instead of reading them as the human's.
    #[serde(serialize_with = "crate::message::serialize_stored_messages")]
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
    /// The file is there and cannot be used. The string is *why* — the shape
    /// the name had (not a regular file), the size it had (past
    /// [`SESSION_READ_CAP`]), what serde objected to, or the IO error — for the
    /// human who has to decide what to do with the copy that [`keep_unreadable`]
    /// sets aside.
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

/// What a name that is not a regular file is, for the refusal a human reads:
/// a link, a directory, a fifo, a socket, a device, or an answer that says so
/// without inventing a kind.
fn not_a_file(meta: &fs::Metadata) -> &'static str {
    use std::os::unix::fs::FileTypeExt;
    let kind = meta.file_type();
    if kind.is_symlink() {
        "a symlink"
    } else if kind.is_dir() {
        "a directory"
    } else if kind.is_fifo() {
        "a fifo"
    } else if kind.is_socket() {
        "a socket"
    } else if kind.is_block_device() || kind.is_char_device() {
        "a device"
    } else {
        "not a regular file"
    }
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
/// leave the workspace. The name is [`crate::workspace::backup_name`]'s — the
/// next free `.bak` name, never a copy already there, because a copy beside the
/// file is one the human already needed — so the conversation and the home
/// config keep their copies by one numbering rule and one bound. Returns where
/// it went; a failure names the file and why it could not be moved
/// (`cannot_keep`).
///
/// This is deliberately not called for an [`Stored::Absent`] workspace: there
/// is nothing to keep, and creating a backup of nothing would be a file a human
/// has to wonder about.
pub fn keep_unreadable(root: &Path) -> Result<PathBuf, String> {
    let from = session_path(root);
    let to = crate::workspace::backup_name(&from).map_err(|why| cannot_keep(&from, why))?;
    fs::rename(&from, &to)
        .map(|()| to)
        .map_err(|error| cannot_keep(&from, error))
}

impl Session {
    /// Read what the workspace stores, distinguishing *absent* from
    /// *unreadable* (see [`Stored`]). A caller that only wants the conversation,
    /// and has nothing to say about which of the two it read, wants
    /// [`Self::load`].
    ///
    /// The file is untrusted input, so the shape is decided before anything is
    /// opened and the read is bounded (finding IN3):
    ///
    /// - the *shape* comes from `symlink_metadata` — the name itself, so a link
    ///   is never followed to whatever it points at. A FIFO named as the store
    ///   parks `open` until a writer appears, and this read runs before the
    ///   terminal is entered: the whole start would wait with no frame and the
    ///   workspace lock held. A symlink to `/dev/zero` — a shape git can commit
    ///   — would read without bound;
    /// - the *cap* comes from that same stat, before a byte is read: a blob is
    ///   refused from its size instead of being loaded to find it out;
    /// - the read is still bounded to [`SESSION_READ_CAP`] + 1 bytes, because a
    ///   file can grow between the stat and the read — the same bound the
    ///   workspace's own whole reads keep.
    ///
    /// Every refusal is [`Stored::Unusable`], which is the road that sets an
    /// unusable store aside as a `.bak` copy rather than destroying it.
    pub fn read(root: &Path) -> Stored {
        let path = session_path(root);
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            // No file is not a file mush cannot use: a workspace nobody has
            // opened yet must stay silent.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Stored::Absent,
            Err(error) => return Stored::Unusable(format!("cannot read the file — {error}")),
        };
        if !meta.is_file() {
            return Stored::Unusable(format!(
                "it is {} — the store is read only when the name is a regular file",
                not_a_file(&meta)
            ));
        }
        if meta.len() > SESSION_READ_CAP {
            return Stored::Unusable(format!(
                "it is {} bytes — past the {SESSION_READ_CAP} bytes mush will read whole",
                meta.len()
            ));
        }
        let mut bytes = Vec::new();
        let read = fs::File::open(&path)
            .and_then(|file| file.take(SESSION_READ_CAP + 1).read_to_end(&mut bytes));
        if let Err(error) = read {
            return Stored::Unusable(format!("cannot read the file — {error}"));
        }
        if bytes.len() as u64 > SESSION_READ_CAP {
            return Stored::Unusable(format!(
                "it grew past the {SESSION_READ_CAP} bytes mush will read whole while it was \
                 being read"
            ));
        }
        match serde_json::from_slice(&bytes) {
            Ok(session) => Stored::Loaded(session),
            // The position serde names is what makes a hand edit findable, so
            // it is carried through rather than flattened into "bad json".
            Err(error) => Stored::Unusable(error.to_string()),
        }
    }

    /// The conversation, or `None` when there is none *or* when the one there
    /// is cannot be used. It is the shorthand for a caller that only wants the
    /// conversation and has nothing to say about which of the two it read; and
    /// it is why [`Self::read`] exists: the two cases must not be
    /// indistinguishable to the caller that overwrites the file.
    pub fn load(root: &Path) -> Option<Self> {
        match Self::read(root) {
            Stored::Loaded(session) => Some(session),
            Stored::Absent | Stored::Unusable(_) => None,
        }
    }

    /// Write the conversation to `<root>/.mush/session.json`.
    ///
    /// Borrows the value rather than consuming it: the caller is the writer
    /// thread's snapshot, and a write that fails has to be retryable — the
    /// conversation must still be there for the next attempt instead of being
    /// freed with the failed one (finding R2). The borrow is mutable because
    /// shedding image payloads is part of the save; the replacement is
    /// idempotent, so a retry writes the same bytes the first attempt would
    /// have. Same path, same fields, same format, still read by [`Self::load`].
    ///
    /// Image bytes are not written. Every message carrying one has its payload
    /// replaced, before serialization, by the placeholder that names its path
    /// ([`Message::drop_images`]) — on the value this call owns, never on the
    /// live conversation the snapshot was cloned from. A screenshot is
    /// megabytes of base64 that no human wants to find in `.mush/session.json`,
    /// and what a resumed agent needs is the path: with it the model can read
    /// the file again. [`Self::load`] therefore returns the placeholder and no
    /// images, and because the drop is idempotent a loaded session saved again
    /// cannot stack a second placeholder on the first one's text.
    pub fn save(&mut self, root: &Path) -> std::io::Result<()> {
        self.shed_images();
        let path = session_path(root);
        // The directory comes back through `ensure_mush_dir`, the same door
        // the start takes: a store written after a `git clean -xfd` (or
        // `rm -rf .mush`) must recreate *mush's* directory, ignore line and
        // all, not a plain one the next `git add -A` stages (finding R18).
        ensure_mush_dir(root)?;
        // A serialization failure is an error like any other: the writer's
        // error channel two files away is where the human hears about it, and a
        // session replaced by `{}` would be a save reporting success.
        // A new store is mush's own file ([`Fresh::Private`]): the write must
        // not make the conversation group- and world-readable.
        let json = serde_json::to_vec_pretty(&self).map_err(std::io::Error::other)?;
        crate::workspace::atomic_write(&path, &json, crate::workspace::Fresh::Private)
    }

    /// Replace every image payload in this conversation — the root transcript's
    /// and every subagent's — with the placeholder that names it. Called only
    /// on a value about to be written and never on the live conversation:
    /// [`Self::save`] calls it on the value it owns, [`keep_previous`] on the
    /// copy kept beside the store; [`Self::save`]'s doc says why the bytes
    /// never reach the file.
    fn shed_images(&mut self) {
        for message in &mut self.messages {
            message.drop_images();
        }
        for agent in &mut self.agents {
            for message in &mut agent.messages {
                message.drop_images();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Image;
    use crate::scratch::Scratch;

    /// A FIFO at the store's name is refused from the name's own shape, never
    /// opened: `open` on a FIFO with no writer *parks* until one appears, and
    /// this read runs before the terminal is entered — so the whole start would
    /// wait with no frame and the workspace lock held (finding IN3). The read
    /// answers on its own, which is the assertion: a read that followed the
    /// name would never send.
    #[test]
    fn a_fifo_named_as_the_store_is_refused_without_opening_it() {
        let root = Scratch::new("session-fifo");
        ensure_mush_dir(&root).unwrap();
        let fifo = std::process::Command::new("mkfifo")
            .arg(session_path(&root))
            .status()
            .expect("mkfifo runs");
        assert!(fifo.success(), "the fixture needs a fifo");

        let dir = root.path().to_path_buf();
        let (done, waited) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(Session::read(&dir));
        });
        // A watchdog, not a bound (shape 2 of the rule in `outline.rs`'s
        // `a_million_declarations_are_counted_and_not_kept`): the refusal is a
        // metadata read, and the thread spawn and channel send around it
        // answered in 0.16 ms on this box at its own load of ~24 and 0.69 ms
        // under a peak of twelve busy loops — so ten seconds is tens of
        // thousands of times the real cost, and only a read that *parked* on
        // the FIFO can reach it. A parked read never answers at any multiple:
        // the timeout is here so a hang fails the suite instead of hanging it.
        let started = std::time::Instant::now();
        let read = waited
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("the read parked on the fifo — it opened the name"));
        eprintln!("session fifo refusal: {:?}", started.elapsed());
        match read {
            Stored::Unusable(reason) => assert!(
                reason.contains("fifo") || reason.contains("regular file"),
                "the refusal names the shape: {reason}"
            ),
            other => panic!("a fifo at the store's name must be refused, got {other:?}"),
        }

        // The road that follows a refusal is intact for this shape too: the
        // name is moved aside (`rename` never opens it) and a fresh
        // conversation can be written over the path.
        let kept = keep_unreadable(&root).unwrap();
        assert!(
            !session_path(&root).exists(),
            "the fifo is moved aside, not read"
        );
        saying("a fresh conversation").save(&root).unwrap();
        assert!(matches!(Session::read(&root), Stored::Loaded(_)));
        assert!(
            std::os::unix::fs::FileTypeExt::is_fifo(
                &fs::symlink_metadata(&kept).unwrap().file_type()
            ),
            "the copy set aside is the fifo itself, untouched"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A symlink at the store's name is refused, not followed: a repository can
    /// commit `.mush/session.json` as a link, and a link to `/dev/zero` reads
    /// without bound — the shape check is the *name* (`symlink_metadata`), so
    /// however readable the target is, mush does not read through it (finding
    /// IN3).
    #[test]
    fn a_symlinked_store_is_refused_not_followed() {
        let root = Scratch::new("session-symlink");
        ensure_mush_dir(&root).unwrap();
        let target = root.join("elsewhere.json");
        fs::write(&target, r#"{"model":"a-model","messages":[]}"#).unwrap();
        std::os::unix::fs::symlink(&target, session_path(&root)).unwrap();

        match Session::read(&root) {
            Stored::Unusable(reason) => assert!(
                reason.contains("symlink") || reason.contains("regular file"),
                "the refusal names the shape: {reason}"
            ),
            other => panic!("a symlink must not be followed, got {other:?}"),
        }
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            r#"{"model":"a-model","messages":[]}"#,
            "the file the link points at is untouched"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A regular file past [`SESSION_READ_CAP`] is refused from its size,
    /// before a byte is read: the size is the fact the decision needs, and a
    /// blob loaded to find it out is the OOM the cap exists to prevent
    /// (finding IN3).
    #[test]
    fn a_store_past_the_cap_is_refused_from_its_size() {
        let root = Scratch::new("session-cap");
        ensure_mush_dir(&root).unwrap();
        // Sparse, so the fixture costs no disk: it is the *size* the read
        // decides from, and the bytes are never read.
        let file = fs::File::create(session_path(&root)).unwrap();
        file.set_len(SESSION_READ_CAP + 1).unwrap();

        match Session::read(&root) {
            Stored::Unusable(reason) => assert!(
                reason.contains(&SESSION_READ_CAP.to_string()),
                "the refusal is the cap's, not serde's: {reason}"
            ),
            other => panic!("a store past the cap must be refused, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// An `absent` session and one mush *cannot read* are two different facts,
    /// and telling them apart is what stops the second from being silently
    /// overwritten: the conversation the human left here — plus one agent, in
    /// the finding — has to survive the first save of the fresh one.
    #[test]
    fn an_unreadable_session_is_told_apart_from_an_absent_one() {
        let root = Scratch::new("session4");
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

    /// A `.mush/` recreated mid-run — `rm -rf .mush`, a `git clean -xfd` — is
    /// mush's directory again, so the store's writes have to bring the ignore
    /// line back with it: the same `ensure_mush_dir` the start takes is the one
    /// the write roads take, so a conversation written after the directory was
    /// recreated is not one `git add -A` from the index (finding R18).
    #[test]
    fn a_store_write_recreates_the_mush_dir_with_its_ignore_line() {
        let root = Scratch::new("session-recreated");

        // The save road: no `.mush/` exists at all, and the write is what
        // creates it.
        saying("hello").save(&root).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(MUSH_DIR).join(".gitignore")).unwrap(),
            SELF_IGNORE,
            "a save recreated .mush/ without its ignore line"
        );

        // The keep road: the directory is taken away mid-run, with the
        // conversation still live in the window.
        fs::remove_dir_all(root.join(MUSH_DIR)).unwrap();
        keep_previous(&root, saying("the cleared conversation")).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(MUSH_DIR).join(".gitignore")).unwrap(),
            SELF_IGNORE,
            "the kept copy recreated .mush/ without its ignore line"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Breaking the file twice must not lose the first copy: the backup name is
    /// searched, not reused.
    #[test]
    fn keeping_a_second_unreadable_session_does_not_overwrite_the_first() {
        let root = Scratch::new("session5");
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
        let root = Scratch::new("session6");
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
        // Every name the shared rule will try, so the bound is the one in
        // [`crate::workspace::backup_name`] rather than a number repeated here.
        for step in 1..=crate::workspace::BACKUP_TRIES {
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
            model: "test".into(),
            provider: "custom".into(),
            base_url: String::new(),
            context: None,
            messages: vec![Message::user(text)],
            agents: Vec::new(),
            notices: Vec::new(),
        }
    }

    #[test]
    fn ensure_creates_self_ignoring_dir() {
        let root = Scratch::new("session");
        ensure_mush_dir(&root).unwrap();
        let ignore = mushroom_dir(&root).join(".gitignore");
        assert_eq!(fs::read_to_string(ignore).unwrap(), "*\n");
        let _ = fs::remove_dir_all(&root);
    }

    /// One git command in `dir`, stdout returned and a failure loud: the test
    /// below reads a repository the way `git add -A` would.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The store's self-ignore is mush's line, and it is enforced rather than
    /// suggested: `ensure_mush_dir` used to write it only when the file was
    /// absent, so a hand edit, another tool, or a repository shipping its own
    /// `.mush/.gitignore` left the whole conversation in front of `git add -A`
    /// (finding C5).
    ///
    /// The property, not only the bytes: a real repository, one session write,
    /// and `git status --porcelain` empty.
    #[test]
    fn mushs_own_ignore_line_is_enforced_and_git_stays_clean() {
        let root = Scratch::new("session-ignore");
        let ignore = mushroom_dir(&root).join(".gitignore");
        fs::create_dir_all(mushroom_dir(&root)).unwrap();

        // A file that ignores nothing, and one an earlier tool left empty: both
        // end up mush's one line.
        for said in ["!*\n", ""] {
            fs::write(&ignore, said).unwrap();
            ensure_mush_dir(&root).unwrap();
            assert_eq!(
                fs::read_to_string(&ignore).unwrap(),
                SELF_IGNORE,
                "the file mush owns holds mush's line, not {said:?}"
            );
        }

        // The property the line is for: `git add -A` sees no conversation.
        git(&root, &["-c", "init.defaultBranch=master", "init", "-q"]);
        saying("a conversation git must not see")
            .save(&root)
            .unwrap();
        let status = git(&root, &["status", "--porcelain"]);
        assert_eq!(status, "", "the store is invisible to git: {status:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn session_roundtrips() {
        let root = Scratch::new("session2");
        ensure_mush_dir(&root).unwrap();

        let mut session = Session {
            model: "test".into(),
            provider: "custom".into(),
            base_url: "http://localhost:9".into(),
            context: Some(123_456),
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

    /// A png whose IHDR names `width × height`, with `padding` bytes behind it:
    /// the size the budget reads, whatever the file happens to weigh.
    fn png_of(width: u32, height: u32, padding: usize) -> Vec<u8> {
        let mut bytes = vec![
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
        ];
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        bytes.extend([8, 6, 0, 0, 0]);
        bytes.resize(33 + padding, 0);
        bytes
    }

    /// A session file is a conversation, not an image store: before anything is
    /// written, every message's payload — the root transcript's and every
    /// subagent's — becomes the placeholder that names its path. Loading yields
    /// the placeholder and no images, and saving what was loaded again must not
    /// stack a second placeholder (the bug this shape invites: the file is
    /// written on every flush, so a placeholder added unconditionally would
    /// grow a new copy each save).
    #[test]
    fn a_session_keeps_the_path_and_not_the_bytes() {
        let root = Scratch::new("session-image");
        ensure_mush_dir(&root).unwrap();

        let mut session = saying("look at this");
        session.messages[0].images.push(Image::new(
            "shots/a.png",
            "image/png",
            vec![0x41; 4_096],
            Some((1_920, 1_080)),
        ));
        session.agents.push(AgentSession {
            id: 1,
            messages: vec![Message {
                images: vec![Image::new(
                    "shots/b.jpg",
                    "image/jpeg",
                    vec![0x42; 4_096],
                    Some((800, 600)),
                )],
                ..Message::user("and this one")
            }],
            ..Default::default()
        });

        session.save(&root).unwrap();

        // The payload is absent in every spelling: a 4 KB run of 0x41 is
        // `QUFB…` in base64, and the field itself has no name on the wire.
        let raw = fs::read_to_string(session_path(&root)).unwrap();
        assert!(
            !raw.contains("QUFBQUFB"),
            "the image bytes are not in the file: {raw}"
        );
        assert!(
            !raw.contains("\"images\""),
            "the bytes have no field of their own in the file: {raw}"
        );
        assert!(
            raw.len() < 2_000,
            "the placeholder is text, not payload: {} bytes",
            raw.len()
        );

        let mut loaded = Session::load(&root).unwrap();
        assert!(
            loaded.messages[0].images.is_empty(),
            "loading brings no bytes back"
        );
        let text = loaded.messages[0].text().to_string();
        assert!(text.starts_with("look at this"), "{text}");
        assert!(
            text.contains("shots/a.png"),
            "the placeholder names the path: {text}"
        );
        assert_eq!(
            text.matches("[image:").count(),
            1,
            "one image, one placeholder: {text}"
        );
        assert!(
            loaded.agents[0].messages[0].images.is_empty(),
            "a subagent's transcript is shed too"
        );
        let child_text = loaded.agents[0].messages[0].text().to_string();
        assert!(
            child_text.contains("shots/b.jpg"),
            "and its placeholder names its path: {child_text}"
        );

        // The road back the placeholder names: the model reads the file again,
        // and the size comes back with it, so the resumed run prices the
        // picture the way the run that attached it did.
        fs::create_dir_all(root.join("shots")).unwrap();
        fs::write(root.join("shots/a.png"), png_of(1_920, 1_080, 120_000)).unwrap();
        let ws = crate::workspace::Workspace::new(&root).unwrap();
        let reread = ws.read_image("shots/a.png").unwrap().unwrap();
        assert_eq!(reread.pixels, Some((1_920, 1_080)));
        assert!(
            reread.weight() < 20_000,
            "the re-read is priced by its pixels, not its {}-byte file: {}",
            reread.bytes.len(),
            reread.weight()
        );

        loaded.save(&root).unwrap();
        let again = Session::load(&root).unwrap();
        assert_eq!(
            again.messages[0].text(),
            text,
            "the second save does not append a second placeholder"
        );
        assert_eq!(
            again.agents[0].messages[0].text(),
            child_text,
            "nor a second one for the subagent"
        );
        assert_eq!(again.messages[0].text().matches("[image:").count(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    /// The third landing has a name in the file, and it is the one the row and
    /// the sweep read back: a no-commit run restored from a session must come
    /// back as itself, not as `merged` (the row that claimed a merge nobody
    /// performed).
    #[test]
    fn a_nothing_committed_landing_round_trips_by_name() {
        let spelled = serde_json::to_string(&StoredLanded::NothingCommitted).unwrap();
        assert_eq!(spelled, "\"nothing_committed\"");
        assert_eq!(
            serde_json::from_str::<StoredLanded>(&spelled).unwrap(),
            StoredLanded::NothingCommitted
        );
    }

    /// The dropped-turns note's provenance is a fact the file has to keep: the
    /// flag is what tells the note from a human's line that is word for word
    /// the same sentence (finding F3), and a restart has no road back to it
    /// other than the file — prose is exactly what must not be the test. The
    /// flag is written on the note alone, so a human's identical line, and a
    /// file written before the field existed, both read as what they are.
    #[test]
    fn a_session_round_trip_keeps_the_notes_provenance() {
        use crate::transcript::{is_dropped_note, DROPPED_TURNS_NOTE};

        let session = Session {
            messages: vec![
                Message::user("the opening task"),
                Message::user(DROPPED_TURNS_NOTE),
                Message::assistant("working"),
                Message::note(DROPPED_TURNS_NOTE),
            ],
            agents: vec![AgentSession {
                id: 1,
                messages: vec![
                    Message::user("the brief"),
                    Message::note(DROPPED_TURNS_NOTE),
                ],
                ..AgentSession::default()
            }],
            ..Session::default()
        };

        let spelled = serde_json::to_string(&session).unwrap();
        assert_eq!(
            spelled.matches("\"note\":true").count(),
            2,
            "one flag on the one note in each transcript: {spelled}"
        );
        let restored: Session = serde_json::from_str(&spelled).unwrap();
        assert!(
            !is_dropped_note(&restored.messages[1]),
            "the human's own line is still the human's"
        );
        assert!(
            is_dropped_note(&restored.messages[3]),
            "and the note is still the note"
        );
        assert!(
            !is_dropped_note(&restored.agents[0].messages[0]),
            "a child's brief that quotes the sentence is not the note"
        );
        assert!(
            is_dropped_note(&restored.agents[0].messages[1]),
            "and its note survives the same road"
        );

        // A file that predates the field names no flag, and its lines read as
        // they did before it existed.
        let before_the_field = r#"{"model":"m","provider":"custom","base_url":"",
          "messages":[{"role":"user","content":"the opening task"}]}"#;
        let loaded: Session = serde_json::from_str(before_the_field).unwrap();
        assert!(!is_dropped_note(&loaded.messages[0]));
    }

    /// Old sessions have no context field; they must still load. So must the
    /// `root` and `updated` keys every file written before those fields were
    /// dropped still carries — an unknown key is not a broken session.
    #[test]
    fn a_session_without_a_context_loads() {
        let root = Scratch::new("session3");
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
