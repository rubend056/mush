//! Agent actors.
//!
//! Each agent is its own thread owning one transcript; parents and children
//! talk *directly* through mailboxes, while the UI observes everything through
//! id-tagged events. This is what makes parallel subagent chains possible:
//! an orchestrator can spawn N children, wait for whichever finishes first,
//! nudge or stop individual agents, and descendants can spawn their own.
//!
//! File access is direct disk I/O on the agent's own thread: the UI holds no
//! copy of any file, so there is nothing to round-trip and nothing to keep in
//! sync. An isolated agent works in its own git worktree.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use serde_json::{json, Value};

use mush_core::config::parse_context_hint;
use mush_core::git;
use mush_core::message::{ChatRequest, ChatResponse};
use mush_core::text::truncate;
use mush_core::tools::ToolName;
use mush_core::transcript::{
    needs_compaction, repair_tool_pairs, sanitize_tool_calls, trim_history, COMPACT_INSTRUCTION,
    COMPACT_REPLY_TOKENS,
};
use mush_core::{prompt, tools, Config, Message, Workspace, CMD_CAP, CMD_TIMEOUT_SECS};

use crate::app::{AgentId, ConversationId, Msg};
use crate::clock;
use crate::events::{Events, Ui};
use crate::machine::{Job, Machine, Shell, ShellCommand};
use crate::model::{HttpModel, ModelClient, ModelError};

/// Backstop against a model that never stops — *not* a budget for the work.
///
/// This used to be 24 and acted as a task budget, which turned honest long work
/// (read a 2,500-line file, edit it, run the gate, edit again) into a truncated
/// run: the agent was stopped mid-task for being thorough. A run is really
/// bounded by the endpoint's context window (compaction and `trim_history`) and
/// by `LOOP_ROUNDS` below, so this is only the last line of defence against a
/// model that answers forever. Set past any real task on purpose.
const RUNAWAY_TURNS: usize = 200;
/// Identical consecutive tool batches before the run is called a loop.
///
/// The honest reason to stop a run *early* is that it stopped making progress,
/// not that it took a certain number of turns. Repeating the same call with the
/// same arguments and nothing changed in between is that signal; a long task
/// that keeps changing something never trips it, however long it runs.
const LOOP_ROUNDS: usize = 5;
/// Ceiling on one model reply, in tokens. It has to cover a thinking model's
/// reasoning too: when the cap is spent before the visible answer, the reply
/// arrives cut off (`finish_reason: length`).
const MAX_REPLY_TOKENS: u32 = 20_480;

/// What one reply may use, given the endpoint's window. Asking for more than a
/// fraction of the window is how a reply arrives cut off: the endpoint cannot
/// deliver it, or spends the cap on reasoning and never reaches the answer.
/// A quarter of the window is the same share `Config::history_budget` reserves
/// for the reply, so the two cannot disagree about what "one reply" means.
fn reply_cap(cfg: &Config) -> u32 {
    let share = (cfg.context_tokens / 4) as u64;
    MAX_REPLY_TOKENS.min(share.max(1024) as u32)
}
/// Consecutive cut-off replies before the run gives up. A cut reply is usually
/// a *too big* answer — a whole file in one `write_file`, or a long reasoning
/// pass — not a broken model, so the run asks for smaller pieces and carries
/// on. It is bounded because a model that cannot write small enough is not
/// going to start now.
const TRUNCATION_ROUNDS: usize = 3;
/// How deep subagent chains may go (0 = root agent only).
pub const MAX_DEPTH: usize = 3;
/// Hard ceiling on simultaneously running agents across the whole tree.
const MAX_AGENTS: u64 = 16;
/// Default `wait_agents` timeout in seconds; 0 means forever.
const WAIT_TIMEOUT_SECS: u64 = 600;
/// Why a cancelled run ends. Internal to the actor: a run that ends with this
/// becomes `Outcome::Stopped` at the actor boundary, so no other layer has to
/// compare result text to know what happened.
const CANCELLED: &str = "cancelled";

/// How a run ended, as the actor reports it to its parent and to its own row.
///
/// Three outcomes, because they mean three different things: a finished run
/// produced a result, a failed run produced an error, and a *stopped* run
/// produced neither — the actor is still alive and a nudge resumes it. A bare
/// summary string could not tell them apart, so a stopped child was reported
/// through the same path as a finished one and read as `done`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The run finished; the string is its summary.
    Finished(String),
    /// The run was stopped (Ctrl-C, `agent_control stop`, `/new`). Not a
    /// result and not a failure: the actor is idle and resumable.
    Stopped,
    /// The run failed; the string is the error.
    Failed(String),
}

/// What a commit subject can say about the run that produced it.
///
/// A subject carries the *task*, not the result, so this is the projection of
/// [`Outcome`] onto what survives in git: how the run ended. It exists so a
/// worktree found on startup can be shown as the work it really is, instead of
/// an anonymous "leftover worktree".
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Committed {
    Finished,
    Stopped,
    Failed(String),
}

impl From<&Outcome> for Committed {
    fn from(outcome: &Outcome) -> Self {
        match outcome {
            Outcome::Finished(_) => Committed::Finished,
            Outcome::Stopped => Committed::Stopped,
            Outcome::Failed(error) => Committed::Failed(error.clone()),
        }
    }
}

/// The commit subject for an isolated agent's work.
///
/// The outcome is in the subject on purpose: an interrupted run commits its work
/// in progress too, and a log full of identically-formatted `mush #3: <brief>`
/// subjects cannot be told apart from finished work. [`parse_commit_subject`] is
/// the inverse, and the two are tested against each other.
pub fn commit_subject(id: u64, brief: &str, outcome: &Outcome) -> String {
    match Committed::from(outcome) {
        Committed::Finished => format!("mush #{id}: {}", truncate(brief, 60)),
        Committed::Stopped => format!(
            "mush #{id} (stopped, work in progress): {}",
            truncate(brief, 60)
        ),
        Committed::Failed(error) => format!(
            "mush #{id} (failed: {}): {}",
            truncate(&error, 40),
            truncate(brief, 60)
        ),
    }
}

/// Read back what [`commit_subject`] wrote: how the run ended, and the task it
/// was given. `None` for any subject mush did not write — a commit the human
/// made by hand on that branch is not evidence about an agent.
pub fn parse_commit_subject(subject: &str) -> Option<(Committed, String)> {
    let after = subject.strip_prefix("mush #")?;
    // The id is a run of digits. It must not be located by splitting on the
    // first space: the `:` sits *before* the space (`mush #7: brief`), so that
    // split would swallow the delimiter and every finished run would fail to
    // parse.
    let rest = after.trim_start_matches(|c: char| c.is_ascii_digit());
    if rest.len() == after.len() {
        return None;
    }
    // What follows is `: ` for a finished run, or ` (` for a stop or failure.
    if let Some(brief) = rest.strip_prefix(": ") {
        return Some((Committed::Finished, brief.to_string()));
    }
    let rest = rest.strip_prefix(" (")?;
    let (head, brief) = rest.split_once("): ")?;
    let ended = if head == "stopped, work in progress" {
        Committed::Stopped
    } else {
        // Any other shape must name a failure; if it does not, this subject is
        // not one mush wrote.
        Committed::Failed(head.strip_prefix("failed: ")?.to_string())
    };
    Some((ended, brief.to_string()))
}

impl Outcome {
    /// The line a parent reads. Each outcome names itself, so a stop can never
    /// be mistaken for a result.
    fn line(&self, id: u64) -> String {
        match self {
            Outcome::Finished(summary) => format!("#{id} done: {summary}"),
            Outcome::Stopped => format!(
                "#{id} stopped: the run ended before it finished — this agent is idle, \
                 not done; agent_control message resumes it"
            ),
            Outcome::Failed(error) => format!("#{id} failed: {error}"),
        }
    }

    /// Whether this is news worth waking a napping parent for. A stop is the
    /// human's doing, not news, so it waits in the transcript instead of
    /// paying for a fresh run.
    fn is_news(&self) -> bool {
        !matches!(self, Outcome::Stopped)
    }
}

/// Commands sent into an agent actor's mailbox.
pub enum AgentMsg {
    /// Adopt these messages and run. The actor keeps the transcript, so later
    /// nudges continue the same conversation.
    Run(Vec<Message>),
    /// Append a user message; if idle, run again.
    Nudge(String),
    /// Fold this agent's conversation into a summary now, instead of waiting
    /// for the window to fill (`/compact`). A summarize request and a
    /// transcript replacement, not work to answer: an idle agent does it at
    /// once and stays idle.
    Compact,
    /// Cancel the current run. An idle agent ignores it — Stop cancels work,
    /// it does not end an agent.
    Stop,
    /// End this actor for good (`/new`, Ctrl-N). A `Stop` cannot do this: an
    /// actor holds its own mailbox open, so it never learns that everyone else
    /// let go — it has to be told.
    Shutdown,
    /// A child's run ended. The outcome says *how*: a stop is not a result.
    ChildDone { id: u64, outcome: Outcome },
}

/// Events streamed to the UI thread, tagged with the emitting agent's id.
///
/// `Clone` so a test's recording sink can hand out what it saw (see
/// `crate::events::fake`): an event carries no state of its own — a mailbox or
/// a cancellation flag is a handle, not a copy.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// A child actor now exists (sent by its parent), with the handle to steer it.
    Spawned {
        child: u64,
        parent: u64,
        brief: String,
        depth: usize,
        branch: Option<String>,
        cmd: Sender<AgentMsg>,
    },
    /// A run began — including one the UI did not ask for, because an idle
    /// agent was woken by a child's result. Keeps `busy` and the tree honest.
    /// `cancel` is this run's flag, and the UI keeps a clone: it is the one the
    /// HTTP reader polls, so a Stop reaches a model call that has not answered.
    Running {
        cancel: Arc<AtomicBool>,
    },
    Status(String),
    /// A line for the transcript that is not a message and not a failure: a
    /// limit the run reached, say. Unlike `Status` it stays visible, and unlike
    /// `Error` it does not mark the run failed.
    Notice(String),
    Message(Message),
    Done,
    /// The run was stopped by a request (a Stop, Ctrl-C, `/new`). The actor is
    /// still alive, so the row goes quiet instead of claiming a failure.
    Stopped,
    Error(String),
    /// The window the endpoint itself named when it rejected a request; the UI
    /// adopts it so the bar, `/context`, and the tool caps agree with the agent
    /// (finding B7).
    Context {
        tokens: usize,
    },
    /// The transcript was folded into a summary (context compaction); the
    /// conversation is now `[system, user(summary)]`.
    Compact {
        summary: String,
    },
}

/// Shared by every actor: config, the UI channel, and the budgets.
pub struct AgentCtx {
    /// Shared so a runtime `/provider` / `/url` / `/model` / `/key` applies to
    /// every agent immediately.
    pub cfg: Arc<Mutex<Config>>,
    /// Where this agent's model calls go. Shared by the whole tree — a child
    /// gets its parent's client — so one endpoint serves every agent, and one
    /// scripted client can serve a whole tree in a test.
    pub model: Arc<dyn ModelClient>,
    /// Where this tree's events go: the UI thread's channel, or a recording
    /// sink in a test. Carried here rather than reached for directly, so an
    /// actor cannot quietly take a second path to the UI.
    pub events: Arc<dyn Events>,
    /// How a `run_command` is started and watched. The real one runs `sh` in
    /// its own process group; a test scripts the end state instead, so the
    /// timeout, the cancellation and the output cap need no subprocess.
    pub machine: Arc<dyn Machine>,
    /// The clock every wait is measured against. `wait_agents` and a running
    /// command are the two places mush spends real time, so both read it here:
    /// a test can reach a timeout or a deadline by advancing a fake instead of
    /// waiting for the real one.
    pub clock: Arc<dyn clock::Clock>,
    /// The main workspace root; agents whose root differs are isolated.
    pub root: PathBuf,
    pub ids: Arc<AtomicU64>,
    pub live: Arc<AtomicU64>,
}

impl AgentCtx {
    /// Report something that happened to agent `id`.
    fn emit(&self, id: u64, event: AgentEvent) {
        self.events.emit(AgentId(id), event);
    }
}

/// Per-actor state that survives across runs (children, summaries).
#[derive(Default)]
struct ActorState {
    children: HashMap<u64, Sender<AgentMsg>>,
    running: HashSet<u64>,
    completed: HashMap<u64, Outcome>,
    /// Completions already handed to the model (via wait_agents or delivery).
    delivered: HashSet<u64>,
    /// Commands parked while a blocking tool call was in flight; folded in at
    /// the next message boundary (see `drain_signals`).
    deferred: Vec<AgentMsg>,
    /// A `Compact` arrived: the conversation is to be folded into a summary
    /// now, rather than when the window fills. Honoured at the next message
    /// boundary mid-run (never between an assistant's tool calls and their
    /// results), and at once while idle.
    compact_requested: bool,
    /// A `Shutdown` arrived: stop the run and end this actor.
    shutdown: bool,
    /// A `Stop` arrived with the work this actor is about to start; the run it
    /// points at is born cancelled (finding B6).
    stop_requested: bool,
}

/// One agent: its identity, its workspace, and the mailboxes it is wired to.
/// Passed by reference through a run, which keeps the loop functions small.
struct Actor {
    ctx: Arc<AgentCtx>,
    id: u64,
    depth: usize,
    ws: Workspace,
    /// The isolated worktree branch this agent works on, if any; children
    /// branch from it so nested work is not lost.
    branch: Option<String>,
    /// The task this agent was given (empty for the root). Used as the commit
    /// subject when an isolated agent's run ends.
    brief: String,
    /// Its own mailbox — where its children report their completions.
    my_tx: Sender<AgentMsg>,
    /// Where it reports its own completion to its parent. Dead for the root,
    /// which has no parent.
    parent_tx: Sender<AgentMsg>,
    rx: Receiver<AgentMsg>,
}

/// The UI's handle on the root actor of one conversation.
pub struct RootHandle {
    /// The root's mailbox.
    pub tx: Sender<AgentMsg>,
    /// The config cell every actor in this tree reads, so a runtime `/model`
    /// reaches them all.
    pub cfg: Arc<Mutex<Config>>,
    /// Identifies this conversation in events; see `agent::next_conversation`.
    pub conversation: u64,
    /// The tree's id counter, so the UI can raise its floor to the highest id
    /// a leftover worktree already occupies (finding B1).
    pub ids: Arc<AtomicU64>,
    /// The tree-wide count of running agents, shared so a revived agent is
    /// counted against `MAX_AGENTS` like any other.
    pub live: Arc<AtomicU64>,
}

/// Start the root actor.
pub fn spawn(cfg: Config, tx: Sender<Msg>, root: PathBuf) -> RootHandle {
    let shared = Arc::new(Mutex::new(cfg));
    // The real endpoint, behind the seam: every agent in this tree calls it
    // through `AgentCtx::model`, children included.
    let model: Arc<dyn ModelClient> = Arc::new(HttpModel::new(shared.clone()));
    let conversation = next_conversation();
    let ui: Arc<dyn Events> = Arc::new(Ui::new(tx, conversation));
    root_actor(shared, model, ui, conversation, root)
}

/// The same tree, with its model calls served by the caller instead of the
/// real endpoint, and its events recorded instead of shown.
///
/// Children inherit the client and the sink through the cloned context, so one
/// scripted model serves a whole tree — a test can drive a parent, its children
/// and its grandchildren through one script, with no socket, no server and no
/// sleep — and one recorder sees every event the whole tree emits.
#[cfg(test)]
pub(crate) fn spawn_scripted(
    cfg: Config,
    events: Arc<dyn Events>,
    root: PathBuf,
    model: Arc<dyn ModelClient>,
) -> RootHandle {
    let conversation = next_conversation();
    root_actor(Arc::new(Mutex::new(cfg)), model, events, conversation, root)
}

/// One conversation per `/new`, so stale events can be told apart: an actor
/// left over from a replaced tree can still be finishing a request, and its
/// events must not land in the new chat.
fn next_conversation() -> ConversationId {
    static CONVERSATIONS: AtomicU64 = AtomicU64::new(1);
    ConversationId(CONVERSATIONS.fetch_add(1, Ordering::SeqCst))
}

/// Start the root actor of one conversation over a given model and sink.
fn root_actor(
    shared: Arc<Mutex<Config>>,
    model: Arc<dyn ModelClient>,
    events: Arc<dyn Events>,
    conversation: ConversationId,
    root: PathBuf,
) -> RootHandle {
    // Root agent is id 0; children start at 1. The UI holds a clone so it can
    // raise the floor above leftover worktree ids.
    let ids = Arc::new(AtomicU64::new(1));
    let live = Arc::new(AtomicU64::new(0));
    let ctx = Arc::new(AgentCtx {
        cfg: shared.clone(),
        model,
        events,
        machine: Arc::new(Shell),
        clock: Arc::new(clock::System),
        root,
        ids: ids.clone(),
        live: live.clone(),
    });
    let ws = Workspace::new(&ctx.root).expect("workspace root must exist");
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<AgentMsg>();
    // The root has no parent. Its children report into its own mailbox; its
    // own completion goes to a dead channel so it can never wake itself up.
    let (dead_tx, dead_rx) = crossbeam_channel::unbounded::<AgentMsg>();
    drop(dead_rx);
    let actor = Actor {
        ctx,
        id: 0,
        depth: 0,
        ws,
        branch: None,
        brief: String::new(),
        my_tx: cmd_tx.clone(),
        parent_tx: dead_tx,
        rx: cmd_rx,
    };
    start(actor, Vec::new(), false);
    RootHandle {
        tx: cmd_tx,
        cfg: shared,
        conversation: conversation.0,
        ids: ids.clone(),
        live,
    }
}

/// What a revived agent needs to be re-adopted.
pub struct ReviveSpec {
    pub id: u64,
    /// The depth it had: it decides the system prompt, and whether this agent
    /// may spawn children of its own.
    pub depth: usize,
    pub brief: String,
    pub branch: Option<String>,
    /// The messages it had, *without* the system prompt (which is regenerated:
    /// it names a workspace that may have moved).
    pub messages: Vec<Message>,
}

/// Bring back an agent whose actor is gone — one restored from a stored session,
/// or a worktree found on disk — seeded with the transcript it had, and run it.
///
/// The human owns this agent, not the root: its completion goes to a dead
/// channel, so reviving a child never wakes the root with news it did not ask
/// for.
pub fn revive(
    cfg: Arc<Mutex<Config>>,
    tx: Sender<Msg>,
    conversation: u64,
    ids: Arc<AtomicU64>,
    live: Arc<AtomicU64>,
    root: PathBuf,
    spec: ReviveSpec,
) -> Sender<AgentMsg> {
    let ReviveSpec {
        id,
        depth,
        brief,
        branch,
        messages,
    } = spec;
    // Its own worktree if it still exists, else the shared root — an agent whose
    // branch was merged continues in the main checkout, which is where its work
    // now is.
    let isolated = branch
        .as_deref()
        .map(|_| git::worktree_path(&root, id))
        .filter(|path| path.exists());
    let ws_root = isolated.clone().unwrap_or_else(|| root.clone());
    let ws = Workspace::new(&ws_root).expect("workspace root must exist");
    let ws_root_str = ws.root_str();
    // A branch with no worktree left must not be carried: the actor commits at
    // the end of every run, and that commit would land in the human's own
    // checkout.
    let branch = if isolated.is_some() { branch } else { None };
    let ctx = Arc::new(AgentCtx {
        cfg: cfg.clone(),
        model: Arc::new(HttpModel::new(cfg)),
        events: Arc::new(Ui::new(tx, ConversationId(conversation))),
        machine: Arc::new(Shell),
        clock: Arc::new(clock::System),
        root,
        ids,
        live,
    });
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<AgentMsg>();
    let (dead_tx, dead_rx) = crossbeam_channel::unbounded::<AgentMsg>();
    drop(dead_rx);
    let actor = Actor {
        ctx,
        id,
        depth,
        ws,
        branch,
        brief: brief.clone(),
        my_tx: cmd_tx.clone(),
        parent_tx: dead_tx,
        rx: cmd_rx,
    };
    // The system prompt is regenerated, and an agent with no transcript but a
    // known brief is seeded with the task — so a worktree found on disk resumes
    // knowing what it was for, even though it has no memory of the run.
    let mut transcript = vec![Message::system(prompt::subagent_prompt(
        &ws_root_str,
        depth,
        isolated.is_some(),
    ))];
    if messages.is_empty() && !brief.is_empty() {
        transcript.push(Message::user(brief));
    } else {
        transcript.extend(messages);
    }
    start(actor, transcript, true);
    cmd_tx
}

/// Run an actor on its own thread. A thread that cannot start is reported as
/// that agent's result, so a parent waiting on it is never left waiting.
fn start(actor: Actor, initial: Vec<Message>, start_immediately: bool) {
    let id = actor.id;
    let ctx = actor.ctx.clone();
    let parent_tx = actor.parent_tx.clone();
    let builder = std::thread::Builder::new().name(format!("mush-agent-{id}"));
    if let Err(error) = builder.spawn(move || actor_main(actor, initial, start_immediately)) {
        let summary = format!("agent #{id} could not start ({error})");
        ctx.emit(id, AgentEvent::Error(summary.clone()));
        let _ = parent_tx.send(AgentMsg::ChildDone {
            id,
            outcome: Outcome::Failed(summary),
        });
    }
}

fn actor_main(actor: Actor, mut transcript: Vec<Message>, start_immediately: bool) {
    let mut state = ActorState::default();
    // Children are handed a task and start at once; the root waits to be asked.
    let mut ready = start_immediately;
    loop {
        if !wait_for_work(&actor, &mut state, &mut transcript, ready) {
            return;
        }
        ready = false;
        let cancel = run_cancel(&mut state);
        // Say so up front: the UI did not necessarily ask for this run (a nap
        // ends with a wake-up), and the tree must show it running. The flag
        // travels with the event so the human can stop a run that is blocked
        // waiting for a model reply.
        actor.ctx.emit(
            actor.id,
            AgentEvent::Running {
                cancel: cancel.clone(),
            },
        );
        actor.ctx.live.fetch_add(1, Ordering::SeqCst);
        let result = run_loop(&actor, &mut state, &mut transcript, &cancel);
        actor.ctx.live.fetch_sub(1, Ordering::SeqCst);
        // How the run ended decides both the commit subject and what the parent
        // is told. A stopped run still has work worth keeping, but its commit
        // must not read like a finished one.
        let outcome = match result {
            Ok(Some(text)) => Outcome::Finished(text),
            Ok(None) => Outcome::Finished("(finished)".to_string()),
            Err(error) if error == CANCELLED => Outcome::Stopped,
            Err(error) => Outcome::Failed(error),
        };
        // An isolated agent's branch *is* the deliverable mush documents for it
        // (`/diff`, `/merge`, `/discard`), so its work is committed here instead
        // of being left as untracked files in the worktree. Before the parent is
        // told, so a diff or merge it triggers already sees the work.
        if let Some(branch) = actor.branch.clone() {
            match commit_worktree(actor.ws.root(), actor.id, &actor.brief, &outcome) {
                Ok(Some(revision)) => {
                    actor.ctx.emit(
                        actor.id,
                        AgentEvent::Status(format!("committed {revision} on {branch}")),
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    actor.ctx.emit(
                        actor.id,
                        AgentEvent::Status(format!("could not commit the worktree: {error}")),
                    );
                }
            }
        }
        let _ = actor.parent_tx.send(AgentMsg::ChildDone {
            id: actor.id,
            outcome: outcome.clone(),
        });
        match outcome {
            Outcome::Failed(error) => actor.ctx.emit(actor.id, AgentEvent::Error(error)),
            Outcome::Stopped => actor.ctx.emit(actor.id, AgentEvent::Stopped),
            Outcome::Finished(_) => actor.ctx.emit(actor.id, AgentEvent::Done),
        }
        // A Shutdown arrived while this run was winding down: it is over, and
        // so is this actor.
        if state.shutdown {
            return;
        }
    }
}

/// Wait for the next run, folding every already-queued command into the
/// transcript first so a batch of completions costs one run, not one each.
/// `ready` skips the blocking wait when the actor was started with a task.
/// Returns `false` when this actor should end.
fn wait_for_work(
    actor: &Actor,
    state: &mut ActorState,
    transcript: &mut Vec<Message>,
    ready: bool,
) -> bool {
    if !ready {
        // Idle: block until there is something to do. A stray Stop carries no
        // work, so it just means waiting again.
        loop {
            // A `/compact` folded in below — or left over from a run that
            // ended before it reached a boundary — is honoured here rather
            // than in the run that follows, because there is no run: the
            // human asked for a summary, not for an answer turn. The actor
            // folds its conversation and goes back to waiting.
            if state.compact_requested {
                compact_now(actor, state, transcript);
                if state.shutdown {
                    return false;
                }
                continue;
            }
            match actor.rx.recv() {
                // Every handle to this agent is gone; so is any reason to live.
                Err(_) => return false,
                Ok(command) => match absorb(state, transcript, command) {
                    Fold::End => return false,
                    Fold::Run => break,
                    Fold::Idle => continue,
                },
            }
        }
    }
    // Fold in whatever else is already queued, so a batch of completions costs
    // one run instead of one run each.
    loop {
        match actor.rx.try_recv() {
            Err(_) => return true,
            Ok(command) => {
                // A Stop that arrives *behind* the work it was aimed at. The
                // blocking loop above folds a Stop away because nothing has
                // been asked of an idle actor; here the run is about to start,
                // so a Stop that came after the Run is aimed at it. Swallowing
                // it is the one way a Ctrl-C does nothing at all: the run pays
                // for its model calls and the human waits for the row to stop
                // saying `⊘` on its own (finding B6).
                let aimed_at_this_run = matches!(command, AgentMsg::Stop);
                match absorb(state, transcript, command) {
                    Fold::End => return false,
                    _ if aimed_at_this_run => state.stop_requested = true,
                    Fold::Run | Fold::Idle => {}
                }
            }
        }
    }
}

/// The cancellation flag a run starts with.
///
/// A Stop that arrived with the work — after the `Run`, before this run's first
/// message boundary — is already aimed at it, so the flag is born set. Anything
/// else starts a run the human has not asked to stop, even if an earlier Stop
/// was folded away while the actor was idle: that one cancelled nothing.
fn run_cancel(state: &mut ActorState) -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(std::mem::take(&mut state.stop_requested)))
}

/// What a command means for an actor that is not running.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Fold {
    /// Folded in; stay idle.
    Idle,
    /// There is work to do.
    Run,
    /// End this actor.
    End,
}

/// Fold one mailbox command into the actor's transcript.
fn absorb(state: &mut ActorState, transcript: &mut Vec<Message>, command: AgentMsg) -> Fold {
    match command {
        // An idle agent has nothing to cancel, so a Stop is a no-op here.
        // Ending an agent is what Shutdown is for.
        AgentMsg::Stop => Fold::Idle,
        AgentMsg::Shutdown => Fold::End,
        AgentMsg::Run(messages) => {
            // The UI's transcript is newer than ours; it wins. Mark as already
            // delivered whatever completion lines it carries (the model reads
            // them there), so the pending-completion step below cannot inject
            // the same news twice; anything it cannot know about is still
            // ours to announce.
            *transcript = messages;
            // A nudge parked here is already in that transcript — the UI echoes
            // every human message before sending it — so keeping the copy would
            // hand the model the same words twice at the next boundary. (That is
            // the cancelled-run case: the run ended before the nudge was folded
            // in, and the human has since written again.)
            // A `Run` is parked here for the same reason a nudge is: the
            // transcript it carries has already replaced ours, so re-folding it
            // would hand the model the human's words twice — and the stale copy
            // would land *after* the newer transcript, reading as the newest
            // message.
            state
                .deferred
                .retain(|command| !matches!(command, AgentMsg::Nudge(_) | AgentMsg::Run(_)));
            // The human may have typed while a tool batch was running, which
            // puts their words between an assistant's calls and their results;
            // strict servers reject that shape.
            repair_tool_pairs(transcript);
            let announced: Vec<u64> = state
                .completed
                .keys()
                .filter(|id| {
                    let line = format!("#{id} done:");
                    transcript
                        .iter()
                        .any(|message| message.text().contains(&line))
                })
                .copied()
                .collect();
            state.delivered = announced.into_iter().collect();
            Fold::Run
        }
        AgentMsg::Nudge(text) => {
            transcript.push(Message::user(text));
            Fold::Run
        }
        // The human asked for a fold now. Not work to answer, so not a run:
        // the flag is honoured by `wait_for_work`'s idle loop, and by the
        // next turn of a run already in flight.
        AgentMsg::Compact => {
            state.compact_requested = true;
            Fold::Idle
        }
        AgentMsg::ChildDone { id, outcome } => {
            // The parent ended (or napped) while a child still ran: waking it
            // with the completion restarts its run with the result folded in,
            // so an early End is not a lost result, it is a nap. The
            // completion counts as delivered because the model is about to
            // read it in this very run.
            let news = outcome.is_news();
            let line = note_completion(state, id, outcome);
            transcript.push(Message::user(line));
            state.delivered.insert(id);
            // A stopped child is the human's doing, not news that warrants
            // waking a napping parent into a fresh (paid) run: the line is in
            // the transcript for whenever the parent runs next.
            if news {
                Fold::Run
            } else {
                Fold::Idle
            }
        }
    }
}

/// One run: model turns → tool calls → results, until the model answers.
fn run_loop(
    actor: &Actor,
    state: &mut ActorState,
    messages: &mut Vec<Message>,
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    let schemas = if actor.depth >= MAX_DEPTH {
        prompt::leaf_tool_schemas()
    } else {
        prompt::tool_schemas()
    };
    // One learning attempt per run: a context-limit complaint teaches the
    // window, anything else is the run's error.
    let mut learned_context = false;

    // Loop detection: what justifies stopping a run early is a lack of
    // progress, not a turn count.
    let mut last_batch = String::new();
    let mut repeats = 0usize;
    // Consecutive replies the endpoint cut off at the token cap.
    let mut cut_offs = 0usize;

    for turn in 0..RUNAWAY_TURNS {
        // The last turn is a wrap-up: no tools, and a request for a summary.
        // A long task then ends with a report of what was done and what is
        // left, instead of a bare `stopped after N turns` (finding N1).
        let wrap_up = turn + 1 == RUNAWAY_TURNS;
        if wrap_up {
            // The wrap-up turn still ends the run with a summary (finding N1),
            // but the human should learn why tools suddenly went away.
            actor.ctx.emit(
                actor.id,
                AgentEvent::Notice(format!(
                    "runaway guard reached ({RUNAWAY_TURNS} turns) — asking the model to wrap up"
                )),
            );
        }
        drain_mailbox(&actor.rx, cancel, messages, state);
        if cancel.load(Ordering::SeqCst) {
            return Err(CANCELLED.to_string());
        }

        let mut cfg = actor
            .ctx
            .cfg
            .lock()
            .map(|config| config.clone())
            .map_err(|_| "shared configuration poisoned".to_string())?;

        let budget = cfg.history_budget();
        // Approaching the context window — or asked for outright by a
        // `/compact` that arrived at this boundary: fold the conversation into
        // a summary instead of dropping old turns, so long-running tasks keep
        // their state. The summarize request re-sends the history, so only
        // fire while it still fits; beyond that, trimming stays the last
        // resort.
        if state.compact_requested || needs_compaction(messages, budget) {
            compact_history(actor, &cfg, messages, cancel, state)?;
        }
        // Keep the whole request inside the endpoint's context window.
        trim_history(messages, budget);

        // The wrap-up turn asks for a summary, appended only to the request so
        // the stored transcript does not carry a turn-limit notice. A stop that
        // arrived in the meantime is honoured below, before the request goes
        // out.
        let asked;
        let request_messages: &[Message] = if wrap_up {
            asked = {
                let mut with_instruction = messages.clone();
                with_instruction.push(Message::user(WRAP_UP_INSTRUCTION));
                with_instruction
            };
            &asked
        } else {
            messages
        };

        let mut request = ChatRequest {
            model: &cfg.model,
            messages: request_messages,
            tools: if wrap_up { &[] } else { &schemas },
            // `auto` keeps models that ignore tools working: they simply answer.
            tool_choice: if wrap_up { "none" } else { "auto" },
            stream: false,
            temperature: cfg.temperature(),
            max_tokens: reply_cap(&cfg),
            max_completion_tokens: None,
            thinking: None,
            reasoning_effort: None,
        };
        // The cap travels as `max_completion_tokens` only where that is the
        // name the endpoint takes (OpenAI's reasoning models reject the old
        // one); everywhere else keeps the field every OpenAI-compatible server
        // documents.
        if cfg.uses_max_completion_tokens() {
            request.max_completion_tokens = Some(request.max_tokens);
            request.max_tokens = 0;
        }
        // Provider-specific knobs (DeepSeek thinking mode), only for providers
        // that advertise them; other endpoints see a plain request.
        if cfg.thinking_enabled() {
            request.thinking = Some(json!({ "type": "enabled" }));
        }
        if let Some(effort) = cfg.reasoning_effort() {
            request.reasoning_effort = Some(effort.to_string());
        }

        let reply = match actor.ctx.model.chat(&request, cancel) {
            Ok(reply) => reply,
            // The reader stops the moment the human cancels; that is a
            // cancellation, not a failure to reach the endpoint.
            Err(ModelError::Cancelled) => return Err(CANCELLED.to_string()),
            // A refusal — a body past `MAX_BODY_BYTES`, a malformed status or
            // chunk line — is not a connection failure: the endpoint answered,
            // and saying so is the difference between "check the URL" and "the
            // reply was too big".
            Err(ModelError::Refused(error)) => {
                return Err(format!("the endpoint's reply was refused: {error}"));
            }
            Err(ModelError::Unreachable(error)) => {
                return Err(format!("cannot reach {}: {error}", cfg.base_url));
            }
            Err(ModelError::Encode(error)) => {
                return Err(format!("could not encode request: {error}"));
            }
            Err(ModelError::Malformed(error)) => {
                return Err(format!("could not parse model response: {error}"));
            }
            Err(ModelError::Status { status, body }) => {
                let parsed = serde_json::from_str::<ChatResponse>(&body).ok();
                let detail = parsed
                    .and_then(|r| r.error.map(|e| e.message))
                    .unwrap_or_else(|| truncate(&body, 600));
                // A hosted API advertises nothing, so its own complaint is the
                // only current source for the window. Learn it, tell the human,
                // retry once — and never again in this run, or a server that
                // complains about everything becomes a loop. A number that
                // would collapse the window by more than 8x is refused: a
                // rate-limit body must not teach mush that the endpoint has ten
                // tokens (finding A3).
                if !learned_context && !cfg.context_explicit {
                    if let Some(tokens) = parse_context_hint(&detail) {
                        let plausible = tokens < cfg.context_tokens
                            && tokens.saturating_mul(8) >= cfg.context_tokens;
                        if plausible {
                            cfg.context_tokens = tokens;
                            if let Ok(mut shared) = actor.ctx.cfg.lock() {
                                shared.context_tokens = tokens;
                            }
                            // The UI owns the copy every surface reads, so it
                            // gets the number too (finding B7).
                            actor.ctx.emit(actor.id, AgentEvent::Context { tokens });
                            actor.ctx.emit(
                                actor.id,
                                AgentEvent::Status(format!(
                                    "context window is {tokens} tokens — retrying"
                                )),
                            );
                            learned_context = true;
                            continue;
                        }
                    }
                }
                return Err(format!("model returned HTTP {status}: {detail}"));
            }
        };

        let Some(choice) = reply.choices.into_iter().next() else {
            return Err("model returned no choices".to_string());
        };
        // `length` means the endpoint cut the reply off at `max_tokens` — with
        // a thinking model the cap can be spent before any visible text. Such a
        // reply is not a result: the text is partial and a tool call may be
        // half-written JSON, so the run fails loudly below instead of ending as
        // if the work were done.
        let truncated = choice.finish_reason.as_deref() == Some("length");

        let assistant = sanitize_tool_calls(choice.message);
        let tool_calls = assistant.tool_calls().to_vec();
        let content = assistant.text().trim().to_string();

        // A Stop (cancel, new chat) or Shutdown may have arrived while the
        // request was in flight: drop the stale reply instead of delivering it
        // into a fresh conversation; the run ends as cancelled. A nudge is
        // parked rather than folded: it arrived after the model wrote this
        // reply, so the transcript must not read as if the model had seen it.
        drain_signals(actor, cancel, state);
        if cancel.load(Ordering::SeqCst) {
            return Err(CANCELLED.to_string());
        }

        messages.push(assistant.clone());
        actor.ctx.emit(actor.id, AgentEvent::Message(assistant));

        if truncated {
            // Every call in the emitted message must be answered or the
            // transcript keeps a dangling tool call, but a call cut off at the
            // token cap must never run: its arguments are whatever JSON
            // survived. Answer them with the reason instead of running them.
            for call in &tool_calls {
                let message = Message::tool(
                    call.id.clone(),
                    format!(
                        "error: the model's reply was cut off at {} tokens; this call was not run",
                        reply_cap(&cfg)
                    ),
                );
                messages.push(message.clone());
                actor.ctx.emit(actor.id, AgentEvent::Message(message));
            }
            // A cut-off reply is not a result, but it is usually a *big* answer
            // rather than a broken model (a whole file in one `write_file`, or
            // a long reasoning pass). Ask for smaller pieces and carry on;
            // only keep failing if the model will not write that small.
            cut_offs += 1;
            if cut_offs > TRUNCATION_ROUNDS {
                return Err(format!(
                    "the model's reply was cut off at the {}-token limit \
                     (finish_reason: length) {cut_offs} times in a row — nothing after it ran",
                    reply_cap(&cfg)
                ));
            }
            actor.ctx.emit(
                actor.id,
                AgentEvent::Notice(format!(
                    "reply cut off at {} tokens — asking for smaller steps",
                    reply_cap(&cfg)
                )),
            );
            messages.push(Message::user(TRUNCATION_INSTRUCTION));
            continue;
        }

        // The same batch of calls, twice in a row with nothing changed in
        // between, means the model is repeating itself rather than working.
        // This — not a turn count — is the honest reason to stop a run early.
        if !tool_calls.is_empty() {
            let batch = tool_calls
                .iter()
                .map(|call| format!("{}:{}", call.function.name, call.function.arguments))
                .collect::<Vec<_>>()
                .join("\n");
            if batch == last_batch {
                repeats += 1;
            } else {
                repeats = 0;
                last_batch = batch;
            }
            if repeats >= LOOP_ROUNDS {
                for call in &tool_calls {
                    let message = Message::tool(
                        call.id.clone(),
                        "error: this call was not run — the run was stopped as a loop",
                    );
                    messages.push(message.clone());
                    actor.ctx.emit(actor.id, AgentEvent::Message(message));
                }
                let count = repeats + 1;
                actor.ctx.emit(
                    actor.id,
                    AgentEvent::Notice(format!(
                        "the run repeated the same tool call {count} times without changing \
                         anything — stopping it as a loop"
                    )),
                );
                return Err(format!(
                    "the run was stopped as a loop: the same tool call repeated {count} times \
                     with nothing changed in between"
                ));
            }
        }

        if wrap_up {
            // Whatever the model wrote is the run's result. If it tried to keep
            // calling tools, answer the calls so the transcript stays valid,
            // but run none of them: the run is out of turns.
            for call in &tool_calls {
                let message = Message::tool(
                    call.id.clone(),
                    format!("error: the run hit its {RUNAWAY_TURNS}-turn runaway guard; tools are no longer available"),
                );
                messages.push(message.clone());
                actor.ctx.emit(actor.id, AgentEvent::Message(message));
            }
            return if content.is_empty() {
                Err(format!(
                    "stopped after {RUNAWAY_TURNS} turns without finishing (runaway guard)"
                ))
            } else {
                Ok(Some(content))
            };
        }

        if tool_calls.is_empty() {
            if content.is_empty() {
                actor.ctx.emit(
                    actor.id,
                    AgentEvent::Notice("model produced an empty reply".into()),
                );
            }
            // Parked nudges belong after the reply; the human wrote them while
            // it was in flight, so the model has not answered them yet.
            let before = messages.len();
            drain_mailbox(&actor.rx, cancel, messages, state);
            if cancel.load(Ordering::SeqCst) {
                return Err(CANCELLED.to_string());
            }
            let steered = messages.len() > before;
            // Children may have finished while we were working without being
            // waited on: deliver their summaries and keep going instead of
            // ending. (Completions that arrive after this run returns wake the
            // idle actor instead — see actor_main.)
            let pending: Vec<(u64, Outcome)> = state
                .completed
                .iter()
                .filter(|(child, _)| !state.delivered.contains(child))
                .map(|(child, outcome)| (*child, outcome.clone()))
                .collect();
            if !pending.is_empty() {
                for (child, outcome) in &pending {
                    let line = note_completion(state, *child, outcome.clone());
                    messages.push(Message::user(line));
                    state.delivered.insert(*child);
                }
                continue;
            }
            // Answer the steering instead of ending the run without it: the
            // model has not seen those words yet.
            if steered {
                continue;
            }
            return Ok(if content.is_empty() {
                None
            } else {
                Some(content)
            });
        }

        // Every call in a batch must be answered, or the transcript keeps an
        // assistant message whose tool calls dangle — which most servers then
        // reject for the rest of the conversation.
        for (index, call) in tool_calls.iter().enumerate() {
            // Keep watching for a Stop/Shutdown between calls, and answer the
            // rest of the batch before leaving: a cancellation must not leave
            // unanswered tool calls behind.
            drain_signals(actor, cancel, state);
            if cancel.load(Ordering::SeqCst) {
                for skipped in &tool_calls[index..] {
                    let message = Message::tool(skipped.id.clone(), format!("error: {CANCELLED}"));
                    messages.push(message.clone());
                    actor.ctx.emit(actor.id, AgentEvent::Message(message));
                }
                return Err(CANCELLED.to_string());
            }

            let named = call.function.name.clone();
            let args: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);

            // An invented name is answered like any other failure, so the batch
            // still gets a tool message for every call.
            let tool = ToolName::parse(&named);
            actor.ctx.emit(
                actor.id,
                AgentEvent::Status(format!("{} {}", named, summarize(&args))),
            );

            let result = match tool {
                Some(tool) => exec_tool(actor, state, tool, &args, cancel),
                None => Err(format!("unknown tool `{named}`")),
            };

            let output = match result {
                Ok(output) => output,
                Err(error) => format!("error: {error}"),
            };
            let tool_message = Message::tool(call.id.clone(), output);
            messages.push(tool_message.clone());
            actor.ctx.emit(actor.id, AgentEvent::Message(tool_message));
        }
        // Fold mailbox commands in at the message boundary, and honour a
        // cancellation now that every call has a result.
        drain_mailbox(&actor.rx, cancel, messages, state);
        if cancel.load(Ordering::SeqCst) {
            return Err(CANCELLED.to_string());
        }
    }

    Err(format!(
        "stopped after {RUNAWAY_TURNS} turns without finishing (runaway guard)"
    ))
}

/// The instruction appended to the request on the run's final turn.
const WRAP_UP_INSTRUCTION: &str = "\
You have reached this run's runaway guard, which is meant to be far past any \
real task. Stop using tools now — they are no longer available. Reply with a \
concise summary of what has been done, what still remains, and anything the \
next run needs to know.";

/// What the model is told after a reply was cut off at the token cap. A cut
/// reply is usually a *big* answer — a whole file in one call, or a long
/// reasoning pass — so the instruction is about size, and about not re-doing
/// work that was already written before the cut.
const TRUNCATION_INSTRUCTION: &str = "\
Your previous reply was cut off by the endpoint's length limit, so none of it \
ran. Do the same work in smaller steps: one file per call, a few hundred lines \
at a time (write the first part with write_file, then add the rest with \
edit_file). Do not repeat work you already completed in earlier calls.";

/// What a human who typed `/compact` is told when there is nothing to fold.
///
/// A fold of `[system, the opening message]` costs a request and can only
/// re-summarize the summary, so it is refused — but never silently: the human
/// asked, and a status line that fades into nothing is the failure mode this
/// whole command exists to avoid. Anything bigger is folded as asked.
const NOTHING_TO_COMPACT: &str =
    "nothing to compact — this transcript is already short enough to send whole";

/// Fold the transcript into a summary: ask the model to condense it, then
/// replace the conversation with `[system, user(summary)]` — the summary is
/// the new opening task message, which trimming protects. Does nothing when
/// the model could not produce a summary; trimming is the fallback.
///
/// The one compaction routine: the automatic trigger (the window filling up)
/// and the human's `/compact` both come through here, so they cannot disagree
/// about what "the summary message" is or about when folding is worth a call.
fn compact_history(
    actor: &Actor,
    cfg: &Config,
    messages: &mut Vec<Message>,
    cancel: &AtomicBool,
    state: &mut ActorState,
) -> Result<(), String> {
    // Whether the human asked for this fold, as opposed to the window filling
    // on its own. Only the first is owed a line when there is nothing to do:
    // the automatic trigger would not have fired, so it has nothing to report.
    let asked = std::mem::take(&mut state.compact_requested);
    if !matches!(messages.first(), Some(message) if message.role == "system") {
        return Ok(());
    }
    // Nothing left to fold: system + one message is already minimal
    // (usually a previous summary), so compacting again would just cost a
    // request and re-summarize the summary. A human who asked for it is told
    // so rather than left watching a status line that never ends.
    if messages.len() <= 2 {
        if asked {
            actor
                .ctx
                .emit(actor.id, AgentEvent::Notice(NOTHING_TO_COMPACT.to_string()));
        }
        return Ok(());
    }
    let actor_id = actor.id;
    // The human is told why this is happening: "nearly full" is a fact about
    // the automatic trigger, and saying it for a fold they asked for would be
    // a line about a condition that is not true.
    let why = if asked {
        "compacting on request…"
    } else {
        "context nearly full — summarizing…"
    };
    actor
        .ctx
        .emit(actor_id, AgentEvent::Status(why.to_string()));

    // Fold pending nudges/completions in first; a Stop cancels the run.
    drain_mailbox(&actor.rx, cancel, messages, state);
    if cancel.load(Ordering::SeqCst) {
        return Err(CANCELLED.to_string());
    }

    let mut ask = messages.clone();
    ask.push(Message::user(COMPACT_INSTRUCTION));
    // A plain, tool-free request: the summary, nothing else.
    let request = ChatRequest {
        model: &cfg.model,
        messages: &ask,
        tools: &[],
        tool_choice: "none",
        stream: false,
        temperature: cfg.temperature(),
        max_tokens: COMPACT_REPLY_TOKENS,
        max_completion_tokens: None,
        thinking: if cfg.thinking_enabled() {
            Some(json!({ "type": "enabled" }))
        } else {
            None
        },
        reasoning_effort: cfg.reasoning_effort().map(str::to_string),
    };
    let reply = match actor.ctx.model.chat(&request, cancel) {
        Ok(reply) => reply,
        // A cancelled run is already ending; do not report a network failure.
        Err(ModelError::Cancelled) => return Err(CANCELLED.to_string()),
        // The run will fail on its real request anyway; surface it.
        Err(ModelError::Unreachable(error)) | Err(ModelError::Refused(error)) => {
            return Err(format!("cannot reach {}: {error}", cfg.base_url));
        }
        Err(ModelError::Encode(error)) => return Err(format!("could not encode request: {error}")),
        // The endpoint complained, or answered something we cannot read: the
        // run will fail on its real request anyway, and a summary mush could
        // not make is not that failure.
        Err(ModelError::Status { .. }) | Err(ModelError::Malformed(_)) => return Ok(()),
    };
    let summary = reply
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message.text().trim().to_string())
        .unwrap_or_default();
    if summary.is_empty() {
        if asked {
            actor.ctx.emit(
                actor.id,
                AgentEvent::Notice("could not compact — the model returned no summary".to_string()),
            );
        }
        return Ok(());
    }

    let system = messages[0].clone();
    *messages = vec![system, Message::user(prompt::compaction_message(&summary))];
    actor.ctx.emit(
        actor.id,
        AgentEvent::Compact {
            summary: summary.clone(),
        },
    );
    Ok(())
}

/// Fold an idle agent's conversation into a summary because the human asked
/// (`/compact`).
///
/// This is the whole request: one summarize call and one transcript
/// replacement, through the same [`compact_history`] the automatic trigger
/// uses, so the pane, the session save and the meter — all driven by the
/// `Compact` event — stay in sync for both. It is deliberately *not* a run:
/// no `Running`, so the row never claims work; no answer turn, because there
/// is nothing to answer (the summary is the result); nothing reported to the
/// parent, because no run ended.
///
/// Failures are a notice rather than an `Error`: an idle agent that could not
/// summarize has not failed at anything, and marking its row failed would be
/// the same lie in the other direction.
fn compact_now(actor: &Actor, state: &mut ActorState, transcript: &mut Vec<Message>) {
    // The same fallback a tool takes on a poisoned cell: the fold everything
    // else reads has already been asked for, and the flag must not be left
    // set, or the actor would fold the same transcript on every wait.
    let cfg = actor
        .ctx
        .cfg
        .lock()
        .map(|cfg| cfg.clone())
        .unwrap_or_else(|_| Config::new("http://127.0.0.1:1", "", None));
    // A fresh flag: an idle agent has no run for a Stop to cancel. One that
    // arrives while the summary is in flight still ends the request early,
    // and is then swallowed exactly as it is for any idle actor.
    let cancel = AtomicBool::new(false);
    match compact_history(actor, &cfg, transcript, &cancel, state) {
        Ok(()) => {}
        // The human stopped it; that is not a failure to report.
        Err(error) if error == CANCELLED => {}
        Err(error) => actor.ctx.emit(
            actor.id,
            AgentEvent::Notice(format!("could not compact: {error}")),
        ),
    }
}

/// Fold in only what may appear between tool calls: cancellation, shutdown, and
/// child completions. Nudges and new transcripts are *parked* for the next
/// message boundary — a user message between an assistant's tool calls and their
/// results makes strict servers reject the whole conversation. They are parked
/// in the actor's own state, never put back in the mailbox: that is the queue
/// this function is draining, so re-sending would spin forever.
fn drain_signals(actor: &Actor, cancel: &AtomicBool, state: &mut ActorState) {
    for command in actor.rx.try_iter() {
        match command {
            AgentMsg::Stop => cancel.store(true, Ordering::SeqCst),
            AgentMsg::Shutdown => {
                cancel.store(true, Ordering::SeqCst);
                state.shutdown = true;
            }
            AgentMsg::ChildDone { id, outcome } => {
                note_completion(state, id, outcome);
            }
            parked => state.deferred.push(parked),
        }
    }
}

/// Fold pending mailbox commands into the current run: nudges become user
/// messages, stops set the cancel flag, child completions update the registry.
/// Everything parked by `drain_signals` goes in first, in order.
fn drain_mailbox(
    rx: &Receiver<AgentMsg>,
    cancel: &AtomicBool,
    messages: &mut Vec<Message>,
    state: &mut ActorState,
) {
    let parked = std::mem::take(&mut state.deferred);
    for command in parked.into_iter().chain(rx.try_iter()) {
        match command {
            AgentMsg::Nudge(text) => messages.push(Message::user(text)),
            // A message-boundary job like a nudge: the transcript is folded
            // into a summary at the next turn, never between an assistant's
            // tool calls and their results.
            AgentMsg::Compact => state.compact_requested = true,
            AgentMsg::Stop => cancel.store(true, Ordering::SeqCst),
            AgentMsg::Shutdown => {
                // Cancel now, and remember: the run ends, and so does the actor.
                cancel.store(true, Ordering::SeqCst);
                state.shutdown = true;
            }
            AgentMsg::ChildDone { id, outcome } => {
                note_completion(state, id, outcome);
            }
            // The UI sends a whole transcript when it believes we are idle.
            // We are mid-run, so the only new information is the message the
            // human just typed — fold that in rather than dropping input the
            // UI has already echoed. The full transcript re-syncs at the next
            // idle Run.
            AgentMsg::Run(transcript) => {
                if let Some(message) = transcript.last() {
                    if message.role == "user" {
                        messages.push(message.clone());
                    }
                }
            }
        }
    }
}

/// Record a child's completion and return the line the model reads. A fresh
/// completion also supersedes any earlier delivery of the same child.
fn note_completion(state: &mut ActorState, id: u64, outcome: Outcome) -> String {
    state.running.remove(&id);
    // Always the latest outcome: a child that was stopped and then nudged
    // finishes later, and the stale `stopped` must not outlive the result.
    let line = outcome.line(id);
    state.completed.insert(id, outcome);
    state.delivered.remove(&id);
    line
}

fn exec_tool(
    actor: &Actor,
    state: &mut ActorState,
    tool: ToolName,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<String, String> {
    match tool {
        ToolName::RunCommand => run_command(actor, state, args, cancel),
        ToolName::SpawnAgent => spawn_tool(actor, state, args),
        ToolName::WaitAgents => wait_tool(actor, state, cancel, args),
        ToolName::AgentStatus => status_tool(state),
        ToolName::AgentControl => control_tool(state, args),
        // The file tools read and write the workspace directly.
        ToolName::ListFiles | ToolName::ReadFile | ToolName::WriteFile | ToolName::EditFile => {
            let cfg = actor
                .ctx
                .cfg
                .lock()
                .map(|config| config.clone())
                .unwrap_or_else(|_| Config::new("http://127.0.0.1:1", "", None));
            direct_tool(&actor.ws, tool, args, &cfg)
        }
    }
}

fn spawn_tool(actor: &Actor, state: &mut ActorState, args: &Value) -> Result<String, String> {
    let ctx = &actor.ctx;
    let (parent, depth) = (actor.id, actor.depth);
    if depth >= MAX_DEPTH {
        return Err(format!(
            "cannot spawn: depth {depth} is the limit ({MAX_DEPTH})"
        ));
    }
    if ctx.live.load(Ordering::SeqCst) >= MAX_AGENTS {
        return Err(format!(
            "cannot spawn: {MAX_AGENTS} agents are already running tree-wide (the limit). \
             Wait for one with wait_agents before spawning another."
        ));
    }
    let brief = tools::arg_string(args, "brief")?;
    let isolated = args
        .get("isolated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !isolated && !state.running.is_empty() {
        // Decide this *before* writing the brief: the check can only fail after
        // the brief exists, so the rule is stated in the tool schema and the
        // system prompt as well.
        return Err(
            "cannot spawn: a sibling agent already runs in this shared workspace, and only one \
             non-isolated child may run at a time. Set isolated=true (its own git worktree) to \
             run siblings in parallel, or wait_agents for the running one first."
                .to_string(),
        );
    }

    let id = ctx.ids.fetch_add(1, Ordering::SeqCst);
    let (child_ws, branch, note) = if isolated {
        // A private worktree on `mush/<id>`, based on the parent's branch (or
        // HEAD). The reason it cannot be made is reported either way, so
        // isolation degrades to the shared workspace instead of failing the
        // delegation.
        match git::worktree_add(&ctx.root, id, actor.branch.as_deref()) {
            Ok((path, branch)) => match Workspace::new(&path) {
                Ok(child_ws) => (child_ws, Some(branch), String::new()),
                // Isolation is best-effort: degrade to the shared workspace
                // rather than fail the delegation outright.
                Err(error) => (
                    actor.ws.clone(),
                    None,
                    format!(" (isolated unavailable: {error}; running in place)"),
                ),
            },
            Err(reason) => (
                actor.ws.clone(),
                None,
                format!(" (isolated unavailable: {reason}; running in place)"),
            ),
        }
    } else {
        (actor.ws.clone(), None, String::new())
    };

    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<AgentMsg>();
    // The UI hears about the child before any of its events can arrive, so
    // every later event has a node to land on.
    ctx.emit(
        parent,
        AgentEvent::Spawned {
            child: id,
            parent,
            brief: brief.clone(),
            depth: depth + 1,
            branch: branch.clone(),
            cmd: cmd_tx.clone(),
        },
    );

    // Who the child is goes in the system prompt; the parent's task is the
    // first user message, mirroring the root's system+user shape. Some
    // servers' chat templates also reject a system-only first request.
    let whoami = prompt::subagent_prompt(&child_ws.root_str(), depth + 1, branch.is_some());
    let brief_text = format!("{brief}{note}");
    let initial = if brief_text.trim().is_empty() {
        vec![
            Message::system(whoami),
            Message::user("Begin the task now."),
        ]
    } else {
        vec![Message::system(whoami), Message::user(brief_text)]
    };
    // The child's own mailbox is where grandchildren report; the parent's
    // mailbox is where this child reports its completion.
    let child = Actor {
        ctx: ctx.clone(),
        id,
        depth: depth + 1,
        ws: child_ws,
        branch,
        brief: brief.clone(),
        my_tx: cmd_tx.clone(),
        parent_tx: actor.my_tx.clone(),
        rx: cmd_rx,
    };
    start(child, initial, true);

    state.children.insert(id, cmd_tx);
    state.running.insert(id);
    // A run is bounded by progress, not by a turn count: it ends when the model
    // stops calling tools, and is cut short only if it starts looping
    // (`LOOP_ROUNDS` identical rounds). The only hard ceiling is a runaway
    // guard far past any real task, so this is not a budget to size a brief
    // against any more.
    Ok(format!(
        "spawned agent #{id} · runs until it stops calling tools · wait_agents returns its summary"
    ))
}

/// Whether the human's words are waiting behind a blocking tool call: a
/// `Nudge`, or a whole transcript the UI sent because it believed the agent was
/// idle (whose last message is the one the human just typed).
///
/// A blocking tool call is the one place a human message would otherwise sit
/// unread for as long as the call takes, so this is what it uses to end the
/// wait — see `wait_tool`.
fn parked_message(state: &ActorState) -> bool {
    state.deferred.iter().any(|command| match command {
        AgentMsg::Nudge(_) => true,
        AgentMsg::Run(messages) => messages
            .last()
            .is_some_and(|message| message.role == "user"),
        _ => false,
    })
}

fn wait_tool(
    actor: &Actor,
    state: &mut ActorState,
    cancel: &AtomicBool,
    args: &Value,
) -> Result<String, String> {
    let ids: Vec<u64> = args
        .get("ids")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    let candidates: Vec<u64> = if ids.is_empty() {
        let mut children: Vec<u64> = state.children.keys().copied().collect();
        children.sort_unstable();
        children
    } else {
        ids
    };
    if candidates.is_empty() {
        return Ok("no child agents to wait for".to_string());
    }
    // Waiting on an id that was never spawned would block for the whole
    // timeout and then claim the agents are still running. Say so instead.
    let unknown: Vec<String> = candidates
        .iter()
        .filter(|id| !state.children.contains_key(id))
        .map(|id| format!("#{id}"))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "no such child agent(s): {} — agent_status lists yours",
            unknown.join(", ")
        ));
    }
    let timeout = args
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(WAIT_TIMEOUT_SECS);
    // A model-supplied timeout must never overflow the clock; an
    // unrepresentable one just means "forever" (0 means that too).
    let clock = actor.ctx.clock.as_ref();
    let deadline = (timeout > 0)
        .then(|| clock.now().checked_add(Duration::from_secs(timeout)))
        .flatten();

    loop {
        // This is the one tool that blocks for minutes, so it is also the one
        // that must notice a cancellation (and a child's result) promptly.
        drain_signals(actor, cancel, state);
        if cancel.load(Ordering::SeqCst) {
            return Err(CANCELLED.to_string());
        }
        // And it must notice the human. Parking their words is not enough when
        // the wait can last the whole timeout: the model would not see them
        // until the child it was waiting on finished, which is the opposite of
        // steering. The wait ends, the words stay parked for the next message
        // boundary, and the model answers them in this run.
        if parked_message(state) {
            return Ok("interrupted — the human wrote to you while you waited; their message is in \
                       your transcript. Answer them; your agents are still running. Use wait_agents \
                       again when you need a result."
                .to_string());
        }
        for id in &candidates {
            if let Some(outcome) = state.completed.get(id) {
                state.delivered.insert(*id);
                // Not always `done`: a stopped child is reported as stopped, so
                // a waiter knows there is no result yet rather than receiving
                // one that says "cancelled".
                return Ok(outcome.line(*id));
            }
        }
        if let Some(deadline) = deadline {
            if clock.now() >= deadline {
                return Ok("wait timed out — your agents are still running".to_string());
            }
        }
        clock.sleep(Duration::from_millis(50));
    }
}

fn status_tool(state: &ActorState) -> Result<String, String> {
    if state.children.is_empty() {
        return Ok("no child agents".to_string());
    }
    let mut lines = Vec::new();
    let mut ids: Vec<u64> = state.children.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        match state.completed.get(&id) {
            // Each state gets its own mark: a stopped child was neither
            // finished (✓) nor failed (✗), and a parent that cannot tell them
            // apart treats a stop as a result.
            Some(Outcome::Finished(summary)) => lines.push(format!("#{id} ✓ {summary}")),
            Some(Outcome::Failed(error)) => lines.push(format!("#{id} ✗ {error}")),
            Some(Outcome::Stopped) => lines.push(format!(
                "#{id} ⊘ stopped — idle and resumable (agent_control message resumes it)"
            )),
            None => lines.push(format!("#{id} ◐ running")),
        }
    }
    Ok(lines.join("\n"))
}

fn control_tool(state: &mut ActorState, args: &Value) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing `id`".to_string())?;
    let action = tools::arg_string(args, "action")?;
    let Some(cmd) = state.children.get(&id) else {
        return Err(format!("no such child agent #{id}"));
    };
    // A dead mailbox means the child is gone; saying "stopping" anyway would
    // have the model wait on a result that can never arrive.
    let sent = match action.as_str() {
        "stop" => cmd.send(AgentMsg::Stop).map_err(|_| (id, "stop")),
        "message" => {
            let text = tools::arg_string(args, "text")?;
            cmd.send(AgentMsg::Nudge(text)).map_err(|_| (id, "message"))
        }
        other => return Err(format!("unknown action `{other}` (stop or message)")),
    };
    match sent {
        Ok(()) if action == "stop" => Ok(format!("stopping agent #{id}")),
        Ok(()) => Ok(format!("messaged agent #{id}")),
        Err((id, _)) => Err(format!("agent #{id} is gone")),
    }
}

/// Commit whatever an isolated agent left in its worktree, so the branch that
/// `/diff`, `/merge`, and `/discard` name actually carries the work. Returns the
/// short revision when something was committed, `None` when the run changed
/// nothing.
///
/// The subject is built here, next to the id, brief and outcome it is made of;
/// the commit itself is one of the core git verbs, so an isolated agent commits
/// by the same rules as everything else that touches a repository.
fn commit_worktree(
    root: &Path,
    id: u64,
    brief: &str,
    outcome: &Outcome,
) -> Result<Option<String>, String> {
    git::commit_all(root, &commit_subject(id, brief, outcome))
}

/// The five file tools, executed against a workspace on disk. Only the agent's
/// own thread touches the files: the human's screen never holds a copy, so
/// there is nothing to keep in sync.
fn direct_tool(
    ws: &Workspace,
    tool: ToolName,
    args: &Value,
    cfg: &Config,
) -> Result<String, String> {
    match tool {
        ToolName::ListFiles => tools::list_result(ws, args, cfg.list_limit()),
        ToolName::ReadFile => {
            let rel = tools::arg_string(args, "path")?;
            ws.read_file(&rel, cfg.read_cap())
        }
        ToolName::WriteFile => {
            let rel = tools::arg_string(args, "path")?;
            let content = tools::arg_string(args, "content")?;
            ws.write_file(&rel, &content)?;
            Ok(format!("wrote {rel}"))
        }
        ToolName::EditFile => {
            let rel = tools::arg_string(args, "path")?;
            let current = ws.read_file(&rel, usize::MAX)?;
            // A list of edits is applied to one read and written once: all of
            // them land or none do, so a batch cannot leave the file
            // half-changed, and the edits see each other's results in order.
            let updated = match args.get("edits").and_then(Value::as_array) {
                Some(list) if !list.is_empty() => {
                    let mut edits = Vec::with_capacity(list.len());
                    for entry in list {
                        edits.push(tools::Edit {
                            old: tools::arg_string(entry, "old_string")?,
                            new: tools::arg_string(entry, "new_string")?,
                            replace_all: entry
                                .get("replace_all")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                        });
                    }
                    tools::edit_text_many(&current, &edits, &rel)?
                }
                _ => {
                    let old = tools::arg_string(args, "old_string")?;
                    let new = tools::arg_string(args, "new_string")?;
                    tools::edit_text(&current, &old, &new, &rel)?
                }
            };
            ws.write_file(&rel, &updated)?;
            Ok(format!("edited {rel}"))
        }
        // Everything else is dispatched by `exec_tool`: reaching here would
        // mean a tool with no implementation, which the match now forbids.
        other => Err(format!("`{other}` is not a file tool")),
    }
}

fn run_command(
    actor: &Actor,
    state: &mut ActorState,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing `command`".to_string())?;
    run_shell(
        command,
        actor.ws.root(),
        Duration::from_secs(CMD_TIMEOUT_SECS),
        cancel,
        actor,
        state,
    )
}

/// Hard ceiling on what one command may write to its scratch files. The model
/// only ever sees the first `CMD_CAP` bytes, so a command that gets here is not
/// communicating, it is running away — and it must not fill the disk. The size
/// is checked every few milliseconds (see `wait_bounded`), so a fast writer can
/// overshoot by a few tens of MB before the kill lands.
const CMD_OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;

/// Why a command stopped running.
enum Ended {
    /// It ended by itself, with this exit code (`-1` when a signal ended it).
    Exited(i32),
    TimedOut,
    Cancelled,
    TooMuchOutput,
}

/// Run a shell command in `root` and return a report the model can read.
///
/// The command itself is the [`Machine`]'s: how to start one, how it is
/// watched, and the three ways it stops (its time is up, a Stop arrived, it
/// wrote too much) are this function's, which is what makes all three
/// assertable with a scripted machine and a scripted clock.
fn run_shell(
    command: &str,
    root: &Path,
    timeout: Duration,
    cancel: &AtomicBool,
    actor: &Actor,
    state: &mut ActorState,
) -> Result<String, String> {
    let mut job = actor.ctx.machine.spawn(&ShellCommand { command, root })?;
    let ended = wait_bounded(job.as_mut(), timeout, cancel, actor, state)?;
    let (stdout, stderr) = job.output(CMD_CAP);

    // No `$ {command}` echo: the tool call is already rendered from the
    // assistant message that made it (`⚙ run_command …`), so printing it here
    // again put the same command in the transcript twice — and, because tool
    // results are stored, in the saved session twice as well.
    let mut report = String::new();
    if !stdout.trim().is_empty() {
        report.push_str(stdout.trim_end());
        report.push('\n');
    }
    if !stderr.trim().is_empty() {
        report.push_str("--- stderr ---\n");
        report.push_str(stderr.trim_end());
        report.push('\n');
    }
    match ended {
        Ended::Exited(code) => report.push_str(&format!("[exit {code}]")),
        Ended::TimedOut => report.push_str(&format!("[timed out after {}s]", timeout.as_secs())),
        Ended::Cancelled => report.push_str("[cancelled]"),
        Ended::TooMuchOutput => report.push_str(&format!(
            "[killed: output passed {CMD_OUTPUT_LIMIT} bytes; the first {CMD_CAP} are above]"
        )),
    }
    Ok(report)
}

/// Wait for a command, stopping it when the timeout, a cancellation, or the
/// output limit arrives first.
fn wait_bounded(
    job: &mut dyn Job,
    timeout: Duration,
    cancel: &AtomicBool,
    actor: &Actor,
    state: &mut ActorState,
) -> Result<Ended, String> {
    let started = actor.ctx.clock.now();
    loop {
        match job.poll() {
            Ok(Some(code)) => return Ok(Ended::Exited(code)),
            Ok(None) => {}
            Err(error) => {
                // Never leave a running process behind on an error path.
                job.kill();
                return Err(error);
            }
        }
        // A Stop or Shutdown has to reach a command *while* it runs, or Ctrl-C
        // would wait for the command to finish (up to CMD_TIMEOUT_SECS). The
        // mailbox is polled here only for signals: nudges are parked for the
        // next message boundary, never folded in mid-batch.
        drain_signals(actor, cancel, state);
        let ended = if actor.ctx.clock.now().saturating_duration_since(started) > timeout {
            Some(Ended::TimedOut)
        } else if cancel.load(Ordering::SeqCst) {
            Some(Ended::Cancelled)
        } else if job.written() > CMD_OUTPUT_LIMIT {
            Some(Ended::TooMuchOutput)
        } else {
            None
        };
        if let Some(ended) = ended {
            job.kill();
            return Ok(ended);
        }
        actor.ctx.clock.sleep(Duration::from_millis(10));
    }
}

/// The one-word reading of a tool call's arguments, from the raw JSON the model
/// sent: what the tree and the transcript both show.
pub fn summarize_args(raw: &str) -> String {
    let args: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    summarize(&args)
}

fn summarize(args: &Value) -> String {
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        return path.to_string();
    }
    if let Some(command) = args.get("command").and_then(Value::as_str) {
        // The *first line*, with whitespace collapsed. Truncating the raw string
        // at 60 characters kept its newlines, so a heredoc turned a one-line
        // label into several — `⚙ run_command cd …` followed by `import io`,
        // `p = 'crates/…`, and so on. A label is one line by definition.
        return truncate(&first_line(command), 50);
    }
    if let Some(brief) = args.get("brief").and_then(Value::as_str) {
        return truncate(&first_line(brief), 40);
    }
    String::new()
}

/// Everything up to the first newline, with runs of whitespace collapsed to one
/// space, so a summary is always a single readable line.
fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::fake::Advanceable;
    use crate::events::fake::Recorder;
    use crate::machine::fake::{Script, Scripted as ScriptedMachine};
    use crate::model::fake::{tool_call, Asked, Gate, Scripted};
    use mush_core::{FunctionCall, ToolCall};
    use serde_json::json;
    use std::fs;
    use std::time::Instant;

    #[test]
    fn summarize_prefers_paths_then_commands_then_briefs() {
        assert_eq!(summarize(&json!({"path": "a.rs"})), "a.rs");
        assert_eq!(summarize(&json!({"command": "ls -la"})), "ls -la");
        assert_eq!(
            summarize(&json!({"brief": "fix the parser"})),
            "fix the parser"
        );
        assert_eq!(summarize(&json!({})), "");
    }

    /// A tool label is one line by definition. Truncating a command's raw text
    /// kept its newlines, so a heredoc turned one row into several.
    #[test]
    fn a_command_summary_collapses_to_one_line() {
        let label = summarize(&json!({
            "command": "cd /w && python3 - <<'PY'\nimport io\nprint('x')\nPY"
        }));
        assert!(!label.contains('\n'), "{label:?} must be one line");
        assert!(label.starts_with("cd /w && python3"), "{label}");
        assert_eq!(first_line("  a\n\n  b  c \n"), "a");
    }

    /// A nudge parked during a run that was cancelled is already in the UI's
    /// transcript, which echoes every human message. Adopting that transcript
    /// must not deliver the parked copy a second time.
    #[test]
    fn adopting_a_transcript_drops_parked_nudges() {
        let mut state = ActorState::default();
        let mut transcript = vec![Message::system("sys")];
        state
            .deferred
            .push(AgentMsg::Nudge("said once".to_string()));

        let carried = vec![Message::system("sys"), Message::user("said once")];
        assert!(matches!(
            absorb(&mut state, &mut transcript, AgentMsg::Run(carried)),
            Fold::Run
        ));
        assert_eq!(transcript.len(), 2, "the UI's transcript wins");
        assert!(state.deferred.is_empty(), "the parked copy is gone");

        // The next message boundary has nothing left to inject, or the model
        // would answer the same sentence twice.
        let (_tx, rx) = crossbeam_channel::unbounded();
        let mut messages = Vec::new();
        drain_mailbox(&rx, &AtomicBool::new(false), &mut messages, &mut state);
        assert!(messages.is_empty(), "no duplicate user message");
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: "{}".into(),
            },
        }
    }

    fn assistant_calling(ids: &[&str]) -> Message {
        let mut message = Message::assistant("working");
        message.tool_calls = Some(ids.iter().map(|id| call(id)).collect());
        message
    }

    fn roles(messages: &[Message]) -> Vec<&str> {
        messages
            .iter()
            .map(|message| message.role.as_str())
            .collect()
    }

    /// Stopped, finished and failed are three different things, and a parent
    /// that cannot tell them apart treats a stop as a result. Each gets its own
    /// mark: `✓` only ever means a run produced something.
    #[test]
    fn agent_status_distinguishes_stopped_from_done_and_failed() {
        let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let mut state = ActorState::default();
        state.children.insert(1, tx.clone());
        state.completed.insert(1, Outcome::Stopped);
        state.children.insert(2, tx.clone());
        state
            .completed
            .insert(2, Outcome::Finished("did the thing".into()));
        state.children.insert(3, tx);
        state
            .completed
            .insert(3, Outcome::Failed("no route".into()));

        let lines = status_tool(&state).unwrap();
        assert!(lines.contains("#1 ⊘ stopped"), "a stop is not a ✓: {lines}");
        assert!(lines.contains("#2 ✓ did the thing"), "{lines}");
        assert!(lines.contains("#3 ✗ no route"), "{lines}");
        // The old shape — a sentinel string leaking into the parent's view —
        // reported a stopped child as a *finished* one.
        assert!(!lines.contains("#1 ✓"), "{lines}");
        assert!(!lines.contains("cancelled"), "{lines}");
    }

    /// The line a parent reads must name the outcome. A stopped run has no
    /// result, and reporting one as `done` is how a lost agent passes for a
    /// finished one.
    #[test]
    fn only_a_finished_run_reports_itself_as_done() {
        assert_eq!(
            Outcome::Finished("wrote the parser".into()).line(3),
            "#3 done: wrote the parser"
        );
        let stopped = Outcome::Stopped.line(3);
        assert!(stopped.starts_with("#3 stopped"), "{stopped}");
        // It must not be *formatted* as a done line. (The words "not done" do
        // appear, deliberately: they are what tells the parent it is not one.)
        assert!(!stopped.starts_with("#3 done"), "{stopped}");
        let failed = Outcome::Failed("no route".into()).line(3);
        assert!(failed.starts_with("#3 failed"), "{failed}");
    }

    /// A stop is the human's doing, not news: it must not wake a napping parent
    /// into a fresh (paid) run. A finish is news and must wake it.
    #[test]
    fn a_stop_does_not_wake_a_napping_parent_but_a_finish_does() {
        let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let mut state = ActorState::default();
        state.children.insert(1, tx.clone());
        let mut messages = vec![Message::system("you are mush")];

        assert!(matches!(
            absorb(
                &mut state,
                &mut messages,
                AgentMsg::ChildDone {
                    id: 1,
                    outcome: Outcome::Stopped
                }
            ),
            Fold::Idle
        ));
        assert!(messages.last().unwrap().text().contains("stopped"));

        assert!(matches!(
            absorb(
                &mut state,
                &mut messages,
                AgentMsg::ChildDone {
                    id: 1,
                    outcome: Outcome::Finished("all done".into())
                }
            ),
            Fold::Run
        ));
    }

    /// The subject written for a commit and the subject read back from git must
    /// agree, or a worktree found on startup is shown as the wrong work.
    #[test]
    fn a_commit_subject_round_trips_through_git() {
        let cases = [
            (Outcome::Finished("done".into()), Committed::Finished),
            (Outcome::Stopped, Committed::Stopped),
            (
                Outcome::Failed("no route".into()),
                Committed::Failed("no route".into()),
            ),
        ];
        for (outcome, expected) in cases {
            let subject = commit_subject(7, "port the parser", &outcome);
            assert!(subject.starts_with("mush #7"), "{subject}");
            let (ended, brief) = parse_commit_subject(&subject)
                .unwrap_or_else(|| panic!("{subject} must parse back"));
            assert_eq!(ended, expected, "{subject}");
            assert_eq!(brief, "port the parser", "{subject}");
        }
    }

    /// A brief cut to a budget is cut by *columns*, not characters: a CJK brief
    /// counted by characters is twice as wide as the subject that holds it
    /// (finding B9). This module used to carry its own character-counting
    /// `truncate`, which is exactly how the two meanings drifted apart.
    #[test]
    fn a_brief_is_measured_in_columns() {
        use unicode_width::UnicodeWidthStr;
        let wide = "编码是这样的".repeat(20);
        let subject = commit_subject(7, &wide, &Outcome::Finished("done".into()));
        let brief = subject.strip_prefix("mush #7: ").expect("the prefix");
        assert!(
            UnicodeWidthStr::width(brief) <= 60,
            "a wide brief overshot its column budget: {brief:?}"
        );
        assert!(
            brief.ends_with('…'),
            "a cut brief says it was cut: {brief:?}"
        );
    }

    /// A branch the human committed to by hand is not evidence about an agent,
    /// so it parses as nothing rather than as a finished run.
    #[test]
    fn only_a_subject_mush_wrote_is_read_back() {
        assert_eq!(parse_commit_subject("fix the bug myself"), None);
        assert_eq!(parse_commit_subject("mush #3"), None);
        assert_eq!(parse_commit_subject("mush #3 (something else): x"), None);
        assert_eq!(
            parse_commit_subject("mush #3: "),
            Some((Committed::Finished, String::new()))
        );
    }

    /// A child that was stopped and then resumed finishes later; the stale
    /// `stopped` must not outlive the result, or the parent waits on a stop
    /// forever.
    #[test]
    fn a_later_finish_replaces_a_stale_stop() {
        let mut state = ActorState::default();
        note_completion(&mut state, 1, Outcome::Stopped);
        assert_eq!(state.completed.get(&1), Some(&Outcome::Stopped));
        let line = note_completion(&mut state, 1, Outcome::Finished("done now".into()));
        assert_eq!(
            state.completed.get(&1),
            Some(&Outcome::Finished("done now".into()))
        );
        assert_eq!(line, "#1 done: done now");
    }

    /// The repair must be wired into the one path that adopts a transcript
    /// wholesale, or it never runs where it matters.
    #[test]
    fn adopting_a_ui_transcript_repairs_tool_pairs() {
        let (actor, _mailbox) = test_actor("adopt");
        let mut state = ActorState::default();
        let mut messages = Vec::new();
        let fresh = vec![
            Message::system("you are mush"),
            Message::user("task"),
            assistant_calling(&["a"]),
            Message::user("steer"),
            Message::tool("a", "result"),
        ];
        let folded = absorb(&mut state, &mut messages, AgentMsg::Run(fresh));
        assert!(matches!(folded, Fold::Run));
        assert_eq!(
            roles(&messages),
            ["system", "user", "assistant", "tool", "user"]
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A completion that the incoming transcript already carries (as a
    /// `wait_agents` result, say) is not news: announcing it again would spend
    /// a turn repeating what the model just read.
    #[test]
    fn adopting_a_transcript_that_announces_a_completion_keeps_it_delivered() {
        let (actor, _mailbox) = test_actor("delivered-yes");
        let mut state = ActorState::default();
        let mut messages = Vec::new();
        state
            .completed
            .insert(1, Outcome::Finished("did the thing".into()));
        state.delivered.insert(1);
        let fresh = vec![
            Message::system("you are mush"),
            Message::user("task"),
            assistant_calling(&["a"]),
            Message::tool("a", "#1 done: did the thing"),
        ];

        assert!(matches!(
            absorb(&mut state, &mut messages, AgentMsg::Run(fresh)),
            Fold::Run
        ));
        assert!(
            state.delivered.contains(&1),
            "the model reads it in the transcript, so it is already delivered"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A background job inherits the command's output file, so leaving one
    /// behind must not hold the tool (and the agent) hostage.
    ///
    /// This one stays a real `sh`. It is the *reason* the output goes to files
    /// rather than pipes — a pipe is only complete once every holder exits — and
    /// a scripted machine cannot demonstrate that, because a scripted job holds
    /// nothing. It is bounded: the command returns at once, and the `sleep 30`
    /// it leaves behind dies with the process group when the scratch files are
    /// read.
    #[test]
    fn a_background_job_does_not_hold_the_tool_hostage() {
        let (actor, _mailbox) = test_actor("background");
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let report = run_shell(
            "sleep 30 & echo started",
            &std::env::temp_dir(),
            Duration::from_secs(10),
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();
        assert!(report.contains("started"), "{report}");
        assert!(report.contains("[exit 0]"), "{report}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The timeout is a real bound, and the report says what happened.
    ///
    /// Both halves are scripted: the command never exits, and the clock moves
    /// only when the wait asks it to. The assertions are about the clock — the
    /// deadline is what stopped the command, and the command was killed rather
    /// than left behind — so proving a five-second timeout no longer costs five
    /// seconds and a real `sleep`.
    #[test]
    fn a_command_that_runs_forever_is_killed_on_time() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("timeout", machine.clone(), clock.clone());
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let timeout = Duration::from_secs(5);
        let started = Instant::now();
        let report = run_shell(
            "sleep 30",
            &std::env::temp_dir(),
            timeout,
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();

        assert!(report.contains("[timed out after 5s]"), "{report}");
        assert!(
            clock.elapsed() >= timeout,
            "the deadline is what stopped it, not the end of the command: {:?}",
            clock.elapsed()
        );
        assert_eq!(
            machine.kills(),
            1,
            "and the command was killed, not left running"
        );
        assert_eq!(machine.spawned(), vec!["sleep 30".to_string()]);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the deadline was reached without waiting for it: {:?}",
            started.elapsed()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A command that ends on its own is reported, not killed: what it said on
    /// both streams, and the code it exited with. The ordinary path, driven by
    /// a script instead of by `sh`.
    #[test]
    fn a_command_that_ends_reports_its_output_and_its_exit_code() {
        let machine = Arc::new(
            ScriptedMachine::new().runs(Script::exits(3).says("on stdout").complains("on stderr")),
        );
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("exits", machine.clone(), clock.clone());
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);

        let report = run_shell(
            "false",
            &std::env::temp_dir(),
            Duration::from_secs(5),
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();

        assert_eq!(
            report, "on stdout\n--- stderr ---\non stderr\n[exit 3]",
            "both streams, then how it ended"
        );
        assert_eq!(machine.kills(), 0, "nothing to kill: it had finished");
        assert_eq!(
            clock.elapsed(),
            Duration::ZERO,
            "and nothing was waited for"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// Ctrl-C reaches a command that is still running; the tool returns at once
    /// and says why. The command hangs, the flag is already set, and the report
    /// is the only thing the model ever sees of it.
    #[test]
    fn a_running_command_can_be_cancelled() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("cancel", machine.clone(), clock.clone());
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(true);
        let started = Instant::now();
        let report = run_shell(
            "sleep 30",
            &std::env::temp_dir(),
            Duration::from_secs(30),
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();

        assert!(report.contains("[cancelled]"), "{report}");
        assert_eq!(machine.kills(), 1, "the command is stopped, not orphaned");
        assert_eq!(
            clock.elapsed(),
            Duration::ZERO,
            "a cancel is not a timeout: no time had to pass"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A Stop that arrives while a command runs is noticed by the command, not
    /// left for the end of the batch.
    #[test]
    fn a_stop_in_the_mailbox_interrupts_a_running_command() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let clock = Arc::new(Advanceable::new());
        let (actor, mailbox) = scripted_tools_actor("stop-command", machine.clone(), clock.clone());
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        mailbox.send(AgentMsg::Stop).unwrap();

        let report = run_shell(
            "echo starting; sleep 30",
            &std::env::temp_dir(),
            Duration::from_secs(30),
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();

        assert!(report.contains("[cancelled]"), "{report}");
        assert!(cancel.load(Ordering::SeqCst), "the Stop set the flag");
        assert_eq!(machine.kills(), 1);
        assert_eq!(
            clock.elapsed(),
            Duration::ZERO,
            "the command never reached its own timeout"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A command that writes without end is stopped at the disk limit rather
    /// than filling the filesystem — the model only ever sees the first chunk.
    /// The writer is scripted: one megabyte a poll, forever, which reaches the
    /// eight-megabyte limit in nine polls with no `yes`, no disk and no race.
    #[test]
    fn a_runaway_writer_is_stopped_at_the_output_limit() {
        let machine = Arc::new(
            ScriptedMachine::new().runs(Script::hangs().says("mush\n").writes_without_end(1 << 20)),
        );
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("runaway", machine.clone(), clock.clone());
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let report = run_shell(
            "yes mush",
            &std::env::temp_dir(),
            Duration::from_secs(30),
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();

        assert!(report.contains("output passed"), "{report}");
        assert_eq!(machine.kills(), 1, "the runaway writer was killed");
        assert!(report.len() < CMD_CAP * 2, "report grew: {}", report.len());
        assert!(
            clock.elapsed() < Duration::from_secs(30),
            "bytes stopped it, not the command's own timeout: {:?}",
            clock.elapsed()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// Output longer than the cap is truncated and marked, and the command
    /// still finishes (nothing blocks on a full pipe).
    ///
    /// This one stays a real `sh` too: `yes` into `head` is what a real,
    /// bounded command writing past `CMD_CAP` looks like, and what it proves is
    /// the *real* scratch-file read — the fake only ever hands back a string.
    #[test]
    fn long_output_is_capped_and_marked() {
        let (actor, _mailbox) = test_actor("long-output");
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let report = run_shell(
            "yes mush | head -c 40000",
            &std::env::temp_dir(),
            Duration::from_secs(10),
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();
        assert!(
            report.contains("[mush: output truncated]"),
            "cap was not marked"
        );
        assert!(
            report.len() < CMD_CAP * 2,
            "report grew past the cap: {}",
            report.len()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A standalone actor over a scratch workspace, for exercising the mailbox
    /// plumbing with no model, no UI, and no threads.
    /// Several edits to the *same* file in one batch must all land: every file
    /// tool re-reads from disk, so the second edit sees the first one's result
    /// instead of clobbering it with a stale copy.
    #[test]
    fn several_edits_to_one_file_in_a_batch_all_land() {
        let (actor, _mailbox) = test_actor("multi-edit");
        let cfg = Config::new("http://127.0.0.1:1", "test", None);
        fs::write(actor.ws.root().join("f.rs"), "let a = 1;\nlet b = 2;\n").unwrap();

        // Exactly what a batch of three `edit_file` calls does, in order.
        for (old, new) in [
            ("let a = 1;", "let a = 10;"),
            ("let b = 2;", "let b = 20;"),
            ("let b = 20;", "let b = 21;"),
        ] {
            direct_tool(
                &actor.ws,
                ToolName::EditFile,
                &json!({ "path": "f.rs", "old_string": old, "new_string": new }),
                &cfg,
            )
            .unwrap();
        }

        assert_eq!(
            fs::read_to_string(actor.ws.root().join("f.rs")).unwrap(),
            "let a = 10;\nlet b = 21;\n"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// An ambiguous `old_string` is refused rather than guessed at, which is
    /// what makes a repeated pattern (a rename) need context each time.
    #[test]
    fn an_ambiguous_edit_is_refused_not_guessed() {
        let (actor, _mailbox) = test_actor("ambiguous");
        let cfg = Config::new("http://127.0.0.1:1", "test", None);
        fs::write(actor.ws.root().join("f.rs"), "x = 1;\nx = 2;\n").unwrap();

        let error = direct_tool(
            &actor.ws,
            ToolName::EditFile,
            &json!({ "path": "f.rs", "old_string": "x = ", "new_string": "y = " }),
            &cfg,
        )
        .unwrap_err();
        assert!(error.contains("2 times"), "{error}");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    fn test_actor(label: &str) -> (Actor, Sender<AgentMsg>) {
        let cfg = test_cfg();
        let (actor, _events, mailbox) =
            build_actor(label, Arc::new(HttpModel::new(cfg.clone())), cfg);
        (actor, mailbox)
    }

    /// The same actor, with its model calls served by a script instead of a
    /// socket — so a whole run can be driven in process, with no server.
    fn scripted_actor(
        label: &str,
        model: &Arc<Scripted>,
    ) -> (Actor, Arc<Recorder>, Sender<AgentMsg>) {
        build_actor(label, model.clone(), test_cfg())
    }

    /// A scratch config cell. The endpoint is deliberately unreachable: every
    /// test that uses it must go through a scripted model.
    fn test_cfg() -> Arc<Mutex<Config>> {
        Arc::new(Mutex::new(Config::new("http://127.0.0.1:1", "test", None)))
    }

    /// A standalone actor over a scratch workspace, with `model` as its client
    /// and `cfg` as the tree's shared configuration. Its events go to a
    /// recording sink, which comes back so a test can read what the run said.
    fn build_actor(
        label: &str,
        model: Arc<dyn ModelClient>,
        cfg: Arc<Mutex<Config>>,
    ) -> (Actor, Arc<Recorder>, Sender<AgentMsg>) {
        build_actor_about(label, model, cfg, Arc::new(Shell), Arc::new(clock::System))
    }

    /// A standalone actor over a scratch workspace whose shells are scripted
    /// and whose clock only moves when the test says so: how a command ends and
    /// how long time takes are both facts the test writes down, so a timeout or
    /// an output cap costs neither a subprocess nor a wait.
    fn scripted_tools_actor(
        label: &str,
        machine: Arc<dyn Machine>,
        clock: Arc<dyn clock::Clock>,
    ) -> (Actor, Sender<AgentMsg>) {
        let cfg = test_cfg();
        let (actor, _events, mailbox) = build_actor_about(
            label,
            Arc::new(HttpModel::new(cfg.clone())),
            cfg,
            machine,
            clock,
        );
        (actor, mailbox)
    }

    /// The same, naming the machine and the clock it runs on. The endpoint in
    /// the config is a port nothing listens on, so a test that reaches the
    /// model at all fails loudly instead of using a socket.
    fn build_actor_about(
        label: &str,
        model: Arc<dyn ModelClient>,
        cfg: Arc<Mutex<Config>>,
        machine: Arc<dyn Machine>,
        clock: Arc<dyn clock::Clock>,
    ) -> (Actor, Arc<Recorder>, Sender<AgentMsg>) {
        let root = std::env::temp_dir().join(format!("mush-actor-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let recorder = Recorder::new();
        let ctx = Arc::new(AgentCtx {
            cfg,
            model,
            events: recorder.clone(),
            machine,
            clock,
            root: root.clone(),
            ids: Arc::new(AtomicU64::new(1)),
            live: Arc::new(AtomicU64::new(0)),
        });
        let (my_tx, rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let (dead_tx, dead_rx) = crossbeam_channel::unbounded::<AgentMsg>();
        drop(dead_rx);
        let actor = Actor {
            ctx,
            id: 7,
            depth: 0,
            ws: Workspace::new(&root).unwrap(),
            branch: None,
            brief: String::new(),
            my_tx: my_tx.clone(),
            parent_tx: dead_tx,
            rx,
        };
        (actor, recorder, my_tx)
    }

    /// The root napping on `wait_agents` must hear the human. Parking their
    /// words is not enough when the wait can last the whole timeout: the model
    /// would not see them until the child it was waiting on finished, which is
    /// the opposite of steering. The wait ends, and the words stay parked so
    /// the next message boundary folds them in.
    #[test]
    fn a_human_message_ends_a_wait_on_children() {
        let (actor, mailbox) = test_actor("wake-wait");
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        // A child that exists and has not finished: the state a parent is in
        // for the whole of a long wait.
        let (child, _child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child);
        state.running.insert(1);

        mailbox
            .send(AgentMsg::Nudge("what about the tests?".into()))
            .unwrap();
        let started = Instant::now();
        let result = wait_tool(&actor, &mut state, &cancel, &json!({ "timeout": 5 })).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the message ends the wait, not the 600 s timeout ({:?})",
            started.elapsed()
        );
        assert!(result.contains("interrupted"), "{result}");
        assert!(
            result.contains("still running"),
            "the model is told the wait was cut short, not that the child finished: {result}"
        );
        // Not eaten by the interruption: the boundary that follows folds the
        // words in, which is what makes the model answer them in this run.
        assert!(
            matches!(state.deferred.first(), Some(AgentMsg::Nudge(text)) if text == "what about the tests?"),
            "the message must survive the interrupted wait: {:?}",
            state.deferred.len()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The other shape a human message arrives in: the UI believed the agent
    /// was idle and sent the whole transcript, whose last message is what the
    /// human just typed. That must end a blocking wait too — an actor that only
    /// listened for `Nudge` would sit here until the child finished.
    #[test]
    fn a_wait_that_no_child_ends_times_out_on_the_clock() {
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) =
            scripted_tools_actor("wait-timeout", Arc::new(Shell), clock.clone());
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        // A child that exists and never finishes: the state a parent is in for
        // the whole of a long wait.
        let (child, _child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child);
        state.running.insert(1);

        let started = Instant::now();
        let result = wait_tool(&actor, &mut state, &cancel, &json!({ "timeout": 600 })).unwrap();

        assert!(result.contains("wait timed out"), "{result}");
        assert!(
            clock.elapsed() >= Duration::from_secs(600),
            "the deadline is what ended the wait: {:?}",
            clock.elapsed()
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "and it was reached without waiting for it: {:?}",
            started.elapsed()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The other shape a human message arrives in: the UI believed the agent
    /// was idle and sent the whole transcript, whose last message is what the
    /// human just typed. That must end a blocking wait too — an actor that only
    /// listened for `Nudge` would sit here until the child finished.
    #[test]
    fn a_transcript_sent_as_a_message_also_ends_a_wait() {
        let (actor, mailbox) = test_actor("wake-wait-run");
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let (child, _child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child);

        let transcript = vec![
            Message::system("you are mush"),
            Message::user("carry on without me"),
        ];
        mailbox.send(AgentMsg::Run(transcript)).unwrap();
        let started = Instant::now();
        let result = wait_tool(&actor, &mut state, &cancel, &json!({ "timeout": 5 })).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a newer transcript ends the wait ({:?})",
            started.elapsed()
        );
        assert!(result.contains("interrupted"), "{result}");

        // A `Run` whose last message is not the human's (a pure re-sync) is
        // not a reason to stop waiting: nothing was said.
        let mut quiet = ActorState::default();
        quiet
            .deferred
            .push(AgentMsg::Run(vec![Message::assistant("hm")]));
        assert!(!parked_message(&quiet));
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The same rule, end to end: the root delegates, parks in `wait_agents`,
    /// the human types, and the model answers their words in that run — while
    /// the child is still working, not after it finishes.
    ///
    /// The child is held inside a real shell command that waits for a file the
    /// test only writes at the very end, so "the answer came back while the
    /// child still ran" is proven by that file's absence rather than by a race.
    #[test]
    fn a_human_message_reaches_a_root_napping_on_wait_agents() {
        let root = std::env::temp_dir().join(format!("mush-wake-e2e-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let gate = root.join("open-the-gate");
        let block = format!(
            "while [ ! -f {} ]; do sleep 0.05; done; echo released",
            gate.display()
        );

        let scripted = Arc::new(
            Scripted::new()
                // The root: delegate, then wait on the child for a long time.
                .when(|asked: &Asked| asked.depth().is_none() && !asked.saw("spawned agent"))
                .calls(vec![tool_call(
                    "c0",
                    "spawn_agent",
                    json!({ "brief": "hold the gate until the test opens it" }),
                )])
                .when(|asked: &Asked| asked.depth().is_none() && !asked.saw("what about the tests"))
                .calls(vec![tool_call(
                    "c1",
                    "wait_agents",
                    json!({ "timeout": 60 }),
                )])
                // Woken by the child's own result, after the human was served.
                .when(|asked: &Asked| asked.depth().is_none() && asked.saw("#1 done"))
                .says("thanks — carrying on")
                // Only reachable if the human's words arrived during this run.
                .when(|asked: &Asked| asked.depth().is_none())
                .says("answered the human")
                // The child: really block, then report.
                .when(|asked: &Asked| asked.depth() == Some(1) && !asked.saw("released"))
                .calls(vec![tool_call(
                    "k0",
                    "run_command",
                    json!({ "command": block }),
                )])
                .when(|asked: &Asked| asked.depth() == Some(1))
                .says("gate opened"),
        );
        let events = Recorder::new();
        let root_tx = spawn_scripted(
            Config::new("http://127.0.0.1:1", "scripted", None),
            events.clone(),
            root.clone(),
            scripted.clone(),
        )
        .tx;
        root_tx
            .send(AgentMsg::Run(vec![
                Message::system(prompt::system_prompt(root.to_str().unwrap())),
                Message::user("delegate, then wait for it".to_string()),
            ]))
            .unwrap();

        // The wait is in flight once the root's second request carries the
        // spawn result — the request whose answer calls `wait_agents`.
        let parked = |needle: &str| {
            let asked = scripted.asked();
            asked
                .iter()
                .any(|ask| ask.depth().is_none() && ask.saw(needle))
        };
        let deadline = Instant::now() + WAIT;
        while !parked("spawned agent") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            parked("spawned agent"),
            "the root never asked for the wait: {:?}",
            scripted.asked().len()
        );

        let started = Instant::now();
        root_tx
            .send(AgentMsg::Nudge("what about the tests?".into()))
            .unwrap();

        let deadline = Instant::now() + WAIT;
        while !parked("what about the tests") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            parked("what about the tests"),
            "a message must reach the model during the run, not at the wait's end: {:?}",
            scripted
                .asked()
                .iter()
                .map(|ask| (ask.depth(), ask.messages.len()))
                .collect::<Vec<_>>()
        );
        assert!(
            !gate.exists(),
            "the child was still blocked in its command when the human was answered"
        );
        assert!(
            started.elapsed() < WAIT,
            "answered promptly, not at the 60 s wait ({:?})",
            started.elapsed()
        );

        // The human's words are what the model answered, in this run.
        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 1),
            "the interrupted run must finish so the answer is delivered: {seen:?}"
        );
        assert_eq!(seen.errors, Vec::<String>::new());

        // Let the child go, then take the tree down.
        fs::write(&gate, "go").unwrap();
        let _ = root_tx.send(AgentMsg::Shutdown);
        let _ = fs::remove_dir_all(&root);
    }

    /// A reply cut off at the token cap is not a result, but it is usually a
    /// *too big* answer rather than a broken model (a whole file in one
    /// `write_file`, or a long reasoning pass). The run must not die on it: it
    /// answers the dangling calls, asks for smaller steps, and carries on.
    #[test]
    fn a_cut_off_reply_is_answered_with_smaller_steps() {
        let scripted = Arc::new(
            Scripted::new()
                // The first reply is cut off mid-tool-call: the half-written
                // call it started must never run.
                .cut_off_call(
                    "write_file",
                    "{\"path\": \"big.rs\", \"content\": \"fn main(",
                )
                // The model then does as it was told, in smaller pieces.
                .calls(vec![tool_call(
                    "c1",
                    "write_file",
                    json!({ "path": "big.rs", "content": "fn main() {}\n" }),
                )])
                .says("wrote it in one small piece"),
        );
        let (actor, rx, mailbox) = scripted_actor("cut-off", &scripted);
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("write big.rs"),
        ];

        let result = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();
        assert_eq!(result.as_deref(), Some("wrote it in one small piece"));

        // The cut-off call never ran, and the model was told to go smaller.
        let asked = scripted.asked();
        assert_eq!(asked.len(), 3, "cut-off, then the work, then the answer");
        assert!(
            asked[0]
                .messages
                .iter()
                .all(|m| !m.text().contains("content") || m.role != "tool"),
            "nothing from the cut-off reply reaches the model as a result"
        );
        assert!(
            asked[1]
                .messages
                .iter()
                .any(|m| m.text().contains("cut off by the endpoint's length limit")),
            "the model is told why, and how to fix it"
        );
        assert!(
            asked[1]
                .messages
                .iter()
                .any(|m| m.role == "tool" && m.text().contains("was not run")),
            "the half-written call is answered, never run"
        );
        // The file the model *did* write in one piece is on disk.
        assert_eq!(
            fs::read_to_string(actor.ws.root().join("big.rs")).unwrap(),
            "fn main() {}\n"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
        let _ = rx;
        let _ = mailbox;
    }

    /// A model that keeps answering too big has to end the run: the bounded
    /// retry is a kindness, not an infinite loop.
    #[test]
    fn a_model_that_never_writes_small_enough_still_fails() {
        let mut scripted = Scripted::new();
        for _ in 0..=TRUNCATION_ROUNDS {
            scripted = scripted.cut_off("still writing the whole world");
        }
        let scripted = Arc::new(scripted);
        let (actor, _rx, _mailbox) = scripted_actor("always-cut", &scripted);
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let mut messages = vec![Message::user("write everything")];

        let error = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap_err();
        assert!(error.contains("cut off"), "{error}");
        assert!(error.contains("in a row"), "{error}");
        assert_eq!(scripted.asked().len(), TRUNCATION_ROUNDS + 1);
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// One reply may never be asked for more than a share of the window: an
    /// endpoint cannot deliver what it does not have, and a thinking model
    /// spends the cap before it reaches the answer.
    #[test]
    fn a_reply_never_asks_for_more_than_the_window_has() {
        let window = |tokens: usize| {
            let mut cfg = Config::new("http://127.0.0.1:1", "m", None);
            cfg.set_context(tokens);
            cfg
        };
        assert_eq!(reply_cap(&window(8_192)), 2_048, "a quarter of 8k, not 20k");
        assert_eq!(
            reply_cap(&window(128_000)),
            MAX_REPLY_TOKENS,
            "a big window keeps the cap"
        );
        // Never zero, however tiny the window: a request for no reply is not a
        // request.
        assert_eq!(reply_cap(&window(512)), 1_024);
    }

    /// The bug this guards: re-queuing a parked nudge into the actor's own
    /// mailbox spins forever, because `my_tx` *is* the queue being drained.
    #[test]
    fn drain_signals_parks_nudges_for_the_next_boundary() {
        let (actor, mailbox) = test_actor("signals");
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        mailbox.send(AgentMsg::Nudge("steer left".into())).unwrap();
        mailbox.send(AgentMsg::Stop).unwrap();

        drain_signals(&actor, &cancel, &mut state);
        assert!(cancel.load(Ordering::SeqCst), "a Stop is honoured at once");
        assert_eq!(state.deferred.len(), 1, "the nudge is parked, not dropped");
        assert!(
            actor.rx.try_recv().is_err(),
            "nothing may be put back in the queue we are draining"
        );

        // The next message boundary folds it in, in order.
        let mut messages = vec![Message::assistant("working")];
        drain_mailbox(&actor.rx, &cancel, &mut messages, &mut state);
        assert!(state.deferred.is_empty());
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].text(), "steer left");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// `Stop` cancels work and is a no-op for an idle agent; only `Shutdown`
    /// ends one — which is what keeps `/new` from leaving an orphan root
    /// behind that still answers to agent #0.
    #[test]
    fn stop_cancels_but_shutdown_ends() {
        let (actor, mailbox) = test_actor("shutdown");
        let mut state = ActorState::default();
        let mut messages = vec![Message::system("you are mush"), Message::user("task")];

        mailbox.send(AgentMsg::Stop).unwrap();
        let cancel = AtomicBool::new(false);
        drain_mailbox(&actor.rx, &cancel, &mut messages, &mut state);
        assert!(cancel.load(Ordering::SeqCst), "a Stop cancels the run");
        assert!(!state.shutdown, "a Stop must not end the actor");

        mailbox.send(AgentMsg::Shutdown).unwrap();
        drain_mailbox(&actor.rx, &cancel, &mut messages, &mut state);
        assert!(
            state.shutdown,
            "a Shutdown ends the actor once the run stops"
        );

        // Idle: the same split, expressed as what the actor should do next.
        let mut state = ActorState::default();
        assert!(matches!(
            absorb(&mut state, &mut messages, AgentMsg::Stop),
            Fold::Idle
        ));
        assert!(matches!(
            absorb(&mut state, &mut messages, AgentMsg::Shutdown),
            Fold::End
        ));
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A Stop that arrives *behind* the work it was aimed at must not be
    /// swallowed. The blocking wait folds a Stop away while the actor is idle —
    /// there is no work to cancel — but once a `Run` has been read, a Stop that
    /// follows it was aimed at the run about to start, and nothing later in the
    /// run will ever see it: the flag is created at the start, so the human's
    /// Ctrl-C did nothing at all and the row sat at `⊘` until the run ended on
    /// its own (finding B6).
    #[test]
    fn a_stop_behind_the_run_it_was_aimed_at_is_not_swallowed() {
        let (actor, mailbox) = test_actor("stop-behind-run");
        let mut state = ActorState::default();
        let mut transcript = vec![Message::system("you are mush")];

        mailbox
            .send(AgentMsg::Run(vec![
                Message::system("you are mush"),
                Message::user("do the work"),
            ]))
            .unwrap();
        mailbox.send(AgentMsg::Stop).unwrap();

        assert!(
            wait_for_work(&actor, &mut state, &mut transcript, false),
            "there is a run to start"
        );
        assert_eq!(transcript.len(), 2, "and its task is folded in");
        assert!(
            run_cancel(&mut state).load(Ordering::SeqCst),
            "the run is born cancelled, so the Stop lands where it was aimed"
        );

        // A Stop with no work in front of it stays a no-op: an agent the human
        // stopped while it was idle must still run when they later ask it to.
        let mut state = ActorState::default();
        assert!(matches!(
            absorb(&mut state, &mut transcript, AgentMsg::Stop),
            Fold::Idle
        ));
        assert!(
            !run_cancel(&mut state).load(Ordering::SeqCst),
            "a Stop folded away while idle cancels nothing"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A completion that the model has not read yet must be delivered when the
    /// UI's transcript replaces the actor's, or the result is lost for good.
    #[test]
    fn replacing_the_transcript_puts_completions_back_on_the_delivery_list() {
        let (actor, _mailbox) = test_actor("deliver");
        let mut state = ActorState::default();
        let mut messages = vec![Message::system("you are mush"), Message::user("task")];
        state
            .completed
            .insert(1, Outcome::Finished("did the thing".into()));
        state.delivered.insert(1);

        let fresh = vec![Message::system("you are mush"), Message::user("carry on")];
        assert!(matches!(
            absorb(&mut state, &mut messages, AgentMsg::Run(fresh)),
            Fold::Run
        ));
        assert!(
            !state.delivered.contains(&1),
            "the new transcript has no #1 done line, so it is undelivered again"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A whole run, in process: the model asks for a tool, the tool runs, and
    /// the model's answer ends the run. The `ModelClient` seam is what makes
    /// this possible with no server, no port and no thread.
    #[test]
    fn a_scripted_run_runs_its_tool_call_and_ends_with_the_answer() {
        let model = Arc::new(
            Scripted::new()
                .calls(vec![tool_call(
                    "call_1",
                    "write_file",
                    json!({ "path": "note.txt", "content": "hello" }),
                )])
                .says("wrote note.txt"),
        );
        let (actor, _events, _mailbox) = scripted_actor("scripted-run", &model);
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("write note.txt"),
        ];

        let result = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();

        assert_eq!(result.as_deref(), Some("wrote note.txt"));
        assert_eq!(
            fs::read_to_string(actor.ws.root().join("note.txt")).unwrap(),
            "hello",
            "the tool the model asked for must actually run"
        );
        assert_eq!(
            messages.last().unwrap().text(),
            "wrote note.txt",
            "the transcript ends with the answer"
        );

        // Two turns, two asks — and the second one carried the tool result.
        let asked = model.asked();
        assert_eq!(asked.len(), 2);
        assert_eq!(asked[0].model, "test");
        assert!(asked[0].tools > 0, "the first turn offered the tools");
        let carried = asked[1].messages.last().unwrap();
        assert_eq!(carried.role, "tool");
        assert_eq!(carried.text(), "wrote note.txt");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The learned-context retry, in process: the endpoint refuses the request
    /// as past its window, the run adopts the window the endpoint named, tells
    /// the UI, and asks again — where before this seam the only way to see that
    /// was a mock server on a fixed port.
    #[test]
    fn a_context_complaint_teaches_the_window_and_the_run_asks_again() {
        let model = Arc::new(
            Scripted::new()
                .fails_with(
                    400,
                    r#"{"error":{"message":"This model's maximum context length is 4096 tokens"}}"#,
                )
                .says("done"),
        );
        let (actor, events, _mailbox) = scripted_actor("learned-context", &model);
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let mut messages = vec![Message::system("you are mush"), Message::user("task")];

        let result = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();

        assert_eq!(result.as_deref(), Some("done"));
        assert_eq!(model.asked().len(), 2, "the run re-asked after learning");
        assert_eq!(
            actor.ctx.cfg.lock().unwrap().context_tokens,
            4_096,
            "the learned window reaches the shared config"
        );
        assert_eq!(contexts(&events), vec![4_096], "and the UI is told");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A cancellation mid-reply is a stop: the client sets the flag the reader
    /// polls, exactly as the real one does, and the run ends cancelled with no
    /// turn taken — not as a failure to reach the endpoint.
    #[test]
    fn a_scripted_cancellation_stops_the_run_without_a_turn() {
        let model = Arc::new(Scripted::new().cancels());
        let (actor, _events, _mailbox) = scripted_actor("cancelled", &model);
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let mut messages = vec![Message::system("you are mush"), Message::user("task")];

        let error = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap_err();

        assert_eq!(error, CANCELLED);
        assert!(cancel.load(Ordering::SeqCst), "the flag is set too");
        assert_eq!(messages.len(), 2, "a cancelled reply is not a turn");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The acknowledgement itself, through a live actor: a run that a Stop ends
    /// is reported as `Stopped` — once, and not as `Done` and not as an error.
    /// That event is what takes the tree's row out of `⊘ cancelling…` the moment
    /// the cancel lands, and it is the difference between a cancel that landed
    /// and one that never will (finding B6). The scripted client answers the way
    /// the reader does when the flag is set, so what this pins is the actor's
    /// half of that: how a cancelled run is *reported*.
    #[test]
    fn a_run_that_a_stop_ends_is_acknowledged_as_stopped() {
        let model = Arc::new(Scripted::new().cancels());
        let events = Recorder::new();
        let root = scratch_dir("stop-ack");
        let root_tx = spawn_scripted(
            Config::new("http://127.0.0.1:1", "scripted", None),
            events.clone(),
            root.clone(),
            model,
        )
        .tx;
        root_tx
            .send(AgentMsg::Run(vec![
                Message::system("you are mush"),
                Message::user("work"),
            ]))
            .unwrap();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.stopped > 0
                || !seen.errors.is_empty()),
            "the run must be acknowledged: {seen:?}"
        );
        assert_eq!(
            seen.stopped, 1,
            "reported as stopped, exactly once: {seen:?}"
        );
        assert_eq!(seen.done, 0, "a stopped run is not a finished one");
        assert!(seen.errors.is_empty(), "and not a failure: {seen:?}");
        let _ = fs::remove_dir_all(&root);
    }

    /// Every context window a run announced to the UI.
    fn contexts(events: &Recorder) -> Vec<usize> {
        events
            .events()
            .into_iter()
            .filter_map(|(_, event)| match event {
                AgentEvent::Context { tokens } => Some(tokens),
                _ => None,
            })
            .collect()
    }

    /// The seam the orchestration scenarios run on: a tree spawned over a
    /// scripted client asks *it*. The endpoint in the config is a port nothing
    /// listens on, so a run that finished cannot have used one — which is what
    /// keeps these tests off the socket, off `python3` and off the clock.
    #[test]
    fn a_spawned_tree_asks_the_scripted_model() {
        let root = scratch_dir("scripted-tree");
        let scripted = Arc::new(Scripted::new().says("nothing to do"));
        let events = Recorder::new();
        let root_tx = spawn_scripted(
            Config::new("http://127.0.0.1:1", "scripted", None),
            events.clone(),
            root.clone(),
            scripted.clone(),
        )
        .tx;
        root_tx
            .send(AgentMsg::Run(vec![
                Message::system("you are mush"),
                Message::user("say something".to_string()),
            ]))
            .unwrap();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done > 0
                || !seen.errors.is_empty()),
            "the run must end: {seen:?}"
        );
        assert_eq!(seen.errors, Vec::<String>::new());
        assert_eq!(seen.replies, vec!["nothing to do"]);
        assert_eq!(
            scripted.asked().len(),
            1,
            "the tree's one turn must have gone to the scripted client"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The full orchestration path, headless: the root spawns an isolated
    /// child, the child writes into its own worktree and its completion wakes
    /// the parent. The model is scripted; the work — the git worktree, the
    /// file, the commit, the merge — is real.
    #[test]
    fn isolated_subagent_writes_its_worktree() {
        let root = init_git_repo("iso");
        // The child's first reply is held until the root's turn has ended, so
        // "the parent was woken by its child" is the only way this run can
        // finish — not a race the test happens to win.
        let gate = Arc::new(Gate::new());
        let scripted = Arc::new(
            Scripted::new()
                .when(|asked: &Asked| asked.depth() == Some(1) && !asked.saw("wrote iso.txt"))
                .held(gate.clone())
                .calls(vec![tool_call(
                    "c1",
                    "write_file",
                    json!({ "path": "iso.txt", "content": "isolated work" }),
                )])
                .when(|asked: &Asked| asked.depth() == Some(1))
                .says("created iso.txt in my worktree")
                .when(|asked: &Asked| asked.saw("#1 done"))
                .says("child finished")
                .when(|asked: &Asked| asked.saw("spawned agent"))
                .says("child left running — I will handle its result when it finishes")
                // The root's first turn: delegate and let the child work.
                .calls(vec![tool_call(
                    "c0",
                    "spawn_agent",
                    json!({
                        "brief": "create a file called iso.txt containing exactly: isolated work",
                        "isolated": true
                    }),
                )]),
        );
        let events = Recorder::new();
        let root_tx = spawn_scripted(
            Config::new("http://127.0.0.1:1", "scripted", None),
            events.clone(),
            root.clone(),
            scripted.clone(),
        )
        .tx;
        root_tx
            .send(AgentMsg::Run(vec![
                Message::system(prompt::system_prompt(root.to_str().unwrap())),
                Message::user("delegate: create iso.txt via an isolated subagent".to_string()),
            ]))
            .unwrap();

        let mut seen = Watched::default();
        assert!(
            gate.wait_until_asked(WAIT),
            "the child never asked for its first turn"
        );
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 1),
            "the root's first turn must end while the child still runs: {seen:?}"
        );
        gate.release();
        // The child finishes, and its completion wakes the root into a second
        // run: 3 Done events, root and child and woken root.
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 3),
            "the child, then the woken root, must each finish: {seen:?}"
        );
        assert_eq!(seen.done, 3, "no other run may happen: {seen:?}");
        assert_eq!(seen.errors, Vec::<String>::new());

        // The isolated child worked in `.mush/wt/1`, not the main root.
        assert_eq!(
            fs::read_to_string(root.join(".mush/wt/1/iso.txt"))
                .ok()
                .as_deref(),
            Some("isolated work")
        );
        // The run's end commits the worktree, so the branch mush advertises for
        // the child (and tells the human to diff and merge) carries the file.
        let branch_files =
            git::run(&root, &["diff", "--name-only", "HEAD...mush/1"]).unwrap_or_default();
        assert!(
            branch_files.contains("iso.txt"),
            "the branch must carry the child's work, got {branch_files:?}"
        );
        // …and nothing is left behind as an uncommitted change.
        let worktree_status = git::run(&root.join(".mush/wt/1"), &["status", "--porcelain"])
            .unwrap_or_else(|error| error);
        assert!(
            worktree_status.is_empty(),
            "the worktree must be left clean, got {worktree_status:?}"
        );
        // The three commands mush prints must now do what they say: merge the
        // work back, then let go of the worktree and the branch.
        let merged = git::run(
            &root,
            &[
                "-c",
                "user.name=mush",
                "-c",
                "user.email=mush@local",
                "merge",
                "--no-edit",
                "mush/1",
            ],
        );
        assert!(merged.is_ok(), "/merge must merge: {merged:?}");
        assert!(
            root.join("iso.txt").exists(),
            "after /merge the file must be in the human's workspace"
        );
        let removed = git::run(&root, &["worktree", "remove", ".mush/wt/1"]);
        assert!(
            removed.is_ok(),
            "/discard must remove the worktree: {removed:?}"
        );
        let deleted = git::run(&root, &["branch", "-D", "mush/1"]);
        assert!(
            deleted.is_ok(),
            "/discard must delete the branch: {deleted:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Root -> child -> grandchild, each isolated: the grandchild's file must
    /// land in `.mush/wt/2/` on a branch that carries it, branched off the
    /// child's worktree (`mush/2` based on `mush/1`), and the summaries bubble up
    /// through wait_agents. Three actors ask one scripted model at once; each
    /// reply says which of them it is for.
    #[test]
    fn deep_chain_writes_nested_worktrees() {
        let root = init_git_repo("chain");
        let scripted = Arc::new(
            Scripted::new()
                // The grandchild is the leaf that writes.
                .when(|asked: &Asked| asked.depth() == Some(2) && !asked.saw("wrote deep.txt"))
                .calls(vec![tool_call(
                    "c2",
                    "write_file",
                    json!({ "path": "deep.txt", "content": "deep work" }),
                )])
                .when(|asked: &Asked| asked.depth() == Some(2))
                .says("created deep.txt")
                // The child only delegates: spawn its own, wait, report.
                .when(|asked: &Asked| asked.depth() == Some(1) && asked.saw("#2 done"))
                .says("chain child done")
                .when(|asked: &Asked| asked.depth() == Some(1) && asked.saw("spawned agent"))
                .calls(vec![tool_call(
                    "c1b",
                    "wait_agents",
                    json!({ "ids": [2], "timeout": 30 }),
                )])
                .when(|asked: &Asked| asked.depth() == Some(1))
                .calls(vec![tool_call(
                    "c1a",
                    "spawn_agent",
                    json!({
                        "brief": "create a file called deep.txt containing exactly: deep work; \
                                  you must delegate this to your own subagent",
                        "isolated": true
                    }),
                )])
                // The root: delegate, wait for the child, report.
                .when(|asked: &Asked| asked.saw("#1 done"))
                .says("chain root done")
                .when(|asked: &Asked| asked.saw("spawned agent"))
                .calls(vec![tool_call(
                    "c0b",
                    "wait_agents",
                    json!({ "ids": [1], "timeout": 30 }),
                )])
                .calls(vec![tool_call(
                    "c0a",
                    "spawn_agent",
                    json!({
                        "brief": "delegate file creation to your own subagent: spawn one with \
                                  brief 'create a file called deep.txt containing exactly: deep \
                                  work' and isolated true, then wait for it, then report",
                        "isolated": true
                    }),
                )]),
        );
        let events = Recorder::new();
        let root_tx = spawn_scripted(
            Config::new("http://127.0.0.1:1", "scripted", None),
            events.clone(),
            root.clone(),
            scripted.clone(),
        )
        .tx;
        root_tx
            .send(AgentMsg::Run(vec![
                Message::system(prompt::system_prompt(root.to_str().unwrap())),
                Message::user(
                    "CHAIN: delegate the file creation through two levels of subagents".to_string(),
                ),
            ]))
            .unwrap();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 3),
            "each level must run and finish once: {seen:?}"
        );
        assert_eq!(seen.done, 3, "each level runs exactly once: {seen:?}");
        assert_eq!(seen.errors, Vec::<String>::new());

        // The grandchild (agent #2) wrote into its own worktree.
        assert_eq!(
            fs::read_to_string(root.join(".mush/wt/2/deep.txt"))
                .ok()
                .as_deref(),
            Some("deep work"),
            "the grandchild must write its own worktree"
        );
        // Nested worktree, now with real history: the grandchild's branch
        // carries its work and is based on the child's branch, which — because
        // the child only delegated — still sits at the branch point. And the
        // child's worktree must NOT contain the grandchild's file.
        let grandchild_files =
            git::run(&root, &["diff", "--name-only", "HEAD...mush/2"]).unwrap_or_default();
        assert!(
            grandchild_files.contains("deep.txt"),
            "mush/2 must carry the grandchild's work, got {grandchild_files:?}"
        );
        assert_eq!(
            git_rev_parse(&root, "mush/1").as_deref(),
            git_rev_parse(&root, "HEAD").as_deref(),
            "the child delegated, so its own branch stays at the branch point"
        );
        assert!(
            git::run(&root, &["merge-base", "--is-ancestor", "mush/1", "mush/2"]).is_ok(),
            "mush/2 must be based on mush/1 (nested, not re-rooted)"
        );
        assert!(
            !root.join(".mush/wt/1/deep.txt").exists(),
            "the child's worktree must stay clean of grandchild work"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A transcript that fills the (tiny, configured) context window must be
    /// folded into a summary — not dropped — and the run continues from it,
    /// with the task still worth doing: the isolated child does the work it was
    /// asked for before the fold.
    #[test]
    fn compaction_folds_overflowing_history_into_a_summary() {
        let root = init_git_repo("compact");
        let summary =
            "the task was to create iso.txt via an isolated subagent; nothing is done yet";
        let scripted = Arc::new(
            Scripted::new()
                // The compaction ask carries the instruction, the transcript
                // search works without a server's help.
                .when(|asked: &Asked| asked.saw(COMPACT_INSTRUCTION))
                .says(summary)
                .when(|asked: &Asked| asked.depth() == Some(1) && !asked.saw("wrote iso.txt"))
                .calls(vec![tool_call(
                    "c1",
                    "write_file",
                    json!({ "path": "iso.txt", "content": "isolated work" }),
                )])
                .when(|asked: &Asked| asked.depth() == Some(1))
                .says("created iso.txt in my worktree")
                .when(|asked: &Asked| asked.saw("#1 done"))
                .says("done")
                .when(|asked: &Asked| asked.saw("spawned agent"))
                .says("child left running — I will handle its result when it finishes")
                // The first thing the run does after the fold: the task again.
                .calls(vec![tool_call(
                    "c0",
                    "spawn_agent",
                    json!({
                        "brief": "create a file called iso.txt containing exactly: isolated work",
                        "isolated": true
                    }),
                )]),
        );

        let mut cfg = Config::new("http://127.0.0.1:1", "scripted", None);
        // Tight window: the reserve scales with it, so the budget is 3 * (ctx
        // - ctx/2) = 6000 bytes.
        cfg.context_tokens = 4_000;
        let budget = cfg.history_budget();
        let events = Recorder::new();
        let root_tx = spawn_scripted(cfg, events.clone(), root.clone(), scripted.clone()).tx;

        // History above 3/4 of the budget but still fitting: compaction must
        // trigger instead of trimming. Built until it crosses the line, so the
        // test does not encode the budget formula.
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("create a file via subagents".to_string()),
        ];
        let mut total: usize = messages.iter().map(Message::weight).sum();
        let mut index = 0;
        while total <= budget * 3 / 4 {
            let assistant = Message::assistant(format!("reply {index} {}", "x".repeat(280)));
            let user = Message::user(format!("again {index}"));
            total += assistant.weight() + user.weight();
            messages.push(assistant);
            messages.push(user);
            index += 1;
        }
        assert!(
            total > budget * 3 / 4 && total <= budget,
            "test transcript must sit in the compaction window (total {total}, budget {budget})"
        );
        root_tx.send(AgentMsg::Run(messages)).unwrap();

        // The root's run and the child's, in either order: whether the child
        // finished before the root's next message boundary is a race the test
        // does not care about.
        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 2),
            "the run must finish, and the child with it: {seen:?}"
        );
        assert_eq!(seen.errors, Vec::<String>::new());
        assert_eq!(
            seen.summaries,
            vec![summary.to_string()],
            "the overflowing history must be folded into a summary, once"
        );

        // “The run continues from it” means the next request is the task again,
        // and it is built from the summary alone — not from the history that no
        // longer fits.
        let asked = scripted.asked();
        assert_eq!(
            asked[0].messages.last().map(Message::text),
            Some(COMPACT_INSTRUCTION),
            "the first request is the summary ask"
        );
        assert_eq!(
            asked[1].messages.len(),
            2,
            "system + the summary, nothing else: {:?}",
            asked[1].messages
        );
        assert_eq!(
            asked[1].messages[1].text(),
            prompt::compaction_message(summary)
        );
        // …and the work survives the fold: the child was asked afterwards.
        assert_eq!(
            fs::read_to_string(root.join(".mush/wt/1/iso.txt"))
                .ok()
                .as_deref(),
            Some("isolated work")
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// `/compact` on an idle agent: the conversation is folded into a summary
    /// there and then, and nothing else happens. No `Running`, no answer turn,
    /// one model call — the request is a summary, not new work to answer.
    #[test]
    fn a_compact_request_folds_an_idle_agent_without_a_run() {
        let root = scratch_dir("compact-idle");
        let summary = "the task was to say something; it was said";
        let scripted = Arc::new(
            Scripted::new()
                .when(|asked: &Asked| asked.saw(COMPACT_INSTRUCTION))
                .says(summary)
                .says("first answer")
                .says("carried on"),
        );
        let events = Recorder::new();
        let root_tx = spawn_scripted(
            Config::new("http://127.0.0.1:1", "scripted", None),
            events.clone(),
            root.clone(),
            scripted.clone(),
        )
        .tx;
        root_tx
            .send(AgentMsg::Run(vec![
                Message::system("you are mush"),
                Message::user("say something".to_string()),
            ]))
            .unwrap();
        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 1),
            "the run must finish before the fold: {seen:?}"
        );

        root_tx.send(AgentMsg::Compact).unwrap();
        assert!(
            seen.wait(&events, WAIT, |seen| !seen.summaries.is_empty()),
            "an idle /compact must fold the transcript: {seen:?}"
        );
        assert_eq!(seen.summaries, vec![summary.to_string()]);
        assert_eq!(
            seen.done, 1,
            "the fold is not a run: no second completion came out of it"
        );
        assert_eq!(seen.errors, Vec::<String>::new());
        let running = events
            .events()
            .iter()
            .filter(|(_, event)| matches!(event, AgentEvent::Running { .. }))
            .count();
        assert_eq!(running, 1, "only the run that was asked for, before it");

        // One ask for the run, one for the fold — the second carries no tools
        // and the instruction, and no answer was requested after it.
        let asked = scripted.asked();
        assert_eq!(asked.len(), 2, "one summarize call, nothing else");
        assert!(asked[1].saw(COMPACT_INSTRUCTION), "the second ask folds");
        assert_eq!(asked[1].tools, 0, "a plain, tool-free request");

        // The transcript really is `[system, user(summary)]`: the next request
        // is that plus the words the human typed after it.
        root_tx
            .send(AgentMsg::Nudge("carry on".to_string()))
            .unwrap();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 2),
            "the nudge must be answered: {seen:?}"
        );
        let asked = scripted.asked();
        let after = asked.last().unwrap();
        assert_eq!(
            after.messages.iter().map(Message::text).collect::<Vec<_>>(),
            vec![
                "you are mush".to_string(),
                prompt::compaction_message(summary),
                "carry on".to_string(),
            ],
            "the fold replaced everything but the system prompt"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A `Compact` that arrives while the model is working is parked like a
    /// nudge: the fold happens at the next message boundary, after the tool
    /// batch it arrived behind has been executed and answered — never between
    /// an assistant's calls and their results.
    #[test]
    fn a_compact_request_waits_for_the_tool_result_it_arrived_behind() {
        let root = scratch_dir("compact-mid-run");
        let gate = Arc::new(Gate::new());
        let summary = "wrote note.txt; nothing else happened";
        let scripted = Arc::new(
            Scripted::new()
                // Held, so the test can put the request in the mailbox while
                // the run is provably in flight — no sleep, no race.
                .held(gate.clone())
                .calls(vec![tool_call(
                    "c1",
                    "write_file",
                    json!({ "path": "note.txt", "content": "worth folding" }),
                )])
                .when(|asked: &Asked| asked.saw(COMPACT_INSTRUCTION))
                .says(summary)
                .says("carried on after the fold"),
        );
        let events = Recorder::new();
        let root_tx = spawn_scripted(
            Config::new("http://127.0.0.1:1", "scripted", None),
            events.clone(),
            root.clone(),
            scripted.clone(),
        )
        .tx;
        root_tx
            .send(AgentMsg::Run(vec![
                Message::system("you are mush"),
                Message::user("write the note".to_string()),
            ]))
            .unwrap();
        assert!(
            gate.wait_until_asked(WAIT),
            "the run never reached the model"
        );
        root_tx.send(AgentMsg::Compact).unwrap();
        gate.release();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 1),
            "the run must finish with the fold folded in: {seen:?}"
        );
        assert_eq!(seen.errors, Vec::<String>::new());
        assert_eq!(seen.summaries, vec![summary.to_string()]);

        let asked = scripted.asked();
        assert_eq!(asked.len(), 3, "tool call, fold, then the answer");
        assert!(
            !asked[0].saw(COMPACT_INSTRUCTION),
            "it is not injected into the request already in flight"
        );
        let fold = &asked[1];
        assert!(fold.saw(COMPACT_INSTRUCTION), "the second ask is the fold");
        assert!(
            fold.saw("wrote note.txt"),
            "the fold happens at the boundary, behind the batch's result: {:?}",
            fold.messages.iter().map(Message::text).collect::<Vec<_>>()
        );
        // And the run continued from the summary alone.
        assert_eq!(
            asked[2]
                .messages
                .iter()
                .map(Message::text)
                .collect::<Vec<_>>(),
            vec![
                "you are mush".to_string(),
                prompt::compaction_message(summary)
            ]
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A `/compact` sent while the model is writing the run's *last* reply
    /// arrives at the boundary the run ends on. It must not be dropped with the
    /// run: the actor folds as it goes idle, after the completion — once.
    #[test]
    fn a_compact_request_behind_the_last_reply_is_honoured_as_the_run_ends() {
        let root = scratch_dir("compact-end-of-run");
        let gate = Arc::new(Gate::new());
        let summary = "the task was to answer; it was answered";
        let scripted = Arc::new(
            Scripted::new()
                .held(gate.clone())
                .says("answered")
                .when(|asked: &Asked| asked.saw(COMPACT_INSTRUCTION))
                .says(summary),
        );
        let events = Recorder::new();
        let root_tx = spawn_scripted(
            Config::new("http://127.0.0.1:1", "scripted", None),
            events.clone(),
            root.clone(),
            scripted.clone(),
        )
        .tx;
        root_tx
            .send(AgentMsg::Run(vec![
                Message::system("you are mush"),
                Message::user("answer me".to_string()),
            ]))
            .unwrap();
        assert!(
            gate.wait_until_asked(WAIT),
            "the run never reached the model"
        );
        root_tx.send(AgentMsg::Compact).unwrap();
        gate.release();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| !seen.summaries.is_empty()),
            "the request must survive the run it arrived in: {seen:?}"
        );
        assert_eq!(seen.errors, Vec::<String>::new());
        assert_eq!(seen.done, 1, "the fold is not a second run");
        assert_eq!(seen.summaries, vec![summary.to_string()]);

        // The completion comes first: the fold happens as the actor goes idle,
        // not instead of finishing the run.
        let order: Vec<&str> = events
            .events()
            .iter()
            .filter_map(|(_, event)| match event {
                AgentEvent::Done => Some("done"),
                AgentEvent::Compact { .. } => Some("compact"),
                _ => None,
            })
            .collect();
        assert_eq!(order, vec!["done", "compact"]);
        assert_eq!(
            scripted.asked().len(),
            2,
            "the answer, then the one summarize call"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A fold of a transcript that is already `system + one message` is refused:
    /// it would cost a request and re-summarize the summary. The human asked,
    /// though, so the refusal is said out loud instead of being silence.
    #[test]
    fn a_fold_with_nothing_to_fold_says_so_instead_of_asking_the_model() {
        let root = scratch_dir("compact-minimal");
        let scripted = Arc::new(Scripted::new().says("answered"));
        let events = Recorder::new();
        let root_tx = spawn_scripted(
            Config::new("http://127.0.0.1:1", "scripted", None),
            events.clone(),
            root.clone(),
            scripted.clone(),
        )
        .tx;
        root_tx
            .send(AgentMsg::Run(vec![Message::system("you are mush")]))
            .unwrap();
        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 1),
            "the run must finish first: {seen:?}"
        );
        assert_eq!(scripted.asked().len(), 1, "the run's own request");

        root_tx.send(AgentMsg::Compact).unwrap();
        assert!(
            seen.wait(&events, WAIT, |seen| !seen.notices.is_empty()),
            "the refusal must be visible: {seen:?}"
        );
        assert_eq!(seen.notices, vec![NOTHING_TO_COMPACT.to_string()]);
        assert!(
            seen.summaries.is_empty(),
            "nothing was folded, so no summary was claimed"
        );
        assert_eq!(
            scripted.asked().len(),
            1,
            "the refusal costs no summarize call"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The human types while the model is answering: the nudge must land after
    /// that reply and be answered, never silently swallowed when the run would
    /// otherwise end. The first reply is held open, so the nudge is provably in
    /// flight — no sleep, and no marker file to poll for.
    #[test]
    fn a_nudge_that_arrives_mid_reply_is_answered() {
        let root = scratch_dir("steer");
        let gate = Arc::new(Gate::new());
        let scripted = Arc::new(
            Scripted::new()
                .held(gate.clone())
                .says("first reply")
                .says("steered"),
        );

        let events = Recorder::new();
        let root_tx = spawn_scripted(
            Config::new("http://127.0.0.1:1", "scripted", None),
            events.clone(),
            root.clone(),
            scripted.clone(),
        )
        .tx;
        root_tx
            .send(AgentMsg::Run(vec![
                Message::system("you are mush"),
                Message::user("STEER: answer this, then whatever else I say".to_string()),
            ]))
            .unwrap();

        // The reply is in flight: the human types while the model answers.
        assert!(
            gate.wait_until_asked(WAIT),
            "the first request never reached the model"
        );
        root_tx
            .send(AgentMsg::Nudge("STEERME".to_string()))
            .unwrap();
        gate.release();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done > 0),
            "the run must finish: {seen:?}"
        );
        assert_eq!(seen.errors, Vec::<String>::new());
        assert_eq!(
            seen.replies,
            vec!["first reply".to_string(), "steered".to_string()],
            "the first reply arrives, and the nudge is answered after it"
        );
        // Answered *because* the model was given it: the second request carries
        // the nudge, so it is not a reply to the same words again.
        let asked = scripted.asked();
        assert_eq!(asked.len(), 2, "one turn for the reply, one for the nudge");
        assert!(asked[1].saw("STEERME"), "{:?}", asked[1].messages);
        let _ = fs::remove_dir_all(&root);
    }

    /// Hitting the turn limit must end with a summary, not a bare `stopped
    /// after 200 turns without finishing`: the safety valve stays, the failure
    /// goes (finding N1). Every turn before the guard does real work — one
    /// `write_file`, with the arguments differing each turn so the run is not
    /// stopped early as a loop instead.
    #[test]
    fn the_turn_limit_ends_with_a_summary() {
        const WRAPPED_UP: &str = "wrapped up: the work done so far is in the workspace";
        let root = scratch_dir("turns");

        let mut scripted = Scripted::new();
        for turn in 0..RUNAWAY_TURNS - 1 {
            scripted = scripted.calls(vec![tool_call(
                "call",
                "write_file",
                json!({ "path": "notes.txt", "content": format!("turn {turn}") }),
            )]);
        }
        let scripted = Arc::new(scripted.says(WRAPPED_UP));

        let mut cfg = Config::new("http://127.0.0.1:1", "scripted", None);
        // A window wide enough that this transcript never compacts: the only
        // thing that may end this run is the runaway guard.
        cfg.context_tokens = 128_000;
        let events = Recorder::new();
        let root_tx = spawn_scripted(cfg, events.clone(), root.clone(), scripted.clone()).tx;
        root_tx
            .send(AgentMsg::Run(vec![
                Message::system(prompt::system_prompt(root.to_str().unwrap())),
                Message::user("TURNS: keep working until you are done".to_string()),
            ]))
            .unwrap();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done > 0
                || !seen.errors.is_empty()),
            "the run must end: {seen:?}"
        );
        assert_eq!(
            seen.errors,
            Vec::<String>::new(),
            "the turn limit must not be reported as an error"
        );
        assert_eq!(seen.done, 1, "the run must finish normally: {seen:?}");
        assert_eq!(
            seen.replies,
            vec![WRAPPED_UP.to_string()],
            "the wrap-up turn's summary must be the result"
        );
        assert!(
            seen.notices
                .iter()
                .any(|notice| notice.contains("runaway guard")),
            "the human must be told why the tools went away: {:?}",
            seen.notices
        );

        // Every turn ran: one request per turn, and the last one asked for the
        // summary with the tools withdrawn rather than failing the run.
        let asked = scripted.asked();
        assert_eq!(
            asked.len(),
            RUNAWAY_TURNS,
            "the run must use every turn before the guard"
        );
        assert_eq!(
            asked[RUNAWAY_TURNS - 1].tools,
            0,
            "the wrap-up turn must be asked without tools"
        );
        assert!(
            asked[RUNAWAY_TURNS - 1].saw("runaway guard"),
            "the wrap-up request must say why the tools went away"
        );
        assert_eq!(
            fs::read_to_string(root.join("notes.txt")).ok().as_deref(),
            Some(format!("turn {}", RUNAWAY_TURNS - 2).as_str()),
            "the last turn before the guard must have done its work"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// How long a scenario waits for something the run is *supposed* to do.
    /// Only ever spent waiting for an event, never asserting on it.
    const WAIT: Duration = Duration::from_secs(5);

    /// What the actors told the UI, as a test watches a run.
    ///
    /// One pass over the event channel answers all of it, so a deadline is
    /// spent waiting for the run rather than sleeping past it.
    #[derive(Default, Debug)]
    struct Watched {
        /// How many recorded events have been read into this one already.
        seen: usize,
        done: usize,
        errors: Vec<String>,
        notices: Vec<String>,
        /// What the model said, in the empty-reply-free sense: an assistant
        /// message that actually carried words.
        replies: Vec<String>,
        summaries: Vec<String>,
        stopped: usize,
    }

    impl Watched {
        /// Read events until `until` holds or `timeout` passes; the return says
        /// whether it held, so a run that never finishes fails an assertion
        /// instead of hanging the suite.
        fn wait(
            &mut self,
            events: &Recorder,
            timeout: Duration,
            until: impl Fn(&Self) -> bool,
        ) -> bool {
            let deadline = Instant::now() + timeout;
            loop {
                self.drain(events);
                if until(self) {
                    return true;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return false;
                }
                // Nothing is emitted until something happens, so the wait is on
                // the sink, not on a poll of it.
                if !events.wait(left) {
                    self.drain(events);
                    return until(self);
                }
            }
        }

        fn drain(&mut self, events: &Recorder) {
            for (id, event) in events.events().into_iter().skip(self.seen) {
                self.seen += 1;
                self.note(id, event);
            }
        }

        fn note(&mut self, _id: AgentId, event: AgentEvent) {
            match event {
                AgentEvent::Done => self.done += 1,
                AgentEvent::Error(why) => self.errors.push(why),
                AgentEvent::Notice(what) => self.notices.push(what),
                AgentEvent::Stopped => self.stopped += 1,
                AgentEvent::Compact { summary } => self.summaries.push(summary),
                AgentEvent::Message(message)
                    if message.role == "assistant" && !message.text().is_empty() =>
                {
                    self.replies.push(message.text().to_string());
                }
                _ => {}
            }
        }
    }

    /// A scratch workspace, empty: for a scenario whose work is not files.
    fn scratch_dir(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mush-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// A scratch git repo with one initial commit, ready for worktrees. The
    /// label keeps parallel tests from sharing a directory.
    fn init_git_repo(label: &str) -> PathBuf {
        use std::process::Command;

        let root = scratch_dir(&format!("git-{label}"));
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
        fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        root
    }

    fn git_rev_parse(root: &Path, rev: &str) -> Option<String> {
        use std::process::Command;

        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", rev])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}
