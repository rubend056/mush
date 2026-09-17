//! Application state and the update function.
//!
//! The UI thread owns no file state: agents read and write the workspace
//! themselves, and this module is the human's view of them — the agent tree,
//! the focused transcript, the git facts, and the message box. Every input — a
//! keystroke, an agent event — becomes a `Msg` and flows through `App::update`.
//!
//! The agents themselves live in [`tree`], which owns their ids, phases and
//! focus; the conversation lives in [`chat`], which owns every transcript the
//! screen shows, the notices, the message box and the context meter. This
//! module routes messages into both and renders what they say.

mod chat;
mod tree;

pub use chat::{Chat, Footnote, NoticeKind, Rank};
pub use tree::{AgentId, AgentNode, AgentTree, ConversationId, Existing, Landed, Phase, Spawn};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use mush_core::message::Message;
use mush_core::{
    git, prompt, session, text::mask_key, userconfig, Config, Provider, Session, UserConfig,
    Workspace,
};

use crate::agent::{self, spawn, AgentEvent, AgentMsg, RootHandle};
use crate::http;

pub enum Msg {
    Key(KeyEvent),
    /// Pasted text, delivered whole by the terminal's bracketed paste. Inserted
    /// in one update: a paste must not cost one message per character.
    Paste(String),
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Agents,
    Chat,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Model,
    Provider,
}

/// A small modal list (models or providers) that grabs the keyboard until
/// Enter or Esc. Drawn as a centered popup by `ui::draw_picker`.
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
        }
    }
}

/// `500k`, `8192`, `1M` — the way a window size wants to be read.
pub fn tokens_label(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
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

#[derive(Clone, Debug)]
pub struct Status {
    pub kind: StatusKind,
    pub text: String,
    pub set_at: Instant,
}

pub struct App {
    pub ws: Workspace,
    pub cfg: Config,
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
    /// Shared with the agent actors so runtime config changes apply everywhere.
    pub cfg_shared: Arc<Mutex<Config>>,
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
    pub fn new(
        ws: Workspace,
        cfg: Config,
        stored: Option<Session>,
        root: RootHandle,
        ui_tx: Sender<Msg>,
        models: Vec<http::Model>,
    ) -> Self {
        let system = Message::system(prompt::system_prompt(&ws.root_str()));
        let (messages, stored_agents) = match stored {
            Some(session) => (session.messages, session.agents),
            None => (Vec::new(), Vec::new()),
        };
        let cfg_shared = root.cfg.clone();
        let mut app = Self {
            ws,
            cfg,
            focus: Focus::Chat,
            chat: Chat::new(system, messages),
            models,
            picker: None,
            git: None,
            tree: AgentTree::rooted(root),
            cfg_shared,
            ui_tx,
            git_at: None,
            git_in_flight: false,
            status: None,
            should_quit: false,
            dirty_screen: true,
            spin: 0,
        };
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
        let cfg = self.cfg_shared.clone();
        let ui_tx = self.ui_tx.clone();
        let conversation = self.tree.conversation().0;
        let ids = self.tree.ids();
        let live = self.tree.live();
        let root = self.ws.root().to_path_buf();
        for agent in stored {
            // Keep the counter above every restored id, or the next spawn hands
            // a live child an id a restored agent already holds (finding B1).
            self.tree.reserve_ids(agent.id + 1);
            let phase = match &agent.status {
                session::StoredStatus::Done => Phase::Done,
                session::StoredStatus::Stopped => Phase::Stopped,
                session::StoredStatus::Failed(error) => Phase::Failed(error.clone()),
                // A run that was still in flight at shutdown is not a result.
                session::StoredStatus::Idle => Phase::Idle,
            };
            let landed = agent.landed.map(|landed| match landed {
                session::StoredLanded::Merged => Landed::Merged,
                session::StoredLanded::Discarded => Landed::Discarded,
            });
            let tx = agent::revive(
                cfg.clone(),
                ui_tx.clone(),
                conversation,
                ids.clone(),
                live.clone(),
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
                summary: agent.summary,
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
        self.tree.agent_stats = stats;
        self.git = status;
        self.git_at = Some(Instant::now());
        self.git_in_flight = false;
        self.dirty_screen = true;
    }

    /// The window in tokens, for the meter. The number is the conversation's,
    /// not a copy of it: nothing can go stale between a push and a draw.
    pub fn context_used_tokens(&self) -> usize {
        self.chat.used_tokens()
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
            }
            AgentEvent::Running { cancel } => {
                // A run started, possibly one the UI did not ask for (an idle
                // agent woken by a child's result). Mark it so `busy`, the
                // spinner, and Ctrl-C agree with the actor. The last run's
                // summary belongs to that run, not this one (finding B14).
                self.tree.begin(id, Some(cancel));
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
                if id == AgentId::ROOT {
                    self.save_session();
                }
                self.chat.scroll_to_bottom();
            }
            AgentEvent::Stopped => {
                // Stopped is not failed and not done: the run produced nothing,
                // and the actor is idle and resumable. Saying which one it is
                // is the difference between a lost agent and a parked one.
                self.tree.stopped(id);
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
                self.refresh_git();
                self.chat.note_error_for(id, error);
                self.chat.scroll_to_bottom();
            }
            AgentEvent::Done => {
                let summary = self.last_assistant_text(id);
                // Every Done replaces the row's summary; keeping the first one
                // described a run that ended long ago (finding B14).
                self.tree.finish(id, summary);
                self.refresh_git();
            }
            AgentEvent::Context { tokens } => {
                // The actor learned the endpoint's real window from a server
                // complaint; the UI owns the copy the bar, `/context`, and the
                // tool caps read, so it has to adopt the same number or the
                // next `/model` clobbers it (finding B7).
                if !self.cfg.context_explicit && tokens != self.cfg.context_tokens {
                    self.cfg.context_tokens = tokens;
                    self.apply_config();
                }
            }
            AgentEvent::Compact { summary } => {
                // The actor's transcript is now [system, user(summary)];
                // mirror it so nudges, saves, and the visible chat stay in
                // sync with what the model actually sees.
                let carried = Message::user(prompt::compaction_message(&summary));
                self.chat.replace_transcript(id, vec![carried]);
                if id == AgentId::ROOT {
                    self.save_session();
                    self.chat
                        .note("context compacted — continuing from a summary");
                }
                self.chat.scroll_to_bottom();
            }
        }
    }

    /// Whether anything in the tree is working: derived from the phases, so it
    /// cannot disagree with the rows.
    pub fn busy(&self) -> bool {
        self.tree.busy()
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
        let spelling = tokens_label(self.cfg.context_tokens);
        if self.cfg.context_explicit {
            format!("ctx {spelling} (set)")
        } else {
            format!("ctx ~{spelling}")
        }
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

    fn send_message(&mut self) {
        let text = self.chat.take_input().trim().to_string();
        if text.is_empty() {
            return;
        }
        if text.starts_with('/') {
            self.run_command(&text);
            return;
        }
        let target = self.tree.focused;
        if target == AgentId::ROOT {
            // The human's words belong in the transcript they can see, whether
            // the root is starting a run or already in one.
            self.chat
                .push_message(AgentId::ROOT, Message::user(text.clone()));
            self.chat.scroll_to_bottom();
            self.save_session();
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

    fn run_command(&mut self, command: &str) {
        let (name, rest) = command
            .split_once(' ')
            .map(|(name, rest)| (name, rest.trim()))
            .unwrap_or((command, ""));
        match name {
            "/new" | "/clear" => self.new_chat(),
            "/quit" | "/q" => self.should_quit = true,
            "/help" | "/?" => {
                self.chat.note(
                    "mush: Tab cycles agents/chat · Enter sends to the focused agent · \
                     Ctrl-P pick a model · Ctrl-N new chat · \
                     Ctrl-C stops the focused agent · Ctrl-X stops them all. \
                     Commands: /provider /model /context /url /key /models \
                     /worktrees /diff /merge /discard /forget /new /quit \
                     (/merge and /discard run git for you and reclaim the worktree; \
                     /forget drops the agent from this session and leaves the branch)",
                );
            }
            "/context" => {
                if rest.is_empty() {
                    self.say(format!(
                        "{} · {} tokens used · set it with /context <tokens>",
                        self.context_label(),
                        self.context_used_tokens()
                    ));
                    return;
                }
                let Ok(tokens) = rest.trim().parse::<usize>() else {
                    self.say("usage: /context <tokens>");
                    return;
                };
                if tokens == 0 {
                    self.say("usage: /context <tokens>");
                    return;
                }
                self.cfg.set_context(tokens);
                self.apply_config();
                self.save_session();
                self.say(format!(
                    "{} — remembered for this workspace",
                    self.context_label()
                ));
            }
            "/diff" | "/merge" | "/discard" => self.worktree_command(name, rest),
            "/forget" => {
                let Ok(id) = rest.trim().parse::<u64>() else {
                    self.say("usage: /forget <agent id>");
                    return;
                };
                self.forget_agent(AgentId(id));
            }
            "/worktrees" => {
                self.discover_worktrees();
                self.refresh_git();
                let count = self.tree.agents.iter().filter(|n| n.leftover).count();
                self.say(if count > 0 {
                    format!("{count} leftover worktree(s) registered — /diff, /merge, /discard work on them")
                } else {
                    "no leftover worktrees".to_string()
                });
            }
            "/provider" => {
                if rest.is_empty() {
                    self.open_provider_picker();
                } else {
                    self.apply_provider(rest);
                }
            }
            "/model" => self.open_model_picker(),
            "/url" => {
                if rest.is_empty() {
                    self.say(
                        "usage: /url http://host:port — base URL of an OpenAI-compatible \
                                  endpoint",
                    );
                    return;
                }
                self.cfg.set_base_url(rest);
                // A new endpoint may host a different model with a different
                // window; re-derive it unless the human stated one (finding A5).
                self.cfg.rederive_context();
                self.refresh_models();
                self.say(format!(
                    "endpoint: {} · {}",
                    self.cfg.base_url,
                    self.context_label()
                ));
                self.apply_config();
                self.persist_user_config();
            }
            "/key" => {
                if rest.is_empty() {
                    match &self.cfg.api_key {
                        Some(key) => self.say(format!("api key set ({}…)", mask_key(key))),
                        None => self.say("no api key — /key <secret> sets one (memory only)"),
                    }
                    return;
                }
                self.cfg.api_key = Some(rest.to_string());
                self.apply_config();
                self.persist_user_config();
                self.say(format!(
                    "api key set ({}…) — saved to {}",
                    mask_key(rest),
                    userconfig::config_path().display()
                ));
            }
            "/models" => {
                self.refresh_models();
                self.say(if self.models.is_empty() {
                    format!("no models from {}", self.cfg.models_url())
                } else {
                    format!(
                        "{} models from {}",
                        self.models.len(),
                        self.cfg.models_url()
                    )
                });
            }
            other => self.chat.note_error(format!("unknown command: {other}")),
        }
    }

    // ------------------------------------------------------------ providers

    /// The config is shared with every agent actor, so a runtime change here
    /// applies to the root *and* all subagents on their next request.
    fn apply_config(&self) {
        if let Ok(mut shared) = self.cfg_shared.lock() {
            *shared = self.cfg.clone();
        }
    }

    /// Remember the current setup in the home config file so the API key (and
    /// endpoint defaults) survive restarts. Never writes to the workspace.
    fn persist_user_config(&mut self) {
        let user = UserConfig {
            api_key: self.cfg.api_key.clone(),
            provider: self.cfg.provider.name().to_string(),
            base_url: self.cfg.base_url.clone(),
            model: self.cfg.model.clone(),
        };
        if let Err(error) = user.save() {
            self.fail(format!("could not save home config: {error}"));
        }
    }

    /// Re-fetch the model list from the current endpoint, falling back to the
    /// provider's built-in list when the endpoint cannot answer. An advertised
    /// context window is adopted here, so it lands before the next request.
    pub fn refresh_models(&mut self) {
        self.models = http::list_models(&self.cfg);
        self.adopt_advertised_context();
    }

    /// Take the endpoint's word for the window of the model in use, unless the
    /// human stated one.
    fn adopt_advertised_context(&mut self) {
        let advertised = self
            .models
            .iter()
            .find(|model| model.id == self.cfg.model)
            .and_then(|model| model.context);
        if let Some(tokens) = advertised {
            if self.cfg.adopt_context(tokens) {
                self.apply_config();
            }
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
            .position(|model| model.id == self.cfg.model)
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

    fn open_provider_picker(&mut self) {
        let items: Vec<String> = Provider::ALL.iter().map(|p| p.name().to_string()).collect();
        let cursor = items
            .iter()
            .position(|name| Provider::parse(name) == Some(self.cfg.provider))
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
                "unknown provider `{name}` — try deepseek or custom"
            ));
            return;
        };
        self.cfg.provider = provider;
        // Switching to DeepSeek points at its hosted API; coming back to
        // custom keeps whatever endpoint is set.
        if provider == Provider::DeepSeek {
            self.cfg.base_url = provider.default_base_url().to_string();
        }
        let known = self.cfg.default_models();
        let mut model = self.cfg.model.clone();
        if !known.contains(&model) {
            if let Some(first) = known.first() {
                model = first.clone();
            }
        }
        // The new provider means a new window; re-derive it unless the human
        // stated one (finding A5).
        self.cfg.set_model(&model);
        self.refresh_models();
        self.apply_config();
        self.persist_user_config();
        self.say(format!(
            "provider: {} · {}",
            provider.name(),
            self.context_label()
        ));
    }

    fn key_picker(&mut self, key: KeyEvent) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Enter => {
                let kind = picker.kind;
                let item = picker.items.get(picker.cursor).cloned();
                self.picker = None;
                if let Some(item) = item {
                    self.pick(kind, &item);
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if picker.cursor + 1 < picker.items.len() {
                    picker.cursor += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                picker.cursor = picker.cursor.saturating_sub(1);
            }
            KeyCode::Char('g') | KeyCode::Home => picker.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => {
                picker.cursor = picker.items.len().saturating_sub(1)
            }
            _ => {}
        }
    }

    fn pick(&mut self, kind: PickerKind, item: &str) {
        match kind {
            PickerKind::Model => {
                // The picker labels models with their window; the id is the
                // part before the separator.
                let id = item.split(" · ").next().unwrap_or(item);
                // A new model means a new documented window, unless the human
                // stated one (finding A5).
                self.cfg.set_model(id);
                self.adopt_advertised_context();
                self.apply_config();
                self.persist_user_config();
                self.say(format!(
                    "model: {} · {}",
                    self.cfg.label(),
                    self.context_label()
                ));
            }
            PickerKind::Provider => self.apply_provider(item),
        }
    }

    /// Print the exact git commands for an isolated agent's branch. The human
    /// merges in their own IDE — mush never auto-merges.
    /// `/diff` names the command to read the work; `/merge` and `/discard` run
    /// it. mush cannot see a git command the human runs in their own shell, so
    /// the only thing that ever reclaims a worktree and its branch is doing it
    /// here — which is why the pane stayed cluttered with leftovers.
    fn worktree_command(&mut self, command: &str, rest: &str) {
        let Ok(raw) = rest.trim().parse::<u64>() else {
            self.say(format!("usage: {command} <agent id>"));
            return;
        };
        let id = AgentId(raw);
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
        if command == "/diff" {
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
                "agent #{id} is still running — Ctrl-C stops it before you {command} its work"
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
        match command {
            "/merge" => match git::run(&root, &["merge", branch.as_str()]) {
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
                    self.save_session();
                }
            },
            "/discard" => {
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
                    self.save_session();
                }
            }
            _ => {}
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
        let root = spawn(
            self.cfg.clone(),
            self.ui_tx.clone(),
            self.ws.root().to_path_buf(),
        );
        // The respawned root owns its own config cell, conversation tag, and id
        // counter; adopt all of them with a fresh tree, or a later /model would
        // never reach the agent, its events would look stale, and a spawn could
        // reuse an id.
        self.cfg_shared = root.cfg.clone();
        self.tree = AgentTree::rooted(root);
        // Running agents vanish with the old conversation; worktrees they left
        // behind are still reviewable (they are re-listed below).
        self.chat.clear();
        self.spin = 0;
        self.discover_worktrees();
        self.refresh_git();
        self.save_session();
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

    fn save_session(&mut self) {
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
        let session = Session {
            root: self.ws.root_str(),
            model: self.cfg.model.clone(),
            provider: self.cfg.provider.name().to_string(),
            base_url: self.cfg.base_url.clone(),
            // Only a window the human stated is worth remembering; a discovered
            // one is re-read next time, so it cannot go stale.
            context: self.cfg.context_explicit.then_some(self.cfg.context_tokens),
            updated: session::now_secs(),
            messages: self.chat.transcript(AgentId::ROOT).to_vec(),
            agents,
        };
        if let Err(error) = session.save(self.ws.root()) {
            self.fail(format!("could not save session: {error}"));
        }
    }

    // ------------------------------------------------------------------ input

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if ctrl {
            match key.code {
                KeyCode::Char('q') => return self.request_quit(),
                KeyCode::Char('c') => return self.interrupt(),
                KeyCode::Char('x') => return self.interrupt_all(),
                KeyCode::Char('n') => return self.new_chat(),
                KeyCode::Char('p') => return self.open_model_picker(),
                _ => {}
            }
        }

        match key.code {
            KeyCode::Tab => return self.cycle_focus(1),
            KeyCode::BackTab => return self.cycle_focus(-1),
            _ => {}
        }

        if self.picker.is_some() {
            self.key_picker(key);
            return;
        }

        match self.focus {
            Focus::Agents => self.key_agents(key),
            Focus::Chat => self.key_chat(key),
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
        // What the human is looking at: the focused agent if it is busy, else
        // the one agent that is busy (there is nothing to disambiguate).
        let busy: Vec<AgentId> = self
            .tree
            .agents
            .iter()
            .filter(|node| node.phase.is_busy())
            .map(|node| node.id)
            .collect();
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
        let targets: Vec<AgentId> = self
            .tree
            .agents
            .iter()
            .filter(|node| node.phase.is_busy())
            .map(|node| node.id)
            .collect();
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

    fn key_agents(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.tree.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.tree.move_cursor(-1),
            KeyCode::Char('g') | KeyCode::Home => self.tree.cursor_top(),
            KeyCode::Char('G') | KeyCode::End => self.tree.cursor_bottom(),
            KeyCode::Enter => {
                if let Some(id) = self.tree.focus_cursor() {
                    let brief = self
                        .tree
                        .node(id)
                        .map(|node| node.brief.clone())
                        .unwrap_or_default();
                    self.say(format!("agent #{id}: {brief}"));
                }
            }
            KeyCode::Char('c') => {
                if let Some(node) = self.tree.agents.get(self.tree.cursor()) {
                    let id = node.id;
                    if !node.phase.is_busy() {
                        // Stop cancels work; an idle agent has none. Ending one
                        // is `/new`'s job.
                        self.say(format!("agent #{id} is not running"));
                        return;
                    }
                    // The row's own `⊘` is the feedback; the bar shows what the
                    // tree as a whole is doing.
                    self.stop_one(id);
                }
            }
            KeyCode::Esc => {
                self.tree.focus(AgentId::ROOT);
            }
            _ => {}
        }
    }

    /// The chat pane's keys: the message box and the transcript's scrollback
    /// belong to the [`Chat`], so they are routed to it whole. `<Enter>` is the
    /// one key it cannot own: sending is the agents' business.
    fn key_chat(&mut self, key: KeyEvent) {
        let modified = key.modifiers.contains(KeyModifiers::SHIFT)
            || key.modifiers.contains(KeyModifiers::ALT);
        if key.code == KeyCode::Enter && !modified {
            self.send_message();
            return;
        }
        let _ = self.chat.key(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crossbeam_channel::Receiver;

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
        let handle = spawn(cfg.clone(), tx.clone(), root.clone());
        App::new(ws, cfg, None, handle, tx, Vec::new())
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

        app.worktree_command("/merge", "1");

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
        app.worktree_command("/merge", "1");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `/discard` throws the work away on purpose and says so.
    #[test]
    fn discarding_an_agent_removes_the_worktree_and_the_branch() {
        let root = repo("discard");
        isolated_work(&root, 2, "throwaway");
        let mut app = app_at(root.clone());

        app.worktree_command("/discard", "2");

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

        app.worktree_command("/merge", "4");
        app.worktree_command("/discard", "4");

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
        let handle = spawn(cfg.clone(), tx.clone(), root.clone());
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
        };

        let app = App::new(ws, cfg, Some(stored), handle, tx, Vec::new());

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

    fn test_app(label: &str) -> (App, Receiver<Msg>) {
        let root = std::env::temp_dir().join(format!("mush-app-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let ws = Workspace::new(&root).unwrap();
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let handle = spawn(cfg.clone(), tx.clone(), root.clone());
        let app = App::new(
            ws,
            cfg,
            None,
            handle,
            tx,
            vec![http::Model {
                id: "test-model".to_string(),
                context: None,
            }],
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
        let before = app.cfg_shared.clone();

        app.run_command("/new");

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
        // The respawned root owns a fresh config cell and the UI adopted it;
        // without that, a later /model would never reach the agent.
        assert!(!Arc::ptr_eq(&before, &app.cfg_shared));

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
        app.run_command("/new");
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

    /// A tree `/new` abandoned can still spawn children, and their `Spawned`
    /// events are dropped — so this is the only moment the UI can tell such a
    /// child to go away. Without it the child runs unseen forever.
    #[test]
    fn a_child_spawned_by_an_abandoned_tree_is_shut_down() {
        let (mut app, _rx) = test_app("stale-child");
        let abandoned = app.tree.conversation();
        app.run_command("/new");
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
        let handle = spawn(cfg.clone(), tx.clone(), root.clone());
        let mut app = App::new(ws, cfg, None, handle, tx, Vec::new());

        assert!(
            app.tree.agents.iter().any(|node| node.id == AgentId(7)),
            "the leftover is registered"
        );
        assert!(
            app.tree.ids().load(Ordering::SeqCst) >= 8,
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
