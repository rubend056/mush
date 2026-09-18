//! Application state and the update function.
//!
//! The UI thread owns no file state: agents read and write the workspace
//! themselves, and this module is the human's view of them — the agent tree,
//! the focused transcript, the git facts, and the message box. Every input — a
//! keystroke, an agent event — becomes a `Msg` and flows through `App::update`.
//!
//! The agents themselves live in [`tree`], which owns their ids, phases and
//! focus; the conversation lives in [`chat`], which owns every transcript the
//! screen shows, the notices, the message box and the context meter; and the
//! endpoint, model, key and context window live in [`settings`], whose one cell
//! the agents read too. This module routes messages into all of them and
//! renders what they say.

mod chat;
pub mod commands;
pub mod keys;
mod screen;
mod settings;
mod tree;

pub use chat::{Chat, Pane, Rank};
pub use screen::{AgentRow, AgentsPane, BarPane, ChatPane, PickerPane, Screen};
pub use settings::{ConfigCell, ConfigHandle, WindowSource};
pub use tree::{
    AgentId, AgentNode, AgentTree, Compacting, ConversationId, Existing, Landed, Phase, Spawn,
};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use ratatui::crossterm::event::KeyEvent;

use mush_core::message::Message;
use mush_core::{
    git, prompt, session, text::mask_key, userconfig, Config, Provider, Session, UserConfig,
    Workspace,
};

use crate::agent::{self, spawn, AgentEvent, AgentMsg, RootHandle};
use crate::attach;
use crate::http;
use crate::session_save::SessionSave;

use commands::{Command, CommandError, Verb};
use keys::Intent;

pub enum Msg {
    Key(KeyEvent),
    /// Pasted text, delivered whole by the terminal's bracketed paste. Inserted
    /// in one update: a paste must not cost one message per character.
    Paste(String),
    /// A model list that finished fetching on its own thread. `endpoint` is the
    /// endpoint it was fetched from: a fetch outlives the human who typed
    /// `/url`, and a list from the endpoint they left must not land
    /// (finding A9).
    Models {
        endpoint: String,
        models: Vec<http::Model>,
    },
    /// A repository read that finished on its own thread.
    Git {
        stats: HashMap<AgentId, git::Stat>,
        status: Option<git::RepoStatus>,
    },
    /// An event from an agent actor. `conversation` identifies the tree that
    /// sent it, so an actor left over from `/new` cannot write into the new
    /// chat: events are tagged and the UI drops the stale ones.
    Agent {
        conversation: ConversationId,
        id: AgentId,
        event: AgentEvent,
    },
    /// A request from the attach socket (M3). The socket thread owns the
    /// connection and blocks on `reply` for the answer, so the socket never
    /// touches `App`'s state: it is parsed off the connection and answered in
    /// the one message loop, which is what keeps `App` the only effector.
    Attach {
        from: String,
        request: attach::Request,
        reply: Sender<attach::Response>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Agents,
    Chat,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Model,
    Provider,
    /// The lines mush wrote about the focused agent, for `/notes`. Nothing here
    /// is a choice, so `Enter` and `Esc` do the same thing.
    Notes,
}

/// A small modal list that grabs the keyboard until Enter or Esc: the models,
/// the providers, and the notes mush wrote about the focused agent. Drawn as a
/// centered popup by `ui::draw_picker`.
pub struct Picker {
    pub kind: PickerKind,
    pub items: Vec<String>,
    pub cursor: usize,
}

impl Picker {
    pub fn title(&self) -> String {
        match self.kind {
            PickerKind::Model => " models · Enter picks ".to_string(),
            PickerKind::Provider => " provider · Enter picks ".to_string(),
            PickerKind::Notes => format!(
                // The position is part of the title because the list opens in
                // the middle of a long note (see `open_notes_picker`): without
                // it, a popup whose head is above the fold looks like the whole
                // note, and nothing says a keypress reads the rest.
                " notes · newest last · line {}/{} ",
                self.cursor + 1,
                self.items.len()
            ),
        }
    }

    /// What the popup's own last row says the keys do. It belongs to the picker
    /// rather than to the painter because it is the same fact as the title: what
    /// this list is for. A long list is paged the same way the transcript is,
    /// so `PgUp`/`PgDn` are named beside `j`/`k`.
    pub fn hint(&self) -> &'static str {
        match self.kind {
            PickerKind::Model | PickerKind::Provider => {
                " j/k or PgUp/PgDn · Enter pick · Esc cancel "
            }
            PickerKind::Notes => " j/k or PgUp/PgDn scrolls · Esc closes ",
        }
    }
}

/// `500k`, `8192`, `1.2M` — the way a token count wants to be read. A run's
/// real numbers come from the endpoint and are large (`1213866`), and a number
/// a human has to count digits in says nothing at a glance.
pub fn tokens_label(tokens: usize) -> String {
    /// One decimal of the unit, and no `.0`: `500k` stays `500k`, and 1,213,866
    /// reads as `1.2M` rather than as `1M` (the first is the size, the second is
    /// a rounding that hides it).
    fn scaled(tokens: usize, divisor: f64, unit: &str) -> String {
        let text = format!("{:.1}", tokens as f64 / divisor);
        format!("{}{unit}", text.strip_suffix(".0").unwrap_or(&text))
    }
    // 999,950 is the last count that rounds to `1000k`; above it the million
    // unit is the honest one.
    if tokens >= 999_950 {
        scaled(tokens, 1_000_000.0, "M")
    } else if tokens >= 1_000 {
        scaled(tokens, 1_000.0, "k")
    } else {
        tokens.to_string()
    }
}

/// Whether a stored transcript ends where a finished run left it: the last
/// message is an assistant turn that asked for no tools, which is an answer and
/// nothing else. A run that failed before it changed the transcript — the
/// endpoint refusing the first request of a resumed agent — leaves one behind,
/// so this is what tells a restored agent's own work apart from a later
/// refusal (see `restore_agents`).
fn ended_on_an_answer(messages: &[Message]) -> bool {
    matches!(
        messages.last(),
        Some(message) if message.role == "assistant" && message.tool_calls().is_empty()
    )
}

/// `/help`: the key table, then the command table.
///
/// Both tables are the same one source their CLI counterparts print —
/// [`keys::help_table`] is `mush --help`'s KEYS block and [`commands::table`]
/// is its COMMANDS block — so no surface can advertise a binding or a command
/// the others do not. `/help` used to name six keys by hand and miss `j`/`k`,
/// `Enter`, `c`, `Esc` and `Ctrl-Q`; rendering [`keys::KEYS`] here is what stops
/// a human learning the keyboard from a subset of it (the key half of finding
/// B2). It is a notice rather than a status line: it is a thing to read, not a
/// thing that just happened.
fn help_notice() -> String {
    format!(
        "mush keys:\n{}\nCommands:\n{}",
        keys::help_table(),
        commands::table(&mush_core::provider::names_piped())
    )
}

/// An age the way a glance wants it: seconds, then minutes, then hours — never
/// a five-digit number that takes arithmetic to read.
pub fn short_age(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

/// Below this the screen has no room to be honest: the painter shows a single
/// notice instead of shreds, and [`App::below_floor`] stops keys that would act
/// without anything visible to show for it. One pair of numbers, owned here
/// rather than by the painter, so the notice, the input gate and the tests
/// cannot disagree about where the floor is (finding P11 / refactor B3).
pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 10;

/// Whether a terminal of this size is below the floor. The predicate is one
/// function so the painter — which passes the frame's own size — and the input
/// gate — which passes the size `main` reported — cannot disagree.
pub const fn is_below_floor(width: u16, height: u16) -> bool {
    width < MIN_WIDTH || height < MIN_HEIGHT
}

/// A transient line for the workspace bar. Nothing here describes work in
/// progress — that is derived from the agents' phases — so it cannot go stale.
/// `Info` fades; `Error` stays until something replaces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Error,
}

/// How long an `Info` line is worth showing. Long enough to read after a
/// command, short enough that it never becomes furniture.
const INFO_TTL: Duration = Duration::from_secs(5);

/// How long the session file may lag the conversation.
///
/// The human reads the transcript, not the file, and the file is only read
/// again by the *next* mush — so a second of staleness is invisible, while a
/// write per streamed message is a lag on every tool result. A second is also
/// what a crash costs: at most 1000 ms of streamed chat, and never the human's
/// own message, a command that changed what is stored, or a compaction, all of
/// which are written before they return (see `App::flush_session` and the marks
/// in `on_agent`).
const SESSION_DEBOUNCE: Duration = Duration::from_secs(1);

/// How wide a job's handle may be: the same bound as an agent's title, for the
/// same reason — a handle, whose full text is the report in the transcript.
const JOB_TITLE_COLUMNS: usize = 30;

/// `short_age`'s companion for a job: the command's own work, on one line.
///
/// A `command` the model wrote can be thirty lines of heredoc with a
/// `cd /w &&` in front of it, and the bar and the row's footer each have one
/// row to name it in: what is left is the last clause of the first line
/// (`cargo build` out of `cd /w && cargo build --release`), collapsed and
/// bounded like an agent's title. One derivation, so the two surfaces cannot
/// spell the same job differently.
fn job_title(command: &str) -> String {
    let first = command
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let clause = first.rsplit("&&").next().unwrap_or(&first).trim();
    mush_core::text::truncate(clause, JOB_TITLE_COLUMNS)
}

#[derive(Clone, Debug)]
pub struct Status {
    pub kind: StatusKind,
    pub text: String,
    pub set_at: Instant,
}

pub struct App {
    pub ws: Workspace,
    /// The endpoint, the model, the key and the context window: the copy the
    /// screen reads and the cell every actor reads, in one owner.
    pub cell: ConfigCell,
    pub focus: Focus,
    /// The conversation: the transcripts the screen shows, the notices, the
    /// message box and the context meter, in one value.
    pub chat: Chat,
    pub models: Vec<http::Model>,
    pub picker: Option<Picker>,
    /// The main worktree's branch, dirty count, and uncommitted line delta.
    pub git: Option<git::RepoStatus>,
    /// The agents, their phases, the focus and the per-agent mailboxes,
    /// cancel flags and git stats.
    pub tree: AgentTree,
    /// Where a snapshot of this conversation goes. The serialization and the
    /// write happen on the writer's own thread; this thread only hands the
    /// state over.
    session_save: Arc<dyn SessionSave>,
    /// When the conversation last changed with no snapshot handed over since.
    /// `None` means the file is current. `tick` is where it is turned into a
    /// write, so a burst of messages costs one rebuild instead of one each.
    session_dirty_at: Option<Instant>,
    /// The UI event channel, needed to respawn the root actor on /new.
    ui_tx: Sender<Msg>,
    /// When the git snapshot was last taken, so a long run refreshes it.
    git_at: Option<Instant>,
    /// A `git` read is already running on its own thread; asking again would
    /// only queue another one behind it.
    git_in_flight: bool,
    /// A transient line for the bar: what just happened, or what went wrong.
    /// Work in progress does not live here — it is derived from the phases.
    pub status: Option<Status>,
    pub should_quit: bool,
    pub dirty_screen: bool,
    /// The terminal's size, as of the last size `main` reported. `/notes` wraps
    /// its lines to the popup this size paints them in, and [`App::below_floor`]
    /// reads it to refuse input the screen cannot show the effect of — so the
    /// floor is a state `App` knows rather than only the painter's early return
    /// (finding P11 / refactor B3).
    term_width: u16,
    term_height: u16,
    pub spin: u64,
}

impl App {
    /// The configuration every screen reads: the UI's copy of the cell.
    pub fn cfg(&self) -> &Config {
        self.cell.ui()
    }

    /// Record the terminal size `main` read, so a `/notes` report can be
    /// wrapped to the popup that size paints and so the floor is known. One
    /// setter, called at startup and from the resize event — the only two
    /// places the terminal's size changes.
    pub fn set_term_size(&mut self, width: u16, height: u16) {
        self.term_width = width;
        self.term_height = height;
    }

    /// Whether the terminal is too small for anything but the floor notice.
    /// `ui.rs` paints the notice; this is what stops a key from acting with no
    /// visible result — the destructive `Ctrl-N` on a screen showing only
    /// `mush needs at least 40×10` was the bug this exists for (finding P11).
    pub fn below_floor(&self) -> bool {
        is_below_floor(self.term_width, self.term_height)
    }

    pub fn new(
        ws: Workspace,
        cell: ConfigCell,
        stored: Option<Session>,
        root: RootHandle,
        ui_tx: Sender<Msg>,
        session_save: Arc<dyn SessionSave>,
    ) -> Self {
        let system = Message::system(prompt::system_prompt(&ws.root_str()));
        let (messages, stored_agents, stored_notices) = match stored {
            Some(session) => (session.messages, session.agents, session.notices),
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let mut app = Self {
            ws,
            cell,
            focus: Focus::Chat,
            chat: Chat::new(system, messages),
            // Empty until a fetch says otherwise: the model list is discovered
            // on its own thread so nothing about an endpoint delays the first
            // frame (finding A9), and `/model` refetches if this is still
            // empty when the human asks.
            models: Vec::new(),
            picker: None,
            git: None,
            tree: AgentTree::rooted(root),
            session_save,
            session_dirty_at: None,
            ui_tx,
            git_at: None,
            git_in_flight: false,
            status: None,
            should_quit: false,
            dirty_screen: true,
            // The ubiquitous terminal, and above the floor: `main` reports the
            // real size before the first key can be read.
            term_width: 80,
            term_height: 24,
            spin: 0,
        };
        // The failures come back before the agents do, because the agent that
        // went back to idle takes its line with it (see `restore_agents`).
        app.chat.restore_notices(stored_notices);
        app.restore_agents(stored_agents);
        app.discover_worktrees();
        app.refresh_git();
        app
    }

    /// Adopt the subagents of the previous conversation: their nodes, their
    /// transcripts, and — through `agent::revive` — a live actor each, so a
    /// follow-up message continues the agent instead of starting over.
    ///
    /// A restored agent whose worktree is gone continues in the main checkout,
    /// which is where its work ended up once it was merged.
    fn restore_agents(&mut self, stored: Vec<session::AgentSession>) {
        if stored.is_empty() {
            return;
        }
        let cfg = self.cell.handle();
        let ui_tx = self.ui_tx.clone();
        let conversation = self.tree.conversation().0;
        let handles = self.tree.handles();
        let root = self.ws.root().to_path_buf();
        for agent in stored {
            // Keep the counter above every restored id, or the next spawn hands
            // a live child an id a restored agent already holds (finding B1).
            self.tree.reserve_ids(agent.id + 1);
            let (phase, summary) = match &agent.status {
                session::StoredStatus::Done => (Phase::Done, agent.summary.clone()),
                session::StoredStatus::Stopped => (Phase::Stopped, agent.summary.clone()),
                // A stored failure is only the last thing that happened if the
                // transcript still ends where a failure would have left it. A
                // run that fails on its first request appends nothing, so an
                // agent that had already answered (its transcript ends on a
                // plain assistant turn) is restored as done, with the refusal
                // as its line. Showing `✗` over a transcript that ends
                // "Done." claims the agent lost work it still has — which is
                // how a wall of `✗` appeared over finished work when a
                // thinking endpoint refused a replayed turn.
                session::StoredStatus::Failed(error) if ended_on_an_answer(&agent.messages) => {
                    (Phase::Done, Some(format!("last attempt refused: {error}")))
                }
                session::StoredStatus::Failed(error) => {
                    (Phase::Failed(error.clone()), agent.summary.clone())
                }
                // A run that was still in flight at shutdown is not a result.
                session::StoredStatus::Idle => (Phase::Idle, agent.summary.clone()),
            };
            // The row is derived and cannot lie, so the line that disagrees with
            // it is the one that goes: an agent come back idle or done keeps no
            // failure notice, and a `✓` row with a `!` line under it is exactly
            // the pair this rule exists to prevent.
            if !matches!(phase, Phase::Failed(_)) {
                self.chat.clear_notes_for(AgentId(agent.id));
            }
            let landed = agent.landed.map(|landed| match landed {
                session::StoredLanded::Merged => Landed::Merged,
                session::StoredLanded::Discarded => Landed::Discarded,
            });
            let tx = agent::revive(
                handles.clone(),
                cfg.clone(),
                ui_tx.clone(),
                conversation,
                root.clone(),
                agent::ReviveSpec {
                    id: agent.id,
                    depth: agent.depth.max(1),
                    brief: agent.brief.clone(),
                    branch: agent.branch.clone(),
                    messages: agent.messages.clone(),
                },
            );
            self.tree.register(Existing {
                id: AgentId(agent.id),
                parent: agent.parent.map(AgentId),
                depth: agent.depth.max(1),
                brief: agent.brief,
                phase,
                branch: agent.branch,
                summary,
                leftover: agent.leftover,
                landed,
                tx: Some(tx),
            });
            // What it said in the previous conversation is where it resumes.
            self.chat
                .replace_transcript(AgentId(agent.id), agent.messages);
        }
        self.tree.repair_focus();
    }

    /// Re-read what the repository looks like: the main worktree's branch,
    /// dirty count and uncommitted delta, plus each isolated branch's own work.
    /// Called on events, never from `draw` — a `git` process per frame would be
    /// absurd (docs/mush.md §8).
    /// Ask for a fresh read of the repository. The work happens on its own
    /// thread and comes back as `Msg::Git`: `git` is subprocesses, and
    /// subprocesses on the UI thread are dropped frames. This used to fork up to
    /// three per isolated agent, synchronously, every two seconds of a run —
    /// which is felt while an agent works, exactly when the screen is busiest.
    pub fn refresh_git(&mut self) {
        if self.git_in_flight {
            return;
        }
        self.git_in_flight = true;
        let root = self.ws.root().to_path_buf();
        // Resolved here: the tree is UI state, and the worker must not touch it.
        // A nested agent forked from its parent's branch, so that is what its
        // work is measured against; a top-level one forked from HEAD.
        let branches: Vec<(AgentId, String, String)> = self
            .tree
            .agents
            .iter()
            .filter_map(|node| {
                let branch = node.branch.clone()?;
                let base = node
                    .parent
                    .and_then(|parent| self.tree.node(parent))
                    .and_then(|parent| parent.branch.clone())
                    .unwrap_or_else(|| "HEAD".to_string());
                Some((node.id, base, branch))
            })
            .collect();
        let tx = self.ui_tx.clone();
        std::thread::spawn(move || {
            let mut stats = HashMap::new();
            for (id, base, branch) in branches {
                if let Some(stat) = git::branch_stat(&root, &base, &branch) {
                    stats.insert(id, stat);
                }
            }
            let status = git::status(&root);
            let _ = tx.send(Msg::Git { stats, status });
        });
    }

    /// Adopt a repository read that finished on its own thread.
    fn adopt_git(&mut self, stats: HashMap<AgentId, git::Stat>, status: Option<git::RepoStatus>) {
        // A reap can land between the read and this adoption (`/forget`, or a
        // stale leftover going away), and the row title sums every entry: a
        // stat for an id with no node would be counted as a ghost. The tree
        // owns which ids exist, so ask it rather than trusting the snapshot.
        let stats = stats
            .into_iter()
            .filter(|(id, _)| self.tree.has(*id))
            .collect();
        self.tree.agent_stats = stats;
        self.git = status;
        self.git_at = Some(Instant::now());
        self.git_in_flight = false;
        self.dirty_screen = true;
    }

    /// The window in tokens, for the meter. The number is the conversation's,
    /// not a copy of it: nothing can go stale between a push and a draw.
    pub fn context_used_tokens(&self) -> usize {
        self.chat.used_tokens_for(self.tree.focused)
    }

    /// How long ago the git snapshot was read, for the bar to say when it is
    /// old. The read is refreshed on the transitions a human drives (a focus
    /// change, a command, a save) and every two seconds while an agent works,
    /// so between events it ages — and an aged fact must not look live
    /// (finding P8).
    pub fn git_age(&self) -> Option<Duration> {
        self.git_at.map(|at| at.elapsed())
    }

    /// Register git worktrees left over from earlier sessions (`mush/<id>`
    /// branches) as finished tree nodes, so `/diff`, `/merge`, `/discard` keep
    /// working after a restart.
    pub fn discover_worktrees(&mut self) {
        let root = self.ws.root().to_path_buf();
        // `None` means git could not answer (no binary, not a repository). The
        // tree then keeps the leftovers it already knows about: dropping them
        // on a failed read would look like the work had been reclaimed.
        let Some(worktrees) = git::worktrees(&root) else {
            return;
        };
        // Drop stale leftovers whose worktree no longer exists. Reaping takes
        // the focus and the cursor off a ghost with them (finding B11).
        let gone: Vec<AgentId> = self
            .tree
            .agents
            .iter()
            .filter(|node| node.leftover)
            .filter(
                |node| match node.branch.as_deref().and_then(git::worktree_id) {
                    Some(id) => !git::worktree_path(&root, id).exists(),
                    None => true,
                },
            )
            .map(|node| node.id)
            .collect();
        self.tree.reap(&gone);
        for worktree in worktrees {
            let Some(id) = worktree.id else {
                continue;
            };
            if self.tree.has(AgentId(id)) {
                continue;
            }
            let full = git::branch_name(id);
            // Keep the tree's counter above every registered id, or the next
            // `spawn_agent` hands a live child an id a leftover already holds
            // (finding B1).
            self.tree.reserve_ids(id + 1);
            // The commit mush made for this worktree names the task and how the
            // run ended, so a leftover is shown as the work it is instead of an
            // anonymous placeholder. A branch the human committed to by hand
            // carries no such subject and stays unnamed.
            let (brief, phase, summary) =
                match git::subject_of(&root, &full).and_then(|s| agent::parse_commit_subject(&s)) {
                    Some((agent::Committed::Finished, brief)) => {
                        (brief, Phase::Done, "last run finished")
                    }
                    Some((agent::Committed::Stopped, brief)) => {
                        (brief, Phase::Stopped, "last run was stopped")
                    }
                    Some((agent::Committed::Failed(error), brief)) => {
                        (brief, Phase::Failed(error), "last run failed")
                    }
                    None => (
                        "leftover worktree".to_string(),
                        Phase::Done,
                        "found on startup",
                    ),
                };
            self.tree.register(Existing {
                id: AgentId(id),
                parent: None,
                depth: 1,
                brief,
                phase,
                branch: Some(full),
                summary: Some(summary.to_string()),
                leftover: true,
                landed: None,
                tx: None,
            });
        }
        self.tree.repair_focus();
    }

    /// What `/worktrees` says: the truth about `.mush/wt` on disk *and* what
    /// this session has adopted. The old line counted only the leftovers the
    /// tree already knew, so a worktree `git worktree list` named — its branch
    /// not `mush/<id>`, so `discover_worktrees` skipped it — was invisible and
    /// the message claimed there were none while the directory sat there
    /// (finding P10). The disk is the fact; the adopted set is what the
    /// commands can act on; the sentence has to say both to be true.
    fn worktree_report(&self) -> String {
        let root = self.ws.root();
        // `None` is git not answering; say so rather than claim a count.
        let Some(worktrees) = git::worktrees(root) else {
            return "cannot list worktrees — git did not answer".to_string();
        };
        let dir = root.join(git::WORKTREE_DIR);
        let on_disk = worktrees
            .iter()
            .filter(|worktree| worktree.path.starts_with(&dir))
            .count();
        let registered = self.tree.agents.iter().filter(|node| node.leftover).count();
        if on_disk == 0 {
            return "no worktrees under .mush/wt on disk".to_string();
        }
        let mut line = format!("{on_disk} worktree(s) under .mush/wt on disk");
        if registered == on_disk {
            line.push_str(" · all registered — /diff, /merge, /discard work on them");
        } else {
            // The difference is a worktree mush cannot name (`mush/<id>`
            // branch missing, or the id already taken) — name the count, not a
            // guess at the cause.
            line.push_str(&format!(" · {registered} registered as leftovers"));
        }
        line
    }

    // ---------------------------------------------------------------- updates

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Models { endpoint, models } => self.adopt_models(endpoint, models),
            Msg::Git { stats, status } => self.adopt_git(stats, status),
            Msg::Paste(text) => {
                // A paste is something the human wants to say, so it lands in
                // the message box whichever pane has focus. An open picker is
                // the one place a paste has no meaning; below the floor there
                // is no box on screen for it to land in, so it is refused the
                // way a key is (finding P11).
                if self.picker.is_none() && !self.below_floor() {
                    // Terminals disagree about line endings in a paste.
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    self.chat.insert(&text);
                }
            }
            Msg::Key(key) => self.on_key(key),
            Msg::Agent {
                conversation,
                id,
                event,
            } => {
                if conversation == self.tree.conversation() {
                    self.on_agent(id, event);
                } else if let AgentEvent::Spawned { cmd, .. } = &event {
                    // A tree `/new` abandoned can still spawn children. They are
                    // not ours, but they must not run either — and because this
                    // event is dropped, telling the child here is the only
                    // chance it gets to end.
                    let _ = cmd.send(AgentMsg::Shutdown);
                }
            }
            Msg::Attach {
                from,
                request,
                reply,
            } => {
                let response = self.handle_attach(&from, &request);
                // A client that hung up while the answer was being built leaves
                // no receiver; that is not an error here.
                let _ = reply.send(response);
            }
        }
        self.dirty_screen = true;
    }

    /// Called once per event-loop pass so the status spinner animates, and so
    /// a line that has outlived its welcome leaves the screen even when nothing
    /// else is happening.
    pub fn tick(&mut self) {
        if self.busy() {
            self.spin = self.spin.wrapping_add(1);
            self.dirty_screen = true;
            // A long run keeps changing the workspace; the bar and the rows
            // should not need a keystroke to notice.
            if self
                .git_at
                .map(|at| at.elapsed() > Duration::from_secs(2))
                .unwrap_or(true)
            {
                self.refresh_git();
            }
        }
        if let Some(status) = &self.status {
            if status.kind == StatusKind::Info && status.set_at.elapsed() >= INFO_TTL {
                self.status = None;
                self.dirty_screen = true;
            }
        }
        // The same expiry for the pane's own transient lines, on the same tick:
        // a command's answer or a hint that nobody ended by acting must not sit
        // in the foot for the life of the session (finding U8). Failures are not
        // chatter and are never taken away here.
        if self.chat.expire_said(session::now_secs()) {
            self.dirty_screen = true;
        }
        // A `⊘` whose acknowledgement never arrives leaves a row spinning
        // forever, which is worse than an idle one (finding B6).
        if self.tree.expire_cancels() {
            self.dirty_screen = true;
        }
        // The file is allowed to lag the conversation by `SESSION_DEBOUNCE`.
        // This is where that lag is paid: the rebuild and the hand-over happen
        // once per interval, on a tick, and never in the handler that received
        // a message — which is what makes a tool result cost the UI nothing.
        if self
            .session_dirty_at
            .map(|dirty_at| dirty_at.elapsed() >= SESSION_DEBOUNCE)
            .unwrap_or(false)
        {
            self.save_session();
        }
        // A write that failed on the writer's thread has no caller to return
        // to, so it is picked up here — the next tick after it happened.
        if let Some(error) = self.session_save.take_error() {
            self.fail(format!("could not save session: {error}"));
        }
    }

    fn on_agent(&mut self, id: AgentId, event: AgentEvent) {
        match event {
            AgentEvent::Spawned {
                child,
                parent,
                brief,
                depth,
                branch,
                cmd,
            } => {
                let opened = self.tree.insert(Spawn {
                    id: AgentId(child),
                    parent: AgentId(parent),
                    brief,
                    depth,
                    branch,
                    cmd,
                });
                // The brief opens the child's transcript: the model sees the
                // brief, so the human should too (finding B13).
                self.chat.push_message(opened.id, opened.opening);
                // The node — its brief, branch and parent — is stored, so a
                // restart comes back with the same tree.
                self.mark_session_dirty();
            }
            AgentEvent::Running { cancel } => {
                // A run started, possibly one the UI did not ask for (an idle
                // agent woken by a child's result). Mark it so `busy`, the
                // spinner, and Ctrl-C agree with the actor. The last run's
                // summary belongs to that run, not this one (finding B14).
                self.tree.begin(id, Some(cancel));
                // A new run supersedes everything mush said about the last one:
                // the hints answered a command in a moment that is over, and the
                // failure belonged to the run this one is replacing. The row is
                // derived from the phase and cannot go stale, so the line is the
                // one that ages out — which is also what keeps a pane from
                // holding a `!` line next to a run in flight that is fixing it.
                if self.chat.clear_notes_for(id) {
                    self.mark_session_dirty();
                }
            }
            AgentEvent::Status(status) => {
                // A status that arrives after the run's own end (a late or
                // duplicated commit line) must not put a finished agent back to
                // work (finding B5).
                self.tree.activity(id, status);
            }
            AgentEvent::Notice(text) => {
                // A limit the run reached (it still produced a result), or a
                // reply that was empty: a line in the transcript, tagged with
                // the agent it concerns (finding B19).
                self.chat.note_for(id, text);
            }
            AgentEvent::Message(message) => {
                // The pane is deliberately not sent to the bottom here: the
                // position is the human's, and a pane that is at the bottom
                // follows the newest line by construction (finding U3).
                self.chat.push_message(id, message);
                // Every message is part of what the file stores — a subagent's
                // as much as the root's — but the mark is O(1): the rebuild and
                // the write wait for the debounced tick, so a streamed tool
                // result cannot stall the frame that shows it.
                self.mark_session_dirty();
            }
            AgentEvent::Stopped => {
                // Stopped is not failed and not done: the run produced nothing,
                // and the actor is idle and resumable. Saying which one it is
                // is the difference between a lost agent and a parked one.
                self.tree.stopped(id);
                // The phase is stored, so a restart must not show a stopped run
                // as one that never happened.
                self.mark_session_dirty();
                self.refresh_git();
                // Only the agent the human is looking at needs the bar; a
                // stop they did not ask for still shows as ⊘ on its row.
                if id == self.tree.focused {
                    self.say(format!("agent #{id} stopped — send a message to resume it"));
                }
            }
            AgentEvent::Error(error) => {
                self.tree.fail(id, error.clone());
                self.mark_session_dirty();
                self.refresh_git();
                // The durable half of the same fact: the row's `✗` is derived and
                // dies with the next run, while this line is tagged, stamped and
                // written to the session, so a restart still says what broke.
                self.chat.note_error_for(id, error.clone());
                // A failure is the third way a run can end, and it is the one
                // that did not reach the bar: `Stopped` says so, `Failed` fell
                // back to the idle hint, so the newest thing that had happened
                // could be a crash under a line advertising Ctrl-P. A guard-stop
                // is this same event (the runaway guard's `stopped after N
                // turns…` is the run's error), so both are said here. Only the
                // agent the human is reading needs the bar — another agent's
                // failure is on its own row's `✗` and in its own pane's foot —
                // and the sentence names the agent, which the foot's `!` line
                // (already in front of the human) does not.
                if id == self.tree.focused {
                    self.fail(format!("agent #{id} failed — {error}"));
                }
            }
            AgentEvent::Done => {
                let summary = self.last_assistant_text(id);
                // Every Done replaces the row's summary; keeping the first one
                // described a run that ended long ago (finding B14).
                self.tree.finish(id, summary);
                self.mark_session_dirty();
                self.refresh_git();
            }
            AgentEvent::JobStarted { job, command } => {
                // The bar says what just happened, and the registry — which the
                // rows read every frame — is what says what is running now. No
                // copy of the job is kept here: the row's `⚙N` count is derived
                // from the registry on every frame, so it cannot go stale.
                let note = format!(
                    "{} detached · {}",
                    crate::jobs::label(job),
                    job_title(&command)
                );
                self.say_for(id, note);
            }
            AgentEvent::JobDone { job, line } => {
                // A job's report is the owner's to read in its transcript (the
                // actor folds it in); on the screen it is the bar's line, and
                // the badge the job was on goes out with it.
                let _ = job;
                self.say_for(id, line);
            }
            AgentEvent::Context { tokens, source } => {
                // The actor learned the endpoint's real window from a server
                // complaint; the UI owns the copy the bar, `/context`, and the
                // tool caps read, so it has to adopt the same number or the
                // next `/model` clobbers it (finding B7). The cell is the one
                // path: this is the UI adopting on the same terms the actor
                // did, and it writes the actors' copy with it.
                self.cell.learn_context(tokens, source);
            }
            AgentEvent::Compacting { why, cancel } => {
                // A fold was accepted, parked, or put on the wire. It is a
                // phase, not a status line: the row, the bar and the foot all
                // read this one answer, and it lasts exactly as long as the
                // fold does — where a status line is dropped for an agent at
                // rest, which is the agent an idle `/compact` runs on, and
                // fades on a timer for one that is not (finding U11).
                self.tree.compacting(id, why, cancel);
            }
            AgentEvent::CompactingEnded { in_run } => {
                self.tree.compacting_ended(id, in_run);
            }
            AgentEvent::Compact { summary, in_run } => {
                // The actor's transcript is now [system, user(summary)];
                // mirror it so nudges, saves, and the visible chat stay in
                // sync with what the model actually sees.
                let carried = Message::user(prompt::compaction_message(&summary));
                self.chat.replace_transcript(id, vec![carried]);
                // The fold is over, and the row must stop saying it is folding:
                // the summary is read where it now lives, in the transcript.
                self.tree.compacted(id, in_run);
                if id == AgentId::ROOT {
                    // A fold is deliberate and expensive, and the transcript it
                    // leaves is what a restart resumes from — so it is written
                    // before this returns rather than waiting out the debounce.
                    self.flush_session();
                    self.chat
                        .note("context compacted — continuing from a summary");
                } else {
                    self.mark_session_dirty();
                }
            }
        }
    }

    /// Whether anything in the tree is working: derived from the phases and the
    /// job registry, so it cannot disagree with the rows. A detached job counts:
    /// the agent may be napping, but the machine is not idle, and the tick uses
    /// this to keep the git snapshot fresh while something runs.
    pub fn busy(&self) -> bool {
        // The tree answers the common case with no registry read; the per-node
        // check only adds the jobs.
        self.tree.busy() || self.tree.agents.iter().any(|node| self.in_flight(node))
    }

    /// Whether one agent has work in flight: its own run, or one of its jobs.
    /// The one per-node derivation behind [`Self::busy`] and
    /// [`Self::working_agents`]. [`AgentTree::busy`] stays agent-only, with no
    /// opinion about jobs.
    fn in_flight(&self, node: &AgentNode) -> bool {
        node.phase.is_busy() || !self.tree.live_jobs(node.id).is_empty()
    }

    /// The jobs `id` has running, read from the one registry that holds them.
    /// Every place the screen mentions a job goes through here, so the row's
    /// count, the footer's list and the bar's line cannot disagree about what
    /// is running on this machine.
    pub fn live_jobs(&self, id: AgentId) -> Vec<crate::jobs::JobView> {
        self.tree.live_jobs(id)
    }

    /// `#c2 cargo build 1m20s` — one job, as the footer and the bar read it.
    pub fn job_lines(&self, id: AgentId) -> Vec<String> {
        self.live_jobs(id)
            .into_iter()
            .map(|job| {
                let held = if job.exclusive { " · the machine" } else { "" };
                format!(
                    "{} {} {}{held}",
                    crate::jobs::label(job.id),
                    job_title(&job.command),
                    short_age(job.age)
                )
            })
            .collect()
    }

    /// The one door a bar line goes through: the text is
    /// [`mush_core::text::sanitize`]d here, because the bar paints it whole —
    /// it does no width arithmetic, so the `truncate`/`fit_row` rule cannot
    /// reach it. Private, so every string the bar can show is created by
    /// [`Self::say`] or [`Self::fail`], which differ only in the kind.
    fn set_status(&mut self, kind: StatusKind, text: impl Into<String>) {
        let text = text.into();
        self.status = Some(Status {
            kind,
            text: mush_core::text::sanitize(&text),
            set_at: Instant::now(),
        });
    }

    /// Remember a transient line for the bar: what a command just did, what the
    /// human just asked for. It fades.
    pub fn say(&mut self, text: impl Into<String>) {
        self.set_status(StatusKind::Info, text);
    }

    /// A bar line about `id`, named with the agent unless it is the focused
    /// one: the focused agent's own line needs no name.
    fn say_for(&mut self, id: AgentId, text: String) {
        if id == self.tree.focused {
            self.say(text);
        } else {
            self.say(format!("agent #{id}: {text}"));
        }
    }

    /// `500k`, `8192`, `1M` — one glance, no counting zeroes.
    pub fn context_label(&self) -> String {
        let spelling = tokens_label(self.cfg().context_tokens);
        if self.cfg().context_explicit {
            format!("ctx {spelling} (set)")
        } else {
            format!("ctx ~{spelling}")
        }
    }

    /// How full the conversation in the open pane is, against the window it is
    /// being sent to: `ctx 3.1k/500k`. The window alone says how much room there
    /// is, never how much of it this conversation has taken — and the pane can
    /// be a subagent's, whose own next request is what this number measures.
    ///
    /// At the window and past it there is no longer a fraction to print: a
    /// learned window can be smaller than the transcript already held, so the
    /// meter read `ctx 1.2k/1k` — a ratio greater than one with nothing saying
    /// so (finding P9). At the limit it says `full`; past it, it says `over`.
    pub fn context_meter(&self) -> String {
        let used = self.context_used_tokens();
        let window = self.cfg().context_tokens;
        let mark = if self.cfg().context_explicit { "" } else { "~" };
        let used_label = tokens_label(used);
        let window_label = tokens_label(window);
        let state = match used.cmp(&window) {
            std::cmp::Ordering::Greater => " over",
            std::cmp::Ordering::Equal => " full",
            std::cmp::Ordering::Less => "",
        };
        format!("ctx {used_label}/{mark}{window_label}{state}")
    }

    /// The conversation this workspace was left holding could not be read, and
    /// the human has to hear it before they mistake the empty screen for an
    /// empty workspace (finding S3).
    ///
    /// It takes the two homes a failure takes: the root pane's foot — wrapped
    /// to the pane, ranked `Alert`, read back whole by `/notes` — and the bar's
    /// line one, so it is visible without opening anything and stays visible
    /// until something replaces it. It is news rather than chatter, so the
    /// human's next send does not end it, and the session is marked dirty so
    /// the next save writes it: a workspace that could not be read is a fact
    /// about the workspace, not about the moment it was noticed. (The root's
    /// own next run supersedes it, as it supersedes any failure — but by then
    /// the human has run something in the conversation it opened.) The words
    /// come from the caller (`main`), which is the one place that knows the
    /// path, the reason and where the only copy went.
    pub fn session_unreadable(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.chat.note_error_for(AgentId::ROOT, text.clone());
        self.fail(text);
        self.mark_session_dirty();
    }

    /// Remember something that went wrong. Errors do not fade: they stay until
    /// a later line replaces them.
    ///
    /// A failure is where the *endpoint's* own words reach the bar — a 500's
    /// error body, a refusal's reason — so it goes through the same door `say`
    /// does (see [`Self::set_status`]).
    pub fn fail(&mut self, text: impl Into<String>) {
        self.set_status(StatusKind::Error, text);
    }

    /// The transient line, if it is still worth showing.
    pub fn status_line(&self) -> Option<(&str, StatusKind)> {
        let status = self.status.as_ref()?;
        if status.kind == StatusKind::Error || status.set_at.elapsed() < INFO_TTL {
            Some((status.text.as_str(), status.kind))
        } else {
            None
        }
    }

    /// The one derived line about the *tree*, when the rows cannot say it
    /// themselves.
    ///
    /// This used to be the focused agent's newest activity
    /// (`#0 edit_file src/lib.rs 12s`), which the agent's own row had said
    /// (`◐ #0 edit_file src/lib.rs 12s`) and the transcript had said again
    /// (`⚙ edit_file src/lib.rs`): one fact, three homes, and the bar's only
    /// line spent on a sentence the human had already read twice (finding U5).
    /// What survives is the fact no row states even though its `⏸N` implies
    /// it: an orchestrator that has ended its turn with children still working
    /// is napping and *will* resume by itself (§5.5), which is a promise about
    /// what happens next rather than a report of what is happening now.
    ///
    /// Everything else the bar's line one carries is an event with no other
    /// home — a failure, a stop, a job's report, a command's answer — and the
    /// newest of those is the status, not this.
    ///
    /// A fold the human is waiting for comes first, because it is the one
    /// derived state that answers a question they are holding in their head:
    /// the fold is running, and the words they are about to type are not lost.
    /// The row says which kind of fold it is (`compacting…`, `folding at the
    /// next step…`) with its age; this is the sentence that lets them keep
    /// typing (finding U11).
    ///
    /// The count is the root's *own* busy children — [`AgentTree::busy_children`] —
    /// the same derivation the row's `⏸N` mark and the title's `M waiting`
    /// read, and not every busy node in the tree. The sentence is a promise
    /// about when the root resumes, and it resumes when its children finish: a
    /// grandchild working under a child that is itself parked promised a resume
    /// the grandchild's finish does not cause, and it contradicted the title of
    /// the very frame it was painted in (a second owner of the fact U2 named).
    pub fn tree_line(&self) -> Option<String> {
        let focused = self.tree.focused;
        if self
            .tree
            .node(focused)
            .is_some_and(|node| node.phase.compacting().is_some())
        {
            return Some(format!(
                "compacting #{focused} · keep typing — your message is answered after the fold"
            ));
        }
        let waiting = self.tree.busy_children(AgentId::ROOT);
        let root_is_working = self
            .tree
            .node(AgentId::ROOT)
            .is_some_and(|root| root.phase.is_busy());
        if waiting == 0 || root_is_working {
            return None;
        }
        Some(format!(
            "waiting on {waiting} subagent(s) — the root resumes as they finish"
        ))
    }

    /// The last assistant reply in an agent's transcript (its final summary).
    fn last_assistant_text(&self, id: AgentId) -> Option<String> {
        self.chat
            .transcript(id)
            .iter()
            .rev()
            .find(|message| message.role == "assistant")
            .map(|message| message.text().trim().to_string())
            .filter(|text| !text.is_empty())
    }

    // ------------------------------------------------------------- chat / LLM

    /// The human's next act ends the moment the last chatter line answered
    /// (finding U8): a `/help`, a diff or a hint was about the thing they typed
    /// *before* this one, and leaving it in the foot spends rows on a question
    /// nobody is asking any more. Failures stay — they are the run's record, not
    /// a moment's — and so does the pane's red line, which is the bar's copy of
    /// the same fact.
    fn ends_the_moment(&mut self) {
        if self.chat.dismiss_said() {
            self.dirty_screen = true;
        }
    }

    /// Send what is in the message box: a command to run, or a message for the
    /// focused agent.
    ///
    /// The two are told apart by the one parser (`app::commands`), which returns
    /// a value rather than a string to compare here — so what a command *is*
    /// has exactly one definition, and the help text reads the same table the
    /// arms do.
    fn send_message(&mut self) {
        let text = self.chat.take_input().trim().to_string();
        if text.is_empty() {
            return;
        }
        let parsed = commands::parse_command(&text);
        // `/notes` is the reader of the chatter lines, not the act that
        // supersedes them: a human asking for the rest of the foot is reading
        // it, and dismissing what they came to read would make the pane's
        // `+N more · /notes` point at nothing (see `ends_the_moment`).
        if !matches!(&parsed, Ok(Command::Notes)) {
            self.ends_the_moment();
        }
        match parsed {
            Ok(command) => self.apply_command(command),
            // No slash: the human is talking to an agent.
            Err(CommandError::NotACommand) => self.deliver(text),
            // A command that exists but whose argument does not read. The line
            // was spelled beside the rule that rejected it, and the bar is
            // where every other complaint of this kind goes.
            Err(CommandError::Usage(line)) => self.say(line),
            // A slash nobody implements answered a command the human just
            // typed, in a moment that ends the instant they type again — an
            // informational line in the focused pane, not a failure. As an
            // error it was ranked an alert (never the line the cap yielded),
            // painted red, and written to the session, so a typo outlived the
            // run it answered and came back at the next start.
            Err(CommandError::Unknown(name)) => self
                .chat
                .note_for(self.tree.focused, format!("unknown command: {name}")),
        }
    }

    /// Why a message to `id` cannot run, if it cannot: its worktree is gone.
    ///
    /// `/merge` and `/discard` reclaim an isolated agent's worktree and branch
    /// but leave its actor alive, and that actor's file tools resolve their
    /// directory from the workspace it was spawned with — so a run would
    /// recreate the reclaimed path as a plain directory where no surface could
    /// see the work (finding S1). The row, the footer and `/diff`/`/merge`/
    /// `/worktrees` already tell the landed story; this is the same story for
    /// typing. A worktree a hand-run `git worktree remove` took reads the same
    /// way: the branch is named, the worktree is not on disk, so a run would
    /// write into a phantom.
    fn worktree_gone(&self, id: AgentId) -> Option<String> {
        let node = self.tree.node(id)?;
        if let Some(landed) = node.landed {
            let past = match landed {
                Landed::Merged => "merged",
                Landed::Discarded => "discarded",
            };
            return Some(format!(
                "agent #{id} was {past} — its worktree is gone; \
                 spawn a fresh agent or work in the root"
            ));
        }
        if node.branch.is_some() && !git::worktree_path(self.ws.root(), id.0).exists() {
            return Some(format!(
                "agent #{id}'s worktree is gone — spawn a fresh agent or work in the root"
            ));
        }
        None
    }

    /// A typed message, from the human to the focused agent.
    fn deliver(&mut self, text: String) {
        // A request without a model is a guaranteed refusal from the endpoint,
        // and since discovery runs after the first frame this state is
        // reachable for as long as one fetch takes (finding A9). Saying so is
        // better than the endpoint's own complaint about an empty model id —
        // and the words go back in the box, because a message that cannot be
        // sent is not something the human should have to retype.
        if self.cfg().model.is_empty() {
            self.chat.insert(&text);
            self.fail("no model yet — /model picks one, /url points mush at an endpoint");
            return;
        }
        let target = self.tree.focused;
        if target == AgentId::ROOT {
            // The human's words belong in the transcript they can see, whether
            // the root is starting a run or already in one.
            //
            // Sending does not send the pane to the bottom either: the human
            // chose where to read, and the key that puts a pane back at the
            // newest line is the one they press (finding U3).
            self.chat
                .push_message(AgentId::ROOT, Message::user(text.clone()));
            // The human's own words are the one thing worth blocking on: the
            // run they start may take minutes, and a crash in it must not lose
            // the request. This is one write per turn, not one per response.
            self.flush_session();
            // The root's own phase, not the tree's: a napping orchestrator is
            // idle, and its next message starts a run rather than nudging a
            // conversation that is not in flight.
            let root_busy = self
                .tree
                .node(AgentId::ROOT)
                .map(|node| node.phase.is_busy())
                .unwrap_or(false);
            if root_busy {
                // Human steering while the root runs: queued as a nudge. If the
                // root's mailbox is dead (it was cancelled), fall through and
                // start a fresh run instead of spinning forever on a ghost.
                let alive = self
                    .tree
                    .agent_tx
                    .get(&AgentId::ROOT)
                    .map(|tx| tx.send(AgentMsg::Nudge(text.clone())).is_ok())
                    .unwrap_or(false);
                if alive {
                    self.say("noted — folded in as the agent continues");
                    return;
                }
                self.tree.idle(AgentId::ROOT);
            }
            let messages = self.chat.conversation();
            match self.tree.agent_tx.get(&AgentId::ROOT) {
                Some(tx) if tx.send(AgentMsg::Run(messages)).is_ok() => {
                    // The run starts now as far as the human is concerned; the
                    // actor's `Running` event will agree with this, and brings
                    // the run's cancel flag with it.
                    self.tree.begin(AgentId::ROOT, None);
                }
                _ => {
                    self.fail("root agent is gone — /new restarts it");
                }
            }
        } else {
            // A landed agent cannot run again: its file tools resolve their
            // directory from the workspace it was spawned with, so a run would
            // recreate the reclaimed path as a plain directory where no surface
            // — not `git status`, not `/diff`, not `/merge` — could see the work
            // (finding S1). The words stay in the box and nothing runs.
            if let Some(line) = self.worktree_gone(target) {
                self.chat.insert(&text);
                self.fail(line);
                return;
            }
            // Nudge a specific agent; running ones fold it in, idle ones rerun.
            // If the mailbox is gone the node's phase is put back exactly as it
            // was, instead of leaving a lie on the row (finding B10).
            let previous = self.tree.nudge(target);
            self.chat.push_message(target, Message::user(text.clone()));
            match self.tree.agent_tx.get(&target) {
                Some(tx) if tx.send(AgentMsg::Nudge(text)).is_ok() => {}
                _ => {
                    self.tree.nudge_failed(target, previous);
                    self.fail(format!("agent #{target} is gone"));
                }
            }
        }
    }

    // -------------------------------------------------------------- attach (M3)

    /// Answer one request from the attach socket. The socket thread hands the
    /// request over and waits; this is the only place an external agent moves
    /// mush, and every op runs the same code a keystroke would — `focus` is
    /// `Enter` on a row, `edit` is the message box and the send — so the
    /// socket cannot reach a state the human could not.
    pub fn handle_attach(&mut self, from: &str, request: &attach::Request) -> attach::Response {
        let reply = match &request.op {
            attach::Op::Read { agent, since } => self.attach_read(*agent, *since),
            attach::Op::Agents => self.attach_agents(),
            attach::Op::Focus { agent } => self.attach_focus(*agent),
            attach::Op::Edit {
                agent,
                base,
                text,
                send,
            } => self.attach_edit(from, *agent, *base, text, *send),
        };
        attach::Response {
            id: request.id.clone(),
            reply,
        }
    }

    /// The transcript lines of `agent` from `since` (0-based, inclusive), with
    /// the revision a later `edit` must carry.
    fn attach_read(&self, agent: u64, since: usize) -> attach::Reply {
        let id = AgentId(agent);
        if !self.tree.has(id) {
            return attach::Reply::Err(attach::ReplyError::bad_request(format!("no agent #{id}")));
        }
        let lines: Vec<serde_json::Value> = self
            .chat
            .transcript(id)
            .iter()
            .enumerate()
            .skip(since)
            .map(|(line, message)| {
                serde_json::json!({
                    "line": line,
                    "role": message.role,
                    "text": message.text(),
                })
            })
            .collect();
        attach::Reply::Ok(serde_json::json!({
            "agent": agent,
            "revision": self.chat.revision(id),
            "lines": lines,
        }))
    }

    /// The roster the tree pane paints, read from the tree and never from the
    /// session file, so a client can see the whole tree — phases, parents,
    /// working children — without a copy that lags it (M3 / H1).
    fn attach_agents(&self) -> attach::Reply {
        let agents: Vec<serde_json::Value> = self
            .tree
            .rows()
            .iter()
            .map(|node| {
                serde_json::json!({
                    "id": node.id.0,
                    "parent": node.parent.map(|parent| parent.0),
                    "depth": node.depth,
                    "phase": node.phase.label(),
                    "activity": node.phase.detail(),
                    "title": node.title(),
                    "branch": node.branch.clone(),
                    "worktree": self.attach_worktree(node.id),
                    "focused": self.tree.focused == node.id,
                    "children_working": self.tree.busy_children(node.id),
                    "leftover": node.leftover,
                    "summary": node.summary.clone(),
                    "revision": self.chat.revision(node.id),
                })
            })
            .collect();
        attach::Reply::Ok(serde_json::json!({
            // The root's transcript is the conversation; its revision is the
            // one token that covers the chat a client is most likely to edit.
            "revision": self.chat.revision(AgentId::ROOT),
            "agents": agents,
        }))
    }

    /// Where an agent works: its worktree when it has a branch, else the main
    /// checkout. Derived from the branch, the same way the row's location is.
    fn attach_worktree(&self, id: AgentId) -> String {
        match self.tree.node(id).and_then(|node| node.branch.as_ref()) {
            Some(_) => git::worktree_path(self.ws.root(), id.0)
                .display()
                .to_string(),
            None => self.ws.root().display().to_string(),
        }
    }

    /// Focus `agent` exactly as `Enter` on its row does: point the tree cursor
    /// at it and run the same path the key does, so the pane, the bar and the
    /// keyboard all move together.
    fn attach_focus(&mut self, agent: u64) -> attach::Reply {
        let id = AgentId(agent);
        if !self.tree.has(id) {
            return attach::Reply::Err(attach::ReplyError::bad_request(format!("no agent #{id}")));
        }
        self.tree.point_cursor_at(id);
        self.focus_cursor_row();
        attach::Reply::Ok(serde_json::json!({}))
    }

    /// An external agent's edit: set the message box's draft for `agent`, or
    /// deliver `text` as the human's message, but only when that agent's
    /// transcript still stands at `base`. Otherwise it is a `conflict` with the
    /// revision that moved — never a guess at what the client meant.
    fn attach_edit(
        &mut self,
        from: &str,
        agent: u64,
        base: u64,
        text: &str,
        send: bool,
    ) -> attach::Reply {
        let id = AgentId(agent);
        if !self.tree.has(id) {
            return attach::Reply::Err(attach::ReplyError::bad_request(format!("no agent #{id}")));
        }
        let revision = self.chat.revision(id);
        if revision != base {
            return attach::Reply::Err(attach::ReplyError::conflict(revision));
        }
        if send {
            // The two refusals a typed message has, said back to the client
            // rather than into the human's box: the words are the client's,
            // and a request that could not run must not land as a draft.
            if self.cfg().model.is_empty() {
                return attach::Reply::Err(attach::ReplyError::bad_request(
                    "no model yet — the message was not sent",
                ));
            }
            if let Some(line) = self.worktree_gone(id) {
                return attach::Reply::Err(attach::ReplyError::bad_request(line));
            }
            // The same path a typed message takes: `deliver` sends to the
            // focused agent, so aim it there for the turn. The keyboard focus
            // is put back, because an external client steering an agent must
            // not move the human's pane.
            let previous = self.tree.focused;
            self.tree.focused = id;
            self.chat.expect_human(text);
            self.deliver(text.to_string());
            self.tree.focused = previous;
        } else {
            self.chat.set_draft(id, text);
            self.say(format!("{from}: set the draft for #{id}"));
        }
        attach::Reply::Ok(serde_json::json!({ "revision": self.chat.revision(id) }))
    }

    /// Run one parsed command.
    ///
    /// Matching the variant rather than the typed line is what makes a command
    /// the parser knows but this match does not a compile error instead of a
    /// silent fall-through, and it is what lets the help text, the parser and
    /// these arms all read the same table (`app::commands`).
    fn apply_command(&mut self, command: Command) {
        match command {
            Command::New => self.new_chat(),
            Command::Quit => self.should_quit = true,
            Command::Help => self.chat.note(help_notice()),
            Command::Notes => self.open_notes_picker(),
            Command::Context(None) => self.say(format!(
                "{} · {} tokens used · set it with /context <tokens>",
                self.context_label(),
                tokens_label(self.context_used_tokens())
            )),
            Command::Context(Some(tokens)) => {
                self.cell.edit(|cfg| cfg.set_context(tokens));
                // A stated window is remembered for this workspace, so it is on
                // disk before the command returns.
                self.flush_session();
                self.say(format!(
                    "{} — remembered for this workspace",
                    self.context_label()
                ));
            }
            Command::Worktree { verb, id } => self.worktree_command(verb, AgentId(id)),
            Command::Compact => self.compact_focused(),
            Command::Forget(id) => self.forget_agent(AgentId(id)),
            Command::Worktrees => {
                self.discover_worktrees();
                self.refresh_git();
                self.say(self.worktree_report());
            }
            Command::Provider(None) => self.open_provider_picker(),
            Command::Provider(Some(name)) => self.apply_provider(&name),
            Command::Model => self.open_model_picker(),
            Command::Url(url) => {
                // A new endpoint may host a different model with a different
                // window; re-derive it unless the human stated one (finding A5).
                self.switch_endpoint(&url);
                self.refresh_models();
                self.say(format!(
                    "endpoint: {} · {}",
                    self.cfg().base_url,
                    self.context_label()
                ));
                self.persist_user_config();
            }
            Command::ApiKey(None) => match &self.cell.ui().api_key {
                Some(key) => self.say(format!("api key set ({}…)", mask_key(key))),
                None => self.say("no api key — /key <secret> sets one (memory only)"),
            },
            Command::ApiKey(Some(secret)) => {
                // Masked before it is moved: the message quotes the same secret
                // the config now holds, and only its head is ever printed.
                let shown = mask_key(&secret);
                self.cell.edit(|cfg| cfg.api_key = Some(secret));
                self.persist_user_config();
                self.say(format!(
                    "api key set ({shown}…) — saved to {}",
                    userconfig::config_path().display()
                ));
            }
            Command::Models => {
                self.refresh_models();
                self.say(if self.models.is_empty() {
                    format!("no models from {}", self.cfg().models_url())
                } else {
                    format!(
                        "{} models from {}",
                        self.models.len(),
                        self.cfg().models_url()
                    )
                });
            }
        }
        // A command is a transition the human drove: whatever they asked for
        // may have changed the workspace, and the bar they read next should
        // not be yesterday's answer (finding P8). The arms that already
        // refreshed are no worse for the second ask — `git_in_flight` drops it.
        self.refresh_git();
    }

    // ------------------------------------------------------------ providers

    /// Remember the current setup in the home config file so the API key (and
    /// endpoint defaults) survive restarts. Never writes to the workspace.
    fn persist_user_config(&mut self) {
        let user = UserConfig {
            api_key: self.cfg().api_key.clone(),
            provider: self.cfg().provider.name().to_string(),
            base_url: self.cfg().base_url.clone(),
            model: self.cfg().model.clone(),
            // The fields this save does not state — the window, the temperature,
            // the reply cap's name, and the two thinking knobs — keep whatever
            // the file already holds.
            ..UserConfig::default()
        };
        if let Err(error) = user.save() {
            self.fail(format!("could not save home config: {error}"));
        }
    }

    /// Re-fetch the model list from the current endpoint, falling back to the
    /// provider's built-in list when the endpoint cannot answer. An advertised
    /// context window is adopted here, so it lands before the next request.
    pub fn refresh_models(&mut self) {
        self.models = http::list_models(self.cfg());
        self.adopt_advertised_context();
    }

    /// Adopt a model list that finished fetching on its own thread.
    ///
    /// The first entry is only a guess, and only when nothing has named a
    /// model: that is the one case that sent the fetch in the first place
    /// (finding A9). A model the human picked, or one the startup precedence
    /// stated, is not overwritten by whatever the endpoint lists first — and a
    /// list from an endpoint the human has since left is dropped, because the
    /// picker is about the endpoint in use.
    fn adopt_models(&mut self, endpoint: String, models: Vec<http::Model>) {
        if endpoint != self.cfg().base_url {
            return;
        }
        self.models = models;
        if self.cfg().model.is_empty() {
            match self.models.first().map(|model| model.id.clone()) {
                Some(id) => {
                    self.cell.edit(|cfg| cfg.set_model(&id));
                    self.say(format!(
                        "model: {} · {}",
                        self.cfg().label(),
                        self.context_label()
                    ));
                }
                None => self.fail(format!(
                    "no model given and none discovered at {} — pick one with /model",
                    self.cfg().models_url()
                )),
            }
        }
        // The endpoint's advertised window can only be adopted from a fetch
        // that happened, and this one just did.
        self.adopt_advertised_context();
    }

    /// Take the endpoint's word for the window of the model in use, unless the
    /// human stated one: a model list is metadata, so it is taken as stated —
    /// the clamp and the refusal of a stated window are the cell's.
    fn adopt_advertised_context(&mut self) {
        let advertised = self
            .models
            .iter()
            .find(|model| model.id == self.cfg().model)
            .and_then(|model| model.context);
        if let Some(tokens) = advertised {
            self.cell.learn_context(tokens, WindowSource::Advertised);
        }
    }

    fn open_model_picker(&mut self) {
        if self.models.is_empty() {
            self.refresh_models();
        }
        if self.models.is_empty() {
            self.fail("no models — point at an endpoint with /url or /provider first");
            return;
        }
        let cursor = self
            .models
            .iter()
            .position(|model| model.id == self.cfg().model)
            .unwrap_or(0);
        let items = self
            .models
            .iter()
            .map(|model| match model.context {
                Some(tokens) => format!("{} · {}", model.id, tokens_label(tokens)),
                None => model.id.clone(),
            })
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::Model,
            items,
            cursor,
        });
    }

    /// `/notes`: the lines mush wrote about the focused agent, in full.
    ///
    /// The foot shows at most two of them, so this is the other half of that
    /// cap: a list where a long line is wrapped rather than clipped, which is
    /// what `/help` and a multi-line failure need. Newest last, the same order
    /// the pane reads in, and the cursor opens on the *head of the newest* note
    /// rather than on the list's last row: a note longer than the popup wraps
    /// into many rows, and its last row is the middle of a sentence with the
    /// stamp that says when it happened above the fold (finding T7). One
    /// keypress down from there reads the rest of it.
    fn open_notes_picker(&mut self) {
        let agent = self.tree.focused;
        // Wrapped for the popup this terminal actually paints: the width comes
        // from the same formula `ui::draw_picker` sizes with, so the lines fit
        // the list instead of being clipped by it (`picker_text_width` says
        // what).
        let width = screen::picker_text_width(self.term_width);
        let notes = self.chat.notes_report(agent, session::now_secs(), width);
        if notes.rows.is_empty() {
            self.say(format!("nothing written about #{agent} yet"));
            return;
        }
        self.picker = Some(Picker {
            kind: PickerKind::Notes,
            items: notes.rows,
            cursor: notes.newest,
        });
    }

    fn open_provider_picker(&mut self) {
        let items: Vec<String> = Provider::ALL.iter().map(|p| p.name().to_string()).collect();
        let cursor = items
            .iter()
            .position(|name| Provider::parse(name) == Some(self.cfg().provider))
            .unwrap_or(0);
        self.picker = Some(Picker {
            kind: PickerKind::Provider,
            items,
            cursor,
        });
    }

    fn apply_provider(&mut self, name: &str) {
        let Some(provider) = Provider::parse(name) else {
            self.fail(format!(
                "unknown provider `{name}` — try {}",
                mush_core::provider::names_hint()
            ));
            return;
        };
        self.switch_provider(provider);
        self.refresh_models();
        self.persist_user_config();
        self.say(format!(
            "provider: {} · {}",
            provider.name(),
            self.context_label()
        ));
    }

    /// Point mush at another endpoint, re-deriving the window for it (finding
    /// A5). One write, so no actor sees the new endpoint with the old window.
    fn switch_endpoint(&mut self, url: &str) {
        self.cell.edit(|cfg| {
            cfg.set_base_url(url);
            cfg.rederive_context();
        });
    }

    /// Select a provider: the endpoint it owns, the model mush knows for it,
    /// and the window that goes with both — in one write, so a request cannot
    /// go out against the new provider with the old one's model or window
    /// (finding A5; a window the human stated is kept by `rederive_context`).
    fn switch_provider(&mut self, provider: Provider) {
        self.cell.edit(|cfg| {
            cfg.provider = provider;
            // A provider that owns an endpoint points at it; one that stands for
            // "wherever the human pointed mush" keeps the endpoint already set.
            if provider.spec().switches_endpoint {
                cfg.base_url = provider.default_base_url().to_string();
            }
            let known = cfg.default_models();
            let mut model = cfg.model.clone();
            if !known.contains(&model) {
                if let Some(first) = known.first() {
                    model = first.clone();
                }
            }
            cfg.set_model(&model);
        });
    }

    fn pick(&mut self, kind: PickerKind, item: &str) {
        match kind {
            PickerKind::Model => {
                // The picker labels models with their window; the id is the
                // part before the separator.
                let id = item.split(" · ").next().unwrap_or(item);
                // A new model means a new documented window, unless the human
                // stated one (finding A5).
                self.cell.edit(|cfg| cfg.set_model(id));
                self.adopt_advertised_context();
                self.persist_user_config();
                self.say(format!(
                    "model: {} · {}",
                    self.cfg().label(),
                    self.context_label()
                ));
            }
            PickerKind::Provider => self.apply_provider(item),
            // Nothing to apply: the list is a reading, and `key_picker` closes it
            // on Enter exactly as it does on Esc.
            PickerKind::Notes => {}
        }
    }

    /// Print the exact git commands for an isolated agent's branch. The human
    /// merges in their own IDE — mush never auto-merges.
    /// `/diff` names the command to read the work; `/merge` and `/discard` run
    /// it. mush cannot see a git command the human runs in their own shell, so
    /// the only thing that ever reclaims a worktree and its branch is doing it
    /// here — which is why the pane stayed cluttered with leftovers.
    fn worktree_command(&mut self, verb: Verb, id: AgentId) {
        let (branch, busy, landed) = match self.tree.node(id) {
            None => {
                self.fail(format!("no agent #{id}"));
                return;
            }
            Some(node) => (node.branch.clone(), node.phase.is_busy(), node.landed),
        };
        // A landed agent has nothing left to look at: `land` took its branch
        // with the worktree, so what happened must be asked *before* the branch
        // it no longer has is read — a `let Some(branch)` guard first would
        // answer a landed agent with "has no worktree branch (not isolated)",
        // which is false. Where the work went is the answer, not a `git diff`
        // against a branch that is gone.
        if let Some(landed) = landed {
            self.say(format!(
                "agent #{id} was already {}",
                match landed {
                    Landed::Merged => "merged",
                    Landed::Discarded => "discarded",
                }
            ));
            return;
        }
        let Some(branch) = branch else {
            self.fail(format!("agent #{id} has no worktree branch (not isolated)"));
            return;
        };
        // The read is the one verb that changes nothing, so it goes first: it
        // is the answer to "what would merging this do".
        if verb == Verb::Diff {
            self.paint_diff(id, &branch);
            return;
        }
        if busy {
            // Merging under a running agent would race the commits it is still
            // making, so refuse instead of interleaving with it.
            self.fail(format!(
                "agent #{id} is still running — Ctrl-C stops it before you {} its work",
                verb.name()
            ));
            return;
        }
        let root = self.ws.root().to_path_buf();
        // The path git removes and the path the note names are one string: the
        // core formatter, made relative to the root `-C` already resolves it
        // against, so a discard cannot remove one worktree and report another.
        let worktree = git::worktree_path(&root, id.0);
        let worktree = worktree
            .strip_prefix(&root)
            .unwrap_or(&worktree)
            .to_string_lossy()
            .to_string();
        match verb {
            // Returned above: a read has nothing to land and nothing to
            // reclaim.
            Verb::Diff => {}
            Verb::Merge => match git::run(&root, &["merge", branch.as_str()]) {
                Err(error) => self.fail(format!("merge {branch} failed: {error}")),
                Ok(_) => {
                    // The work is in HEAD now, so reclaim the disk and the
                    // branch. Best-effort: a worktree git refuses to remove is
                    // worth reporting, but the merge — the part that mattered —
                    // already happened.
                    let _ = git::run(&root, &["worktree", "remove", "--force", &worktree]);
                    let branch_note = match git::run(&root, &["branch", "-d", branch.as_str()]) {
                        Ok(_) => format!("{branch} deleted"),
                        Err(error) => format!("branch kept: {error}"),
                    };
                    self.tree.land(id, Landed::Merged);
                    self.chat
                        .note(format!("merged {branch} into HEAD · {branch_note}"));
                    self.refresh_git();
                    self.flush_session();
                }
            },
            Verb::Discard => {
                let removed = git::run(&root, &["worktree", "remove", "--force", &worktree]);
                let deleted = git::run(&root, &["branch", "-D", branch.as_str()]);
                // Say what actually happened: a discard that half-failed must
                // not read like a clean one.
                let mut steps = Vec::new();
                steps.push(match &removed {
                    Ok(_) => format!("removed {worktree}"),
                    Err(error) => format!("worktree kept: {error}"),
                });
                steps.push(match &deleted {
                    Ok(_) => format!("deleted {branch}"),
                    Err(error) => format!("branch kept: {error}"),
                });
                let outcome = steps.join(" · ");
                if removed.is_err() && deleted.is_err() {
                    self.fail(format!("cannot discard agent #{id}: {outcome}"));
                } else {
                    self.tree.land(id, Landed::Discarded);
                    self.chat.note(format!(
                        "discarded agent #{id} — its work is gone · {outcome}"
                    ));
                    self.refresh_git();
                    self.flush_session();
                }
            }
        }
    }

    /// `/diff`: run the diff of an isolated agent's work against HEAD and paint
    /// it, instead of naming the command and leaving the human to type it. The
    /// old answer was the command and nothing else — a transcript whose only
    /// reply to "what did this agent do" was `· git diff HEAD...mush/2`, which
    /// is an instruction, not an answer (finding T9).
    ///
    /// The shapes, because a diff can be enormous:
    ///
    /// * The **bar** gets the one-glance line — the stat, or "nothing changed"
    ///   — because a bar row is one row.
    /// * The **transcript** gets the diff itself — each hunk named by its file
    ///   ([`diff_rows`]), capped at `cmd_cap()` bytes of whole lines, head
    ///   first, with a last row naming the command that reads the rest. That
    ///   is the cap idiom the tool results already keep (`READ_CAP`, `CMD_CAP`,
    ///   and the eight rows one result is painted with): git's output is not
    ///   special, and a branch that touched a lockfile can print more diff than
    ///   every conversation in the session.
    /// * The **model** gets nothing. `/diff` is the human's command: its answer
    ///   goes to `Chat`'s notices, never to the messages that are sent, so no
    ///   tokens are spent and there is no model-facing shape to pick. A model
    ///   that wants a diff has `run_command`.
    ///
    /// An empty diff says so. `branch` at HEAD is a fact about the work —
    /// nothing changed — and silence would leave the human unable to tell it
    /// from a command that did not run.
    fn paint_diff(&mut self, id: AgentId, branch: &str) {
        let root = self.ws.root().to_path_buf();
        let command = format!("git diff HEAD...{branch}");
        // Both names are resolved to commits before git reads them, through the
        // one home `git::resolve` keeps for that rule: a branch name is
        // untrusted input, and one beginning with `-` would be taken by `diff`
        // as an option.
        let (Some(base), Some(tip)) = (git::resolve(&root, "HEAD"), git::resolve(&root, branch))
        else {
            // The branch a node names can be gone — a hand-run `git branch -d`,
            // a worktree git pruned — and the honest answer is git's own, not a
            // diff against a name that does not resolve.
            self.fail(format!(
                "cannot diff {branch}: it does not resolve to a commit — {command}"
            ));
            return;
        };
        let range = format!("{base}...{tip}");
        let stat = match git::run(&root, &["diff", "--shortstat", &range]) {
            Ok(text) => git::parse_shortstat(&text).unwrap_or_default(),
            Err(error) => {
                self.fail(format!("cannot diff {branch}: {error}"));
                return;
            }
        };
        let diff = match git::run(&root, &["diff", &range]) {
            Ok(text) => text,
            Err(error) => {
                self.fail(format!("cannot diff {branch}: {error}"));
                return;
            }
        };
        let summary = format!("#{id} {branch} {}", stat.compact());
        if diff.is_empty() {
            self.say(format!("{summary} — nothing changed"));
            self.chat
                .note(format!("{command} — nothing changed; {branch} is at HEAD"));
            return;
        }
        // The head of the *change*, not of git's boilerplate: a pane spends
        // two rows on this answer, and `diff --git`/`index` are not the answer
        // (finding S8(ii)).
        let rows = diff_rows(&diff);
        let (head, elided) = head_lines(&rows, self.cfg().cmd_cap());
        // The bar already carries the one-glance line, so the transcript is the
        // diff itself rather than the same summary again; a second copy only
        // pushed the change one row further out of the pane's two-row foot.
        let mut note = head;
        if elided > 0 {
            note.push_str(&format!(
                "\n[+{elided} more lines — {command} reads the rest]"
            ));
        }
        self.say(summary);
        self.chat.note(note);
    }

    /// `/compact`: ask the focused agent to fold its conversation into a
    /// summary now, instead of waiting for the window to fill.
    ///
    /// The request goes to the agent, not to its row: the fold itself is the
    /// actor's job, and its `Compact` event is what replaces the transcript
    /// here, saves the session and moves the meter. What the row shows is the
    /// actor's own answer — `Compacting::Parked` the moment it takes a request
    /// it cannot run yet, `Compacting::Requested` while the summarize call is
    /// on the wire (finding U11); the line this writes is the acknowledgement
    /// for the instant before either arrives. A mailbox that is gone is the one
    /// thing the human has to hear, and it is said plainly rather than left as
    /// a status line about work nobody is doing (finding B10).
    fn compact_focused(&mut self) {
        let target = self.tree.focused;
        // The transcript travels with the request, for the root only: an actor
        // restored from a session starts with none, and a fold is not a run, so
        // this is the only hand-over it will ever get (a child is revived with
        // its transcript). Sending it unconditionally would be wrong — the
        // actor's own copy is the newer one while a run is in flight.
        let messages = if target == AgentId::ROOT {
            self.chat.conversation()
        } else {
            Vec::new()
        };
        match self.tree.agent_tx.get(&target) {
            Some(tx) if tx.send(AgentMsg::Compact(messages)).is_ok() => {
                self.say(format!("compacting #{target}…"));
            }
            _ => self.fail(format!("agent #{target} is gone")),
        }
    }

    /// Drop an agent's node and transcript from this session.
    ///
    /// This is deliberately *not* `/discard`: the worktree and any unmerged work
    /// are left alone, so forgetting a live one only means `/worktrees` lists it
    /// again (which is the honest outcome — forgetting is about the
    /// conversation, not the disk).
    fn forget_agent(&mut self, id: AgentId) {
        if id == AgentId::ROOT {
            self.fail("the root agent cannot be forgotten — /new restarts it");
            return;
        }
        let Some(node) = self.tree.node(id) else {
            self.fail(format!("no agent #{id}"));
            return;
        };
        if node.phase.is_busy() {
            self.fail(format!(
                "agent #{id} is still running — Ctrl-C stops it first"
            ));
            return;
        }
        let unmerged = node.branch.clone().filter(|_| node.landed.is_none());
        self.tree.reap(&[id]);
        self.chat.forget(id);
        // Written before this returns: a forgotten agent that came back after a
        // restart would be the worst kind of surprise, and it is one line to
        // prevent.
        self.flush_session();
        match unmerged {
            Some(branch) => self.chat.note(format!(
                "forgot agent #{id} — {branch} is untouched, so /worktrees lists it again"
            )),
            None => self.say(format!("forgot agent #{id}")),
        }
    }

    /// Reset the conversation: stop every actor in the old tree and start a
    /// fresh root, so the new chat has a clean slate and a live mailbox.
    ///
    /// Both `Ctrl-N` and `/new` land here — a chat that is cleared without
    /// restarting the root would leave the actor holding the old transcript
    /// (and a busy flag) while the UI shows an empty one.
    fn new_chat(&mut self) {
        self.stop_all();
        // The respawned root owns its own config cell, conversation tag, and id
        // counter; the UI adopts the handle with the fresh tree, or a later
        // /model would never reach the agent.
        let root = spawn(
            ConfigHandle::own(self.cell.ui().clone()),
            self.ui_tx.clone(),
            self.ws.root().to_path_buf(),
        );
        self.cell.adopt_handle(root.cfg.clone());
        self.tree = AgentTree::rooted(root);
        // Running agents vanish with the old conversation; worktrees they left
        // behind are still reviewable (they are re-listed below).
        self.chat.clear();
        self.spin = 0;
        self.discover_worktrees();
        self.refresh_git();
        // The old conversation is gone from this moment: if the write were left
        // to the debounce, a crash would bring it back with the next start.
        self.flush_session();
        self.say("new chat — agents stopped, root restarted");
    }

    /// Ask every actor in the tree to shut down. `Shutdown`, not `Stop`: a
    /// cancelled actor goes back to waiting for work (which is what Ctrl-C
    /// should do), while `/new` needs the threads to be gone — and an actor
    /// holds its own mailbox open, so it never notices that the UI let go.
    fn stop_all(&self) {
        for tx in self.tree.agent_tx.values() {
            let _ = tx.send(AgentMsg::Shutdown);
        }
    }

    /// Note that what the session stores has changed.
    ///
    /// O(1), because the caller is a message handler on the UI thread: a
    /// streamed response must not rebuild a session, let alone write one. The
    /// mark is deliberately *not* moved by later changes — a stream that never
    /// pauses still reaches the file once per `SESSION_DEBOUNCE` instead of
    /// being deferred until it stops.
    fn mark_session_dirty(&mut self) {
        if self.session_dirty_at.is_none() {
            self.session_dirty_at = Some(Instant::now());
        }
    }

    /// Rebuild the session and hand it to the writer, which serializes it and
    /// writes it on its own thread.
    ///
    /// Called from the debounced tick. The rebuild is the one part of a save
    /// that cannot leave this thread — the conversation lives here — so it is
    /// paid once per `SESSION_DEBOUNCE` rather than once per streamed message,
    /// and never while a burst of them is being drained.
    fn save_session(&mut self) {
        self.session_dirty_at = None;
        let session = self.session_snapshot();
        self.session_save.save(session);
    }

    /// Write the session and wait for the disk: the call sites that mean "this
    /// must not be lost" — the human's own message, a command that changed what
    /// is stored, a compaction, quitting.
    ///
    /// Nothing else waits, which is what keeps the wait off the message path: a
    /// streamed response is covered by the debounce and by the flush on the way
    /// out, so the most a crash can cost is the last `SESSION_DEBOUNCE` of chat.
    fn flush_session(&mut self) {
        self.session_dirty_at = None;
        let session = self.session_snapshot();
        self.session_save.save(session);
        self.session_save.flush();
        if let Some(error) = self.session_save.take_error() {
            self.fail(format!("could not save session: {error}"));
        }
        // A save is a moment the state is being fixed; the repository is part
        // of that picture, so refresh it here rather than leaving the bar with
        // a read older than the file on disk (finding P8). Only the human-
        // driven flushes come through here — a streamed response uses the
        // debounced `save_session` — so this does not put a git process on the
        // message path.
        self.refresh_git();
    }

    /// The conversation as it is stored: the root transcript, every subagent's,
    /// and the endpoint selection it was held against.
    fn session_snapshot(&self) -> Session {
        // Every subagent, not just the root: without this a relaunch forgot
        // each child's context, and "continue that agent" meant writing the
        // brief again from scratch.
        let agents = self
            .tree
            .agents
            .iter()
            .filter(|node| node.id != AgentId::ROOT)
            .map(|node| session::AgentSession {
                id: node.id.0,
                parent: node.parent.map(|parent| parent.0),
                depth: node.depth,
                brief: node.brief.clone(),
                branch: node.branch.clone(),
                status: match &node.phase {
                    Phase::Done => session::StoredStatus::Done,
                    Phase::Stopped => session::StoredStatus::Stopped,
                    Phase::Failed(error) => session::StoredStatus::Failed(error.clone()),
                    // Mid-run at shutdown is not a result; it comes back idle,
                    // which is what it will actually be.
                    _ => session::StoredStatus::Idle,
                },
                landed: node.landed.map(|landed| match landed {
                    Landed::Merged => session::StoredLanded::Merged,
                    Landed::Discarded => session::StoredLanded::Discarded,
                }),
                leftover: node.leftover,
                summary: node.summary.clone(),
                // The system prompt is regenerated on the way back in, since it
                // names a workspace that may have moved.
                messages: self
                    .chat
                    .transcript(node.id)
                    .iter()
                    .filter(|message| message.role != "system")
                    .cloned()
                    .collect(),
            })
            .collect();
        Session {
            root: self.ws.root_str(),
            model: self.cfg().model.clone(),
            provider: self.cfg().provider.name().to_string(),
            base_url: self.cfg().base_url.clone(),
            // Only a window the human stated is worth remembering; a discovered
            // one is re-read next time, so it cannot go stale.
            context: self
                .cfg()
                .context_explicit
                .then_some(self.cfg().context_tokens),
            updated: session::now_secs(),
            messages: self.chat.transcript(AgentId::ROOT).to_vec(),
            agents,
            // A failure is the one line worth coming back to; a command's answer
            // is not (see `Chat::stored_notices`).
            notices: self.chat.stored_notices(),
        }
    }

    // ------------------------------------------------------------------ input

    /// One key: [`keys::key`] decides what it means and [`Self::apply_intent`]
    /// carries the decision out. Nothing from here down reads a `KeyCode`, so a
    /// binding is testable without an `App` — which the old shape, where an arm
    /// both matched a key and did its work, made impossible (finding B2).
    fn on_key(&mut self, key: KeyEvent) {
        let intent = keys::key(self.focus, self.picker.is_some(), key);
        // Below the floor the screen is a single notice: a key whose effect the
        // human cannot see — `Ctrl-N` wipes the conversation and starts a new
        // one — must not act. `Ctrl-Q` is the exception: a terminal too small
        // to read is still a way out. The floor is `App`'s state, not the
        // painter's early return (finding P11 / refactor B3).
        if self.below_floor() && intent != Intent::Quit {
            return;
        }
        self.apply_intent(intent);
    }

    /// Do what an intent says. One arm per intent, every side effect of the
    /// keyboard in one readable list: which pane moves, what is sent, what is
    /// picked, and which config change is remembered.
    fn apply_intent(&mut self, intent: Intent) {
        match intent {
            Intent::Ignore => {}
            Intent::Quit => self.request_quit(),
            Intent::NewChat => self.new_chat(),
            Intent::Interrupt => self.interrupt(),
            Intent::InterruptAll => self.interrupt_all(),
            Intent::OpenModelPicker => self.open_model_picker(),
            Intent::CycleFocus(direction) => self.cycle_focus(direction),
            Intent::PickerClose => self.picker = None,
            Intent::PickerPick => self.pick_cursor(),
            Intent::PickerMove(step) => self.move_picker(step),
            Intent::PickerFirst => self.set_picker_cursor(0),
            Intent::PickerLast => self.set_picker_cursor(usize::MAX),
            Intent::TreeMove(step) => self.move_tree_cursor(step),
            Intent::TreeWalk(direction) => self.tree_walk(direction),
            Intent::TreeFirst => self.tree.cursor_top(),
            Intent::TreeLast => self.tree.cursor_bottom(),
            Intent::TreeFocus => self.focus_cursor_row(),
            Intent::TreeCancel => self.cancel_cursor_row(),
            Intent::TreeBackToRoot => {
                self.tree.focus(AgentId::ROOT);
            }
            Intent::Send => self.send_message(),
            Intent::Chat(key) => self.chat.apply(self.tree.focused, key),
        }
    }

    fn request_quit(&mut self) {
        self.should_quit = true;
    }

    /// Ask one agent's current run to stop: flip the flag its in-flight model
    /// call polls, and leave a Stop in the mailbox for everything else (a
    /// parked wait, a shell command, the next message boundary).
    /// Stop the agent the human is looking at. Ctrl-C used to stop *every*
    /// busy agent at once, which is the wrong default: the agents it killed
    /// were usually the ones already finished and about to report, and their
    /// work was lost with them. Stopping one agent is what the key should do;
    /// stopping the whole tree is `interrupt_all`.
    fn interrupt(&mut self) {
        // What the human is looking at: the focused agent if it is working, else
        // the one agent that is — and "working" includes a detached job, which
        // is work in flight even while its owner naps.
        let busy = self.working_agents();
        let target = if busy.contains(&self.tree.focused) {
            Some(self.tree.focused)
        } else if busy.len() == 1 {
            Some(busy[0])
        } else {
            None
        };
        let Some(id) = target else {
            if busy.is_empty() {
                self.say("nothing running · Ctrl-Q quits · Ctrl-N starts a new chat");
            } else {
                // Several agents are busy and the focused one is not among
                // them: stopping the wrong one silently would be worse than
                // asking, so name the scope instead.
                self.say(format!(
                    "{} agents running · Enter picks one to stop · Ctrl-X stops them all",
                    busy.len()
                ));
            }
            return;
        };
        self.stop_one(id);
    }

    /// Stop every busy agent. The old Ctrl-C, now on its own key: it is the
    /// emergency brake, not the everyday one.
    fn interrupt_all(&mut self) {
        let targets = self.working_agents();
        if targets.is_empty() {
            self.say("nothing running · Ctrl-Q quits · Ctrl-N starts a new chat");
            return;
        }
        let ids = targets.clone();
        for id in targets {
            self.stop_one(id);
        }
        let list = ids
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(", ");
        self.say(format!("stopped {} agents ({list})", ids.len()));
    }

    /// The agents with work in flight: a run, or a job. `Stop` is aimed at the
    /// work, not at the phase, so both count ([`Self::in_flight`]).
    fn working_agents(&self) -> Vec<AgentId> {
        self.tree
            .agents
            .iter()
            .filter(|node| self.in_flight(node))
            .map(|node| node.id)
            .collect()
    }

    /// Ask one agent to stop and show it immediately. A dead mailbox is a gone
    /// actor: mark the row so it stops showing work that can never finish
    /// (finding B6).
    fn stop_one(&mut self, id: AgentId) {
        self.tree.cancel_requested(id);
    }

    fn cycle_focus(&mut self, direction: i64) {
        let order = [Focus::Agents, Focus::Chat];
        let index = match self.focus {
            Focus::Agents => 0,
            Focus::Chat => 1,
        };
        let next = (index as i64 + direction).rem_euclid(order.len() as i64) as usize;
        self.focus = order[next];
        // Looking somewhere new is a moment the human reads the bar, and the
        // git line has no other reason to move: refresh on the transition, or
        // a file written outside mush stays invisible on a screen nobody has
        // touched (finding P8). The read is cheap and `git_in_flight` drops a
        // second ask while the first is out.
        self.refresh_git();
    }

    /// Move the tree's cursor `step` rows — one for `j`/`k`, a whole page for
    /// `PgUp`/`PgDn`, the pair `PickerMove` carries.
    ///
    /// `move_cursor` is the tree's only cursor setter and it moves a *sign*,
    /// one row per call, stopping at either end of the list — so the step is
    /// taken one row at a time, and a page from the first or the last row is an
    /// honest no-op rather than a wrap or an index off the end. Rows are the
    /// painted ones: `rows()` paints every agent exactly once, so the storage
    /// length `move_cursor` clamps against is the length the pane draws, and
    /// the cursor it lands on is a row the human can see.
    fn move_tree_cursor(&mut self, step: i64) {
        for _ in 0..step.abs() {
            self.tree.move_cursor(step.signum());
        }
    }

    /// `←`/`→` in the agents pane: walk the painted rows along the parent links
    /// (finding U10). `direction < 0` selects the selected agent's parent;
    /// `> 0` its first child. Both read the order the pane paints and `j`/`k`
    /// walk, so the cursor lands on the row the human sees (finding U4), and
    /// the parent link — not the row above — is what `←` follows: a later
    /// sibling's row sits directly above a node without being its parent.
    ///
    /// The root has no parent and a leaf has no child, so the cursor stays put:
    /// an honest no-op, chosen over jumping to `g` (the root) or to a sibling,
    /// either of which would move the selection somewhere the human did not
    /// point.
    fn tree_walk(&mut self, direction: i64) {
        let Some(from) = self.tree.cursor_id() else {
            return;
        };
        let target = if direction < 0 {
            self.tree
                .node(from)
                .and_then(|node| node.parent)
                .filter(|parent| self.tree.has(*parent))
        } else {
            // The first child in painted order: `rows` is pre-order, so the
            // first node whose parent is `from` is the one directly under it.
            self.tree
                .rows()
                .iter()
                .find(|node| node.parent == Some(from))
                .map(|node| node.id)
        };
        let Some(target) = target else {
            return;
        };
        if let Some(index) = self.tree.rows().iter().position(|node| node.id == target) {
            // The tree's only cursor setter is `move_cursor`, and it moves a
            // *sign*, one row per call — so the walk takes one step for each row
            // it must cross rather than a second way to write the cursor. The
            // cursor lands on `target` exactly, because `steps` is the gap.
            let steps = index as i64 - self.tree.cursor() as i64;
            for _ in 0..steps.abs() {
                self.tree.move_cursor(steps.signum());
            }
        }
    }

    /// Focus the row the tree's cursor is on, and say whose pane the chat now
    /// shows: `Enter` in the agent pane is a move of the *view*, so the brief
    /// goes to the bar where a human can read it before typing.
    ///
    /// The keyboard moves with the view. `Enter` used to show the agent's
    /// transcript but leave the tree holding the keys, so the next thing the
    /// human typed went to the tree — `g`/`G` jumped the cursor, a `c` in the
    /// message cancelled the agent, and the pane snapped back to the root with
    /// the words nowhere (finding S2). Focus is one value, so moving it moves
    /// the bar's `chat`/`agents` badge, the pane borders and the key table
    /// together.
    fn focus_cursor_row(&mut self) {
        if let Some(id) = self.tree.focus_cursor() {
            self.focus = Focus::Chat;
            let brief = self
                .tree
                .node(id)
                .map(|node| node.brief.clone())
                .unwrap_or_default();
            self.say(format!("agent #{id}: {brief}"));
            // A focus change is a read-the-bar moment; refresh the git line so
            // it is current when the human looks (finding P8).
            self.refresh_git();
        }
    }

    /// `c` on the tree's cursor row: stop it if it has work to stop.
    ///
    /// Stopping an idle agent is not a no-op to be swallowed — the human asked
    /// for something that cannot happen, and the row's phase is left alone
    /// because it has no work in flight to cancel. Ending an agent is `/new`'s
    /// job.
    fn cancel_cursor_row(&mut self) {
        // The painted row under the cursor, not the storage vector: they are
        // different orders (finding U4).
        let Some(id) = self.tree.cursor_id() else {
            return;
        };
        // The same question `working_agents` answers: a run *or* a job is work
        // in flight. An agent whose run ended while its `cargo bench` still runs
        // is not idle on the machine, and the key is aimed at the work — so
        // answering "not running" here, while Ctrl-C stops the very same job,
        // made the two paths disagree about the same agent.
        let working = self.tree.node(id).is_some_and(|node| node.phase.is_busy())
            || !self.tree.live_jobs(id).is_empty();
        if !working {
            self.say(format!("agent #{id} is not running"));
            return;
        }
        // The row's own `⊘` is the feedback; the bar shows what the tree as a
        // whole is doing.
        self.stop_one(id);
    }

    /// Take the picker's selected row: the picker is gone either way, because
    /// `Enter` is a decision even when there is nothing to decide.
    fn pick_cursor(&mut self) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let kind = picker.kind;
        let item = picker.items.get(picker.cursor).cloned();
        self.picker = None;
        if let Some(item) = item {
            self.pick(kind, &item);
        }
    }

    /// Move the picker's cursor, stopping at both ends: a picker is not a
    /// wheel, and wrapping from the last model to the first hides how many
    /// there are.
    fn move_picker(&mut self, step: i64) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        let last = picker.items.len().saturating_sub(1) as i64;
        picker.cursor = (picker.cursor as i64 + step).clamp(0, last) as usize;
    }

    /// Put the picker's cursor on a row, clamped to the list — `usize::MAX` is
    /// the end of it.
    fn set_picker_cursor(&mut self, row: usize) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        picker.cursor = row.min(picker.items.len().saturating_sub(1));
    }
}

impl Drop for App {
    /// The exit flush: whatever the debounce had not written yet goes out here,
    /// so quitting — the one way out of the event loop — costs nothing. This is
    /// what bounds a crash to `SESSION_DEBOUNCE` of streamed chat rather than to
    /// everything since the last boundary. A failure here is reported the usual
    /// way and then lost with the status line: there is no screen left to read
    /// it on.
    fn drop(&mut self) {
        if self.session_dirty_at.is_some() {
            self.flush_session();
        }
        // Jobs die with mush itself. Each one is a process group, and a build an
        // agent started used to outlive a clean quit — the human's next `cargo
        // build` then fought a ghost for the target directory. Killing here,
        // on the way out of the process, is the last moment it can happen.
        self.tree.handles().jobs.kill_all();
    }
}

/// The head of a long text, in whole lines and at most `max` bytes, with how
/// many lines were left out.
///
/// Whole lines because a diff is read as rows: half a hunk header is not a
/// shorter diff, it is a broken one. Bytes because that is the cap the other
/// tool results keep ([`Config::cmd_cap`]), which is what makes `/diff`'s answer
/// the same size of thing as a `run_command` result instead of a rule of its
/// own. A single line longer than the whole cap is cut at a char boundary — one
/// minified file is one line, and it must not be able to fill the transcript
/// either.
fn head_lines(text: &str, max: usize) -> (String, usize) {
    /// The longest prefix that is at most `max` bytes and ends on a char
    /// boundary: a truncated UTF-8 glyph in a transcript is worse than a
    /// shorter row.
    fn char_head(text: &str, max: usize) -> &str {
        if text.len() <= max {
            return text;
        }
        let mut end = max;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }

    let lines: Vec<&str> = text.lines().collect();
    let mut kept = String::new();
    for line in &lines {
        // The newline between two kept rows is part of the budget: `max` is a
        // byte count of what is handed on, not of the rows' text alone.
        let next = line.len() + usize::from(!kept.is_empty());
        if kept.len() + next > max {
            break;
        }
        if !kept.is_empty() {
            kept.push('\n');
        }
        kept.push_str(line);
    }
    if kept.is_empty() && !lines.is_empty() {
        kept = char_head(lines[0], max).to_string();
    }
    let kept_lines = kept.lines().count();
    (kept, lines.len().saturating_sub(kept_lines))
}

/// git's diff, with each file's preamble folded into the hunks that belong to
/// it: `more.txt @@ -0,0 +1,2 @@`.
///
/// A pane spends two rows on a `/diff` answer, and git's first two rows are
/// always the same boilerplate — `diff --git a/x b/x`, then `index …` — so the
/// change itself was never on screen without `/notes` (finding S8(ii)). The
/// headline is the answer instead: the file, the hunk's line numbers, then the
/// lines. A file with no hunk (`Binary files … differ`, a mode-only change)
/// keeps a line of its own so it is still named.
fn diff_rows(diff: &str) -> String {
    /// Whether a line is git's per-file preamble: identity and mode, not a
    /// change. Only dropped before the file's first `@@`, because a hunk's own
    /// content can itself be a line beginning `---` (a removed line of `--`) or
    /// `+++` (an added line of `++`).
    fn preamble(line: &str) -> bool {
        [
            "index ",
            "new file mode ",
            "deleted file mode ",
            "old mode ",
            "new mode ",
            "similarity index ",
            "dissimilarity index ",
            "rename from ",
            "rename to ",
            "copy from ",
            "copy to ",
            "--- ",
            "+++ ",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
    }
    /// The b-side path of git's `a/x b/x`, which is the file the hunk is in.
    fn path_of(after: &str) -> String {
        match after.rsplit_once(" b/") {
            Some((_, b)) => b.to_string(),
            None => after.strip_prefix("b/").unwrap_or(after).to_string(),
        }
    }

    let mut rows: Vec<String> = Vec::new();
    let mut path = String::new();
    // A file named but not yet spoken for: it is kept by name when nothing else
    // of it survives (a mode-only change).
    let mut pending = false;
    let mut preamble_open = false;
    for line in diff.lines() {
        if let Some(after) = line.strip_prefix("diff --git ") {
            if pending {
                rows.push(path.clone());
            }
            path = path_of(after);
            pending = true;
            preamble_open = true;
            continue;
        }
        if line.starts_with("@@") {
            rows.push(if path.is_empty() {
                line.to_string()
            } else {
                format!("{path} {line}")
            });
            pending = false;
            preamble_open = false;
            continue;
        }
        if preamble_open && preamble(line) {
            continue;
        }
        // Anything outside the preamble is kept as it is: `Binary files …`,
        // `GIT binary patch`, `\ No newline at end of file`.
        rows.push(line.to_string());
        pending = false;
        preamble_open = false;
    }
    if pending {
        rows.push(path);
    }
    rows.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crossbeam_channel::Receiver;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::layout::Rect;
    use ratatui::widgets::{Block, Borders};
    use ratatui::Terminal;

    use crate::session_save;
    use crate::session_save::SessionSave;

    /// Run a typed line the way a send does: through the pure parser, then the
    /// arms. A test that names a command string this way exercises both, so the
    /// parser cannot be right about an argument the executor reads differently
    /// (finding B2).
    fn run(app: &mut App, line: &str) {
        let command = commands::parse_command(line)
            .unwrap_or_else(|error| panic!("`{line}` is not a command: {error}"));
        app.apply_command(command);
    }

    /// The transient line the bar would show, or the empty string.
    fn text_of(app: &App) -> &str {
        app.status_line().map(|(text, _)| text).unwrap_or("")
    }

    /// Pretend a status line was written `seconds` ago.
    fn age_status(app: &mut App, seconds: u64) {
        if let Some(status) = app.status.as_mut() {
            status.set_at = Instant::now() - Duration::from_secs(seconds);
        }
    }

    /// A real `App` on a scratch directory, with a real (idle) root actor. The
    /// returned receiver keeps the UI channel alive for the life of the test.
    /// A real repository, because `/merge`, `/discard` and worktree discovery
    /// all shell out to git — a fake would test nothing they actually do.
    fn repo(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mush-land-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["-c", "init.defaultBranch=master", "init", "-q"]);
        git(&dir, &["config", "user.email", "mush@test"]);
        git(&dir, &["config", "user.name", "mush"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "init"]);
        dir
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The fixture every `App` test starts from: a real workspace at `root`, an
    /// endpoint that answers nothing (`127.0.0.1:1`), and the session store the
    /// caller chose. One construction, so the twelve call sites cannot drift
    /// from what `App::new` takes.
    fn app_root(
        root: &std::path::Path,
        stored: Option<Session>,
        save: Arc<dyn SessionSave>,
    ) -> (App, Receiver<Msg>) {
        let ws = Workspace::new(root).unwrap();
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let cell = ConfigCell::own(cfg);
        let handle = spawn(cell.handle(), tx.clone(), root.to_path_buf());
        let app = App::new(ws, cell, stored, handle, tx, save);
        (app, rx)
    }

    fn app_at(root: std::path::PathBuf) -> App {
        app_root(&root, None, session_save::fake::Recorder::new()).0
    }

    /// An `App` with the UI channel kept, so a read that finishes on its own
    /// thread (`Msg::Git`) can be waited for instead of raced.
    fn app_and_rx(root: std::path::PathBuf) -> (App, Receiver<Msg>) {
        app_root(&root, None, session_save::fake::Recorder::new())
    }

    /// Adopt the next `Msg::Git` that arrives within the deadline, applying any
    /// message in front of it. A read that never comes back is a test failure,
    /// so this panics rather than proceeding on a stale snapshot.
    fn wait_git(app: &mut App, rx: &Receiver<Msg>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(msg) => {
                    let is_git = matches!(msg, Msg::Git { .. });
                    app.update(msg);
                    if is_git {
                        return;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        panic!("no git read came back");
    }

    /// The git line is refreshed on the transitions a human drives — a focus
    /// change and a command — so a file written outside mush does not leave the
    /// bar asserting a clean tree indefinitely (finding P8). The snapshot only
    /// moved on agent events and on a tick while something ran.
    #[test]
    fn a_focus_change_and_a_command_refresh_the_git_read() {
        let root = repo("refresh-git");
        let (mut app, rx) = app_and_rx(root.clone());
        wait_git(&mut app, &rx);
        assert_eq!(
            app.git.as_ref().map(|g| g.dirty),
            Some(0),
            "clean at the first read"
        );

        // A file written outside mush, after the read.
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        app.cycle_focus(1);
        wait_git(&mut app, &rx);
        assert_eq!(
            app.git.as_ref().map(|g| g.dirty),
            Some(1),
            "a focus change re-reads the repository"
        );

        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        app.apply_command(Command::Context(None));
        wait_git(&mut app, &rx);
        assert_eq!(
            app.git.as_ref().map(|g| g.dirty),
            Some(2),
            "a command re-reads the repository"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Over-full is a real state — a learned window smaller than the transcript
    /// already held — and the meter must not print a ratio greater than one
    /// with no mark (finding P9).
    #[test]
    fn the_context_meter_says_full_and_over_at_the_window() {
        let (mut app, _rx) = test_app("meter-full");
        // The smallest window the cell will hold, 1,024 tokens.
        app.cell.edit(|cfg| cfg.set_context(1_024));
        app.chat.insert(&"z".repeat(3_000));
        app.send_message();
        let used = app.context_used_tokens();
        assert!(used > 1_024, "the message passed the window: {used}");
        let over = app.context_meter();
        assert!(over.ends_with(" over"), "{over}");

        // Exactly at the window is `full`, not an over-full ratio.
        app.cell.edit(|cfg| cfg.set_context(used));
        let full = app.context_meter();
        assert!(full.ends_with(" full"), "{full}");
        assert!(!full.contains(" over"), "{full}");
    }

    /// `/worktrees` asks about disk, not only about the leftovers the session
    /// already knows: a worktree `git worktree list` names but whose branch is
    /// not `mush/<id>` was invisible to `discover_worktrees`, and the message
    /// then said there were none while the directory sat there (finding P10).
    #[test]
    fn worktrees_reports_what_is_on_disk_even_when_it_cannot_name_it() {
        let root = repo("wt-truth");
        git(
            &root,
            &["worktree", "add", "-q", "-b", "scratch", ".mush/wt/1"],
        );
        let mut app = app_at(root.clone());
        assert_eq!(
            app.tree.agents.iter().filter(|n| n.leftover).count(),
            0,
            "a hand-named branch is not adopted as a leftover"
        );

        run(&mut app, "/worktrees");
        let line = text_of(&app).to_string();
        assert!(
            line.contains("1 worktree(s) under .mush/wt on disk"),
            "{line}"
        );
        assert!(line.contains("0 registered as leftovers"), "{line}");
        assert!(!line.contains("no worktrees"), "{line}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A worktree mush can name is registered, and `/worktrees` says so both
    /// ways: what is on disk and what the commands can act on.
    #[test]
    fn worktrees_reports_a_registered_leftover() {
        let root = repo("wt-reg");
        isolated_work(&root, 3, "port the parser module");
        let mut app = app_at(root.clone());
        run(&mut app, "/worktrees");
        let line = text_of(&app).to_string();
        assert!(
            line.contains("1 worktree(s) under .mush/wt on disk"),
            "{line}"
        );
        assert!(line.contains("all registered"), "{line}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Below the floor the screen is one notice, so a key whose effect the
    /// human cannot see must not act: `Ctrl-N` used to wipe the conversation
    /// and start a new one from a screen showing only the floor message
    /// (finding P11 / refactor B3).
    #[test]
    fn the_floor_refuses_keys_except_quit() {
        let (mut app, _rx) = test_app("floor-keys");
        app.chat
            .push_message(AgentId::ROOT, Message::user("keep me"));
        app.set_term_size(30, 8);
        assert!(app.below_floor());

        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(text_of(&app), "", "Ctrl-N did not start a new chat");
        assert!(
            app.chat
                .conversation()
                .iter()
                .any(|m| m.content.as_deref() == Some("keep me")),
            "the conversation survives"
        );

        // A paste has no box on screen to land in either.
        app.update(Msg::Paste("typed while tiny".to_string()));
        assert!(!app.chat.input().text().contains("typed while tiny"));

        // Quit still works: a terminal too small to read is still a way out.
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        )));
        assert!(app.should_quit);

        // Grown back, the keymap drives the app again.
        let (mut app, _rx) = test_app("floor-grown");
        app.chat
            .push_message(AgentId::ROOT, Message::user("keep me"));
        app.set_term_size(30, 8);
        app.set_term_size(120, 32);
        assert!(!app.below_floor());
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
        )));
        assert!(
            app.chat
                .conversation()
                .iter()
                .all(|m| m.content.as_deref() != Some("keep me")),
            "Ctrl-N runs once the terminal is big enough"
        );
    }

    /// At the ubiquitous 80×24 the facts line carries the branch, the dirty
    /// count and the delta. The bar needed 26 rows for its second line, so at 24
    /// the doc's "always in view" was simply false (finding P12).
    #[test]
    fn the_facts_line_survives_at_80x24() {
        let (mut app, _rx) = test_app("facts-24");
        app.git = Some(git::RepoStatus {
            branch: "main".to_string(),
            dirty: 3,
            stat: git::Stat {
                files: 1,
                added: 12,
                removed: 4,
            },
        });
        app.git_at = Some(Instant::now());
        let rows = screen(&mut app, 80, 24);
        let facts = rows.last().unwrap();
        assert!(
            facts.contains("main") && facts.contains("±3") && facts.contains("+12−4"),
            "the facts row is missing the repository: {facts}"
        );
    }

    /// In compact mode the pane still owes the selected row a footer: the
    /// worktree and the commands to land it, which its row had to drop. Under
    /// six inner rows the footer used to vanish entirely (finding P12).
    #[test]
    fn a_compact_pane_pays_the_selected_row_a_footer() {
        let (mut app, _rx) = test_app("compact-footer");
        let conversation = app.tree.conversation();
        for id in 1..=4u64 {
            app.update(Msg::Agent {
                conversation,
                id: AgentId::ROOT,
                event: AgentEvent::Spawned {
                    child: id,
                    parent: 0,
                    brief: format!("task {id}"),
                    depth: 1,
                    branch: Some(format!("mush/{id}")),
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }
        app.tree.move_cursor(1);
        let rows = screen(&mut app, 60, 17);
        assert!(
            rows.iter().any(|row| row.contains(".mush/wt/1")),
            "the selected row's worktree is on screen: {rows:?}"
        );
    }

    /// A pane smaller than the tree says how many rows it is hiding and which
    /// side they are on: nineteen agents in a four-row pane hid fifteen with
    /// nothing on screen saying so (finding P12).
    #[test]
    fn a_pane_smaller_than_the_tree_names_the_hidden_rows() {
        let (mut app, _rx) = test_app("hidden-rows");
        crowd(&mut app, 19);
        let title = screen(&mut app, 60, 17)[0].clone();
        assert!(title.contains('▼'), "rows below are named: {title}");

        app.tree.cursor_bottom();
        let title = screen(&mut app, 60, 17)[0].clone();
        assert!(title.contains('▲'), "at the bottom they are above: {title}");
    }

    /// An `App` whose session writes go to a real writer on a real path, for
    /// the tests that read the file back. The writer is returned so a test can
    /// see how many writes the conversation cost.
    fn app_writing(root: &std::path::Path) -> (App, Arc<session_save::Writer>) {
        let writer = Arc::new(session_save::Writer::new(root.to_path_buf()));
        let (app, _rx) = app_root(root, None, writer.clone());
        (app, writer)
    }

    /// An `App` on a scratch directory whose saves are recorded instead of
    /// written, so a test sees what the UI thread handed over and when.
    fn app_recording(label: &str) -> (App, Arc<session_save::fake::Recorder>) {
        let recorder = session_save::fake::Recorder::new();
        let (app, _rx) = app_root(&dir(label), None, recorder.clone());
        (app, recorder)
    }

    /// An empty directory for a session to be written into.
    fn dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mush-save-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A real `App` that adopted what is on disk in `root`, the way a restart
    /// does.
    fn reopened(root: &std::path::Path) -> App {
        let (app, _rx) = app_root(
            root,
            Session::load(root),
            session_save::fake::Recorder::new(),
        );
        app
    }

    /// One painted frame: the `Screen` `App` derived and the grid it was
    /// painted into, cell by cell and borders included.
    ///
    /// The layout tiers, the panes and the foot are only true together, which
    /// is why the audit that found these defects read rows instead of reasoning
    /// about them — and why the sweep reads the `Screen` too: a rect is a
    /// promise about where the words are allowed to be.
    struct Shot {
        screen: Screen,
        /// The frame, cell by cell: not the rows joined, because a cell can hold
        /// a grapheme of more than one character and a column is a column.
        cells: Vec<Vec<String>>,
    }

    impl Shot {
        /// One row, as it is read.
        fn line(&self, y: u16) -> String {
            self.cells[y as usize].concat()
        }

        /// The rows as a test reads them, with a pane's border stripped.
        fn rows(&self) -> Vec<String> {
            self.cells
                .iter()
                .map(|row| row.concat().trim_matches('│').trim_end().to_string())
                .collect()
        }

        /// The rows as a terminal *shows* them: the column after a wide glyph
        /// belongs to that glyph, so the blank the buffer keeps there is not a
        /// space of its own (finding B9's arithmetic, read back).
        fn shown(&self) -> Vec<String> {
            self.cells
                .iter()
                .map(|row| {
                    let mut out = String::new();
                    let mut covered = 0usize;
                    for cell in row {
                        if covered > 0 {
                            covered -= 1;
                            continue;
                        }
                        out.push_str(cell);
                        covered =
                            unicode_width::UnicodeWidthStr::width(cell.as_str()).saturating_sub(1);
                    }
                    out
                })
                .collect()
        }

        /// The whole frame as one string, for a `contains` assertion.
        fn text(&self) -> String {
            (0..self.cells.len() as u16)
                .map(|y| self.line(y))
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// The cells inside a rect, row by row.
        fn inside(&self, rect: Rect) -> Vec<String> {
            (rect.y..rect.y.saturating_add(rect.height))
                .map(|y| {
                    (rect.x..rect.x.saturating_add(rect.width))
                        .map(|x| self.cell(x, y))
                        .collect::<String>()
                })
                .collect()
        }

        fn cell(&self, x: u16, y: u16) -> String {
            self.cells
                .get(y as usize)
                .and_then(|row| row.get(x as usize))
                .cloned()
                .unwrap_or_default()
        }

        /// Every rect a pane owns, the popup included.
        fn pane_rects(&self) -> Vec<Rect> {
            match &self.screen {
                Screen::Floor { .. } => Vec::new(),
                Screen::Panes(panes) => {
                    let mut rects = vec![
                        panes.agents.area,
                        panes.chat.transcript_area,
                        panes.chat.input_area,
                        panes.bar.area,
                    ];
                    if let Some(picker) = &panes.picker {
                        rects.push(picker.area);
                    }
                    rects
                }
            }
        }

        /// Every rule a frame must keep whatever it says: nothing painted
        /// outside a pane, every pane's own frame intact, the bar keeping its
        /// row, and every line a pane was handed fitting the pane it is painted
        /// in.
        fn assert_shape(&self, case: &str, width: u16, height: u16) {
            let at = format!("{case} at {width}×{height}");
            let Screen::Panes(panes) = &self.screen else {
                panic!("{at}: the floor notice has no panes to check");
            };
            let rects = self.pane_rects();
            let holds = |rect: &Rect, x: u16, y: u16| {
                (rect.x..rect.x + rect.width).contains(&x)
                    && (rect.y..rect.y + rect.height).contains(&y)
            };

            // Nothing is painted outside a pane: the frame is blank everywhere
            // else, so a cell that is not is a widget that spilled over its
            // rect — or a pane whose rect is not where it paints.
            for (y, row) in self.cells.iter().enumerate() {
                for (x, cell) in row.iter().enumerate() {
                    if cell == " " {
                        continue;
                    }
                    assert!(
                        rects.iter().any(|rect| holds(rect, x as u16, y as u16)),
                        "{at}: `{cell}` at {x},{y} is outside every pane"
                    );
                }
            }

            // Every pane's own frame is intact — a border that moved, vanished
            // or was painted over is a pane whose neighbour is wrong. The cells
            // a popup covers are skipped: `Clear` erases what is under it on
            // purpose.
            let covered = panes.picker.as_ref().map(|picker| picker.area);
            for rect in [
                panes.agents.area,
                panes.chat.transcript_area,
                panes.chat.input_area,
            ] {
                // The sides, from just under the top border to just above the
                // bottom one: the top row carries the pane's title, which may
                // reach either corner.
                for y in rect.y + 1..rect.y + rect.height.saturating_sub(1) {
                    for x in [rect.x, rect.x + rect.width - 1] {
                        if covered.is_some_and(|covered| holds(&covered, x, y)) {
                            continue;
                        }
                        assert_eq!(self.cell(x, y), "│", "{at}: the border at {x},{y}");
                    }
                }
                for x in rect.x..rect.x + rect.width {
                    let y = rect.y + rect.height - 1;
                    if covered.is_some_and(|covered| holds(&covered, x, y)) {
                        continue;
                    }
                    assert!(
                        "─└┘".contains(&self.cell(x, y)),
                        "{at}: the bottom border at {x},{y}: {:?}",
                        self.cell(x, y)
                    );
                }
            }
            if let Some(picker) = &panes.picker {
                let rect = picker.area;
                for y in rect.y + 1..rect.y + rect.height.saturating_sub(1) {
                    for x in [rect.x, rect.x + rect.width - 1] {
                        assert_eq!(self.cell(x, y), "│", "{at}: the popup's border at {x},{y}");
                    }
                }
                for x in rect.x..rect.x + rect.width {
                    let y = rect.y + rect.height - 1;
                    assert!(
                        "─└┘".contains(&self.cell(x, y)),
                        "{at}: the popup's bottom border at {x},{y}: {:?}",
                        self.cell(x, y)
                    );
                }
            }

            // The bar keeps its row on every terminal: the focus badge and the
            // line that is the only home an Info line, a failure or a command's
            // usage error has. It was the trailing constraint once, and at 40×10
            // it was simply not painted.
            let bar = self
                .inside(Rect {
                    height: 1,
                    ..panes.bar.area
                })
                .join("");
            assert!(
                bar.contains(" chat ") || bar.contains(" agents "),
                "{at}: the bar lost its badge: {bar:?}"
            );
            assert!(
                bar.trim().chars().count() > " chat ".len(),
                "{at}: the bar is a badge and nothing else: {bar:?}"
            );

            // Every row the agents pane was handed fits the columns it is
            // painted in: `fit_row`'s promise, read at the width the row is
            // really painted at, and a row that loses its tail to the renderer
            // is the defect this reads for (R1).
            let rows_area = Block::default()
                .borders(Borders::ALL)
                .inner(panes.agents.area);
            for row in &panes.agents.rows {
                let line = crate::ui::agent_line(row, rows_area.width as usize);
                assert!(
                    unicode_width::UnicodeWidthStr::width(line.as_str())
                        <= rows_area.width as usize,
                    "{at}: an agent row is wider than its pane: {line:?}"
                );
            }

            // Every line a pane was handed fits the pane it is painted in. A
            // wider line is clipped silently by the renderer, which is how a
            // wrapped message loses its tail.
            let room = Block::default()
                .borders(Borders::ALL)
                .inner(panes.chat.transcript_area);
            if let Some(painted) = &panes.chat.transcript {
                for line in &painted.lines {
                    assert!(
                        line.width() <= room.width as usize,
                        "{at}: a transcript line is wider than its pane: {line:?}"
                    );
                }
            }
            let field = Block::default()
                .borders(Borders::ALL)
                .inner(panes.chat.input_area);
            if let Some(input) = &panes.chat.input {
                let prompt = unicode_width::UnicodeWidthStr::width(input.prompt.as_str());
                for line in &input.lines {
                    assert!(
                        prompt + unicode_width::UnicodeWidthStr::width(line.as_str())
                            <= field.width as usize,
                        "{at}: a message-box line is wider than its pane: {line:?}"
                    );
                }
            }
        }
    }

    /// Paint one frame at a real terminal size, the way `main` does: the size
    /// the next frame (and any `/notes` opened between frames) sees, one
    /// `Screen` derived for the frame's own area, and it painted into a
    /// `TestBackend`.
    fn shot(app: &mut App, width: u16, height: u16) -> Shot {
        app.set_term_size(width, height);
        let screen = app.screen(Rect::new(0, 0, width, height));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &screen))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let cells = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect();
        Shot { screen, cells }
    }

    /// One painted frame, cell by cell and borders included: a leak lands *on*
    /// a border column, which is the one thing `screen` trims away.
    fn frame_grid(app: &mut App, width: u16, height: u16) -> Vec<String> {
        shot(app, width, height).rows()
    }

    /// No cell may carry a character that commands the display instead of being
    /// read: a control byte, a carriage return, an escape, or a bidi isolate.
    /// Untrusted words — a model reply, a tool result, an endpoint's error body,
    /// a model id, a job's command — reach every surface on this screen, so this
    /// is asserted over the *painted frame*, borders included, and never over
    /// the text those words came from (findings U9/N3).
    fn assert_no_command(at: &str, shot: &Shot) {
        for (y, row) in shot.cells.iter().enumerate() {
            for (x, cell) in row.iter().enumerate() {
                for ch in cell.chars() {
                    assert!(
                        !ch.is_control()
                            && !matches!(
                                ch,
                                '\u{202a}'..='\u{202e}'
                                    | '\u{2066}'..='\u{2069}'
                                    | '\u{200e}'
                                    | '\u{200f}'
                            ),
                        "{at}: {ch:?} at {x},{y} commands the display"
                    );
                }
            }
        }
    }

    /// The painted screen, row by row, at a real terminal size. The layout
    /// tiers, the panes and the foot are only true together, which is why the
    /// audit that found these defects read rows instead of reasoning about
    /// them. A pane's border is stripped: what a test reads is the row's text.
    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        shot(app, width, height).rows()
    }

    /// The painted rows that wear the agents pane's selection highlight — the
    /// cyan background `List` gives the cursor row. `screen` reads text only,
    /// and after the `› ` marker went the selection *is* that style, so this is
    /// how a test still reads which row the pane paints as selected. The bar is
    /// excluded because its focus badge wears the same cyan and is not a row.
    fn selected_rows(app: &mut App, width: u16, height: u16) -> Vec<usize> {
        app.set_term_size(width, height);
        let screen = app.screen(Rect::new(0, 0, width, height));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &screen))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let bar_rows = if height >= 24 { 2 } else { 1 };
        (0..(height - bar_rows) as usize)
            .filter(|&y| {
                (0..width as usize)
                    .any(|x| buffer[(x as u16, y as u16)].bg == ratatui::style::Color::Cyan)
            })
            .collect()
    }

    /// What the chat pane paints, and only it: the agents pane is the columns
    /// to its left at this size.
    fn chat_rows(app: &mut App) -> Vec<String> {
        screen(app, 120, 32)
            .into_iter()
            .map(|row| row.chars().skip(31).collect())
            .collect()
    }

    /// One streamed message from the root, the way its actor sends them.
    fn streamed(app: &mut App, text: &str) {
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Message(Message::assistant(text)),
        });
    }

    /// Pretend the session became dirty `elapsed` ago, so the debounce is
    /// tested instead of waited out.
    fn age_session(app: &mut App, elapsed: Duration) {
        if app.session_dirty_at.is_some() {
            app.session_dirty_at = Some(Instant::now() - elapsed);
        }
    }

    /// An isolated agent's worktree with one commit on it, committed exactly the
    /// way the actor commits (so the subject is real, not test-shaped).
    fn isolated_work(root: &std::path::Path, id: u64, brief: &str) {
        let worktree = git::worktree_path(root, id);
        std::fs::create_dir_all(root.join(".mush")).unwrap();
        git(
            root,
            &[
                "worktree",
                "add",
                "-q",
                worktree.to_str().unwrap(),
                "-b",
                &git::branch_name(id),
            ],
        );
        std::fs::write(worktree.join("work.txt"), "the work\n").unwrap();
        git(&worktree, &["add", "-A"]);
        git(
            &worktree,
            &[
                "commit",
                "-qm",
                &crate::agent::commit_subject(id, brief, &agent::Outcome::Finished("ok".into())),
            ],
        );
    }

    /// A worktree left on disk is identified by what its commit says, not by a
    /// placeholder: the row must show the task the agent was actually given.
    #[test]
    fn a_leftover_worktree_recovers_its_brief_from_git() {
        let root = repo("recover");
        isolated_work(&root, 3, "port the parser module");
        let app = app_at(root.clone());

        let node = app
            .tree
            .agents
            .iter()
            .find(|node| node.id == AgentId(3))
            .expect("the worktree must be registered");
        assert_eq!(node.brief, "port the parser module");
        assert!(node.leftover);
        assert_eq!(node.branch.as_deref(), Some("mush/3"));
        assert_eq!(node.phase, Phase::Done, "the subject says it finished");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A branch mush did not commit to has no subject to read, so it keeps the
    /// placeholder rather than inventing a task.
    #[test]
    fn a_hand_made_branch_keeps_the_placeholder() {
        let root = repo("hand-made");
        isolated_work(&root, 5, "first");
        let worktree = root.join(".mush/wt/5");
        std::fs::write(worktree.join("more.txt"), "more\n").unwrap();
        git(&worktree, &["add", "-A"]);
        git(&worktree, &["commit", "-qm", "my own commit message"]);

        let app = app_at(root.clone());
        let node = app
            .tree
            .agents
            .iter()
            .find(|node| node.id == AgentId(5))
            .unwrap();
        assert_eq!(node.brief, "leftover worktree");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `/merge` runs git for real: the work lands in HEAD, and the worktree and
    /// the branch — the clutter — are reclaimed, which is the whole reason the
    /// pane kept filling up.
    #[test]
    fn merging_an_agent_lands_the_work_and_reclaims_the_worktree() {
        let root = repo("merge");
        isolated_work(&root, 1, "add the parser");
        let mut app = app_at(root.clone());

        run(&mut app, "/merge 1");

        assert!(root.join("work.txt").exists(), "the work is in HEAD now");
        assert!(
            !root.join(".mush/wt/1").exists(),
            "the worktree is reclaimed"
        );
        let branches = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["branch", "--list", "mush/1"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "the branch is reclaimed"
        );
        let node = app
            .tree
            .agents
            .iter()
            .find(|node| node.id == AgentId(1))
            .unwrap();
        assert_eq!(node.landed, Some(Landed::Merged));
        assert!(
            node.branch.is_none(),
            "the branch git deleted must leave the row with it"
        );
        // A second /merge must not re-run git or claim a second merge. It says
        // what happened — `land` cleared the branch, so a landed agent must be
        // asked first, or this reports a missing branch that was never missing.
        run(&mut app, "/merge 1");
        assert!(
            text_of(&app).contains("was already merged"),
            "a second /merge reports the merge, not a branch: {}",
            text_of(&app)
        );
        // `/diff` too: it used to name a branch git had already deleted.
        run(&mut app, "/diff 1");
        assert!(
            text_of(&app).contains("was already merged") && !text_of(&app).contains("git diff"),
            "/diff must not offer a branch that is gone: {}",
            text_of(&app)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `/discard` throws the work away on purpose and says so.
    #[test]
    fn discarding_an_agent_removes_the_worktree_and_the_branch() {
        let root = repo("discard");
        isolated_work(&root, 2, "throwaway");
        let mut app = app_at(root.clone());

        run(&mut app, "/discard 2");

        assert!(!root.join(".mush/wt/2").exists());
        assert!(!root.join("work.txt").exists(), "the work did not land");
        let node = app
            .tree
            .agents
            .iter()
            .find(|node| node.id == AgentId(2))
            .unwrap();
        assert_eq!(node.landed, Some(Landed::Discarded));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A nudge to a landed agent is refused, not run into the reclaimed path
    /// (finding S1). Landing already took the worktree and the branch, so the
    /// run would recreate `.mush/wt/N` as a plain directory nothing can show,
    /// diff or land; the words stay in the box and tell the same story the row,
    /// the footer and `/diff`/`/merge`/`/worktrees` do.
    #[test]
    fn a_nudge_to_a_merged_agent_is_refused_and_writes_nothing() {
        let root = repo("nudge-merged");
        isolated_work(&root, 1, "add the parser");
        let mut app = app_at(root.clone());
        run(&mut app, "/merge 1");

        app.tree.focus(AgentId(1));
        app.chat.insert("write extra.txt");
        app.send_message();

        assert!(
            text_of(&app).contains("agent #1 was merged — its worktree is gone"),
            "the refusal says what happened: {}",
            text_of(&app)
        );
        assert_eq!(
            app.chat.input().text(),
            "write extra.txt",
            "the words are still in the box: nothing ran"
        );
        assert!(
            !app.chat
                .transcript(AgentId(1))
                .iter()
                .any(|message| message.text().contains("write extra.txt")),
            "a refused message is not a message"
        );
        assert!(
            !root.join(".mush/wt/1").exists(),
            "the reclaimed path was not recreated"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same for `/discard`: its work is gone on purpose, and typing must
    /// not bring the path back.
    #[test]
    fn a_nudge_to_a_discarded_agent_is_refused_and_writes_nothing() {
        let root = repo("nudge-discarded");
        isolated_work(&root, 2, "throwaway");
        let mut app = app_at(root.clone());
        run(&mut app, "/discard 2");

        app.tree.focus(AgentId(2));
        app.chat.insert("write extra.txt");
        app.send_message();

        assert!(
            text_of(&app).contains("agent #2 was discarded — its worktree is gone"),
            "the refusal says what happened: {}",
            text_of(&app)
        );
        assert!(
            !app.chat
                .transcript(AgentId(2))
                .iter()
                .any(|message| message.text().contains("write extra.txt")),
            "a refused message is not a message"
        );
        assert!(
            !root.join(".mush/wt/2").exists(),
            "the reclaimed path was not recreated"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Landing work under a *running* agent would race the commits it is still
    /// making, so both commands refuse instead of interleaving with it.
    #[test]
    fn a_running_agent_refuses_to_be_merged_or_discarded() {
        let root = repo("busy");
        isolated_work(&root, 4, "still working");
        let mut app = app_at(root.clone());
        app.tree.begin(AgentId(4), None);

        run(&mut app, "/merge 4");
        run(&mut app, "/discard 4");

        assert!(root.join(".mush/wt/4").exists(), "nothing was reclaimed");
        let node = app
            .tree
            .agents
            .iter()
            .find(|node| node.id == AgentId(4))
            .unwrap();
        assert_eq!(node.landed, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `/diff` runs the diff and paints it. The old arm printed the command it
    /// *would* have run — a transcript whose only answer to "what did this agent
    /// do" was `· git diff HEAD...mush/1`, which is an instruction, not an
    /// answer (finding T9).
    #[test]
    fn the_diff_command_runs_the_diff_and_paints_it() {
        let root = repo("diff");
        isolated_work(&root, 1, "add the parser");
        let mut app = app_at(root.clone());

        run(&mut app, "/diff 1");

        // The bar got the one-glance line, in the vocabulary the row uses.
        assert_eq!(text_of(&app), "#1 mush/1 +1−0");
        let note = app
            .chat
            .notices_for(AgentId::ROOT)
            .next()
            .map(|notice| notice.text.clone())
            .expect("the diff is the answer");
        assert_eq!(
            note, "work.txt @@ -0,0 +1 @@\n+the work",
            "the answer is the change itself, named by its file: {note:?}"
        );
        // The model pays nothing for a command the human typed: this is a
        // notice, not a message, and notices are never sent anywhere.
        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "the diff is the human's reading, not a turn"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The pane's own rows show the change, not git's file header (finding
    /// S8(ii)): `/diff` keeps the head, and the head is the first hunks, each
    /// named by its file — so a `+` line is on screen without `/notes`.
    #[test]
    fn the_diff_pane_shows_the_first_hunks_not_gits_preamble() {
        let root = repo("diff-first-hunk");
        isolated_work(&root, 1, "add work");
        let worktree = git::worktree_path(&root, 1);
        std::fs::write(worktree.join("more.txt"), "alpha\nbravo\n").unwrap();
        git(&worktree, &["add", "-A"]);
        git(&worktree, &["commit", "-qm", "more"]);
        let mut app = app_at(root.clone());

        run(&mut app, "/diff 1");

        let note = app
            .chat
            .notices_for(AgentId::ROOT)
            .next()
            .map(|notice| notice.text.clone())
            .expect("the diff is the answer");
        assert!(
            !note.contains("diff --git") && !note.contains("index "),
            "git's preamble is not the answer: {note:?}"
        );
        assert!(
            note.lines().any(|line| line.starts_with("more.txt @@")),
            "each hunk is named by its file: {note:?}"
        );

        // What the human actually sees at a normal terminal: the pane's two
        // note rows are the first hunk and its first changed line.
        let rows = screen(&mut app, 80, 16);
        assert!(
            rows.iter().any(|row| row.contains("more.txt @@")),
            "the first hunk is painted: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.trim_start().starts_with("+alpha")),
            "and so is a changed line: {rows:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A branch with nothing on it is a fact — the work is at HEAD — and saying
    /// nothing would be indistinguishable from a command that did not run.
    #[test]
    fn a_diff_with_nothing_to_show_says_so() {
        let root = repo("diff-empty");
        let worktree = git::worktree_path(&root, 3);
        std::fs::create_dir_all(root.join(".mush")).unwrap();
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                worktree.to_str().unwrap(),
                "-b",
                &git::branch_name(3),
            ],
        );
        let mut app = app_at(root.clone());

        run(&mut app, "/diff 3");

        assert_eq!(text_of(&app), "#3 mush/3 ±0 — nothing changed");
        let note = app
            .chat
            .notices_for(AgentId::ROOT)
            .next()
            .map(|notice| notice.text.clone())
            .expect("an answer either way");
        assert!(
            note.contains("nothing changed") && note.contains("mush/3 is at HEAD"),
            "an empty diff is still an answer: {note:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A diff can be enormous — a branch that touched a lockfile prints more
    /// than the whole session — so it is capped like every other tool result:
    /// whole lines, the head kept, and one row saying what was left out and how
    /// to read it.
    #[test]
    fn a_huge_diff_is_capped_and_says_what_it_dropped() {
        let root = repo("diff-huge");
        isolated_work(&root, 2, "a big change");
        let worktree = git::worktree_path(&root, 2);
        let body: String = (0..800).map(|line| format!("line {line}\n")).collect();
        std::fs::write(worktree.join("big.txt"), body).unwrap();
        git(&worktree, &["add", "-A"]);
        git(&worktree, &["commit", "-qm", "big"]);
        let mut app = app_at(root.clone());

        run(&mut app, "/diff 2");

        let note = app
            .chat
            .notices_for(AgentId::ROOT)
            .next()
            .map(|notice| notice.text.clone())
            .expect("the diff is the answer");
        assert!(
            note.contains("+line 0") && note.starts_with("big.txt @@"),
            "the head of the change is what is kept: {:?}",
            &note[..note.len().min(200)]
        );
        assert!(
            !note.contains("+line 799"),
            "and the tail is what is dropped"
        );
        let marker = note.lines().last().expect("a last row");
        assert!(
            marker.starts_with("[+") && marker.ends_with("reads the rest]"),
            "one row says how much is left and how to read it: {marker:?}"
        );
        assert!(
            note.len() <= app.cfg().cmd_cap() + 200,
            "the cap is the tool-result cap, not a shape of its own: {} bytes",
            note.len()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A node can name a branch git no longer has — a leftover restored with a
    /// name the human deleted by hand — and the honest answer is git's own
    /// complaint, not a diff against a revision that does not resolve.
    #[test]
    fn a_diff_of_a_branch_git_no_longer_has_fails_honestly() {
        let root = repo("diff-gone");
        let mut app = app_at(root.clone());
        app.tree.insert(Spawn {
            id: AgentId(7),
            parent: AgentId::ROOT,
            brief: "work that left".to_string(),
            depth: 1,
            branch: Some("mush/7".to_string()),
            cmd: crossbeam_channel::unbounded().0,
        });

        run(&mut app, "/diff 7");

        assert!(
            text_of(&app).contains("cannot diff mush/7"),
            "the failure names the branch that could not be read: {}",
            text_of(&app)
        );
        assert!(
            app.chat.notices_for(AgentId::ROOT).next().is_none(),
            "and nothing was written as if a diff had been read"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The cap is on bytes of whole lines, and it never cuts a glyph in half:
    /// one minified file is one line, and half a UTF-8 sequence in a transcript
    /// is worse than a shorter row.
    #[test]
    fn a_long_text_is_cut_at_whole_lines_and_on_a_char_boundary() {
        let text = "alpha\nbravo\ncharlie\n";
        assert_eq!(head_lines(text, 100), (text.trim_end().to_string(), 0));
        assert_eq!(head_lines(text, 11), ("alpha\nbravo".to_string(), 1));
        assert_eq!(head_lines(text, 5), ("alpha".to_string(), 2));
        assert_eq!(head_lines("", 10), (String::new(), 0));
        // Two two-byte glyphs, one byte short of a third: the boundary is what
        // decides, not the count.
        assert_eq!(head_lines("εεεε", 5), ("εε".to_string(), 0));
    }

    /// `/forget` drops the conversation, not the work: the branch survives, so
    /// the honest thing is to say it is still there.
    #[test]
    fn forgetting_an_agent_drops_the_node_and_keeps_the_worktree() {
        let root = repo("forget");
        isolated_work(&root, 6, "leave me");
        let mut app = app_at(root.clone());
        app.chat
            .replace_transcript(AgentId(6), vec![Message::user("hello")]);

        app.forget_agent(AgentId(6));

        assert!(app.tree.agents.iter().all(|node| node.id != AgentId(6)));
        assert!(
            app.chat.transcript(AgentId(6)).is_empty(),
            "its transcript goes with it"
        );
        assert!(
            root.join(".mush/wt/6").exists(),
            "forgetting is not discarding"
        );
        assert_eq!(
            app.tree.focused,
            AgentId::ROOT,
            "focus cannot point at a ghost"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The root is not forgettable, and a running agent has to be stopped first.
    #[test]
    fn forgetting_refuses_the_root_and_a_running_agent() {
        let (mut app, _rx) = test_app("forget-guards");
        app.forget_agent(AgentId::ROOT);
        assert!(app.tree.agents.iter().any(|node| node.id == AgentId::ROOT));

        app.tree.insert(Spawn {
            id: AgentId(9),
            parent: AgentId::ROOT,
            brief: "busy".to_string(),
            depth: 1,
            branch: None,
            cmd: crossbeam_channel::unbounded().0,
        });
        app.forget_agent(AgentId(9));
        assert!(
            app.tree.agents.iter().any(|node| node.id == AgentId(9)),
            "a running agent is not forgotten under itself"
        );
    }

    /// A stored conversation comes back with its subagents: a live mailbox each
    /// (so a follow-up message is delivered, not lost) and the transcript it had
    /// (which is the whole point of storing it).
    #[test]
    fn a_stored_conversation_restores_its_agents_with_a_live_mailbox() {
        let root = repo("restore");
        let stored = Session {
            root: root.display().to_string(),
            model: "test-model".into(),
            provider: "custom".into(),
            base_url: "http://127.0.0.1:1".into(),
            context: None,
            updated: 0,
            messages: vec![Message::user("the root task")],
            agents: vec![session::AgentSession {
                id: 2,
                parent: Some(0),
                depth: 1,
                brief: "port the parser".into(),
                branch: None,
                status: session::StoredStatus::Done,
                landed: None,
                leftover: false,
                summary: Some("finished it".into()),
                messages: vec![Message::user("port the parser"), Message::assistant("done")],
            }],
            notices: Vec::new(),
        };

        let (app, _rx) = app_root(&root, Some(stored), session_save::fake::Recorder::new());

        let node = app
            .tree
            .agents
            .iter()
            .find(|node| node.id == AgentId(2))
            .expect("the stored agent comes back");
        assert_eq!(node.brief, "port the parser");
        assert_eq!(node.phase, Phase::Done);
        assert_eq!(node.summary.as_deref(), Some("finished it"));
        // The transcript is what makes a follow-up possible: without it the
        // human is back to writing the brief from scratch.
        assert_eq!(app.chat.transcript(AgentId(2)).len(), 2);
        assert_eq!(app.chat.transcript(AgentId(2))[1].text(), "done");
        // A live mailbox: a follow-up is delivered rather than dropped, which is
        // what "revive" has to mean to be worth anything.
        let tx_to_child = app
            .tree
            .agent_tx
            .get(&AgentId(2))
            .expect("a restored agent gets a mailbox");
        assert!(
            tx_to_child
                .send(AgentMsg::Nudge("one more thing".into()))
                .is_ok(),
            "the restored actor must still be listening"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A stored conversation with one agent, for the restore path.
    fn stored_with_agent(
        root: &std::path::Path,
        status: session::StoredStatus,
        messages: Vec<Message>,
    ) -> Session {
        Session {
            root: root.display().to_string(),
            model: "test-model".into(),
            provider: "custom".into(),
            base_url: "http://127.0.0.1:1".into(),
            context: None,
            updated: 0,
            messages: vec![Message::user("the root task")],
            agents: vec![session::AgentSession {
                id: 2,
                parent: Some(0),
                depth: 1,
                brief: "port the parser".into(),
                branch: None,
                status,
                landed: None,
                leftover: false,
                summary: None,
                messages,
            }],
            notices: Vec::new(),
        }
    }

    /// Everything the restore heard from one agent, as text, for a test that
    /// has to prove what did *not* happen.
    fn events_from(rx: &Receiver<Msg>, id: AgentId, within: Duration) -> Vec<String> {
        let deadline = Instant::now() + within;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(Msg::Agent {
                    id: from, event, ..
                }) if from == id => {
                    seen.push(format!("{event:?}"));
                }
                Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        seen
    }

    /// Opening mush is not a request. A restored agent comes back with its
    /// transcript and its mailbox and does nothing until the human asks.
    ///
    /// Reviving an agent by *running* it replayed every stored task against the
    /// endpoint the moment mush opened — thirteen agents, thirteen requests
    /// nobody asked for — and a thinking endpoint refusing a replayed turn left
    /// the whole tree marked `✗` over work that had finished.
    #[test]
    fn a_restored_agent_comes_back_at_rest() {
        let root = repo("restore-at-rest");
        let stored = stored_with_agent(
            &root,
            session::StoredStatus::Done,
            vec![Message::user("port the parser"), Message::assistant("done")],
        );

        // Nothing answers here: an agent that ran would fail, loudly, in the
        // events this test reads.
        let (app, rx) = app_root(&root, Some(stored), session_save::fake::Recorder::new());

        let seen = events_from(&rx, AgentId(2), Duration::from_millis(300));
        assert!(
            seen.is_empty(),
            "a restored agent must not run at startup: {seen:?}"
        );
        assert_eq!(
            app.tree.node(AgentId(2)).map(|node| node.phase.clone()),
            Some(Phase::Done),
            "and its row still says what it did last"
        );
        // The mailbox is live, so the *human's* next message is what starts it.
        assert!(
            app.tree.agent_tx[&AgentId(2)]
                .send(AgentMsg::Nudge("carry on".into()))
                .is_ok(),
            "a restored agent is idle, not dead"
        );
        let started = events_from(&rx, AgentId(2), Duration::from_millis(500));
        assert!(
            started.iter().any(|event| event.contains("Running")),
            "the message it was sent starts the run: {started:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The stored transcript is the evidence of what an agent produced; the
    /// stored status is only the last thing that happened. An attempt the
    /// endpoint refused before it could touch the transcript (a resumed agent's
    /// first request) must not turn a finished agent into a `✗`, and the
    /// refusal belongs on the row either way.
    #[test]
    fn a_refused_attempt_does_not_turn_a_finished_agent_into_a_failure() {
        let root = repo("restore-refused");
        let refusal = "model returned HTTP 400: The `reasoning_content` in the thinking \
                       mode must be passed back to the API.";
        let stored = stored_with_agent(
            &root,
            session::StoredStatus::Failed(refusal.to_string()),
            vec![Message::user("port the parser"), Message::assistant("done")],
        );

        let (app, _rx) = app_root(&root, Some(stored), session_save::fake::Recorder::new());

        let node = app.tree.node(AgentId(2)).expect("the agent is restored");
        assert_eq!(
            node.phase,
            Phase::Done,
            "the transcript ends on the answer the agent produced"
        );
        assert_eq!(
            node.summary.as_deref(),
            Some(format!("last attempt refused: {refusal}").as_str()),
            "the refusal is still on the row"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other half of that rule: an agent whose stored transcript stops
    /// mid-task has no answer to fall back on, so a stored failure stays one.
    #[test]
    fn a_failure_that_left_the_transcript_mid_task_is_still_a_failure() {
        let root = repo("restore-mid-task");
        let stored = stored_with_agent(
            &root,
            session::StoredStatus::Failed("the endpoint stopped responding".into()),
            vec![
                Message::user("port the parser"),
                Message::assistant("starting now"),
                Message::user("and make it fast"),
            ],
        );

        let (app, _rx) = app_root(&root, Some(stored), session_save::fake::Recorder::new());

        assert_eq!(
            app.tree.node(AgentId(2)).map(|node| node.phase.clone()),
            Some(Phase::Failed("the endpoint stopped responding".into())),
            "an unfinished transcript keeps its failure"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A count a human has to count digits in says nothing at a glance, and the
    /// endpoint's real numbers are large ones.
    #[test]
    fn token_counts_read_at_a_glance() {
        assert_eq!(tokens_label(842), "842");
        assert_eq!(tokens_label(3_100), "3.1k");
        assert_eq!(tokens_label(32_000), "32k", "a whole number keeps no `.0`");
        assert_eq!(tokens_label(500_000), "500k");
        assert_eq!(tokens_label(999_900), "999.9k");
        assert_eq!(
            tokens_label(999_999),
            "1M",
            "the last stretch before a million is not `1000k`"
        );
        assert_eq!(tokens_label(1_213_866), "1.2M");
    }

    /// A paste lands in the message box as one insert and is *not* sent: mush
    /// must never decide for the human that what they pasted was a message.
    #[test]
    fn a_paste_fills_the_box_without_sending() {
        let (mut app, _rx) = test_app("paste");
        app.focus = Focus::Chat;
        app.update(Msg::Paste("line one\r\nline two\n".into()));
        // Line endings are normalised, and the whole paste arrives at once.
        assert_eq!(app.chat.input().text(), "line one\nline two\n");
        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "a paste is not a send"
        );
    }

    /// Enter sends; Shift+Enter and Alt+Enter start a new line instead, so a
    /// multi-line message can be written before it goes.
    #[test]
    fn a_modified_enter_starts_a_new_line_a_plain_one_sends() {
        let (mut app, _rx) = test_app("multiline-key");
        app.focus = Focus::Chat;
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, modifiers)));
        }
        assert_eq!(app.chat.input().text(), "\n\n");
        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "a modified Enter must not send"
        );
    }

    /// A streamed message is the message path, and the message path must build
    /// nothing: the debounce is what turns a burst of them into one write.
    #[test]
    fn a_streamed_message_hands_no_snapshot_to_the_writer() {
        let (mut app, recorder) = app_recording("streamed");
        for i in 0..5 {
            streamed(&mut app, &format!("m{i}"));
        }
        assert_eq!(
            recorder.len(),
            0,
            "the handler rebuilt nothing: a tool result costs no snapshot"
        );
        assert!(
            app.session_dirty_at.is_some(),
            "it did mark the conversation dirty, though"
        );

        // A second on, the tick pays for all five at once — with the last of
        // them, which is the state a restart must resume from.
        age_session(&mut app, SESSION_DEBOUNCE);
        app.tick();
        let saved = recorder.saved();
        assert_eq!(saved.len(), 1, "one snapshot for the burst");
        assert_eq!(saved[0].messages.len(), 5);
        assert_eq!(saved[0].messages[4].text(), "m4");
        assert!(
            app.session_dirty_at.is_none(),
            "and the file is current again"
        );
    }

    /// The whole point of the debounce, end to end: a burst of root messages
    /// costs one write, and the file it leaves holds the last of them.
    #[test]
    fn a_burst_of_messages_ends_in_one_write_holding_the_last_of_them() {
        let root = dir("burst");
        let (mut app, writer) = app_writing(&root);
        for i in 0..5 {
            streamed(&mut app, &format!("m{i}"));
        }
        assert!(
            !session::session_path(&root).exists(),
            "no message wrote a file"
        );

        age_session(&mut app, SESSION_DEBOUNCE);
        app.tick();
        // Wait for the writer rather than for a clock: the hand-over is the
        // UI thread's, and the write is the writer's.
        writer.flush();
        assert_eq!(writer.writes(), 1, "five messages, one write");
        let stored = Session::load(&root).expect("the burst reached the disk");
        assert_eq!(stored.messages.len(), 5);
        assert_eq!(stored.messages.last().unwrap().text(), "m4");
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The human's own message is on disk before the send returns: the run it
    /// starts may take minutes, and a crash in it must not lose the request.
    /// This is the boundary the debounce is *not* allowed to cover.
    #[test]
    fn a_sent_message_is_on_disk_before_the_send_returns() {
        let root = dir("send");
        let (mut app, _writer) = app_writing(&root);
        app.chat.insert("please port the parser");
        app.send_message();

        let stored = Session::load(&root).expect("the send flushed it");
        assert_eq!(
            stored.messages.last().map(|message| message.text()),
            Some("please port the parser")
        );
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A window the human stated is remembered for the workspace, so `/context`
    /// must not lose it: the command is not done until the file says so.
    #[test]
    fn a_stated_context_is_on_disk_before_the_command_returns() {
        let root = dir("context");
        let (mut app, _writer) = app_writing(&root);
        run(&mut app, "/context 240000");

        let stored = Session::load(&root).expect("the command flushed it");
        assert_eq!(stored.context, Some(240_000));
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The shipped default window is not the human's: a session budgeted on it
    /// must store `context: null`, or the next launch would read a default
    /// nobody stated back as a statement — and no endpoint could ever teach
    /// mush a smaller window for that workspace again. Only an explicit window
    /// is worth remembering.
    #[test]
    fn a_default_window_is_never_stored_as_if_it_were_stated() {
        let root = dir("default-context");
        let (mut app, _writer) = app_writing(&root);
        app.cell.edit(|cfg| {
            cfg.provider = Provider::DeepSeek;
            cfg.rederive_context();
        });
        assert_eq!(app.cfg().context_tokens, 120_000, "the shipped default");
        assert!(!app.cfg().context_explicit);

        app.chat.insert("hello");
        app.send_message();

        let stored = Session::load(&root).expect("the send flushed it");
        assert_eq!(stored.context, None, "a guess is not a statement");
        assert_eq!(stored.model, app.cfg().model);
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `/forget` is a deletion, and a deletion that only lived in memory would
    /// come back at the next start — the agent would be listed as if the human
    /// had never dropped it.
    #[test]
    fn a_forgotten_agent_is_gone_from_the_file() {
        let root = repo("forget-file");
        isolated_work(&root, 6, "leave me");
        let (mut app, _writer) = app_writing(&root);
        app.chat
            .replace_transcript(AgentId(6), vec![Message::user("hello")]);

        app.forget_agent(AgentId(6));

        let stored = Session::load(&root).expect("the command flushed it");
        assert!(
            stored.agents.iter().all(|agent| agent.id != 6),
            "a forgotten agent cannot come back from the file"
        );
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Quitting writes what the debounce had not: the exit flush is what makes
    /// a crash cost a second of chat rather than everything since the last
    /// boundary.
    #[test]
    fn quitting_writes_the_messages_the_debounce_had_not() {
        let root = dir("quit");
        let (mut app, _writer) = app_writing(&root);
        streamed(&mut app, "the last thing said");
        assert!(
            !session::session_path(&root).exists(),
            "the debounce has not elapsed"
        );

        drop(app);

        let stored = Session::load(&root).expect("the exit flush wrote it");
        assert_eq!(
            stored.messages.last().map(|message| message.text()),
            Some("the last thing said")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Quitting kills every job, wherever it is in the tree. The `Drop` is the
    /// one way out of the event loop, and a build an agent started must not
    /// outlive a clean quit — the human's next `cargo build` would otherwise
    /// fight a ghost for the target directory.
    #[test]
    fn quitting_kills_the_jobs_the_agents_started() {
        use crate::jobs::Launch;
        use crate::machine::fake::{Script, Scripted as ScriptedMachine};
        use crate::machine::{Machine, ShellCommand};

        let (app, _rx) = test_app("jobs-die-on-quit");
        let machine = Arc::new(
            ScriptedMachine::new()
                .runs(Script::hangs())
                .runs(Script::hangs()),
        );
        let registry = app.tree.handles().jobs;
        let mut job_rx = Vec::new();
        // One job for the root, one for an agent that is not the root: the kill
        // is the registry's, so it is not the root's jobs that die but every
        // job in the tree.
        for owner in [0u64, 3] {
            let job = machine
                .spawn(&ShellCommand {
                    command: "cargo build",
                    root: std::path::Path::new("/tmp"),
                })
                .unwrap();
            let (tx, rx) = crossbeam_channel::unbounded();
            registry
                .launch(Launch::started(
                    owner,
                    "cargo build".to_string(),
                    false,
                    tx,
                    job,
                ))
                .unwrap();
            job_rx.push(rx);
        }
        assert_eq!(registry.running(), 2, "both jobs are live before the quit");

        drop(app);

        // The kill is synchronous: the process groups are gone before `Drop`
        // returns, not ten milliseconds later.
        assert_eq!(machine.kills(), 2, "quitting killed every job");
        for rx in &job_rx {
            assert!(
                matches!(
                    rx.recv_timeout(Duration::from_secs(5)),
                    Ok(AgentMsg::CommandDone { .. })
                ),
                "each job's own thread reported its stop"
            );
        }
        assert_eq!(registry.running(), 0, "nothing is left running");
    }

    /// Quitting kills the command an agent is *waiting on*, not only a detached
    /// job. A `run_command` without `detach` is spawned for the length of a tool
    /// call and used to be registered nowhere, so `kill_all` — the last thing
    /// `App::drop` does — could not see it: a plain `sleep 10; touch marker`
    /// survived a clean `Ctrl-Q`, in its own process group, and did its work
    /// after mush was gone (finding S4). The hold below is the same one the
    /// agent takes, and the quit is the same one the human's `Ctrl-Q` runs.
    #[test]
    fn quitting_kills_a_running_foreground_command() {
        use crate::machine::fake::{Script, Scripted as ScriptedMachine};
        use crate::machine::{Machine, ShellCommand};

        let (app, _rx) = test_app("foreground-dies-on-quit");
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let registry = app.tree.handles().jobs;
        let job = machine
            .spawn(&ShellCommand {
                command: "sleep 10; touch marker",
                root: std::path::Path::new("/tmp"),
            })
            .unwrap();
        let held = registry.hold(0, job);
        assert!(
            registry.holding_foreground(0),
            "the call holds it while the model waits on it"
        );
        assert_eq!(machine.kills(), 0, "and nothing has stopped it yet");

        drop(app);

        assert_eq!(
            machine.kills(),
            1,
            "quitting killed the command the agent was waiting on"
        );
        // The call is over, so its slot is gone: the registry is left holding
        // nothing, and this test keeps it alive precisely to check that.
        drop(held);
        assert!(
            !registry.holding_foreground(0),
            "a finished call leaves no slot behind"
        );
        registry.kill_all();
        assert_eq!(
            machine.kills(),
            1,
            "so the next quit cannot kill the same command twice"
        );
    }

    /// `c` on a row is aimed at the work, and a detached job is work in flight:
    /// an agent whose run ended while its `cargo bench` still runs is not idle
    /// on the machine. The row used to answer "not running" and do nothing,
    /// while Ctrl-C — which asks `working_agents`, the same question spelled
    /// once — stopped that very job. Two paths, one answer.
    #[test]
    fn c_on_an_idle_agent_still_stops_its_job() {
        use crate::jobs::Launch;
        use crate::machine::fake::{Script, Scripted as ScriptedMachine};
        use crate::machine::{Machine, ShellCommand};

        let (mut app, _rx) = test_app("cancel-job-row");
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let registry = app.tree.handles().jobs;
        let job = machine
            .spawn(&ShellCommand {
                command: "cargo bench",
                root: std::path::Path::new("/tmp"),
            })
            .unwrap();
        let (tx, job_rx) = crossbeam_channel::unbounded();
        registry
            .launch(Launch::started(
                0,
                "cargo bench".to_string(),
                false,
                tx,
                job,
            ))
            .unwrap();
        // The root is idle — and the row must not say so as if the machine were.
        assert!(!app.tree.agents[0].phase.is_busy());
        assert_eq!(app.live_jobs(AgentId::ROOT).len(), 1);
        assert!(app.busy(), "the tree has work in flight on the machine");

        app.tree.cursor_top();
        app.focus = Focus::Agents;
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
        )));

        assert!(
            !text_of(&app).contains("is not running"),
            "there is something to stop: {}",
            text_of(&app)
        );
        // Stopped for real: the Stop goes to the owner's actor, which kills what
        // it started, and the job's own thread reports it.
        match job_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone { line, .. }) => {
                assert!(line.contains("stopped after"), "{line}")
            }
            other => panic!("the job must be stopped: {:?}", other.is_ok()),
        }
        assert!(!app.busy(), "and the machine is free again");
    }

    /// `/new` clears the stored conversation too, not just the visible one: the
    /// old chat coming back at the next start is exactly what the flush stops.
    #[test]
    fn a_new_chat_clears_the_stored_conversation() {
        let root = dir("new-chat");
        let (mut app, _writer) = app_writing(&root);
        app.chat.insert("something worth remembering");
        app.send_message();
        assert_eq!(Session::load(&root).unwrap().messages.len(), 1);

        run(&mut app, "/new");

        let stored = Session::load(&root).expect("the command flushed it");
        assert!(stored.messages.is_empty(), "the old chat is not resumed");
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `/merge` lands an agent, and landing it is stored: a restart must not
    /// offer to merge work that is already in HEAD.
    #[test]
    fn a_landed_merge_is_in_the_file_before_the_command_returns() {
        let root = repo("merge-file");
        isolated_work(&root, 3, "add the parser");
        let (mut app, _writer) = app_writing(&root);
        run(&mut app, "/merge 3");

        let stored = Session::load(&root).expect("the command flushed it");
        let landed = stored
            .agents
            .iter()
            .find(|agent| agent.id == 3)
            .expect("the agent it landed");
        assert_eq!(landed.landed, Some(session::StoredLanded::Merged));
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A session file mush cannot read is *said*, not silently dropped
    /// (finding S3). The human comes back to an empty screen; "empty" must not
    /// be the whole story, so the line names the file, the reason and where the
    /// only copy went — in the pane's foot, in `/notes`, and on the bar without
    /// opening anything. A workspace with nothing stored says none of it.
    #[test]
    fn an_unreadable_session_is_said_rather_than_dropped() {
        let (mut app, _rx) = test_app("unreadable-session");

        // Nothing stored: no line about a session, and the bar keeps its hint.
        assert!(
            app.chat.notices_for(AgentId::ROOT).next().is_none(),
            "an absent session is not news"
        );
        let rows = screen(&mut app, 200, 40);
        assert!(
            !rows.join("\n").contains("could not read"),
            "and nothing is said about one: {rows:?}"
        );

        let notice = "could not read .mush/session.json — expected value at line 1 column 2; \
                      kept as .mush/session.json.bak · starting a new conversation";
        app.session_unreadable(notice);

        // The bar says it without anything being opened: line one, in red,
        // after the focus badge — and the pane's foot carries it whole.
        let rows = screen(&mut app, 200, 40);
        assert!(
            rows.iter().any(|row| row.starts_with(" chat ")
                && row.contains("could not read .mush/session.json")),
            "the bar carries it: {rows:?}"
        );
        let painted = rows.join("\n");
        assert!(
            painted.contains("expected value at line 1 column 2"),
            "the reason is on screen: {painted}"
        );
        assert!(
            painted.contains("kept as .mush/session.json.bak"),
            "and where the only copy went: {painted}"
        );
        // `/notes` reads it back, so a line that wrapped or scrolled is still
        // readable in full.
        run(&mut app, "/notes");
        assert!(
            screen(&mut app, 200, 40)
                .join("\n")
                .contains("could not read .mush/session.json"),
            "the report holds it"
        );
        // It is news, not chatter: the human's next send does not take it away,
        // and the session is marked dirty so the next save carries it.
        assert!(
            !app.chat.dismiss_said(),
            "a send must not end what the workspace said about itself"
        );
        assert_eq!(
            app.chat
                .notices_for(AgentId::ROOT)
                .filter(|notice| notice.rank() == crate::app::chat::Rank::Alert)
                .count(),
            1,
            "and it ranks as a failure, so the foot and the bar cannot hide it"
        );
        assert_eq!(app.chat.stored_notices().len(), 1);
        let _ = std::fs::remove_dir_all(app.ws.root());
    }

    /// A compaction is what a restart resumes from, so the folded transcript is
    /// written before the event returns rather than waiting out the debounce.
    #[test]
    fn a_compaction_is_written_before_the_event_returns() {
        let root = dir("compact");
        let (mut app, _writer) = app_writing(&root);
        streamed(&mut app, "a long conversation");
        let conversation = app.tree.conversation();

        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Compact {
                in_run: false,
                summary: "porting the parser".to_string(),
            },
        });

        let stored = Session::load(&root).expect("the fold flushed it");
        assert_eq!(stored.messages.len(), 1, "the conversation is the summary");
        assert!(stored.messages[0].text().contains("porting the parser"));
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A background write's failure has no caller to return to, so it waits for
    /// a tick and is reported the way the flush path reports it: a workspace
    /// mush cannot write to must not look saved.
    #[test]
    fn a_failed_background_write_is_reported_on_a_tick() {
        use session_save::fake::Recorder;

        let recorder = Recorder::new().fails("no space left on device");
        let root = dir("failed-write");
        let mut app = app_root(&root, None, recorder).0;
        streamed(&mut app, "lost");

        // The debounce hands it over; the scripted write fails; the next tick is
        // where the UI can hear about it.
        age_session(&mut app, SESSION_DEBOUNCE);
        app.tick();
        app.tick();

        assert_eq!(
            app.status.as_ref().map(|status| status.kind),
            Some(StatusKind::Error)
        );
        assert!(
            text_of(&app).contains("could not save session"),
            "it says what failed: {}",
            text_of(&app)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// How long one frame costs on a session the size of a real one. A frame
    /// that does not fit in a 60 fps budget is felt as lag, so this is a
    /// regression guard as much as a measurement.
    #[test]
    fn a_frame_fits_in_a_60fps_budget_on_a_long_transcript() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let (mut app, _rx) = test_app("perf");
        // Roughly what a long session looks like: hundreds of messages, with
        // multi-kilobyte tool results in among them.
        for i in 0..300 {
            app.chat
                .push_message(AgentId::ROOT, Message::user(format!("message {i}")));
            app.chat.push_message(
                AgentId::ROOT,
                Message::assistant("a reply a few words long"),
            );
            app.chat.push_message(
                AgentId::ROOT,
                Message::tool(format!("c{i}"), "x".repeat(2000)),
            );
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        // A frame is now two halves — `App` derives the `Screen` and the
        // painter paints it — and the budget is for *both*: a screen built in
        // 55 ms and painted in one is still 55 ms of lag.
        let area = Rect::new(0, 0, 120, 40);
        terminal
            .draw(|f| {
                let screen = app.screen(area);
                crate::ui::draw(f, &screen)
            })
            .unwrap();
        const FRAMES: u32 = 30;
        let start = Instant::now();
        for _ in 0..FRAMES {
            let screen = app.screen(area);
            terminal.draw(|f| crate::ui::draw(f, &screen)).unwrap();
        }
        let per_frame = start.elapsed() / FRAMES;
        eprintln!("measured: {per_frame:?} per frame");
        assert!(
            per_frame < Duration::from_millis(16),
            "a frame must fit a 60 fps budget, took {per_frame:?}"
        );
    }

    /// The two lifetimes, read off the file. A run's failure belongs to the run
    /// and to the workspace it broke, so it is on disk and comes back at the
    /// next start; a line that answered a command answered a moment that is
    /// over by then, and restoring it out of context would say "git diff
    /// HEAD...mush/2" over a tree nobody was looking at.
    #[test]
    fn a_failure_is_stored_and_a_command_answer_is_not() {
        let root = dir("notes-file");
        let (mut app, _writer) = app_writing(&root);
        app.chat.note("git diff HEAD...mush/2");
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Error("no route to host".into()),
        });
        app.flush_session();
        drop(app);

        let stored = Session::load(&root).expect("the command flushed it");
        assert_eq!(stored.notices.len(), 1, "only the failure is stored");
        assert_eq!(stored.notices[0].agent, 0);
        assert_eq!(stored.notices[0].text, "no route to host");
        assert!(
            stored.notices[0].at > 0,
            "a stored line says when it happened, not only what happened"
        );

        let app = reopened(&root);
        let notes: Vec<&str> = app
            .chat
            .notices_for(AgentId::ROOT)
            .map(|notice| notice.text.as_str())
            .collect();
        assert_eq!(
            notes,
            vec!["no route to host"],
            "the restart kept the failure and dropped the diff line"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A typo'd command answers the moment the human typed it, so it is nothing
    /// more than a note for that moment: not an alert that outlives the moment,
    /// and not a line the session file writes down and a relaunch reads back. A
    /// *run's* failure is the opposite and stays stored — see
    /// [`a_failure_is_stored_and_a_command_answer_is_not`].
    #[test]
    fn a_typoed_command_is_a_moment_not_a_stored_failure() {
        let root = dir("typo");
        let (mut app, _writer) = app_writing(&root);
        app.chat.insert("/hlep");
        app.send_message();

        let notice = app
            .chat
            .notices_for(AgentId::ROOT)
            .next()
            .expect("the typo is answered");
        assert_eq!(notice.text, "unknown command: /hlep");
        assert_eq!(
            notice.rank(),
            Rank::Said,
            "an informational line, not an alert the cap may never yield"
        );
        assert!(
            app.chat.stored_notices().is_empty(),
            "nothing about a typo is written to the session"
        );

        app.flush_session();
        let stored = Session::load(&root).expect("the flush wrote the file");
        assert!(
            stored.notices.is_empty(),
            "so the file records no such line: {:?}",
            stored.notices
        );
        drop(app);

        let app = reopened(&root);
        assert!(
            app.chat.notices_for(AgentId::ROOT).next().is_none(),
            "and a relaunch does not bring the typo back"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A fresh run of an agent supersedes its stale failure: the row is derived
    /// from the phase and cannot lie, so the line that disagrees with it is the
    /// one that goes.
    #[test]
    fn a_completed_run_supersedes_an_older_failure() {
        let (mut app, _rx) = test_app("supersede");
        let conversation = app.tree.conversation();
        let send = |app: &mut App, id: AgentId, event: AgentEvent| {
            app.update(Msg::Agent {
                conversation,
                id,
                event,
            })
        };
        let notes = |app: &App| -> Vec<String> {
            app.chat
                .notices_for(AgentId::ROOT)
                .map(|notice| notice.text.clone())
                .collect()
        };

        send(
            &mut app,
            AgentId::ROOT,
            AgentEvent::Error("no route to host".into()),
        );
        assert_eq!(notes(&app), vec!["no route to host"]);

        // Failing twice in one run is still one failure: two lines would
        // disagree about which of them is current.
        send(
            &mut app,
            AgentId::ROOT,
            AgentEvent::Error("still no route".into()),
        );
        assert_eq!(notes(&app), vec!["still no route"]);

        // A child's run is not the root's business: the line is tagged with the
        // agent it concerns (finding B19).
        send(
            &mut app,
            AgentId(1),
            AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        assert_eq!(
            notes(&app),
            vec!["still no route"],
            "an agent that ran clears its own line and nobody else's"
        );

        // The run that supersedes it: the failure belonged to the attempt this
        // one replaced, and once it has started and finished there is nothing
        // left claiming the agent is broken.
        send(
            &mut app,
            AgentId::ROOT,
            AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        send(&mut app, AgentId::ROOT, AgentEvent::Done);
        assert!(
            notes(&app).is_empty(),
            "a completed run leaves no failure line behind: {:?}",
            notes(&app)
        );
    }

    /// The foot at the two sizes the audit names. 40×10 leaves the transcript
    /// pane one row, so the pane keeps that row and says the count in its
    /// title; 60×17 has room for two note rows and the count above them. Either
    /// way the number is the number of lines not painted, which is what makes
    /// it a decision and not a discovery.
    #[test]
    fn the_foot_caps_at_two_rows_and_counts_the_rest() {
        let (mut app, _rx) = test_app("foot-sizes");
        // The line is pushed straight in, without the box: a user message that
        // is not the echo of what the human sent is somebody else's — the
        // voice is `chat.rs`'s business and this test is about the foot.
        app.chat
            .push_message(AgentId::ROOT, Message::user("port the parser"));
        for index in 0..6 {
            app.chat.note_for(AgentId::ROOT, format!("note {index}"));
        }

        // 40×10: the pane is one row tall inside its border. The transcript
        // keeps it — the foot takes none, and the title says what is missing.
        let small = screen(&mut app, 40, 10);
        assert_eq!(small.len(), 10);
        assert!(
            small[4].contains("› port the parser"),
            "the one transcript row is the conversation, not a foot: {:?}",
            &small[3..6]
        );
        assert!(
            small[3].contains("+6 more lines") && small[3].contains("/notes"),
            "six note lines are hidden, the pane says so and names the way to read \
             them even though it has no count row to spend: {:?}",
            small[3]
        );
        assert!(
            !small.iter().any(|row| row.contains("  +6 more lines")),
            "but the count row itself needs a row the pane does not have: {small:?}"
        );

        // 60×17: two rows of notes, then one that says four lines are not
        // there. Six lines were written, two are painted, four are counted.
        let roomy = screen(&mut app, 60, 17);
        assert!(roomy[4].contains("› port the parser"), "{:?}", &roomy[3..9]);
        assert_eq!(roomy[5], "  +4 more lines · /notes", "{:?}", &roomy[3..9]);
        assert_eq!(roomy[6], "· note 4", "{:?}", &roomy[3..9]);
        assert_eq!(roomy[7], "· note 5", "{:?}", &roomy[3..9]);
        assert!(
            !roomy.iter().any(|row| row.contains("note 3")),
            "the cap is two rows, and the count is the rest: {:?}",
            &roomy[3..9]
        );
    }

    /// A tree bigger than its pane, which is the case the size tiers exist for:
    /// the compact strip wants two rows more than it has agents.
    fn crowd(app: &mut App, agents: u64) {
        let conversation = app.tree.conversation();
        for id in 1..=agents {
            app.update(Msg::Agent {
                conversation,
                id: AgentId::ROOT,
                event: AgentEvent::Spawned {
                    child: id,
                    parent: 0,
                    brief: format!("task {id}"),
                    depth: 1,
                    branch: None,
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }
    }

    /// The bar keeps a row whatever else is on screen. In compact mode it was
    /// the trailing constraint behind a `Min(6)` chat, and at 40×10 the panes
    /// above it took the row it was owed: the frame painted the tree, the
    /// transcript and the message box, and the ` chat ` row — the focus badge,
    /// the key hint, and the only home an Info line or a command's usage error
    /// has — was simply not there.
    #[test]
    fn the_bar_keeps_a_row_on_the_shortest_terminals() {
        let (mut app, _rx) = test_app("bar-floor");
        crowd(&mut app, 3);
        // `/diff 9` is a failure with no other home: it is not a message, so it
        // is never in the transcript, and the row it is about does not exist.
        run(&mut app, "/diff 9");
        assert!(
            text_of(&app).contains("no agent #9"),
            "the line the bar is supposed to carry: {}",
            text_of(&app)
        );

        for (width, height) in [(40u16, 10u16), (40, 11), (40, 12), (60, 12), (120, 12)] {
            let rows = screen(&mut app, width, height);
            assert_eq!(rows.len(), height as usize, "{width}x{height}");
            let bar = rows.last().unwrap();
            assert!(
                bar.contains(" chat ") && bar.contains("no agent #9"),
                "the bar is missing its only row at {width}x{height}: {rows:?}"
            );
        }
    }

    /// Two children whose briefs open identically are two agents on the screen,
    /// named by what they were asked to make — without opening either (finding
    /// U6).
    #[test]
    fn the_row_names_an_agent_by_its_derived_title() {
        let (mut app, _rx) = test_app("agent-titles");
        let conversation = app.tree.conversation();
        for (id, path) in [(1u64, "deep.txt"), (2, "wide.txt")] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId::ROOT,
                event: AgentEvent::Spawned {
                    child: id,
                    parent: 0,
                    depth: 1,
                    brief: format!("create a file called {path} containing exactly: work"),
                    branch: None,
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }
        let rows = screen(&mut app, 120, 32);
        assert!(
            rows.iter().any(|row| row.contains("#1 deep.txt")),
            "the row names the agent by what it was asked to make: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("#2 wide.txt")),
            "and its sibling by its own: {rows:?}"
        );
    }

    /// A job's handle is its own command on one line: a `command` can be a
    /// thirty-line heredoc with a `cd` in front of it, and the footer and the
    /// bar each have one row to name it in.
    #[test]
    fn a_job_is_named_by_its_command_on_one_line() {
        assert_eq!(job_title("cargo build --release"), "cargo build --release");
        assert_eq!(job_title("cd /w && cargo test -q"), "cargo test -q");
        assert_eq!(
            job_title("python3 - <<'PY'\nimport io\nprint('x')\nPY"),
            "python3 - <<'PY'"
        );
        assert!(job_title("").is_empty());
        assert!(job_title(&"x".repeat(80)).chars().count() <= JOB_TITLE_COLUMNS);
    }

    /// A failure is the third way a run can end, and it reaches the bar the way
    /// a stop does. `Stopped` said what happened and `Failed` did not, so the
    /// newest thing on screen could be a crash (`✗ #0` on the row, `! cannot
    /// reach …` in the foot) under a line advertising Ctrl-P. A guard-stop is
    /// this same event — the runaway guard's complaint is the run's error — so
    /// one arm covers both endings.
    #[test]
    fn a_failure_reaches_the_bar_like_a_stop_does() {
        let (mut app, _rx) = test_app("failure-bar");
        let conversation = app.tree.conversation();
        let guard = "stopped after 40 turns without finishing (runaway guard)";
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Error(guard.to_string()),
        });
        assert_eq!(app.tree.agents[0].phase, Phase::Failed(guard.to_string()));
        let rows = screen(&mut app, 40, 10);
        let bar = rows.last().expect("the bar is painted");
        assert!(
            bar.contains("agent #0 failed"),
            "the smallest terminal still says what happened: {rows:?}"
        );

        // The stop that already worked, for comparison: the two endings are
        // told the same way.
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Stopped,
        });
        let rows = screen(&mut app, 40, 10);
        let bar = rows.last().expect("the bar is painted");
        assert!(
            bar.contains("agent #0 stopped"),
            "a stop says so too: {rows:?}"
        );
    }

    /// A run parked in a wait is not a model call. The transcript foot's
    /// spinner may only claim work in flight, so `wait_agents` must not paint
    /// `working…` over an agent that is waiting for a child's result — and the
    /// row says what it is waiting for instead (finding U7).
    #[test]
    fn a_waiting_agent_is_not_drawn_working() {
        let (mut app, _rx) = test_app("waiting-foot");
        app.tree.begin(AgentId::ROOT, None);
        // The label the actor emits for `wait_agents` with no arguments.
        app.tree.activity(AgentId::ROOT, "wait_agents ");
        app.tree.age(AgentId::ROOT, Duration::from_secs(5));

        let rows = screen(&mut app, 120, 32);
        assert!(
            !rows.join("\n").contains("working…"),
            "nothing is being computed, so nothing spins: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("waiting on agents 5s")),
            "the row says what it is waiting for: {rows:?}"
        );
        assert!(
            !rows.join("\n").contains("wait_agents"),
            "and not the tool's name, which reads like work: {rows:?}"
        );

        // A model that really has not answered still says so: the point is the
        // distinction, not the silence.
        app.tree.begin(AgentId::ROOT, None);
        app.tree.age(AgentId::ROOT, Duration::from_secs(2));
        let rows = screen(&mut app, 120, 32);
        assert!(
            rows.iter().any(|row| row.contains("thinking 2s")),
            "{rows:?}"
        );
        assert!(rows.join("\n").contains("working…"), "{rows:?}");
    }

    /// `/notes` is the other half of the cap: the lines the foot ceded are read
    /// in full, oldest first, with the cursor on the newest.
    #[test]
    fn the_notes_command_lists_what_the_foot_could_not_show() {
        let (mut app, _rx) = test_app("notes-command");
        assert!(
            app.chat
                .notes_report(AgentId::ROOT, session::now_secs(), 74)
                .rows
                .is_empty(),
            "nothing has been written about a fresh conversation"
        );
        run(&mut app, "/notes");
        assert!(
            app.picker.is_none(),
            "and the command says so instead of opening an empty list"
        );
        assert_eq!(text_of(&app), "nothing written about #0 yet");

        for index in 0..5 {
            app.chat.note_for(AgentId::ROOT, format!("note {index}"));
        }
        run(&mut app, "/notes");
        let picker = app.picker.as_ref().expect("the lines mush wrote");
        assert!(
            matches!(picker.kind, PickerKind::Notes),
            "the popup is a reading, not a choice"
        );
        assert_eq!(
            picker.items.len(),
            5,
            "every note, not only the held-back ones"
        );
        assert_eq!(
            picker.items[0], "0s · note 0",
            "oldest first, like the pane"
        );
        assert_eq!(picker.cursor, 4, "the cursor opens on the newest");
    }

    /// A note longer than the popup must not drop the human mid-sentence: the
    /// list opens on the head of the newest note — the row that carries when it
    /// happened and how it started — not on its last row (finding T7).
    #[test]
    fn the_notes_popup_opens_on_the_head_of_the_newest_note() {
        let (mut app, _rx) = test_app("notes-head");
        // Old enough to have a stamp, short enough to be one row.
        app.chat.note_for(AgentId::ROOT, "opened notes.txt");
        // The newest note, long enough to wrap into several rows at the width
        // the popup is painted at.
        app.chat.note_for(
            AgentId::ROOT,
            "the run failed while folding the transcript: the endpoint returned 503 \
             for the third summarisation attempt, and the fold was abandoned with the \
             conversation left half-written, so read the tail of the transcript before \
             trusting anything above it",
        );

        let _ = screen(&mut app, 80, 24);
        run(&mut app, "/notes");
        let picker = app.picker.as_ref().expect("the lines mush wrote");
        assert!(
            picker.items.len() > 3,
            "the newest note wraps, which is the case this is about: {:?}",
            picker.items
        );
        assert!(
            picker.cursor < picker.items.len() - 1,
            "the cursor is not on the last row, which is mid-sentence: {:?}",
            picker.items
        );
        assert_eq!(
            picker.items[picker.cursor], "0s · the run failed while folding the",
            "it opens on the head of the newest note, stamp included"
        );
        assert!(
            picker.items[..picker.cursor]
                .iter()
                .any(|row| row.contains("opened notes.txt")),
            "and the older note is above it, not scrolled away: {:?}",
            picker.items
        );
        let title = picker.title();
        assert!(
            title.contains(&format!(
                "line {}/{}",
                picker.cursor + 1,
                picker.items.len()
            )),
            "the title says where in the list the cursor is: {title}"
        );
    }

    /// `/notes` is the escape hatch for the lines the foot ceded, so it has to
    /// be readable at every size the popup is painted at. The report used to be
    /// wrapped at a fixed 74 — the popup's content width only at the widest size
    /// — so every row was clipped below 80 columns, and the first row clipped
    /// even on a 200-column terminal. The width has to come from the popup the
    /// frame actually paints, which is what `picker_text_width` says.
    #[test]
    fn the_notes_popup_wraps_to_the_width_it_is_painted_at() {
        let (mut app, _rx) = test_app("notes-wrap");
        app.chat.note(
            "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo \
             lima mike november oscar papa quebec romeo sierra tango",
        );

        // 60 columns: the popup is 40 wide and its list 34. A paint records the
        // width the way `main` does, then the command wraps its report to it.
        let _ = screen(&mut app, 60, 17);
        run(&mut app, "/notes");
        let narrow = screen(&mut app, 60, 17).join("\n");
        assert!(
            narrow.contains("tango"),
            "the whole note is read at 60 columns, not clipped: {narrow}"
        );

        // 200 columns: the widest popup, 74 content columns. The first row used
        // to overrun it by the age and marker that lead it.
        let _ = screen(&mut app, 200, 50);
        run(&mut app, "/notes");
        let wide = screen(&mut app, 200, 50).join("\n");
        assert!(wide.contains("tango"), "and at 200 columns: {wide}");
    }

    /// The empty state is a row like any other: wrapped to the pane and windowed
    /// to its height. Returned raw it was cut mid-word on a narrow pane, so the
    /// end of the instruction never appeared at all.
    #[test]
    fn the_empty_state_is_readable_on_a_narrow_pane() {
        let (mut app, _rx) = test_app("empty-state");
        let rows = screen(&mut app, 60, 17).join("\n");
        assert!(
            rows.contains("directly."),
            "the instruction wraps whole instead of clipping mid-word: {rows}"
        );
        assert!(
            rows.contains("Tab cycles panes · Enter sends"),
            "and the hints under it still get their rows: {rows}"
        );
    }

    /// A send ends the chatter the last command left in the foot: `/help` and a
    /// diff answer the thing the human typed *before* this message, and leaving
    /// them there spends the pane's rows on a question nobody is asking any more
    /// (finding U8). A failure is not a moment and survives the send.
    #[test]
    fn sending_the_next_message_ends_the_last_command_answer() {
        let (mut app, _rx) = test_app("chatter-send");
        run(&mut app, "/help");
        app.chat.note_error_for(AgentId::ROOT, "first failure");
        assert_eq!(
            app.chat.notices_for(AgentId::ROOT).count(),
            2,
            "the bar line and the pane's list both exist"
        );

        app.chat.insert("carry on");
        app.send_message();

        let left: Vec<&str> = app
            .chat
            .notices_for(AgentId::ROOT)
            .map(|notice| notice.text.as_str())
            .collect();
        assert_eq!(
            left,
            vec!["first failure"],
            "the chatter went and the failure stayed: {left:?}"
        );
        assert!(
            app.chat
                .transcript(AgentId::ROOT)
                .iter()
                .any(|message| message.role == "user"),
            "and the message itself was still sent"
        );
    }

    /// `/notes` reads the chatter instead of superseding it: dismissing on the
    /// way in would make the pane's own `+N more · /notes` point at nothing,
    /// which is the half of finding U8 the command exists to answer.
    #[test]
    fn asking_for_the_notes_does_not_throw_them_away() {
        let (mut app, _rx) = test_app("chatter-notes");
        app.chat.note_for(AgentId::ROOT, "opened notes.txt");

        app.chat.insert("/notes");
        app.send_message();

        assert!(app.picker.is_some(), "the list opened");
        assert_eq!(
            app.chat.notices_for(AgentId::ROOT).count(),
            1,
            "and it had the line it came to read"
        );
    }

    /// The clock, on the tick that already expires the bar's own transient line:
    /// a hint read once and then left on the screen must not outlive its moment,
    /// because the agent it answers may never run again (finding U8).
    #[test]
    fn a_transient_line_leaves_the_foot_without_a_run() {
        let (mut app, _rx) = test_app("chatter-clock");
        app.chat.note_for(AgentId::ROOT, "opened notes.txt");
        app.chat.note_error_for(AgentId::ROOT, "no route to host");

        app.tick();
        assert_eq!(
            app.chat.notices_for(AgentId::ROOT).count(),
            2,
            "a line inside its moment is still there"
        );

        app.chat.age_notices(600);
        app.tick();

        let left: Vec<&str> = app
            .chat
            .notices_for(AgentId::ROOT)
            .map(|notice| notice.text.as_str())
            .collect();
        assert_eq!(
            left,
            vec!["no route to host"],
            "the tick took the chatter and left the failure: {left:?}"
        );
    }

    fn test_app(label: &str) -> (App, Receiver<Msg>) {
        let root = std::env::temp_dir().join(format!("mush-app-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        app_root(&root, None, session_save::fake::Recorder::new())
    }

    /// `/new` must do what Ctrl-N does: a cleared chat with a live root, not
    /// an empty pane over a stale conversation.
    #[test]
    fn slash_new_restarts_the_root_and_clears_the_conversation() {
        let (mut app, _rx) = test_app("new");
        app.chat
            .push_message(AgentId::ROOT, Message::user("an old task"));
        app.chat.note_for(AgentId::ROOT, "old noise");
        let before = app.cell.handle();

        run(&mut app, "/new");

        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "the conversation is gone"
        );
        assert!(
            app.chat.notices_for(AgentId::ROOT).next().is_none(),
            "notices are gone"
        );
        assert_eq!(app.tree.agents.len(), 1, "the tree is reset to the root");
        assert!(!app.busy());
        // The respawned root owns a fresh config cell and the UI adopted its
        // handle; without that, a later /model would write the old tree's cell
        // and the live actor would never hear about it.
        assert!(
            !before.same_cell(&app.cell.handle()),
            "the UI moved to the new tree's cell"
        );
        app.cell.edit(|cfg| cfg.set_model("after-the-restart"));
        assert_ne!(
            before.config().unwrap().model,
            "after-the-restart",
            "and the old tree's cell is no longer the one it writes"
        );

        // Its mailbox is alive, so the next message starts a run instead of
        // reporting that the root is gone.
        app.chat.insert("hello");
        app.send_message();
        assert!(app.busy());
        assert_eq!(app.tree.agents[0].phase, Phase::Thinking);
    }

    /// An actor `/new` abandoned can still be finishing a request (up to the
    /// HTTP timeout); its events must not land in the new conversation. Ids
    /// collide by design — the new root is #0 too.
    #[test]
    fn events_from_an_abandoned_conversation_are_ignored() {
        let (mut app, _rx) = test_app("stale");
        let abandoned = app.tree.conversation();
        run(&mut app, "/new");
        assert_ne!(app.tree.conversation(), abandoned, "a new conversation tag");
        app.chat
            .push_message(AgentId::ROOT, Message::user("current work"));

        app.update(Msg::Agent {
            conversation: abandoned,
            id: AgentId::ROOT,
            event: AgentEvent::Message(Message::assistant("stale reply")),
        });
        app.update(Msg::Agent {
            conversation: abandoned,
            id: AgentId::ROOT,
            event: AgentEvent::Compact {
                in_run: false,
                summary: "stale summary".to_string(),
            },
        });
        assert_eq!(
            app.chat.transcript(AgentId::ROOT).len(),
            1,
            "the stale reply and summary are dropped"
        );

        app.update(Msg::Agent {
            conversation: app.tree.conversation(),
            id: AgentId::ROOT,
            event: AgentEvent::Message(Message::assistant("fresh reply")),
        });
        assert_eq!(
            app.chat.transcript(AgentId::ROOT).len(),
            2,
            "the live conversation still lands"
        );
    }

    /// A steering message sent while the root is busy is folded into the run,
    /// and it must also be visible: the human has to see what they said.
    #[test]
    fn steering_text_is_echoed_in_the_chat() {
        let (mut app, _rx) = test_app("steer");
        app.tree.begin(AgentId::ROOT, None);
        app.chat.insert("also rename the module");

        app.send_message();

        assert_eq!(
            app.chat.transcript(AgentId::ROOT).len(),
            1,
            "the steering message is echoed"
        );
        assert_eq!(
            app.chat.transcript(AgentId::ROOT)[0].text(),
            "also rename the module"
        );
        assert_eq!(text_of(&app), "noted — folded in as the agent continues");
    }

    /// The meter counts the human's own words as soon as they are sent, not
    /// only once a reply arrives (finding B8).
    #[test]
    fn the_context_meter_counts_the_human_message() {
        let (mut app, _rx) = test_app("meter");
        let before = app.context_used_tokens();
        app.chat.insert("a question long enough to weigh something");
        app.send_message();
        assert!(
            app.context_used_tokens() > before,
            "the meter ignores the human's message"
        );
    }

    /// The meter follows the pane: a subagent's conversation is weighed against
    /// its own transcript, not the root's, because that transcript is what its
    /// next request will send.
    #[test]
    fn the_meter_measures_the_open_conversation() {
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::user("x".repeat(300)));
        let root = chat.used_tokens_for(AgentId::ROOT);
        assert!(root > 0);
        assert_eq!(
            chat.used_tokens_for(AgentId(7)),
            0,
            "nothing has been said to that agent"
        );

        chat.push_message(AgentId(7), Message::user("y".repeat(900)));
        assert!(
            chat.used_tokens_for(AgentId(7)) > root,
            "a longer child conversation weighs more than the root's"
        );
        assert_eq!(
            chat.used_tokens_for(AgentId::ROOT),
            root,
            "the root's own number is unchanged by a child's"
        );
    }

    /// The facts line says how full the conversation is as well as how big the
    /// window is: the window alone cannot tell a human whether the next message
    /// will compact.
    #[test]
    fn the_context_meter_shows_used_over_window() {
        let (mut app, _rx) = test_app("meter-label");
        // The system prompt is part of every request, so the meter starts at its
        // weight — never at zero.
        let empty = app.context_meter();
        assert!(
            empty.starts_with("ctx ")
                && empty.ends_with(&format!("~{}", tokens_label(app.cfg().context_tokens))),
            "{empty}"
        );

        app.chat.insert("a question long enough to weigh something");
        app.send_message();
        let meter = app.context_meter();
        assert_ne!(meter, empty, "the human's words are counted: {meter}");

        // A window the human stated is not marked as derived, and is shown as
        // the number it is: 32,768 tokens is `32.8k`, not `32k`.
        app.cell.edit(|cfg| cfg.set_context(32_768));
        assert!(
            app.context_meter().ends_with("32.8k"),
            "{}",
            app.context_meter()
        );
        assert!(
            !app.context_meter().contains('~'),
            "{}",
            app.context_meter()
        );
    }

    /// A window an actor learned reaches the UI's cell, through the event that
    /// announced it — not as a mutex write the UI never hears about (finding
    /// B7). The bar, `/context` and the tool caps read one number, and the
    /// actors holding a handle from before the event measure against the same
    /// one.
    #[test]
    fn a_window_learned_by_an_actor_reaches_the_ui_cell() {
        let (mut app, _rx) = test_app("learned-window");
        // Taken first, the way the root actor's context holds it for the life
        // of the tree.
        let handle = app.cell.handle();
        let first = app.cfg().context_tokens;
        assert_ne!(first, 4_096, "the test wants a window that changes");

        app.update(Msg::Agent {
            conversation: app.tree.conversation(),
            id: AgentId::ROOT,
            event: AgentEvent::Context {
                tokens: 4_096,
                source: WindowSource::Complaint,
            },
        });

        assert_eq!(
            app.cfg().context_tokens,
            4_096,
            "the bar, /context and the caps read the learned window"
        );
        assert!(
            !app.cfg().context_explicit,
            "and they still read it as learned, not as the human's"
        );
        assert_eq!(
            handle.config().unwrap().context_tokens,
            4_096,
            "the actors read the same one"
        );
    }

    /// A window the human stated is not the endpoint's to overwrite, whichever
    /// side learns the number first (finding B7's other half).
    #[test]
    fn a_stated_window_survives_what_an_actor_learns() {
        let (mut app, _rx) = test_app("stated-window");
        app.cell.edit(|cfg| cfg.set_context(32_768));
        let handle = app.cell.handle();

        app.update(Msg::Agent {
            conversation: app.tree.conversation(),
            id: AgentId::ROOT,
            event: AgentEvent::Context {
                tokens: 4_096,
                source: WindowSource::Complaint,
            },
        });

        assert_eq!(app.cfg().context_tokens, 32_768);
        assert_eq!(handle.config().unwrap().context_tokens, 32_768);
    }

    /// The runtime switches re-derive the window: a window mush only *learned*
    /// belongs to the endpoint that taught it, and a new endpoint or provider
    /// means a new one (finding A5).
    ///
    /// The switch itself, not `/url` and `/provider`: those arms fetch the
    /// model list after switching, which is a socket the default suite does not
    /// take. The arms call exactly these two functions and nothing else, so the
    /// behaviour they have is the behaviour pinned here.
    #[test]
    fn a_runtime_switch_rederives_the_window() {
        let (mut app, _rx) = test_app("rederive");
        let fallback = app.cfg().fallback_context();
        app.cell.learn_context(4_096, WindowSource::Advertised);
        assert_eq!(app.cfg().context_tokens, 4_096, "learned, not stated");

        app.switch_endpoint("http://127.0.0.1:9999");
        assert_eq!(
            app.cfg().context_tokens,
            fallback,
            "a new endpoint re-derives the window"
        );
        assert_eq!(
            app.cell.handle().config().unwrap().context_tokens,
            fallback,
            "and the actors measure against the re-derived one"
        );

        app.switch_provider(Provider::DeepSeek);
        assert_eq!(app.cfg().base_url, "https://api.deepseek.com");
        assert_eq!(app.cfg().model, "deepseek-flash", "the vendor's own model");
        assert_eq!(
            app.cfg().context_tokens,
            500_000,
            "and the window documented for it"
        );
        assert_eq!(
            app.cell.handle().config().unwrap().context_tokens,
            500_000,
            "the actors follow the switch"
        );

        // A window the human stated is theirs: neither switch touches it.
        app.cell.edit(|cfg| cfg.set_context(32_768));
        app.switch_endpoint("http://127.0.0.1:9998");
        app.switch_provider(Provider::Custom);
        assert_eq!(
            app.cfg().context_tokens,
            32_768,
            "what the human said stays"
        );
    }

    /// The model list is discovered on its own thread, so it arrives after the
    /// first frame (finding A9). Its first entry is the guess only when nothing
    /// else named a model, its advertised window comes with it, and a model the
    /// human picked is never overwritten by whatever the endpoint lists first.
    #[test]
    fn a_late_model_list_names_a_model_only_when_nothing_did() {
        let (mut app, _rx) = test_app("late-models");
        let endpoint = app.cfg().base_url.clone();
        // What "no model given and none known yet" looks like after the
        // precedence chain: mush started without one.
        app.cell.edit(|cfg| cfg.model.clear());

        app.update(Msg::Models {
            endpoint: endpoint.clone(),
            models: vec![http::Model {
                id: "found".to_string(),
                context: Some(4_096),
            }],
        });

        assert_eq!(app.cfg().model, "found", "the endpoint's first model");
        assert_eq!(
            app.cfg().context_tokens,
            4_096,
            "and the window it advertised for it"
        );
        assert_eq!(
            app.cell.handle().config().unwrap().model,
            "found",
            "the actors ask for the model the UI shows"
        );
        assert!(
            text_of(&app).contains("found"),
            "a model that appeared on its own is said out loud: {}",
            text_of(&app)
        );

        app.cell.edit(|cfg| cfg.set_model("mine"));
        app.update(Msg::Models {
            endpoint,
            models: vec![http::Model {
                id: "other".to_string(),
                context: None,
            }],
        });
        assert_eq!(app.cfg().model, "mine", "a stated model wins over the list");
    }

    /// A list fetched from the endpoint the human has since left must not land:
    /// the picker, and the model it would name, are about the endpoint in use.
    #[test]
    fn a_model_list_from_an_endpoint_mush_left_is_dropped() {
        let (mut app, _rx) = test_app("stale-models");
        app.cell.edit(|cfg| cfg.model.clear());

        app.update(Msg::Models {
            endpoint: "http://127.0.0.1:2".to_string(),
            models: vec![http::Model {
                id: "from-elsewhere".to_string(),
                context: None,
            }],
        });

        assert!(app.models.is_empty(), "nothing was adopted");
        assert!(app.cfg().model.is_empty(), "and no model was named");
    }

    /// A request with no model is a guaranteed refusal, and discovery now runs
    /// after the first frame, so this state is reachable for as long as a fetch
    /// takes: the human is told, rather than reading the endpoint's complaint
    /// about an empty model id (finding A9).
    #[test]
    fn a_message_with_no_model_says_so_instead_of_asking() {
        let (mut app, _rx) = test_app("no-model");
        app.cell.edit(|cfg| cfg.model.clear());

        app.chat.insert("hello");
        app.send_message();

        assert!(!app.busy(), "no run was started");
        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "and nothing was put in the transcript an endpoint would be sent"
        );
        assert!(
            text_of(&app).contains("no model"),
            "the bar says why: {}",
            text_of(&app)
        );
        assert_eq!(
            app.chat.input().text(),
            "hello",
            "and the words are still in the box to retry"
        );
    }

    /// A failed nudge puts the node's phase back exactly as it was (finding
    /// B10); a dead mailbox must not rewrite a `✓` into `· idle`.
    #[test]
    fn a_failed_nudge_restores_the_phase() {
        let (mut app, _rx) = test_app("nudge-restore");
        app.focus = Focus::Agents;
        // A finished child with a summary, whose actor is gone: the nudge below
        // cannot be delivered, and the row must not end up claiming work.
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            cmd: crossbeam_channel::unbounded().0,
        });
        app.tree
            .finish(AgentId(1), Some("did the work".to_string()));
        app.tree.agent_tx.remove(&AgentId(1));
        app.tree.focus(AgentId(1));
        app.chat.insert("one more thing");

        app.send_message();

        assert_eq!(
            app.tree.node(AgentId(1)).unwrap().phase,
            Phase::Done,
            "the ✓ is not rewritten"
        );
        assert_eq!(text_of(&app), "agent #1 is gone");
    }

    /// `Enter` on a tree row shows that agent *and* hands it the keyboard
    /// (finding S2): typing then reaches the agent, the letters are a message
    /// and not tree bindings, and the bar's badge moves with the keyboard.
    #[test]
    fn enter_on_a_row_moves_the_keyboard_with_the_focus() {
        let (mut app, _rx) = test_app("enter-focus");
        app.focus = Focus::Agents;
        // Two rows, so a leaked `g`/`G` would move the cursor somewhere the
        // message could hide.
        let (cmd, mailbox) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            cmd,
        });
        app.tree.insert(Spawn {
            id: AgentId(2),
            parent: AgentId::ROOT,
            brief: "parser".to_string(),
            depth: 1,
            branch: None,
            cmd: crossbeam_channel::unbounded().0,
        });
        app.tree.cursor_top();
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.tree.cursor_id(), Some(AgentId(1)));

        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.tree.focused, AgentId(1), "the pane shows #1");
        assert_eq!(app.focus, Focus::Chat, "and the keyboard went with it");
        let cursor = app.tree.cursor();
        assert!(
            screen(&mut app, 80, 24)
                .iter()
                .any(|row| row.contains(" chat ")),
            "the bar's badge agrees with the key table"
        );

        // `g`, the space and `c` are ordinary letters now.
        for ch in "go c".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        match mailbox.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::Nudge(text)) => assert_eq!(text, "go c"),
            other => panic!("the words must reach #1, not the tree: {:?}", other.is_ok()),
        }
        assert_eq!(
            app.tree.cursor(),
            cursor,
            "a `g` in the message must not jump the cursor"
        );
        assert_eq!(
            app.tree.node(AgentId(1)).unwrap().phase,
            Phase::Thinking,
            "a `c` in the message must not cancel the agent"
        );
    }

    /// The other half of the same rule: while the chat owns the keyboard, `c`
    /// is a letter, not a tree binding (finding S2).
    #[test]
    fn a_c_in_the_chat_types_a_c_and_cancels_nothing() {
        let (mut app, _rx) = test_app("chat-c");
        app.focus = Focus::Chat;
        let (cmd, mailbox) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            cmd,
        });
        app.tree.begin(AgentId(1), None);
        app.tree.focus(AgentId(1));

        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));

        assert_eq!(app.chat.input().text(), "c", "the letter went into the box");
        assert!(mailbox.try_recv().is_err(), "no Stop was sent");
        assert!(
            app.tree.node(AgentId(1)).unwrap().phase.is_busy(),
            "the agent is still running"
        );
    }

    /// `/compact` asks the *focused* agent to fold its conversation, and says
    /// so on the bar. Nothing about the row's phase changes: the fold is the
    /// actor's job, and its `Compact` event is what replaces the transcript.
    #[test]
    fn compact_asks_the_focused_agent() {
        let (mut app, _rx) = test_app("compact-focus");
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            cmd: crossbeam_channel::unbounded().0,
        });
        let (mailbox, asked) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.agent_tx.insert(AgentId(1), mailbox);
        app.tree.focus(AgentId(1));
        let before = app.tree.node(AgentId(1)).unwrap().phase.clone();

        run(&mut app, "/compact");

        assert!(
            matches!(asked.try_recv(), Ok(AgentMsg::Compact(_))),
            "the request goes to the agent whose pane is focused"
        );
        assert_eq!(text_of(&app), "compacting #1…");
        assert_eq!(
            app.tree.node(AgentId(1)).unwrap().phase,
            before,
            "a fold is not a run, so the row is not put to work"
        );
        // The root is a different agent: its mailbox is not what was written to.
        assert!(asked.try_recv().is_err());
    }

    /// A `/compact` that cannot be delivered says so, the way a nudge does —
    /// and leaves the row exactly as it was (finding B10).
    #[test]
    fn compact_on_a_dead_mailbox_does_not_lie_on_the_row() {
        let (mut app, _rx) = test_app("compact-dead");
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            cmd: crossbeam_channel::unbounded().0,
        });
        app.tree
            .finish(AgentId(1), Some("did the work".to_string()));
        app.tree.agent_tx.remove(&AgentId(1));
        app.tree.focus(AgentId(1));

        run(&mut app, "/compact");

        assert_eq!(text_of(&app), "agent #1 is gone");
        assert_eq!(
            app.tree.node(AgentId(1)).unwrap().phase,
            Phase::Done,
            "no phase is claimed for work nobody is doing"
        );
    }

    /// `/help` is the list a human reads to find out what exists.
    #[test]
    fn help_advertises_compact() {
        let (mut app, _rx) = test_app("compact-help");
        run(&mut app, "/help");
        let help = app
            .chat
            .notices_for(AgentId::ROOT)
            .map(|notice| notice.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(help.contains("/compact"), "{help}");
    }

    /// A fold the human asked for is on the screen while it runs, at both sizes
    /// the audit photographs: the row wears its own glyph and words, the bar
    /// says what happens to the words they are about to type, and the
    /// transcript's foot repeats the fold rather than `working…` (finding U11).
    #[test]
    fn a_fold_in_flight_is_painted_on_every_surface() {
        let (mut app, _rx) = test_app("compact-painted");
        app.chat
            .push_message(AgentId::ROOT, Message::user("fold this".to_string()));
        let flag = Arc::new(AtomicBool::new(false));
        app.on_agent(
            AgentId::ROOT,
            AgentEvent::Compacting {
                why: Compacting::Requested,
                cancel: Some(flag.clone()),
            },
        );

        for (width, height) in [(80u16, 24u16), (40, 10)] {
            let rows = screen(&mut app, width, height);
            let painted = rows.join("\n");
            assert!(
                painted.contains("≡ #0"),
                "the row says a fold, not a run, at {width}×{height}: {rows:?}"
            );
            assert!(
                painted.contains("compacting 0s") || painted.contains("compacting 1s"),
                "with the fold's own words at {width}×{height}: {rows:?}"
            );
            assert!(
                painted.contains("keep typing"),
                "and the human's question answered at {width}×{height}: {rows:?}"
            );
            assert!(
                !painted.contains("working…"),
                "a fold is not the run's own model call at {width}×{height}: {rows:?}"
            );
        }

        // The box still takes a line while the fold runs: mush never blocks
        // input, and the sentence the bar prints says what happens to the
        // words. What proves it is the painted box, not the buffer: a line can
        // be in the box and nowhere on the screen.
        app.focus = Focus::Chat;
        for letter in "noted".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(letter), KeyModifiers::NONE));
        }
        let rows = screen(&mut app, 40, 10).join("\n");
        assert!(rows.contains("noted"), "the box took the line: {rows}");
        assert_eq!(app.chat.input().text(), "noted");
    }

    /// When the fold lands the state leaves the screen, and the transcript says
    /// what happened to the conversation instead (finding U11).
    #[test]
    fn a_landed_fold_takes_its_state_off_the_screen() {
        let (mut app, _rx) = test_app("compact-landed");
        app.chat
            .push_message(AgentId::ROOT, Message::user("fold this".to_string()));
        app.on_agent(
            AgentId::ROOT,
            AgentEvent::Compacting {
                why: Compacting::Requested,
                cancel: None,
            },
        );
        assert!(screen(&mut app, 80, 24).join("\n").contains("compacting"));

        app.on_agent(
            AgentId::ROOT,
            AgentEvent::Compact {
                summary: "the task, and where it got to".to_string(),
                in_run: false,
            },
        );
        for (width, height) in [(80u16, 24u16), (40, 10)] {
            let painted = screen(&mut app, width, height).join("\n");
            assert!(
                !painted.contains("compacting") && !painted.contains('≡'),
                "nothing still claims to be folding at {width}×{height}: {painted}"
            );
        }
        assert_eq!(app.tree_line(), None, "and the bar stops saying so");
        assert_eq!(
            app.tree.node(AgentId::ROOT).unwrap().phase,
            Phase::Done,
            "the fold that landed is a thing that finished"
        );
    }

    /// A fold that fails or is stopped takes its state off the row too: the
    /// notice owns what went wrong, and the `≡` does not outlive the request.
    #[test]
    fn a_fold_that_ends_without_landing_leaves_a_quiet_row() {
        let (mut app, _rx) = test_app("compact-ended");
        app.on_agent(
            AgentId::ROOT,
            AgentEvent::Compacting {
                why: Compacting::NearlyFull,
                cancel: None,
            },
        );
        app.on_agent(AgentId::ROOT, AgentEvent::CompactingEnded { in_run: false });
        let painted = screen(&mut app, 80, 24).join("\n");
        assert!(!painted.contains("compacting"), "{painted}");
        assert_eq!(app.tree.node(AgentId::ROOT).unwrap().phase, Phase::Idle);
    }

    /// The bar's derived sentence answers "may I keep typing?" while the fold
    /// runs, and it is the *same* fact the row draws — one derivation, so the
    /// two cannot disagree about whether anything is folding (finding U11).
    #[test]
    fn the_bar_answers_may_i_keep_typing_while_a_fold_runs() {
        let (mut app, _rx) = test_app("compact-bar");
        assert_eq!(app.tree_line(), None, "nothing to report at rest");

        app.tree.compacting(AgentId::ROOT, Compacting::Parked, None);
        assert_eq!(
            app.tree_line().as_deref(),
            Some("compacting #0 · keep typing — your message is answered after the fold"),
            "a parked fold is what the human is waiting for"
        );
        let rows = screen(&mut app, 80, 24).join("\n");
        assert!(rows.contains("keep typing"), "{rows}");

        app.tree.compacted(AgentId::ROOT, false);
        assert_eq!(app.tree_line(), None, "and it goes when the fold does");
    }

    /// Ctrl-C reaches a fold from rest: the fold's `Compacting` event carries
    /// the only handle there is to the summarize call, and the UI's Stop looks
    /// exactly where that handle is put (findings U11 and B6).
    #[test]
    fn a_stop_reaches_the_summarize_call_of_a_fold_from_rest() {
        let (mut app, _rx) = test_app("compact-stop");
        let flag = Arc::new(AtomicBool::new(false));
        app.on_agent(
            AgentId::ROOT,
            AgentEvent::Compacting {
                why: Compacting::Requested,
                cancel: Some(flag.clone()),
            },
        );

        app.interrupt_all();

        assert!(
            flag.load(Ordering::SeqCst),
            "the Stop the human pressed reached the summarize request"
        );
        assert_eq!(
            app.tree.node(AgentId::ROOT).unwrap().phase,
            Phase::Cancelling,
            "and the row says the stop is on its way"
        );
    }

    /// `/help` renders the same key table `mush --help` does, so a human who
    /// learns the keyboard from the notice can discover every binding instead
    /// of the six the old hand-written line named — and discover the keys that
    /// really scroll (finding K3), not a wheel mush never takes.
    #[test]
    fn help_names_the_whole_key_table() {
        let (mut app, _rx) = test_app("keys-help");
        run(&mut app, "/help");
        let help = app
            .chat
            .notices_for(AgentId::ROOT)
            .map(|notice| notice.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        for want in [
            "j / k, ↑ / ↓",
            "focus the selected agent",
            "cancel the selected agent",
            "back to the root agent",
            "←",
            "→",
            "Ctrl-Q",
            "↑ / ↓, PgUp / PgDn",
            "page up / down the rows",
        ] {
            assert!(
                help.contains(want),
                "`{want}` is missing from /help:\n{help}"
            );
        }
        assert!(
            !help.contains("wheel"),
            "a wheel it does not scroll:\n{help}"
        );
    }

    /// `←`/`→` in the agents pane walk the tree by its parent links, over the
    /// *painted* pre-order rows, not the storage order (finding U10): from a
    /// great-grandchild `←` reaches the root one ancestor per press, `→` walks
    /// back down the same chain, and `←` follows the parent link rather than the
    /// row above it.
    #[test]
    fn left_and_right_walk_the_tree_by_its_parent_links() {
        let (mut app, _rx) = test_app("tree-walk");
        app.focus = Focus::Agents;
        // root(0) → #1 → #2 → #3, plus #4 as the root's second child, spawned
        // *before* #2 and #3. Storage order is therefore 0,1,4,2,3, while the
        // painted pre-order is 0,1,2,3,4 — the two orders differ, so the test
        // can tell which one the cursor walks.
        let mut _mailboxes = Vec::new();
        for (id, parent, depth) in [(1u64, 0u64, 1usize), (4, 0, 1), (2, 1, 2), (3, 2, 3)] {
            let (cmd, rx) = crossbeam_channel::unbounded::<AgentMsg>();
            _mailboxes.push(rx);
            app.tree.insert(Spawn {
                id: AgentId(id),
                parent: AgentId(parent),
                brief: format!("child {id}"),
                depth,
                branch: None,
                cmd,
            });
        }
        let stored: Vec<u64> = app.tree.agents.iter().map(|node| node.id.0).collect();
        let painted: Vec<u64> = app.tree.rows().iter().map(|node| node.id.0).collect();
        assert_eq!(stored, vec![0, 1, 4, 2, 3], "storage is spawn order");
        assert_eq!(
            painted,
            vec![0, 1, 2, 3, 4],
            "rows are the painted pre-order"
        );
        // `G` walks the painted rows: the bottom row is #4, not the storage
        // vector's last (#3).
        app.tree.cursor_bottom();
        assert_eq!(app.tree.cursor_id(), Some(AgentId(4)));

        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);

        // Select the great-grandchild #3 (painted index 3) and walk up. (The
        // tree's `move_cursor` moves one row per call, so three calls.)
        app.tree.cursor_top();
        for _ in 0..3 {
            app.tree.move_cursor(1);
        }
        assert_eq!(app.tree.cursor_id(), Some(AgentId(3)));
        app.on_key(left);
        assert_eq!(app.tree.cursor_id(), Some(AgentId(2)), "#3's parent");
        app.on_key(left);
        assert_eq!(app.tree.cursor_id(), Some(AgentId(1)), "its grandparent");
        app.on_key(left);
        assert_eq!(app.tree.cursor_id(), Some(AgentId::ROOT), "three levels up");
        // The root has no parent: the cursor stays where it is, an honest no-op.
        app.on_key(left);
        assert_eq!(app.tree.cursor_id(), Some(AgentId::ROOT));
        // `→` is the companion: the root's first child, then down the chain.
        app.on_key(right);
        assert_eq!(
            app.tree.cursor_id(),
            Some(AgentId(1)),
            "the root's first child"
        );
        app.on_key(right);
        assert_eq!(app.tree.cursor_id(), Some(AgentId(2)));
        app.on_key(right);
        assert_eq!(
            app.tree.cursor_id(),
            Some(AgentId(3)),
            "down the same chain"
        );
        // A leaf has no child: the cursor stays.
        app.on_key(right);
        assert_eq!(app.tree.cursor_id(), Some(AgentId(3)));

        // #4's painted row is directly below #3's, but its parent is the root:
        // `←` follows the link (root), not row-minus-one (#3).
        app.tree.cursor_bottom();
        assert_eq!(app.tree.cursor_id(), Some(AgentId(4)));
        app.on_key(left);
        assert_eq!(
            app.tree.cursor_id(),
            Some(AgentId::ROOT),
            "the parent link, not the row above"
        );
    }

    /// `PgUp`/`PgDn` in the agents pane move the cursor a whole page, stop at
    /// both ends of the painted rows, and leave the selection on the row the
    /// pane paints as selected — a run with twenty-four children is paged, not
    /// walked a row at a time.
    #[test]
    fn page_keys_move_the_tree_cursor_a_page_and_clamp_at_both_ends() {
        let (mut app, _rx) = test_app("tree-page");
        app.focus = Focus::Agents;
        // The receivers are kept for the test's life, so the children's
        // mailboxes stay open: a child whose actor has gone is a row the pane
        // still paints, but this is the tree a run builds.
        let mut _mailboxes = Vec::new();
        for id in 1..=24u64 {
            let (cmd, mailbox) = crossbeam_channel::unbounded::<AgentMsg>();
            _mailboxes.push(mailbox);
            app.tree.insert(Spawn {
                id: AgentId(id),
                parent: AgentId::ROOT,
                brief: format!("child {id}"),
                depth: 1,
                branch: None,
                cmd,
            });
        }
        let painted: Vec<AgentId> = app.tree.rows().iter().map(|node| node.id).collect();
        assert_eq!(painted.len(), 25, "the root and twenty-four children");
        assert!(
            painted.len() > 18,
            "more rows than an 80x24 pane shows, so the bottom is reached by paging"
        );
        let bottom = *painted.last().unwrap();

        let down = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        // The page is whatever the key table says it is, so this test cannot
        // disagree with `keys.rs` about the distance.
        let page = match keys::key(Focus::Agents, false, down) {
            Intent::TreeMove(step) => step,
            other => panic!("PgDn in the agents pane is not a page: {other:?}"),
        };
        assert!(page > 1, "a page is more than a row");
        let page = page as usize;

        app.tree.cursor_top();
        app.on_key(down);
        assert_eq!(
            app.tree.cursor_id(),
            Some(painted[page]),
            "one page down the painted rows"
        );
        app.on_key(down);
        assert_eq!(app.tree.cursor_id(), Some(painted[page * 2]), "two pages");
        // Only five rows are left: the third page clamps on the last painted
        // row instead of wrapping to the top or running off the end.
        app.on_key(down);
        assert_eq!(app.tree.cursor_id(), Some(bottom), "clamped at the bottom");
        app.on_key(down);
        assert_eq!(
            app.tree.cursor_id(),
            Some(bottom),
            "and the end stays the end"
        );

        app.on_key(up);
        assert_eq!(
            app.tree.cursor_id(),
            Some(painted[page + 4]),
            "a page back up"
        );
        app.on_key(up);
        app.on_key(up);
        assert_eq!(app.tree.cursor_id(), Some(painted[0]), "clamped at the top");
        app.on_key(up);
        assert_eq!(
            app.tree.cursor_id(),
            Some(painted[0]),
            "and the top stays the top"
        );

        // The selection is the row that becomes visible: the pane scrolls to
        // follow the cursor, and the last row sits below an eighteen-row
        // window. At the top of the list it is not on the pane at all — the
        // control that says the check below is about following the cursor and
        // not about a row that happened to be painted.
        app.tree.cursor_top();
        let top_view = screen(&mut app, 80, 24);
        assert!(
            !top_view
                .iter()
                .any(|row| row.contains(&format!("#{bottom}"))),
            "the bottom row is outside the pane's window at the top of the list:\n{}",
            top_view.join("\n")
        );

        app.on_key(down);
        app.on_key(down);
        app.on_key(down);
        assert_eq!(app.tree.cursor_id(), Some(bottom));
        let rows = screen(&mut app, 80, 24);
        // The row and the cursor row's footer both name the agent; the row is
        // the one the pane paints as selected, and it says so with the
        // highlight alone — no marker beside it.
        let named: Vec<&String> = rows
            .iter()
            .filter(|row| row.contains(&format!("#{bottom}")))
            .collect();
        assert!(
            !named.is_empty(),
            "the page's row is on the pane now — the list followed the cursor:\n{}",
            rows.join("\n")
        );
        let selected = selected_rows(&mut app, 80, 24);
        assert_eq!(
            selected.len(),
            1,
            "exactly one row wears the pane's selection highlight: {selected:?}\n{}",
            rows.join("\n")
        );
        assert!(
            rows[selected[0]].contains(&format!("#{bottom}")),
            "and it is the row the pane paints as selected:\n{}",
            rows.join("\n")
        );
    }

    /// `←` in the chat pane is the message box's cursor and moves nothing in the
    /// tree: the two panes keep their own meaning for the same key.
    #[test]
    fn left_in_the_chat_pane_leaves_the_tree_alone() {
        let (mut app, _rx) = test_app("chat-left");
        app.focus = Focus::Chat;
        app.tree.cursor_bottom();
        let before = app.tree.cursor_id();
        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(
            app.tree.cursor_id(),
            before,
            "the chat does not move the tree"
        );
    }

    /// A tree `/new` abandoned can still spawn children, and their `Spawned`
    /// events are dropped — so this is the only moment the UI can tell such a
    /// child to go away. Without it the child runs unseen forever.
    #[test]
    fn a_child_spawned_by_an_abandoned_tree_is_shut_down() {
        let (mut app, _rx) = test_app("stale-child");
        let abandoned = app.tree.conversation();
        run(&mut app, "/new");
        let (child_tx, child_rx) = crossbeam_channel::unbounded::<AgentMsg>();

        app.update(Msg::Agent {
            conversation: abandoned,
            id: AgentId(1),
            event: AgentEvent::Spawned {
                child: 1,
                parent: 0,
                brief: "sneaky".to_string(),
                depth: 1,
                branch: None,
                cmd: child_tx,
            },
        });

        assert!(
            matches!(child_rx.recv().unwrap(), AgentMsg::Shutdown),
            "the stray child is told to end"
        );
        assert_eq!(app.tree.agents.len(), 1, "and it never joins the new tree");
    }

    /// Ctrl-C cancels work; it must not end an idle root, which only comes
    /// back with `/new`.
    #[test]
    fn ctrl_c_stops_running_agents_only() {
        let (mut app, _rx) = test_app("interrupt");
        app.interrupt();
        assert_eq!(
            text_of(&app),
            "nothing running · Ctrl-Q quits · Ctrl-N starts a new chat"
        );

        app.chat.insert("hello");
        app.send_message();
        assert_eq!(
            app.tree.agents[0].phase,
            Phase::Thinking,
            "the idle root is usable"
        );

        let (mut app, _rx) = test_app("interrupt-running");
        app.tree.begin(AgentId::ROOT, None);
        app.interrupt();
        assert_eq!(
            app.tree.agents[0].phase,
            Phase::Cancelling,
            "the row must show that a cancel is in flight"
        );
        let rows = screen(&mut app, 120, 32);
        assert!(
            rows.iter()
                .any(|row| row.contains("⊘ #0") && row.contains("cancelling…")),
            "the row is where a cancel in flight is drawn: {rows:?}"
        );
    }

    /// The cancel mark lasts exactly as long as the cancel does: the actor
    /// acknowledges by ending the run, and a fresh run clears it too.
    #[test]
    fn a_cancel_mark_clears_when_the_actor_yields() {
        let (mut app, _rx) = test_app("cancel-mark");
        let conversation = app.tree.conversation();
        app.tree.begin(AgentId::ROOT, None);
        app.interrupt();
        assert_eq!(app.tree.agents[0].phase, Phase::Cancelling);

        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Stopped,
        });
        assert_eq!(
            app.tree.agents[0].phase,
            Phase::Stopped,
            "a stopped run is its own state, not Idle and not Done"
        );

        app.tree.begin(AgentId::ROOT, None);
        app.interrupt();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        });
        assert_eq!(
            app.tree.agents[0].phase,
            Phase::Thinking,
            "a new run is running"
        );
    }

    /// An agent that has not run is not done, and it is not busy: the row must
    /// say so rather than claim a `✓` it never earned.
    #[test]
    fn an_idle_agent_is_neither_done_nor_busy() {
        let (app, _rx) = test_app("idle-phase");
        assert_eq!(app.tree.agents[0].phase, Phase::Idle);
        assert!(!app.busy());
        assert_eq!(app.tree_line(), None, "nothing to report");
    }

    /// The stale-status defect: a finished run must leave nothing behind. Every
    /// line about work in progress is derived from the phase, so when the run
    /// ends they all stop existing at once.
    #[test]
    fn a_finished_run_leaves_nothing_behind() {
        let (mut app, _rx) = test_app("finished-phase");
        let conversation = app.tree.conversation();
        app.tree.begin(AgentId::ROOT, None);
        app.tree.age(AgentId::ROOT, Duration::from_secs(70));
        assert!(
            screen(&mut app, 120, 32)
                .iter()
                .any(|row| row.contains("◐ #0") && row.contains("thinking 1m10s")),
            "a run in flight names its age"
        );

        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Done,
        });
        assert_eq!(app.tree.agents[0].phase, Phase::Done);
        assert!(!app.busy());
        let rows = screen(&mut app, 120, 32).join("\n");
        assert!(!rows.contains("thinking"), "no `thinking` survives: {rows}");
    }

    /// The pane title counts the phases it names, and no agent is in two of its
    /// counts (finding U2).
    ///
    /// It used to say `N running` over every busy phase, so an agent napping on
    /// its children — a row wearing `⏸` — was counted as work the title could
    /// not show. The counts now come from the phases as two disjoint buckets,
    /// and each clause says which one it is.
    #[test]
    fn the_title_counts_working_and_waiting_agents_separately() {
        let (mut app, _rx) = test_app("title-counts");
        let conversation = app.tree.conversation();
        // The root naps on two children, and one of those has a child of its
        // own: three agents work, one waits, and nobody is both.
        for (id, parent, depth) in [(1u64, 0u64, 1usize), (2, 0, 1), (3, 2, 2)] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId(parent),
                event: AgentEvent::Spawned {
                    child: id,
                    parent,
                    brief: format!("child {id}"),
                    depth,
                    branch: None,
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Done,
        });
        // A branch with work on it, so the totals have something to say and
        // must yield the line to the counts rather than be cut in half.
        app.tree.agent_stats.insert(
            AgentId(1),
            mush_core::git::Stat {
                files: 1,
                added: 324,
                removed: 40,
            },
        );
        assert_eq!(
            app.tree.roster(),
            crate::app::tree::Roster {
                working: 3,
                waiting: 1
            }
        );

        let rows = screen(&mut app, 200, 50);
        assert!(
            rows[0].contains(" agents · 3 working · 1 waiting"),
            "the title counts what it names: {}",
            rows[0]
        );
        assert!(
            !rows[0].contains("Σ +324 −") || rows[0].contains("Σ +324 −40"),
            "a total is painted whole or not at all: {}",
            rows[0]
        );
    }

    /// A clause that does not fit is dropped whole, and the totals are the last
    /// to go: the pane is at most 46 columns wide, and ` agents · 1 working · 1
    /// waiting · Σ +324 −40` is 44 of them — the half of it that would fit
    /// (`Σ +324 −`) is a total that is not the total.
    #[test]
    fn the_title_elides_clauses_instead_of_cutting_numbers() {
        let (mut app, _rx) = test_app("title-elides");
        app.tree.agent_stats.insert(
            AgentId::ROOT,
            mush_core::git::Stat {
                files: 1,
                added: 324,
                removed: 40,
            },
        );
        // A running child: one agent working, and a root that is waiting on it.
        crowd(&mut app, 1);

        // Widest pane `draw` ever gives this pane: every clause fits, totals
        // included, and each one is whole.
        let wide = screen(&mut app, 200, 50);
        assert!(
            wide[0].contains(" agents · 1 working · 1 waiting · Σ +324 −40"),
            "{}",
            wide[0]
        );

        // The narrow pane (80 columns, where the tree keeps its thirty): the
        // totals go first, then the waiting count, and never mid-number.
        let narrow = screen(&mut app, 80, 24);
        assert!(!narrow[0].contains("Σ"), "{}", narrow[0]);
        assert!(!narrow[0].contains("waiting"), "{}", narrow[0]);
        assert!(narrow[0].contains(" agents · 1 working"), "{}", narrow[0]);
    }

    /// The pane paints tree order, and every key that moves or reads the cursor
    /// follows what it paints: `j`/`k` walk the rows, `g`/`G` land on the first
    /// and last of them, `Enter` focuses the agent under the highlight, and the
    /// footer under the list names that same agent (finding U4).
    #[test]
    fn the_cursor_walks_the_rows_the_pane_paints() {
        let (mut app, _rx) = test_app("row-order");
        let conversation = app.tree.conversation();
        // Spawn order that is not tree order: the root's second child is spawned
        // before the first child's own child, so `#3` belongs under `#1` and
        // above `#2`.
        for (id, parent, depth) in [(1u64, 0u64, 1usize), (2, 0, 1), (3, 1, 2)] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId(parent),
                event: AgentEvent::Spawned {
                    child: id,
                    parent,
                    brief: format!("agent {id}"),
                    depth,
                    branch: None,
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }
        app.focus = Focus::Agents;
        // The pane's own columns only: the bar under it names agents too.
        let pane = |app: &mut App| -> Vec<String> {
            screen(app, 120, 32)
                .into_iter()
                .map(|row| row.chars().take(31).collect())
                .collect()
        };
        let key = |app: &mut App, c: char| {
            app.update(Msg::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        };

        // Painted order: the root, #1, #3 (its child), then #2 — while the
        // storage order is the spawn order 0, 1, 2, 3.
        let rows = pane(&mut app);
        for (row, id) in rows[1..=4].iter().zip(["#0", "#1", "#3", "#2"]) {
            assert!(row.contains(id), "the row for {id} is not there: {row:?}");
        }

        // `j` twice lands on the third painted row, which is #3: storage order
        // would have put its sibling #2 there.
        key(&mut app, 'j');
        key(&mut app, 'j');
        assert_eq!(app.tree.cursor_id(), Some(AgentId(3)));
        assert_eq!(app.tree.agents[2].id, AgentId(2), "storage order is not it");

        // `Enter` focuses the row the highlight is on, and the footer under the
        // list is that same row's facts.
        app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(app.tree.focused, AgentId(3));
        assert_eq!(text_of(&app), "agent #3: agent 3");
        let rows = pane(&mut app);
        assert!(
            rows.iter().any(|row| row.contains("#3 agent 3")),
            "the footer names the cursor row: {rows:?}"
        );

        // `Enter` hands the keyboard to the agent it focused (finding S2), so
        // the rest of this walk — which is about the *rows* — takes it back the
        // way a human does.
        app.focus = Focus::Agents;

        // `k` back up one row is #1, and `G` is the last painted row — the
        // root's second child, whose storage index is 2 of 3.
        key(&mut app, 'k');
        assert_eq!(app.tree.cursor_id(), Some(AgentId(1)));
        key(&mut app, 'G');
        assert_eq!(app.tree.cursor_id(), Some(AgentId(2)));
        key(&mut app, 'g');
        assert_eq!(app.tree.cursor_id(), Some(AgentId::ROOT));
    }

    /// A napping root with live children is derived from the tree: nobody has
    /// to remember to write it, so it cannot be forgotten either.
    #[test]
    fn a_napping_root_reports_its_children() {
        let (mut app, _rx) = test_app("napping-root");
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId(1),
            event: AgentEvent::Spawned {
                child: 1,
                parent: 0,
                brief: "lexer".to_string(),
                depth: 1,
                branch: None,
                cmd: crossbeam_channel::unbounded().0,
            },
        });
        app.update(Msg::Agent {
            conversation,
            id: AgentId(1),
            event: AgentEvent::Status("edit_file src/lex.rs".to_string()),
        });
        assert_eq!(app.tree.agents[0].phase, Phase::Idle, "the root napped");
        assert_eq!(
            app.tree_line().as_deref(),
            Some("waiting on 1 subagent(s) — the root resumes as they finish")
        );
    }

    /// A working agent with working children is drawn working, and the children
    /// are a second mark rather than a replacement glyph (finding U1).
    ///
    /// The row used to derive its glyph from "has live children", so an agent
    /// mid-turn with children running wore `⏸` — "paused" about the one agent
    /// the human was watching work. The glyph is now a function of the agent's
    /// own phase and `⏸N` carries the children, so neither fact hides the other.
    #[test]
    fn a_working_agent_with_working_children_is_not_drawn_paused() {
        let (mut app, _rx) = test_app("waiting-glyph");
        let conversation = app.tree.conversation();
        // The root is mid-turn, and two of its children are working.
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        });
        for id in [1u64, 2u64] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId::ROOT,
                event: AgentEvent::Spawned {
                    child: id,
                    parent: 0,
                    brief: format!("child {id}"),
                    depth: 1,
                    branch: None,
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }

        let rows = screen(&mut app, 120, 32);
        let root_row = rows
            .iter()
            .find(|row| row.contains("#0"))
            .expect("the root has a row")
            .clone();
        assert!(
            root_row.contains("◐ #0"),
            "a working agent is `◐`, never `⏸`: {root_row}"
        );
        assert!(
            root_row.contains("⏸2"),
            "and its two working children are still on the row: {root_row}"
        );

        // A child with no children of its own wears the plain running glyph.
        let child_row = rows
            .iter()
            .find(|row| row.contains("#1"))
            .expect("the child has a row")
            .clone();
        assert!(child_row.contains("◐ #1"), "{child_row}");
        assert!(!child_row.contains("⏸"), "{child_row}");
    }

    /// A pane's position is the human's: another agent's line cannot move it,
    /// and neither can the pane's own line while they are away from the bottom
    /// (finding U3).
    #[test]
    fn news_moves_only_the_pane_it_is_about_and_only_from_the_bottom() {
        let (mut app, _rx) = test_app("scroll-pin");
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Spawned {
                child: 1,
                parent: 0,
                brief: "lexer".to_string(),
                depth: 1,
                branch: None,
                cmd: crossbeam_channel::unbounded().0,
            },
        });
        // A transcript long enough to scroll, in the pane the human is reading.
        for index in 0..30 {
            app.chat
                .push_message(AgentId(1), Message::assistant(format!("line {index}")));
        }
        app.tree.focus(AgentId(1));
        app.chat.scroll_by(AgentId(1), 4);
        let held = chat_rows(&mut app);
        assert!(
            !held.join("\n").contains("line 29"),
            "the pane is away from the newest line: {held:?}"
        );

        // Another agent's news: the root says something of its own.
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Message(Message::assistant("the root's line")),
        });
        assert_eq!(
            chat_rows(&mut app),
            held,
            "another agent's line must not move this pane"
        );

        // And the pane's own agent speaks while the human is away from the
        // bottom: they are still where they put themselves.
        app.update(Msg::Agent {
            conversation,
            id: AgentId(1),
            event: AgentEvent::Message(Message::assistant("its own line")),
        });
        assert_eq!(chat_rows(&mut app), held, "a held pane is the human's");

        // At the bottom the same news is exactly what the pane shows: that is
        // what following means, and it needs nothing to be told to it.
        app.chat.scroll_by(AgentId(1), -4);
        assert!(
            chat_rows(&mut app).join("\n").contains("its own line"),
            "a pane at the bottom follows the newest line"
        );
    }

    /// Busy agents are named with their age, on their own row: a model that
    /// has thought for two minutes should look different from one that has
    /// thought for a second, and the row is where that is read.
    #[test]
    fn busy_agents_are_named_with_their_age() {
        let (mut app, _rx) = test_app("activity-age");
        app.tree.begin(AgentId::ROOT, None);
        app.tree.activity(AgentId::ROOT, "edit_file src/lib.rs");
        app.tree.age(AgentId::ROOT, Duration::from_secs(12));
        let rows = screen(&mut app, 120, 32);
        assert!(
            rows.iter()
                .any(|row| row.contains("◐ #0") && row.contains("edit_file src/lib.rs 12s")),
            "the row names the work and its age: {rows:?}"
        );
        // And the first line of the bar says something else: the activity has
        // two homes already, and the bar has only one line (finding U5).
        let bar = rows.last().expect("the bar is painted");
        assert!(
            !bar.contains("edit_file"),
            "the bar does not repeat the row: {bar:?}"
        );
    }

    /// An informational line is worth reading for a moment; an error is worth
    /// reading until it is fixed or replaced.
    #[test]
    fn info_fades_and_errors_stay() {
        let (mut app, _rx) = test_app("status-life");
        // Nothing is said at startup: the bar's facts line already carries the
        // model and the window, so a status line would only repeat it.
        assert_eq!(text_of(&app), "");

        app.say("saved src/main.rs");
        assert_eq!(text_of(&app), "saved src/main.rs");
        age_status(&mut app, 6);
        assert_eq!(text_of(&app), "", "an info line fades");
        app.tick();
        assert!(app.status.is_none(), "and is dropped, not just hidden");

        app.fail("cannot write /etc/passwd");
        age_status(&mut app, 600);
        assert_eq!(text_of(&app), "cannot write /etc/passwd");
        assert_eq!(
            app.status_line().map(|(_, kind)| kind),
            Some(StatusKind::Error)
        );
        assert_eq!(
            app.status_line().map(|(_, kind)| kind),
            Some(StatusKind::Error)
        );
    }

    /// Ages are read at a glance, so they must not be raw seconds.
    #[test]
    fn ages_read_like_clocks() {
        assert_eq!(short_age(Duration::from_secs(3)), "3s");
        assert_eq!(short_age(Duration::from_secs(70)), "1m10s");
        assert_eq!(short_age(Duration::from_secs(3600 + 120)), "1h02m");
    }

    /// The commands an untrusted word can carry: an OSC title rename, a carriage
    /// return, a bidi isolate. Every one of them has leaked onto some one-line
    /// surface of this screen at least once (findings U9/N3).
    const HOSTILE: &str = "\x1b]0;PWNED\x07\r\u{2066}escaped";

    /// Every size the draw sweep paints at: the seven the §4.5 audit
    /// photographs, plus the tiers' own edges — the compact cut at 80 wide and
    /// 20 tall, the bar's second row at 24 — and the degenerate 1×1 a resize can
    /// reach mid-frame.
    const SWEEP_SIZES: &[(u16, u16)] = &[
        (200, 50),
        (160, 26),
        (120, 32),
        (100, 25),
        (80, 24),
        (79, 24),
        (60, 20),
        (60, 17),
        (50, 12),
        (40, 10),
        (39, 10),
        (39, 9),
        (30, 8),
        (20, 5),
        (1, 1),
    ];

    /// One state of the draw sweep: an app, and the words its frame must have
    /// painted.
    ///
    /// `words` are painted at *every* size with a floor to paint in: the facts
    /// that must survive the smallest screen, and the ones a bug in the size
    /// tiers would take away. `roomy` are painted at every size at least 80×24,
    /// where the transcript's foot, the selected row's footer and the facts line
    /// all have the room they were built for. `reopen` is for the popups whose
    /// item wrapping is derived from the terminal's width *when they open*
    /// (`/notes`): the sweep opens them again for each size, which is what a
    /// human resizing the terminal with the popup up would get.
    struct Sweep {
        name: &'static str,
        app: App,
        words: Vec<&'static str>,
        roomy: Vec<&'static str>,
        reopen: Option<fn(&mut App)>,
    }

    /// One agent, as its parent reports it to the UI.
    fn spawn_agent(
        app: &mut App,
        id: u64,
        parent: u64,
        depth: usize,
        brief: &str,
        branch: Option<&str>,
    ) {
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId(parent),
            event: AgentEvent::Spawned {
                child: id,
                parent,
                depth,
                brief: brief.to_string(),
                branch: branch.map(str::to_string),
                cmd: crossbeam_channel::unbounded().0,
            },
        });
    }

    /// A run in flight on one agent: what its actor reports when it starts.
    fn begin_run(app: &mut App, id: AgentId) {
        app.on_agent(
            id,
            AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
    }

    /// A job running on this machine, launched through the one registry the
    /// rows, the selected row's footer and the bar all read.
    fn running_job(app: &mut App, owner: u64) {
        use crate::jobs::Launch;
        use crate::machine::fake::{Script, Scripted};
        use crate::machine::{Machine, ShellCommand};

        let machine = Arc::new(Scripted::new().runs(Script::hangs()));
        let job = machine
            .spawn(&ShellCommand {
                command: "cargo build --release",
                root: std::path::Path::new("/tmp"),
            })
            .unwrap();
        let (mailbox, _rx) = crossbeam_channel::unbounded();
        app.tree
            .handles()
            .jobs
            .launch(Launch::started(
                owner,
                "cargo build --release".to_string(),
                false,
                mailbox,
                job,
            ))
            .unwrap();
    }

    /// Twenty agents three levels deep, with a branch on one of them, a run in
    /// flight, a failure and a dirty repository: the shape the audit's worst
    /// screen was.
    fn a_twenty_agent_tree(label: &str) -> (App, Receiver<Msg>) {
        let (mut app, rx) = test_app(label);
        for id in 1..=12u64 {
            // One isolated child, one working, one failed: the three states a
            // row's tail can carry.
            let branch = (id == 3).then(|| format!("mush/{id}"));
            spawn_agent(
                &mut app,
                id,
                0,
                1,
                &format!("task {id} for the sweep"),
                branch.as_deref(),
            );
        }
        for id in 13..=19u64 {
            spawn_agent(&mut app, id, 1, 2, &format!("grandchild {id}"), None);
        }
        // A mixed tree: the states a row can wear, so the title's counts count
        // something. Everything `insert` made thinking is put back at rest
        // except the children that stay in flight.
        app.tree.idle(AgentId(1));
        app.tree.activity(AgentId(2), "edit_file src/lexer.rs");
        app.tree
            .finish(AgentId(3), Some("the parser is ported".to_string()));
        app.tree.fail(AgentId(4), "no route to host".to_string());
        app.tree.stopped(AgentId(5));
        for id in 13..=19u64 {
            app.tree.idle(AgentId(id));
        }
        app.tree.agent_stats.insert(
            AgentId(3),
            git::Stat {
                files: 2,
                added: 12,
                removed: 4,
            },
        );
        // A dirty repository read a moment ago, so the facts line has its
        // branch, its dirty count and its delta.
        app.git = Some(git::RepoStatus {
            branch: "main".to_string(),
            dirty: 3,
            stat: git::Stat {
                files: 1,
                added: 9,
                removed: 2,
            },
        });
        app.git_at = Some(Instant::now());
        (app, rx)
    }

    /// The states the sweep paints: one per fact a frame can carry, each built
    /// the way its actor, its file or its keyboard would build it.
    ///
    /// The receivers are handed back with the states so the UI channel each app
    /// was built with stays alive for as long as the app does — a dropped
    /// receiver is a message that never lands.
    fn sweep_states() -> (Vec<Sweep>, Vec<Receiver<Msg>>) {
        let mut states: Vec<Sweep> = Vec::new();
        let mut keep = Vec::new();

        // Nothing has happened yet.
        let (fresh, rx) = test_app("sweep-fresh");
        states.push(Sweep {
            name: "fresh",
            app: fresh,
            words: vec!["· #0", " agents ", " chat ", "Tab cycles panes"],
            roomy: vec!["you", " message ", "Ask for a change"],
            reopen: None,
        });
        keep.push(rx);

        // A run in flight, naming the tool it is running.
        let (mut run, rx) = test_app("sweep-run");
        begin_run(&mut run, AgentId::ROOT);
        run.on_agent(
            AgentId::ROOT,
            AgentEvent::Status("edit_file src/lib.rs".into()),
        );
        states.push(Sweep {
            name: "a run in flight",
            app: run,
            words: vec!["◐ #0", "edit_file src/lib.rs", "working…", " chat "],
            roomy: vec![" agents · 1 working"],
            reopen: None,
        });
        keep.push(rx);

        // A root parked on a child's result — and a grandchild of its own in
        // flight under that child, because the count the bar prints, the `⏸N`
        // on the row and the title's `M waiting` all have to be about the
        // root's *own* children (finding U2).
        let (mut parked, rx) = test_app("sweep-parked");
        spawn_agent(
            &mut parked,
            1,
            0,
            1,
            "build the lexer for the config format",
            None,
        );
        spawn_agent(&mut parked, 2, 1, 2, "tokenize the samples", None);
        states.push(Sweep {
            name: "parked on children",
            app: parked,
            words: vec!["⏸1", "2 working", "waiting on 1 subagent"],
            roomy: vec!["◐ #1", "◐ #2", "lexer", " agents · 2 working · 1 waiting"],
            reopen: None,
        });
        keep.push(rx);

        // The three folds the tree can be doing, each in its own words. The
        // transcript is left empty on purpose: at 40×10 a pane with messages
        // protects its last row, and the foot that carries the fold's longer
        // spelling is the row it protects it *from* — the existing fold test
        // keeps the message case.
        for (label, why, words) in [
            ("sweep-fold-requested", Compacting::Requested, "compacting"),
            (
                "sweep-fold-parked",
                Compacting::Parked,
                "folding at the next step",
            ),
            (
                "sweep-fold-nearly-full",
                Compacting::NearlyFull,
                "context nearly full",
            ),
        ] {
            let (mut fold, rx) = test_app(label);
            fold.on_agent(AgentId::ROOT, AgentEvent::Compacting { why, cancel: None });
            states.push(Sweep {
                name: "a fold",
                app: fold,
                words: vec!["≡ #0", words, "keep typing"],
                roomy: vec!["your message is answered after the fold"],
                reopen: None,
            });
            keep.push(rx);
        }

        // A failed run: the model's own error words, on the row, the foot and
        // the bar.
        let (mut failed, rx) = test_app("sweep-failed");
        begin_run(&mut failed, AgentId::ROOT);
        failed.on_agent(AgentId::ROOT, AgentEvent::Error("no route to host".into()));
        states.push(Sweep {
            name: "a failed run",
            app: failed,
            words: vec!["✗ #0", "no route to host", "agent #0 failed"],
            roomy: vec!["! no route to host"],
            reopen: None,
        });
        keep.push(rx);

        // A stopped run is not a failure and must not borrow `✓`.
        let (mut stopped, rx) = test_app("sweep-stopped");
        begin_run(&mut stopped, AgentId::ROOT);
        stopped.on_agent(AgentId::ROOT, AgentEvent::Stopped);
        states.push(Sweep {
            name: "a stopped run",
            app: stopped,
            words: vec!["⊘ #0", "stopped"],
            roomy: vec!["re-send to resume"],
            reopen: None,
        });
        keep.push(rx);

        // A session restored from a hand-written file: a failed child and the
        // failure mush wrote about the root's last run.
        states.push(Sweep {
            name: "a restored session",
            app: restored_session("sweep-restored"),
            words: vec!["· #0", " agents ", " chat ", "mush › "],
            roomy: vec!["✗ #1", "mush/1", "! the endpoint returned 503"],
            reopen: None,
        });

        // A pane was scrolled away from the bottom: the title says so.
        let (mut scrolled, rx) = test_app("sweep-scrolled");
        for i in 1..=30 {
            scrolled
                .chat
                .push_message(AgentId::ROOT, Message::user(format!("message {i}")));
            scrolled
                .chat
                .push_message(AgentId::ROOT, Message::assistant(format!("reply {i}")));
        }
        scrolled.chat.scroll_by(AgentId::ROOT, 5);
        states.push(Sweep {
            name: "a transcript scrolled back",
            app: scrolled,
            words: vec!["scrolled ↑5 rows", "PgDn"],
            roomy: vec![],
            reopen: None,
        });
        keep.push(rx);

        // More notes than the foot has rows: the pane says how many it is
        // hiding, and where to read them.
        let (mut noted, rx) = test_app("sweep-notes-foot");
        for i in 1..=7 {
            noted
                .chat
                .note_for(AgentId::ROOT, format!("mush wrote this: note {i}"));
        }
        states.push(Sweep {
            name: "a foot with hidden lines",
            app: noted,
            words: vec!["more lines", "/notes"],
            roomy: vec!["note 7", "note 6", "+5 more lines"],
            reopen: None,
        });
        keep.push(rx);

        // A job on this machine: the row's badge, the footer's line and the
        // bar's report, all from the one registry.
        let (mut jobbed, rx) = test_app("sweep-job");
        running_job(&mut jobbed, 0);
        jobbed.on_agent(
            AgentId::ROOT,
            AgentEvent::JobStarted {
                job: 1,
                command: "cargo build --release".to_string(),
            },
        );
        states.push(Sweep {
            name: "a job",
            app: jobbed,
            words: vec!["⚙1", "#c1"],
            roomy: vec![" 1 jobs · #c1"],
            reopen: None,
        });
        keep.push(rx);

        // Twenty agents, three levels deep, with a dirty repository.
        let (twenty, twenty_rx) = a_twenty_agent_tree("sweep-twenty");
        states.push(Sweep {
            name: "twenty agents",
            app: twenty,
            words: vec![" agents · ", " chat "],
            roomy: vec![
                "8 working",
                "1 waiting",
                "grandchild",
                "mush/3",
                "main ±3 +9−2",
            ],
            reopen: None,
        });
        keep.push(twenty_rx);

        // The `/model` picker.
        let (mut models, rx) = test_app("sweep-model-picker");
        models.models = vec![
            http::Model {
                id: "test-model".to_string(),
                context: Some(500_000),
            },
            http::Model {
                id: "deepseek-chat".to_string(),
                context: Some(128_000),
            },
            // An endpoint names its own models, and the picker paints the name
            // whole: this one is a command, and the sweep's cell check is what
            // reads it.
            http::Model {
                id: format!("{HOSTILE}model"),
                context: None,
            },
        ];
        models.open_model_picker();
        states.push(Sweep {
            name: "an open /model picker",
            app: models,
            words: vec![
                " models · Enter picks ",
                "• test-model · 500k",
                "j/k or PgUp/PgDn",
            ],
            roomy: vec!["deepseek-chat · 128k"],
            reopen: None,
        });
        keep.push(rx);

        // The `/notes` popup, whose wrapping is derived from the terminal's
        // width when it opens — so it is opened again for every size.
        let (mut notes, rx) = test_app("sweep-notes-picker");
        notes.chat.note_for(
            AgentId::ROOT,
            "the endpoint returned 503 while the run was folding the transcript",
        );
        notes.open_notes_picker();
        states.push(Sweep {
            name: "an open /notes popup",
            app: notes,
            words: vec![
                "notes · newest last",
                "the endpoint returned 503",
                "j/k or PgUp/PgDn",
            ],
            roomy: vec![],
            reopen: None,
        });
        keep.push(rx);

        // Untrusted words on every one-line surface at once: a model reply, a
        // run's failure, a brief, a job command and a model id, each carrying
        // the commands the verifier used.
        let (mut hostile, rx) = test_app("sweep-hostile");
        spawn_agent(&mut hostile, 1, 0, 1, HOSTILE, Some("mush/1"));
        // The cursor is on the child, whose brief is the hostile text: the
        // selected row's *footer* is the one-line surface that carries a brief
        // through `truncate`, and the root's own failure is on the bar.
        hostile.tree.move_cursor(1);
        hostile
            .chat
            .push_message(AgentId::ROOT, Message::assistant(HOSTILE));
        hostile.on_agent(AgentId::ROOT, AgentEvent::Error(HOSTILE.to_string()));
        hostile.chat.note_for(AgentId::ROOT, HOSTILE);
        states.push(Sweep {
            name: "hostile words",
            app: hostile,
            words: vec!["escaped", "␍", " chat "],
            roomy: vec!["escaped", "␍"],
            reopen: None,
        });
        keep.push(rx);

        (states, keep)
    }

    /// A workspace whose `.mush/session.json` was written by hand: two root
    /// messages, a child whose last run failed, and the failure notice mush
    /// stored about the root. The app that adopts it is what a restart paints.
    fn restored_session(label: &str) -> App {
        let root = dir(label);
        let session = root.join(".mush/session.json");
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        // Written as text, not built from `Session`: a hand-edited file is the
        // input this path has to survive, and a round trip through the writer
        // would test the writer instead.
        std::fs::write(
            &session,
            r#"{
  "root": "/tmp/sweep-restored",
  "model": "test-model",
  "updated": 1,
  "messages": [
    {"role": "user", "content": "port the parser module"},
    {"role": "assistant", "content": "done, mostly"}
  ],
  "agents": [
    {
      "id": 1,
      "parent": 0,
      "depth": 1,
      "brief": "port the parser module",
      "branch": "mush/1",
      "status": {"failed": "no route to host"},
      "messages": []
    }
  ],
  "notices": [
    {
      "agent": 0,
      "at": 1,
      "text": "the endpoint returned 503 while the run was folding the transcript"
    }
  ]
}
"#,
        )
        .unwrap();
        reopened(&root)
    }

    /// The draw sweep: every state, at every size, read as *text*.
    ///
    /// What it replaced asserted "does not panic" — which is what a layout
    /// arithmetic bug looks like from the outside and nothing more. This one
    /// asserts what a frame *says*: the words each state is about, that no pane
    /// painted over its own border or outside its rect, that the bar keeps its
    /// row, that every line a pane was handed fits the pane, that no cell is a
    /// control byte, and that the frame is exactly the terminal's size. The
    /// words are the evidence; a layout that silently drops a fact is the bug
    /// class the audit's ten defects all belonged to (refactor B17).
    #[test]
    fn the_draw_sweep_asserts_painted_text_not_that_it_did_not_panic() {
        let (mut states, _keep) = sweep_states();
        for state in &mut states {
            let Sweep {
                name,
                app,
                words,
                roomy,
                reopen,
            } = state;
            for &(width, height) in SWEEP_SIZES {
                // A popup whose contents are wrapped to the terminal's width is
                // opened again for each size, the way a resize would.
                if let Some(reopen) = reopen {
                    app.set_term_size(width, height);
                    reopen(app);
                }
                let shot = shot(app, width, height);
                let at = format!("{name} at {width}×{height}");
                // The frame is the terminal: every row, every column, and not
                // one cell that commands the display instead of being read.
                assert_eq!(shot.cells.len(), height as usize, "{at}: the rows");
                assert!(
                    shot.cells.iter().all(|row| row.len() == width as usize),
                    "{at}: a row is not {width} columns wide"
                );
                assert_no_command(&at, &shot);

                if is_below_floor(width, height) {
                    // Below the floor: one notice, centred on both axes, and
                    // *nothing else* painted — the panes are not built at all,
                    // so nothing can leak through the floor (R3, finding P11).
                    assert!(
                        matches!(shot.screen, Screen::Floor { .. }),
                        "{at}: the floor notice"
                    );
                    let painted: Vec<usize> = (0..height as usize)
                        .filter(|&y| !shot.line(y as u16).trim().is_empty())
                        .collect();
                    assert_eq!(
                        painted,
                        vec![(height as usize).saturating_sub(1) / 2],
                        "{at}: one row, at the middle: {painted:?}"
                    );
                    if width >= 5 {
                        let floor = format!("{MIN_WIDTH}×{MIN_HEIGHT}");
                        assert!(
                            shot.line(painted[0] as u16).contains(&floor),
                            "{at}: the notice must name the floor"
                        );
                    }
                    continue;
                }

                let text = shot.text();
                for word in words.iter() {
                    assert!(text.contains(word), "{at}: must paint `{word}`:\n{text}");
                }
                // The audit's two presentation sizes: the widest terminal mush
                // is photographed on and the one a working session sits at.
                // Every pane — the two-column tree, a foot with a count row, the
                // bar's facts line — has the room it was built for there, which
                // is what makes a word missing here a word that exists nowhere.
                if (width, height) == (200, 50) || (width, height) == (120, 32) {
                    for word in roomy.iter() {
                        assert!(text.contains(word), "{at}: must paint `{word}`:\n{text}");
                    }
                }
                shot.assert_shape(name, width, height);
            }
        }
    }

    /// A wide glyph is two columns of the pane, not one, and every pane paints
    /// it inside its own border: the row's fields, the transcript's wrap, the
    /// message box and the footer are all column arithmetic, and a width taken
    /// for granted is how a CJK row ends up a column over its border (finding
    /// B9).
    #[test]
    fn the_sweep_fits_wide_glyphs_in_their_panes() {
        let (mut app, _rx) = test_app("sweep-wide-glyphs");
        spawn_agent(&mut app, 1, 0, 1, "移植解析器与词法分析", Some("mush/1"));
        app.chat.push_message(
            AgentId::ROOT,
            Message::assistant("请把解析器移植过来，然后运行测试"),
        );
        app.on_agent(AgentId(1), AgentEvent::Error("端点的回应无法解析".into()));
        for &(width, height) in SWEEP_SIZES {
            if is_below_floor(width, height) {
                continue;
            }
            let shot = shot(&mut app, width, height);
            shot.assert_shape("wide glyphs", width, height);
            assert_no_command(&format!("wide glyphs at {width}×{height}"), &shot);
        }
        let text = shot(&mut app, 200, 50).shown().join("\n");
        assert!(
            text.contains("移植解析器"),
            "the row names the agent in its own script: {text}"
        );
        assert!(
            text.contains("请把解析器移植过来"),
            "and the transcript keeps the message: {text}"
        );
    }

    /// The hidden-row counts and the arrows that name the side, read off the
    /// painted title at the size where the pane is shortest.
    ///
    /// Nineteen rows hidden under a one-row window is the defect P12 named, and
    /// a bare `+19` cannot say which way they went — at the bottom of the pane
    /// every hidden row is *above* the window, which is the other half of the
    /// same defect.
    #[test]
    fn the_sweep_counts_the_rows_the_pane_cannot_show() {
        let (mut app, _rx) = a_twenty_agent_tree("sweep-hidden");
        // The compact strip gives the tree one inner row at 40×10: the root is
        // the window, and the other nineteen rows are below it.
        let text = shot(&mut app, 40, 10).text();
        assert!(text.contains("▼19"), "the rows below are named: {text}");
        assert!(!text.contains('▲'), "nothing is above the top row: {text}");

        app.tree.cursor_bottom();
        let text = shot(&mut app, 40, 10).text();
        assert!(
            text.contains("▲19"),
            "at the bottom they are all above: {text}"
        );
        assert!(!text.contains('▼'), "nothing is below: {text}");

        // At the presentation size nothing is hidden, so nothing says it is.
        app.tree.cursor_top();
        let text = shot(&mut app, 200, 50).text();
        assert!(!text.contains('▲') && !text.contains('▼'), "{text}");
    }

    /// The pane's title wears the counts that fit it and drops the rest whole:
    /// a clause cut mid-number (`Σ +324 −`, `2 waitin`) is a count that is not
    /// the count (finding U2).
    #[test]
    fn the_sweep_paints_the_title_clauses_that_fit() {
        let (mut app, _rx) = test_app("sweep-title");
        spawn_agent(
            &mut app,
            1,
            0,
            1,
            "build the lexer for the config format",
            None,
        );
        // At 40×10 the compact strip gives the tree the whole width, and both
        // counts fit.
        let text = shot(&mut app, 40, 10).text();
        assert!(text.contains("1 working"), "{text}");
        assert!(text.contains("1 waiting"), "{text}");
        // At 80×24 the tree gets thirty columns, and the widest clause yields.
        let text = shot(&mut app, 80, 24).text();
        assert!(text.contains(" agents · 1 working"), "{text}");
        assert!(
            !text.contains("1 waiting"),
            "a clause that does not fit is dropped whole, never cut: {text}"
        );
    }

    /// A derived line is not a hidden line: a busy agent with nothing written
    /// about it must not claim `+1 more lines` for its own spinner — a count
    /// `/notes` cannot answer.
    #[test]
    fn the_sweep_never_counts_a_derived_line_as_hidden() {
        let (mut app, _rx) = test_app("sweep-spinner-count");
        app.chat
            .push_message(AgentId::ROOT, Message::user("what is happening"));
        begin_run(&mut app, AgentId::ROOT);
        // At 40×10 the pane has one row of transcript, it protects it for the
        // message, and the foot therefore has no row at all: the spinner is not
        // painted, and a pane that counted it would say so in its title.
        let text = shot(&mut app, 40, 10).text();
        assert!(!text.contains("working…"), "no row for it here: {text}");
        assert!(
            !text.contains("more lines"),
            "a derived line is not a hidden line: {text}"
        );
        // Where the foot has a row, the spinner is painted and still nothing is
        // counted as hidden.
        for &(width, height) in SWEEP_SIZES {
            if is_below_floor(width, height) || (width, height) == (40, 10) {
                continue;
            }
            let text = shot(&mut app, width, height).text();
            assert!(
                text.contains("working…"),
                "the spinner is the pane's activity at {width}×{height}: {text}"
            );
            assert!(
                !text.contains("more lines"),
                "the spinner is not a hidden line at {width}×{height}: {text}"
            );
        }
    }

    /// The stored failure survives the restart into the *painted* frame: a row
    /// where the pane has one, and the title — which is the pane's way of
    /// saying what it is hiding — where it does not.
    #[test]
    fn the_sweep_paints_a_restored_failure_at_every_size() {
        let mut app = restored_session("sweep-restored-notice");
        let text = shot(&mut app, 80, 24).text();
        assert!(
            text.contains("! the endpoint returned 503"),
            "the stored failure is a row of the foot: {text}"
        );
        let text = shot(&mut app, 40, 10).text();
        assert!(
            text.contains("more lines") && text.contains("/notes"),
            "a pane with no row for the line says how many it is hiding: {text}"
        );
    }

    /// A wrapped message keeps its tail: the pane is anchored at the bottom, so
    /// the last row of the newest message is the row that must be there at
    /// every size, and the head is there when the pane has the rows for it.
    #[test]
    fn the_sweep_keeps_the_tail_of_a_wrapped_message() {
        let (mut app, _rx) = test_app("sweep-wrapped");
        let long = format!("HEADWORD {} TAILWORD", "wide ".repeat(40));
        app.chat
            .push_message(AgentId::ROOT, Message::assistant(&long));
        for &(width, height) in SWEEP_SIZES {
            if is_below_floor(width, height) {
                continue;
            }
            let shot = shot(&mut app, width, height);
            assert!(
                shot.text().contains("TAILWORD"),
                "the tail of the newest message at {width}×{height}:\n{}",
                shot.text()
            );
            shot.assert_shape("a wrapped message", width, height);
        }
        let text = shot(&mut app, 200, 50).text();
        assert!(text.contains("HEADWORD"), "the head, where it fits: {text}");
    }

    /// Scrolling holds the window: the pane says so, and what it shows is the
    /// rows above the bottom rather than the ones following it.
    #[test]
    fn the_sweep_shows_the_rows_a_scrolled_pane_is_holding() {
        let (mut app, _rx) = test_app("sweep-held");
        for i in 1..=40 {
            app.chat
                .push_message(AgentId::ROOT, Message::user(format!("message {i}")));
        }
        let bottom = shot(&mut app, 120, 32).text();
        app.chat.scroll_by(AgentId::ROOT, 20);
        let held = shot(&mut app, 120, 32).text();
        assert!(held.contains("scrolled ↑20 rows"), "{held}");
        let older = (1..=40)
            .map(|i| format!("message {i}"))
            .find(|word| held.contains(word.as_str()) && !bottom.contains(word.as_str()))
            .unwrap_or_else(|| {
                panic!("the held window shows rows the bottom did not:\n{held}\n---\n{bottom}")
            });
        assert!(
            !bottom.contains(&older),
            "{older} is only in the held frame"
        );
    }

    /// A popup's own text is read at every size too: the wrapped note, the
    /// bullet on the current model and the hint are all painted, and the popup
    /// leaves the panes it covers alone.
    #[test]
    fn the_sweep_paints_what_a_popup_says() {
        let (mut app, _rx) = test_app("sweep-popup");
        app.models = vec![
            http::Model {
                id: "test-model".to_string(),
                context: Some(500_000),
            },
            http::Model {
                id: "deepseek-chat".to_string(),
                context: None,
            },
        ];
        app.open_model_picker();
        for &(width, height) in SWEEP_SIZES {
            if is_below_floor(width, height) {
                continue;
            }
            let shot = shot(&mut app, width, height);
            let text = shot.text();
            assert!(
                text.contains(" models · Enter picks "),
                "the popup's title at {width}×{height}: {text}"
            );
            assert!(
                text.contains("• test-model"),
                "the current model is marked at {width}×{height}: {text}"
            );
            assert!(
                text.contains("j/k or PgUp/PgDn"),
                "and the keys are named at {width}×{height}: {text}"
            );
            shot.assert_shape("an open /model picker", width, height);
        }

        // A note can carry a failure's own words, and a model id comes from the
        // endpoint: both are painted whole in the popup, and both must be
        // defanged where they are painted.
        let (mut notes, _rx) = test_app("sweep-popup-hostile");
        notes.chat.note_error_for(AgentId::ROOT, HOSTILE);
        notes.open_notes_picker();
        for &(width, height) in SWEEP_SIZES {
            if is_below_floor(width, height) {
                continue;
            }
            notes.set_term_size(width, height);
            notes.open_notes_picker();
            let shot = shot(&mut notes, width, height);
            let at = format!("a hostile /notes popup at {width}×{height}");
            assert_no_command(&at, &shot);
            shot.assert_shape(&at, width, height);
            assert!(
                shot.shown().join("\n").contains("escaped"),
                "{at}: the words survive the commands around them"
            );
        }
    }

    /// Deliver one hostile payload the way its actor does: a model reply (the
    /// run then ends, so the reply becomes the row's and the footer's summary),
    /// a run's failure (the endpoint's own words), and a tool result.
    fn feed_hostile(app: &mut App, case: &str, text: &str) {
        let conversation = app.tree.conversation();
        let event = match case {
            "reply" => {
                app.update(Msg::Agent {
                    conversation,
                    id: AgentId::ROOT,
                    event: AgentEvent::Message(Message::assistant(text)),
                });
                AgentEvent::Done
            }
            "error" => AgentEvent::Error(text.to_string()),
            "result" => AgentEvent::Message(Message::tool("call_1", text)),
            other => unreachable!("no case {other}"),
        };
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event,
        });
    }

    /// A pane is a terminal, and a terminal acts on what it is given — so the
    /// one-line surfaces are fed the same bytes the verifier used, and what is
    /// asserted is the *painted frame*, not the source string.
    ///
    /// The wrapped transcript was already defanged when this leaked: a reply of
    /// `…\rREPLACED…` returned the cursor over the agents pane's own border from
    /// the *footer*, an error body's `ESC ]0;PWNED BEL` renamed the window from
    /// the *bar*, and a CSI in either wiped the frame. A test that reads the
    /// transcript is exactly what let the other four surfaces through, so this
    /// one reads the cells, at the audit's 80×24 and at the compact 40×10.
    #[test]
    fn a_one_line_surface_cannot_be_commanded_by_the_text_it_paints() {
        const CR: &str = "\r";
        const OSC: &str = "\x1b]0;PWNED\x07";
        const CSI: &str = "\x1b[2J\x1b[H";
        const BIDI: &str = "\u{2066}";

        // Each case, with the commands in it and with plain words in the same
        // places: the second is what the frame is measured against, so a pane
        // that a leak moved is a pane whose border is somewhere else.
        let payload = |case: &str, hostile: bool| -> String {
            match (case, hostile) {
                ("reply", true) => {
                    format!("final reply{CR}STEP1 {OSC}{CSI}{BIDI}middle")
                }
                ("reply", false) => "final reply STEP1 middle".to_string(),
                ("error", true) => {
                    format!("model returned HTTP 500: boom{CR}REST {OSC} {CSI}{BIDI}after")
                }
                ("error", false) => "model returned HTTP 500: boom REST  after".to_string(),
                ("result", true) => format!("boom{CR}REST {OSC} {CSI}{BIDI}after"),
                ("result", false) => "boom REST  after".to_string(),
                other => unreachable!("no case {other:?}"),
            }
        };
        // What the surface still says once the commands are gone: the CR is
        // marked, never taken away, and the words around it are the words.
        let says = |case: &str| match case {
            "reply" => "final reply␍STEP1",
            _ => "boom␍REST",
        };

        for (width, height) in [(80u16, 24u16), (40, 10)] {
            for case in ["reply", "error", "result"] {
                let (mut app, _rx) = test_app("hostile-one-line");
                feed_hostile(&mut app, case, &payload(case, true));
                let frame = frame_grid(&mut app, width, height);

                let (mut clean, _rx) = test_app("hostile-one-line-plain");
                feed_hostile(&mut clean, case, &payload(case, false));
                let pristine = frame_grid(&mut clean, width, height);

                let painted = frame.join("\n");
                for bad in ['\x1b', '\r'] {
                    assert!(
                        !painted.contains(bad),
                        "{case} at {width}×{height} painted {bad:?}: {painted:?}"
                    );
                }
                // Cell by cell, and never the newline this test joined them with:
                // every other control byte, and the bidi isolates, are a command
                // to a display rather than something to read.
                for row in &frame {
                    for ch in row.chars() {
                        assert!(
                            !ch.is_control()
                                && !matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'),
                            "{case} at {width}×{height} painted {ch:?}, which commands the display: {row:?}"
                        );
                    }
                }
                assert!(
                    painted.contains(says(case)),
                    "{case} at {width}×{height} lost its words: {painted:?}"
                );

                // The box skeleton belongs to the frame, not to the text: a
                // border that moved or vanished is a pane that was overwritten.
                for (y, (row, base)) in frame.iter().zip(pristine.iter()).enumerate() {
                    for (x, (cell, want)) in row.chars().zip(base.chars()).enumerate() {
                        if "─│┌┐└┘├┤┬┴┼".contains(want) {
                            assert_eq!(
                                cell, want,
                                "{case} at {width}×{height} overwrote the border at {x},{y}: {row:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A long unbroken token — the model reply that is one 20 000-character
    /// word — must be cut at a one-line surface's edge, with the `…` the rest of
    /// the UI uses, and never have its middle painted into a pane.
    #[test]
    fn a_one_line_surface_cuts_a_long_token_at_its_edge() {
        let token = "S".repeat(20_000);
        let reply = format!("final model reply\rSTEP1 middle {token}");

        for (width, height) in [(80u16, 24u16), (40, 10)] {
            let (mut app, _rx) = test_app("long-token");
            feed_hostile(&mut app, "reply", &reply);
            let frame = frame_grid(&mut app, width, height);
            let painted = frame.join("\n");

            assert!(!painted.contains('\r'), "{painted:?}");
            // The agents pane paints none of the token: at 80×24 it is the
            // thirty columns on the left, at 40×10 the three rows on top. (The
            // transcript is *meant* to wrap the token — that is the one surface
            // that may show its text.)
            let pane: Vec<&String> = if width >= 80 {
                frame.iter().collect()
            } else {
                frame.iter().take(3).collect()
            };
            for row in pane {
                let cells: String = row
                    .chars()
                    .take(if width >= 80 { 30 } else { 40 })
                    .collect();
                assert!(
                    !cells.contains("SSSS"),
                    "the agents pane painted the token at {width}×{height}: {row:?}"
                );
            }
            if width >= 80 {
                assert!(
                    painted.contains('…'),
                    "the footer must say it cut the token: {painted:?}"
                );
            }
        }
    }

    /// One frame with a nested tree: the root is at rest with work out, its own
    /// child #1 is parked in `wait_agents`, and the grandchild #2 is working.
    ///
    /// The bar's sentence is a promise about when the root resumes — "the root
    /// resumes as they finish" — and the root resumes when *its* children
    /// finish, which is the derivation the row's `⏸N` and the title's `M
    /// waiting` read. The bar counted every busy node in the tree instead, so
    /// with a grandchild at work it promised a resume that the grandchild's
    /// finish does not cause, in the same frame where the pane title named one
    /// (finding U2's second owner).
    #[test]
    fn the_bar_and_the_title_agree_on_who_the_root_waits_for() {
        let (mut app, _rx) = test_app("nested-wait");
        let conversation = app.tree.conversation();
        for (child, parent, depth) in [(1u64, 0u64, 1usize), (2, 1, 2)] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId(parent),
                event: AgentEvent::Spawned {
                    child,
                    parent,
                    brief: "a task".to_string(),
                    depth,
                    branch: None,
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
            app.update(Msg::Agent {
                conversation,
                id: AgentId(child),
                event: AgentEvent::Running {
                    cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                },
            });
        }
        // The root ended its turn with child #1 still out; #1 is parked on its
        // own child, and #2 is the one actually working.
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Done,
        });
        app.tree.activity(AgentId(1), "wait_agents 3s");
        app.tree.activity(AgentId(2), "read_file deep.txt 2s");
        assert_eq!(
            (
                app.tree.busy_children(AgentId::ROOT),
                app.tree.roster().working
            ),
            (1, 2),
            "the tree this is about: the root waits on one of two busy agents"
        );

        let rows = screen(&mut app, 200, 50);
        let title = rows.first().expect("the pane title is painted");
        // The bar's message row, above the facts row it shares the foot with.
        let bar = &rows[rows.len() - 2];

        assert!(title.contains("2 working"), "{title:?}");
        assert!(title.contains("1 waiting"), "{title:?}");
        assert!(
            bar.contains("waiting on 1 subagent(s)"),
            "the bar counted a grandchild the root does not resume on: {bar:?}"
        );
        assert!(
            !bar.contains("waiting on 2"),
            "the bar disagrees with the title in the same frame: {bar:?}"
        );
    }

    /// A status that arrives after the run ended must not put a finished agent
    /// back to work (finding B5).
    #[test]
    fn a_late_status_does_not_restart_a_finished_agent() {
        let (mut app, _rx) = test_app("late-status");
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Done,
        });
        assert_eq!(app.tree.agents[0].phase, Phase::Done);
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Status("committed abc123 on mush/1".to_string()),
        });
        assert_eq!(app.tree.agents[0].phase, Phase::Done, "the ✓ must survive");
        assert!(!app.busy());
    }

    /// A `⊘` whose acknowledgement can never arrive goes quiet instead of
    /// spinning forever (finding B6).
    #[test]
    fn a_stale_cancel_falls_back_to_idle() {
        let (mut app, _rx) = test_app("stale-cancel");
        app.tree.cancel_requested(AgentId::ROOT);
        app.tree.age(AgentId::ROOT, Duration::from_secs(11));
        app.tick();
        assert_eq!(app.tree.agents[0].phase, Phase::Idle);
        assert!(!app.busy(), "the bar must stop claiming work");
    }

    /// The child's brief is the first thing its transcript shows, exactly as
    /// the model received it (finding B13).
    #[test]
    fn a_childs_brief_is_the_start_of_its_transcript() {
        let (mut app, _rx) = test_app("brief");
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Spawned {
                child: 1,
                parent: 0,
                brief: "count the lexer tokens".to_string(),
                depth: 1,
                branch: None,
                cmd: crossbeam_channel::unbounded().0,
            },
        });
        let messages = app.chat.transcript(AgentId(1));
        assert_eq!(messages.len(), 1, "the brief opens the transcript");
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].text(), "count the lexer tokens");
    }

    /// Every Done replaces the row's summary, and a new run clears it: the row
    /// describes the current run, not the first one forever (finding B14).
    #[test]
    fn a_rows_summary_follows_the_latest_run() {
        let (mut app, _rx) = test_app("summary");
        let conversation = app.tree.conversation();
        app.chat
            .push_message(AgentId::ROOT, Message::assistant("first result"));
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Done,
        });
        assert_eq!(app.tree.agents[0].summary.as_deref(), Some("first result"));
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        });
        assert_eq!(
            app.tree.agents[0].summary, None,
            "a new run clears the old one"
        );
        app.chat
            .push_message(AgentId::ROOT, Message::assistant("second result"));
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Done,
        });
        assert_eq!(app.tree.agents[0].summary.as_deref(), Some("second result"));
    }

    /// Leftover worktrees keep their ids, and the tree's counter is raised
    /// above them, so the next spawned child cannot collide (finding B1); a
    /// reaped leftover releases the focus (finding B11).
    #[test]
    fn leftover_worktrees_raise_the_id_floor_and_release_the_focus() {
        use std::fs;
        use std::process::Command;

        let root = std::env::temp_dir().join(format!("mush-app-wt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        git(&["worktree", "add", "-q", "-b", "mush/7", ".mush/wt/7"]);

        let (mut app, _rx) = app_root(&root, None, session_save::fake::Recorder::new());

        assert!(
            app.tree.agents.iter().any(|node| node.id == AgentId(7)),
            "the leftover is registered"
        );
        assert!(
            app.tree.handles().ids.load(Ordering::SeqCst) >= 8,
            "the next spawn must not reuse #7"
        );

        app.tree.focus(AgentId(7));
        git(&["worktree", "remove", "--force", ".mush/wt/7"]);
        app.discover_worktrees();
        assert!(
            !app.tree.agents.iter().any(|node| node.id == AgentId(7)),
            "the reaped leftover is gone"
        );
        assert_eq!(
            app.tree.focused,
            AgentId::ROOT,
            "focus cannot point at a ghost"
        );
        let _ = fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------- attach (M3)
    //
    // The socket's own half — framing, a bad line, a kept connection — lives in
    // `crate::attach`'s tests. These are the `App` half: the ops, over the tree
    // and the chat, on the same `handle_attach` the message loop calls.

    fn attach_request(id: u64, op: attach::Op) -> attach::Request {
        attach::Request {
            id: serde_json::json!(id),
            op,
        }
    }

    fn attach_ok(response: attach::Response) -> serde_json::Value {
        match response.reply {
            attach::Reply::Ok(body) => body,
            attach::Reply::Err(error) => panic!("expected ok, got {error:?}"),
        }
    }

    fn attach_err(response: attach::Response) -> attach::ReplyError {
        match response.reply {
            attach::Reply::Err(error) => error,
            attach::Reply::Ok(body) => panic!("expected an error, got {body}"),
        }
    }

    /// `read` hands back the transcript lines with the revision a client hands
    /// to `edit`, and `since` skips the lines it has already seen.
    #[test]
    fn attach_read_returns_the_lines_and_a_revision() {
        let (mut app, _rx) = test_app("attach-read");
        app.chat.push_message(AgentId::ROOT, Message::user("first"));
        app.chat
            .push_message(AgentId::ROOT, Message::assistant("second"));

        let body = attach_ok(app.handle_attach(
            "a client",
            &attach_request(1, attach::Op::Read { agent: 0, since: 0 }),
        ));
        assert_eq!(body["agent"], serde_json::json!(0));
        assert_eq!(
            body["revision"].as_u64(),
            Some(app.chat.revision(AgentId::ROOT))
        );
        let lines = body["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["line"], serde_json::json!(0));
        assert_eq!(lines[0]["role"], "user");
        assert_eq!(lines[0]["text"], "first");
        assert_eq!(lines[1]["text"], "second");

        // `since` is the first line to read, so the second read is the tail.
        let body = attach_ok(app.handle_attach(
            "a client",
            &attach_request(2, attach::Op::Read { agent: 0, since: 1 }),
        ));
        let lines = body["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["line"], serde_json::json!(1));
        assert_eq!(lines[0]["text"], "second");
    }

    /// An id the tree does not have is a bad request, not an empty transcript.
    #[test]
    fn attach_read_of_an_unknown_agent_is_a_bad_request() {
        let (mut app, _rx) = test_app("attach-read-missing");
        let error = attach_err(app.handle_attach(
            "a client",
            &attach_request(1, attach::Op::Read { agent: 9, since: 0 }),
        ));
        assert_eq!(error.kind, "bad_request");
        assert!(error.message.unwrap().contains('9'));
    }

    /// The roster is read from the tree: parents, phases, the focused flag, and
    /// the working-children count the row paints as `⏸N` (M3 / H1).
    #[test]
    fn attach_agents_reflects_the_tree() {
        let (mut app, _rx) = test_app("attach-agents");
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: Some("mush/1".to_string()),
            cmd: crossbeam_channel::unbounded().0,
        });

        let body = attach_ok(app.handle_attach("a client", &attach_request(2, attach::Op::Agents)));
        let agents = body["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 2, "the root and its child");

        assert_eq!(agents[0]["id"], serde_json::json!(0));
        assert_eq!(agents[0]["parent"], serde_json::Value::Null);
        assert_eq!(agents[0]["phase"], "idle");
        assert_eq!(agents[0]["focused"], serde_json::json!(true));
        assert_eq!(
            agents[0]["children_working"],
            serde_json::json!(1),
            "the root has one child thinking"
        );

        assert_eq!(agents[1]["id"], serde_json::json!(1));
        assert_eq!(agents[1]["parent"], serde_json::json!(0));
        assert_eq!(agents[1]["phase"], "thinking");
        assert_eq!(agents[1]["activity"], serde_json::Value::Null);
        assert_eq!(agents[1]["branch"], "mush/1");
        assert_eq!(agents[1]["focused"], serde_json::json!(false));
        assert_eq!(
            agents[1]["revision"].as_u64(),
            Some(app.chat.revision(AgentId(1)))
        );
    }

    /// `focus` moves the pane, the keyboard and the tree cursor exactly as
    /// `Enter` on the row does — the same `focus_cursor_row` path.
    #[test]
    fn attach_focus_moves_the_focus_like_enter() {
        let (mut app, _rx) = test_app("attach-focus");
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            cmd: crossbeam_channel::unbounded().0,
        });
        app.focus = Focus::Agents;
        app.tree.cursor_top();

        let body = attach_ok(app.handle_attach(
            "a client",
            &attach_request(3, attach::Op::Focus { agent: 1 }),
        ));
        assert_eq!(body, serde_json::json!({}));
        assert_eq!(app.tree.focused, AgentId(1), "the pane shows #1");
        assert_eq!(app.focus, Focus::Chat, "and the keyboard went with it");
        assert_eq!(
            app.tree.cursor_id(),
            Some(AgentId(1)),
            "the row is selected"
        );

        let error = attach_err(app.handle_attach(
            "a client",
            &attach_request(4, attach::Op::Focus { agent: 9 }),
        ));
        assert_eq!(error.kind, "bad_request");
    }

    /// A stale base is a `conflict` that names the revision that moved, and it
    /// changes nothing — not the box, not the transcript.
    #[test]
    fn attach_edit_with_a_stale_base_conflicts_and_changes_nothing() {
        let (mut app, _rx) = test_app("attach-conflict");
        let stale = app.chat.revision(AgentId::ROOT);
        // Something moved the transcript after the client last read.
        app.chat
            .push_message(AgentId::ROOT, Message::assistant("a reply"));

        let response = app.handle_attach(
            "a client",
            &attach_request(
                5,
                attach::Op::Edit {
                    agent: 0,
                    base: stale,
                    text: "a draft".to_string(),
                    send: false,
                },
            ),
        );
        let error = attach_err(response);
        assert_eq!(error.kind, "conflict");
        assert_eq!(
            error.revision,
            Some(app.chat.revision(AgentId::ROOT)),
            "the client is told where the transcript went"
        );
        assert_eq!(app.chat.input().text(), "", "the draft was not written");
        assert_eq!(
            app.chat.transcript(AgentId::ROOT).len(),
            1,
            "and nothing was appended"
        );
    }

    /// A fresh base lands a draft in the box and moves the revision, so the
    /// same base cannot be spent twice.
    #[test]
    fn attach_edit_with_a_fresh_base_lands_a_draft() {
        let (mut app, _rx) = test_app("attach-draft");
        let base = app.chat.revision(AgentId::ROOT);
        let body = attach_ok(app.handle_attach(
            "a client",
            &attach_request(
                6,
                attach::Op::Edit {
                    agent: 0,
                    base,
                    text: "a draft from the agent".to_string(),
                    send: false,
                },
            ),
        ));
        assert_eq!(body["revision"].as_u64(), Some(base + 1));
        assert_eq!(app.chat.input().text(), "a draft from the agent");

        // The revision the first edit returned is the base a second one needs;
        // the old base is refused.
        let error = attach_err(app.handle_attach(
            "a client",
            &attach_request(
                7,
                attach::Op::Edit {
                    agent: 0,
                    base,
                    text: "again".to_string(),
                    send: false,
                },
            ),
        ));
        assert_eq!(error.kind, "conflict");
    }

    /// `send: true` speaks to the agent the way a typed message does: the line
    /// lands in its transcript as the human's and its mailbox gets a nudge —
    /// without moving the human's own focus off the agent they were watching.
    #[test]
    fn attach_edit_that_sends_reaches_the_mailbox() {
        let (mut app, _rx) = test_app("attach-send");
        let (cmd, mailbox) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            cmd,
        });
        let base = app.chat.revision(AgentId(1));

        let body = attach_ok(app.handle_attach(
            "a client",
            &attach_request(
                8,
                attach::Op::Edit {
                    agent: 1,
                    base,
                    text: "also rename the module".to_string(),
                    send: true,
                },
            ),
        ));
        assert_eq!(body["revision"].as_u64(), Some(base + 1));
        assert_eq!(
            app.chat.transcript(AgentId(1)).last().map(Message::text),
            Some("also rename the module"),
            "the words are in the agent's transcript"
        );
        assert_eq!(
            app.tree.focused,
            AgentId::ROOT,
            "the human's pane did not move"
        );
        assert!(
            matches!(
                mailbox.try_recv(),
                Ok(AgentMsg::Nudge(text)) if text == "also rename the module"
            ),
            "the message reaches the agent's mailbox the way a typed one does"
        );
    }
}
