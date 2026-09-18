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
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use serde_json::{json, Value};

use mush_core::config::parse_context_hint;
use mush_core::git;
use mush_core::message::{ChatRequest, ChatResponse};
use mush_core::text::{sanitize, truncate};
use mush_core::tools::ToolName;
use mush_core::transcript::{
    needs_compaction, repair_tool_pairs, sanitize_tool_calls, trim_history, COMPACT_INSTRUCTION,
    COMPACT_REPLY_TOKENS,
};
use mush_core::{prompt, tools, Config, Message, Workspace, CMD_CAP, CMD_TIMEOUT_SECS};

use crate::app::{
    tokens_label, AgentId, Compacting, ConfigHandle, ConversationId, Msg, WindowSource,
};
use crate::clock;
use crate::events::{Events, Ui};
use crate::jobs::{self, Refused};
use crate::machine::{Job, Machine, Shell, ShellCommand};
use crate::model::{retrying, HttpModel, ModelClient, ModelError};

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
/// The real token counts the endpoint reported for this run's calls, summed
/// over the turns it reported them on. `None` until a reply carries `usage`: a
/// server that reports none leaves mush's own bytes-per-token estimate as the
/// only number there is, and that estimate is what the UI's meter shows.
#[derive(Clone, Copy, Default)]
struct RunUsage {
    prompt: u64,
    completion: u64,
    total: u64,
    /// Whether a reply that was counted left the total out. A sum with a hole
    /// in it is not a total, so the two parts stand in for one.
    total_missing: bool,
}

impl RunUsage {
    fn add(&mut self, usage: &mush_core::Usage) {
        self.prompt += usage.prompt_tokens;
        self.completion += usage.completion_tokens;
        if usage.total_tokens == 0 {
            self.total_missing = true;
        } else {
            self.total += usage.total_tokens;
        }
    }

    /// The line the run reports. A server that omits the total still gets one:
    /// the two parts are what it counted, and adding them invents nothing.
    fn line(&self) -> String {
        let total = if self.total_missing {
            self.prompt + self.completion
        } else {
            self.total
        };
        format!(
            "the endpoint counted {} prompt + {} completion tokens this run ({} total)",
            tokens_label(self.prompt as usize),
            tokens_label(self.completion as usize),
            tokens_label(total as usize),
        )
    }
}

/// Report the endpoint's own numbers once, when the run ends. Cheap and rare
/// (one line per run), and the only place a real count can come from: the
/// UI's meter is bytes/3, which is all a server without `usage` offers.
fn report_usage(actor: &Actor, usage: Option<RunUsage>) {
    if let Some(usage) = usage {
        actor.ctx.emit(actor.id, AgentEvent::Notice(usage.line()));
    }
}

/// The reply's finish reason when it is none of the three mush understands:
/// `stop`, a tool batch, and the token cap. `content_filter` is the endpoint
/// refusing to hand over what the model wrote; any other value is a reply the
/// endpoint chose not to finish normally. Neither is a result — and with no
/// content a refused reply used to surface as an empty one, which reads as the
/// model having nothing to say.
///
/// A missing reason — or an empty one, which some servers send instead of
/// `stop` — says nothing, and is the only reason read as a normal end besides
/// the three.
fn refusal_reason(finish_reason: Option<&str>) -> Option<&str> {
    match finish_reason {
        None | Some("") | Some("stop") | Some("tool_calls") | Some("length") => None,
        Some(other) => Some(other),
    }
}

/// Why a refused reply is not a result, in the run's own words. `length` gets
/// its own account (the cap can be asked down and retried); a refusal is the
/// endpoint's verdict and no amount of retrying inside one run changes it.
fn refusal_error(reason: &str) -> String {
    match reason {
        "content_filter" => "the model's reply was stopped by the endpoint's content filter \
                             (finish_reason: content_filter) — the endpoint refused to answer"
            .to_string(),
        other => format!(
            "the model's reply ended with an unsupported finish_reason: {other} \
             — the endpoint did not finish the answer"
        ),
    }
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
/// How wide one line of an `agent_status` digest may be, in columns. A listing
/// is for telling children apart and knowing what is unread; the body itself
/// travels through the delivery roads, once (see [`Outcome::digest`]).
const DIGEST_COLUMNS: usize = 100;
/// Why a cancelled run ends. Internal to the actor: a run that ends with this
/// becomes `Outcome::Stopped` at the actor boundary, so no other layer has to
/// compare result text to know what happened.
const CANCELLED: &str = "cancelled";

/// How a run ended, as the actor reports it to its parent and to its own row.
///
/// Four outcomes, because they mean four different things: a finished run
/// produced a result, a failed run produced an error, a *stopped* run produced
/// neither — the actor is still alive and a nudge resumes it — and a run that
/// was **cut off** never ended at all: the process went away with it in flight,
/// or the actor's thread did, and nothing was committed by it. A bare summary
/// string could not tell them apart, so a stopped child was reported through the
/// same path as a finished one and read as `done`; and the fourth had no name at
/// all, which is how a killed run came back looking idle (`docs/findings.md`
/// H2).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The run finished; the string is its summary.
    Finished(String),
    /// The run was stopped (Ctrl-C, `agent_control stop`, `/new`). Not a
    /// result and not a failure: the actor is idle and resumable.
    Stopped,
    /// The run never ended: mush went away with it in flight, and nothing was
    /// committed by it. Not `Stopped` — there is no actor left to resume — and
    /// not `Failed` — nothing the model or the endpoint did broke.
    ///
    /// It is news, unlike a stop: the parent has to know that the work it was
    /// waiting for is not on its way and may be sitting uncommitted in a
    /// worktree.
    CutOff,
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
    /// A run that never ended: an interrupted run's work, committed anyway.
    /// Its own shape so the branch cannot read as a stop the human asked for.
    CutOff,
    Failed(String),
}

impl From<&Outcome> for Committed {
    fn from(outcome: &Outcome) -> Self {
        match outcome {
            Outcome::Finished(_) => Committed::Finished,
            Outcome::Stopped => Committed::Stopped,
            Outcome::CutOff => Committed::CutOff,
            Outcome::Failed(error) => Committed::Failed(error.clone()),
        }
    }
}

/// How wide a commit subject's brief may be, in columns.
///
/// A subject is one line in `git log --oneline`; a brief is a paragraph. The
/// docs write the subject as `mush #N: <brief>` (`docs/mush.md` §…, README),
/// which no commit subject can be — a subject cannot be unbounded — so this is
/// the code's rule and the docs are the imprecise side (reported, not edited).
const SUBJECT_COLUMNS: usize = 60;

/// The task a commit subject carries: the brief's first line, trimmed to
/// [`SUBJECT_COLUMNS`] columns and cut at a word boundary.
///
/// The first line because a subject is one line and the brief's first line is
/// the task ("create a file called iso.txt…" — the reasons live below). The
/// word boundary because [`truncate`] alone ends a subject mid-word
/// (`isolated w…`), which neither reads as English nor matches the brief; the
/// whole word that does not fit is dropped and the `…` says so. A first line
/// with no space to cut on keeps the hard cut — a clipped subject is better
/// than no subject.
fn subject_brief(brief: &str) -> String {
    let first = brief.lines().next().unwrap_or("").trim();
    let cut = truncate(first, SUBJECT_COLUMNS);
    if !cut.ends_with('…') {
        return cut;
    }
    let body = cut.trim_end_matches('…');
    match body.rfind(char::is_whitespace) {
        Some(at) if at > 0 => {
            // The `…` spends one of the columns, so the word is re-cut one
            // short of the budget and the ellipsis added back.
            format!("{}…", truncate(body[..at].trim_end(), SUBJECT_COLUMNS - 1))
        }
        _ => cut,
    }
}

/// The commit subject for an isolated agent's work.
///
/// The outcome is in the subject on purpose: an interrupted run commits its work
/// in progress too, and a log full of identically-formatted `mush #3: <brief>`
/// subjects cannot be told apart from finished work. The brief is
/// [`subject_brief`]'s, so a subject is one line, at a word boundary. The
/// stopped and failed shapes therefore carry the outcome *and* the brief, each
/// bounded. [`parse_commit_subject`] is the inverse, and the two are tested
/// against each other.
pub fn commit_subject(id: u64, brief: &str, outcome: &Outcome) -> String {
    let brief = subject_brief(brief);
    match Committed::from(outcome) {
        Committed::Finished => format!("mush #{id}: {brief}"),
        Committed::Stopped => format!("mush #{id} (stopped, work in progress): {brief}"),
        // A run that was cut off commits its work in progress too — this is the
        // subject of a commit that *exists*, so it says "work in progress", not
        // "nothing committed": that phrase describes the state the cut-off run
        // itself left behind, and a commit is the state after someone picked the
        // work up.
        Committed::CutOff => format!("mush #{id} (cut off, work in progress): {brief}"),
        Committed::Failed(error) => {
            format!("mush #{id} (failed: {}): {brief}", truncate(&error, 40))
        }
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
    } else if head == "cut off, work in progress" {
        Committed::CutOff
    } else {
        // Any other shape must name a failure; if it does not, this subject is
        // not one mush wrote.
        Committed::Failed(head.strip_prefix("failed: ")?.to_string())
    };
    Some((ended, brief.to_string()))
}

impl Outcome {
    /// The line a parent reads. Each outcome names itself, so a stop can never
    /// be mistaken for a result — and a run that never ended can never be
    /// mistaken for either.
    ///
    /// `pub(crate)` because a cut-off run has no actor left to write this line:
    /// the UI, which is the only observer that can tell an actor vanished, files
    /// it through the very same sentence (`App::report_cut_off`).
    pub(crate) fn line(&self, id: u64) -> String {
        match self {
            Outcome::Finished(summary) => format!("#{id} done: {summary}"),
            Outcome::Stopped => format!(
                "#{id} stopped: the run ended before it finished — this agent is idle, \
                 not done; agent_control message resumes it"
            ),
            Outcome::CutOff => format!(
                "#{id} cut off: the run never ended — nothing was committed; \
                 its work is where it left it"
            ),
            Outcome::Failed(error) => format!("#{id} failed: {error}"),
        }
    }

    /// Whether this is news worth waking a napping parent for. A stop is the
    /// human's doing, not news, so it waits in the transcript instead of
    /// paying for a fresh run. A cut-off *is* news: the parent is waiting for a
    /// result that will never come, and the work it was waiting on may be
    /// sitting uncommitted, so it has to be told rather than left to assume.
    fn is_news(&self) -> bool {
        !matches!(self, Outcome::Stopped)
    }

    /// A bounded rendering of this outcome for a *listing* (`agent_status`):
    /// the first line, cut at [`DIGEST_COLUMNS`], plus the size of the whole.
    ///
    /// Never the body. A listing is not a delivery: the body is handed to the
    /// model exactly once, by the roads that ask [`ActorState::record_child`]
    /// and mark it read. `agent_status` used to print every child's entire
    /// final message on every call, so a parent that polled it re-read every
    /// child's report again and again (the replay this digest closes).
    fn digest(&self, id: u64) -> String {
        let (mark, body) = match self {
            Outcome::Finished(summary) => ("✓", summary.as_str()),
            Outcome::Failed(error) => ("✗", error.as_str()),
            Outcome::Stopped => {
                return format!("#{id} ⊘ stopped — idle and resumable");
            }
            Outcome::CutOff => {
                return format!("#{id} ⚠ cut off — the run never ended; nothing was committed");
            }
        };
        let first = body.lines().next().unwrap_or("").trim();
        let cut = truncate(first, DIGEST_COLUMNS);
        if body.chars().count() > cut.chars().count() {
            format!("#{id} {mark} {cut} ({} chars total)", body.chars().count())
        } else {
            format!("#{id} {mark} {cut}")
        }
    }
}

/// The run number a cut-off run is reported under.
///
/// A cut-off run *never ended*, so it never got the number a report carries:
/// [`ActorState::runs`] is incremented where the outcome is decided, and this
/// run had no outcome — the actor was gone before it could decide one. What the
/// parent needs from [`AgentMsg::ChildDone`]'s `run` is identity, not
/// arithmetic: a value no real report can carry is newer than every run the
/// parent has read (so the line folds, and wakes a napping parent) and the same
/// value twice is the same report (so it folds once — `docs/findings.md` B24).
/// Nothing can ever claim it afterwards either: the actor that would is the
/// thing that vanished. The UI is the only hand left that can file the report
/// (`App::report_cut_off`).
pub(crate) const CUT_OFF_RUN: u64 = u64::MAX;

/// Commands sent into an agent actor's mailbox.
pub enum AgentMsg {
    /// Adopt these messages and run. The actor keeps the transcript, so later
    /// nudges continue the same conversation.
    Run(Vec<Message>),
    /// Append a user message the human typed; if idle, run again. The UI echoed
    /// these words before sending them, so the actor folds them in without
    /// telling it to add them again.
    Nudge(String),
    /// A steering message from another agent — what `agent_control message`
    /// sends (`docs/findings.md` B22). It is the same kind of work as a nudge
    /// and travels the same roads, but it is *not* the human's own typing: the
    /// UI has never seen these words, so the actor emits the line as it folds
    /// them in, and the human can read what their model was told.
    Steer(String),
    /// Fold this agent's conversation into a summary now, instead of waiting
    /// for the window to fill (`/compact`). A summarize request and a
    /// transcript replacement, not work to answer: an idle agent does it at
    /// once and stays idle.
    ///
    /// `messages` is the UI's copy of the conversation, used only by an actor
    /// that has none yet: a session restored from disk starts every actor with
    /// an empty transcript (the UI holds the history until the human's next
    /// message hands it over), and a fold is not a run, so nothing else would
    /// ever hand it over — the request folded nothing at all, silently.
    Compact(Vec<Message>),
    /// Cancel the current run. An idle agent ignores it — Stop cancels work,
    /// it does not end an agent.
    Stop,
    /// End this actor for good (`/new`, Ctrl-N). A `Stop` cannot do this: an
    /// actor holds its own mailbox open, so it never learns that everyone else
    /// let go — it has to be told.
    Shutdown,
    /// A child's run ended. The outcome says *how*: a stop is not a result.
    /// `run` is which of that child's runs this was (its own counter, 1 for the
    /// first). The parent needs it to tell two reports apart: the *same* run
    /// reported twice is one piece of news, while a run after it is a new one
    /// even when it reads identically — two runs that both fail
    /// `Connection reset by peer (os error 104)` are two failures, and a bare
    /// `Outcome` cannot say which of the two it is holding
    /// (`docs/findings.md` B24).
    ChildDone { id: u64, run: u64, outcome: Outcome },
    /// A job this agent started ended. `line` is the report its owner reads,
    /// rendered once by the registry; `news` says whether it is worth waking a
    /// napping agent for (`ChildDone` and `Outcome::is_news` again: a job mush
    /// killed is the human's doing, not a result).
    CommandDone { id: u64, line: String, news: bool },
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
    /// The parent has read a child's result: the line is in its transcript now,
    /// wherever it came from — the fold at a message boundary, the wake-up a
    /// napping parent got, or a `wait_agents` that asked for it.
    ///
    /// Emitted by the *parent* (the id it is tagged with) about the child it
    /// names, because the parent owns the `delivered` set. This is that fact
    /// leaving the actor, so the child's row can stop wearing `✉` on the
    /// parent's reading rather than on a guess from the shape of the rows
    /// (finding H4).
    ResultRead {
        child: u64,
    },
    Error(String),
    /// A job this agent started began running in the background. The registry
    /// is where a job lives; this is only what tells the screen to look at it.
    JobStarted {
        job: u64,
        command: String,
    },
    /// A job ended, with the line its owner reads (`#c2 done: exit 0 · 3m12s ·
    /// cargo test — …`). Emitted by the job's own thread, so a job that ends
    /// while its owner naps still updates the screen.
    JobDone {
        job: u64,
        line: String,
    },
    /// The window the endpoint itself named when it rejected a request; the UI
    /// adopts it so the bar, `/context`, and the tool caps agree with the agent
    /// (finding B7). `source` travels with the number so the UI trusts it on the
    /// same terms the actor did.
    Context {
        tokens: usize,
        source: WindowSource,
    },
    /// The transcript was folded into a summary (context compaction); the
    /// conversation is now `[system, user(summary)]`.
    Compact {
        summary: String,
        /// Whether the fold was part of a run that is still going: the row
        /// falls back to the run's own phase, not to `done` (see
        /// [`AgentTree::compacted`](crate::app::AgentTree::compacted)).
        in_run: bool,
    },
    /// A fold started: accepted now, or parked until the next message boundary
    /// because a run is in flight. Emitted the moment the actor takes the
    /// request, so a `/compact` that is going to wait says so instead of
    /// looking like a command nobody heard (finding U11).
    ///
    /// `cancel` is the fold's own flag when the fold owns one; an idle fold has
    /// no run behind it, so this is the only handle a Stop can reach. A fold
    /// inside a run leaves it out: the run's flag is already the UI's.
    Compacting {
        why: Compacting,
        cancel: Option<Arc<AtomicBool>>,
    },
    /// The fold is over and the transcript is unchanged: the summarize call
    /// failed, or its reply could not be read as a summary. The `compacting…`
    /// phase must not outlive the request that justified it — a row claiming
    /// work forever after a failed fold is the same lie as a fold nobody can
    /// see. The cancelled case is [`AgentEvent::Stopped`], which already means
    /// "the work in flight was interrupted, the actor is alive".
    CompactingEnded {
        /// Whether the run the fold was part of is still in flight. A fold that
        /// came to nothing inside a run leaves that run's agent at work
        /// (`thinking…`, the phase a run wears between the request and the tool
        /// it names); an idle fold's agent is back at rest.
        in_run: bool,
    },
}

/// Shared by every actor: config, the UI channel, and the budgets.
pub struct AgentCtx {
    /// Shared so a runtime `/provider` / `/url` / `/model` / `/key` applies to
    /// every agent immediately. A handle rather than the lock itself: an actor
    /// reads it and may learn one window, and has no other write (finding B7).
    pub cfg: ConfigHandle,
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
    /// Every job this tree started, and the machine-wide lock. One registry for
    /// the whole tree, because a job is a fact about the *machine*: the budget
    /// is machine-wide, the lock is machine-wide, and `/new` kills what the old
    /// tree left running (`crate::jobs`).
    pub registry: Arc<jobs::Registry>,
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

    /// Adopt a window the endpoint named, for the whole tree at once.
    ///
    /// One call writes the copy the actors read and tells the UI, so the two
    /// cannot disagree (finding B7): the number cannot travel as a mutex write
    /// the UI never hears about. `Ok(false)` is the cell refusing a number —
    /// the human stated a window, or it was not plausible.
    fn learn_context(&self, id: u64, tokens: usize, source: WindowSource) -> Result<bool, String> {
        if !self.cfg.learn_context(tokens, source)? {
            return Ok(false);
        }
        self.emit(id, AgentEvent::Context { tokens, source });
        Ok(true)
    }
}

/// Per-actor state that survives across runs (children, summaries).
#[derive(Default)]
struct ActorState {
    children: HashMap<u64, Sender<AgentMsg>>,
    running: HashSet<u64>,
    /// The latest outcome of each child, with the run it came from, whether or
    /// not the model has read it. A completion is recorded the moment it
    /// arrives — even mid-batch — and folded into the transcript by
    /// [`fold_completions`].
    completed: HashMap<u64, Completion>,
    /// The run of each child whose outcome the model has read (via `wait_agents`
    /// or a folded line). A *later* run leaves this mark naming an older run, so
    /// the new outcome is announced; re-recording the run the mark names changes
    /// nothing, which is what keeps one piece of news from folding twice
    /// (`docs/findings.md` B24).
    delivered: HashMap<u64, u64>,
    /// The jobs this agent started and has not yet read a report about, and the
    /// reports it has read. The same three books as `running`/`completed`/
    /// `delivered` above, because a job's completion travels the same road as a
    /// child's: delivered once, folded into the transcript, never twice. The
    /// mark is the bare id, not a run: a job ends once, under an id nothing else
    /// reuses, so a report recorded again is the *same* report and there is no
    /// newer one to re-arm for (`docs/findings.md` B24).
    running_jobs: HashSet<u64>,
    done_jobs: HashMap<u64, JobReport>,
    delivered_jobs: HashSet<u64>,
    /// How many runs this actor has finished — the identity a parent records on
    /// `ChildDone { run, .. }`. It counts runs, not turns, and is incremented
    /// where the run's outcome is decided.
    runs: u64,
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
    /// How many identical rounds the previous run repeated before the loop
    /// guard stopped it. The next run opens by saying so, so a nudge can
    /// resume the agent instead of repeating the call that stopped it
    /// (finding H14).
    loop_stop: Option<usize>,
}

/// One child's run ending, as its parent keeps it: which run it was, and how it
/// ended. Identity is by run, not by outcome: two runs can carry the same error
/// text (and must be told about twice), while one run can be reported twice (and
/// must be folded once) — `Outcome` alone cannot separate those two cases
/// (`docs/findings.md` B24).
#[derive(Clone)]
struct Completion {
    run: u64,
    outcome: Outcome,
}

impl ActorState {
    /// The latest outcome recorded for a child. The run it came from is kept
    /// beside it (`completed`), so a reader that only wants "how did #N end"
    /// does not have to know about run identity.
    fn outcome(&self, id: u64) -> Option<&Outcome> {
        self.completed
            .get(&id)
            .map(|completion| &completion.outcome)
    }

    /// Whether `id`'s latest recorded outcome is one the model has not read.
    /// The one derivation of the `✉` mark: `agent_status` prints it, and the
    /// delivery roads consume it (a run recorded again under a mark that names
    /// it is not fresh). A child with no recorded outcome is not unread — there
    /// is nothing to read.
    fn unread(&self, id: u64) -> bool {
        match self.completed.get(&id) {
            Some(completion) => self.delivered.get(&id) != Some(&completion.run),
            None => false,
        }
    }

    /// Record a child's completion and say whether its line is *fresh* — one
    /// the model has not read yet: `(line, fresh)`. The record is kept either
    /// way (it is what makes a *later* run newsworthy), and a fresh line is
    /// marked read as it is handed back. The seven callers of "record and push
    /// a completion once" — the two in [`absorb`], `drain_mailbox`'s, both of
    /// [`fold_completions`]'s, and the two wait tools — differ only in what
    /// they do with the answer (`Fold::Run` or `Fold::Idle`, or return the
    /// line). Reading the same run again is not fresh: folding it would hand
    /// the model a line it has answered (`docs/findings.md` B24).
    fn record_child(&mut self, id: u64, run: u64, outcome: Outcome) -> (String, bool) {
        let line = note_completion(self, id, run, outcome);
        if self.delivered.get(&id) == Some(&run) {
            return (line, false);
        }
        self.delivered.insert(id, run);
        (line, true)
    }

    /// The same once-only delivery for a job: record the report and return its
    /// line when the model has not read it, or `None` when it has — a report
    /// recorded again is the *same* report, and folding it again would repeat a
    /// line the model has answered (`docs/findings.md` B24).
    fn record_job(&mut self, id: u64, line: String, news: bool) -> Option<String> {
        let line = note_job(self, id, line, news);
        self.delivered_jobs.insert(id).then_some(line)
    }
}

/// A job's completion, as its owner keeps it: the line the model reads, and
/// whether it was worth waking a napping agent for.
#[derive(Clone)]
struct JobReport {
    line: String,
    news: bool,
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

/// The handles every actor in one tree shares: the id counter it draws from,
/// the running-agent count it respects, and the job registry it starts commands
/// in. They travel together, because an agent given two of the three is living
/// in a tree of its own — ids that collide, or a job nobody else can see.
#[derive(Clone)]
pub struct TreeHandles {
    pub ids: Arc<AtomicU64>,
    pub live: Arc<AtomicU64>,
    pub jobs: Arc<jobs::Registry>,
}

/// The UI's handle on the root actor of one conversation.
pub struct RootHandle {
    /// The root's mailbox.
    pub tx: Sender<AgentMsg>,
    /// The cell every actor in this tree reads, so a runtime `/model` reaches
    /// them all — and so the UI can hold the same one (finding B7).
    pub cfg: ConfigHandle,
    /// Identifies this conversation in events; see `agent::next_conversation`.
    pub conversation: u64,
    /// The tree's id counter, so the UI can raise its floor to the highest id
    /// a leftover worktree already occupies (finding B1).
    pub ids: Arc<AtomicU64>,
    /// The tree-wide count of running agents, shared so a revived agent is
    /// counted against `MAX_AGENTS` like any other.
    pub live: Arc<AtomicU64>,
    /// The tree's job registry. The UI holds it for two reasons: to show what is
    /// running on the machine (a derived count and state, read from the one
    /// place jobs live), and to kill every process group on the way out.
    pub jobs: Arc<jobs::Registry>,
}

/// Start the root actor.
pub fn spawn(cfg: ConfigHandle, tx: Sender<Msg>, root: PathBuf) -> RootHandle {
    // The real endpoint, behind the seam: every agent in this tree calls it
    // through `AgentCtx::model`, children included.
    let model: Arc<dyn ModelClient> = Arc::new(HttpModel::new(cfg.clone()));
    let conversation = next_conversation();
    let ui: Arc<dyn Events> = Arc::new(Ui::new(tx, conversation));
    root_actor(cfg, model, ui, conversation, root)
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
    root_actor(ConfigHandle::own(cfg), model, events, conversation, root)
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
    shared: ConfigHandle,
    model: Arc<dyn ModelClient>,
    events: Arc<dyn Events>,
    conversation: ConversationId,
    root: PathBuf,
) -> RootHandle {
    // Root agent is id 0; children start at 1. The UI holds a clone so it can
    // raise the floor above leftover worktree ids.
    let ids = Arc::new(AtomicU64::new(1));
    let live = Arc::new(AtomicU64::new(0));
    let clock = Arc::new(clock::System);
    let registry = jobs::Registry::new(clock.clone(), events.clone(), ids.clone());
    let ctx = Arc::new(AgentCtx {
        cfg: shared.clone(),
        model,
        events,
        machine: Arc::new(Shell),
        clock,
        registry: registry.clone(),
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
        jobs: registry,
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

/// The branch an agent can still work on: one whose worktree is on disk.
///
/// A merged or discarded branch is not this agent's any more — its work is in
/// the main checkout, and its actor would commit into the human's own tree if
/// it kept the name (`revive` sends it to the root, where the work now is).
/// The node and the actor must read the same answer, or the row offers `/diff`
/// and `/merge` for a reclaimed directory while the actor runs in the checkout
/// (finding U13); this one function is where both get it.
pub fn live_branch(root: &Path, id: u64, branch: Option<String>) -> Option<String> {
    branch.filter(|_| git::worktree_path(root, id).exists())
}

/// Bring back an agent whose actor is gone — one restored from a stored session,
/// or a worktree found on disk — seeded with the transcript it had, and run it.
///
/// The human owns this agent, not the root: its completion goes to a dead
/// channel, so reviving a child never wakes the root with news it did not ask
/// for. It joins the same tree as the root, which is why its handles come in one
/// value (see [`TreeHandles`]).
pub fn revive(
    handles: TreeHandles,
    cfg: ConfigHandle,
    tx: Sender<Msg>,
    conversation: u64,
    root: PathBuf,
    spec: ReviveSpec,
) -> Sender<AgentMsg> {
    let TreeHandles {
        ids,
        live,
        jobs: registry,
    } = handles;
    let ReviveSpec {
        id,
        depth,
        brief,
        branch,
        messages,
    } = spec;
    // Its own worktree if it still exists, else the shared root — an agent whose
    // branch was merged continues in the main checkout, which is where its work
    // now is. The same decision the node's branch goes through
    // ([`live_branch`]), so the two cannot disagree (finding U13).
    let branch = live_branch(&root, id, branch);
    let isolated = branch
        .as_deref()
        .map(|_| git::worktree_path(&root, id))
        .filter(|path| path.exists());
    let ws_root = isolated.clone().unwrap_or_else(|| root.clone());
    let ws = Workspace::new(&ws_root).expect("workspace root must exist");
    let ws_root_str = ws.root_str();
    let ctx = Arc::new(AgentCtx {
        cfg: cfg.clone(),
        model: Arc::new(HttpModel::new(cfg)),
        events: Arc::new(Ui::new(tx, ConversationId(conversation))),
        machine: Arc::new(Shell),
        clock: Arc::new(clock::System),
        registry,
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
    // It comes back at rest, not running: a restart is not a request. Starting
    // a run here replayed every restored agent's task against the endpoint the
    // moment mush opened — thirteen agents, thirteen requests nobody asked for,
    // and a tree full of ✗ when the endpoint refused a replayed turn (a
    // thinking model rejects one without its `reasoning_content`). Whatever an
    // agent was doing when the process ended, its transcript is where it
    // resumes, and the human's next message is what starts it.
    start(actor, transcript, false);
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
            // The run it never got to take: its first, and only.
            run: 1,
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
        // This run is over, and this is its number: a parent that hears the
        // same run again has heard this report twice, while a run after it is
        // news even when the two read identically (`docs/findings.md` B24).
        state.runs += 1;
        let _ = actor.parent_tx.send(AgentMsg::ChildDone {
            id: actor.id,
            run: state.runs,
            outcome: outcome.clone(),
        });
        match outcome {
            Outcome::Failed(error) => actor.ctx.emit(actor.id, AgentEvent::Error(error)),
            Outcome::Stopped => actor.ctx.emit(actor.id, AgentEvent::Stopped),
            Outcome::Finished(_) => actor.ctx.emit(actor.id, AgentEvent::Done),
            // Unreachable from here, and deliberately listed rather than
            // swallowed by a wildcard: a cut-off run is one whose actor is
            // *gone*, so the only hand that can report it is the UI's
            // (`App::report_cut_off`), which files both the row's mark and the
            // parent's line. Nothing is emitted, because nothing here is alive
            // to have run.
            Outcome::CutOff => {}
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
            // A command parked *while a run was in flight* is folded in here,
            // before waiting: that run may have ended without reaching a
            // message boundary (a cancel mid-tool-call does), and a parked
            // command that waits for the human's *next* message is a command
            // they watched do nothing — a `/compact` whose status line never
            // ends, or words they typed that nobody reads until later.
            match fold_parked(actor, state, transcript) {
                Some(Fold::End) => return false,
                Some(Fold::Run) => break,
                _ => {}
            }
            // The human asked for a fold now. Not work to answer, so not a run:
            // the flag is honoured here, and by the next turn of a run already
            // in flight.
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
                Ok(command) => match absorb(actor, state, transcript, command) {
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
                match absorb(actor, state, transcript, command) {
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

/// Fold every command a run parked in `state.deferred`, in order, and report
/// what the last one meant for an actor that is now idle.
///
/// `None` is "nothing was parked". A `Run` is why this returns anything else:
/// words the human typed, or a completion that arrived, are work to answer even
/// though the run they interrupted is over.
fn fold_parked(
    actor: &Actor,
    state: &mut ActorState,
    transcript: &mut Vec<Message>,
) -> Option<Fold> {
    if state.deferred.is_empty() {
        return None;
    }
    let parked = std::mem::take(&mut state.deferred);
    let mut last = Fold::Idle;
    for command in parked {
        match absorb(actor, state, transcript, command) {
            Fold::End => return Some(Fold::End),
            Fold::Run => last = Fold::Run,
            Fold::Idle => {}
        }
    }
    Some(last)
}

/// What a command means for an actor that is not running.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Fold {
    /// Folded in; stay idle.
    Idle,
    /// There is work to do.
    Run,
    /// End this actor.
    End,
}

/// Fold one mailbox command into the actor's transcript.
fn absorb(
    actor: &Actor,
    state: &mut ActorState,
    transcript: &mut Vec<Message>,
    command: AgentMsg,
) -> Fold {
    match command {
        // An idle agent has nothing to cancel — but it may still own a job,
        // and a Stop aimed at an agent means "stop the work in flight".
        AgentMsg::Stop => {
            actor.ctx.registry.kill_owned(actor.id);
            Fold::Idle
        }
        AgentMsg::Shutdown => {
            actor.ctx.registry.kill_owned(actor.id);
            Fold::End
        }
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
            // Which run each line is asked about is the completion's own run:
            // a transcript that carries the line for the *current* record has
            // read that record, and one that carries an older line has not. The
            // line comes from `Outcome::line`, the one place that says all four
            // shapes, so a replayed `#N failed: …` (or a `#N stopped: …`, or a
            // `#N cut off: …`) is
            // recognised exactly like a `#N done: …` — the old scan knew only
            // the `done:` shape, so a failure read in the transcript looked
            // unread and was folded again (`docs/findings.md` B24).
            let announced: Vec<(u64, u64)> = state
                .completed
                .iter()
                .filter(|(id, completion)| {
                    let line = completion.outcome.line(**id);
                    transcript
                        .iter()
                        .any(|message| message.text().contains(&line))
                })
                .map(|(id, completion)| (*id, completion.run))
                .collect();
            // Adoption may only *add* marks, never remove one or move one
            // backwards: every line this actor folds is emitted as a `Message`
            // event, so the copy the UI hands back carries it — but that copy
            // can be older than the emit, and un-marking a delivery the model
            // has already read would inject the same result twice. A mark
            // naming a later run than the adopted transcript holds stays.
            for (id, run) in announced {
                state.delivered.entry(id).or_insert(run);
            }
            // The same question for jobs, answered on the line itself: it
            // carries the job's id, its exit status, its command and its tail,
            // so a transcript that holds it is a transcript that has read it.
            let announced_jobs: Vec<u64> = state
                .done_jobs
                .iter()
                .filter(|(_, report)| {
                    transcript
                        .iter()
                        .any(|message| message.text().contains(&report.line))
                })
                .map(|(id, _)| *id)
                .collect();
            state.delivered_jobs.extend(announced_jobs);
            Fold::Run
        }
        AgentMsg::Nudge(text) => {
            if worktree_gone(actor) {
                actor
                    .ctx
                    .emit(actor.id, AgentEvent::Notice(worktree_gone_line(actor.id)));
                return Fold::Idle;
            }
            transcript.push(Message::user(text));
            Fold::Run
        }
        // A parent's steering: work to answer, like a nudge, and told to the UI
        // like a completion — the human has no other way to see the words their
        // subagent was given.
        AgentMsg::Steer(text) => {
            if worktree_gone(actor) {
                actor
                    .ctx
                    .emit(actor.id, AgentEvent::Notice(worktree_gone_line(actor.id)));
                return Fold::Idle;
            }
            push_line(actor, transcript, text);
            Fold::Run
        }
        // The human asked for a fold now. Not work to answer, so not a run:
        // the flag is honoured by `wait_for_work`'s idle loop, and by the
        // next turn of a run already in flight.
        AgentMsg::Compact(messages) => {
            // An actor restored from a session starts with no transcript — the
            // UI holds the conversation until the human's next message hands it
            // over. A fold is not a run, so this is that hand-over: without it
            // the command folded nothing and said nothing.
            if transcript.is_empty() {
                *transcript = messages;
            }
            state.compact_requested = true;
            Fold::Idle
        }
        AgentMsg::ChildDone { id, run, outcome } => {
            // The parent ended (or napped) while a child still ran: waking it
            // with the completion restarts its run with the result folded in,
            // so an early End is not a lost result, it is a nap. The
            // completion counts as delivered because the model is about to
            // read it in this very run.
            let news = outcome.is_news();
            let (line, fresh) = state.record_child(id, run, outcome);
            // A run the model has already read is not news however often it is
            // reported: folding it here would hand the model a line it has
            // answered, and a result would even pay for a turn to repeat it
            // (`docs/findings.md` B24). The record itself is kept — it is what
            // makes a *later* run newsworthy.
            if !fresh {
                return Fold::Idle;
            }
            push_line(actor, transcript, line);
            // The line is in this parent's transcript now, so the child's row
            // stops claiming nobody has read it — the one moment that fact
            // changes hands, told to the UI from the actor that owns it
            // (finding H4). `fresh` is that moment's one home: every road that
            // hands the model a result asks `record_child` first.
            actor
                .ctx
                .emit(actor.id, AgentEvent::ResultRead { child: id });
            // A stopped child is the human's doing, not news that warrants
            // waking a napping parent into a fresh (paid) run: the line is in
            // the transcript for whenever the parent runs next.
            if news {
                Fold::Run
            } else {
                Fold::Idle
            }
        }
        AgentMsg::CommandDone { id, line, news } => {
            // `ChildDone` for a job: the same wake, the same once-only
            // delivery, the same "the human's stop is not a result" — and the
            // same silence when the report has already been read, so a report
            // recorded again cannot repeat a line the model has answered
            // (`docs/findings.md` B24).
            match state.record_job(id, line, news) {
                None => Fold::Idle,
                Some(line) => {
                    push_line(actor, transcript, line);
                    if news {
                        Fold::Run
                    } else {
                        Fold::Idle
                    }
                }
            }
        }
    }
}

/// Whether this actor is an isolated agent whose worktree has been reclaimed
/// (`/merge`, `/discard`, or a hand-run `git worktree remove`).
///
/// It must not run again: its file tools resolve their directory from the
/// workspace it was spawned with, so a write would recreate the dead path as a
/// plain directory that no surface — not `git status`, not `/diff`, not
/// `/merge` — can show, diff or land (finding S1). `App::deliver` refuses the
/// human's own message before it is sent; this is the backstop for every other
/// sender (a parent's `agent_control message`).
fn worktree_gone(actor: &Actor) -> bool {
    actor.branch.is_some() && !actor.ws.root().exists()
}

/// What such an actor reports when work arrives anyway: nothing ran, and where
/// to work instead.
fn worktree_gone_line(id: u64) -> String {
    format!(
        "agent #{id}'s worktree is gone (it was merged or discarded) — \
         work in the root or spawn a fresh agent; this message did not run"
    )
}

/// The tool schemas this agent's requests carry: the leaf set at the deepest
/// level, the full set above it.
///
/// One function, because every request an agent makes has to carry the *same*
/// schemas. The rendered prompt starts with the tool definitions, so a request
/// that drops them — the summarize call, or a wrap-up turn — shares no prefix
/// with the run it belongs to: the endpoint's prompt cache misses at the first
/// token and the whole history is prefilled again, which is the one cost
/// compaction exists to avoid, paid exactly when the history is largest. A
/// request that must not call tools says so with `tool_choice: "none"`, a
/// request parameter rather than prompt text.
fn tool_schemas(actor: &Actor) -> Vec<Value> {
    if actor.depth >= MAX_DEPTH {
        prompt::leaf_tool_schemas()
    } else {
        prompt::tool_schemas()
    }
}

/// The thinking knobs a request sends. Unstated, they are the provider's own:
/// DeepSeek asks for its thinking mode and `high`, every other endpoint gets
/// neither field. Stated (flag, environment, or home config), they are the
/// human's, wherever they pointed mush. One derivation, so the run's ask and
/// the fold cannot disagree about what "stated" means.
fn thinking_fields(cfg: &Config) -> (Option<Value>, Option<String>) {
    (
        cfg.thinking_enabled().then(|| json!({ "type": "enabled" })),
        cfg.reasoning_effort().map(str::to_string),
    )
}

/// The request shape both the run loop and the fold send: same model, same
/// sampling, same thinking knobs, and the reply cap carried under the name the
/// endpoint takes. `tool_choice` and `cap` are the only things a caller varies
/// from the run's turn — a wrap-up turn withdraws tools, a fold asks for a
/// smaller reply — so they travel in the arguments, and the swap between
/// `max_tokens` and `max_completion_tokens` lives here and nowhere else. A
/// second, hand-built request is how the fold came to send `max_tokens` to an
/// endpoint that rejects it and silently never compacted.
fn request<'a>(
    cfg: &'a Config,
    messages: &'a [Message],
    tools: &'a [Value],
    tool_choice: &'a str,
    cap: u32,
) -> ChatRequest<'a> {
    let (thinking, reasoning_effort) = thinking_fields(cfg);
    let mut request = ChatRequest {
        model: &cfg.model,
        messages,
        tools,
        tool_choice,
        stream: false,
        temperature: cfg.temperature(),
        max_tokens: cap,
        max_completion_tokens: None,
        thinking,
        reasoning_effort,
    };
    // The cap travels as `max_completion_tokens` only where that is the name
    // the endpoint takes (OpenAI's reasoning models reject the old one);
    // everywhere else keeps the field every OpenAI-compatible server
    // documents. One swap, for every caller.
    if cfg.uses_max_completion_tokens() {
        request.max_completion_tokens = Some(request.max_tokens);
        request.max_tokens = 0;
    }
    request
}

/// One turn's ask, with the bounded retry a transport hiccup gets: the pause
/// waits on the run's clock, the cancel flag is read between attempts, and
/// every retry is a line in this agent's transcript rather than a spinner that
/// looks stuck (finding B23). Everything the endpoint *answered* — a status, a
/// refusal, a body that did not parse — is returned unchanged, first time. Both
/// callers ask through this; only their error arms differ.
fn ask(
    actor: &Actor,
    request: &ChatRequest<'_>,
    cancel: &AtomicBool,
) -> Result<ChatResponse, ModelError> {
    retrying(
        actor.ctx.clock.as_ref(),
        cancel,
        |line| {
            actor
                .ctx
                .emit(actor.id, AgentEvent::Notice(line.to_string()))
        },
        || actor.ctx.model.chat(request, cancel),
    )
}

/// One round of the loop guard: did this batch repeat the last one, and does
/// that make the run a loop?
///
/// A batch that asked for something and was *refused* before anything ran is
/// not a repeat — nothing happened, so nothing is being repeated, and counting
/// it is what killed a fixer and an integrator whose only crime was retrying a
/// locked machine (finding H13). A refusal also clears the count: rounds that
/// did run before it are not evidence about this one.
fn count_round(last_batch: &mut String, repeats: &mut usize, batch: &str, refused: bool) {
    if batch == last_batch {
        if refused {
            *repeats = 0;
        } else {
            *repeats += 1;
        }
    } else {
        *repeats = 0;
        *last_batch = batch.to_string();
    }
}

/// One run: model turns → tool calls → results, until the model answers.
fn run_loop(
    actor: &Actor,
    state: &mut ActorState,
    messages: &mut Vec<Message>,
    cancel: &Arc<AtomicBool>,
) -> Result<Option<String>, String> {
    let schemas = tool_schemas(actor);
    // A run that follows a loop-stop opens with the guard's own words: the one
    // fact that lets the model do something different instead of repeating the
    // call that stopped the last run. Without it, a nudge did exactly what the
    // row promised and the guard stopped it again, so a resumable agent was not
    // (finding H14).
    if let Some(count) = state.loop_stop.take() {
        push_line(
            actor,
            messages,
            format!(
                "Your previous run was stopped as a loop: the same tool call repeated {count} \
                 times with nothing changed in between. Do not repeat that call — change what \
                 you do (different arguments, a different approach, or a wait for whatever it \
                 was blocked on), or finish the run and say what you need."
            ),
        );
    }
    // One learning attempt per run: a context-limit complaint teaches the
    // window, anything else is the run's error.
    let mut learned_context = false;

    // Loop detection: what justifies stopping a run early is a lack of
    // progress, not a turn count.
    let mut last_batch = String::new();
    let mut repeats = 0usize;
    // Whether the batch just run was refused before anything ran — the one
    // thing the guard must not read as a model repeating itself (finding H13).
    let mut refused_round = false;
    // Consecutive replies the endpoint cut off at the token cap.
    let mut cut_offs = 0usize;
    // What the endpoint itself counted, when it says: the UI's meter is an
    // estimate, and this is the one number that is not.
    let mut usage: Option<RunUsage> = None;

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
        drain_mailbox(actor, cancel, messages, state);
        if cancel.load(Ordering::SeqCst) {
            return Err(CANCELLED.to_string());
        }

        let cfg = actor.ctx.cfg.config()?;

        let budget = cfg.history_budget();
        // Approaching the context window — or asked for outright by a
        // `/compact` that arrived at this boundary: fold the conversation into
        // a summary instead of dropping old turns, so long-running tasks keep
        // their state. The summarize request re-sends the history, so only
        // fire while it still fits; beyond that, trimming stays the last
        // resort.
        if state.compact_requested || needs_compaction(messages, budget) {
            // A fold that came to nothing (a history nothing can be made of)
            // says so itself: the run carries on, and the phase the fold put on
            // the row goes back to what a run wears between the request and the
            // tool it names.
            compact_history(actor, &cfg, messages, cancel, state, true)?;
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

        // The schemas stay even on a wrap-up turn: the prompt starts with
        // them, so withdrawing them re-prefills a history that is at its
        // longest (see `tool_schemas`). `tool_choice` is what stops the
        // calls, and a call the model makes anyway is answered, not run — and
        // `auto` keeps models that ignore tools working: they simply answer.
        let request = request(
            &cfg,
            request_messages,
            &schemas,
            if wrap_up { "none" } else { "auto" },
            cfg.reply_cap(),
        );

        // One turn's ask: the retry policy and the retry line are [`ask`]'s,
        // the error arms below are the run's own.
        let reply = match ask(actor, &request, cancel) {
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
            // Unreachable and Transport reach the human the same way; the
            // difference between them is that a Transport failure was already
            // retried, and its message says so.
            Err(ModelError::Unreachable(error)) | Err(ModelError::Transport(error)) => {
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
                        // The cell decides whether the number is worth taking
                        // (a plausible one, and never over a window the human
                        // stated, finding A3); this call is also what tells the
                        // UI, so the learned window cannot reach one side and
                        // not the other (finding B7).
                        if actor
                            .ctx
                            .learn_context(actor.id, tokens, WindowSource::Complaint)?
                        {
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
        // Read before the reply's other parts are consumed below.
        if let Some(reported) = reply.usage.as_ref() {
            usage.get_or_insert_with(RunUsage::default).add(reported);
        }
        // `length` means the endpoint cut the reply off at `max_tokens` — with
        // a thinking model the cap can be spent before any visible text. Such a
        // reply is not a result: the text is partial and a tool call may be
        // half-written JSON, so the run fails loudly below instead of ending as
        // if the work were done.
        let finish = choice.finish_reason.as_deref().map(str::trim);
        let truncated = finish == Some("length");
        // Any other reason mush does not know — `content_filter` first among
        // them — is not a normal end either, and must not be read as one.
        let refused = refusal_reason(finish).map(str::to_string);

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
                        cfg.reply_cap()
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
                    cfg.reply_cap()
                ));
            }
            actor.ctx.emit(
                actor.id,
                AgentEvent::Notice(format!(
                    "reply cut off at {} tokens — asking for smaller steps",
                    cfg.reply_cap()
                )),
            );
            messages.push(Message::user(TRUNCATION_INSTRUCTION));
            continue;
        }

        if let Some(reason) = refused {
            // A refused reply may still carry tool calls (a filtering endpoint
            // emits the call, then stops). Answer them, never run them: half a
            // plan is not a plan, and a dangling call would poison every later
            // request in the conversation.
            for call in &tool_calls {
                let message = Message::tool(
                    call.id.clone(),
                    format!(
                        "error: the model's reply ended with finish_reason: {reason}; \
                         this call was not run"
                    ),
                );
                messages.push(message.clone());
                actor.ctx.emit(actor.id, AgentEvent::Message(message));
            }
            return Err(refusal_error(&reason));
        }

        // The same batch of calls, twice in a row with nothing changed in
        // between, means the model is repeating itself rather than working.
        // This — not a turn count — is the honest reason to stop a run early.
        // A batch the machine *refused* is the exception: nothing ran, so
        // nothing is repeating (finding H13).
        if !tool_calls.is_empty() {
            let batch = tool_calls
                .iter()
                .map(|call| format!("{}:{}", call.function.name, call.function.arguments))
                .collect::<Vec<_>>()
                .join("\n");
            count_round(&mut last_batch, &mut repeats, &batch, refused_round);
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
                // The next run starts with the guard's own words, so a nudge
                // can actually resume: without them the model repeats the call
                // that stopped it and is stopped again (finding H14).
                state.loop_stop = Some(count);
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
                report_usage(actor, usage);
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
            drain_mailbox(actor, cancel, messages, state);
            if cancel.load(Ordering::SeqCst) {
                return Err(CANCELLED.to_string());
            }
            let steered = messages.len() > before;
            // Children and jobs may have finished while we were working without
            // being waited on: deliver their lines and keep going instead of
            // ending. (Completions that arrive after this run returns wake the
            // idle actor instead — see actor_main.)
            if fold_completions(actor, state, messages) {
                continue;
            }
            // Answer the steering instead of ending the run without it: the
            // model has not seen those words yet.
            if steered {
                continue;
            }
            report_usage(actor, usage);
            return Ok(if content.is_empty() {
                None
            } else {
                Some(content)
            });
        }

        // Every call in a batch must be answered, or the transcript keeps an
        // assistant message whose tool calls dangle — which most servers then
        // reject for the rest of the conversation.
        //
        // Whether every call in this batch was refused *before it ran* is the
        // loop guard's business (H13), so it is collected as the batch runs and
        // handed to the next round's guard.
        let mut all_refused = true;
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
                None => Err(ToolError::Failed(format!("unknown tool `{named}`"))),
            };

            let (output, refused) = match result {
                Ok(output) => (output, false),
                Err(ToolError::Refused(why)) => (format!("error: {why}"), true),
                Err(ToolError::Failed(error)) => (format!("error: {error}"), false),
            };
            // Every call in this batch refused before it ran: the round counts
            // as nothing attempted, which is what keeps the loop guard from
            // condemning a model waiting on a locked machine (H13).
            all_refused &= refused;
            let tool_message = Message::tool(call.id.clone(), output);
            messages.push(tool_message.clone());
            actor.ctx.emit(actor.id, AgentEvent::Message(tool_message));
        }
        refused_round = all_refused;
        // Fold mailbox commands in at the message boundary, and honour a
        // cancellation now that every call has a result.
        drain_mailbox(actor, cancel, messages, state);
        if cancel.load(Ordering::SeqCst) {
            return Err(CANCELLED.to_string());
        }
        // The same boundary as a tool-free turn, so the same deliveries: a
        // parent that keeps calling tools hears its children's results here
        // rather than whenever it next stops calling them (§5.5). The call
        // comes *after* the batch's tool results, which is what keeps the
        // transcript a shape a strict server accepts.
        fold_completions(actor, state, messages);
    }

    Err(format!(
        "stopped after {RUNAWAY_TURNS} turns without finishing (runaway guard)"
    ))
}

/// The instruction appended to the request on the run's final turn.
const WRAP_UP_INSTRUCTION: &str = "\
You have reached this run's runaway guard, which is meant to be far past any \
real task. Stop using tools now — none of them will be run. Reply with a \
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
///
/// The `bool` in the `Ok` says whether the transcript was replaced. A fold that
/// came to nothing (a short history, a refusal mush cannot read as a summary)
/// emits its own [`AgentEvent::CompactingEnded`] instead, so neither caller has
/// to know how far it got: `in_run` is on both ends of the fold — a fold at a
/// run's message boundary is part of the run, one `compact_now` makes is not.
fn compact_history(
    actor: &Actor,
    cfg: &Config,
    messages: &mut Vec<Message>,
    cancel: &Arc<AtomicBool>,
    state: &mut ActorState,
    in_run: bool,
) -> Result<bool, String> {
    // Whether the human asked for this fold, as opposed to the window filling
    // on its own. Only the first is owed a line when there is nothing to do:
    // the automatic trigger would not have fired, so it has nothing to report.
    let asked = std::mem::take(&mut state.compact_requested);
    if !matches!(messages.first(), Some(message) if message.role == "system") {
        // Nothing to fold *and* nothing to replace: a fresh actor's transcript
        // is empty until its first `Run`, so the fold below has no `system` to
        // keep. The transcript is not made minimal by that, so this is not the
        // `system + one message` refusal — but a human who typed `/compact` is
        // owed the same answer, for the same reason: the bar says
        // `compacting #0…`, and silence there is indistinguishable from a fold
        // that quietly failed. The automatic trigger never reaches this arm
        // with an empty transcript (there is nothing to weigh), and it is
        // never told anything anyway.
        if asked && messages.is_empty() {
            actor
                .ctx
                .emit(actor.id, AgentEvent::Notice(NOTHING_TO_COMPACT.to_string()));
        }
        // Nothing was replaced, whether the transcript was empty or its opening
        // message was something else entirely: the fold ends here as every
        // other `Ok(false)` does, this arm being reached before a `Compacting`.
        actor
            .ctx
            .emit(actor.id, AgentEvent::CompactingEnded { in_run });
        return Ok(false);
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
        actor
            .ctx
            .emit(actor.id, AgentEvent::CompactingEnded { in_run });
        return Ok(false);
    }
    let actor_id = actor.id;
    // The human is told why this is happening: "nearly full" is a fact about
    // the automatic trigger, and saying it for a fold they asked for would be
    // a line about a condition that is not true. It travels as a phase, not as
    // a status line: a status is dropped for an agent that is not already busy
    // (`AgentTree::activity`), which is exactly the agent an idle `/compact`
    // runs on — the fold that never woke anything up (finding U11).
    let why = if asked {
        Compacting::Requested
    } else {
        Compacting::NearlyFull
    };
    actor.ctx.emit(
        actor_id,
        AgentEvent::Compacting {
            why,
            // A fold inside a run does not own a flag: the run's is already the
            // UI's, and handing over a second copy of it would let a Stop's
            // cleanup take the run's away. A fold from rest has no run, so this
            // Arc is the only handle anything can stop it with.
            cancel: if in_run { None } else { Some(cancel.clone()) },
        },
    );

    // Fold pending nudges/completions in first; a Stop cancels the run.
    drain_mailbox(actor, cancel, messages, state);
    if cancel.load(Ordering::SeqCst) {
        return Err(CANCELLED.to_string());
    }

    let mut folded = messages.clone();
    folded.push(Message::user(COMPACT_INSTRUCTION));
    // A summarize request: the run's own request, byte for byte, plus that one
    // user message. Same system prompt, same tools, same `tool_choice`, same
    // thinking knobs. What shapes the prompt shapes the endpoint's cache, and
    // the tools are the head of it — dropping them here saves no token, it
    // throws the whole cached history away at the moment the history is at its
    // largest, which is the one cost compaction exists to avoid. What stops the
    // model from calling a tool is the instruction, *persisted in the user
    // message* rather than encoded in a request field: a turn that says "reply
    // with the summary and call no tool" is a turn the model can take, while a
    // `tool_choice` the endpoint reads is not part of what the model is asked,
    // and a model that answers with a call instead of a summary is a model mush
    // cannot fold with either way.
    //
    // Sampling and length parameters are a separate matter: they are not prompt
    // text, so the summary's own cap costs no cache miss.
    let schemas = tool_schemas(actor);
    // The fold's cap is its own (`COMPACT_REPLY_TOKENS`) and it leaves
    // `tool_choice` at `auto`; everything else — the field the cap travels
    // under included — is [`request`]'s, shared with the run's own ask.
    let request = request(cfg, &folded, &schemas, "auto", COMPACT_REPLY_TOKENS);
    let reply = match ask(actor, &request, cancel) {
        Ok(reply) => reply,
        // A cancelled run is already ending; do not report a network failure.
        Err(ModelError::Cancelled) => return Err(CANCELLED.to_string()),
        // The run will fail on its real request anyway; surface it. A
        // `Transport` failure got its retries here, the same as the run's own
        // ask: compaction is a model call like any other.
        Err(ModelError::Unreachable(error))
        | Err(ModelError::Transport(error))
        | Err(ModelError::Refused(error)) => {
            return Err(format!("cannot reach {}: {error}", cfg.base_url));
        }
        Err(ModelError::Encode(error)) => return Err(format!("could not encode request: {error}")),
        // The endpoint complained, or answered something we cannot read: the
        // run will fail on its real request anyway, and a summary mush could
        // not make is not that failure. A human who *asked* for this fold is
        // owed the reason all the same — a `/compact` that quietly does nothing
        // is the hole this whole state exists to close (finding U11) — while the
        // automatic trigger, which the human never asked about, stays quiet.
        Err(error @ (ModelError::Status { .. } | ModelError::Malformed(_))) => {
            if asked {
                // The endpoint's own words, the way a run reports them: a
                // refusal and an unreadable body are different things, and the
                // human is the one who can act on either.
                let why = match &error {
                    ModelError::Status { status, body } => {
                        format!("the endpoint answered {status}: {body}")
                    }
                    ModelError::Malformed(what) => {
                        format!("the endpoint's reply could not be read: {what}")
                    }
                    _ => unreachable!("the arm above matched a status or a malformed reply"),
                };
                actor.ctx.emit(
                    actor.id,
                    AgentEvent::Notice(format!("could not compact: {why}")),
                );
            }
            // Either way the fold is over, and the phase it put on the row
            // goes — whether or not the human was told why.
            actor
                .ctx
                .emit(actor.id, AgentEvent::CompactingEnded { in_run });
            return Ok(false);
        }
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
        actor
            .ctx
            .emit(actor.id, AgentEvent::CompactingEnded { in_run });
        return Ok(false);
    }

    let system = messages[0].clone();
    *messages = vec![system, Message::user(prompt::compaction_message(&summary))];
    actor.ctx.emit(
        actor.id,
        AgentEvent::Compact {
            summary: summary.clone(),
            in_run,
        },
    );
    Ok(true)
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
        .config()
        .unwrap_or_else(|_| Config::new("http://127.0.0.1:1", "", None));
    // A flag of the fold's own, and not a private one: an idle agent has no run
    // for a Stop to cancel, so this Arc is the *only* handle anything can reach
    // the summarize request through. It travels to the UI with the
    // `Compacting` event, which puts it where Ctrl-C looks (`agent_cancel`) —
    // a fold that spins an hourglass while no key can stop it is worse than one
    // nobody can see.
    let cancel = Arc::new(AtomicBool::new(false));
    // A fold that landed needs nothing here: its `Compact` event is what the
    // pane, the session and the meter read. A fold that came to nothing emits
    // its own ending too, so only its *failures* are left to report.
    if let Err(error) = compact_history(actor, &cfg, transcript, &cancel, state, false) {
        // The human stopped it. A stop is its own event, not a failure: the
        // actor is alive and resumable, and the row must say which of the two
        // just happened.
        if error == CANCELLED {
            actor.ctx.emit(actor.id, AgentEvent::Stopped);
        } else {
            actor.ctx.emit(
                actor.id,
                AgentEvent::Notice(format!("could not compact: {error}")),
            );
            // The endpoint refused, could not be reached, or answered
            // something unreadable: the fold got as far as putting its
            // `Compacting` on the row, and the row must stop claiming it.
            actor
                .ctx
                .emit(actor.id, AgentEvent::CompactingEnded { in_run: false });
        }
    }
}

/// Fold in only what may appear between tool calls: cancellation and shutdown.
/// A child's completion is *recorded* here rather than folded — it must not be
/// missed while a call is in flight, and the line it becomes is a user message
/// that only belongs at a message boundary (`fold_completions`). Nudges and new
/// transcripts are *parked* for that same boundary: a user message between an
/// assistant's tool calls and their results makes strict servers reject the
/// whole conversation. They are parked in the actor's own state, never put
/// back in the mailbox: that is the queue this function is draining, so
/// re-sending would spin forever.
fn drain_signals(actor: &Actor, cancel: &AtomicBool, state: &mut ActorState) {
    for command in actor.rx.try_iter() {
        match command {
            AgentMsg::Stop => {
                cancel.store(true, Ordering::SeqCst);
                // A Stop means "stop the work in flight", and a job is work in
                // flight: whatever this agent started keeps running otherwise.
                actor.ctx.registry.kill_owned(actor.id);
            }
            AgentMsg::Shutdown => {
                cancel.store(true, Ordering::SeqCst);
                state.shutdown = true;
                actor.ctx.registry.kill_owned(actor.id);
            }
            AgentMsg::ChildDone { id, run, outcome } => {
                note_completion(state, id, run, outcome);
            }
            AgentMsg::CommandDone { id, line, news } => {
                note_job(state, id, line, news);
            }
            // A fold that arrived while a tool call was in flight: parked for
            // the next message boundary, like a nudge. Said out loud, because
            // this is the one window in which the request exists and nothing is
            // happening yet — the human who typed `/compact` has to be able to
            // see that it was taken and is waiting (finding U11).
            AgentMsg::Compact(_) => {
                state.compact_requested = true;
                actor.ctx.emit(
                    actor.id,
                    AgentEvent::Compacting {
                        why: Compacting::Parked,
                        cancel: None,
                    },
                );
            }
            parked => state.deferred.push(parked),
        }
    }
}

/// Fold pending mailbox commands into the current run: nudges become user
/// messages, stops set the cancel flag, child completions update the registry.
/// Everything parked by `drain_signals` goes in first, in order.
fn drain_mailbox(
    actor: &Actor,
    cancel: &AtomicBool,
    messages: &mut Vec<Message>,
    state: &mut ActorState,
) {
    let parked = std::mem::take(&mut state.deferred);
    for command in parked.into_iter().chain(actor.rx.try_iter()) {
        match command {
            // The human's own words: the UI echoed them before sending, so the
            // actor folds them in without telling the UI to add them again.
            AgentMsg::Nudge(text) => messages.push(Message::user(text)),
            // A parent's steering was never echoed anywhere: this is the only
            // way it reaches the human's copy of this agent's transcript.
            AgentMsg::Steer(text) => push_line(actor, messages, text),
            // A message-boundary job like a nudge: the transcript is folded
            // into a summary at the next turn, never between an assistant's
            // tool calls and their results. The transcript the request carries
            // is for an actor that has none (see `absorb`); mid-run, the
            // transcript this actor owns is the newer one.
            AgentMsg::Compact(_) => state.compact_requested = true,
            AgentMsg::Stop => {
                cancel.store(true, Ordering::SeqCst);
                actor.ctx.registry.kill_owned(actor.id);
            }
            AgentMsg::Shutdown => {
                // Cancel now, and remember: the run ends, and so does the actor.
                cancel.store(true, Ordering::SeqCst);
                state.shutdown = true;
                actor.ctx.registry.kill_owned(actor.id);
            }
            AgentMsg::ChildDone { id, run, outcome } => {
                note_completion(state, id, run, outcome);
            }
            // A job's report is folded into the transcript as a user message:
            // the model reads `#c2 done: exit 0 · …` in the next request, and
            // the line is marked delivered so it is never injected twice. A
            // report the model has *already* read is not folded again however
            // often it is recorded (`docs/findings.md` B24), which is what makes
            // a replayed record cost nothing.
            AgentMsg::CommandDone { id, line, news } => {
                if let Some(line) = state.record_job(id, line, news) {
                    push_line(actor, messages, line);
                }
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

/// Record a child's completion and return the line the model reads.
///
/// A *newer run* supersedes the recorded outcome — a child that was stopped and
/// then nudged finishes later, and the stale `stopped` must not outlive the
/// result. The same run recorded again is not newer: it changes nothing, and in
/// particular it does **not** clear the delivery mark. Clearing it there is
/// precisely what let one outcome fold twice — the mark is a fact about what the
/// model read, and hearing the same report again cannot make it unread
/// (`docs/findings.md` B24: the defect was the unconditional
/// `state.delivered.remove(&id)` that used to end this function).
fn note_completion(state: &mut ActorState, id: u64, run: u64, outcome: Outcome) -> String {
    state.running.remove(&id);
    let line = outcome.line(id);
    if state.completed.get(&id).map(|completion| completion.run) != Some(run) {
        state.completed.insert(id, Completion { run, outcome });
    }
    line
}

/// The same bookkeeping for a job: it is no longer running, and its report is
/// the line the model reads. A job ends once, under an id nothing else reuses,
/// so a report recorded again is the *same* report: the delivery mark stands,
/// and it is not cleared here. Clearing it unconditionally is what let a job's
/// line fold twice (`docs/findings.md` B24, `note_completion`'s twin).
fn note_job(state: &mut ActorState, id: u64, line: String, news: bool) -> String {
    state.running_jobs.remove(&id);
    state.done_jobs.insert(
        id,
        JobReport {
            line: line.clone(),
            news,
        },
    );
    line
}

/// Fold one line into this actor's transcript *and* tell the UI to put it in
/// its own copy — one fact, two readers.
///
/// A line that reaches `messages` alone is a line the human cannot see
/// (`docs/findings.md` B20) and, because the UI's copy is what an idle `Run`
/// hands back, a delivery that adoption then re-arms and the model reads
/// twice. Every fold of a completion or a steering line goes through here, so
/// the two copies cannot drift apart in either direction.
fn push_line(actor: &Actor, messages: &mut Vec<Message>, text: String) {
    let message = Message::user(text);
    messages.push(message.clone());
    actor.ctx.emit(actor.id, AgentEvent::Message(message));
}

/// Fold into the transcript every completion the run has heard about but the
/// model has not read — a child's summary, or a job's report — and say whether
/// any of them is *news* (a result, which the model still has to answer).
///
/// This is the one home of "a result is never lost just because nobody called
/// `wait_agents` in time" (docs/mush.md §5.5), and it runs at *every* message
/// boundary: after a batch of tool results, and on a tool-free turn. It used to
/// run only on the tool-free turn, so a parent in a long chain of tool calls —
/// sixty turns of reading, editing and running the gate — never heard that its
/// child had finished, however long the child had been done.
///
/// A completion is a legal user message exactly here, after the assistant's
/// tool calls and their results. A *nudge* is not: the human's words between an
/// assistant's calls and their results are the shape strict servers reject, so
/// nudges keep parking for the tool-free boundary (`drain_mailbox`).
fn fold_completions(actor: &Actor, state: &mut ActorState, messages: &mut Vec<Message>) -> bool {
    // Jobs first: they are the newest actors, and a job's line is only news if
    // the job ended on its own — one mush killed is the human's or the model's
    // own doing, and its line waits for the next run instead of paying for one.
    let jobs: Vec<(u64, String, bool)> = state
        .done_jobs
        .iter()
        .filter(|(job, _)| !state.delivered_jobs.contains(job))
        .map(|(job, report)| (*job, report.line.clone(), report.news))
        .collect();
    let mut news = false;
    for (job, line, job_news) in jobs {
        if let Some(line) = state.record_job(job, line, job_news) {
            push_line(actor, messages, line);
        }
        news |= job_news;
    }
    // A child's completion is always worth a turn: the model has to read a
    // summary it asked for, even of a child that was stopped (`Outcome::is_news`
    // decides that only for an *idle* actor, where the run it wakes has a
    // price).
    let children: Vec<(u64, u64, Outcome)> = state
        .completed
        .iter()
        .filter(|(child, completion)| state.delivered.get(*child) != Some(&completion.run))
        .map(|(child, completion)| (*child, completion.run, completion.outcome.clone()))
        .collect();
    for (child, run, outcome) in children {
        let (line, fresh) = state.record_child(child, run, outcome);
        if fresh {
            push_line(actor, messages, line);
            actor.ctx.emit(actor.id, AgentEvent::ResultRead { child });
        }
        news = true;
    }
    news
}

/// Why a tool call produced no result.
///
/// `Refused` is the machine saying *not now* — the lock is held, the job budget
/// is full — so nothing ran and nothing changed. `Failed` is the call itself
/// going wrong. The loop guard reads the difference: a batch of refusals is not
/// a model repeating itself, and counting it as one killed an integrator and a
/// fixer whose only mistake was retrying a locked machine (finding H13).
#[derive(Debug)]
enum ToolError {
    Refused(String),
    Failed(String),
}

impl From<String> for ToolError {
    fn from(error: String) -> Self {
        ToolError::Failed(error)
    }
}

fn exec_tool(
    actor: &Actor,
    state: &mut ActorState,
    tool: ToolName,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<String, ToolError> {
    // `run_command` is the one tool that can be refused before anything runs
    // (the machine lock, the job budget), so it returns the verdict itself;
    // every other tool either ran or failed.
    let answer = match tool {
        ToolName::RunCommand => return run_command(actor, state, args, cancel),
        ToolName::SpawnAgent => spawn_tool(actor, state, args),
        ToolName::WaitAgents => wait_tool(actor, state, cancel, args),
        ToolName::AgentStatus => status_tool(state),
        ToolName::AgentControl => control_tool(state, args),
        ToolName::CommandStatus => command_status_tool(actor),
        ToolName::CommandControl => command_control_tool(actor, args),
        ToolName::WaitCommands => wait_commands_tool(actor, state, cancel, args),
        // The file tools read and write the workspace directly.
        ToolName::ListFiles | ToolName::ReadFile | ToolName::WriteFile | ToolName::EditFile => {
            let cfg = actor
                .ctx
                .cfg
                .config()
                .unwrap_or_else(|_| Config::new("http://127.0.0.1:1", "", None));
            direct_tool(&actor.ws, tool, args, &cfg)
        }
    };
    answer.map_err(ToolError::Failed)
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
    // Why isolation was not available, if it was asked for and refused. One
    // reason, two readers: the child's brief carries it for the model (which
    // wrote `isolated: true` and has to know it did not get its own worktree),
    // and the parent's pane carries it for the human — who asked for two
    // siblings that would not touch the same files, and whose rows would
    // otherwise look exactly like a child that never asked to be isolated.
    // Every way `worktree_add` refuses takes this road: no repository, no
    // commit to fork from, git's own refusal to add the worktree.
    let mut degraded: Option<String> = None;
    let (child_ws, branch) = if isolated {
        // A private worktree on `mush/<id>`, based on the parent's branch (or
        // HEAD). The reason it cannot be made is reported either way, so
        // isolation degrades to the shared workspace instead of failing the
        // delegation.
        match git::worktree_add(&ctx.root, id, actor.branch.as_deref()) {
            Ok((path, branch)) => match Workspace::new(&path) {
                Ok(child_ws) => (child_ws, Some(branch)),
                // Isolation is best-effort: degrade to the shared workspace
                // rather than fail the delegation outright.
                Err(error) => {
                    degraded = Some(error.to_string());
                    (actor.ws.clone(), None)
                }
            },
            Err(reason) => {
                degraded = Some(reason);
                (actor.ws.clone(), None)
            }
        }
    } else {
        (actor.ws.clone(), None)
    };
    let note = match &degraded {
        Some(reason) => format!(" (isolated unavailable: {reason}; running in place)"),
        None => String::new(),
    };
    // The branch the parent will need to land the work, said where it is born:
    // the parent chose the worktree, and a child whose branch it never learned
    // is a child it cannot diff or merge by hand (finding H1).
    let on = branch
        .as_deref()
        .map(|branch| format!(" on {branch}"))
        .unwrap_or_default();

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

    // The same fact, to the human. The child's brief below tells the model; a
    // row with no branch is not an explanation, and two "isolated" siblings
    // editing one workspace while the human believes they are apart is the
    // failure this line exists to prevent. It is a notice on the parent — the
    // pane the human is reading when they asked for this child — and it says
    // which child, because a parent may spawn several.
    if let Some(reason) = &degraded {
        ctx.emit(
            parent,
            AgentEvent::Notice(format!(
                "#{id} isolated unavailable: {reason} — it shares this workspace"
            )),
        );
    }

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
        "spawned agent #{id}{on} · runs until it stops calling tools · wait_agents returns its summary"
    ))
}

/// Who is waiting behind a blocking tool call.
///
/// The human's words (a `Nudge`, or a whole transcript the UI sent because it
/// believed this agent idle) and a parent's steering (`Steer`) both end the
/// wait — words the model does not see until the deadline are not steering —
/// but the sentence the model reads names which, because "the human wrote to
/// you" is not true of a sibling's note.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Waiting {
    Human,
    Parent,
}

/// Whether anything said to this agent is waiting behind a blocking tool call,
/// and who said it. A blocking call is the one place a message would otherwise
/// sit unread for as long as the call takes, so this is what ends the wait —
/// see `wait_tool`.
fn parked_message(state: &ActorState) -> Option<Waiting> {
    let mut said = None;
    for command in &state.deferred {
        match command {
            AgentMsg::Nudge(_) => return Some(Waiting::Human),
            AgentMsg::Run(messages)
                if messages
                    .last()
                    .is_some_and(|message| message.role == "user") =>
            {
                return Some(Waiting::Human)
            }
            AgentMsg::Steer(_) => said = Some(Waiting::Parent),
            _ => {}
        }
    }
    said
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
    let mut results = WaitResults {
        // Pure: which children hold a result this waiter can be given. Asking
        // must not read anything — `deliver` is what the answer actually hands
        // over, and only for the results this call returns (a poll that marked
        // every ready child would swallow bodies the model never saw).
        is_ready: &mut |state, id| state.completed.contains_key(&id),
        // One home decides whether this answer is the model's first read of the
        // run (`record_child`). A fresh result is delivered in full — the shape
        // the fold uses, so adoption still recognises it — and the child's `✉`
        // goes out with it. A result the model has already read is answered
        // with its digest and said to be old, never with the body again
        // (finding H15: a wait must not report the past as news, and must not
        // replay a report the model has answered).
        deliver: &mut |state, id| {
            let Some(completion) = state.completed.get(&id).cloned() else {
                return format!("#{id} (no result recorded)");
            };
            let digest = completion.outcome.digest(id);
            let (body, fresh) = state.record_child(id, completion.run, completion.outcome);
            if fresh {
                actor
                    .ctx
                    .emit(actor.id, AgentEvent::ResultRead { child: id });
                body
            } else {
                format!("{digest} (already read — no new run since)")
            }
        },
    };
    wait_for_results(
        actor,
        state,
        cancel,
        args,
        &candidates,
        jobs::Waited::Agents,
        &mut results,
    )
}

/// The two questions a wait asks about one candidate, in one value — so the
/// wait itself stays a small function rather than an argument list.
///
/// `is_ready` is a *pure* question (`true` = a result exists) and never touches
/// the records; `deliver` turns one chosen result into the line the model
/// reads, and is called only for the results a call actually returns. The split
/// is the fix for a wait that asked about every child: it used to
/// render-and-mark each ready one while returning only the first, so one wait
/// silently marked results the model was never handed as read.
struct WaitResults<'a> {
    is_ready: &'a mut dyn FnMut(&ActorState, u64) -> bool,
    deliver: &'a mut dyn FnMut(&mut ActorState, u64) -> String,
}

/// The one blocking wait `wait_agents` and `wait_commands` both run: poll the
/// mailbox, honour a cancellation, notice the human, stop at the deadline, and
/// return whatever results are ready. The two tools differ only in what "a
/// result" is — a child's outcome or a job's report line — which the caller
/// supplies ([`WaitResults`]), so the subtle parts (the deadline, the parked
/// human, the cancel) exist once.
fn wait_for_results(
    actor: &Actor,
    state: &mut ActorState,
    cancel: &AtomicBool,
    args: &Value,
    candidates: &[u64],
    waiting_for: jobs::Waited,
    results: &mut WaitResults<'_>,
) -> Result<String, String> {
    // `all` asks for every result instead of the first one: the first is what an
    // orchestrator wants the moment one delegate is free, and `all` is what it
    // wants before it proceeds with the whole set.
    let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
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
        // that must notice a cancellation (and a completion) promptly.
        drain_signals(actor, cancel, state);
        if cancel.load(Ordering::SeqCst) {
            return Err(CANCELLED.to_string());
        }
        // And it must notice anything said to it. Parking those words is not
        // enough when the wait can last the whole timeout: the model would not
        // see them until the thing it was waiting on finished, which is the
        // opposite of steering. The wait ends, the words stay parked for the
        // next message boundary, and the model answers them in this run.
        if let Some(waiting) = parked_message(state) {
            let who = match waiting {
                Waiting::Human => "the human wrote to you",
                Waiting::Parent => "your parent sent you a message",
            };
            return Ok(format!(
                "interrupted — {who} while you waited; it is in \
                 your transcript. Answer it; your {} are still running. Use {} again when \
                 you need a result.",
                waiting_for.noun(),
                waiting_for.tool()
            ));
        }
        let mut finished: Vec<u64> = Vec::new();
        let mut waiting = Vec::new();
        for id in candidates {
            if (results.is_ready)(state, *id) {
                finished.push(*id);
            } else {
                waiting.push(waiting_for.label(*id));
            }
        }
        // What this call returns: the first ready child, or every one of them
        // once nothing is left running. Only these are delivered.
        let take = if all {
            if waiting.is_empty() {
                finished.len()
            } else {
                0
            }
        } else {
            usize::from(!finished.is_empty())
        };
        if take > 0 {
            let answers: Vec<String> = finished
                .iter()
                .take(take)
                .map(|id| (results.deliver)(state, *id))
                .collect();
            return Ok(answers.join("\n"));
        }
        if let Some(deadline) = deadline {
            if clock.now() >= deadline {
                // What is known is returned, and what is not is named: a wait
                // that timed out is not a wait that lost the results.
                let note = format!("wait timed out — {} still running", waiting.join(", "));
                if finished.is_empty() {
                    return Ok(note);
                }
                let answers: Vec<String> = finished
                    .iter()
                    .map(|id| (results.deliver)(state, *id))
                    .collect();
                return Ok(format!("{}\n{note}", answers.join("\n")));
            }
        }
        clock.sleep(Duration::from_millis(50));
    }
}

/// `agent_status`: what this agent's children are doing, as a bounded listing.
///
/// A listing is not a delivery. Each child's outcome is rendered as a digest —
/// the first line, cut at [`DIGEST_COLUMNS`], with the size of the whole — and
/// the results the model has not read yet wear `✉`. The body itself reaches the
/// model exactly once, through the fold, the wake, or an explicit wait (the
/// roads that ask [`ActorState::record_child`]). Printing the bodies here is
/// what made a parent that polled its children re-read every report on every
/// call; the `✉` tells it what is still worth waiting for, which is the fact a
/// listing owes a reader.
fn status_tool(state: &ActorState) -> Result<String, String> {
    if state.children.is_empty() {
        return Ok("no child agents".to_string());
    }
    let mut lines = Vec::new();
    let mut ids: Vec<u64> = state.children.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        // The same `✉` the tree rows carry (H4): a result nobody has read.
        let unread = if state.unread(id) { "✉ " } else { "" };
        match state.outcome(id) {
            // Each state gets its own mark: a stopped child was neither
            // finished (✓) nor failed (✗), and a parent that cannot tell them
            // apart treats a stop as a result.
            Some(outcome @ (Outcome::Finished(_) | Outcome::Failed(_))) => {
                lines.push(format!("{unread}{}", outcome.digest(id)));
            }
            Some(Outcome::Stopped) => lines.push(format!(
                "{unread}#{id} ⊘ stopped — idle and resumable (agent_control message resumes it)"
            )),
            // A run that never ended. Its own line, because the parent's next
            // move depends on it: there is no result coming and the work may be
            // sitting uncommitted (finding H2).
            Some(Outcome::CutOff) => lines.push(format!(
                "{unread}#{id} ⚠ cut off — the run never ended; nothing was committed"
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
    // Whether the words are read *now* or at the child's next message boundary
    // is the parent's own book (`running`), and the reply says which: "messaged
    // agent #N" claimed delivery with no way to tell a child that resumes from
    // one that is mid-run (finding H5).
    let at_rest = !state.running.contains(&id);
    // A dead mailbox means the child is gone; saying "stopping" anyway would
    // have the model wait on a result that can never arrive.
    let sent = match action.as_str() {
        "stop" => cmd.send(AgentMsg::Stop).map_err(|_| (id, "stop")),
        "message" => {
            let text = tools::arg_string(args, "text")?;
            cmd.send(AgentMsg::Steer(text)).map_err(|_| (id, "message"))
        }
        other => return Err(format!("unknown action `{other}` (stop or message)")),
    };
    match sent {
        Ok(()) if action == "stop" => Ok(format!("stopping agent #{id}")),
        Ok(()) if at_rest => Ok(format!(
            "messaged agent #{id} — it was at rest, so this resumes it"
        )),
        Ok(()) => Ok(format!(
            "messaged agent #{id} — it is mid-run, so it reads this at its next step"
        )),
        Err((id, _)) => Err(format!("agent #{id} is gone")),
    }
}

/// `command_status`: what this agent's commands are doing, live from the one
/// registry that holds them. A running job is read through its own window, so
/// this is always current and never a copy.
fn command_status_tool(actor: &Actor) -> Result<String, String> {
    Ok(actor.ctx.registry.status_for(actor.id))
}

/// `command_control`: stop a job this agent started.
fn command_control_tool(actor: &Actor, args: &Value) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing `id`".to_string())?;
    let action = tools::arg_string(args, "action")?;
    match action.as_str() {
        "stop" => actor.ctx.registry.stop(actor.id, id),
        other => Err(format!("unknown action `{other}` (stop)")),
    }
}

/// `wait_commands`: the same wait as `wait_agents`, over jobs. A job's report
/// arrives in the owner's mailbox like a child's completion, so waiting is the
/// same act — and `all` means the same thing.
fn wait_commands_tool(
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
    let mut candidates: Vec<u64> = if ids.is_empty() {
        // Every job this agent started: the running ones and the ones whose
        // reports it has already been given.
        let mut mine: Vec<u64> = state
            .running_jobs
            .iter()
            .chain(state.done_jobs.keys())
            .copied()
            .collect();
        mine.sort_unstable();
        mine.dedup();
        mine
    } else {
        ids
    };
    candidates.dedup();
    if candidates.is_empty() {
        return Ok("no jobs to wait for".to_string());
    }
    let unknown: Vec<String> = candidates
        .iter()
        .filter(|id| !state.running_jobs.contains(id) && !state.done_jobs.contains_key(id))
        .map(|id| jobs::label(*id))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "no such job(s): {} — command_status lists yours",
            unknown.join(", ")
        ));
    }
    let mut results = WaitResults {
        // Pure, like the agents' half: asking about a job must not hand its
        // report over — only the answer this call returns does that.
        is_ready: &mut |state, id| state.done_jobs.contains_key(&id),
        deliver: &mut |state, id| {
            // The report is marked delivered as it is handed over, so the fold
            // at the next message boundary cannot inject the same line again.
            // Asking a second time answers with the job's line again — a wait
            // is "tell me what happened", and the model that asks twice gets an
            // answer twice rather than a silence it has to interpret. (A job's
            // report is already bounded — exit status and the end of its
            // output — so repeating it is not the replay a child's report is.)
            let Some(report) = state.done_jobs.get(&id).cloned() else {
                return jobs::label(id);
            };
            state
                .record_job(id, report.line.clone(), report.news)
                .unwrap_or(report.line)
        },
    };
    wait_for_results(
        actor,
        state,
        cancel,
        args,
        &candidates,
        jobs::Waited::Jobs,
        &mut results,
    )
}

/// Commit whatever an isolated agent left in its worktree, so the branch that
/// `/diff`, `/merge`, and `/discard` name actually carries the work. Returns the
/// short revision when something was committed, `None` when the run changed
/// nothing. The subject is built above, next to the id, brief and outcome it is
/// made of.
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

/// How long a sibling's command queues for a machine lock held by another
/// agent before the refusal stands. Long enough to ride out a short timing
/// run, short enough that a wave is not silently parked behind a ten-minute
/// one; the wait runs on the actor's clock, so it is cancel-aware and a test
/// reaches the bound without waiting (finding H13).
const LOCK_QUEUE: Duration = Duration::from_secs(30);
/// The queue's polling slice.
const LOCK_POLL: Duration = Duration::from_millis(200);

/// Wait, bounded and cancel-aware, for a machine lock held by another agent.
///
/// A refusal is honest but it is not a wait: "do not retry this call" leaves
/// the model with nothing to do, and a model that retries it anyway is doing
/// exactly what the loop guard counts (finding H13). So a sibling queues for
/// [`LOCK_QUEUE`] first and is refused only when the lock outlasts that. The
/// holder is re-read every slice, so a refusal names whoever holds it at the
/// end, not whoever held it at the start.
fn wait_for_machine(actor: &Actor, cancel: &AtomicBool) -> Result<(), jobs::Held> {
    let deadline = actor.ctx.clock.now() + LOCK_QUEUE;
    loop {
        match actor.ctx.registry.machine_free_for(actor.id) {
            Ok(()) => return Ok(()),
            Err(held) => {
                if cancel.load(Ordering::SeqCst) || actor.ctx.clock.now() >= deadline {
                    return Err(held);
                }
            }
        }
        actor.ctx.clock.sleep(LOCK_POLL);
    }
}

/// Append the fact that a command ran beside a sibling's exclusive run.
///
/// The root is exempt from the lock (finding H13), and an exemption that is not
/// said is exactly the kind of silent state this tree keeps finding: the model
/// can decide to distrust a timing-sensitive result, or wait next time.
fn beside_note(text: String, held: Option<&jobs::Held>) -> String {
    match held {
        None => text,
        Some(held) => format!(
            "{text}\n(ran while #{} held the machine for an exclusive command ({}) — \
             timing-sensitive results from its run may be perturbed)",
            held.agent,
            truncate(&held.command, 40)
        ),
    }
}

fn run_command(
    actor: &Actor,
    state: &mut ActorState,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<String, ToolError> {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing `command`".to_string())?;
    let detach = args.get("detach").and_then(Value::as_bool).unwrap_or(false);
    let exclusive = args
        .get("exclusive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let registry = actor.ctx.registry.clone();
    // Two decisions, both made *before* a process exists: whether this command
    // may use the machine at all (the lock), and whether a long one has
    // somewhere to go (the budget). A command that cannot be watched or may not
    // run must never be started. Neither is a failure of the work, so both are
    // `Refused` — the model is not looping when it asks again later (H13).
    // The lock coordinates *siblings*: the root is the human's own hands — a
    // human may run any command in another terminal while mush works — so the
    // root commands beside a held lock and is *told*, not refused. Being blind
    // for the duration of a sibling's benchmark cost the orchestrator its only
    // lever (finding H13). A root *exclusive* command is refused like anyone's:
    // two claims to own the machine is the one thing the lock exists to
    // prevent. A sibling queues, bounded, instead of refusing on sight.
    let mut beside: Option<jobs::Held> = None;
    if let Err(held) = registry.machine_free_for(actor.id) {
        if actor.id == AgentId::ROOT.0 {
            if exclusive {
                return Err(ToolError::Refused(Refused::Machine(held).message(actor.id)));
            }
            beside = Some(held);
        } else if let Err(held) = wait_for_machine(actor, cancel) {
            return Err(ToolError::Refused(Refused::Machine(held).message(actor.id)));
        }
    }
    if detach && !registry.has_room() {
        return Err(ToolError::Refused(Refused::Budget.message(actor.id)));
    }
    if exclusive {
        registry
            .take_machine(actor.id, command)
            .map_err(|held| ToolError::Refused(Refused::Machine(held).message(actor.id)))?;
    }
    // `detach: true` asks for a job from the start: the model knows it started
    // a server, and waiting sixty seconds to be told so is not an answer. This
    // is the *only* path that spawns here — every other command is spawned once,
    // by `run_shell`, which is the thing that watches it.
    if detach {
        let spawned = actor
            .ctx
            .machine
            .spawn(&ShellCommand {
                command,
                root: actor.ws.root(),
            })
            .map_err(|error| {
                if exclusive {
                    registry.release_machine(actor.id);
                }
                error
            })?;
        let id = detach_now(
            actor,
            &registry,
            jobs::Launch::started(
                actor.id,
                command.to_string(),
                exclusive,
                actor.my_tx.clone(),
                spawned,
            ),
        )?;
        state.running_jobs.insert(id);
        return Ok(beside_note(detached_line(id), beside.as_ref()));
    }
    // A foreground command that outlives `CMD_DETACH_AFTER` becomes a job too —
    // unless there is no room for one, in which case the 120 s timeout and the
    // output cap are the whole story.
    let detach = if registry.has_room() {
        Detach::Job {
            registry: &registry,
            exclusive,
        }
    } else {
        Detach::No
    };
    let report = run_shell(
        command,
        actor.ws.root(),
        Duration::from_secs(CMD_TIMEOUT_SECS),
        detach,
        cancel,
        actor,
        state,
    );
    // The tool call's own claim ends here — and *only* its own: a command that
    // auto-detached has handed the lock to the job it became, which is what
    // keeps it for the job's whole life (§5.6) and gives it up in
    // `Registry::finish`.
    if exclusive {
        registry.release_machine(actor.id);
    }
    report.map(|text| beside_note(text, beside.as_ref()))
}

/// Whether a foreground command may become a job when it outlives
/// `CMD_DETACH_AFTER`, and what it hands over if it does.
#[derive(Clone, Copy)]
enum Detach<'a> {
    /// It may not: the machine-wide budget is full, so the timeout is the only
    /// bound and the job registry is not involved.
    No,
    /// Hand its process group to the registry, lock and all.
    Job {
        registry: &'a Arc<jobs::Registry>,
        exclusive: bool,
    },
}

impl Detach<'_> {
    /// When the watcher stops treating this as a tool call.
    fn after(&self) -> Option<Duration> {
        match self {
            Detach::No => None,
            Detach::Job { .. } => Some(jobs::CMD_DETACH_AFTER),
        }
    }
}

/// Hand a running command to the registry as a job and return its id.
///
/// The `launch` is built by the caller, because where the process group comes
/// from is the caller's fact: one that has just been started (`detach: true`),
/// or the one a foreground call was holding when it outlived
/// `CMD_DETACH_AFTER` (see `jobs::Launch::held`).
fn detach_now(
    actor: &Actor,
    registry: &Arc<jobs::Registry>,
    launch: jobs::Launch,
) -> Result<u64, ToolError> {
    let command = launch.command.clone();
    // A launch can be refused after the checks above — a sibling may have
    // taken the lock in between — and that is the machine saying "not now",
    // not the call going wrong (H13).
    let id = registry.launch(launch).map_err(|refused| {
        registry.release_machine(actor.id);
        ToolError::Refused(refused.message(actor.id))
    })?;
    actor.ctx.emit(
        actor.id,
        AgentEvent::JobStarted {
            job: id,
            command: command.clone(),
        },
    );
    Ok(id)
}

/// The answer a detach gives the model, in the words the spec uses. The job's
/// id is in it because every later tool call about it (status, stop, wait) needs
/// the id, and the model has nothing else to go on.
fn detached_line(id: u64) -> String {
    format!(
        "[still running — detached as {}; you will be told when it finishes]",
        jobs::label(id)
    )
}

/// Hard ceiling on what one command may write to its scratch files. The model
/// only ever sees the first `CMD_CAP` bytes, so a command that gets here is not
/// communicating, it is running away — and it must not fill the disk. The size
/// is checked every few milliseconds (see `wait_bounded`), so a fast writer can
/// overshoot by a few tens of MB before the kill lands. It lives in
/// `crate::jobs` beside the other rule a job and a tool call share.
use crate::jobs::CMD_OUTPUT_LIMIT;

/// Why a command stopped running.
enum Ended {
    /// It ended by itself, with this exit code (`-1` when a signal ended it).
    Exited(i32),
    TimedOut,
    Cancelled,
    TooMuchOutput,
    /// It outlived `CMD_DETACH_AFTER` and is now a job; the caller hands the
    /// still-running process group over instead of killing it.
    Detached,
}

/// Run a shell command in `root` and return a report the model can read.
///
/// The command itself is the [`Machine`]'s: how to start one, how it is
/// watched, and the ways it stops (its time is up, a Stop arrived, it wrote too
/// much, it outlived `CMD_DETACH_AFTER`) are this function's, which is what
/// makes all four assertable with a scripted machine and a scripted clock.
fn run_shell(
    command: &str,
    root: &Path,
    timeout: Duration,
    detach: Detach<'_>,
    cancel: &AtomicBool,
    actor: &Actor,
    state: &mut ActorState,
) -> Result<String, ToolError> {
    let spawned = actor.ctx.machine.spawn(&ShellCommand { command, root })?;
    // From here to the end of the call the command is the registry's as much as
    // this actor's: quitting mush, a `Stop` and `/new` all reach it (finding
    // S4). It is *not* a job — no id, no line, no budget — it is a tool call
    // whose result the model is waiting for, which is exactly why nothing was
    // watching it before.
    let mut running = actor.ctx.registry.hold(actor.id, spawned);
    let ended = wait_bounded(&mut running, timeout, detach.after(), cancel, actor, state)?;
    if matches!(ended, Ended::Detached) {
        if let Detach::Job {
            registry,
            exclusive,
        } = detach
        {
            let id = detach_now(
                actor,
                registry,
                jobs::Launch::held(
                    actor.id,
                    command.to_string(),
                    exclusive,
                    actor.my_tx.clone(),
                    running,
                ),
            )?;
            state.running_jobs.insert(id);
            return Ok(detached_line(id));
        }
    }
    // A kill that arrived from *outside* the watcher — a quit, which is what
    // finding S4 is about, or the registry's half of a `Stop` — must not be
    // reported as the command's own exit: `-1` is a signal nobody asked about.
    let ended = ending(ended, running.stopped());
    let (stdout, stderr) = running.output(CMD_CAP);

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
        // Only reachable without a `Detach::Job`, which returns above.
        Ended::Detached => report.push_str(&format!("[timed out after {}s]", timeout.as_secs())),
    }
    Ok(report)
}

/// How a command's end is read once the watcher has returned.
///
/// Three ways a foreground command stops must not be confusable in the report:
///
/// - A command the watcher stopped — its time was up, it wrote past the output
///   cap, or a `Stop` reached the run — is reported with *that* reason. The
///   watcher's own kill sets the same flag an outside one does, which is why
///   the arm below only touches an exit.
/// - A command killed from *outside* the watcher — quitting mush (`kill_all`),
///   `/new`, or the registry's half of a `Stop` — is reported as a cancel. The
///   process died from the signal mush sent it, and `-1` handed to the model as
///   an exit code would read as the command's own doing; this is the arm
///   finding S4's fix needs.
/// - A command that ended by itself keeps its real exit status, signal deaths
///   included: nobody asked for those.
fn ending(ended: Ended, stopped_from_outside: bool) -> Ended {
    match ended {
        Ended::Exited(_) if stopped_from_outside => Ended::Cancelled,
        ended => ended,
    }
}

/// Wait for a command, stopping it when its time is up, a cancellation arrives,
/// it writes too much, or it has outlived `CMD_DETACH_AFTER`.
///
/// The four ways out are decided by [`jobs::stopping`] plus the detach deadline,
/// so the foreground watcher and a job's own thread cannot disagree about what
/// ends a command.
fn wait_bounded(
    job: &mut dyn Job,
    timeout: Duration,
    detach_after: Option<Duration>,
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
        let waited = actor.ctx.clock.now().saturating_duration_since(started);
        if detach_after.is_some_and(|after| waited > after) {
            // Not killed: the process keeps its group, and the registry takes
            // over watching it (`run_shell`'s caller does the handover).
            return Ok(Ended::Detached);
        }
        if let Some(stopped) = jobs::stopping(
            job.written(),
            waited,
            Some(timeout),
            cancel.load(Ordering::SeqCst),
        ) {
            job.kill();
            return Ok(match stopped {
                jobs::Stopped::TimedOut => Ended::TimedOut,
                jobs::Stopped::Cancelled => Ended::Cancelled,
                jobs::Stopped::TooMuchOutput => Ended::TooMuchOutput,
            });
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

/// [`read_args`], defanged. The arguments are the model's own text and a file
/// name is a file name, and this label is painted raw — one span in the
/// transcript (`docs/mush.md` §4.5 R4), one activity line in the tree — so a
/// `path` of `…\u{1b}]0;PWNED` would otherwise repaint the terminal it is drawn
/// on. The one reading both callers share is the one place to do it.
fn summarize(args: &Value) -> String {
    sanitize(&read_args(args))
}

/// What the arguments say, before the text is made safe to paint.
fn read_args(args: &Value) -> String {
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
    // The orchestration tools, which name none of the three above: the tools a
    // human watching a tree most needs to read are exactly the ones that
    // rendered as a bare `⚙ agent_control` — R4's "`⚙ name summarized-args`"
    // vacuous for the calls that steer the run.
    //
    // `agent_control {id, action, text?}` and `command_control {id, action}`
    // share a shape, so they share an arm: the id is the target and the action
    // is what is being done to it.
    if let Some(id) = args.get("id").and_then(Value::as_u64) {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        let mut label = format!("#{id}");
        if !action.is_empty() {
            label.push(' ');
            label.push_str(action);
        }
        if let Some(text) = args.get("text").and_then(Value::as_str) {
            label.push_str(&format!(" \"{}\"", truncate(&first_line(text), 30)));
        }
        return label;
    }
    // `wait_agents {ids?, timeout?}` and `wait_commands {ids?, timeout?}`: which
    // ids are being waited on, and how long. An empty list is not "nothing" —
    // the schema reads it as *all* of them.
    if let Some(ids) = args.get("ids").and_then(Value::as_array) {
        let list: Vec<String> = ids
            .iter()
            .filter_map(Value::as_u64)
            .map(|id| format!("#{id}"))
            .collect();
        let mut label = if list.is_empty() {
            "all".to_string()
        } else {
            list.join(" ")
        };
        if let Some(timeout) = args.get("timeout").and_then(Value::as_u64) {
            label.push_str(&format!(" {timeout}s"));
        }
        return label;
    }
    if let Some(timeout) = args.get("timeout").and_then(Value::as_u64) {
        return format!("{timeout}s");
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
    use crate::model::RETRY_ATTEMPTS;
    use mush_core::config::{ReasoningEffort, ThinkingMode};
    // The fold's trigger is core's formula, not this file's: the test below
    // crosses it instead of restating it.
    use mush_core::transcript::compaction_trigger;
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

    /// The label is painted raw — one span in the transcript, one activity line
    /// in the tree — and a file name is a file name: a model that puts an escape
    /// sequence in an argument must not repaint the terminal it is drawn on.
    #[test]
    fn a_summary_carries_no_escape_from_an_argument() {
        assert_eq!(
            summarize_args(r#"{"path":"src/\u001b]0;PWNED\u0007main.rs"}"#),
            "src/main.rs"
        );
        assert_eq!(
            summarize_args(r#"{"command":"cat log\u001b[2J\u001b[H"}"#),
            "cat log"
        );
        assert_eq!(
            summarize_args(r#"{"brief":"do\u0007 this\rplease"}"#),
            "do this please"
        );
    }

    /// The orchestration tools carry no path, command or brief, so they rendered
    /// as a bare `⚙ agent_control` — for exactly the calls an orchestrator uses
    /// to steer a tree, which is where a human most needs to know *whom*.
    #[test]
    fn an_orchestration_call_summarizes_its_target() {
        assert_eq!(
            summarize(&json!({"id": 2, "action": "message", "text": "keep the steps small"})),
            "#2 message \"keep the steps small\""
        );
        assert_eq!(
            summarize(&json!({"id": 3, "action": "stop"})),
            "#3 stop",
            "command_control and agent_control share one shape"
        );
        assert_eq!(summarize(&json!({"ids": [1, 2]})), "#1 #2");
        assert_eq!(
            summarize(&json!({"ids": [], "timeout": 60})),
            "all 60s",
            "an empty id list is the schema's `all`, not nothing"
        );
        assert_eq!(summarize(&json!({"timeout": 30})), "30s");
        // A tool with no arguments has nothing to summarize, and says so by
        // summarizing nothing: `command_status`, `agent_status`.
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
        let (actor, _mailbox) = test_actor("parked-nudge");
        let mut state = ActorState::default();
        let mut transcript = vec![Message::system("sys")];
        state
            .deferred
            .push(AgentMsg::Nudge("said once".to_string()));

        let carried = vec![Message::system("sys"), Message::user("said once")];
        assert!(matches!(
            absorb(&actor, &mut state, &mut transcript, AgentMsg::Run(carried)),
            Fold::Run
        ));
        assert_eq!(transcript.len(), 2, "the UI's transcript wins");
        assert!(state.deferred.is_empty(), "the parked copy is gone");

        // The next message boundary has nothing left to inject, or the model
        // would answer the same sentence twice.
        let mut messages = Vec::new();
        drain_mailbox(&actor, &AtomicBool::new(false), &mut messages, &mut state);
        assert!(messages.is_empty(), "no duplicate user message");
        let _ = fs::remove_dir_all(actor.ws.root());
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
        note_completion(&mut state, 1, 1, Outcome::Stopped);
        state.children.insert(2, tx.clone());
        note_completion(&mut state, 2, 1, Outcome::Finished("did the thing".into()));
        state.children.insert(3, tx);
        note_completion(&mut state, 3, 1, Outcome::Failed("no route".into()));

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
        // A run that never ended names itself too, and says the one thing the
        // parent has to act on: its work is uncommitted (finding H2).
        let cut_off = Outcome::CutOff.line(3);
        assert!(cut_off.starts_with("#3 cut off"), "{cut_off}");
        assert!(cut_off.contains("nothing was committed"), "{cut_off}");
        assert!(cut_off.contains("never ended"), "{cut_off}");
        assert!(!cut_off.starts_with("#3 done"), "{cut_off}");
        assert!(!cut_off.starts_with("#3 stopped"), "{cut_off}");
    }

    /// A stop is the human's doing, not news: it must not wake a napping parent
    /// into a fresh (paid) run. A finish is news and must wake it.
    #[test]
    fn a_stop_does_not_wake_a_napping_parent_but_a_finish_does() {
        let (actor, _mailbox) = test_actor("napping");
        let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let mut state = ActorState::default();
        state.children.insert(1, tx.clone());
        let mut messages = vec![Message::system("you are mush")];

        assert!(matches!(
            absorb(
                &actor,
                &mut state,
                &mut messages,
                AgentMsg::ChildDone {
                    id: 1,
                    run: 1,
                    outcome: Outcome::Stopped
                }
            ),
            Fold::Idle
        ));
        assert!(messages.last().unwrap().text().contains("stopped"));

        assert!(matches!(
            absorb(
                &actor,
                &mut state,
                &mut messages,
                AgentMsg::ChildDone {
                    id: 1,
                    run: 2,
                    outcome: Outcome::Finished("all done".into())
                }
            ),
            Fold::Run
        ));
    }

    /// A cut-off run is news for the same reason a stop is not: the parent is
    /// waiting for a result that will never come, and the work it was waiting on
    /// may be sitting uncommitted, so it has to be woken and told rather than
    /// left to assume (finding H2).
    #[test]
    fn a_cut_off_child_wakes_a_napping_parent() {
        let (actor, _mailbox) = test_actor("cut-off-wakes");
        let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let mut state = ActorState::default();
        state.children.insert(1, tx);
        let mut messages = vec![Message::system("you are mush")];

        assert!(matches!(
            absorb(
                &actor,
                &mut state,
                &mut messages,
                AgentMsg::ChildDone {
                    id: 1,
                    run: CUT_OFF_RUN,
                    outcome: Outcome::CutOff
                }
            ),
            Fold::Run
        ));
        let line = messages.last().unwrap().text();
        assert!(line.starts_with("#1 cut off"), "{line}");
    }

    /// The subject written for a commit and the subject read back from git must
    /// agree, or a worktree found on startup is shown as the wrong work.
    #[test]
    fn a_commit_subject_round_trips_through_git() {
        let cases = [
            (Outcome::Finished("done".into()), Committed::Finished),
            (Outcome::Stopped, Committed::Stopped),
            (Outcome::CutOff, Committed::CutOff),
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

    /// The subject is the brief's *first line*, cut at a word boundary with a
    /// trailing `…` (finding S8(i)). The docs' `mush #N: <brief>` is the
    /// imprecise side: a subject cannot be unbounded, and a subject that ends
    /// mid-word (`isolated w…`) neither reads as English nor matches the brief.
    #[test]
    fn a_subject_is_the_briefs_first_line_cut_on_a_word_boundary() {
        // A second line is a body, not a subject.
        assert_eq!(
            commit_subject(
                7,
                "first line\nsecond line here",
                &Outcome::Finished("done".into())
            ),
            "mush #7: first line"
        );
        // A first line past the budget loses whole words, never half a word.
        let brief = "port the parser module to the new configuration format and then run the tests";
        let subject = commit_subject(7, brief, &Outcome::Finished("done".into()));
        let cut = subject.strip_prefix("mush #7: ").unwrap();
        let kept = cut.strip_suffix('…').expect("a cut subject says so");
        assert!(
            brief[kept.len()..].starts_with(' '),
            "the cut must fall between words: {cut:?} of {brief:?}"
        );
        assert!(kept.starts_with("port the parser"), "{cut}");
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

    /// The other half of the same rule, and the half a hidden fold used to
    /// break: adoption must not *re-arm* a delivery that already happened.
    ///
    /// The two threads are independent, so the copy the UI hands back can be
    /// older than the event that carried the folded line: the human wrote after
    /// the fold but the App sent its transcript before it drained that event.
    /// Un-marking the delivery there folds the same result into the model's
    /// transcript a second time — the model answers news it has answered.
    ///
    /// The transcript stays exactly as adopted (it is what the App has), so
    /// nothing is invented here; the two copies converge at the next adoption.
    #[test]
    fn adoption_does_not_re_arm_a_delivery_that_already_happened() {
        let (actor, _mailbox) = test_actor("stale-copy");
        let mut state = ActorState::default();
        note_completion(&mut state, 1, 1, Outcome::Finished("did the thing".into()));
        let mut messages = vec![Message::system("you are mush")];
        assert!(fold_completions(&actor, &mut state, &mut messages));

        // The App's copy as it was before it drained the fold's own event.
        let stale = vec![Message::system("you are mush")];
        let mut adopted = stale.clone();
        assert!(matches!(
            absorb(&actor, &mut state, &mut adopted, AgentMsg::Run(stale)),
            Fold::Run
        ));
        assert!(
            state.delivered.get(&1) == Some(&1),
            "the model has read it, whatever the copy says"
        );
        assert!(
            !fold_completions(&actor, &mut state, &mut adopted),
            "and it is not read twice"
        );
        assert_eq!(adopted.len(), 1, "nothing is invented into the transcript");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// Steering a subagent is visible (`docs/findings.md` B22). The words a
    /// parent's `agent_control message` puts in a child's transcript are
    /// emitted to the UI, which routes them into that child's transcript — the
    /// human reads what their model was told, and the session file keeps it.
    ///
    /// The human's own words are deliberately *not* emitted: the UI echoed them
    /// before sending them, and a second copy would put the same sentence in
    /// that transcript twice.
    #[test]
    fn a_parents_steering_reaches_the_child_and_the_ui() {
        let (actor, events, _mailbox) = recording_actor("steer");
        let mut state = ActorState::default();
        let mut messages = vec![Message::system("you are mush")];
        let text = "stop spawning subagents";

        // Sent the way `agent_control message` sends it.
        let (child_tx, child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child_tx);
        let sent = exec_tool(
            &actor,
            &mut state,
            ToolName::AgentControl,
            &json!({ "id": 1, "action": "message", "text": text }),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            sent,
            "messaged agent #1 — it was at rest, so this resumes it"
        );
        match child_rx.try_recv() {
            Ok(AgentMsg::Steer(words)) => assert_eq!(words, text),
            _ => panic!("steering must travel as steering, not as the human's own words"),
        }
        // The reply names the other road too: a child that is mid-run reads the
        // words at its next message boundary, not now (finding H5).
        state.running.insert(1);
        let sent = exec_tool(
            &actor,
            &mut state,
            ToolName::AgentControl,
            &json!({ "id": 1, "action": "message", "text": "keep going" }),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(sent.contains("mid-run"), "{sent}");
        assert!(
            matches!(child_rx.try_recv(), Ok(AgentMsg::Steer(words)) if words == "keep going"),
            "the words are queued either way"
        );

        // Folding it in: the model reads the line, and the UI was told to put it
        // in the same transcript.
        let folded = absorb(
            &actor,
            &mut state,
            &mut messages,
            AgentMsg::Steer(text.into()),
        );
        assert!(matches!(folded, Fold::Run), "steering is work to answer");
        assert_eq!(messages.last().unwrap().text(), text);
        let ui = ui_copy(&events);
        assert!(
            ui.iter().any(|message| message.text() == text),
            "the human sees the words their model read: {ui:?}"
        );

        // The mid-run road is the same road.
        _mailbox.send(AgentMsg::Steer("keep going".into())).unwrap();
        drain_mailbox(&actor, &AtomicBool::new(false), &mut messages, &mut state);
        let ui = ui_copy(&events);
        assert!(
            ui.iter().any(|message| message.text() == "keep going"),
            "a mid-run fold is visible too: {ui:?}"
        );

        // And a parked steering message survives adoption: it is not in the
        // UI's copy yet — unlike the human's own words, which that copy echoes.
        let mut parked = ActorState::default();
        parked.deferred.push(AgentMsg::Steer("kept".into()));
        let mut transcript = vec![Message::system("you are mush")];
        absorb(
            &actor,
            &mut parked,
            &mut transcript,
            AgentMsg::Run(vec![Message::system("you are mush")]),
        );
        assert!(
            matches!(parked.deferred.first(), Some(AgentMsg::Steer(words)) if words == "kept"),
            "steering is not the UI's to echo, so adoption must not drop it"
        );

        // The human's own typing is never emitted: the UI has it already.
        let before = events.len();
        absorb(
            &actor,
            &mut state,
            &mut messages,
            AgentMsg::Nudge("my own words".into()),
        );
        assert_eq!(events.len(), before, "a typed nudge is not echoed twice");
        assert_eq!(messages.last().unwrap().text(), "my own words");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// Steering that arrives during a blocking wait ends it — words the model
    /// does not see until the deadline are not steering — and the sentence it
    /// reads names who wrote, because "the human wrote to you" is not true of a
    /// parent's note.
    #[test]
    fn a_parents_steering_ends_a_wait_and_names_the_speaker() {
        let (actor, mailbox) = test_actor("steer-wait");
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        // A child that has not finished: the state a parent waits in.
        let (child, _child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child);
        state.running.insert(1);
        mailbox.send(AgentMsg::Steer("stop".into())).unwrap();

        let started = Instant::now();
        let result = wait_tool(&actor, &mut state, &cancel, &json!({ "timeout": 600 })).unwrap();

        assert!(
            result.contains("your parent sent you a message"),
            "{result}"
        );
        assert!(
            result.contains("still running"),
            "and does not claim the child finished: {result}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the wait ended on the message, not the timeout ({:?})",
            started.elapsed()
        );
        assert!(
            matches!(state.deferred.first(), Some(AgentMsg::Steer(words)) if words == "stop"),
            "and the words stay parked for the boundary that folds them in"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A child that was stopped and then resumed finishes later; the stale
    /// `stopped` must not outlive the result, or the parent waits on a stop
    /// forever. The two outcomes are two *runs*: the same run reported again is
    /// the same news, a later run is not (finding B24).
    #[test]
    fn a_later_finish_replaces_a_stale_stop() {
        let mut state = ActorState::default();
        note_completion(&mut state, 1, 1, Outcome::Stopped);
        assert_eq!(state.outcome(1), Some(&Outcome::Stopped));
        let line = note_completion(&mut state, 1, 2, Outcome::Finished("done now".into()));
        assert_eq!(
            state.outcome(1),
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
        let folded = absorb(&actor, &mut state, &mut messages, AgentMsg::Run(fresh));
        assert!(matches!(folded, Fold::Run));
        assert_eq!(
            roles(&messages),
            ["system", "user", "assistant", "tool", "user"]
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The other half of a delivery, on every road the line can take into a
    /// parent's transcript: the UI is told that the parent has *read* it, which
    /// is the one fact the child's row cannot derive from its own phase — and
    /// the one the human's question is about ("did #2 see #6?"). The actor owns
    /// it (`delivered`), so the actor is what says so (finding H4).
    #[test]
    fn a_result_the_parent_has_read_is_reported_to_the_ui() {
        let (actor, events, _mailbox) = recording_actor("read-report");
        let read = |events: &Recorder| {
            events
                .events_for(AgentId(7))
                .into_iter()
                .filter_map(|event| match event {
                    AgentEvent::ResultRead { child } => Some(child),
                    _ => None,
                })
                .collect::<Vec<u64>>()
        };

        // 1. The idle road: a napping parent is woken by the completion and
        //    folds it before the run it starts.
        let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let mut state = ActorState::default();
        state.children.insert(1, tx.clone());
        state.children.insert(2, tx.clone());
        state.children.insert(3, tx);
        let mut messages = vec![Message::system("you are mush")];
        assert!(matches!(
            absorb(
                &actor,
                &mut state,
                &mut messages,
                AgentMsg::ChildDone {
                    id: 1,
                    run: 1,
                    outcome: Outcome::Finished("did it".into())
                }
            ),
            Fold::Run
        ));
        assert_eq!(read(&events), vec![1], "the wake-up is a reading");

        // 2. The mid-run road: the completion is recorded while a tool call is
        //    in flight and folded at the next message boundary.
        note_completion(&mut state, 2, 1, Outcome::Finished("and this".into()));
        assert!(fold_completions(&actor, &mut state, &mut messages));
        assert_eq!(read(&events), vec![1, 2], "the fold is a reading too");

        // 3. The road the model asked for: `wait_agents` hands the result over
        //    itself, so the mark goes out with the line.
        note_completion(&mut state, 3, 1, Outcome::Finished("waited for".into()));
        let cancel = AtomicBool::new(false);
        let waited = wait_tool(&actor, &mut state, &cancel, &json!({ "ids": [3] })).unwrap();
        assert!(waited.contains("#3 done"), "{waited}");
        assert_eq!(read(&events), vec![1, 2, 3], "and so is a wait");
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
        note_completion(&mut state, 1, 1, Outcome::Finished("did the thing".into()));
        state.delivered.insert(1, 1);
        let fresh = vec![
            Message::system("you are mush"),
            Message::user("task"),
            assistant_calling(&["a"]),
            Message::tool("a", "#1 done: did the thing"),
        ];

        assert!(matches!(
            absorb(&actor, &mut state, &mut messages, AgentMsg::Run(fresh)),
            Fold::Run
        ));
        assert!(
            state.delivered.contains_key(&1),
            "the model reads it in the transcript, so it is already delivered"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// Exactly once, and visible, across an idle `Run`: the human writes, the
    /// UI hands its own transcript back, and the actor adopts it.
    ///
    /// Both halves are asserted against the UI's copy built from the events, as
    /// the App builds it — because before this the folded line was pushed into
    /// the actor's `messages` alone (`docs/findings.md` B20): the human never
    /// saw what the model was told, and the copy the UI handed back could not
    /// contain the line, so `absorb` un-marked the delivery and the model read
    /// the same result twice.
    #[test]
    fn a_child_completion_is_delivered_once_across_an_idle_run() {
        let (actor, events, _mailbox) = recording_actor("once-child");
        let mut state = ActorState::default();
        note_completion(
            &mut state,
            1,
            1,
            Outcome::Finished("wrote the parser".into()),
        );
        let mut messages = vec![Message::system("you are mush")];

        assert!(fold_completions(&actor, &mut state, &mut messages));

        let line = "#1 done: wrote the parser";
        assert_eq!(messages.last().unwrap().text(), line);
        let ui = ui_copy(&events);
        assert!(
            ui.iter().any(|message| message.text() == line),
            "the human's copy holds what the model was told: {ui:?}"
        );

        let mut adopted = ui.clone();
        assert!(matches!(
            absorb(&actor, &mut state, &mut adopted, AgentMsg::Run(ui)),
            Fold::Run
        ));
        assert!(
            !fold_completions(&actor, &mut state, &mut adopted),
            "a delivery that already happened is not re-armed by adoption"
        );
        assert_eq!(
            adopted
                .iter()
                .filter(|message| message.text() == line)
                .count(),
            1,
            "one completion, one line"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The same road for a job (§5.6: a job's completion is `ChildDone`'s twin),
    /// folded by `absorb` when its owner was idle.
    #[test]
    fn a_job_report_is_delivered_once_across_an_idle_run() {
        let (actor, events, _mailbox) = recording_actor("once-job");
        let mut state = ActorState::default();
        state.running_jobs.insert(1);
        let mut messages = vec![Message::system("you are mush")];
        let line = "#c1 done: exit 0 · 3m12s · cargo test — test result: ok";

        let folded = absorb(
            &actor,
            &mut state,
            &mut messages,
            AgentMsg::CommandDone {
                id: 1,
                line: line.into(),
                news: true,
            },
        );
        assert!(matches!(folded, Fold::Run), "a result is work to answer");
        assert_eq!(messages.last().unwrap().text(), line);

        let ui = ui_copy(&events);
        assert!(
            ui.iter().any(|message| message.text() == line),
            "the job's line reaches the human too: {ui:?}"
        );

        let mut adopted = ui.clone();
        assert!(matches!(
            absorb(&actor, &mut state, &mut adopted, AgentMsg::Run(ui)),
            Fold::Run
        ));
        assert!(!fold_completions(&actor, &mut state, &mut adopted));
        assert_eq!(
            adopted
                .iter()
                .filter(|message| message.text() == line)
                .count(),
            1,
            "one job completion, one line"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The human's shape, seen live (`docs/findings.md` B24): a parent folds a
    /// child's failure, so the model has read it — and then the *same run* is
    /// reported again, which used to clear the delivery mark (`note_completion`
    /// ended with an unconditional `state.delivered.remove`) and hand the model
    /// the failure it had just answered a second time.
    ///
    /// A *later* run of the same child is a different matter, and is covered by
    /// `a_second_run_failing_the_same_way_is_news_again`: that one is genuinely
    /// new news even when it reads identically.
    #[test]
    fn a_child_run_reported_again_is_not_folded_twice() {
        let (actor, _events, mailbox) = recording_actor("re-reported");
        let mut state = ActorState::default();
        let mut messages = vec![Message::system("you are mush")];
        let error = "Connection reset by peer (os error 104)";
        let line = format!("#2 failed: {error}");

        // The child fails while the parent naps: the record is delivered, and
        // the model reads it in the run it starts.
        assert!(matches!(
            absorb(
                &actor,
                &mut state,
                &mut messages,
                AgentMsg::ChildDone {
                    id: 2,
                    run: 1,
                    outcome: Outcome::Failed(error.into()),
                }
            ),
            Fold::Run
        ));
        assert_eq!(messages.last().unwrap().text(), line);

        // The same run arrives a second time — the re-record. This is the road
        // the defect lived on: the *recording* path cleared the mark on its way
        // past, so the next boundary folded a line the model had already
        // answered into the transcript again.
        mailbox
            .send(AgentMsg::ChildDone {
                id: 2,
                run: 1,
                outcome: Outcome::Failed(error.into()),
            })
            .unwrap();
        drain_mailbox(&actor, &AtomicBool::new(false), &mut messages, &mut state);
        assert!(
            !fold_completions(&actor, &mut state, &mut messages),
            "a run the model has read is not news when it is reported again"
        );
        assert_eq!(
            messages.iter().filter(|m| m.text() == line).count(),
            1,
            "one failure, one line: {messages:?}"
        );

        // And the same message absorbed by an idle actor is the same news: no
        // line pushed, and no run paid for to repeat it.
        assert!(matches!(
            absorb(
                &actor,
                &mut state,
                &mut messages,
                AgentMsg::ChildDone {
                    id: 2,
                    run: 1,
                    outcome: Outcome::Failed(error.into()),
                }
            ),
            Fold::Idle
        ));
        assert_eq!(messages.iter().filter(|m| m.text() == line).count(), 1);
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The batch the human saw: three children failed while the parent worked,
    /// one of them had already been answered through `wait_agents`, and then
    /// the records were replayed. Before the fix the next boundary pushed the
    /// answered failure as well — a second copy of a line the model had just
    /// been told, in a message that read as freshly replayed news.
    #[test]
    fn a_replayed_batch_delivers_each_unread_outcome_once() {
        let (actor, mailbox) = test_actor("replayed-batch");
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let mut messages = vec![Message::system("you are mush")];
        let error = "Connection reset by peer (os error 104)";
        for id in [4u64, 5, 6] {
            let (tx, _rx) = crossbeam_channel::unbounded();
            state.children.insert(id, tx);
            mailbox
                .send(AgentMsg::ChildDone {
                    id,
                    run: 1,
                    outcome: Outcome::Failed(error.into()),
                })
                .unwrap();
        }
        // Mid-run: recorded where the boundary can see them, and folded nowhere
        // yet (a completion is a user message, which belongs after a batch's
        // results, not between calls and them).
        drain_signals(&actor, &cancel, &mut state);
        assert_eq!(messages.len(), 1, "nothing is folded between tool calls");

        // The model asks for #4's result first: the wait answers with the line
        // and that answer is what the model has read.
        let answered = exec_tool(
            &actor,
            &mut state,
            ToolName::WaitAgents,
            &json!({ "ids": [4], "timeout": 5 }),
            &cancel,
        )
        .unwrap();
        assert_eq!(answered, format!("#4 failed: {error}"));
        assert_eq!(
            state.delivered.get(&4),
            Some(&1),
            "an answer from `wait_agents` is a delivery"
        );

        // And now every record is sent again — the replay.
        for id in [4u64, 5, 6] {
            mailbox
                .send(AgentMsg::ChildDone {
                    id,
                    run: 1,
                    outcome: Outcome::Failed(error.into()),
                })
                .unwrap();
        }
        drain_mailbox(&actor, &AtomicBool::new(false), &mut messages, &mut state);

        assert!(
            fold_completions(&actor, &mut state, &mut messages),
            "the two results nobody has read are still news"
        );
        let folded: Vec<&str> = messages.iter().map(Message::text).collect();
        assert_eq!(
            folded
                .iter()
                .filter(|line| line.starts_with("#4 failed"))
                .count(),
            0,
            "the failure the wait answered with is not folded again: {folded:?}"
        );
        for id in [5u64, 6] {
            let line = format!("#{id} failed: {error}");
            assert_eq!(
                folded.iter().filter(|folded| **folded == line).count(),
                1,
                "one line for #{id}: {folded:?}"
            );
        }
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The sweep the replay bug asked for: every road that hands a parent a
    /// child's body, one table, one invariant — the body arrives **once**, the
    /// result is left read, nothing folds it again, and a repeated wait answers
    /// with the digest instead of the report. The per-road tests each knew their
    /// own road; this is the one that would have caught `agent_status` replaying
    /// every child's report on every call.
    #[test]
    fn every_delivery_road_hands_a_result_over_once() {
        let body = "## report\nfirst line of detail\nsecond line of detail";
        for road in ["wait", "fold", "wake"] {
            let (actor, _events, _mailbox) = recording_actor(&format!("road-{road}"));
            let mut state = ActorState::default();
            let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
            state.children.insert(1, tx);
            // The arrival road, which never delivers: a completion is recorded
            // the moment it reaches the parent, and delivered by a boundary.
            note_completion(&mut state, 1, 1, Outcome::Finished(body.into()));
            let mut transcript = vec![Message::system("you are mush")];
            let mut answer = String::new();
            match road {
                "wait" => {
                    answer = exec_tool(
                        &actor,
                        &mut state,
                        ToolName::WaitAgents,
                        &json!({ "ids": [1], "timeout": 5 }),
                        &AtomicBool::new(false),
                    )
                    .unwrap();
                }
                "fold" => {
                    assert!(fold_completions(&actor, &mut state, &mut transcript));
                }
                "wake" => {
                    assert!(matches!(
                        absorb(
                            &actor,
                            &mut state,
                            &mut transcript,
                            AgentMsg::ChildDone {
                                id: 1,
                                run: 1,
                                outcome: Outcome::Finished(body.into()),
                            },
                        ),
                        Fold::Run
                    ));
                }
                _ => unreachable!(),
            }
            let carried = transcript
                .iter()
                .filter(|message| message.text().contains(body))
                .count()
                + usize::from(answer.contains(body));
            assert_eq!(carried, 1, "{road}: the body was delivered {carried} times");
            assert!(!state.unread(1), "{road}: the result is still unread");
            // Nothing folds it again: the once-only rule is what the mark is for.
            let before = transcript.len();
            assert!(
                !fold_completions(&actor, &mut state, &mut transcript),
                "{road}: the read result is still news"
            );
            assert_eq!(transcript.len(), before, "{road}: the fold replayed a line");
            // And asking a second time answers with the digest, never the body
            // (finding H15: a wait must not report the past as news).
            let again = exec_tool(
                &actor,
                &mut state,
                ToolName::WaitAgents,
                &json!({ "ids": [1], "timeout": 5 }),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert!(
                !again.contains(body),
                "{road}: the repeated wait replayed it: {again}"
            );
            assert!(again.contains("already read"), "{road}: {again}");
            // The listing never carries the body, read or unread.
            let listing = status_tool(&state).unwrap();
            assert!(
                !listing.contains(body),
                "{road}: the listing carried the body"
            );
            let _ = fs::remove_dir_all(actor.ws.root());
        }
    }

    /// `agent_status` is a listing, not a delivery: a child's whole final
    /// message used to be printed on every call, so a parent that polled its
    /// children re-read every report, and the fold then re-delivered it as the
    /// same text a second time. Now the line is a bounded digest with the size
    /// of what it is not showing, and `✉` says whether the body is still
    /// waiting — the fold remains the one road that hands it over.
    #[test]
    fn a_listing_digests_a_result_and_says_what_is_unread() {
        let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let mut state = ActorState::default();
        state.children.insert(1, tx);
        let long = format!("first line of a long report\n{}", "detail ".repeat(900));
        note_completion(&mut state, 1, 1, Outcome::Finished(long.clone()));

        let listing = status_tool(&state).unwrap();
        assert!(
            listing.contains("✉ #1 ✓ first line of a long report"),
            "{listing}"
        );
        assert!(
            listing.contains("chars total"),
            "the digest says how much it hides: {listing}"
        );
        assert!(!listing.contains(&long), "the listing carried the body");
        assert!(
            listing.chars().count() < 200,
            "the listing is bounded: {listing}"
        );
        assert!(state.unread(1), "a listing is not a read");

        // Delivered once by the fold: the marker goes, the digest stays.
        let (actor, _events, _mailbox) = recording_actor("listing-digest");
        let mut transcript = vec![Message::system("you are mush")];
        assert!(fold_completions(&actor, &mut state, &mut transcript));
        let listing = status_tool(&state).unwrap();
        assert!(!listing.contains('✉'), "nothing is unread now: {listing}");
        assert!(listing.contains("first line of a long report"), "{listing}");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// Two runs, the same failure text: the second is news, and folds once of
    /// its own. This is what makes plain `Outcome` equality (or a scan for the
    /// line) the wrong identity for a delivery — it would swallow a real second
    /// failure, or swallow the first and repeat the second.
    #[test]
    fn a_second_run_failing_the_same_way_is_news_again() {
        let (actor, _mailbox) = test_actor("same-text-twice");
        let mut state = ActorState::default();
        let mut messages = vec![Message::system("you are mush")];
        let error = "Connection reset by peer (os error 104)";
        let line = format!("#3 failed: {error}");

        for run in 1..=2 {
            assert!(matches!(
                absorb(
                    &actor,
                    &mut state,
                    &mut messages,
                    AgentMsg::ChildDone {
                        id: 3,
                        run,
                        outcome: Outcome::Failed(error.into()),
                    }
                ),
                Fold::Run
            ));
            assert_eq!(messages.last().unwrap().text(), line);
            assert!(
                !fold_completions(&actor, &mut state, &mut messages),
                "run {run} is folded once, not again at the next boundary"
            );
        }
        assert_eq!(
            messages.iter().filter(|m| m.text() == line).count(),
            2,
            "two runs, two failures, told twice: {messages:?}"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// Adoption asks "has this transcript already read this child?", and the
    /// answer has to be yes for all four shapes `Outcome::line` writes. The
    /// scan knew only `#N done:`, so a replayed failure — or a stop — read as
    /// unread and was folded in again (`docs/findings.md` B24).
    #[test]
    fn adoption_reads_a_failed_or_stopped_line_as_delivered() {
        let (actor, _mailbox) = test_actor("adopt-shapes");
        let mut state = ActorState::default();
        note_completion(&mut state, 2, 1, Outcome::Failed("no route".into()));
        note_completion(&mut state, 3, 1, Outcome::Stopped);
        note_completion(&mut state, 4, 1, Outcome::CutOff);
        let mut messages = Vec::new();
        let fresh = vec![
            Message::system("you are mush"),
            Message::user("task"),
            Message::tool("a", "#2 failed: no route"),
            Message::tool("b", Outcome::Stopped.line(3)),
            Message::tool("c", Outcome::CutOff.line(4)),
        ];
        assert!(matches!(
            absorb(&actor, &mut state, &mut messages, AgentMsg::Run(fresh)),
            Fold::Run
        ));
        assert_eq!(
            state.delivered.get(&2),
            Some(&1),
            "a failed line is a completion the model has read"
        );
        assert_eq!(state.delivered.get(&3), Some(&1), "and so is a stopped one");
        assert_eq!(
            state.delivered.get(&4),
            Some(&1),
            "and a cut-off line, which is the fourth shape `Outcome::line` writes"
        );
        assert!(
            !fold_completions(&actor, &mut state, &mut messages),
            "so neither is folded in a second time"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The job half of the same rule: a report the model has read is not folded
    /// again when the same record arrives a second time. `note_job` cleared the
    /// mark exactly as `note_completion` did, and the boundary that folds a job
    /// straight into the transcript pushed it without asking either.
    #[test]
    fn a_job_report_recorded_again_is_not_folded_twice() {
        let (actor, _events, mailbox) = recording_actor("re-reported-job");
        let mut state = ActorState::default();
        state.running_jobs.insert(1);
        let mut messages = vec![Message::system("you are mush")];
        let line = "#c1 done: exit 0 · 3m12s · cargo test — test result: ok";
        let report = || AgentMsg::CommandDone {
            id: 1,
            line: line.into(),
            news: true,
        };

        assert!(matches!(
            absorb(&actor, &mut state, &mut messages, report()),
            Fold::Run
        ));
        assert_eq!(messages.last().unwrap().text(), line);

        // The record arrives again while the owner is mid-run, so it is only
        // *recorded* for the boundary: `note_job` used to drop the mark there.
        mailbox.send(report()).unwrap();
        drain_signals(&actor, &AtomicBool::new(false), &mut state);
        assert!(
            !fold_completions(&actor, &mut state, &mut messages),
            "a report the model has read is not news again"
        );
        assert_eq!(messages.iter().filter(|m| m.text() == line).count(), 1);

        // And the boundary that folds a job's line itself asks the same
        // question before pushing it.
        mailbox.send(report()).unwrap();
        drain_mailbox(&actor, &AtomicBool::new(false), &mut messages, &mut state);
        assert_eq!(
            messages.iter().filter(|m| m.text() == line).count(),
            1,
            "one report, one line: {messages:?}"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A job that ends while its owner is mid-run is folded in at the message
    /// boundary by `drain_mailbox`, not by `absorb`. That path has to tell the
    /// UI too, or the human's copy is missing exactly the lines the model read
    /// while it worked.
    #[test]
    fn a_job_report_folded_mid_run_reaches_the_ui() {
        let (actor, events, mailbox) = recording_actor("mid-run-job");
        let mut state = ActorState::default();
        state.running_jobs.insert(1);
        let mut messages = vec![Message::system("you are mush")];
        let line = "#c1 stopped after 1s · npm run dev";
        mailbox
            .send(AgentMsg::CommandDone {
                id: 1,
                line: line.into(),
                news: false,
            })
            .unwrap();

        drain_mailbox(&actor, &AtomicBool::new(false), &mut messages, &mut state);

        assert_eq!(messages.last().unwrap().text(), line);
        assert!(state.delivered_jobs.contains(&1));
        let ui = ui_copy(&events);
        assert!(
            ui.iter().any(|message| message.text() == line),
            "a fold mid-run is visible too: {ui:?}"
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
        let cancel = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let report = run_shell(
            "sleep 30 & echo started",
            &std::env::temp_dir(),
            Duration::from_secs(10),
            Detach::No,
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
        let cancel = Arc::new(AtomicBool::new(false));
        let timeout = Duration::from_secs(5);
        let started = Instant::now();
        let report = run_shell(
            "sleep 30",
            &std::env::temp_dir(),
            timeout,
            Detach::No,
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
        let cancel = Arc::new(AtomicBool::new(false));

        let report = run_shell(
            "false",
            &std::env::temp_dir(),
            Duration::from_secs(5),
            Detach::No,
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
        let cancel = Arc::new(AtomicBool::new(true));
        let started = Instant::now();
        let report = run_shell(
            "sleep 30",
            &std::env::temp_dir(),
            Duration::from_secs(30),
            Detach::No,
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
        let cancel = Arc::new(AtomicBool::new(false));
        mailbox.send(AgentMsg::Stop).unwrap();

        let report = run_shell(
            "echo starting; sleep 30",
            &std::env::temp_dir(),
            Duration::from_secs(30),
            Detach::No,
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
        let cancel = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let report = run_shell(
            "yes mush",
            &std::env::temp_dir(),
            Duration::from_secs(30),
            Detach::No,
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
        let cancel = Arc::new(AtomicBool::new(false));
        let report = run_shell(
            "yes mush | head -c 40000",
            &std::env::temp_dir(),
            Duration::from_secs(10),
            Detach::No,
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

    /// The same actor, keeping the sink it emits into: how a delivery test sees
    /// both halves of one fact — the line the model reads and the line the UI
    /// was told.
    fn recording_actor(label: &str) -> (Actor, Arc<Recorder>, Sender<AgentMsg>) {
        let cfg = test_cfg();
        build_actor_about(
            label,
            Arc::new(HttpModel::new(cfg.clone())),
            cfg,
            Arc::new(ScriptedMachine::new()),
            Arc::new(clock::System),
        )
    }

    /// The App's half of the delivery contract, in miniature: the UI's copy of
    /// agent 7's transcript is the system message plus every `Message` event
    /// that agent emitted, in order. A test that wants to know what the human
    /// reads has to build it the same way, because that is the only road a line
    /// takes into it.
    fn ui_copy(events: &Recorder) -> Vec<Message> {
        let mut messages = vec![Message::system("you are mush")];
        messages.extend(events.events_for(AgentId(7)).into_iter().filter_map(
            |event| match event {
                AgentEvent::Message(message) => Some(message),
                _ => None,
            },
        ));
        messages
    }

    /// The same actor, with its model calls served by a script instead of a
    /// socket — so a whole run can be driven in process, with no server.
    fn scripted_actor(
        label: &str,
        model: &Arc<Scripted>,
    ) -> (Actor, Arc<Recorder>, Sender<AgentMsg>) {
        build_actor(label, model.clone(), test_cfg())
    }

    /// The same, on a clock that only moves when the test says so: how long a
    /// retry's backoff took is then a number the test reads, not a wait it
    /// pays for.
    fn scripted_actor_on_clock(
        label: &str,
        model: &Arc<Scripted>,
        clock: Arc<dyn clock::Clock>,
    ) -> (Actor, Arc<Recorder>, Sender<AgentMsg>) {
        build_actor_about(
            label,
            model.clone(),
            test_cfg(),
            Arc::new(ScriptedMachine::new()),
            clock,
        )
    }

    /// A scratch config cell. The endpoint is deliberately unreachable: every
    /// test that uses it must go through a scripted model.
    fn test_cfg() -> ConfigHandle {
        ConfigHandle::own(Config::new("http://127.0.0.1:1", "test", None))
    }

    /// A standalone actor over a scratch workspace, with `model` as its client
    /// and `cfg` as the tree's shared configuration. Its events go to a
    /// recording sink, which comes back so a test can read what the run said.
    fn build_actor(
        label: &str,
        model: Arc<dyn ModelClient>,
        cfg: ConfigHandle,
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
        cfg: ConfigHandle,
        machine: Arc<dyn Machine>,
        clock: Arc<dyn clock::Clock>,
    ) -> (Actor, Arc<Recorder>, Sender<AgentMsg>) {
        let root = std::env::temp_dir().join(format!("mush-actor-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let recorder = Recorder::new();
        let ids = Arc::new(AtomicU64::new(1));
        // The tree's one registry, over the same scripted machine and clock:
        // a job a test starts is watched in process, and its events land in the
        // same recording sink as the actor's.
        let registry = jobs::Registry::new(clock.clone(), recorder.clone(), ids.clone());
        let ctx = Arc::new(AgentCtx {
            cfg,
            model,
            events: recorder.clone(),
            machine,
            clock,
            registry,
            root: root.clone(),
            ids,
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
        let cancel = Arc::new(AtomicBool::new(false));
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
        let cancel = Arc::new(AtomicBool::new(false));
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

    /// `wait_commands` is `wait_agents` over jobs, and it had no test at all —
    /// not through `run_command`, not through the tool. This is the call site:
    /// two jobs are started, the wait returns their reports, and a wait with no
    /// `ids` reaches the jobs this agent started on its own books.
    #[test]
    fn wait_commands_returns_a_jobs_report() {
        let machine = Arc::new(
            ScriptedMachine::new()
                .runs(Script::exits(0).says("build ok"))
                .runs(Script::exits(0).says("test ok")),
        );
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("wait-jobs", machine, clock);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));

        for command in ["make build", "make test"] {
            let started = exec_tool(
                &actor,
                &mut state,
                ToolName::RunCommand,
                &json!({ "command": command, "detach": true }),
                &cancel,
            )
            .unwrap();
            assert!(started.contains("detached as #c"), "{started}");
        }

        // Each job's own watcher thread delivers its report to this actor's
        // mailbox. Block on those two arrivals *before* the wait begins, rather
        // than let the wait's fake clock race the watcher being scheduled: on
        // this clock a `timeout: 5` elapses in microseconds, so a report whose
        // thread had not yet been given the CPU made the wait answer "wait
        // timed out — #c1 still running" — a fact about the scheduler, not
        // about `wait_commands`. The recorded event is what the wait reads
        // anyway (`drain_signals` records a `CommandDone` through this same
        // `note_job`), so this changes only *when* the report is known.
        for _ in 0..2 {
            match actor.rx.recv_timeout(Duration::from_secs(10)) {
                Ok(AgentMsg::CommandDone { id, line, news }) => {
                    note_job(&mut state, id, line, news);
                }
                Ok(_) => panic!("a job's report must reach the owner's mailbox"),
                Err(error) => panic!(
                    "a job that has already ended must report — waited for its `CommandDone`: \
                     {error}"
                ),
            }
        }

        // One report, by id: the wait answers with the job's own line.
        let one = exec_tool(
            &actor,
            &mut state,
            ToolName::WaitCommands,
            &json!({ "ids": [1], "timeout": 5 }),
            &cancel,
        )
        .unwrap();
        assert!(one.contains("exit 0"), "{one}");
        assert!(one.contains("make build"), "{one}");
        assert!(
            !one.contains("make test"),
            "and only what was asked for: {one}"
        );

        // No ids: every job this agent started, and no `all` means the first
        // report that is ready rather than every one.
        let mine = exec_tool(
            &actor,
            &mut state,
            ToolName::WaitCommands,
            &json!({ "timeout": 5 }),
            &cancel,
        )
        .unwrap();
        assert!(mine.contains("exit 0"), "{mine}");
        assert!(!mine.contains('\n'), "one result, not a list: {mine}");

        // Nothing to wait for is an answer, not an error.
        let (idle, _mailbox) = scripted_tools_actor(
            "wait-jobs-idle",
            Arc::new(ScriptedMachine::new()),
            Arc::new(Advanceable::new()),
        );
        assert_eq!(
            exec_tool(
                &idle,
                &mut ActorState::default(),
                ToolName::WaitCommands,
                &json!({}),
                &cancel
            )
            .unwrap(),
            "no jobs to wait for"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
        let _ = fs::remove_dir_all(idle.ws.root());
    }

    /// `all` asks for every result instead of the first one. It is the whole
    /// difference between "one delegate is free" and "the whole set is in", and
    /// it appeared nowhere in the suite — on either wait.
    #[test]
    fn a_wait_returns_the_first_result_or_all_of_them() {
        let (actor, _mailbox) = test_actor("wait-all");
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let (child, _child_rx) = crossbeam_channel::unbounded();
        for id in [1u64, 2] {
            state.children.insert(id, child.clone());
        }
        state.completed.insert(
            1,
            Completion {
                run: 1,
                outcome: Outcome::Finished("wrote the parser".into()),
            },
        );
        state.completed.insert(
            2,
            Completion {
                run: 1,
                outcome: Outcome::Failed("no route".into()),
            },
        );

        let first = exec_tool(
            &actor,
            &mut state,
            ToolName::WaitAgents,
            &json!({ "timeout": 5 }),
            &cancel,
        )
        .unwrap();
        assert_eq!(first, "#1 done: wrote the parser", "one result by default");
        // The wait asked about every child but handed over only #1. #2's result
        // must still be unread, or a wait for the first would swallow the body
        // of a result it never answered with (the poll used to mark them all).
        assert!(!state.unread(1), "the answer was #1's read");
        assert!(
            state.unread(2),
            "#2's body was not handed over, so it is not read"
        );

        let every = exec_tool(
            &actor,
            &mut state,
            ToolName::WaitAgents,
            &json!({ "all": true, "timeout": 5 }),
            &cancel,
        )
        .unwrap();
        assert!(
            every.contains("#2 failed: no route"),
            "every result, with #2's body delivered now: {every}"
        );
        assert!(
            every.contains("already read"),
            "and #1 named as one the model has read, not replayed (H15): {every}"
        );
        let one = every.find("#1").expect("#1 is named");
        let two = every.find("#2").expect("#2 is named");
        assert!(one < two, "in the order they were asked about: {every}");
        assert!(!state.unread(1) && !state.unread(2), "both are read now");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The job-side deadline: a command that never ends cannot hold the run for
    /// the rest of the timeout. What is known is returned and what is not is
    /// named, and the clock is what ended the wait, not the command.
    #[test]
    fn a_command_wait_that_nothing_ends_times_out_on_the_clock() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("wait-jobs-timeout", machine, clock.clone());
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));

        let started = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "npm run dev", "detach": true }),
            &cancel,
        )
        .unwrap();
        assert!(started.contains("detached as #c1"), "{started}");

        let begun = Instant::now();
        let result = exec_tool(
            &actor,
            &mut state,
            ToolName::WaitCommands,
            &json!({ "timeout": 30 }),
            &cancel,
        )
        .unwrap();

        assert!(result.contains("wait timed out"), "{result}");
        assert!(result.contains("#c1 still running"), "{result}");
        assert!(
            clock.elapsed() >= Duration::from_secs(30),
            "the deadline ended it: {:?}",
            clock.elapsed()
        );
        assert!(
            begun.elapsed() < Duration::from_secs(1),
            "and it was reached without waiting for it: {:?}",
            begun.elapsed()
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
        let cancel = Arc::new(AtomicBool::new(false));
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
        assert_eq!(parked_message(&quiet), None);
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The same rule, end to end: the root delegates, parks in `wait_agents`,
    /// the human types, and the model answers their words in that run — while
    /// the child is still working, not after it finishes.
    ///
    /// The child is held inside a real shell command that waits for a file the
    /// test only writes at the very end, so "the answer came back while the
    /// child still ran" is proven by that file's absence rather than by a race.
    /// Two guards keep that shell from outliving a failure: the gate is opened
    /// on the way out of the test however it ends, and the wait itself is
    /// bounded, so even a killed test process cannot leave a child spinning.
    #[test]
    fn a_human_message_reaches_a_root_napping_on_wait_agents() {
        /// Opens the gate when the test leaves, panic or not: the child is a
        /// real command looping until the file appears, and an assertion that
        /// fires before the write would otherwise leak that shell for good.
        struct Gate(std::path::PathBuf);

        impl Drop for Gate {
            fn drop(&mut self) {
                let _ = fs::write(&self.0, "go");
            }
        }

        let root = std::env::temp_dir().join(format!("mush-wake-e2e-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let gate = root.join("open-the-gate");
        let _gate = Gate(gate.clone());
        let block = format!(
            "i=0; while [ ! -f {} ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done; echo released",
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

        // Let the child go, and wait for its command to come back before
        // taking the tree down: the gate sits inside the workspace, so deleting
        // it while the child still watched for it would re-arm a shell that
        // then spins out its whole bound — the wake-test orphan again, only
        // smaller. The next thing the child does after its command returns is
        // ask the model, and that ask carries the output.
        fs::write(&gate, "go").unwrap();
        let deadline = Instant::now() + WAIT;
        while !scripted
            .asked()
            .iter()
            .any(|ask| ask.depth() == Some(1) && ask.saw("released"))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
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
        let cancel = Arc::new(AtomicBool::new(false));
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

    /// Some compatible servers (and models) answer with a tool call that has no
    /// id, or repeat one across a batch. Strict servers pair a result with its
    /// call *by id*, so the run must answer each call — with its own id, never
    /// `""` and never a duplicate.
    #[test]
    fn a_reply_whose_calls_have_no_ids_still_gets_answered() {
        let scripted = Arc::new(
            Scripted::new()
                .calls(vec![
                    tool_call("", "read_file", json!({ "path": "missing.rs" })),
                    tool_call("dup", "read_file", json!({ "path": "missing.rs" })),
                    tool_call("dup", "list_files", json!({})),
                ])
                .says("done"),
        );
        let (actor, _events, mailbox) = scripted_actor("id-less-calls", &scripted);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("look around"),
        ];

        let result = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();
        assert_eq!(result.as_deref(), Some("done"));

        // The second request carries the assistant's calls and their results:
        // the pairing a server validates is exactly this.
        let asked = scripted.asked();
        assert_eq!(asked.len(), 2, "the batch, then the answer");
        let ids: Vec<String> = asked[1]
            .messages
            .iter()
            .flat_map(|message| message.tool_calls().iter().map(|call| call.id.clone()))
            .collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.iter().all(|id| !id.trim().is_empty()), "{ids:?}");
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            3,
            "each call is answerable on its own: {ids:?}"
        );

        let answered: Vec<String> = asked[1]
            .messages
            .iter()
            .filter(|message| message.role == "tool")
            .map(|message| message.tool_call_id.clone().unwrap_or_default())
            .collect();
        assert_eq!(
            answered, ids,
            "every result answers the id that asked for it"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
        let _ = mailbox;
    }

    /// A thinking model's reasoning is part of the turn it decided, and the
    /// endpoint refuses a request that replays an assistant turn without it
    /// (DeepSeek: "the `reasoning_content` in the thinking mode must be passed
    /// back to the API"). The request that carries the tool result is the one
    /// that breaks first, so that is the one this pins.
    #[test]
    fn a_thinking_replys_reasoning_is_sent_back_with_its_turn() {
        let reasoning = "the gate file is not mine to open; write the note first";
        let scripted = Arc::new(
            Scripted::new()
                .finishing(
                    Message {
                        role: "assistant".into(),
                        reasoning_content: Some(reasoning.into()),
                        tool_calls: Some(vec![tool_call(
                            "c0",
                            "write_file",
                            json!({ "path": "note.txt", "content": "hi" }),
                        )]),
                        ..Default::default()
                    },
                    "tool_calls",
                )
                .says("done"),
        );
        let (actor, _events, mailbox) = scripted_actor("reasoning", &scripted);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("write the note"),
        ];

        let result = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();
        assert_eq!(result.as_deref(), Some("done"));

        // The transcript kept the reasoning on its own turn...
        assert_eq!(
            messages[2].reasoning_content.as_deref(),
            Some(reasoning),
            "the reply's reasoning stays on the assistant turn"
        );
        // ...the request that carries the tool result replayed it...
        let asked = scripted.asked();
        assert_eq!(asked.len(), 2, "the call, then the answer");
        let replayed = asked[1]
            .messages
            .iter()
            .find(|message| message.role == "assistant")
            .expect("the tool result's request replays the assistant turn");
        assert_eq!(
            replayed.reasoning_content.as_deref(),
            Some(reasoning),
            "a thinking endpoint refuses this request without it"
        );
        // ...and nothing invented reasoning for the request that had no reply
        // yet, nor for the tool result that never had any.
        assert!(
            asked[0]
                .messages
                .iter()
                .all(|message| message.reasoning_content.is_none()),
            "the first request has no reply to carry reasoning from"
        );
        assert!(
            asked[1]
                .messages
                .iter()
                .filter(|message| message.role == "tool")
                .all(|message| message.reasoning_content.is_none()),
            "a tool result carries no reasoning"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
        let _ = mailbox;
    }

    /// The thinking knobs are the human's: what the config states is what the
    /// request carries, on an endpoint no provider default would send it to
    /// (that is what stating a value means), and an unstated custom endpoint
    /// still gets neither field.
    #[test]
    fn a_stated_effort_and_thinking_mode_reach_the_request() {
        let scripted = Arc::new(Scripted::new().says("done"));
        let mut local = Config::new("http://localhost:11434", "local-thinker", None);
        local.reasoning_effort = Some(ReasoningEffort::Medium);
        local.thinking = Some(ThinkingMode::Off);
        let (actor, _events, _mailbox) =
            build_actor("knobs", scripted.clone(), ConfigHandle::own(local));
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![Message::system("you are mush"), Message::user("hi")];

        let result = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();
        assert_eq!(result.as_deref(), Some("done"));
        let asked = scripted.asked();
        assert_eq!(asked[0].reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(
            asked[0].thinking, None,
            "stating off sends no `thinking` field at all"
        );
        let _ = fs::remove_dir_all(actor.ws.root());

        // The same code path with nothing stated: a custom endpoint sees a
        // plain request, exactly as it did before these knobs were
        // configurable.
        let scripted = Arc::new(Scripted::new().says("done"));
        let (actor, _events, _mailbox) = scripted_actor("no-knobs", &scripted);
        let mut state = ActorState::default();
        let mut messages = vec![Message::system("you are mush"), Message::user("hi")];
        run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();
        let asked = scripted.asked();
        assert_eq!(asked[0].reasoning_effort, None);
        assert_eq!(asked[0].thinking, None);
        let _ = fs::remove_dir_all(actor.ws.root());
    }
    /// `content_filter` is the endpoint saying it refused to hand over what the
    /// model wrote, and an unknown reason is no more a normal end — neither may
    /// be reported as if the model had simply had nothing to say.
    #[test]
    fn a_refused_reply_is_an_error_not_an_empty_answer() {
        let scripted =
            Arc::new(Scripted::new().finishing(Message::assistant(""), "content_filter"));
        let (actor, events, mailbox) = scripted_actor("filtered", &scripted);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("say something"),
        ];

        let error = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap_err();
        assert!(error.contains("content_filter"), "{error}");
        assert!(error.contains("refused"), "{error}");
        assert!(
            !events.events_for(AgentId(7)).iter().any(
                |event| matches!(event, AgentEvent::Notice(what) if what.contains("empty reply"))
            ),
            "a refusal is not an empty reply"
        );
        assert_eq!(scripted.asked().len(), 1, "a refusal is not retried");
        let _ = fs::remove_dir_all(actor.ws.root());
        let _ = mailbox;
    }

    /// A reason mush has never heard of is still the endpoint saying it did not
    /// finish the answer; the run names it instead of ending as if it had.
    #[test]
    fn an_unknown_finish_reason_is_named_in_the_runs_error() {
        let scripted =
            Arc::new(Scripted::new().finishing(Message::assistant("half a th"), "safety"));
        let (actor, _events, mailbox) = scripted_actor("unknown-reason", &scripted);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![Message::user("do the thing")];

        let error = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap_err();
        assert!(error.contains("safety"), "{error}");
        assert!(error.contains("finish_reason"), "{error}");
        let _ = fs::remove_dir_all(actor.ws.root());
        let _ = mailbox;
    }

    /// A refused reply can still carry the call it was about to make. It must
    /// be answered (never run), or the transcript keeps a dangling call that
    /// poisons every later request in the conversation.
    #[test]
    fn a_refused_reply_answers_the_calls_it_carried() {
        let mut assistant = Message::assistant("");
        assistant.tool_calls = Some(vec![tool_call(
            "c0",
            "write_file",
            json!({ "path": "secret.txt", "content": "x" }),
        )]);
        let scripted = Arc::new(Scripted::new().finishing(assistant, "content_filter"));
        let (actor, _events, mailbox) = scripted_actor("filtered-call", &scripted);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![Message::user("write the file")];

        let error = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap_err();
        assert!(error.contains("content_filter"), "{error}");
        assert!(
            !actor.ws.root().join("secret.txt").exists(),
            "a call from a refused reply must never run"
        );
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[2].role, "tool");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c0"));
        assert!(
            messages[2].text().contains("was not run"),
            "{}",
            messages[2].text()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
        let _ = mailbox;
    }

    /// A server that reports `usage` is the only source of a *real* token
    /// count: the UI's meter is bytes/3. The run reports what it was told,
    /// once, when the run ends.
    #[test]
    fn the_run_reports_the_endpoints_own_token_counts() {
        let scripted = Arc::new(
            Scripted::new()
                .says("all done")
                .with_usage(1_200, 34, 1_234),
        );
        let (actor, events, mailbox) = scripted_actor("usage", &scripted);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![Message::system("you are mush"), Message::user("say hi")];

        let result = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();
        assert_eq!(result.as_deref(), Some("all done"));

        let usage: Vec<String> = notices(&events);
        assert_eq!(usage.len(), 1, "one line per run: {usage:?}");
        assert_eq!(
            usage[0],
            "the endpoint counted 1.2k prompt + 34 completion tokens this run (1.2k total)"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
        let _ = mailbox;
    }

    /// A run is more than one call, and the number reported is the run's: the
    /// parts are added up, and a server that never sends `total_tokens` still
    /// gets one that adds up.
    #[test]
    fn the_runs_usage_adds_up_over_its_calls() {
        let scripted = Arc::new(
            Scripted::new()
                .calls(vec![tool_call("c1", "list_files", json!({}))])
                .with_usage(1_100, 11, 1_111)
                .says("done")
                // The same server, not reporting a total this time.
                .with_usage(2_200, 22, 0),
        );
        let (actor, events, mailbox) = scripted_actor("usage-sum", &scripted);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![Message::user("look around")];

        run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();
        let usage: Vec<String> = notices(&events);
        assert_eq!(usage.len(), 1, "{usage:?}");
        assert_eq!(
            usage[0],
            "the endpoint counted 3.3k prompt + 33 completion tokens this run (3.3k total)"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
        let _ = mailbox;
    }

    /// A server that reports nothing leaves mush's own estimate as the only
    /// number there is, and the run says nothing it was not told.
    #[test]
    fn a_server_without_usage_reports_no_numbers() {
        let scripted = Arc::new(Scripted::new().says("all done"));
        let (actor, events, mailbox) = scripted_actor("usage-none", &scripted);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![Message::user("say hi")];

        run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();
        assert!(notices(&events).is_empty(), "{:?}", notices(&events));
        let _ = fs::remove_dir_all(actor.ws.root());
        let _ = mailbox;
    }

    /// The usage lines a run emitted, in order.
    fn notices(events: &Recorder) -> Vec<String> {
        events
            .events_for(AgentId(7))
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::Notice(what) if what.contains("endpoint counted") => Some(what),
                _ => None,
            })
            .collect()
    }

    /// The three reasons mush does understand are the only ones read as ends:
    /// everything else goes to `refusal_reason`.
    #[test]
    fn only_stop_a_tool_batch_and_the_cap_are_normal_ends() {
        for reason in [
            None,
            Some(""),
            Some("stop"),
            Some("tool_calls"),
            Some("length"),
        ] {
            assert_eq!(refusal_reason(reason), None, "{reason:?}");
        }
        for reason in ["content_filter", "safety", "eos"] {
            assert_eq!(refusal_reason(Some(reason)), Some(reason), "{reason:?}");
        }
        assert!(refusal_error("content_filter").contains("content filter"));
        assert!(refusal_error("safety").contains("safety"));
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
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![Message::user("write everything")];

        let error = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap_err();
        assert!(error.contains("cut off"), "{error}");
        assert!(error.contains("in a row"), "{error}");
        assert_eq!(scripted.asked().len(), TRUNCATION_ROUNDS + 1);
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The cap a request carries is the config's, window and all: a run
    /// budgeted against a 120k window asks for a quarter of it rather than the
    /// 20_480 that cut a real run off mid-task, and the number travels under
    /// the name the config chose. `Asked` records what the endpoint was really
    /// sent, so this is the number a reply would be cut off at.
    #[test]
    fn a_request_carries_the_cap_its_window_derives() {
        let scripted = Arc::new(Scripted::new().says("done"));
        let mut cfg = Config::new("http://127.0.0.1:1", "test", None);
        cfg.provider = mush_core::config::Provider::DeepSeek;
        cfg.set_context(120_000);
        let (actor, _events, _mailbox) =
            build_actor("reply-cap", scripted.clone(), ConfigHandle::own(cfg));
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![Message::user("hi")];

        run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();
        let asked = scripted.asked();
        assert_eq!(
            asked[0].max_tokens, 30_000,
            "a quarter of the 120k window, not a fixed 20_480, under the field this config chose"
        );
        assert_eq!(
            asked[0].max_completion_tokens, None,
            "the documented field, since this endpoint takes it"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A command parked while a run was in flight must not wait for the
    /// human's *next* message: a run can end without reaching a boundary (a
    /// cancel mid-tool-call does), and then the actor is idle with the human's
    /// words — or their `/compact` — sitting in `deferred`.
    #[test]
    fn a_command_parked_by_a_finished_run_is_folded_in_at_once() {
        let (actor, _mailbox) = test_actor("fold-parked");
        let mut state = ActorState::default();
        let mut messages = vec![Message::system("you are mush")];
        assert_eq!(
            fold_parked(&actor, &mut state, &mut messages),
            None,
            "nothing parked is not a reason to wake"
        );

        // A `/compact` parked mid-tool-call: a fold to do, not a run to start.
        state.deferred.push(AgentMsg::Compact(Vec::new()));
        assert_eq!(
            fold_parked(&actor, &mut state, &mut messages),
            Some(Fold::Idle)
        );
        assert!(state.compact_requested, "the request survives the fold");
        assert!(state.deferred.is_empty(), "and is not folded twice");

        // The human's words are work to answer, so they start a run.
        state.compact_requested = false;
        state
            .deferred
            .push(AgentMsg::Nudge("are you there?".into()));
        assert_eq!(
            fold_parked(&actor, &mut state, &mut messages),
            Some(Fold::Run)
        );
        assert_eq!(messages.last().unwrap().text(), "are you there?");

        // A shutdown outranks whatever was parked behind it.
        state
            .deferred
            .push(AgentMsg::Nudge("one more thing".into()));
        state.deferred.push(AgentMsg::Shutdown);
        assert_eq!(
            fold_parked(&actor, &mut state, &mut messages),
            Some(Fold::End)
        );

        // A stray Stop is not work, and must not wake anyone.
        state.deferred.push(AgentMsg::Stop);
        assert_eq!(
            fold_parked(&actor, &mut state, &mut messages),
            Some(Fold::Idle)
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The stall `fold_parked` closes, through the real actor loop.
    ///
    /// The vehicle matters. A cancel usually reaches the run at a message
    /// boundary, where everything parked is folded in first — but not when it
    /// lands while a *model call* is in flight: that path drains, sees the
    /// Stop, and drops the stale reply, returning with the parked `/compact`
    /// still in `deferred`. The actor is idle then, and must fold there rather
    /// than wait for the human's next message.
    #[test]
    fn a_compact_parked_by_a_cancelled_reply_still_folds() {
        let root = std::env::temp_dir().join(format!("mush-compact-parked-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let summary = "folded after the cancel";
        // The first reply is held inside the model call, so the test can act
        // while it is genuinely in flight.
        let gate = Arc::new(Gate::new());
        let scripted = Arc::new(
            Scripted::new()
                .when(|asked: &Asked| asked.saw(COMPACT_INSTRUCTION))
                .says(summary)
                .when(|asked: &Asked| asked.depth().is_none())
                .held(gate.clone())
                .says("a reply the human cancelled"),
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
                Message::user("say something"),
                Message::assistant("something"),
                Message::user("and again"),
            ]))
            .unwrap();

        assert!(
            gate.wait_until_asked(WAIT),
            "the first request never reached the model"
        );
        // Both arrive while the model is thinking: the cancel is honoured as
        // soon as the reply comes back, and the fold is left parked.
        root_tx.send(AgentMsg::Compact(Vec::new())).unwrap();
        root_tx.send(AgentMsg::Stop).unwrap();
        gate.release();

        let folded = |events: &Recorder| {
            events.events_for(AgentId::ROOT).iter().any(
                |event| matches!(event, AgentEvent::Compact { summary, .. } if summary == summary),
            )
        };
        let deadline = Instant::now() + WAIT;
        while !folded(&events) && Instant::now() < deadline {
            let _ = events.wait(Duration::from_millis(50));
        }
        assert!(
            folded(&events),
            "a /compact parked by the cancelled reply must fold once the actor is idle: {:?}",
            events.events_for(AgentId::ROOT)
        );

        let _ = root_tx.send(AgentMsg::Shutdown);
        let _ = fs::remove_dir_all(&root);
    }

    /// The bug this guards: re-queuing a parked nudge into the actor's own
    /// mailbox spins forever, because `my_tx` *is* the queue being drained.
    #[test]
    fn drain_signals_parks_nudges_for_the_next_boundary() {
        let (actor, mailbox) = test_actor("signals");
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
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
        drain_mailbox(&actor, &cancel, &mut messages, &mut state);
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
        let cancel = Arc::new(AtomicBool::new(false));
        drain_mailbox(&actor, &cancel, &mut messages, &mut state);
        assert!(cancel.load(Ordering::SeqCst), "a Stop cancels the run");
        assert!(!state.shutdown, "a Stop must not end the actor");

        mailbox.send(AgentMsg::Shutdown).unwrap();
        drain_mailbox(&actor, &cancel, &mut messages, &mut state);
        assert!(
            state.shutdown,
            "a Shutdown ends the actor once the run stops"
        );

        // Idle: the same split, expressed as what the actor should do next.
        let mut state = ActorState::default();
        assert!(matches!(
            absorb(&actor, &mut state, &mut messages, AgentMsg::Stop),
            Fold::Idle
        ));
        assert!(matches!(
            absorb(&actor, &mut state, &mut messages, AgentMsg::Shutdown),
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
            absorb(&actor, &mut state, &mut transcript, AgentMsg::Stop),
            Fold::Idle
        ));
        assert!(
            !run_cancel(&mut state).load(Ordering::SeqCst),
            "a Stop folded away while idle cancels nothing"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A command that outlives `CMD_DETACH_AFTER` stops being a tool call and
    /// becomes a job: not killed, and its owner is told how it ends. This is the
    /// 120-second kill replaced — the thing that was exactly wrong for a fresh
    /// worktree's cold build.
    #[test]
    fn a_command_that_outlives_the_detach_deadline_becomes_a_job() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("detach", machine.clone(), clock.clone());
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));

        let report = run_shell(
            "cargo build",
            &std::env::temp_dir(),
            Duration::from_secs(CMD_TIMEOUT_SECS),
            Detach::Job {
                registry: &actor.ctx.registry,
                exclusive: false,
            },
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();

        assert!(report.contains("detached as #c1"), "{report}");
        assert!(
            report.contains("you will be told when it finishes"),
            "{report}"
        );
        assert_eq!(
            machine.kills(),
            0,
            "the command keeps running in its own process group"
        );
        assert_eq!(actor.ctx.registry.running(), 1, "and it is a live job");
        assert!(
            state.running_jobs.contains(&1),
            "the owner's books know about it"
        );
        assert!(
            clock.elapsed() >= jobs::CMD_DETACH_AFTER,
            "the deadline is what moved it, not the end of the command: {:?}",
            clock.elapsed()
        );

        // And its end lands in the owner's own mailbox, once, saying it was
        // stopped rather than blamed on an exit code.
        let stopped = actor.ctx.registry.stop(actor.id, 1).unwrap();
        assert_eq!(stopped, "stopping job #c1");
        match actor.rx.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone { id, line, news }) => {
                assert_eq!(id, 1);
                assert!(!news, "a job mush killed wakes nobody: {line}");
                assert!(line.contains("stopped after"), "{line}");
                assert!(line.contains("cargo build"), "{line}");
            }
            other => panic!("the owner must be told: {:?}", other.is_ok()),
        }
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// §5.6: "A detached exclusive job holds the lock for its whole life". A
    /// foreground `exclusive` command that outlives `CMD_DETACH_AFTER` becomes a
    /// job, and the lock goes with it: the tool call is over, the benchmark is
    /// not. The release that ends every foreground call must not clear the
    /// claim its own new job has just taken — which is exactly what it did,
    /// silently, while `detach: true` (which returns before that release) kept
    /// it. The two paths have to agree.
    #[test]
    fn an_auto_detached_exclusive_command_keeps_the_machine() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("exclusive-detach", machine.clone(), clock);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));

        let report = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "cargo bench", "exclusive": true }),
            &cancel,
        )
        .unwrap();
        assert!(report.contains("detached as #c1"), "{report}");
        assert_eq!(
            actor.ctx.registry.held(),
            Some((7, "cargo bench".to_string(), Some(1))),
            "the job holds the machine, not just its first sixty seconds"
        );

        // A sibling is refused, and told who has it — the whole point of the
        // lock is that everyone else knows what to wait for.
        let held = actor.ctx.registry.machine_free_for(9).unwrap_err();
        let refusal = Refused::Machine(held).message(9);
        assert!(refusal.starts_with("#7 holds the machine"), "{refusal}");
        assert!(
            refusal.contains("do not retry this call"),
            "the refusal must not read as try-again-now (H13): {refusal}"
        );

        // And the job gives it up when it ends, not before.
        assert_eq!(
            actor.ctx.registry.stop(actor.id, 1).unwrap(),
            "stopping job #c1"
        );
        match actor.rx.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone { id, .. }) => assert_eq!(id, 1),
            _ => panic!("the job must report its own end"),
        }
        assert!(
            actor.ctx.registry.machine_free_for(9).is_ok(),
            "the machine is free once the job is over"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The other half of the same rule, so the two paths are pinned against
    /// each other: a foreground `exclusive` command that *ended* releases the
    /// machine — the release still means what it says.
    #[test]
    fn a_foreground_exclusive_command_releases_the_machine() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::exits(0).says("bench done")));
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("exclusive-foreground", machine, clock);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));

        let report = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "cargo bench", "exclusive": true }),
            &cancel,
        )
        .unwrap();

        assert!(report.contains("bench done"), "{report}");
        assert_eq!(actor.ctx.registry.held(), None, "the call is over");
        assert!(
            actor.ctx.registry.machine_free_for(9).is_ok(),
            "and a sibling may start"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A clock that lets go of the machine on its first slice: the wait's own
    /// movement ends the wait, so the queue is proved with no thread and no
    /// real time. The registry is installed after the actor is built (the
    /// actor makes it), which is why this is a cell.
    struct ReleasesOnSleep {
        inner: Advanceable,
        release: std::sync::Mutex<Option<(Arc<jobs::Registry>, u64)>>,
    }

    impl ReleasesOnSleep {
        fn new() -> Self {
            Self {
                inner: Advanceable::new(),
                release: std::sync::Mutex::new(None),
            }
        }

        fn release_after(&self, registry: Arc<jobs::Registry>, holder: u64) {
            *self.release.lock().unwrap() = Some((registry, holder));
        }
    }

    impl clock::Clock for ReleasesOnSleep {
        fn now(&self) -> Instant {
            self.inner.now()
        }

        fn sleep(&self, d: Duration) {
            if let Some((registry, holder)) = self.release.lock().unwrap().clone() {
                registry.release_machine(holder);
            }
            self.inner.sleep(d);
        }
    }

    /// A sibling queues for the machine instead of refusing on sight: the wait
    /// is the thing the refusal left out, and a model that retries the refusal
    /// is doing exactly what the loop guard counts (finding H13). Here the
    /// holder lets go on the first slice, so the command runs.
    #[test]
    fn a_sibling_command_queues_for_the_machine_and_then_runs() {
        let machine =
            Arc::new(ScriptedMachine::new().runs(Script::exits(0).says("ran beside the bench")));
        let clock = Arc::new(ReleasesOnSleep::new());
        let (actor, _mailbox) = scripted_tools_actor("machine-queue", machine, clock.clone());
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        actor.ctx.registry.take_machine(2, "cargo bench").unwrap();
        clock.release_after(actor.ctx.registry.clone(), 2);

        let report = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "cargo test" }),
            &cancel,
        )
        .unwrap();

        assert!(report.contains("ran beside the bench"), "{report}");
        assert!(
            actor.ctx.registry.machine_free_for(9).is_ok(),
            "and the queue holds nothing afterwards"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// And the wait is bounded: a lock that outlasts it refuses, naming the
    /// holder. The clock, not a real timeout, is what ends it.
    #[test]
    fn a_command_locked_out_for_too_long_is_refused_with_the_holder_named() {
        // No script on the machine: a command that runs at all has spent its
        // wait on nothing.
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor(
            "machine-timeout",
            Arc::new(ScriptedMachine::new()),
            clock.clone(),
        );
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        actor.ctx.registry.take_machine(2, "cargo bench").unwrap();

        let refused = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "cargo test" }),
            &cancel,
        )
        .unwrap_err();
        let ToolError::Refused(why) = refused else {
            panic!("a lock that outlasts the queue is refused");
        };
        assert!(why.starts_with("#2 holds the machine"), "{why}");
        assert!(why.contains("do not retry this call"), "{why}");
        assert!(
            clock.elapsed() >= LOCK_QUEUE,
            "the queue waited its whole bound: {:?}",
            clock.elapsed()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The root is not a sibling: it commands beside a held lock rather than
    /// being refused, and the result says so out loud. The live cost was an
    /// orchestrator that could not work while a child benchmarked (finding
    /// H13).
    #[test]
    fn the_root_commands_beside_a_held_lock_and_is_told() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::exits(0).says("diffed anyway")));
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("root-beside", machine, clock);
        // The helper builds agent 7; only the root wears id 0, and the
        // exemption is about that id.
        let actor = Actor {
            id: AgentId::ROOT.0,
            ..actor
        };
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        actor.ctx.registry.take_machine(2, "cargo bench").unwrap();

        let report = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "git diff" }),
            &cancel,
        )
        .unwrap();

        assert!(report.contains("diffed anyway"), "{report}");
        assert!(
            report.contains("#2 held the machine"),
            "the exemption is said out loud: {report}"
        );
        assert!(
            actor.ctx.registry.machine_free_for(9).is_err(),
            "the root ran beside the lock; it did not take or break it"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// `detach: true` asks for a job from the start, which is what a server
    /// needs: waiting sixty seconds to be told `npm run dev` started is not an
    /// answer. The job is then visible, and stoppable, through its own tools.
    #[test]
    fn detach_true_returns_at_once_and_the_job_tools_see_it() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("detach-now", machine.clone(), clock.clone());
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut call =
            |tool: ToolName, args: Value| exec_tool(&actor, &mut state, tool, &args, &cancel);

        let started = call(
            ToolName::RunCommand,
            json!({ "command": "npm run dev", "detach": true }),
        )
        .unwrap();
        assert!(started.contains("detached as #c1"), "{started}");
        assert_eq!(actor.ctx.registry.running(), 1);

        // The job's own tools: status names it, what it is doing, and how long
        // — the age itself is asserted on the pure formatter, because the job's
        // own thread is advancing the same clock as this test reads.
        let status = call(ToolName::CommandStatus, json!({})).unwrap();
        assert!(status.contains("#c1 running "), "{status}");
        assert!(status.contains("npm run dev"), "{status}");

        let stopped = call(
            ToolName::CommandControl,
            json!({ "id": 1, "action": "stop" }),
        )
        .unwrap();
        assert_eq!(stopped, "stopping job #c1");
        // A stop is a request to the job's own thread; the report is what the
        // owner reads next, and `command_status` then says it ended.
        match actor.rx.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone { line, .. }) => assert!(line.contains("stopped after")),
            other => panic!("the stop must be reported: {:?}", other.is_ok()),
        }
        let status = call(ToolName::CommandStatus, json!({})).unwrap();
        assert!(status.contains("stopped after"), "{status}");
        // An id that was never a job is an error the model can correct, and
        // another action is one it cannot use.
        assert!(call(
            ToolName::CommandControl,
            json!({ "id": 99, "action": "stop" })
        )
        .is_err());
        assert!(call(
            ToolName::CommandControl,
            json!({ "id": 1, "action": "poke" })
        )
        .is_err());
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A foreground `run_command` is *one* command. It used to be two: the tool
    /// box spawned a process to hand to the registry and the foreground path
    /// spawned its own, so every side-effecting command (a `git` write, an `rm`,
    /// a migration) ran twice — and the first copy belonged to nobody, so
    /// `kill_all` could not reach it and it outlived mush.
    ///
    /// What the bug *is* is a count, and the machine at the seam counts spawns,
    /// so this needs no process of its own: it read a real `sh -c 'echo hit >> …
    /// && sleep 0.2'` for a while, which put a subprocess and a 200 ms sleep in
    /// a suite whose contract is that a default run needs neither. A second
    /// spawn has no script left to run and fails loudly, so the count is also
    /// asserted from the other side. The one thing a real process added — that
    /// a file on disk was written once — is not expressible without one; the
    /// spawn count is the same fact one layer up.
    #[test]
    fn a_foreground_run_command_reports_one_run_once() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::exits(0).says("hit")));
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("run-once", machine.clone(), clock);
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);

        let report = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "echo hit" }),
            &cancel,
        )
        .unwrap();

        assert_eq!(
            machine.spawned(),
            vec!["echo hit".to_string()],
            "the command ran exactly once — not twice, as it did when the tool \
             box and the foreground path each spawned one"
        );
        assert_eq!(report, "hit\n[exit 0]", "and its one run comes back once");
        assert_eq!(
            actor.ctx.registry.running(),
            0,
            "and nothing was left over as a job"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The three ways a foreground command can stop must not be confusable in
    /// the report the model reads, and the flag mush sets when it kills a
    /// command is the same one for all of them: what tells them apart is *who*
    /// killed it and *how* the watcher learned of it.
    #[test]
    fn the_three_ways_a_foreground_command_ends_are_not_confusable() {
        // A kill from outside the watcher: the process died of a signal, and
        // that is a cancel — not `[exit -1]`, which reads as the command's own
        // doing. This is the arm finding S4 added.
        assert!(matches!(ending(Ended::Exited(-1), true), Ended::Cancelled));
        // A command that ended by itself keeps its exit status, whoever else's
        // signal it was.
        assert!(matches!(ending(Ended::Exited(3), false), Ended::Exited(3)));
        assert!(matches!(
            ending(Ended::Exited(-1), false),
            Ended::Exited(-1)
        ));
        // The watcher's own kills keep their own reasons. They set the same
        // flag on the way out, so an arm that keyed off the flag rather than
        // off the *shape* of the end would report a timeout as a cancel — which
        // is exactly what happened when this was written, and what
        // `a_command_that_runs_forever_is_killed_on_time` and
        // `a_runaway_writer_is_stopped_at_the_output_limit` caught.
        assert!(matches!(ending(Ended::TimedOut, true), Ended::TimedOut));
        assert!(matches!(
            ending(Ended::TooMuchOutput, true),
            Ended::TooMuchOutput
        ));
        assert!(matches!(ending(Ended::Cancelled, true), Ended::Cancelled));
    }

    /// The quit half of finding S4, at the seam: a command killed from *outside*
    /// the watcher — nothing sets the run's own cancel flag, which is the shape
    /// a quit has — is reported as a cancel rather than as an exit code of `-1`.
    /// The kill is the registry's, the same call `App::drop` makes.
    ///
    /// The real clock here on purpose: the watcher sleeps ten milliseconds a
    /// poll, so the test has the whole sixty-second detach window to land its
    /// kill, and the command dies within one poll of it. No fake-clock race
    /// against a deadline nobody is testing.
    #[test]
    fn a_foreground_command_killed_from_outside_reports_cancelled() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let (actor, _mailbox) =
            scripted_tools_actor("killed-outside", machine.clone(), Arc::new(clock::System));
        let registry = actor.ctx.registry.clone();
        let owner = actor.id;

        let running = std::thread::spawn(move || {
            let mut state = ActorState::default();
            let cancel = AtomicBool::new(false);
            exec_tool(
                &actor,
                &mut state,
                ToolName::RunCommand,
                &json!({ "command": "make" }),
                &cancel,
            )
        });
        // Wait for the call to be holding the command, which is the state the
        // whole fix is about: until it is held, there is nothing to kill.
        let mut held = false;
        for _ in 0..2_000 {
            if registry.holding_foreground(owner) {
                held = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(held, "the command never got going");

        registry.kill_owned(owner);
        let report = running
            .join()
            .expect("the agent thread must finish")
            .unwrap();

        assert_eq!(
            report, "[cancelled]",
            "a kill from outside is not an exit code"
        );
        assert_eq!(machine.kills(), 1, "and the command was killed once");
        assert!(
            !registry.holding_foreground(owner),
            "the call holds nothing now"
        );
    }

    /// `Stop` cancels the work in flight, and a `run_command` the agent is
    /// waiting on *is* work in flight (§5.5). The kill reaches the command the
    /// same way it reaches a job — the registry's `kill_owned`, through the
    /// same slot the call is held in — and the model is told the command was
    /// cancelled rather than handed a signal's `-1` as if it were an exit code.
    #[test]
    fn a_stop_kills_the_foreground_command() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let clock = Arc::new(Advanceable::new());
        let (actor, mailbox) = scripted_tools_actor("stop-foreground", machine.clone(), clock);
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        // The human's Ctrl-C, in the actor's own terms: the mailbox is drained
        // by the watcher's next pass, which is the latency a Stop has.
        mailbox.send(AgentMsg::Stop).unwrap();

        let report = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "make" }),
            &cancel,
        )
        .unwrap();

        assert_eq!(report, "[cancelled]", "a stop is not an exit code");
        assert_eq!(machine.kills(), 1, "and the command really was killed");
        assert!(
            !actor.ctx.registry.holding_foreground(actor.id),
            "the call is over, so its slot is gone"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A command that ends by itself is reported with *its* exit status, leaves
    /// no slot held, and is never signalled afterwards — by the call's own end
    /// or by a later `kill_all`. That last part is the safety half of finding
    /// S4's fix: a process group id is free once the command is reaped, so a
    /// kill that landed on a slot a finished call had left behind could kill
    /// somebody else's process. Nothing here is killed, so the count says so.
    #[test]
    fn a_foreground_command_that_ends_normally_leaves_nothing_behind() {
        let machine = Arc::new(
            ScriptedMachine::new()
                .runs(Script::exits(3).says("first"))
                .runs(Script::exits(0).says("second")),
        );
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("foreground-ends", machine.clone(), clock);
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let mut call = |command: &str| {
            exec_tool(
                &actor,
                &mut state,
                ToolName::RunCommand,
                &json!({ "command": command }),
                &cancel,
            )
            .unwrap()
        };

        assert_eq!(call("ouch"), "first\n[exit 3]", "its own exit status");
        assert!(
            !actor.ctx.registry.holding_foreground(actor.id),
            "a finished call holds nothing"
        );
        // A later command is a fresh call with a fresh slot, and the finished
        // one is not in the way of it.
        assert_eq!(call("again"), "second\n[exit 0]");
        assert!(!actor.ctx.registry.holding_foreground(actor.id));
        assert_eq!(machine.kills(), 0, "nothing that ended was signalled");
        actor.ctx.registry.kill_all();
        assert_eq!(
            machine.kills(),
            0,
            "and no later kill lands on a slot a finished call left behind"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The same fact without a subprocess: a foreground `run_command` asks the
    /// machine for one job. The fake machine refuses to invent a script for a
    /// second spawn, so a double start fails loudly rather than silently.
    #[test]
    fn a_foreground_run_command_spawns_one_job() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::exits(0).says("ok")));
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("one-spawn", machine.clone(), clock);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));

        let report = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "make" }),
            &cancel,
        )
        .unwrap();

        assert!(report.contains("ok"), "{report}");
        assert_eq!(
            machine.spawned(),
            vec!["make".to_string()],
            "one command, not two"
        );
        assert_eq!(actor.ctx.registry.running(), 0);
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A leaf (depth `MAX_DEPTH`) has no delegation tools — that is what bounds
    /// the tree — but its own jobs are its business: the subagent prompt names
    /// `command_status`/`wait_commands`/`command_control`, so the schema has to
    /// carry them, and the executors have to answer a leaf exactly as they
    /// answer the root.
    #[test]
    fn a_leaf_agent_keeps_the_job_tools_and_can_manage_its_job() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let clock = Arc::new(Advanceable::new());
        let (mut actor, _mailbox) = scripted_tools_actor("leaf-jobs", machine, clock);
        actor.depth = MAX_DEPTH;

        // The leaf set is the workspace tools and the job tools; the delegation
        // tools are what the depth removes.
        let schemas = tool_schemas(&actor);
        let names: Vec<&str> = schemas
            .iter()
            .map(|schema| schema["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"command_status"), "{names:?}");
        assert!(names.contains(&"command_control"), "{names:?}");
        assert!(names.contains(&"wait_commands"), "{names:?}");
        assert!(!names.contains(&"spawn_agent"), "{names:?}");
        assert!(!names.contains(&"wait_agents"), "{names:?}");

        // And they work: a leaf detaches a job, lists it and stops it.
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut call =
            |tool: ToolName, args: Value| exec_tool(&actor, &mut state, tool, &args, &cancel);
        let started = call(
            ToolName::RunCommand,
            json!({ "command": "npm run dev", "detach": true }),
        )
        .unwrap();
        assert!(started.contains("detached as #c1"), "{started}");
        let status = call(ToolName::CommandStatus, json!({})).unwrap();
        assert!(status.contains("#c1 running "), "{status}");
        assert!(status.contains("npm run dev"), "{status}");
        assert_eq!(
            call(
                ToolName::CommandControl,
                json!({ "id": 1, "action": "stop" })
            )
            .unwrap(),
            "stopping job #c1"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A job's completion is `ChildDone`'s twin: it wakes a napping owner when
    /// there is a result, and folds quietly into the transcript when mush killed
    /// the job.
    #[test]
    fn a_job_completion_wakes_a_napping_owner_only_when_it_is_a_result() {
        let (actor, _mailbox) = test_actor("job-wake");
        let mut state = ActorState::default();
        state.running_jobs.insert(1);
        state.running_jobs.insert(2);
        let mut messages = vec![Message::system("you are mush")];

        let folded = absorb(
            &actor,
            &mut state,
            &mut messages,
            AgentMsg::CommandDone {
                id: 1,
                line: "#c1 done: exit 0 · 3m12s · cargo test — test result: ok".into(),
                news: true,
            },
        );
        assert!(matches!(folded, Fold::Run), "a result is work to answer");
        assert_eq!(
            messages.last().unwrap().text(),
            "#c1 done: exit 0 · 3m12s · cargo test — test result: ok"
        );
        assert!(state.delivered_jobs.contains(&1), "and it counts as read");
        assert!(!state.running_jobs.contains(&1), "and as no longer running");

        let folded = absorb(
            &actor,
            &mut state,
            &mut messages,
            AgentMsg::CommandDone {
                id: 2,
                line: "#c2 stopped after 4s · npm run dev".into(),
                news: false,
            },
        );
        assert!(
            matches!(folded, Fold::Idle),
            "a kill is the human's doing, not a reason to pay for a run"
        );
        assert_eq!(
            messages.last().unwrap().text(),
            "#c2 stopped after 4s · npm run dev",
            "it is in the transcript all the same"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// One home, one rule: a completion is folded in, a nudge stays parked.
    ///
    /// The two are different kinds of user message. A completion belongs *after*
    /// the results of a tool batch — the model has to read it before it decides
    /// what to do next — while the human's words between an assistant's calls
    /// and their results are the shape strict servers reject, so they keep
    /// waiting for `drain_mailbox` on a tool-free turn.
    #[test]
    fn fold_completions_folds_results_and_leaves_nudges_parked() {
        let (actor, events, _mailbox) = recording_actor("fold-boundary");
        let mut state = ActorState::default();
        state.deferred.push(AgentMsg::Nudge("steer".into()));
        note_completion(&mut state, 1, 1, Outcome::Finished("did the thing".into()));
        let mut messages = vec![
            Message::system("you are mush"),
            Message::assistant("working"),
        ];

        assert!(
            fold_completions(&actor, &mut state, &mut messages),
            "a child's result is worth a turn"
        );
        assert_eq!(messages.last().unwrap().text(), "#1 done: did the thing");
        assert!(
            state.delivered.contains_key(&1),
            "and it counts as delivered"
        );

        // A job's report travels the same road, and is news only when the job
        // ended on its own.
        state.running_jobs.insert(2);
        state.done_jobs.insert(
            2,
            JobReport {
                line: "#c2 done: exit 0 · 12s · npm test — ok".into(),
                news: true,
            },
        );
        state.running_jobs.insert(3);
        state.done_jobs.insert(
            3,
            JobReport {
                line: "#c3 stopped after 1s · npm run dev".into(),
                news: false,
            },
        );
        assert!(
            fold_completions(&actor, &mut state, &mut messages),
            "a job's result is work to answer"
        );
        let folded: Vec<&str> = messages.iter().map(Message::text).collect();
        assert!(
            folded.contains(&"#c2 done: exit 0 · 12s · npm test — ok"),
            "the result is folded in: {folded:?}"
        );
        assert!(
            folded.contains(&"#c3 stopped after 1s · npm run dev"),
            "and so is the kill, without a turn being paid for it: {folded:?}"
        );
        assert!(state.delivered_jobs.contains(&2) && state.delivered_jobs.contains(&3));

        // Delivered once: the next boundary has nothing new to say, and the
        // human's parked words are still parked.
        let before = messages.len();
        assert!(!fold_completions(&actor, &mut state, &mut messages));
        assert_eq!(messages.len(), before, "a completion is never repeated");
        assert_eq!(
            state.deferred.len(),
            1,
            "a nudge is not this function's to fold"
        );
        // And every one of those lines reached the copy the human reads.
        let ui = ui_copy(&events);
        for line in [
            "#1 done: did the thing",
            "#c2 done: exit 0 · 12s · npm test — ok",
            "#c3 stopped after 1s · npm run dev",
        ] {
            assert!(
                ui.iter().any(|message| message.text() == line),
                "`{line}` is missing from the UI's copy: {ui:?}"
            );
        }
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A parent that keeps calling tools must still hear its children.
    ///
    /// The bug this guards was seen live: a root made sixty-seven consecutive
    /// tool-calling turns and never learned that its child had finished eight
    /// turns in, because `ChildDone` was *recorded* between tool calls (into
    /// `state.completed`) but only folded into the transcript on a turn with no
    /// tool calls. A parent in a long chain therefore ran past its child's
    /// result indefinitely — the one thing §5.5 promises cannot happen.
    #[test]
    fn a_parent_in_a_tool_chain_still_hears_its_child() {
        let root = std::env::temp_dir().join(format!("mush-chain-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // Three replies: a tool call, a tool call held open so the completion
        // can land while the model is thinking, and an answer.
        let gate = Arc::new(Gate::new());
        let scripted = Arc::new(
            Scripted::new()
                .calls(vec![tool_call("c1", "list_files", json!({}))])
                .held(gate.clone())
                .calls(vec![tool_call("c2", "list_files", json!({}))])
                .says("read it"),
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
                Message::user("do the work"),
            ]))
            .unwrap();

        assert!(
            gate.wait_until_asked(WAIT),
            "the second request never reached the model"
        );
        // The child finishes mid-run, between two tool-calling turns. Nobody
        // asks for it: `wait_agents` is never called.
        root_tx
            .send(AgentMsg::ChildDone {
                id: 1,
                run: 1,
                outcome: Outcome::Finished("did the thing".into()),
            })
            .unwrap();
        gate.release();

        let finished = |events: &Recorder| {
            events
                .events_for(AgentId::ROOT)
                .iter()
                .any(|event| matches!(event, AgentEvent::Done))
        };
        let deadline = Instant::now() + WAIT;
        while !finished(&events) && Instant::now() < deadline {
            let _ = events.wait(Duration::from_millis(50));
        }
        assert!(finished(&events), "the run never ended");

        let asked = scripted.asked();
        assert_eq!(
            asked.len(),
            3,
            "the run ended on the model's answer, not by waiting: {:?}",
            asked.iter().map(|a| a.messages.len()).collect::<Vec<_>>()
        );
        assert!(
            !asked[1].saw("#1 done:"),
            "the request already in flight cannot carry it"
        );
        assert!(
            asked[2].saw("#1 done: did the thing"),
            "the very next request must carry the child's result: {:?}",
            asked[2]
                .messages
                .iter()
                .map(Message::text)
                .collect::<Vec<_>>()
        );

        let _ = root_tx.send(AgentMsg::Shutdown);
        let _ = fs::remove_dir_all(&root);
    }

    /// A completion the model has *not* read survives an adoption: the UI's
    /// transcript replaces the actor's, and the result is still the next thing
    /// folded in — or it would be lost for good.
    ///
    /// The mirror of this is
    /// `a_child_completion_is_delivered_once_across_an_idle_run`: adoption
    /// re-arms nothing that has already been read. Together they say what the
    /// books have to mean — the delivery fact belongs to the actor, and the
    /// transcript is a copy of it, not a second place to keep it.
    #[test]
    fn replacing_the_transcript_keeps_an_unread_completion_deliverable() {
        let (actor, _mailbox) = test_actor("deliver");
        let mut state = ActorState::default();
        let mut messages = vec![Message::system("you are mush"), Message::user("task")];
        // A completion that arrived between boundaries and has not been folded
        // into the model's transcript yet.
        note_completion(&mut state, 1, 1, Outcome::Finished("did the thing".into()));

        let fresh = vec![Message::system("you are mush"), Message::user("carry on")];
        assert!(matches!(
            absorb(&actor, &mut state, &mut messages, AgentMsg::Run(fresh)),
            Fold::Run
        ));
        assert!(
            !state.delivered.contains_key(&1),
            "the model has not read it yet"
        );
        assert!(
            fold_completions(&actor, &mut state, &mut messages),
            "so the next boundary folds it in"
        );
        assert_eq!(messages.last().unwrap().text(), "#1 done: did the thing");
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
        let cancel = Arc::new(AtomicBool::new(false));
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

    /// A parent that keeps calling tools still hears its child. The completion
    /// is folded in at the batch's own boundary, so the very next request
    /// carries it; waiting for a turn that made no calls at all is the failure
    /// §5.5 promises cannot happen, and a parent in a chain of busy turns never
    /// makes that turn.
    #[test]
    fn a_childs_result_reaches_a_parent_that_keeps_calling_tools() {
        let model = Arc::new(
            Scripted::new()
                .calls(vec![tool_call("c0", "list_files", json!({}))])
                .says("all done"),
        );
        let (actor, _events, mailbox) = scripted_actor("child-done-mid-batch", &model);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("orchestrate"),
        ];

        // The child finished while this run was in flight: its outcome is in
        // the mailbox, not yet in the transcript.
        mailbox
            .send(AgentMsg::ChildDone {
                id: 1,
                run: 1,
                outcome: Outcome::Finished("wrote the parser".into()),
            })
            .unwrap();

        let result = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();

        assert_eq!(result.as_deref(), Some("all done"));
        let asked = model.asked();
        assert_eq!(asked.len(), 2, "two turns: the batch's, then the answer");
        assert!(
            asked[1].saw("#1 done: wrote the parser"),
            "the result must travel with the request that follows the batch: {:?}",
            asked[1].messages
        );
        assert_eq!(
            asked[1].messages.last().unwrap().text(),
            "#1 done: wrote the parser",
            "and it is the newest thing the model reads"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// Folding is a hand-over, so it happens once: the same completion cannot
    /// reach the model twice, however many boundaries the run passes.
    #[test]
    fn a_completion_is_delivered_once() {
        let (actor, _mailbox) = test_actor("delivered-once");
        let mut state = ActorState::default();
        let mut messages = vec![Message::system("you are mush")];
        note_completion(
            &mut state,
            1,
            1,
            Outcome::Finished("wrote the parser".into()),
        );

        assert!(fold_completions(&actor, &mut state, &mut messages));
        assert_eq!(messages.len(), 2, "one line for the one completion");
        assert_eq!(messages[1].text(), "#1 done: wrote the parser");
        assert!(
            !fold_completions(&actor, &mut state, &mut messages),
            "the model has read it, so there is nothing left to fold"
        );
        assert_eq!(messages.len(), 2, "and nothing is pushed a second time");
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
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![Message::system("you are mush"), Message::user("task")];

        let result = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();

        assert_eq!(result.as_deref(), Some("done"));
        assert_eq!(model.asked().len(), 2, "the run re-asked after learning");
        assert_eq!(
            actor.ctx.cfg.config().unwrap().context_tokens,
            4_096,
            "the learned window reaches the shared config"
        );
        // The number reaching the shared cell and the number the UI is told
        // are the same event: that pairing is the whole of finding B7.
        assert_eq!(
            contexts(&events),
            vec![(4_096, WindowSource::Complaint)],
            "and the UI is told, on the terms the run trusted it"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A hiccup on the wire is not the end of the run (finding B23: three
    /// agents in one session died mid-work on `Connection reset by peer`, and
    /// one of them had committed nothing). The run asks again, the human is
    /// told each time in the transcript rather than left with a stuck spinner,
    /// and the pause costs the clock seam rather than this suite.
    #[test]
    fn a_transport_hiccup_is_retried_and_the_run_carries_on() {
        let hiccup = "Connection reset by peer (os error 104)";
        let model = Arc::new(
            Scripted::new()
                .fails_transport(hiccup)
                .fails_transport(hiccup)
                .says("done"),
        );
        let clock = Arc::new(Advanceable::new());
        let (actor, events, _mailbox) =
            scripted_actor_on_clock("transport-hiccup", &model, clock.clone());
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![Message::system("you are mush"), Message::user("task")];

        let result = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();

        assert_eq!(result.as_deref(), Some("done"), "the run finished its work");
        assert_eq!(
            model.asked().len(),
            RETRY_ATTEMPTS,
            "and the request was really made three times"
        );
        let mut seen = Watched::default();
        seen.drain(&events);
        assert_eq!(
            seen.notices,
            vec![
                format!("{hiccup} — retrying (2/3)"),
                format!("{hiccup} — retrying (3/3)"),
            ],
            "the human is told, in the agent's own transcript"
        );
        assert_eq!(
            clock.elapsed(),
            Duration::from_millis(1_500),
            "the backoff came through the clock seam: no test waited for it"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// Every attempt loses: the run fails the way it failed before this, with
    /// the wire's own words in front and the attempts named after them — never
    /// a bare "gave up" that hides what the endpoint's side actually said.
    #[test]
    fn a_run_that_never_reaches_the_model_names_the_attempts() {
        let hiccup = "Connection reset by peer (os error 104)";
        let model = Arc::new(
            Scripted::new()
                .fails_transport(hiccup)
                .fails_transport(hiccup)
                .fails_transport(hiccup),
        );
        let (actor, _events, _mailbox) =
            scripted_actor_on_clock("transport-dead", &model, Arc::new(Advanceable::new()));
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![Message::system("you are mush"), Message::user("task")];

        let error = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap_err();

        assert!(
            error.starts_with(&format!(
                "cannot reach {}: {hiccup}",
                actor.ctx.cfg.config().unwrap().base_url
            )),
            "the original error reaches the caller: {error}"
        );
        assert!(
            error.contains("3 attempts"),
            "with its attempts named: {error}"
        );
        assert_eq!(model.asked().len(), RETRY_ATTEMPTS);
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
        let cancel = Arc::new(AtomicBool::new(false));
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

    /// Every context window a run announced to the UI, and on what terms.
    fn contexts(events: &Recorder) -> Vec<(usize, WindowSource)> {
        events
            .events()
            .into_iter()
            .filter_map(|(_, event)| match event {
                AgentEvent::Context { tokens, source } => Some((tokens, source)),
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

    /// A tree whose isolated child has finished its first run, left `iso.txt` on
    /// `mush/1`, and is now idle — the state `/merge` and `/discard` start from.
    ///
    /// Returns the repository, the recorded events, and the *live* child's
    /// mailbox, so a test can do what the commands do and then nudge it. The
    /// child's script answers the nudge with a `write_file extra.txt`, so a test
    /// that forgot to land the worktree would see the file really written.
    fn finished_isolated_child(label: &str) -> (PathBuf, Arc<Recorder>, Sender<AgentMsg>) {
        let root = init_git_repo(label);
        let scripted = Arc::new(
            Scripted::new()
                .when(|asked: &Asked| asked.depth() == Some(1) && !asked.saw("wrote iso.txt"))
                .calls(vec![tool_call(
                    "c1",
                    "write_file",
                    json!({ "path": "iso.txt", "content": "isolated work" }),
                )])
                // The nudge turn: only ever reached if the worktree-nudge is
                // allowed to run, which is the bug this test pins.
                .when(|asked: &Asked| asked.depth() == Some(1) && asked.saw("write extra.txt"))
                .calls(vec![tool_call(
                    "c2",
                    "write_file",
                    json!({ "path": "extra.txt", "content": "phantom" }),
                )])
                .when(|asked: &Asked| asked.depth() == Some(1))
                .says("created iso.txt in my worktree")
                .when(|asked: &Asked| asked.saw("#1 done"))
                .says("child finished")
                .when(|asked: &Asked| asked.saw("spawned agent"))
                .says("child left running")
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
        // The root, the child and the woken root: three runs. The child is then
        // idle with its worktree intact and `iso.txt` on `mush/1`.
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 3),
            "the child must finish and wake its parent: {seen:?}"
        );
        assert!(
            root.join(".mush/wt/1/iso.txt").exists(),
            "the child must have written its worktree"
        );
        let child_tx = events
            .events()
            .into_iter()
            .find_map(|(_, event)| match event {
                AgentEvent::Spawned { child: 1, cmd, .. } => Some(cmd),
                _ => None,
            })
            .expect("the child's Spawned event carries its mailbox");
        (root, events, child_tx)
    }

    /// Do what `/merge` does to a child, with real git: land the branch, then
    /// reclaim the worktree and the branch.
    fn land_with_merge(root: &Path) {
        git::run(root, &["merge", "--no-edit", "mush/1"]).expect("merge mush/1");
        let worktree = git::worktree_path(root, 1);
        git::run(
            root,
            &["worktree", "remove", "--force", worktree.to_str().unwrap()],
        )
        .expect("reclaim the worktree");
        git::run(root, &["branch", "-d", "mush/1"]).expect("delete the branch");
    }

    /// Do what `/discard` does: reclaim the worktree and delete the branch,
    /// without merging.
    fn land_with_discard(root: &Path) {
        let worktree = git::worktree_path(root, 1);
        git::run(
            root,
            &["worktree", "remove", "--force", worktree.to_str().unwrap()],
        )
        .expect("reclaim the worktree");
        git::run(root, &["branch", "-D", "mush/1"]).expect("delete the branch");
    }

    /// After `/merge`, a nudge to the child must be refused: its actor is alive
    /// but its worktree is gone, so a run would recreate `.mush/wt/1` as a plain
    /// directory where no surface could see, diff or land the file — the work
    /// would exist somewhere nothing can reach (finding S1). The test asserts
    /// the refusal, the untouched main tree, and no recreated directory.
    #[test]
    fn a_nudge_to_a_merged_child_is_refused_not_run_in_the_phantom_path() {
        let (root, events, child_tx) = finished_isolated_child("s1-merged");
        let worktree = git::worktree_path(&root, 1);
        land_with_merge(&root);

        child_tx
            .send(AgentMsg::Nudge("write extra.txt".to_string()))
            .unwrap();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen
                .notices
                .iter()
                .any(|line| line.contains("worktree is gone"))),
            "the child says why it did not run: {seen:?}"
        );
        assert!(
            !worktree.exists(),
            "the reclaimed path must not be recreated"
        );
        assert!(
            !root.join("extra.txt").exists(),
            "and no run landed in the root either"
        );
        assert_eq!(
            git::run(&root, &["status", "--porcelain"]).unwrap_or_default(),
            "",
            "the main tree is untouched"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The same after `/discard`: the work was thrown away on purpose, and a
    /// nudge must not quietly recreate the path it was thrown from.
    #[test]
    fn a_nudge_to_a_discarded_child_is_refused_not_run_in_the_phantom_path() {
        let (root, events, child_tx) = finished_isolated_child("s1-discarded");
        let worktree = git::worktree_path(&root, 1);
        land_with_discard(&root);

        child_tx
            .send(AgentMsg::Nudge("write extra.txt".to_string()))
            .unwrap();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen
                .notices
                .iter()
                .any(|line| line.contains("worktree is gone"))),
            "the child says why it did not run: {seen:?}"
        );
        assert!(
            !worktree.exists(),
            "the reclaimed path must not be recreated"
        );
        assert_eq!(
            git::run(&root, &["status", "--porcelain"]).unwrap_or_default(),
            "",
            "the main tree is untouched"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// `isolated: true` in a workspace that is not a git repository: the child
    /// runs in the shared workspace, and until now *only the model* was told —
    /// the reason travelled in the child's brief and nowhere else. The human
    /// asked for a child of their own, and what they got was one sharing their
    /// checkout, with a row that looks exactly like a child that never asked to
    /// be isolated: two of them would edit the same files while the human
    /// believed they were apart.
    ///
    /// Both halves are pinned here: the spawn's answer to the model is
    /// unchanged (the same result line, and the brief still carrying why), and
    /// the same fact now reaches the parent's pane as a notice.
    #[test]
    fn a_degraded_isolation_is_said_to_the_human_too() {
        let gate = Arc::new(Gate::new());
        let scripted = Arc::new(
            Scripted::new()
                .when(|asked: &Asked| asked.depth() == Some(1))
                .held(gate.clone())
                .says("child answered"),
        );
        // A scratch directory, not `init_git_repo`: the workspace this test
        // opens is exactly the plain one the finding describes.
        let (actor, events, _mailbox) = build_actor_about(
            "isolation-in-place",
            scripted.clone(),
            test_cfg(),
            Arc::new(ScriptedMachine::new()),
            Arc::new(clock::System),
        );
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);

        let report = exec_tool(
            &actor,
            &mut state,
            ToolName::SpawnAgent,
            &json!({ "brief": "do the thing", "isolated": true }),
            &cancel,
        )
        .unwrap();

        // The model's answer is unchanged: the same line it always got, and the
        // child's own brief still carries the reason it has no worktree.
        assert_eq!(
            report,
            "spawned agent #1 · runs until it stops calling tools · wait_agents returns its summary"
        );
        assert!(
            gate.wait_until_asked(WAIT),
            "the child must ask for its first turn"
        );
        let asked = scripted.asked();
        assert!(
            asked[0].saw("(isolated unavailable: not a git repository; running in place)"),
            "the model is still told: {:?}",
            asked[0].messages
        );
        gate.release();

        // The human's half: a notice on the parent, naming the child and the
        // reason, and — the row's own honesty — a spawn with no branch, so no
        // surface can claim a worktree this child does not have.
        let notices: Vec<String> = events
            .events_for(AgentId(7))
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::Notice(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            vec![
                "#1 isolated unavailable: not a git repository — it shares this workspace"
                    .to_string()
            ],
            "the human must be told what the model was told"
        );
        let spawned_branch = events
            .events()
            .into_iter()
            .find_map(|(_, event)| match event {
                AgentEvent::Spawned { branch, .. } => Some(branch),
                _ => None,
            })
            .expect("the child was spawned");
        assert_eq!(
            spawned_branch, None,
            "the row has no branch, so it cannot imply a worktree that does not exist"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
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
        while total <= compaction_trigger(budget) {
            let assistant = Message::assistant(format!("reply {index} {}", "x".repeat(280)));
            let user = Message::user(format!("again {index}"));
            total += assistant.weight() + user.weight();
            messages.push(assistant);
            messages.push(user);
            index += 1;
        }
        assert!(
            total > compaction_trigger(budget) && total <= budget,
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

        root_tx.send(AgentMsg::Compact(Vec::new())).unwrap();
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

        // One ask for the run, one for the fold — the second carries the
        // instruction, and no answer was requested after it.
        let asked = scripted.asked();
        assert_eq!(asked.len(), 2, "one summarize call, nothing else");
        assert!(asked[1].saw(COMPACT_INSTRUCTION), "the second ask folds");
        // The tools travel with the summarize call, and so does the way they are
        // offered. They are the head of the rendered prompt, so a request
        // without them is no prefix of the conversation: the endpoint's cache
        // would miss on every token, and the history it re-prefills is the
        // largest one there has ever been — the cost compaction exists to avoid,
        // paid at the worst moment. What stops a tool call is the instruction in
        // the appended user message, not a request field.
        assert_eq!(
            asked[1].tool_schemas, asked[0].tool_schemas,
            "the fold must share the run's prefix, tools included"
        );
        assert_eq!(
            asked[1].tool_choice, asked[0].tool_choice,
            "the fold asks the model the same thing, not a different kind of turn"
        );
        assert!(
            !asked[1].tool_schemas.is_empty(),
            "and they must be the real schemas, not two empty lists"
        );
        assert!(
            asked[1].saw("call no tool"),
            "the instruction is where the tools are refused: {}",
            COMPACT_INSTRUCTION
        );

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
        root_tx.send(AgentMsg::Compact(Vec::new())).unwrap();
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

    /// A fold the human asked for is said out loud, at every step it takes:
    /// parked while the run it arrived behind is still going, on the wire when
    /// it fires, and landed when the transcript is replaced. Exactly once — the
    /// half of the bug where a request was *silent* (finding U11).
    ///
    /// The actor reads its mailbox between the things it does, not while a
    /// request is in flight, so what this pins is the order: the request is
    /// acknowledged as parked before anything is folded, and the fold is asked
    /// for once.
    #[test]
    fn a_compact_request_mid_run_says_where_it_is_every_step() {
        let root = scratch_dir("compact-parked-visible");
        let gate = Arc::new(Gate::new());
        let summary = "wrote note.txt; nothing else happened";
        let scripted = Arc::new(
            Scripted::new()
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

        root_tx.send(AgentMsg::Compact(Vec::new())).unwrap();
        gate.release();
        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 1),
            "the run must finish with the fold folded in: {seen:?}"
        );
        assert_eq!(seen.errors, Vec::<String>::new());
        assert_eq!(
            seen.folds,
            vec![
                "Parked".to_string(),
                "Requested".to_string(),
                "landed".to_string()
            ],
            "the whole fold, once, step by step: {seen:?}"
        );
        let folds = scripted
            .asked()
            .into_iter()
            .filter(|asked| asked.saw(COMPACT_INSTRUCTION))
            .count();
        assert_eq!(folds, 1, "a parked request folds once, not once per turn");
        let _ = fs::remove_dir_all(&root);
    }

    /// A fold the window triggered — nobody asked, the history is three quarters
    /// of the budget — reaches the same visible state, and says *why* it is
    /// happening: the human did not ask for this one. It folds once.
    #[test]
    fn a_full_history_folds_once_and_says_the_window_asked() {
        let root = scratch_dir("compact-auto-visible");
        let summary = "condensed work so far";
        let scripted = Arc::new(
            Scripted::new()
                .when(|asked: &Asked| asked.saw(COMPACT_INSTRUCTION))
                .says(summary)
                .calls(vec![tool_call(
                    "c1",
                    "write_file",
                    json!({ "path": "after.txt", "content": "written after the fold" }),
                )])
                .says("done"),
        );
        let events = Recorder::new();
        let root_tx = spawn_scripted(
            Config::new("http://127.0.0.1:1", "scripted", None),
            events.clone(),
            root.clone(),
            scripted.clone(),
        )
        .tx;
        let budget = scripted_budget();
        // Past the trigger, and still one the summarize request can carry: the
        // window the automatic fold fires in.
        let long = "x".repeat(mush_core::transcript::compaction_trigger(budget) + 1_000);
        assert!(
            long.len() <= budget,
            "the transcript must fit the whole budget"
        );
        root_tx
            .send(AgentMsg::Run(vec![
                Message::system("you are mush"),
                Message::user("read this".to_string()),
                Message::assistant(long),
            ]))
            .unwrap();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 1),
            "the run must finish: {seen:?}"
        );
        assert_eq!(seen.errors, Vec::<String>::new());
        assert_eq!(
            seen.folds,
            vec!["NearlyFull".to_string(), "landed".to_string()],
            "the window's fold is visible too, and it fires once: {seen:?}"
        );
        assert_eq!(seen.summaries, vec![summary.to_string()]);
        let folds = scripted
            .asked()
            .into_iter()
            .filter(|asked| asked.saw(COMPACT_INSTRUCTION))
            .count();
        assert_eq!(
            folds, 1,
            "the folded transcript is small again, so no turn folds a second time"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// `compacting…` cannot outlive the fold that justified it. An endpoint that
    /// refuses the summarize call, or answers something mush cannot read as a
    /// summary, is the notice's to report — the row goes quiet either way, and a
    /// human who asked is told (finding U11).
    #[test]
    fn a_fold_that_fails_leaves_no_fold_on_the_row() {
        let root = scratch_dir("compact-failed");
        let scripted = Arc::new(
            Scripted::new()
                .says("the run's answer")
                .fails_with(500, "no summarizer today"),
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
        let mut ran = Watched::default();
        assert!(
            ran.wait(&events, WAIT, |seen| seen.done >= 1),
            "the run must finish: {ran:?}"
        );

        root_tx.send(AgentMsg::Compact(Vec::new())).unwrap();
        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen
                .folds
                .contains(&"ended".to_string())),
            "the fold must end, however it ends: {seen:?}"
        );
        assert_eq!(
            seen.folds,
            vec!["Requested+flag".to_string(), "ended".to_string()],
            "a fold from rest owns the flag that can stop it: {seen:?}"
        );
        assert!(
            seen.notices
                .iter()
                .any(|notice| notice.contains("could not compact")),
            "a human who asked is told why nothing happened: {:?}",
            seen.notices
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// An idle fold can be stopped, and a stop is a stop: the actor says
    /// `Stopped`, which is what takes the `⊘` off the row's fold and leaves the
    /// agent resumable. A cancelled fold is not a failure to report.
    #[test]
    fn a_stopped_fold_is_a_stop_and_not_a_failure() {
        let root = scratch_dir("compact-stopped");
        let scripted = Arc::new(Scripted::new().says("the run's answer").cancels());
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
        let mut ran = Watched::default();
        assert!(ran.wait(&events, WAIT, |seen| seen.done >= 1), "{ran:?}");

        root_tx.send(AgentMsg::Compact(Vec::new())).unwrap();
        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.stopped >= 1),
            "the actor must say the fold was stopped: {seen:?}"
        );
        assert_eq!(seen.errors, Vec::<String>::new());
        assert_eq!(seen.summaries, Vec::<String>::new());
        assert!(
            seen.notices.is_empty(),
            "a stop is the human's doing, not a failure: {:?}",
            seen.notices
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// An actor restored from a session starts with no transcript — the UI
    /// holds the conversation until the human's next message. A fold is not a
    /// run, so the request carries it: without that a `/compact` on a restored
    /// agent folded nothing, and said nothing about it (finding U11).
    #[test]
    fn a_compact_request_carries_the_transcript_an_actor_has_none_of() {
        let root = scratch_dir("compact-adopted");
        let summary = "the task and where it got to";
        let scripted = Arc::new(
            Scripted::new()
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
        // No `Run` first: this is the actor a restart leaves behind.
        root_tx
            .send(AgentMsg::Compact(vec![
                Message::system("you are mush"),
                Message::user("the old task".to_string()),
                Message::assistant("the old answer"),
            ]))
            .unwrap();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| !seen.summaries.is_empty()),
            "a fold with nothing to fold is a command that does nothing: {seen:?}"
        );
        assert_eq!(seen.summaries, vec![summary.to_string()]);
        let asked = scripted.asked();
        assert_eq!(
            asked.len(),
            1,
            "one summarize call, and the transcript it carried"
        );
        assert!(
            asked[0].saw("the old task"),
            "the fold is of the conversation the request carried"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The history budget of the config the tests spawn actors with, so a test
    /// can build a transcript that sits in the compaction window.
    fn scripted_budget() -> usize {
        Config::new("http://127.0.0.1:1", "scripted", None).history_budget()
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
        root_tx.send(AgentMsg::Compact(Vec::new())).unwrap();
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

        root_tx.send(AgentMsg::Compact(Vec::new())).unwrap();
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

    /// `/compact` typed into a *fresh* workspace: the actor has never run, so
    /// its transcript is not even a system message, and the fold cannot
    /// replace what is not there. That is still a human who typed a command,
    /// and the answer they got was nothing at all — the bar painted
    /// `compacting #0…` and then the bar went quiet: no fold, no refusal, no
    /// request. Silence there is the failure mode `/compact` exists to avoid,
    /// so the empty case says the same refusal the short one does.
    #[test]
    fn a_fold_of_an_empty_transcript_says_so_instead_of_nothing() {
        let root = scratch_dir("compact-empty");
        let scripted = Arc::new(Scripted::new().says("answered"));
        let events = Recorder::new();
        let root_tx = spawn_scripted(
            Config::new("http://127.0.0.1:1", "scripted", None),
            events.clone(),
            root.clone(),
            scripted.clone(),
        )
        .tx;
        // No `Run` at all: this is the transcript a launch leaves behind. The
        // UI's copy is empty too, so the actor stays without one — which is the
        // case the refusal is about.
        root_tx.send(AgentMsg::Compact(Vec::new())).unwrap();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| !seen.notices.is_empty()),
            "an empty /compact must be answered: {seen:?}"
        );
        assert_eq!(
            seen.notices,
            vec![NOTHING_TO_COMPACT.to_string()],
            "the same refusal a too-short transcript gets"
        );
        assert!(
            seen.summaries.is_empty() && seen.done == 0,
            "nothing was folded and no run started: {seen:?}"
        );
        assert!(
            scripted.asked().is_empty(),
            "the refusal costs no model call: {:?}",
            scripted.asked().len()
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A fold is a model call like any other: the reply cap travels under the
    /// endpoint's own field name, the same one the run's ask uses. The fold
    /// used to build its own `ChatRequest` and always set `max_tokens`, so an
    /// endpoint configured for `max_completion_tokens` (OpenAI's reasoning
    /// models) refused the fold with a 400 — and `compact_history` swallowed
    /// the refusal, so `/compact` silently never happened.
    #[test]
    fn a_fold_carries_the_cap_under_the_endpoints_field() {
        let root = scratch_dir("compact-cap-field");
        let summary = "the task was to say something; it was said";
        let scripted = Arc::new(
            Scripted::new()
                .when(|asked: &Asked| asked.saw(COMPACT_INSTRUCTION))
                .says(summary)
                .says("first answer"),
        );
        let mut cfg = Config::new("http://127.0.0.1:1", "scripted", None);
        cfg.max_completion_tokens = true;
        let events = Recorder::new();
        let root_tx = spawn_scripted(cfg, events.clone(), root.clone(), scripted.clone()).tx;
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

        root_tx.send(AgentMsg::Compact(Vec::new())).unwrap();
        assert!(
            seen.wait(&events, WAIT, |seen| !seen.summaries.is_empty()),
            "the endpoint's own field is what the fold must send: {seen:?}"
        );

        let asked = scripted.asked();
        let fold = asked.last().expect("the fold asked the model");
        assert!(fold.saw(COMPACT_INSTRUCTION), "the last ask is the fold");
        assert_eq!(
            fold.max_completion_tokens,
            Some(COMPACT_REPLY_TOKENS),
            "the fold's cap, under the field this endpoint requires"
        );
        assert_eq!(
            fold.max_tokens, 0,
            "and never the field this endpoint rejects"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A fold the endpoint refuses is not a silent one when the human asked for
    /// it: mush must say the `/compact` did not happen, or it reads as one that
    /// did. The automatic trigger stays quiet — its run's own request follows
    /// and will say the same thing.
    #[test]
    fn a_refused_fold_is_not_silent_when_it_was_asked_for() {
        let root = scratch_dir("compact-refused");
        let scripted = Arc::new(
            Scripted::new()
                .when(|asked: &Asked| asked.saw(COMPACT_INSTRUCTION))
                .fails_with(
                    400,
                    "{\"error\":{\"message\":\"max_completion_tokens is required\"}}",
                )
                .says("first answer"),
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
            "the run must finish first: {seen:?}"
        );

        root_tx.send(AgentMsg::Compact(Vec::new())).unwrap();
        assert!(
            seen.wait(&events, WAIT, |seen| seen
                .notices
                .iter()
                .any(|line| line.contains("could not compact"))),
            "an asked fold the endpoint refused must be told, not swallowed: {seen:?}"
        );
        assert!(
            seen.summaries.is_empty(),
            "nothing was folded, so no summary was claimed"
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
        // The tools stay even here, and so does `tool_choice`: the schemas are
        // the head of the prompt, so withdrawing them re-prefills the longest
        // history the run has had. The instruction is what tells the model to
        // stop calling them, and a call it makes anyway is answered, not run.
        assert_eq!(
            asked[RUNAWAY_TURNS - 1].tool_schemas,
            asked[0].tool_schemas,
            "the wrap-up turn must share the run's prefix, tools included"
        );
        assert!(
            !asked[RUNAWAY_TURNS - 1].tool_schemas.is_empty(),
            "and they must be the real schemas, not two empty lists"
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

    /// A refused round is not a repeat: nothing ran, so nothing is being
    /// repeated. Two agents died to a lock refusal counted as "the same call
    /// with nothing changed in between" (finding H13), so this is the rule the
    /// guard reads, tested on its own.
    #[test]
    fn a_refused_round_is_not_a_loop() {
        let mut last = String::new();
        let mut repeats = 0usize;
        for _ in 0..LOOP_ROUNDS + 3 {
            count_round(&mut last, &mut repeats, "run_command:{}", true);
        }
        assert_eq!(repeats, 0, "refusals never accumulate");
        // The same batch that actually ran does accumulate and trips the guard.
        for _ in 0..LOOP_ROUNDS {
            count_round(&mut last, &mut repeats, "run_command:{}", false);
        }
        assert_eq!(repeats, LOOP_ROUNDS, "a real repeat still trips it");
        // And a different batch starts over, refusals or not.
        count_round(&mut last, &mut repeats, "read_file:{}", false);
        assert_eq!(repeats, 0);
    }

    /// A run stopped as a loop can be resumed. The next run opens with the
    /// guard's own words, so the model is told to change what it does instead
    /// of repeating the call that stopped it — nudging a loop-stopped agent
    /// used to re-stop it immediately and identically, which made the row's
    /// promise to resume it unactionable (finding H14).
    #[test]
    fn a_loop_stopped_run_resumes_with_a_warning() {
        let root = scratch_dir("loop-resume");
        let mut scripted = Scripted::new();
        for _ in 0..LOOP_ROUNDS + 1 {
            scripted = scripted.calls(vec![tool_call(
                "call",
                "write_file",
                json!({ "path": "same.txt", "content": "same" }),
            )]);
        }
        let scripted = Arc::new(scripted.says("changed my approach"));
        let mut cfg = Config::new("http://127.0.0.1:1", "scripted", None);
        cfg.context_tokens = 128_000;
        let events = Recorder::new();
        let root_tx = spawn_scripted(cfg, events.clone(), root.clone(), scripted.clone()).tx;
        let opening = || {
            vec![
                Message::system(prompt::system_prompt(root.to_str().unwrap())),
                Message::user("LOOP: keep writing the same file".to_string()),
            ]
        };
        root_tx.send(AgentMsg::Run(opening())).unwrap();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| !seen.errors.is_empty()),
            "the loop guard must stop the run: {seen:?}"
        );
        assert!(
            seen.errors
                .iter()
                .any(|error| error.contains("stopped as a loop")),
            "the run ends as a loop: {:?}",
            seen.errors
        );

        // The nudge: a new run with the human's words. The model must be told
        // why it was stopped before it is asked again.
        root_tx.send(AgentMsg::Run(opening())).unwrap();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done > 0),
            "the resumed run must finish: {seen:?}"
        );
        let asked = scripted.asked();
        assert!(
            asked
                .last()
                .unwrap()
                .saw("previous run was stopped as a loop"),
            "the resumed request must carry the guard's words"
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
        /// Every fold state this actor reported, in order: what the row, the
        /// bar and the foot were told (finding U11). `Parked`, `Requested`,
        /// `NearlyFull`, `ended`, `landed`.
        folds: Vec<String>,
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
                AgentEvent::Compact { summary, .. } => {
                    self.summaries.push(summary);
                    self.folds.push("landed".to_string());
                }
                AgentEvent::Compacting { why, cancel } => self.folds.push(format!(
                    "{why:?}{}",
                    if cancel.is_some() { "+flag" } else { "" }
                )),
                AgentEvent::CompactingEnded { .. } => self.folds.push("ended".to_string()),
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
