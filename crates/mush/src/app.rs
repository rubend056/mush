//! Application state and the update function.
//!
//! The UI thread owns no file state: agents read and write the workspace
//! themselves, and this module is the human's view of them — the agent tree,
//! the focused transcript, the git facts, and the message box. Every input — a
//! keystroke, an agent event — becomes a `Msg` and flows through `App::update`.

use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};

use mush_core::message::Message;
use mush_core::{
    git, prompt, session, text::mask_key, userconfig, Config, Provider, Session, UserConfig,
    Workspace,
};

use crate::agent::{self, spawn, AgentEvent, AgentMsg, RootHandle};
use crate::http;
use crate::input::Input;

pub enum Msg {
    Key(KeyEvent),
    /// Pasted text, delivered whole by the terminal's bracketed paste. Inserted
    /// in one update: a paste must not cost one message per character.
    Paste(String),
    /// A mouse event: today only the wheel, which scrolls the transcript.
    Mouse(MouseEvent),
    /// An event from an agent actor. `conversation` identifies the tree that
    /// sent it, so an actor left over from `/new` cannot write into the new
    /// chat: events are tagged and the UI drops the stale ones.
    Agent {
        conversation: u64,
        id: u64,
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

/// One entry in the agent tree. Order in the vector is tree order; ids are
/// stable, so positions do not shift while agents are alive.
/// What an agent is doing *now*, as opposed to what it last said it was doing.
///
/// The row glyph, the activity text, and the status bar all render this, so a
/// finished or cancelled run cannot leave a `thinking…` behind: when a phase
/// ends, the lines that described it stop existing. Nothing here is a string
/// mirror of the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Nothing in flight: never ran, or its run ended without a result.
    Idle,
    /// A request is in flight and the model has not named a tool yet.
    Thinking,
    /// The last thing the agent reported doing: `edit_file src/lib.rs`, `run_command cargo test`, `summarizing…`.
    Activity(String),
    /// A Stop is on its way and the actor has not yielded yet.
    Cancelling,
    /// A Stop landed: the run ended with no result, but the actor is still
    /// alive and a nudge resumes it. Its own state, because `Idle` (never ran),
    /// `Done` (produced a result) and `Stopped` (produced nothing, resumable)
    /// are three different things and blanking a stop to `Idle` lost the one
    /// fact the human needed: that work was interrupted mid-flight.
    Stopped,
    /// The run finished; `summary` holds what it produced.
    Done,
    /// The run failed; the payload is what the human needs to read.
    Failed(String),
}

impl Phase {
    /// Whether work is in flight. `Idle`, `Done`, and `Failed` are at rest.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Phase::Thinking | Phase::Activity(_) | Phase::Cancelling
        )
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

/// A line for the transcript that is not a message: a note from mush itself.
/// It is tagged with the agent it concerns, so a root-level failure is not
/// rendered into every child's transcript (finding B19).
#[derive(Clone, Debug)]
pub struct Notice {
    pub agent: u64,
    pub kind: NoticeKind,
    pub text: String,
}

/// Only failures are red. Hints — `/help`, the git command to merge a branch —
/// are information, and colouring them like errors is how a screen cries wolf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Error,
}

/// Where an isolated agent's work ended up, once the human landed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landed {
    /// Merged into the main branch; the worktree and the branch were reclaimed.
    Merged,
    /// Thrown away on purpose; the worktree and the branch were reclaimed.
    Discarded,
}

pub struct AgentNode {
    pub id: u64,
    pub parent: Option<u64>,
    pub depth: usize,
    pub brief: String,
    /// What it is doing now, and since when — the row and the bar derive from
    /// this instead of storing rendered text.
    pub phase: Phase,
    pub since: Instant,
    pub branch: Option<String>,
    /// The agent's result once it has one (a leftover worktree has one too).
    pub summary: Option<String>,
    /// Found on disk rather than spawned in this session. An explicit flag, not
    /// a sentinel `brief`: the brief is now recovered from the commit subject,
    /// so matching on its text would stop recognising leftovers the moment they
    /// learned their real names.
    pub leftover: bool,
    /// Set once `/merge` or `/discard` reclaimed the worktree.
    pub landed: Option<Landed>,
}

pub struct App {
    pub ws: Workspace,
    pub cfg: Config,
    pub focus: Focus,
    pub chat: Vec<Message>,
    pub notices: Vec<Notice>,
    /// The message box: text plus a grapheme cursor, so editing is not
    /// append-and-backspace.
    pub input: Input,
    pub chat_scroll: usize,
    pub models: Vec<http::Model>,
    pub picker: Option<Picker>,
    /// The main worktree's branch, dirty count, and uncommitted line delta.
    pub git: Option<git::RepoStatus>,
    /// Each isolated agent's own work, measured on its branch. Refreshed by
    /// events, never computed while painting.
    pub agent_stats: HashMap<u64, git::Stat>,
    /// Bytes of the root conversation, so the context meter costs nothing to
    /// draw. Updated whenever the transcript changes.
    pub context_used: usize,
    pub agents: Vec<AgentNode>,
    pub agent_cursor: usize,
    /// The agent whose transcript the chat shows and whose mailbox typing targets.
    pub focused: u64,
    /// Transcripts of non-root agents; the root's lives in `chat`.
    pub agent_msgs: HashMap<u64, Vec<Message>>,
    /// Steering handles: one mailbox per agent, keyed by id.
    pub agent_tx: HashMap<u64, Sender<AgentMsg>>,
    /// Each running agent's cancellation flag. The HTTP reader polls it, so a
    /// Ctrl-C stops a model call that has not answered yet — the mailbox alone
    /// cannot: the actor is blocked inside the request.
    pub agent_cancel: HashMap<u64, Arc<AtomicBool>>,
    /// Shared with the agent actors so runtime config changes apply everywhere.
    pub cfg_shared: Arc<Mutex<Config>>,
    /// The tree's id counter. Leftover worktrees are registered under their own
    /// ids, so the next spawn must start above them or two nodes share an id
    /// and every id-keyed lookup hits the wrong one (finding B1).
    agent_ids: Arc<AtomicU64>,
    /// The UI event channel, needed to respawn the root actor on /new.
    ui_tx: Sender<Msg>,
    /// The tree-wide running count, shared with the actors so an agent revived
    /// from a stored session is counted against the ceiling like any other.
    agent_live: Arc<AtomicU64>,
    /// Which conversation the live actor tree belongs to; events tagged with
    /// any other are from an abandoned tree and are ignored.
    conversation: u64,
    /// When the git snapshot was last taken, so a long run refreshes it.
    git_at: Option<Instant>,
    /// A transient line for the bar: what just happened, or what went wrong.
    /// Work in progress does not live here — it is derived from the phases.
    pub status: Option<Status>,
    pub busy: bool,
    pub should_quit: bool,
    pub dirty_screen: bool,
    pub spin: u64,
    system: Message,
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
        let (chat, stored_agents) = match stored {
            Some(session) => (session.messages, session.agents),
            None => (Vec::new(), Vec::new()),
        };
        let mut app = Self {
            ws,
            cfg,
            focus: Focus::Chat,
            chat,
            notices: Vec::new(),
            input: Input::default(),
            chat_scroll: 0,
            models,
            picker: None,
            git: None,
            agent_stats: HashMap::new(),
            context_used: 0,
            agents: vec![AgentNode {
                id: 0,
                parent: None,
                depth: 0,
                brief: "you (root agent)".to_string(),
                phase: Phase::Idle,
                since: Instant::now(),
                branch: None,
                summary: None,
                leftover: false,
                landed: None,
            }],
            agent_cursor: 0,
            focused: 0,
            agent_msgs: HashMap::new(),
            agent_tx: HashMap::from([(0, root.tx)]),
            agent_cancel: HashMap::new(),
            cfg_shared: root.cfg,
            agent_ids: root.ids.clone(),
            agent_live: root.live.clone(),
            ui_tx,
            conversation: root.conversation,
            git_at: None,
            status: None,
            busy: false,
            should_quit: false,
            dirty_screen: true,
            spin: 0,
            system,
        };
        app.restore_agents(stored_agents);
        app.discover_worktrees();
        app.count_context();
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
        let conversation = self.conversation;
        let ids = self.agent_ids.clone();
        let live = self.agent_live.clone();
        let root = self.ws.root().to_path_buf();
        for agent in stored {
            // Keep the counter above every restored id, or the next spawn hands
            // a live child an id a restored agent already holds (finding B1).
            self.agent_ids.fetch_max(agent.id + 1, Ordering::SeqCst);
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
            self.agent_msgs.insert(agent.id, agent.messages);
            self.agent_tx.insert(agent.id, tx);
            self.agents.push(AgentNode {
                id: agent.id,
                parent: agent.parent,
                depth: agent.depth.max(1),
                brief: agent.brief,
                phase,
                since: Instant::now(),
                branch: agent.branch,
                summary: agent.summary,
                leftover: agent.leftover,
                landed,
            });
        }
        self.repair_focus();
    }

    /// Re-read what the repository looks like: the main worktree's branch,
    /// dirty count and uncommitted delta, plus each isolated branch's own work.
    /// Called on events, never from `draw` — a `git` process per frame would be
    /// absurd (docs/mush.md §8).
    pub fn refresh_git(&mut self) {
        let root = self.ws.root();
        let mut stats = HashMap::new();
        for node in &self.agents {
            let Some(branch) = node.branch.clone() else {
                continue;
            };
            // A nested agent forked from its parent's branch, so that is what
            // its work is measured against; a top-level one forked from HEAD.
            let base = node
                .parent
                .and_then(|parent| self.agents.iter().find(|n| n.id == parent))
                .and_then(|parent| parent.branch.clone())
                .unwrap_or_else(|| "HEAD".to_string());
            if let Some(stat) = git::branch_stat(root, &base, &branch) {
                stats.insert(node.id, stat);
            }
        }
        self.agent_stats = stats;
        self.git = git::status(root);
        self.git_at = Some(Instant::now());
    }

    /// How many tokens the root conversation is holding, roughly (the same
    /// three-bytes-per-token heuristic the trimmer uses).
    fn count_context(&mut self) {
        self.context_used =
            self.chat.iter().map(Message::weight).sum::<usize>() + self.system.weight();
    }

    /// The window in tokens, for the meter.
    pub fn context_used_tokens(&self) -> usize {
        self.context_used / 3
    }

    /// Register git worktrees left over from earlier sessions (`mush/<id>`
    /// branches) as finished tree nodes, so `/diff`, `/merge`, `/discard` keep
    /// working after a restart.
    pub fn discover_worktrees(&mut self) {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.ws.root())
            .args(["worktree", "list", "--porcelain"])
            .output();
        let Ok(output) = output else { return };
        if !output.status.success() {
            return;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let root = self.ws.root().to_path_buf();
        // Drop stale leftovers whose worktree no longer exists.
        self.agents.retain(|node| {
            if !node.leftover {
                return true;
            }
            let Some(branch) = node.branch.as_deref() else {
                return false;
            };
            let Some(id) = branch
                .strip_prefix("mush/")
                .and_then(|s| s.parse::<u64>().ok())
            else {
                return false;
            };
            root.join(format!(".mush/wt/{id}")).exists()
        });
        for block in text.split("\n\n") {
            let Some(branch) = block
                .lines()
                .find_map(|line| line.strip_prefix("branch refs/heads/mush/"))
            else {
                continue;
            };
            let Some(id) = branch.parse::<u64>().ok() else {
                continue;
            };
            if self.agents.iter().any(|node| node.id == id) {
                continue;
            }
            let full = format!("mush/{id}");
            // Keep the tree's counter above every registered id, or the next
            // `spawn_agent` hands a live child an id a leftover already holds
            // (finding B1).
            self.agent_ids.fetch_max(id + 1, Ordering::SeqCst);
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
            self.agents.push(AgentNode {
                id,
                parent: None,
                depth: 1,
                brief,
                phase,
                since: Instant::now(),
                branch: Some(full),
                summary: Some(summary.to_string()),
                leftover: true,
                landed: None,
            });
        }
        self.repair_focus();
    }

    /// Reaping a leftover node leaves `focused` (and the cursor) pointing at a
    /// ghost: the pane would stay titled `agent #4` while typing reports that
    /// the agent is gone (finding B11).
    fn repair_focus(&mut self) {
        if !self.agents.iter().any(|node| node.id == self.focused) {
            self.focused = 0;
        }
        self.agent_cursor = self.agent_cursor.min(self.agents.len().saturating_sub(1));
    }

    // ---------------------------------------------------------------- updates

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Mouse(mouse) => {
                // One notch is several lines: a wheel event carries no repeat, unlike a
                // held arrow key, so the amount has to come from mush.
                const NOTCH: i64 = 3;
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll_chat(NOTCH),
                    MouseEventKind::ScrollDown => self.scroll_chat(-NOTCH),
                    _ => {}
                }
            }
            Msg::Paste(text) => {
                // A paste is something the human wants to say, so it lands in
                // the message box whichever pane has focus. An open picker is
                // the one place a paste has no meaning.
                if self.picker.is_none() {
                    // Terminals disagree about line endings in a paste.
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    self.input.insert(&text);
                }
            }
            Msg::Key(key) => self.on_key(key),
            Msg::Agent {
                conversation,
                id,
                event,
            } => {
                if conversation == self.conversation {
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
        if self.busy {
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
        self.age_stale_cancels();
    }

    /// A cancel is acknowledged quickly — the actor yields, or the run ends. A
    /// `⊘` older than this means the acknowledgement will never arrive (the
    /// actor's mailbox is dead), and a row that spins forever is worse than an
    /// idle one (finding B6).
    fn age_stale_cancels(&mut self) {
        const STALE_CANCEL: Duration = Duration::from_secs(10);
        let stale: Vec<u64> = self
            .agents
            .iter()
            .filter(|node| {
                matches!(node.phase, Phase::Cancelling) && node.since.elapsed() > STALE_CANCEL
            })
            .map(|node| node.id)
            .collect();
        if stale.is_empty() {
            return;
        }
        for id in stale {
            if let Some(node) = self.agent_node_mut(id) {
                node.phase = Phase::Idle;
                node.since = Instant::now();
            }
            self.agent_cancel.remove(&id);
        }
        self.recompute_busy();
        self.dirty_screen = true;
    }

    fn on_agent(&mut self, id: u64, event: AgentEvent) {
        match event {
            AgentEvent::Spawned {
                child,
                parent,
                brief,
                depth,
                branch,
                cmd,
            } => {
                self.agent_tx.insert(child, cmd);
                // The child's first user message is its brief — the model sees
                // it, so the transcript should too; otherwise a focused child
                // looks as if it started from nothing (finding B13).
                let opening = if brief.trim().is_empty() {
                    "Begin the task now.".to_string()
                } else {
                    brief.clone()
                };
                self.agent_msgs
                    .entry(child)
                    .or_default()
                    .push(Message::user(opening));
                self.agents.push(AgentNode {
                    id: child,
                    parent: Some(parent),
                    depth,
                    brief,
                    phase: Phase::Thinking,
                    since: Instant::now(),
                    branch,
                    summary: None,
                    leftover: false,
                    landed: None,
                });
                self.busy = true;
            }
            AgentEvent::Running { cancel } => {
                // A run started, possibly one the UI did not ask for (an idle
                // agent woken by a child's result). Mark it so `busy`, the
                // spinner, and Ctrl-C agree with the actor. The last run's
                // summary belongs to that run, not this one (finding B14).
                if let Some(node) = self.agent_node_mut(id) {
                    node.phase = Phase::Thinking;
                    node.since = Instant::now();
                    node.summary = None;
                }
                // Keep the run's flag: a Stop must be able to reach a model
                // call that is still waiting, not just the actor's mailbox.
                self.agent_cancel.insert(id, cancel);
                self.recompute_busy();
            }
            AgentEvent::Status(status) => {
                // A status that arrives after the run's own end (a late or
                // duplicated commit line) must not put a finished agent back to
                // work (finding B5).
                let at_rest = self
                    .agents
                    .iter()
                    .find(|node| node.id == id)
                    .map(|node| !node.phase.is_busy())
                    .unwrap_or(true);
                if !at_rest {
                    if let Some(node) = self.agent_node_mut(id) {
                        node.phase = Phase::Activity(status);
                        node.since = Instant::now();
                    }
                }
                // A status can arrive after the run's work is done (a commit
                // message, say) and before `Done`: keep `busy` in step with the
                // phases it is derived from.
                self.recompute_busy();
            }
            AgentEvent::Notice(text) => {
                // A limit the run reached (it still produced a result), or a
                // reply that was empty: a line in the transcript, tagged with
                // the agent it concerns (finding B19).
                self.note_for(id, text);
                self.chat_scroll = 0;
            }
            AgentEvent::Message(message) => {
                if id == 0 {
                    self.chat.push(message);
                    self.count_context();
                    self.save_session();
                } else if let Some(msgs) = self.agent_msgs.get_mut(&id) {
                    msgs.push(message);
                }
                self.chat_scroll = 0;
            }
            AgentEvent::Stopped => {
                // Stopped is not failed and not done: the run produced nothing,
                // and the actor is idle and resumable. Saying which one it is
                // is the difference between a lost agent and a parked one.
                if let Some(node) = self.agent_node_mut(id) {
                    node.phase = Phase::Stopped;
                    node.since = Instant::now();
                }
                self.agent_cancel.remove(&id);
                self.refresh_git();
                // Only the agent the human is looking at needs the bar; a
                // stop they did not ask for still shows as ⊘ on its row.
                if id == self.focused {
                    self.say(format!("agent #{id} stopped — send a message to resume it"));
                }
                self.chat_scroll = 0;
                self.recompute_busy();
            }
            AgentEvent::Error(error) => {
                if let Some(node) = self.agent_node_mut(id) {
                    node.phase = Phase::Failed(error.clone());
                    node.since = Instant::now();
                }
                self.agent_cancel.remove(&id);
                self.refresh_git();
                self.note_error_for(id, error);
                self.chat_scroll = 0;
                self.recompute_busy();
            }
            AgentEvent::Done => {
                let summary = self.last_assistant_text(id);
                if let Some(node) = self.agent_node_mut(id) {
                    node.phase = Phase::Done;
                    node.since = Instant::now();
                    // Every Done replaces the row's summary; keeping the first
                    // one described a run that ended long ago (finding B14).
                    node.summary = summary;
                }
                self.agent_cancel.remove(&id);
                self.refresh_git();
                self.recompute_busy();
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
                if id == 0 {
                    self.chat = vec![carried];
                    self.count_context();
                    self.save_session();
                    self.note("context compacted — continuing from a summary");
                } else if let Some(msgs) = self.agent_msgs.get_mut(&id) {
                    *msgs = vec![carried];
                }
                self.chat_scroll = 0;
            }
        }
    }

    /// An agent stopped working: we are busy while any agent in the tree is
    /// running. Derived from the phases, so it cannot disagree with the rows.
    fn recompute_busy(&mut self) {
        self.busy = self.agents.iter().any(|node| node.phase.is_busy());
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

    /// A line for the transcript that is not a message: a hint, or a failure.
    /// It concerns the root conversation unless tagged otherwise.
    pub fn note(&mut self, text: impl Into<String>) {
        self.note_for(0, text);
    }

    pub fn note_for(&mut self, agent: u64, text: impl Into<String>) {
        self.notices.push(Notice {
            agent,
            kind: NoticeKind::Info,
            text: text.into(),
        });
    }

    pub fn note_error(&mut self, text: impl Into<String>) {
        self.note_error_for(0, text);
    }

    pub fn note_error_for(&mut self, agent: u64, text: impl Into<String>) {
        self.notices.push(Notice {
            agent,
            kind: NoticeKind::Error,
            text: text.into(),
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
            .agents
            .iter()
            .filter(|node| node.phase.is_busy())
            .collect();
        if busy.is_empty() {
            return None;
        }
        // The root napped and its children still work: what matters is that the
        // root will come back, not which child is typing.
        if !busy.iter().any(|node| node.id == 0) {
            return Some(format!(
                "waiting on {} subagent(s) — the root resumes as they finish",
                busy.len()
            ));
        }
        let shown = busy
            .iter()
            .find(|node| node.id == self.focused)
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
    fn last_assistant_text(&self, id: u64) -> Option<String> {
        let messages: &[Message] = if id == 0 {
            &self.chat
        } else {
            self.agent_msgs.get(&id).map(Vec::as_slice)?
        };
        messages
            .iter()
            .rev()
            .find(|message| message.role == "assistant")
            .map(|message| message.text().trim().to_string())
            .filter(|text| !text.is_empty())
    }

    fn agent_node_mut(&mut self, id: u64) -> Option<&mut AgentNode> {
        self.agents.iter_mut().find(|node| node.id == id)
    }

    // ------------------------------------------------------------- chat / LLM

    fn send_message(&mut self) {
        let text = self.input.take().trim().to_string();
        if text.is_empty() {
            return;
        }
        if text.starts_with('/') {
            self.run_command(&text);
            return;
        }
        let target = self.focused;
        if target == 0 {
            // The human's words belong in the transcript they can see, whether
            // the root is starting a run or already in one.
            self.chat.push(Message::user(text.clone()));
            // The meter counts the root's conversation, the human's own words
            // included (finding B8).
            self.count_context();
            self.chat_scroll = 0;
            self.save_session();
            // The root's own phase, not the tree's: a napping orchestrator is
            // idle, and its next message starts a run rather than nudging a
            // conversation that is not in flight.
            let root_busy = self
                .agents
                .iter()
                .any(|node| node.id == 0 && node.phase.is_busy());
            if root_busy {
                // Human steering while the root runs: queued as a nudge. If the
                // root's mailbox is dead (it was cancelled), fall through and
                // start a fresh run instead of spinning forever on a ghost.
                let alive = self
                    .agent_tx
                    .get(&0)
                    .map(|tx| tx.send(AgentMsg::Nudge(text.clone())).is_ok())
                    .unwrap_or(false);
                if alive {
                    self.say("noted — folded in as the agent continues");
                    return;
                }
                if let Some(node) = self.agent_node_mut(0) {
                    node.phase = Phase::Idle;
                }
            }
            let mut messages = Vec::with_capacity(self.chat.len() + 1);
            messages.push(self.system.clone());
            messages.extend(self.chat.iter().cloned());
            match self.agent_tx.get(&0) {
                Some(tx) if tx.send(AgentMsg::Run(messages)).is_ok() => {
                    // The run starts now as far as the human is concerned; the
                    // actor's `Running` event will agree with this.
                    if let Some(node) = self.agent_node_mut(0) {
                        node.phase = Phase::Thinking;
                        node.since = Instant::now();
                    }
                    self.recompute_busy();
                }
                _ => {
                    self.fail("root agent is gone — /new restarts it");
                }
            }
        } else {
            // Nudge a specific agent; running ones fold it in, idle ones rerun.
            // If the mailbox is gone the node's phase is put back exactly as it
            // was, instead of leaving a lie on the row (finding B10).
            let previous = self.agent_node_mut(target).map(|node| node.phase.clone());
            if let Some(msgs) = self.agent_msgs.get_mut(&target) {
                msgs.push(Message::user(text.clone()));
            }
            if let Some(node) = self.agent_node_mut(target) {
                node.phase = Phase::Thinking;
                node.since = Instant::now();
            }
            match self.agent_tx.get(&target) {
                Some(tx) if tx.send(AgentMsg::Nudge(text)).is_ok() => {}
                _ => {
                    if let (Some(node), Some(previous)) = (self.agent_node_mut(target), previous) {
                        node.phase = previous;
                    }
                    self.fail(format!("agent #{target} is gone"));
                }
            }
            self.recompute_busy();
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
                self.note(
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
                self.forget_agent(id);
            }
            "/worktrees" => {
                self.discover_worktrees();
                self.refresh_git();
                let count = self.agents.iter().filter(|n| n.leftover).count();
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
            other => self.note_error(format!("unknown command: {other}")),
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
        let Ok(id) = rest.trim().parse::<u64>() else {
            self.say(format!("usage: {command} <agent id>"));
            return;
        };
        let (branch, busy, landed) = match self.agents.iter().find(|node| node.id == id) {
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
            self.note(text);
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
        let worktree = format!(".mush/wt/{id}");
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
                    if let Some(node) = self.agent_node_mut(id) {
                        node.landed = Some(Landed::Merged);
                    }
                    self.note(format!("merged {branch} into HEAD · {branch_note}"));
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
                    if let Some(node) = self.agent_node_mut(id) {
                        node.landed = Some(Landed::Discarded);
                    }
                    self.note(format!(
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
    fn forget_agent(&mut self, id: u64) {
        if id == 0 {
            self.fail("the root agent cannot be forgotten — /new restarts it");
            return;
        }
        let Some(node) = self.agents.iter().find(|node| node.id == id) else {
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
        self.agents.retain(|node| node.id != id);
        self.agent_msgs.remove(&id);
        // Dropping the last sender ends the actor: an idle agent whose mailbox
        // is gone has nothing left to wait for.
        self.agent_tx.remove(&id);
        self.agent_cancel.remove(&id);
        self.agent_stats.remove(&id);
        self.repair_focus();
        self.recompute_busy();
        match unmerged {
            Some(branch) => self.note(format!(
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
        // counter; adopt all three, or a later /model would never reach the
        // agent, its events would look stale, and a spawn could reuse an id.
        self.cfg_shared = root.cfg.clone();
        self.conversation = root.conversation;
        self.agent_ids = root.ids.clone();
        self.agent_tx = HashMap::from([(0, root.tx)]);
        self.agent_cancel.clear();
        self.agent_msgs.clear();
        // Running agents vanish with the old conversation; worktrees they left
        // behind are still reviewable (they are re-listed below).
        self.agents = vec![AgentNode {
            id: 0,
            parent: None,
            depth: 0,
            brief: "you (root agent)".to_string(),
            phase: Phase::Idle,
            since: Instant::now(),
            branch: None,
            summary: None,
            leftover: false,
            landed: None,
        }];
        self.agent_cursor = 0;
        self.chat.clear();
        self.notices.clear();
        self.chat_scroll = 0;
        self.focused = 0;
        self.busy = false;
        self.spin = 0;
        self.discover_worktrees();
        self.count_context();
        self.refresh_git();
        self.save_session();
        self.say("new chat — agents stopped, root restarted");
    }

    /// Ask every actor in the tree to shut down. `Shutdown`, not `Stop`: a
    /// cancelled actor goes back to waiting for work (which is what Ctrl-C
    /// should do), while `/new` needs the threads to be gone — and an actor
    /// holds its own mailbox open, so it never notices that the UI let go.
    fn stop_all(&self) {
        for tx in self.agent_tx.values() {
            let _ = tx.send(AgentMsg::Shutdown);
        }
    }

    fn save_session(&mut self) {
        // Every subagent, not just the root: without this a relaunch forgot
        // each child's context, and "continue that agent" meant writing the
        // brief again from scratch.
        let agents = self
            .agents
            .iter()
            .filter(|node| node.id != 0)
            .map(|node| session::AgentSession {
                id: node.id,
                parent: node.parent,
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
                    .agent_msgs
                    .get(&node.id)
                    .map(|messages| {
                        messages
                            .iter()
                            .filter(|message| message.role != "system")
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default(),
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
            messages: self.chat.clone(),
            agents,
        };
        if let Err(error) = session.save(self.ws.root()) {
            self.fail(format!("could not save session: {error}"));
        }
    }

    pub fn scroll_chat(&mut self, delta: i64) {
        let next = self.chat_scroll as i64 + delta;
        self.chat_scroll = next.max(0) as usize;
    }

    // ------------------------------------------------------------------ input

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

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
            Focus::Chat => self.key_chat(key, ctrl, alt),
        }
    }

    fn request_quit(&mut self) {
        self.should_quit = true;
    }

    /// Ask one agent's current run to stop: flip the flag its in-flight model
    /// call polls, and leave a Stop in the mailbox for everything else (a
    /// parked wait, a shell command, the next message boundary).
    fn stop_agent(&mut self, id: u64) -> bool {
        if let Some(flag) = self.agent_cancel.get(&id) {
            flag.store(true, Ordering::SeqCst);
        }
        self.agent_tx
            .get(&id)
            .map(|tx| tx.send(AgentMsg::Stop).is_ok())
            .unwrap_or(false)
    }

    /// Stop the agent the human is looking at. Ctrl-C used to stop *every*
    /// busy agent at once, which is the wrong default: the agents it killed
    /// were usually the ones already finished and about to report, and their
    /// work was lost with them. Stopping one agent is what the key should do;
    /// stopping the whole tree is `interrupt_all`.
    fn interrupt(&mut self) {
        // What the human is looking at: the focused agent if it is busy, else
        // the one agent that is busy (there is nothing to disambiguate).
        let busy: Vec<u64> = self
            .agents
            .iter()
            .filter(|node| node.phase.is_busy())
            .map(|node| node.id)
            .collect();
        let target = if busy.contains(&self.focused) {
            Some(self.focused)
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
        let targets: Vec<u64> = self
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
    fn stop_one(&mut self, id: u64) {
        let alive = self.stop_agent(id);
        if let Some(node) = self.agent_node_mut(id) {
            // Say so immediately: the actor may be mid-request, and a row that
            // keeps spinning looks like the Stop was never heard.
            node.phase = if alive {
                Phase::Cancelling
            } else {
                Phase::Stopped
            };
            node.since = Instant::now();
        }
        self.recompute_busy();
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
            KeyCode::Char('j') | KeyCode::Down => {
                if self.agent_cursor + 1 < self.agents.len() {
                    self.agent_cursor += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.agent_cursor = self.agent_cursor.saturating_sub(1);
            }
            KeyCode::Char('g') | KeyCode::Home => self.agent_cursor = 0,
            KeyCode::Char('G') | KeyCode::End => {
                self.agent_cursor = self.agents.len().saturating_sub(1)
            }
            KeyCode::Enter => {
                if let Some(node) = self.agents.get(self.agent_cursor) {
                    self.focused = node.id;
                    self.say(format!("agent #{}: {}", node.id, node.brief));
                }
            }
            KeyCode::Char('c') => {
                if let Some(node) = self.agents.get(self.agent_cursor) {
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
                self.focused = 0;
            }
            _ => {}
        }
    }

    /// The message box: a plain text field with a real cursor, so a long
    /// prompt can be edited instead of backspaced away.
    fn key_chat(&mut self, key: KeyEvent, ctrl: bool, alt: bool) {
        match key.code {
            // A new line instead of sending. Only terminals that report the
            // modifier can deliver Shift+Enter (kitty, WezTerm, foot, Ghostty,
            // recent Alacritty); elsewhere it arrives as a plain Enter, which is
            // why Alt+Enter does the same thing and is the reliable one.
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) || alt => {
                self.input.insert("\n")
            }
            KeyCode::Enter => self.send_message(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete_forward(),
            KeyCode::Left => self.input.move_left(),
            KeyCode::Right => self.input.move_right(),
            KeyCode::Home => self.input.move_home(),
            KeyCode::End => self.input.move_end(),
            KeyCode::Char(c) if !ctrl && !alt => self.input.insert(&c.to_string()),
            KeyCode::Up => self.scroll_chat(1),
            KeyCode::Down => self.scroll_chat(-1),
            KeyCode::PageUp => self.scroll_chat(10),
            KeyCode::PageDown => self.scroll_chat(-10),
            KeyCode::Esc => self.input.clear(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let worktree = root.join(format!(".mush/wt/{id}"));
        std::fs::create_dir_all(root.join(".mush")).unwrap();
        git(
            root,
            &[
                "worktree",
                "add",
                "-q",
                worktree.to_str().unwrap(),
                "-b",
                &format!("mush/{id}"),
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
            .agents
            .iter()
            .find(|node| node.id == 3)
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
        let node = app.agents.iter().find(|node| node.id == 5).unwrap();
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
        let node = app.agents.iter().find(|node| node.id == 1).unwrap();
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
        let node = app.agents.iter().find(|node| node.id == 2).unwrap();
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
        app.agents.iter_mut().find(|n| n.id == 4).unwrap().phase = Phase::Thinking;

        app.worktree_command("/merge", "4");
        app.worktree_command("/discard", "4");

        assert!(root.join(".mush/wt/4").exists(), "nothing was reclaimed");
        let node = app.agents.iter().find(|node| node.id == 4).unwrap();
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
        app.agent_msgs.insert(6, vec![Message::user("hello")]);

        app.forget_agent(6);

        assert!(app.agents.iter().all(|node| node.id != 6));
        assert!(!app.agent_msgs.contains_key(&6));
        assert!(
            root.join(".mush/wt/6").exists(),
            "forgetting is not discarding"
        );
        assert_eq!(app.focused, 0, "focus cannot point at a ghost");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The root is not forgettable, and a running agent has to be stopped first.
    #[test]
    fn forgetting_refuses_the_root_and_a_running_agent() {
        let (mut app, _rx) = test_app("forget-guards");
        app.forget_agent(0);
        assert!(app.agents.iter().any(|node| node.id == 0));

        app.agents.push(AgentNode {
            id: 9,
            parent: Some(0),
            depth: 1,
            brief: "busy".to_string(),
            phase: Phase::Thinking,
            since: Instant::now(),
            branch: None,
            summary: None,
            leftover: false,
            landed: None,
        });
        app.forget_agent(9);
        assert!(
            app.agents.iter().any(|node| node.id == 9),
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
            .agents
            .iter()
            .find(|node| node.id == 2)
            .expect("the stored agent comes back");
        assert_eq!(node.brief, "port the parser");
        assert_eq!(node.phase, Phase::Done);
        assert_eq!(node.summary.as_deref(), Some("finished it"));
        // The transcript is what makes a follow-up possible: without it the
        // human is back to writing the brief from scratch.
        assert_eq!(app.agent_msgs[&2].len(), 2);
        assert_eq!(app.agent_msgs[&2][1].text(), "done");
        // A live mailbox: a follow-up is delivered rather than dropped, which is
        // what "revive" has to mean to be worth anything.
        let tx_to_child = app
            .agent_tx
            .get(&2)
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
        assert_eq!(app.input.text(), "line one\nline two\n");
        assert!(app.chat.is_empty(), "a paste is not a send");
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
        assert_eq!(app.input.text(), "\n\n");
        assert!(app.chat.is_empty(), "a modified Enter must not send");
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
        app.chat.push(Message::user("an old task"));
        app.notices.push(Notice {
            agent: 0,
            kind: NoticeKind::Info,
            text: "old noise".to_string(),
        });
        app.busy = true;
        let before = app.cfg_shared.clone();

        app.run_command("/new");

        assert!(app.chat.is_empty(), "the conversation is gone");
        assert!(app.notices.is_empty(), "notices are gone");
        assert_eq!(app.agents.len(), 1, "the tree is reset to the root");
        assert!(!app.busy);
        // The respawned root owns a fresh config cell and the UI adopted it;
        // without that, a later /model would never reach the agent.
        assert!(!Arc::ptr_eq(&before, &app.cfg_shared));

        // Its mailbox is alive, so the next message starts a run instead of
        // reporting that the root is gone.
        app.input.insert("hello");
        app.send_message();
        assert!(app.busy);
        assert_eq!(app.agents[0].phase, Phase::Thinking);
    }

    /// An actor `/new` abandoned can still be finishing a request (up to the
    /// HTTP timeout); its events must not land in the new conversation. Ids
    /// collide by design — the new root is #0 too.
    #[test]
    fn events_from_an_abandoned_conversation_are_ignored() {
        let (mut app, _rx) = test_app("stale");
        let abandoned = app.conversation;
        app.run_command("/new");
        assert_ne!(app.conversation, abandoned, "a new conversation tag");
        app.chat.push(Message::user("current work"));

        app.update(Msg::Agent {
            conversation: abandoned,
            id: 0,
            event: AgentEvent::Message(Message::assistant("stale reply")),
        });
        app.update(Msg::Agent {
            conversation: abandoned,
            id: 0,
            event: AgentEvent::Compact {
                summary: "stale summary".to_string(),
            },
        });
        assert_eq!(app.chat.len(), 1, "the stale reply and summary are dropped");

        app.update(Msg::Agent {
            conversation: app.conversation,
            id: 0,
            event: AgentEvent::Message(Message::assistant("fresh reply")),
        });
        assert_eq!(app.chat.len(), 2, "the live conversation still lands");
    }

    /// A steering message sent while the root is busy is folded into the run,
    /// and it must also be visible: the human has to see what they said.
    #[test]
    fn steering_text_is_echoed_in_the_chat() {
        let (mut app, _rx) = test_app("steer");
        app.busy = true;
        app.agents[0].phase = Phase::Thinking;
        app.input.insert("also rename the module");

        app.send_message();

        assert_eq!(app.chat.len(), 1, "the steering message is echoed");
        assert_eq!(app.chat[0].text(), "also rename the module");
        assert_eq!(text_of(&app), "noted — folded in as the agent continues");
    }

    /// The meter counts the human's own words as soon as they are sent, not
    /// only once a reply arrives (finding B8).
    #[test]
    fn the_context_meter_counts_the_human_message() {
        let (mut app, _rx) = test_app("meter");
        let before = app.context_used_tokens();
        app.input
            .insert("a question long enough to weigh something");
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
        app.agent_tx.remove(&1);
        app.agents.push(AgentNode {
            id: 1,
            parent: Some(0),
            depth: 1,
            brief: "lexer".to_string(),
            phase: Phase::Done,
            since: Instant::now(),
            branch: None,
            summary: Some("did the work".to_string()),
            leftover: false,
            landed: None,
        });
        app.focused = 1;
        app.input.insert("one more thing");

        app.send_message();

        assert_eq!(app.agents[1].phase, Phase::Done, "the ✓ is not rewritten");
        assert_eq!(text_of(&app), "agent #1 is gone");
    }

    /// A tree `/new` abandoned can still spawn children, and their `Spawned`
    /// events are dropped — so this is the only moment the UI can tell such a
    /// child to go away. Without it the child runs unseen forever.
    #[test]
    fn a_child_spawned_by_an_abandoned_tree_is_shut_down() {
        let (mut app, _rx) = test_app("stale-child");
        let abandoned = app.conversation;
        app.run_command("/new");
        let (child_tx, child_rx) = crossbeam_channel::unbounded::<AgentMsg>();

        app.update(Msg::Agent {
            conversation: abandoned,
            id: 1,
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
        assert_eq!(app.agents.len(), 1, "and it never joins the new tree");
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

        app.input.insert("hello");
        app.send_message();
        assert_eq!(
            app.agents[0].phase,
            Phase::Thinking,
            "the idle root is usable"
        );

        let (mut app, _rx) = test_app("interrupt-running");
        app.agents[0].phase = Phase::Thinking;
        app.recompute_busy();
        app.interrupt();
        assert_eq!(
            app.agents[0].phase,
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
        let conversation = app.conversation;
        app.agents[0].phase = Phase::Thinking;
        app.interrupt();
        assert_eq!(app.agents[0].phase, Phase::Cancelling);

        app.update(Msg::Agent {
            conversation,
            id: 0,
            event: AgentEvent::Stopped,
        });
        assert_eq!(
            app.agents[0].phase,
            Phase::Stopped,
            "a stopped run is its own state, not Idle and not Done"
        );

        app.agents[0].phase = Phase::Thinking;
        app.interrupt();
        app.update(Msg::Agent {
            conversation,
            id: 0,
            event: AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        });
        assert_eq!(app.agents[0].phase, Phase::Thinking, "a new run is running");
    }

    /// An agent that has not run is not done, and it is not busy: the row must
    /// say so rather than claim a `✓` it never earned.
    #[test]
    fn an_idle_agent_is_neither_done_nor_busy() {
        let (app, _rx) = test_app("idle-phase");
        assert_eq!(app.agents[0].phase, Phase::Idle);
        assert!(!app.busy);
        assert_eq!(app.activity_line(), None, "nothing to report");
    }

    /// The stale-status defect: a finished run must leave nothing behind. The
    /// bar derives from the phases, so when the run ends the line is gone.
    #[test]
    fn a_finished_run_leaves_nothing_behind() {
        let (mut app, _rx) = test_app("finished-phase");
        let conversation = app.conversation;
        app.agents[0].phase = Phase::Thinking;
        app.agents[0].since = Instant::now() - Duration::from_secs(70);
        app.recompute_busy();
        assert_eq!(app.activity_line().as_deref(), Some("#0 thinking 1m10s"));

        app.update(Msg::Agent {
            conversation,
            id: 0,
            event: AgentEvent::Done,
        });
        assert_eq!(app.agents[0].phase, Phase::Done);
        assert!(!app.busy);
        assert_eq!(app.activity_line(), None, "no `thinking` survives the run");
    }

    /// A napping root with live children is derived from the tree: nobody has
    /// to remember to write it, so it cannot be forgotten either.
    #[test]
    fn a_napping_root_reports_its_children() {
        let (mut app, _rx) = test_app("napping-root");
        let conversation = app.conversation;
        app.update(Msg::Agent {
            conversation,
            id: 1,
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
            id: 1,
            event: AgentEvent::Status("edit_file src/lex.rs".to_string()),
        });
        assert_eq!(app.agents[0].phase, Phase::Idle, "the root napped");
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
        app.agents[0].phase = Phase::Activity("edit_file src/lib.rs".to_string());
        app.agents[0].since = Instant::now() - Duration::from_secs(12);
        app.recompute_busy();
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
        let conversation = app.conversation;
        for (id, parent, depth) in [(1u64, 0u64, 1usize), (2, 1, 2)] {
            app.update(Msg::Agent {
                conversation,
                id: parent,
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
        app.agents[1].phase = Phase::Activity("edit_file src/lexer.rs 12s".to_string());
        app.agents[2].phase = Phase::Failed("no route to host".to_string());
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
        let conversation = app.conversation;
        app.update(Msg::Agent {
            conversation,
            id: 0,
            event: AgentEvent::Done,
        });
        assert_eq!(app.agents[0].phase, Phase::Done);
        app.update(Msg::Agent {
            conversation,
            id: 0,
            event: AgentEvent::Status("committed abc123 on mush/1".to_string()),
        });
        assert_eq!(app.agents[0].phase, Phase::Done, "the ✓ must survive");
        assert!(!app.busy);
    }

    /// A `⊘` whose acknowledgement can never arrive goes quiet instead of
    /// spinning forever (finding B6).
    #[test]
    fn a_stale_cancel_falls_back_to_idle() {
        let (mut app, _rx) = test_app("stale-cancel");
        app.agents[0].phase = Phase::Cancelling;
        app.agents[0].since = Instant::now() - Duration::from_secs(11);
        app.busy = true;
        app.tick();
        assert_eq!(app.agents[0].phase, Phase::Idle);
        assert!(!app.busy, "the bar must stop claiming work");
    }

    /// The child's brief is the first thing its transcript shows, exactly as
    /// the model received it (finding B13).
    #[test]
    fn a_childs_brief_is_the_start_of_its_transcript() {
        let (mut app, _rx) = test_app("brief");
        let conversation = app.conversation;
        app.update(Msg::Agent {
            conversation,
            id: 0,
            event: AgentEvent::Spawned {
                child: 1,
                parent: 0,
                brief: "count the lexer tokens".to_string(),
                depth: 1,
                branch: None,
                cmd: crossbeam_channel::unbounded().0,
            },
        });
        let messages = &app.agent_msgs[&1];
        assert_eq!(messages.len(), 1, "the brief opens the transcript");
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].text(), "count the lexer tokens");
    }

    /// Every Done replaces the row's summary, and a new run clears it: the row
    /// describes the current run, not the first one forever (finding B14).
    #[test]
    fn a_rows_summary_follows_the_latest_run() {
        let (mut app, _rx) = test_app("summary");
        let conversation = app.conversation;
        app.chat.push(Message::assistant("first result"));
        app.update(Msg::Agent {
            conversation,
            id: 0,
            event: AgentEvent::Done,
        });
        assert_eq!(app.agents[0].summary.as_deref(), Some("first result"));
        app.update(Msg::Agent {
            conversation,
            id: 0,
            event: AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        });
        assert_eq!(app.agents[0].summary, None, "a new run clears the old one");
        app.chat.push(Message::assistant("second result"));
        app.update(Msg::Agent {
            conversation,
            id: 0,
            event: AgentEvent::Done,
        });
        assert_eq!(app.agents[0].summary.as_deref(), Some("second result"));
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
            app.agents.iter().any(|node| node.id == 7),
            "the leftover is registered"
        );
        assert!(
            app.agent_ids.load(Ordering::SeqCst) >= 8,
            "the next spawn must not reuse #7"
        );

        app.focused = 7;
        git(&["worktree", "remove", "--force", ".mush/wt/7"]);
        app.discover_worktrees();
        assert!(
            !app.agents.iter().any(|node| node.id == 7),
            "the reaped leftover is gone"
        );
        assert_eq!(app.focused, 0, "focus cannot point at a ghost");
        let _ = fs::remove_dir_all(&root);
    }
}
