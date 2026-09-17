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
mod keys;
mod settings;
mod tree;

pub use chat::{Chat, Pane, Rank};
pub use settings::{ConfigCell, ConfigHandle, WindowSource};
pub use tree::{AgentId, AgentNode, AgentTree, ConversationId, Existing, Landed, Phase, Spawn};

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
            PickerKind::Notes => " notes · newest last ".to_string(),
        }
    }

    /// What the popup's own last row says the keys do. It belongs to the picker
    /// rather than to the painter because it is the same fact as the title: what
    /// this list is for.
    pub fn hint(&self) -> &'static str {
        match self.kind {
            PickerKind::Model | PickerKind::Provider => " Enter pick · Esc cancel ",
            PickerKind::Notes => " j/k scrolls · Esc closes ",
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

/// `/help`: what the keys do, then the command table.
///
/// The table is the same one `mush --help` prints, so the two surfaces cannot
/// advertise different commands — `/compact` used to be implemented, listed by
/// `/help` and missing from `--help`, because each list was written by hand
/// (the help/status drift half of finding B2). It is a notice rather than a
/// status line: it is a thing to read, not a thing that just happened.
fn help_notice() -> String {
    format!(
        "mush: Tab cycles agents/chat · Enter sends to the focused agent · \
         Ctrl-P pick a model · Ctrl-N new chat · \
         Ctrl-C stops the focused agent · Ctrl-X stops them all. Commands:\n{}",
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
    pub spin: u64,
}

impl App {
    /// The configuration every screen reads: the UI's copy of the cell.
    pub fn cfg(&self) -> &Config {
        self.cell.ui()
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

    // ---------------------------------------------------------------- updates

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Models { endpoint, models } => self.adopt_models(endpoint, models),
            Msg::Git { stats, status } => self.adopt_git(stats, status),
            Msg::Paste(text) => {
                // A paste is something the human wants to say, so it lands in
                // the message box whichever pane has focus. An open picker is
                // the one place a paste has no meaning.
                if self.picker.is_none() {
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
                self.chat.scroll_to_bottom();
            }
            AgentEvent::Message(message) => {
                self.chat.push_message(id, message);
                // Every message is part of what the file stores — a subagent's
                // as much as the root's — but the mark is O(1): the rebuild and
                // the write wait for the debounced tick, so a streamed tool
                // result cannot stall the frame that shows it.
                self.mark_session_dirty();
                self.chat.scroll_to_bottom();
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
                self.chat.scroll_to_bottom();
            }
            AgentEvent::Error(error) => {
                self.tree.fail(id, error.clone());
                self.mark_session_dirty();
                self.refresh_git();
                // The durable half of the same fact: the row's `✗` is derived and
                // dies with the next run, while this line is tagged, stamped and
                // written to the session, so a restart still says what broke.
                self.chat.note_error_for(id, error);
                self.chat.scroll_to_bottom();
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
                // copy of the job is kept here: the badge is derived (see
                // `JobBadge`), so it cannot go stale.
                let note = format!("{} detached · {}", crate::jobs::label(job), command);
                if id == self.tree.focused {
                    self.say(note);
                } else {
                    self.say(format!("agent #{id}: {note}"));
                }
            }
            AgentEvent::JobDone { job, line } => {
                // A job's report is the owner's to read in its transcript (the
                // actor folds it in); on the screen it is the bar's line, and
                // the badge the job was on goes out with it.
                let _ = job;
                if id == self.tree.focused {
                    self.say(line);
                } else {
                    self.say(format!("agent #{id}: {line}"));
                }
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
            AgentEvent::Compact { summary } => {
                // The actor's transcript is now [system, user(summary)];
                // mirror it so nudges, saves, and the visible chat stay in
                // sync with what the model actually sees.
                let carried = Message::user(prompt::compaction_message(&summary));
                self.chat.replace_transcript(id, vec![carried]);
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
                self.chat.scroll_to_bottom();
            }
        }
    }

    /// Whether anything in the tree is working: derived from the phases, so it
    /// cannot disagree with the rows. A detached job counts: the agent may be
    /// napping, but the machine is not idle, and the tick uses this to keep the
    /// git snapshot fresh while something runs.
    pub fn busy(&self) -> bool {
        self.tree.busy()
            || self
                .tree
                .agents
                .iter()
                .any(|node| !self.tree.live_jobs(node.id).is_empty())
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
                    job.command,
                    short_age(job.age)
                )
            })
            .collect()
    }

    /// Remember a transient line for the bar: what a command just did, what the
    /// human just asked for. It fades.
    pub fn say(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            kind: StatusKind::Info,
            text: text.into(),
            set_at: Instant::now(),
        });
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
    pub fn context_meter(&self) -> String {
        let used = self.context_used_tokens();
        let window = tokens_label(self.cfg().context_tokens);
        let mark = if self.cfg().context_explicit { "" } else { "~" };
        format!("ctx {used}/{mark}{window}", used = tokens_label(used))
    }

    /// Remember something that went wrong. Errors do not fade: they stay until
    /// a later line replaces them.
    pub fn fail(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            kind: StatusKind::Error,
            text: text.into(),
            set_at: Instant::now(),
        });
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

    /// What the tree is doing, in one line — derived every frame from the
    /// phases, never stored. When nothing is busy there is nothing to say, and
    /// the bar falls back to the transient line or the idle hint.
    pub fn activity_line(&self) -> Option<String> {
        let busy: Vec<&AgentNode> = self
            .tree
            .agents
            .iter()
            .filter(|node| node.phase.is_busy())
            .collect();
        if busy.is_empty() {
            return None;
        }
        // The root napped and its children still work: what matters is that the
        // root will come back, not which child is typing.
        if !busy.iter().any(|node| node.id == AgentId::ROOT) {
            return Some(format!(
                "waiting on {} subagent(s) — the root resumes as they finish",
                busy.len()
            ));
        }
        let shown = busy
            .iter()
            .find(|node| node.id == self.tree.focused)
            .copied()
            .unwrap_or(busy[0]);
        let age = short_age(shown.since.elapsed());
        let what = match &shown.phase {
            Phase::Thinking => "thinking".to_string(),
            Phase::Activity(what) => what.clone(),
            Phase::Cancelling => "cancelling".to_string(),
            Phase::Stopped => "stopped".to_string(),
            _ => return None,
        };
        let mut line = format!("#{} {what} {age}", shown.id);
        if busy.len() > 1 {
            line.push_str(&format!(" · {} agents working", busy.len()));
        }
        Some(line)
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
        match commands::parse_command(&text) {
            Ok(command) => self.apply_command(command),
            // No slash: the human is talking to an agent.
            Err(CommandError::NotACommand) => self.deliver(text),
            // A command that exists but whose argument does not read. The line
            // was spelled beside the rule that rejected it, and the bar is
            // where every other complaint of this kind goes.
            Err(CommandError::Usage(line)) => self.say(line),
            // A slash nobody implements: a transcript error, where a human
            // looking at what they typed will see it.
            Err(CommandError::Unknown(name)) => {
                self.chat.note_error(format!("unknown command: {name}"))
            }
        }
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
            self.chat
                .push_message(AgentId::ROOT, Message::user(text.clone()));
            self.chat.scroll_to_bottom();
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
                let count = self.tree.agents.iter().filter(|n| n.leftover).count();
                self.say(if count > 0 {
                    format!("{count} leftover worktree(s) registered — /diff, /merge, /discard work on them")
                } else {
                    "no leftover worktrees".to_string()
                });
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
    /// the pane reads in, with the cursor on it — the newest is what the human
    /// came back for.
    fn open_notes_picker(&mut self) {
        let agent = self.tree.focused;
        let items = self
            .chat
            .notes_report(agent, session::now_secs(), chat::NOTES_WIDTH);
        if items.is_empty() {
            self.say(format!("nothing written about #{agent} yet"));
            return;
        }
        let cursor = items.len() - 1;
        self.picker = Some(Picker {
            kind: PickerKind::Notes,
            items,
            cursor,
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
        let Some(branch) = branch else {
            self.fail(format!("agent #{id} has no worktree branch (not isolated)"));
            return;
        };
        // The read is the one verb that changes nothing, so it goes first: it
        // is the answer to "what would merging this do", and neither of the
        // refusals below applies to looking.
        if verb == Verb::Diff {
            let text = format!("git diff HEAD...{branch}");
            self.say(text.clone());
            self.chat.note(text);
            return;
        }
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

    /// `/compact`: ask the focused agent to fold its conversation into a
    /// summary now, instead of waiting for the window to fill.
    ///
    /// The request goes to the agent, not to its row: the fold itself is the
    /// actor's job, and its `Compact` event is what replaces the transcript
    /// here, saves the session and moves the meter. So nothing is claimed
    /// about the agent's phase — unlike a nudge, which the row shows as
    /// `thinking…` because a run really is about to start. A mailbox that is
    /// gone is the one thing the human has to hear, and it is said plainly
    /// rather than left as a status line about work nobody is doing
    /// (finding B10).
    fn compact_focused(&mut self) {
        let target = self.tree.focused;
        match self.tree.agent_tx.get(&target) {
            Some(tx) if tx.send(AgentMsg::Compact).is_ok() => {
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
        self.apply_intent(keys::key(self.focus, self.picker.is_some(), key));
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
            Intent::TreeMove(step) => self.tree.move_cursor(step),
            Intent::TreeFirst => self.tree.cursor_top(),
            Intent::TreeLast => self.tree.cursor_bottom(),
            Intent::TreeFocus => self.focus_cursor_row(),
            Intent::TreeCancel => self.cancel_cursor_row(),
            Intent::TreeBackToRoot => {
                self.tree.focus(AgentId::ROOT);
            }
            Intent::Send => self.send_message(),
            Intent::Chat(key) => self.chat.apply(key),
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
    /// work, not at the phase, so both count — an agent whose run ended while
    /// its `cargo bench` still runs is not idle on the machine.
    fn working_agents(&self) -> Vec<AgentId> {
        self.tree
            .agents
            .iter()
            .filter(|node| node.phase.is_busy() || !self.tree.live_jobs(node.id).is_empty())
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
    }

    /// Focus the row the tree's cursor is on, and say whose pane the chat now
    /// shows: `Enter` in the agent pane is a move of the *view*, so the brief
    /// goes to the bar where a human can read it before typing.
    fn focus_cursor_row(&mut self) {
        if let Some(id) = self.tree.focus_cursor() {
            let brief = self
                .tree
                .node(id)
                .map(|node| node.brief.clone())
                .unwrap_or_default();
            self.say(format!("agent #{id}: {brief}"));
        }
    }

    /// `c` on the tree's cursor row: stop it if it has work to stop.
    ///
    /// Stopping an idle agent is not a no-op to be swallowed — the human asked
    /// for something that cannot happen, and the row's phase is left alone
    /// because it has no work in flight to cancel. Ending an agent is `/new`'s
    /// job.
    fn cancel_cursor_row(&mut self) {
        if let Some(node) = self.tree.agents.get(self.tree.cursor()) {
            let id = node.id;
            if !node.phase.is_busy() {
                self.say(format!("agent #{id} is not running"));
                return;
            }
            // The row's own `⊘` is the feedback; the bar shows what the tree as
            // a whole is doing.
            self.stop_one(id);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crossbeam_channel::Receiver;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
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

    fn app_at(root: std::path::PathBuf) -> App {
        let ws = Workspace::new(&root).unwrap();
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, _rx) = crossbeam_channel::unbounded::<Msg>();
        let cell = ConfigCell::own(cfg);
        let handle = spawn(cell.handle(), tx.clone(), root.clone());
        App::new(
            ws,
            cell,
            None,
            handle,
            tx,
            session_save::fake::Recorder::new(),
        )
    }

    /// An `App` whose session writes go to a real writer on a real path, for
    /// the tests that read the file back. The writer is returned so a test can
    /// see how many writes the conversation cost.
    fn app_writing(root: &std::path::Path) -> (App, Arc<session_save::Writer>) {
        let ws = Workspace::new(root).unwrap();
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, _rx) = crossbeam_channel::unbounded::<Msg>();
        let cell = ConfigCell::own(cfg);
        let handle = spawn(cell.handle(), tx.clone(), root.to_path_buf());
        let writer = Arc::new(session_save::Writer::new(root.to_path_buf()));
        let app = App::new(ws, cell, None, handle, tx, writer.clone());
        (app, writer)
    }

    /// An `App` on a scratch directory whose saves are recorded instead of
    /// written, so a test sees what the UI thread handed over and when.
    fn app_recording(label: &str) -> (App, Arc<session_save::fake::Recorder>) {
        let root = dir(label);
        let ws = Workspace::new(&root).unwrap();
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, _rx) = crossbeam_channel::unbounded::<Msg>();
        let cell = ConfigCell::own(cfg);
        let handle = spawn(cell.handle(), tx.clone(), root.clone());
        let recorder = session_save::fake::Recorder::new();
        let app = App::new(ws, cell, None, handle, tx, recorder.clone());
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
        let ws = Workspace::new(root).unwrap();
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, _rx) = crossbeam_channel::unbounded::<Msg>();
        let cell = ConfigCell::own(cfg);
        let handle = spawn(cell.handle(), tx.clone(), root.to_path_buf());
        App::new(
            ws,
            cell,
            Session::load(root),
            handle,
            tx,
            session_save::fake::Recorder::new(),
        )
    }

    /// The painted screen, row by row, at a real terminal size. The layout
    /// tiers, the panes and the foot are only true together, which is why the
    /// audit that found these defects read rows instead of reasoning about
    /// them. A pane's border is stripped: what a test reads is the row's text.
    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_matches('│')
                    .trim_end()
                    .to_string()
            })
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
        // A second /merge must not re-run git or claim a second merge.
        run(&mut app, "/merge 1");
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
        let ws = Workspace::new(&root).unwrap();
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, _rx) = crossbeam_channel::unbounded::<Msg>();
        let cell = ConfigCell::own(cfg);
        let handle = spawn(cell.handle(), tx.clone(), root.clone());
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

        let app = App::new(
            ws,
            cell,
            Some(stored),
            handle,
            tx,
            session_save::fake::Recorder::new(),
        );

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
        let ws = Workspace::new(&root).unwrap();
        // Nothing answers here: an agent that ran would fail, loudly, in the
        // events this test reads.
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let cell = ConfigCell::own(cfg);
        let handle = spawn(cell.handle(), tx.clone(), root.clone());
        let stored = stored_with_agent(
            &root,
            session::StoredStatus::Done,
            vec![Message::user("port the parser"), Message::assistant("done")],
        );

        let app = App::new(
            ws,
            cell,
            Some(stored),
            handle,
            tx,
            session_save::fake::Recorder::new(),
        );

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
        let ws = Workspace::new(&root).unwrap();
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, _rx) = crossbeam_channel::unbounded::<Msg>();
        let cell = ConfigCell::own(cfg);
        let handle = spawn(cell.handle(), tx.clone(), root.clone());
        let refusal = "model returned HTTP 400: The `reasoning_content` in the thinking \
                       mode must be passed back to the API.";
        let stored = stored_with_agent(
            &root,
            session::StoredStatus::Failed(refusal.to_string()),
            vec![Message::user("port the parser"), Message::assistant("done")],
        );

        let app = App::new(
            ws,
            cell,
            Some(stored),
            handle,
            tx,
            session_save::fake::Recorder::new(),
        );

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
        let ws = Workspace::new(&root).unwrap();
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, _rx) = crossbeam_channel::unbounded::<Msg>();
        let cell = ConfigCell::own(cfg);
        let handle = spawn(cell.handle(), tx.clone(), root.clone());
        let stored = stored_with_agent(
            &root,
            session::StoredStatus::Failed("the endpoint stopped responding".into()),
            vec![
                Message::user("port the parser"),
                Message::assistant("starting now"),
                Message::user("and make it fast"),
            ],
        );

        let app = App::new(
            ws,
            cell,
            Some(stored),
            handle,
            tx,
            session_save::fake::Recorder::new(),
        );

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
                .launch(Launch {
                    owner,
                    command: "cargo build".to_string(),
                    exclusive: false,
                    job,
                    mailbox: tx,
                })
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
        let ws = Workspace::new(&root).unwrap();
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, _rx) = crossbeam_channel::unbounded::<Msg>();
        let cell = ConfigCell::own(cfg);
        let handle = spawn(cell.handle(), tx.clone(), root.clone());
        let mut app = App::new(ws, cell, None, handle, tx, recorder);
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
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        const FRAMES: u32 = 30;
        let start = Instant::now();
        for _ in 0..FRAMES {
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
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
            small[4].contains("you › port the parser"),
            "the one transcript row is the conversation, not a foot: {:?}",
            &small[3..6]
        );
        assert!(
            small[3].contains("+6 more lines"),
            "six note lines are hidden and the pane says so: {:?}",
            small[3]
        );
        assert!(
            !small.iter().any(|row| row.contains("/notes")),
            "a pane with no room for the count line does not pretend otherwise: {small:?}"
        );

        // 60×17: two rows of notes, then one that says four lines are not
        // there. Six lines were written, two are painted, four are counted.
        let roomy = screen(&mut app, 60, 17);
        assert!(
            roomy[4].contains("you › port the parser"),
            "{:?}",
            &roomy[3..9]
        );
        assert_eq!(roomy[5], "  +4 more lines · /notes", "{:?}", &roomy[3..9]);
        assert_eq!(roomy[6], "· note 4", "{:?}", &roomy[3..9]);
        assert_eq!(roomy[7], "· note 5", "{:?}", &roomy[3..9]);
        assert!(
            !roomy.iter().any(|row| row.contains("note 3")),
            "the cap is two rows, and the count is the rest: {:?}",
            &roomy[3..9]
        );
    }

    /// `/notes` is the other half of the cap: the lines the foot ceded are read
    /// in full, oldest first, with the cursor on the newest.
    #[test]
    fn the_notes_command_lists_what_the_foot_could_not_show() {
        let (mut app, _rx) = test_app("notes-command");
        assert!(
            app.chat
                .notes_report(AgentId::ROOT, session::now_secs(), chat::NOTES_WIDTH)
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

    fn test_app(label: &str) -> (App, Receiver<Msg>) {
        let root = std::env::temp_dir().join(format!("mush-app-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let ws = Workspace::new(&root).unwrap();
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let cell = ConfigCell::own(cfg);
        let handle = spawn(cell.handle(), tx.clone(), root.clone());
        let app = App::new(
            ws,
            cell,
            None,
            handle,
            tx,
            session_save::fake::Recorder::new(),
        );
        (app, rx)
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
            matches!(asked.try_recv(), Ok(AgentMsg::Compact)),
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
        assert_eq!(app.activity_line().as_deref(), Some("#0 cancelling 0s"));
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
        assert_eq!(app.activity_line(), None, "nothing to report");
    }

    /// The stale-status defect: a finished run must leave nothing behind. The
    /// bar derives from the phases, so when the run ends the line is gone.
    #[test]
    fn a_finished_run_leaves_nothing_behind() {
        let (mut app, _rx) = test_app("finished-phase");
        let conversation = app.tree.conversation();
        app.tree.begin(AgentId::ROOT, None);
        app.tree.age(AgentId::ROOT, Duration::from_secs(70));
        assert_eq!(app.activity_line().as_deref(), Some("#0 thinking 1m10s"));

        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Done,
        });
        assert_eq!(app.tree.agents[0].phase, Phase::Done);
        assert!(!app.busy());
        assert_eq!(app.activity_line(), None, "no `thinking` survives the run");
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
            app.activity_line().as_deref(),
            Some("waiting on 1 subagent(s) — the root resumes as they finish")
        );
    }

    /// Busy agents are named with their age: a model that has thought for two
    /// minutes should look different from one that has thought for a second.
    #[test]
    fn busy_agents_are_named_with_their_age() {
        let (mut app, _rx) = test_app("activity-age");
        app.tree.begin(AgentId::ROOT, None);
        app.tree.activity(AgentId::ROOT, "edit_file src/lib.rs");
        app.tree.age(AgentId::ROOT, Duration::from_secs(12));
        assert_eq!(
            app.activity_line().as_deref(),
            Some("#0 edit_file src/lib.rs 12s")
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

    /// The layout must survive every terminal size.
    ///
    /// This is the regression guard for the arithmetic in `ui.rs`: subticks,
    /// index math, and `Rect` construction all have to hold at the floor and at
    /// sizes between the tiers — a panic there is a blank screen for the user.
    #[test]
    fn the_layout_survives_every_size() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let (mut app, _rx) = test_app("layout-sizes");
        // A tree with depth, a branch, and every phase, so no branch of the
        // renderer goes unexercised.
        let conversation = app.tree.conversation();
        for (id, parent, depth) in [(1u64, 0u64, 1usize), (2, 1, 2)] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId(parent),
                event: AgentEvent::Spawned {
                    child: id,
                    parent,
                    brief: "a deliberately long brief that will not fit".to_string(),
                    depth,
                    branch: Some(format!("mush/{id}")),
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }
        app.tree.activity(AgentId(1), "edit_file src/lexer.rs 12s");
        app.tree.fail(AgentId(2), "no route to host".to_string());
        app.refresh_git();

        for (width, height) in [
            (200u16, 50u16),
            (160, 26),
            (120, 32),
            (100, 25),
            (80, 24),
            (79, 24),
            (60, 20),
            (60, 19),
            (50, 12),
            (40, 10),
            (39, 9),
            (20, 5),
            (1, 1),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| crate::ui::draw(frame, &mut app))
                .unwrap_or_else(|error| panic!("draw failed at {width}x{height}: {error}"));
            // Both focus states, since a border is painted differently when it
            // is focused and a focused-but-tiny pane is the awkward case.
            app.focus = Focus::Agents;
            terminal
                .draw(|frame| crate::ui::draw(frame, &mut app))
                .unwrap_or_else(|error| panic!("draw failed at {width}x{height}: {error}"));
            app.focus = Focus::Chat;
        }
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

        let ws = Workspace::new(&root).unwrap();
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, _rx) = crossbeam_channel::unbounded::<Msg>();
        let cell = ConfigCell::own(cfg);
        let handle = spawn(cell.handle(), tx.clone(), root.clone());
        let mut app = App::new(
            ws,
            cell,
            None,
            handle,
            tx,
            session_save::fake::Recorder::new(),
        );

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
}
