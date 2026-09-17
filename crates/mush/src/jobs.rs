//! The job registry: every command that outlived its tool call.
//!
//! A worktree isolates files and nothing else. Two agents share CPU, memory,
//! ports, `/tmp` and the human's patience, so a command that outlives
//! `CMD_DETACH_AFTER` stops being a tool call and becomes a **job**: it keeps
//! its process group, and its owner is told how it ended instead of reading a
//! timeout. This module owns that fact and nothing else — which commands are
//! alive, who started each, and how to stop it. Agents, transcripts and pixels
//! are elsewhere.
//!
//! One registry per conversation, shared by every agent in the tree through
//! `AgentCtx` (the same handle `ids` and `live` travel through), because a job
//! is a fact about the *machine*, not about one actor: the budget is
//! machine-wide, the lock is machine-wide, and `/new` has to be able to kill
//! everything the old tree left running. Job ids are drawn from the tree's one
//! id counter, so `#c2` can never collide with agent `#2`, with `#N` in a
//! message, or with an id recovered from a leftover worktree (finding B1).
//!
//! Three rules live here:
//!
//! 1. **A kept output window is a tail.** §11.9 asked; the answer is the tail,
//!    consistently, in the completion line and in `command_status` alike. A job
//!    is read when it *ends*, and what ended it is at the bottom of the log:
//!    `test result: FAILED`, `error: could not compile`, the panic. The head is
//!    what a foreground command's result keeps, because the model reads it
//!    while the command still runs. There is no second window and no second
//!    source: both readers go through [`preview`].
//! 2. **A job dies with its owner.** `Stop`, `Shutdown`, `/new` and quitting
//!    mush all end up in [`Registry::kill_owned`] or [`Registry::kill_all`], and
//!    [`Registry`]'s `Drop` is the backstop for a path that forgets. A build an
//!    agent started must not outlive a clean quit.
//! 3. **One command at a time may own the machine.** An `exclusive` command
//!    takes a workspace-wide lock, so a benchmark, a profiler, or anything that
//!    binds a fixed port runs without a sibling stealing cores. The lock
//!    coordinates *agents*: it cannot see the human's own build, a service, or
//!    an unrelated process, so it is "agents do not fight each other", not
//!    isolation. A refusal names the holder, because "the machine is busy" with
//!    no name leaves the model with nothing to do about it.
//!
//! Locking: one `Mutex` guards the registry's own bookkeeping, and it is never
//! held while a command's handle is locked. The watch thread locks the handle to
//! poll and then the registry to report; every reader here takes a copy of the
//! records under the registry lock, drops it, and only then touches a handle.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use mush_core::workspace::tail_for_model;

use crate::agent::{AgentEvent, AgentMsg};
use crate::app::{short_age, AgentId};
use crate::clock::Clock;
use crate::events::Events;
use crate::machine::Job;

/// How many jobs may be alive in one workspace at once, beside `MAX_AGENTS`.
///
/// Each job is a thread, a process group and disk, and it can outlive the run
/// that started it — so this is a budget for the *machine*, not for one agent: a
/// per-agent cap would let eight agents hold eight builds each, which is the
/// situation the cap exists to prevent.
pub const MAX_JOBS: usize = 8;

/// How many bytes of each stream a job keeps. The window is a tail (see the
/// module docs), so a `cargo build` that printed 40 MB is described by its last
/// few dozen lines rather than by its first ones.
pub const JOB_TAIL: usize = 2_000;

/// How much of that tail goes into a completion line. Small on purpose: the line
/// is folded into a transcript and sent to the model, so it summarises the end
/// of the command and is not the log.
const JOB_LINE_TAIL: usize = 400;

/// How many finished jobs stay listed. A job the owner has already read about
/// does not need to stay forever; what is running and what just ended is what
/// anyone asks about.
const JOB_HISTORY: usize = 8;

/// How often a running command is polled. Ten milliseconds is the latency
/// between a `Stop` and a process group dying, and costs nothing while idle.
const POLL: Duration = Duration::from_millis(10);

/// Hard ceiling on what one command may write to its scratch files before mush
/// stops it. The disk is shared by every agent, and a command that gets here is
/// not communicating, it is running away. One number for both watchers — the
/// foreground one in `agent.rs` and a job's own thread here — so they cannot
/// drift.
pub const CMD_OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;

/// How long a command may run as a tool call before it becomes a job.
pub const CMD_DETACH_AFTER: Duration = Duration::from_secs(60);

/// `#c2` — a job's name, as the model and the human both read it. The `c` is
/// what tells a command's id from an agent's at a glance.
pub fn label(id: u64) -> String {
    format!("#c{id}")
}

/// Which of the two waits a call is: `wait_agents` or `wait_commands`.
///
/// One enum rather than three parallel strings (a noun, a tool name and a label
/// builder), because they have to agree: a message that says "your agents are
/// still running" in a job's wait is worse than no message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waited {
    Agents,
    Jobs,
}

impl Waited {
    /// What the wait is waiting for, in a sentence.
    pub fn noun(self) -> &'static str {
        match self {
            Waited::Agents => "agents",
            Waited::Jobs => "jobs",
        }
    }

    /// The tool that runs this wait.
    pub fn tool(self) -> &'static str {
        match self {
            Waited::Agents => "wait_agents",
            Waited::Jobs => "wait_commands",
        }
    }

    /// One thing being waited for, as the model names it (`#2`, `#c2`).
    pub fn label(self, id: u64) -> String {
        match self {
            Waited::Agents => format!("#{id}"),
            Waited::Jobs => label(id),
        }
    }
}

/// Why a command had to be stopped even though it had not ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    TimedOut,
    Cancelled,
    TooMuchOutput,
}

/// Whether a still-running command must now be stopped. `limit` is the
/// foreground waiter's timeout: a detached job has none — running until it ends
/// is the whole point — so `None` means only a cancellation or a runaway writer
/// can stop it. One function, so both watchers answer this the same way.
pub fn stopping(
    written: u64,
    waited: Duration,
    limit: Option<Duration>,
    cancel: bool,
) -> Option<Stopped> {
    if limit.is_some_and(|limit| waited > limit) {
        return Some(Stopped::TimedOut);
    }
    if cancel {
        return Some(Stopped::Cancelled);
    }
    if written > CMD_OUTPUT_LIMIT {
        return Some(Stopped::TooMuchOutput);
    }
    None
}

/// How a job ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobOutcome {
    /// It ended by itself, with this exit code (`-1` when a signal ended it).
    Exited(i32),
    /// mush stopped it: a `Stop` aimed at its owner, `command_control stop`,
    /// `/new`, or quitting.
    Stopped,
    /// It passed the output limit, so mush killed it rather than let it fill the
    /// disk.
    TooMuchOutput,
}

impl JobOutcome {
    /// Whether this is news worth waking a napping owner for. A job mush killed
    /// is the human's or the model's own doing; a job that *ended* is a result
    /// nobody has read yet.
    pub fn is_news(&self) -> bool {
        matches!(self, JobOutcome::Exited(_))
    }

    /// The one line a job is reported in: `#c2 done: exit 0 · 3m12s · cargo
    /// test — test result: ok.`. Kept here so the transcript line, the bar and
    /// `command_status` say the same thing about the same job.
    pub fn line(&self, id: u64, command: &str, age: Duration, tail: &str) -> String {
        let head = match self {
            JobOutcome::Exited(code) => {
                format!("{} done: exit {code} · {}", label(id), short_age(age))
            }
            JobOutcome::Stopped => format!("{} stopped after {}", label(id), short_age(age)),
            JobOutcome::TooMuchOutput => format!(
                "{} killed: it wrote past {CMD_OUTPUT_LIMIT} bytes · {}",
                label(id),
                short_age(age)
            ),
        };
        let tail = preview_tail(tail);
        if tail.is_empty() {
            format!("{head} · {command}")
        } else {
            format!("{head} · {command} — {tail}")
        }
    }
}

/// What a job's kept window looks like inside one line: the very end of it, on
/// one line, with an ellipsis where the rest was. `·` joins its lines because
/// this is a summary of the end; the window itself is in `command_status`.
pub fn preview_tail(tail: &str) -> String {
    let lines: Vec<&str> = tail
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    let text = lines.join(" · ");
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= JOB_LINE_TAIL {
        return text;
    }
    // The end of the end: a line that kept the first 400 columns of the window
    // would cut off the one thing a tail is read for.
    let kept: String = chars[chars.len() - JOB_LINE_TAIL + 1..].iter().collect();
    format!("…{kept}")
}

/// One job as the registry holds it.
struct Record {
    id: u64,
    owner: u64,
    command: String,
    started: Instant,
    /// How to reach it while it runs; `None` once it has ended.
    live: Option<Live>,
    /// The line its owner reads. `None` until it ends.
    line: Option<String>,
    /// The end of what it wrote: read live while it runs, captured as it ends.
    tail: String,
}

impl Record {
    fn running(&self) -> bool {
        self.live.is_some()
    }
}

/// The handles a running job is reached through. Cloned into the job's own
/// thread at launch, which is what lets the registry answer "what has it
/// written" and "stop it" while that thread polls it.
#[derive(Clone)]
struct Live {
    job: Arc<Mutex<Box<dyn Job>>>,
    /// Set by anything in mush that wants this job to stop, so the thread can
    /// report *why* it ended instead of blaming the command's exit code.
    stop: Arc<AtomicBool>,
}

impl Live {
    /// Stop it and everything it started, now. Killing is idempotent and goes
    /// through the handle rather than the flag: on `/new` and on quit the
    /// process groups must be gone before this returns, not ten milliseconds
    /// later.
    fn kill(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut job) = self.job.lock() {
            job.kill();
        }
    }

    /// The end of what it has written so far.
    fn tail(&self) -> String {
        let Ok(job) = self.job.lock() else {
            return String::new();
        };
        let (stdout, stderr) = job.tail(JOB_TAIL);
        preview(&stdout, &stderr)
    }
}

/// Who holds the machine, and with what.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub agent: u64,
    pub command: String,
}

/// Why a command may not be started. Every sentence the model reads about a
/// refusal is built here, so the condition and the wording cannot drift apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Another agent holds the workspace-wide lock.
    Machine(Held),
    /// The machine-wide job budget is full.
    Budget,
    /// The job's own thread could not start.
    Thread(String),
}

impl Refused {
    /// What the model is told, in words that let it act: who to wait for, or
    /// what to stop.
    pub fn message(&self, asker: u64) -> String {
        match self {
            Refused::Machine(held) if held.agent == asker => format!(
                "you hold the machine with an exclusive command ({}); wait for it \
                 (wait_commands) or stop it (command_control stop) before starting another",
                held.command
            ),
            Refused::Machine(held) => {
                format!("#{} holds the machine; retry when it finishes", held.agent)
            }
            Refused::Budget => format!(
                "cannot detach: {MAX_JOBS} commands are already running as jobs (the limit). \
                 Stop one with command_control, or wait for one with wait_commands."
            ),
            Refused::Thread(error) => format!("could not start the job: {error}"),
        }
    }
}

/// What to start. Built by the caller (`run_command`), which owns the decision
/// to detach; the registry owns admission and watching.
pub struct Launch {
    pub owner: u64,
    pub command: String,
    /// Whether this command owns the machine for its whole life.
    pub exclusive: bool,
    pub job: Box<dyn Job>,
    /// Where the completion lands: the owner's own mailbox, exactly as a child's
    /// completion does.
    pub mailbox: Sender<AgentMsg>,
}

/// Every live job in one conversation, and the machine-wide lock.
pub struct Registry {
    clock: Arc<dyn Clock>,
    events: Arc<dyn Events>,
    /// The tree's id counter, shared with the agents: one space, no collisions.
    ids: Arc<AtomicU64>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// In id order: at most `MAX_JOBS` live plus `JOB_HISTORY` finished, so a
    /// handful of records.
    jobs: BTreeMap<u64, Record>,
    /// The agent holding the machine, its command, and the job holding it while
    /// that job runs.
    holder: Option<(u64, String, Option<u64>)>,
}

impl Registry {
    pub fn new(clock: Arc<dyn Clock>, events: Arc<dyn Events>, ids: Arc<AtomicU64>) -> Arc<Self> {
        Arc::new(Self {
            clock,
            events,
            ids,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// A registry with no clock of its own, no sink, and nobody to tell: for the
    /// tests that only need the type to exist (the tree's, the app's). Nothing
    /// should ever be launched in one — `launch` would work, and nothing would
    /// hear about it.
    #[cfg(test)]
    pub fn bare() -> Arc<Self> {
        Self::new(
            Arc::new(crate::clock::System),
            Arc::new(Silent),
            Arc::new(AtomicU64::new(1)),
        )
    }

    /// The jobs still running for `owner`, oldest first: what a row, a footer or
    /// the bar shows. No command handle is touched, so the painter may ask.
    pub fn live_for(&self, owner: u64) -> Vec<JobView> {
        let now = self.clock.now();
        let held = self.held();
        self.jobs()
            .into_iter()
            .filter(|record| record.owner == owner && record.running())
            .map(|record| JobView {
                age: now.saturating_duration_since(record.started),
                id: record.id,
                command: record.command,
                exclusive: matches!(&held, Some((_, _, Some(job))) if *job == record.id),
            })
            .collect()
    }

    /// How many jobs are alive right now — the number the budget is about.
    pub fn running(&self) -> usize {
        self.jobs().iter().filter(|record| record.running()).count()
    }

    /// Whether there is room for one more job. Checked before a process is
    /// spawned: a command that cannot be watched must not be started.
    pub fn has_room(&self) -> bool {
        self.running() < MAX_JOBS
    }

    /// Whether `agent` may run a command at all. Only an exclusive command takes
    /// the lock, but *every* command respects it: a benchmark with a sibling's
    /// `cargo build` on the other cores is not a benchmark. An agent's own
    /// commands are its business — it holds the machine and can decide.
    pub fn machine_free_for(&self, agent: u64) -> Result<(), Held> {
        match self.held() {
            Some((holder, command, _)) if holder != agent => Err(Held {
                agent: holder,
                command,
            }),
            _ => Ok(()),
        }
    }

    /// Take the workspace-wide lock for `agent`.
    pub fn take_machine(&self, agent: u64, command: &str) -> Result<(), Held> {
        self.machine_free_for(agent)?;
        let mut inner = self.inner.lock().unwrap();
        inner.holder = Some((agent, command.to_string(), None));
        Ok(())
    }

    /// Release the lock if `agent` holds it *as a tool call*. A release from
    /// anyone else is a no-op, so an agent cannot unlock a sibling by finishing
    /// its own work.
    ///
    /// A holder that is a detached job is deliberately left alone. Every
    /// foreground call ends by releasing, and a call that outlived
    /// `CMD_DETACH_AFTER` has just handed its claim to the job it became (see
    /// [`Registry::launch`]): clearing it would let a sibling start while the
    /// benchmark the lock exists for still runs, contradicting §5.6 — "a
    /// detached exclusive job holds the lock for its whole life". The job's own
    /// end gives the machine back, in [`Registry::finish`].
    pub fn release_machine(&self, agent: u64) {
        let mut inner = self.inner.lock().unwrap();
        if matches!(&inner.holder, Some((holder, _, None)) if *holder == agent) {
            inner.holder = None;
        }
    }

    /// Who holds the machine, if anyone: the agent, its command, and the job
    /// holding it when the holder is a detached job rather than a live tool
    /// call.
    pub fn held(&self) -> Option<(u64, String, Option<u64>)> {
        let inner = self.inner.lock().unwrap();
        inner.holder.clone()
    }

    /// Start watching a command that is already running, and give it an id.
    ///
    /// This is the one door into the registry, so admission — the lock and the
    /// budget — is decided here under one lock: two agents cannot both be told
    /// there is room, and a job that cannot be watched is never registered.
    ///
    /// The job arrives already running, so a refusal has to kill it: dropping
    /// the handle would leave the process group alive, unregistered and out of
    /// `kill_all`'s reach. The kill happens after the registry lock is dropped,
    /// never under it (see the module docs on lock order).
    pub fn launch(self: &Arc<Self>, launch: Launch) -> Result<u64, Refused> {
        let Launch {
            owner,
            command,
            exclusive,
            job,
            mailbox,
        } = launch;
        let live = Live {
            job: Arc::new(Mutex::new(job)),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let admitted = {
            let mut inner = self.inner.lock().unwrap();
            let refusal = if exclusive {
                match &inner.holder {
                    Some((holder, held, _)) if *holder != owner => Some(Refused::Machine(Held {
                        agent: *holder,
                        command: held.clone(),
                    })),
                    _ => None,
                }
            } else {
                None
            }
            .or_else(|| {
                (inner
                    .jobs
                    .values()
                    .filter(|record| record.running())
                    .count()
                    >= MAX_JOBS)
                    .then_some(Refused::Budget)
            });
            match refusal {
                Some(refusal) => Err(refusal),
                None => {
                    let id = self.ids.fetch_add(1, Ordering::SeqCst);
                    if exclusive {
                        inner.holder = Some((owner, command.clone(), Some(id)));
                    }
                    inner.jobs.insert(
                        id,
                        Record {
                            id,
                            owner,
                            command: command.clone(),
                            started: self.clock.now(),
                            live: Some(live.clone()),
                            line: None,
                            tail: String::new(),
                        },
                    );
                    Ok(id)
                }
            }
        };
        let id = match admitted {
            Ok(id) => id,
            // Refuse before you own it, or kill what you refuse: this command
            // was started before admission was asked for (the budget can be
            // checked up to `CMD_DETACH_AFTER` after the process exists, in the
            // auto-detach path), so it is ours to end.
            Err(refusal) => {
                live.kill();
                return Err(refusal);
            }
        };
        let registry = Arc::clone(self);
        let watching = live.clone();
        let started = self.clock.now();
        let spawned = thread::Builder::new()
            .name(format!("mush-job-{id}"))
            .spawn(move || watch(registry, watching, id, owner, command, started, mailbox));
        if let Err(error) = spawned {
            // The job never ran: kill it, forget it, and hand back the lock it
            // claimed — which `release_machine` will not do, because a job's
            // claim is a job's to give up.
            live.kill();
            let mut inner = self.inner.lock().unwrap();
            inner.jobs.remove(&id);
            if matches!(&inner.holder, Some((holder, _, claimed)) if *holder == owner && *claimed == Some(id))
            {
                inner.holder = None;
            }
            return Err(Refused::Thread(error.to_string()));
        }
        Ok(id)
    }

    /// Ask one job to stop, and report the line its owner reads. A job that has
    /// already ended is not an error: it is an answer.
    pub fn stop(&self, owner: u64, id: u64) -> Result<String, String> {
        let record = self.jobs().into_iter().find(|record| record.id == id);
        match record {
            None => Err(format!(
                "no such job {} — command_status lists yours",
                label(id)
            )),
            Some(record) if record.owner != owner => Err(format!(
                "job {} belongs to agent #{}",
                label(id),
                record.owner
            )),
            Some(record) => match record.live {
                Some(live) => {
                    live.kill();
                    Ok(format!("stopping job {}", label(id)))
                }
                None => Ok(record
                    .line
                    .unwrap_or_else(|| format!("{} already ended", label(id)))),
            },
        }
    }

    /// Stop every job one agent started. A `Stop` aimed at an agent means "stop
    /// the work in flight", and a job is work in flight.
    pub fn kill_owned(&self, owner: u64) {
        for record in self.jobs() {
            if record.owner == owner {
                if let Some(live) = record.live {
                    live.kill();
                }
            }
        }
    }

    /// Stop everything. This is what quitting mush runs, where a build an agent
    /// started used to outlive a clean quit.
    pub fn kill_all(&self) {
        for record in self.jobs() {
            if let Some(live) = record.live {
                live.kill();
            }
        }
    }

    /// The jobs `owner` should know about: what is running, and what recently
    /// ended. One line each with the window under it — read live from the
    /// command while it runs, so `command_status` is never a stale copy.
    pub fn status_for(&self, owner: u64) -> String {
        let now = self.clock.now();
        let held = self.held();
        let mine: Vec<Record> = self
            .jobs()
            .into_iter()
            .filter(|record| record.owner == owner)
            .collect();
        if mine.is_empty() {
            return "no jobs".to_string();
        }
        let mut lines = Vec::new();
        for record in mine {
            let tail = match &record.live {
                Some(live) => live.tail(),
                None => record.tail.clone(),
            };
            let head = match (&record.live, &record.line) {
                (Some(_), _) => {
                    let holds = matches!(&held, Some((_, _, Some(job))) if *job == record.id);
                    format!(
                        "{} running {}{} · {}",
                        label(record.id),
                        short_age(now.saturating_duration_since(record.started)),
                        if holds { " · holds the machine" } else { "" },
                        record.command
                    )
                }
                (None, Some(line)) => line.clone(),
                (None, None) => format!("{} ended · {}", label(record.id), record.command),
            };
            lines.push(head);
            if !tail.is_empty() {
                // Indented, so the window reads as belonging to the line above
                // it and not as another job.
                for line in tail.lines() {
                    lines.push(format!("  {line}"));
                }
            }
        }
        lines.join("\n")
    }

    /// A copy of the records, so nothing is read or killed while the registry's
    /// own lock is held. The watch thread takes the handle lock before this one,
    /// and holding both in the other order would deadlock.
    fn jobs(&self) -> Vec<Record> {
        let inner = self.inner.lock().unwrap();
        inner
            .jobs
            .values()
            .map(|record| Record {
                id: record.id,
                owner: record.owner,
                command: record.command.clone(),
                started: record.started,
                live: record.live.clone(),
                line: record.line.clone(),
                tail: record.tail.clone(),
            })
            .collect()
    }

    /// A job has ended: keep its line and its window, release the machine if it
    /// was the holder, and forget the oldest ended job if there are too many.
    /// Called from the job's own thread, which is also what tells the owner.
    fn finish(&self, id: u64, outcome: &JobOutcome, tail: String) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        let (started, command) = {
            let record = inner.jobs.get(&id)?;
            (record.started, record.command.clone())
        };
        let age = self.clock.now().saturating_duration_since(started);
        let line = outcome.line(id, &command, age, &tail);
        if let Some(record) = inner.jobs.get_mut(&id) {
            record.live = None;
            record.tail = tail;
            record.line = Some(line.clone());
        }
        if matches!(&inner.holder, Some((_, _, Some(job))) if *job == id) {
            inner.holder = None;
        }
        while inner
            .jobs
            .values()
            .filter(|record| !record.running())
            .count()
            > JOB_HISTORY
        {
            let oldest = inner
                .jobs
                .values()
                .find(|record| !record.running())
                .map(|record| record.id);
            match oldest {
                Some(oldest) => {
                    inner.jobs.remove(&oldest);
                }
                None => break,
            }
        }
        Some(line)
    }
}

impl Drop for Registry {
    /// The backstop: whatever path ends the tree, no process group it started
    /// outlives it. `App` calls `kill_all` explicitly on the way out; this
    /// catches the paths that do not (a panic inside an actor, a test).
    fn drop(&mut self) {
        if let Ok(inner) = self.inner.lock() {
            for record in inner.jobs.values() {
                if let Some(live) = &record.live {
                    live.kill();
                }
            }
        }
    }
}

/// A sink that is not there: a registry nobody listens to.
#[cfg(test)]
struct Silent;

#[cfg(test)]
impl Events for Silent {
    fn emit(&self, _id: AgentId, _event: AgentEvent) {}
}

/// One job as the screen asks about it: the count, the command, and how long it
/// has been running. No handle, no file read — a painter can ask for this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobView {
    pub id: u64,
    pub command: String,
    pub exclusive: bool,
    pub age: Duration,
}

/// Watch one detached command until it ends or must be ended, then tell its
/// owner. This is the whole life of a job: no other thread touches it, and the
/// completion is sent exactly once, at the end.
fn watch(
    registry: Arc<Registry>,
    live: Live,
    id: u64,
    owner: u64,
    command: String,
    started: Instant,
    mailbox: Sender<AgentMsg>,
) {
    // A poll that fails means the command cannot be waited for any more. That
    // is a reason to stop it, and the reason belongs in the window its owner
    // reads rather than in a log nobody sees.
    let mut note = String::new();
    let outcome = loop {
        if live.stop.load(Ordering::SeqCst) {
            break JobOutcome::Stopped;
        }
        let (written, ended) = {
            let Ok(mut job) = live.job.lock() else {
                break JobOutcome::Stopped;
            };
            match job.poll() {
                Ok(Some(code)) => (job.written(), Ok(code)),
                Ok(None) => (job.written(), Err(None)),
                Err(error) => (job.written(), Err(Some(error))),
            }
        };
        match ended {
            Ok(code) => {
                // A kill that landed between the poll and the flag is what the
                // flag is for: report it as a stop rather than as the command's
                // own exit code.
                break if live.stop.load(Ordering::SeqCst) {
                    JobOutcome::Stopped
                } else {
                    JobOutcome::Exited(code)
                };
            }
            Err(None) => {}
            Err(Some(error)) => {
                note = format!("[mush: could not wait for the command: {error}]");
                live.kill();
                break JobOutcome::Stopped;
            }
        }
        let waited = registry.clock.now().saturating_duration_since(started);
        let stop = live.stop.load(Ordering::SeqCst);
        match stopping(written, waited, None, stop) {
            Some(Stopped::TooMuchOutput) => {
                live.kill();
                break JobOutcome::TooMuchOutput;
            }
            Some(_) => break JobOutcome::Stopped,
            None => registry.clock.sleep(POLL),
        }
    };
    let mut tail = live.tail();
    if !note.is_empty() {
        tail = if tail.is_empty() {
            note
        } else {
            format!("{tail}\n{note}")
        };
    }
    let line = registry
        .finish(id, &outcome, tail.clone())
        .unwrap_or_else(|| outcome.line(id, &command, Duration::ZERO, &tail));
    registry.events.emit(
        AgentId(owner),
        AgentEvent::JobDone {
            job: id,
            line: line.clone(),
        },
    );
    // Exactly once, and to the owner's own mailbox: a job's completion is
    // `ChildDone`'s twin, so a napping agent wakes to it and an idle one finds
    // it in the transcript.
    let _ = mailbox.send(AgentMsg::CommandDone {
        id,
        line,
        news: outcome.is_news(),
    });
}

/// Render the two streams of a command into one window: stdout, then stderr
/// under a heading, exactly as a foreground result reads — so the same bytes
/// describe a command however it ended.
fn preview(stdout: &str, stderr: &str) -> String {
    let mut out = String::new();
    if !stdout.trim().is_empty() {
        out.push_str(stdout.trim_end());
    }
    if !stderr.trim().is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("--- stderr ---\n");
        out.push_str(stderr.trim_end());
    }
    tail_for_model(&out, JOB_TAIL)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crossbeam_channel::Receiver;

    use super::*;
    use crate::clock::fake::Advanceable;
    use crate::events::fake::Recorder;
    use crate::machine::fake::{Script, Scripted as ScriptedMachine};
    use crate::machine::{Machine, ShellCommand};

    /// A registry over a scripted machine and an advanceable clock, so a job's
    /// whole life is asserted without a subprocess and without waiting.
    fn registry() -> (Arc<Registry>, Arc<Recorder>, Arc<Advanceable>) {
        let clock = Arc::new(Advanceable::new());
        let events = Recorder::new();
        let registry = Registry::new(clock.clone(), events.clone(), Arc::new(AtomicU64::new(1)));
        (registry, events, clock)
    }

    /// Start a scripted command as a job for `owner`, and hand back its id and
    /// the mailbox its completion will arrive in. The script the command
    /// follows is the machine's, written by the test that built it.
    fn launch(
        registry: &Arc<Registry>,
        machine: &Arc<ScriptedMachine>,
        owner: u64,
    ) -> (u64, Receiver<AgentMsg>) {
        let job = machine
            .spawn(&ShellCommand {
                command: "cargo build",
                root: Path::new("/tmp"),
            })
            .unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let id = registry
            .launch(Launch {
                owner,
                command: "cargo build".to_string(),
                exclusive: false,
                job,
                mailbox: tx,
            })
            .unwrap();
        (id, rx)
    }

    /// The three ways mush stops a command, decided in one place so the
    /// foreground watcher and a job's thread cannot disagree.
    #[test]
    fn stopping_names_the_three_ways_a_command_is_killed() {
        let nothing = Duration::from_secs(0);
        let waited = Duration::from_secs(1);
        assert_eq!(
            stopping(0, nothing, None, false),
            None,
            "it is still running"
        );
        assert_eq!(
            stopping(0, waited, Some(nothing), false),
            Some(Stopped::TimedOut)
        );
        assert_eq!(stopping(0, nothing, None, true), Some(Stopped::Cancelled));
        assert_eq!(
            stopping(CMD_OUTPUT_LIMIT + 1, nothing, None, false),
            Some(Stopped::TooMuchOutput)
        );
        // The timeout outranks a cancel, and a cancel outranks the writer: each
        // answer means a different thing to the model, so the order is fixed.
        assert_eq!(
            stopping(CMD_OUTPUT_LIMIT + 1, waited, Some(nothing), true),
            Some(Stopped::TimedOut)
        );
    }

    /// The line every reader of a job sees, in one place: how it ended, how long
    /// it took, what it was, and the end of what it said.
    #[test]
    fn a_completion_line_names_the_status_the_age_the_command_and_the_tail() {
        let tail = "running 12 tests\ntest result: ok. 12 passed";
        let line = JobOutcome::Exited(0).line(2, "cargo test", Duration::from_secs(192), tail);
        assert_eq!(
            line,
            "#c2 done: exit 0 · 3m12s · cargo test — running 12 tests · test result: ok. 12 passed"
        );
        assert!(JobOutcome::Exited(0).is_news(), "a result nobody has read");
        let stopped = JobOutcome::Stopped.line(2, "cargo test", Duration::from_secs(4), "");
        assert_eq!(stopped, "#c2 stopped after 4s · cargo test");
        assert!(!JobOutcome::Stopped.is_news(), "a kill is not a result");
        assert!(JobOutcome::TooMuchOutput
            .line(2, "yes", Duration::from_secs(1), "y")
            .contains("wrote past"));

        // The kept window is a tail, so a long one keeps its *end* and says
        // where it was cut off — the head is what a foreground result keeps.
        let long = format!("start{}{}", "x".repeat(4000), "the end that matters");
        let line = JobOutcome::Exited(0).line(3, "cargo build", Duration::from_secs(1), &long);
        assert!(line.contains("the end that matters"), "{line}");
        assert!(!line.contains("startxxxx"), "the head was dropped: {line}");
    }

    /// A job that ends by itself reports itself to its owner exactly once, with
    /// the exit code, the command and the end of what it wrote.
    #[test]
    fn a_job_reports_its_end_once_to_its_owner() {
        let machine = Arc::new(
            ScriptedMachine::new().runs(Script::exits(3).says("on stdout").complains("on stderr")),
        );
        let (registry, events, _clock) = registry();
        let (id, mailbox) = launch(&registry, &machine, 7);
        assert_eq!(id, 1, "jobs draw from the tree's one id counter");

        let (reported, line, news) = match mailbox.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone { id, line, news }) => (id, line, news),
            Ok(_) => panic!("the completion must be a `CommandDone`"),
            Err(error) => panic!("the owner was never told: {error}"),
        };
        assert_eq!(reported, id);
        assert!(news, "a command that ended is news: {line}");
        assert!(line.contains("exit 3"), "{line}");
        assert!(line.contains("cargo build"), "{line}");
        assert!(line.contains("on stdout"), "{line}");
        assert!(line.ends_with("on stderr"), "{line}");

        // Exactly once: nothing else is on its way.
        assert!(
            mailbox.recv_timeout(Duration::from_millis(20)).is_err(),
            "the completion is delivered once"
        );
        assert_eq!(registry.running(), 0, "and it is no longer a live job");
        assert_eq!(machine.kills(), 0, "nothing had to be killed");
        assert!(
            registry.status_for(7).contains("exit 3"),
            "a finished job stays listed: {}",
            registry.status_for(7)
        );
        // The UI heard too, so the badge on the owner's row goes out.
        assert!(events
            .events_for(AgentId(7))
            .iter()
            .any(|event| matches!(event, AgentEvent::JobDone { .. })));
    }

    /// A job that never ends is stopped by a `Stop` aimed at its owner, and by
    /// nothing else: another agent's Stop must not touch it.
    #[test]
    fn a_stop_ends_the_jobs_of_the_agent_it_was_aimed_at() {
        let machine = Arc::new(
            ScriptedMachine::new()
                .runs(Script::hangs())
                .runs(Script::hangs()),
        );
        let (registry, _events, _clock) = registry();
        let (first, _mine) = launch(&registry, &machine, 7);
        let (_other, other_mailbox) = launch(&registry, &machine, 8);
        assert_eq!(registry.running(), 2);

        registry.kill_owned(8);
        match other_mailbox.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone { id, news, .. }) => {
                assert_ne!(id, first, "agent 8's job, not agent 7's");
                assert!(!news, "a job mush killed is not news to wake anyone for");
            }
            other => panic!("agent 8's job must report its stop: {:?}", other.is_ok()),
        }
        assert_eq!(registry.running(), 1, "agent 7's job is untouched");
        assert!(
            registry.status_for(7).contains("running"),
            "and it is still running: {}",
            registry.status_for(7)
        );

        // `command_control stop` is the same act, with an answer for the model:
        // it names the job, and a job that already ended is an answer rather
        // than an error.
        assert_eq!(registry.stop(7, first).unwrap(), "stopping job #c1");
        assert!(registry.stop(7, first).is_ok(), "stopping twice is fine");
        assert!(
            registry.stop(9, first).is_err(),
            "another agent's job is not"
        );
        assert!(registry.stop(7, 99).is_err(), "and an id nobody ran is not");
        registry.kill_all();
    }

    /// A runaway writer is stopped the same way whichever watcher is on it: the
    /// disk is shared, and a command that writes past the limit is not
    /// communicating.
    #[test]
    fn a_job_that_writes_past_the_limit_is_killed_and_says_so() {
        let machine = Arc::new(
            ScriptedMachine::new().runs(Script::hangs().writes_without_end(CMD_OUTPUT_LIMIT / 2)),
        );
        let (registry, _events, _clock) = registry();
        let (_id, mailbox) = launch(&registry, &machine, 7);
        match mailbox.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone { line, .. }) => {
                assert!(line.contains("wrote past"), "{line}")
            }
            other => panic!("expected a kill: {:?}", other.is_ok()),
        }
        assert_eq!(machine.kills(), 1, "the runaway writer was stopped");
    }

    /// The budget is the machine's, not one agent's: `MAX_JOBS` live jobs are
    /// the limit, and the next launch is refused with something the model can
    /// act on.
    #[test]
    fn the_job_budget_is_machine_wide() {
        // The real clock here: eight spinning scripted jobs on the fake clock
        // would be eight threads with nothing to wait for.
        let mut scripted = ScriptedMachine::new();
        for _ in 0..=MAX_JOBS {
            scripted = scripted.runs(Script::hangs());
        }
        let machine = Arc::new(scripted);
        let registry = Registry::new(
            Arc::new(crate::clock::System),
            Recorder::new(),
            Arc::new(AtomicU64::new(1)),
        );
        for _ in 0..MAX_JOBS {
            launch(&registry, &machine, 7);
        }
        assert!(!registry.has_room(), "the budget is spent");

        let job = machine
            .spawn(&ShellCommand {
                command: "cargo build",
                root: Path::new("/tmp"),
            })
            .unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let refused = registry.launch(Launch {
            owner: 8,
            command: "cargo build".to_string(),
            exclusive: false,
            job,
            mailbox: tx,
        });
        assert_eq!(refused, Err(Refused::Budget));
        assert_eq!(
            machine.kills(),
            1,
            "the command handed to the refused launch is stopped, not orphaned"
        );
        assert_eq!(registry.running(), MAX_JOBS, "and it took no slot");
        assert!(
            Refused::Budget.message(8).contains("command_control"),
            "the refusal tells the model how to make room"
        );
        registry.kill_all();
    }

    /// The same rule for the lock: a launch refused because a sibling holds the
    /// machine must not leave the process it was handed running either.
    /// `launch` owns the job before it decides — the lock and the budget are
    /// checked under one lock, so two agents cannot both be told there is room —
    /// and a refusal that merely dropped it left the command in its own process
    /// group, registered nowhere and out of `kill_all`'s reach.
    #[test]
    fn a_launch_refused_by_the_lock_is_killed_too() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let (registry, _events, _clock) = registry();
        registry.take_machine(7, "cargo bench").unwrap();

        let job = machine
            .spawn(&ShellCommand {
                command: "cargo build",
                root: Path::new("/tmp"),
            })
            .unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded();
        assert_eq!(
            registry.launch(Launch {
                owner: 9,
                command: "cargo build".to_string(),
                exclusive: true,
                job,
                mailbox: tx,
            }),
            Err(Refused::Machine(Held {
                agent: 7,
                command: "cargo bench".to_string(),
            })),
        );
        assert_eq!(
            machine.kills(),
            1,
            "the refused command's process group is killed"
        );
        assert_eq!(registry.running(), 0, "and it is not a job");
    }

    /// The lock lives here, so who holds the machine and what a refused sibling
    /// is told are this module's facts even before `exclusive` is wired to
    /// `run_command`.
    #[test]
    fn the_machine_is_held_by_one_agent_at_a_time() {
        let registry = Registry::bare();
        registry.take_machine(3, "cargo bench").unwrap();
        // The holder may run its own commands; a sibling may not, and is told
        // who to wait for.
        assert!(registry.machine_free_for(3).is_ok());
        let held = registry.machine_free_for(4).unwrap_err();
        assert_eq!(held.agent, 3);
        assert_eq!(
            Refused::Machine(held).message(4),
            "#3 holds the machine; retry when it finishes"
        );
        // Only the holder can release it: a release from anyone else is a no-op
        // rather than a way to unlock a sibling.
        registry.release_machine(4);
        assert!(registry.machine_free_for(4).is_err());
        registry.release_machine(3);
        assert!(registry.machine_free_for(4).is_ok());
    }

    /// The registry is the tree's one counter of live jobs, and the UI reads it
    /// as the row's badge: what it lists is what is running, with the command
    /// and the age.
    #[test]
    fn the_live_jobs_are_what_the_screen_reads() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let (registry, _events, clock) = registry();
        let (id, _mailbox) = launch(&registry, &machine, 7);

        clock.advance(Duration::from_secs(1));
        let jobs = registry.live_for(7);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, id);
        assert_eq!(jobs[0].command, "cargo build");
        // At least: the job's own thread sleeps on the same clock, so how far
        // it has moved is not this test's business.
        assert!(jobs[0].age >= Duration::from_secs(1), "{:?}", jobs[0].age);
        assert!(!jobs[0].exclusive);
        assert!(
            registry.live_for(8).is_empty(),
            "another agent's row is empty"
        );
        registry.kill_all();
    }
}
