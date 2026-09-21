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
//! machine-wide, the lock is machine-wide, and Ctrl-N has to be able to kill
//! everything the old tree left running. Job ids are drawn from the
//! conversation's own job counter ([`crate::ids`]), so `#c2` and agent `#2` are
//! two different things that may share a number — the `c` in the display, and
//! the `JobId` type in the code, are what keep them apart.
//!
//! Three rules live here:
//!
//! 1. **A kept output window is a tail.** §11.9 asked; the answer is the tail,
//!    consistently, in the completion line and in `status` alike. A job
//!    is read when it *ends*, and what ended it is at the bottom of the log:
//!    `test result: FAILED`, `error: could not compile`, the panic. The head is
//!    what a foreground command's result keeps, because the model reads it
//!    while the command still runs. There is no second window and no second
//!    source: both readers go through [`preview`].
//! 2. **A job dies with its owner.** `Stop`, `Shutdown`, Ctrl-N, a cut-off
//!    owner and quitting mush all end up in [`Registry::kill_owned`] or
//!    [`Registry::kill_all`]; [`Registry`]'s `Drop` is the backstop for a path
//!    that forgets, with the reach its own doc states. A build an agent started
//!    must not outlive a clean quit.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use mush_core::text::truncate;
use mush_core::workspace::tail_for_model;

use crate::agent::{AgentEvent, AgentMsg};
use crate::app::short_age;
use crate::clock::Clock;
use crate::events::Events;
use crate::ids::{AgentId, Ids, JobId};
use crate::machine::{End, Job};

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

/// How much of the jobs' own windows one `status` result carries, in total:
/// every job's headline plus this much output, however many jobs there are.
///
/// A status is a list to choose from, not a log to read, so it is bounded on
/// its own terms: `CMD_CAP` bounds what one *command* may write back, and a
/// listing that grew with it would spend a bigger and bigger answer on windows
/// nobody asked to read. Sixteen jobs — `MAX_JOBS` running plus `JOB_HISTORY`
/// finished — at a full `JOB_TAIL` each would be 32 KB in one answer.
///
/// Spent on the windows rather than on the list, so no job is ever dropped from
/// a status for being old.
pub const STATUS_WINDOW: usize = 6_000;

/// How much of a job's command a job's line carries, in columns.
///
/// A `run_command` is uncapped upstream — a 2 KB script is an ordinary call —
/// while `status` is the one tool result bounded on its own terms
/// ([`STATUS_WINDOW`]), and every headline in it carries the command: a headline
/// that spelled one whole would put an unbounded line on top of a bounded
/// window, however many jobs there are. The line lands in the transcript too, so
/// it is cut where it is built, exactly as a refusal sentence cuts the command
/// it names ([`REFUSAL_COMMAND_COLUMNS`]).
const STATUS_COMMAND_COLUMNS: usize = 60;

/// How often a running command is polled. Ten milliseconds is the latency
/// between a `Stop` and a process group dying, and costs nothing while idle.
///
/// One rule with two watchers — a job's own thread here and the foreground
/// watcher in `agent.rs` — so it is crate-visible and both sleep on it.
pub(crate) const POLL: Duration = Duration::from_millis(10);

/// Hard ceiling on what one command may write to its scratch files before mush
/// stops it. The disk is shared by every agent, and a command that gets here is
/// not communicating, it is running away. One number for both watchers — the
/// foreground one in `agent.rs` and a job's own thread here — so they cannot
/// drift.
pub const CMD_OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;

/// How long a command may run as a tool call before it becomes a job.
pub const CMD_DETACH_AFTER: Duration = Duration::from_secs(60);

/// How long a job may live, in wall time, before mush ends it.
///
/// A job is the one thing here that is meant to outlive the run that started
/// it — a server, a watch — so it is also the one thing with no tool call to
/// time it out. [`MAX_JOBS`] caps how many may exist; this caps how long one
/// may hold its slot, its process group and its scratch files. Four hours is
/// past any honest build, test or benchmark and still finite, and it is
/// hardcoded rather than configurable on purpose: a ceiling a config can raise
/// is not a ceiling on the disk every agent shares.
pub const JOB_MAX_AGE: Duration = Duration::from_secs(4 * 60 * 60);

/// How much of a command goes into a sentence that names it while refusing a
/// call or noting a run beside another's: both arms of [`Refused::Machine`]
/// and `agent.rs`'s `beside_note`. The command can be a paragraph; a refusal is
/// a line the model reads and acts on, and the part it needs is the start
/// (finding H13).
///
/// `status`'s headlines name a command too and have their own bound
/// ([`STATUS_COMMAND_COLUMNS`]): a listing and a refusal cut the same field for
/// different reasons — the headline keeps a `STATUS_WINDOW` bounded, the
/// sentence keeps itself to one line.
pub(crate) const REFUSAL_COMMAND_COLUMNS: usize = 60;

/// `#c2` — a job's name, as the model and the human both read it.
///
/// The `c` that tells a command's id from an agent's lives in [`JobId`]'s
/// `Display`, which every site in this module formats through: `{id}` is the
/// whole spelling. This wrapper survives for the two callers outside this
/// module that read a `JobId` (`app/mod.rs`'s bar note and job lines) and
/// forwards to that `Display`; every other site spells `{id}` itself.
pub fn label(id: JobId) -> String {
    id.to_string()
}

/// What a run is parked on: the one `wait` tool.
///
/// It used to be two tools, `wait_agents` and `wait_commands`, one noun each.
/// `wait` covers both — every child and every job its owner started — so the
/// noun is generic and the row says `waiting on results` rather than guessing
/// which half is slow. It is also what a run in flight is parked on with no
/// model call behind it — [`Phase::waiting`] derives it from the actor's label,
/// so the row, the footer and the transcript foot all read the one answer, and
/// an hourglass is never painted as a spinner (finding U7).
///
/// [`Phase::waiting`]: crate::app::Phase::waiting
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Waited;

impl Waited {
    /// What the wait is waiting for, in a sentence.
    pub fn noun(self) -> &'static str {
        "results"
    }
}

/// Why a command had to be stopped even though it had not ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    TimedOut,
    /// It passed [`JOB_MAX_AGE`]: long enough that it is not building anything
    /// any more, whatever it is doing.
    RanTooLong,
    Cancelled,
    TooMuchOutput,
}

/// Whether a still-running command must now be stopped. `limit` is the
/// foreground waiter's timeout and `ceiling` is [`JOB_MAX_AGE`]: the first is
/// how long a *tool call* may wait, the second is how long a *job* may live. A
/// foreground command passes `None` for the ceiling, because it cannot reach
/// one — past [`CMD_DETACH_AFTER`] it is a job, and the job's own thread watches
/// it from there. One function, so both watchers answer this the same way.
pub fn stopping(
    written: u64,
    waited: Duration,
    limit: Option<Duration>,
    ceiling: Option<Duration>,
    cancel: bool,
) -> Option<Stopped> {
    if limit.is_some_and(|limit| waited > limit) {
        return Some(Stopped::TimedOut);
    }
    if ceiling.is_some_and(|ceiling| waited > ceiling) {
        return Some(Stopped::RanTooLong);
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
    /// It ended by itself, with this exit code.
    Exited(i32),
    /// A signal killed it — the OOM killer's `9`, the `SIGSEGV` of a crashed
    /// binary — with this signal's number. A separate outcome, not an exit code
    /// standing in for one: a signal death has no exit code, and the `-1` that
    /// used to spell it was a number no command returns, telling an OOM kill and
    /// a crash apart from neither (finding B6).
    Signalled(i32),
    /// mush stopped it: a `Stop` aimed at its owner, `control stop`,
    /// Ctrl-N, or quitting.
    Stopped,
    /// It passed the output limit, so mush killed it rather than let it fill the
    /// disk.
    TooMuchOutput,
    /// It passed [`JOB_MAX_AGE`], so mush killed it: four hours of wall clock is
    /// past any honest build, and its slot and its scratch belong to someone
    /// else by then.
    RanTooLong,
    /// It ended in a way the platform's status does not name: neither an exit
    /// code nor a signal. Its own state rather than an exit code standing in
    /// for one: the `-1` this used to be reads as a real code, and a state that
    /// says "unknown" cannot be acted on as one (finding H26). No job mush
    /// starts ends this way — `machine::ended` reads an exit or a death by
    /// signal from the statuses `wait` produces — and a status that names
    /// neither is what this is for.
    Unknown,
}

impl JobOutcome {
    /// Whether this is news worth waking a napping owner for.
    ///
    /// A job that *ended* is a result nobody has read yet, and so is a job mush
    /// killed for a reason the owner must act on: it ran past [`JOB_MAX_AGE`]
    /// or wrote past [`CMD_OUTPUT_LIMIT`], and the line saying so is the thing
    /// that decides what the owner does next. A `Stopped` job is the human's own
    /// doing — a Stop aimed at its owner, Ctrl-N, a quit — and a stop ends the
    /// agent's work by design, so its line waits in the transcript and starts no
    /// run.
    pub fn is_news(&self) -> bool {
        matches!(
            self,
            JobOutcome::Exited(_)
                | JobOutcome::Signalled(_)
                | JobOutcome::TooMuchOutput
                | JobOutcome::RanTooLong
                | JobOutcome::Unknown
        )
    }

    /// The one line a job is reported in: `#c2 done: exit 0 · 3m12s · cargo
    /// test — test result: ok.`. Kept here so the transcript line, the bar and
    /// `status` say the same thing about the same job.
    ///
    /// The command is cut to [`STATUS_COMMAND_COLUMNS`] here, where the line is
    /// built: `status` prints this line for a job that has ended, and the
    /// command it names is uncapped upstream.
    pub fn line(&self, id: JobId, command: &str, age: Duration, tail: &str) -> String {
        let command = truncate(command, STATUS_COMMAND_COLUMNS);
        let head = match self {
            JobOutcome::Exited(code) => format!("{id} done: exit {code} · {}", short_age(age)),
            // The signal by its number is the one name for it every human reads
            // the same way (`9` under an OOM killer, `11` for a `SIGSEGV`); the
            // colon-and-name spellings are per-platform and per-shell.
            JobOutcome::Signalled(signal) => {
                format!("{id} killed by signal {signal} · {}", short_age(age))
            }
            JobOutcome::Stopped => format!("{id} stopped after {}", short_age(age)),
            // The state names what is unknown rather than inventing a code:
            // `exit -1` was a real-looking number that no command returns and
            // that a reader could act on as if it were one (finding H26).
            JobOutcome::Unknown => format!(
                "{id} ended without an exit code or a signal · {}",
                short_age(age)
            ),
            JobOutcome::TooMuchOutput => format!(
                "{id} killed: it wrote past {CMD_OUTPUT_LIMIT} bytes · {}",
                short_age(age)
            ),
            JobOutcome::RanTooLong => format!(
                "{id} killed: it ran past the {}h ceiling · {}",
                JOB_MAX_AGE.as_secs() / 3600,
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

/// The machine's own distinction, kept all the way to the line a human reads:
/// a command that ended by itself did so with a code or with a signal, and the
/// two are not interchangeable (finding B6).
impl From<End> for JobOutcome {
    fn from(end: End) -> Self {
        match end {
            End::Exited(code) => JobOutcome::Exited(code),
            End::Signalled(signal) => JobOutcome::Signalled(signal),
            // The end of the `-1`: a status that named neither is carried as
            // neither, so no reader can mistake it for an exit code.
            End::Unknown => JobOutcome::Unknown,
        }
    }
}

/// What a job's kept window looks like inside one line: the very end of it, on
/// one line, with an ellipsis where the rest was. `·` joins its lines because
/// this is a summary of the end; the window itself is in `status`.
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
#[derive(Clone)]
struct Record {
    id: JobId,
    owner: u64,
    command: String,
    started: Instant,
    /// What it is: running, or what it left behind. One field, because it is one
    /// fact — a record that has ended has the line its owner reads and the window
    /// it kept, and one that is running has neither yet. As three fields
    /// (`live`, `line`, `tail`) kept in step by `finish` alone, that left a
    /// fourth combination nothing could build and two readers worded to defend.
    state: State,
}

/// The two states a record has, as the two things it can be.
#[derive(Clone)]
enum State {
    /// Still running: reached through this handle, which is how `status` reads
    /// a window that is being written now.
    Running(Live),
    /// Ended: the line its owner reads, and the end of what it wrote.
    Ended { line: String, tail: String },
}

impl Record {
    fn running(&self) -> bool {
        matches!(self.state, State::Running(_))
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
    /// A grip on a command that has just been handed over: nothing has asked it
    /// to stop yet. One constructor, so the two places that take a process group
    /// into the registry build the same handle (refactor R20).
    fn new(job: Box<dyn Job>) -> Self {
        Self {
            job: Arc::new(Mutex::new(job)),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Stop it and everything it started, now. Killing is idempotent and goes
    /// through the handle rather than the flag: on Ctrl-N and on quit the
    /// process groups must be gone before this returns, not ten milliseconds
    /// later.
    fn kill(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut job) = self.job.lock() {
            job.kill();
        }
    }

    /// Whether mush stopped this command from outside its own watcher — a quit
    /// (`kill_all`), a Ctrl-N, or a `Stop` aimed at the agent that started it.
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// The three questions a foreground watcher asks its command, asked through
    /// the registry's grip on it. A poison outside this module must not take
    /// the waiter down with it: the command cannot be waited for any more, and
    /// that is what the answer says.
    fn poll(&self) -> Result<Option<End>, String> {
        match self.job.lock() {
            Ok(mut job) => job.poll(),
            Err(_) => Err("the command's handle was poisoned".to_string()),
        }
    }

    fn written(&self) -> u64 {
        self.job.lock().map(|job| job.written()).unwrap_or(0)
    }

    fn output(&self, cap: usize) -> (String, String) {
        self.job
            .lock()
            .map(|job| job.output(cap))
            .unwrap_or_default()
    }

    /// The end of what it has written so far, at most `cap` bytes of it: the
    /// window a completion keeps is `JOB_TAIL`, and a `status` that
    /// lists several jobs reads a smaller one for each (see `status_for`).
    fn tail(&self, cap: usize) -> String {
        let Ok(job) = self.job.lock() else {
            return String::new();
        };
        let (stdout, stderr) = job.tail(cap);
        preview(&stdout, &stderr, cap)
    }
}

/// A command running as a *tool call* — `run_command` without `detach`, which
/// the agent waits on — held where everything that stops mush's work can reach
/// it.
///
/// It used to live only on the actor's stack: spawned for the duration of the
/// call, killed by the watcher when the call ended, and invisible to
/// [`Registry::kill_all`], which is what `App::drop` runs on the way out. So a
/// plain `sleep 10; touch marker` survived a clean `Ctrl-Q` in its own process
/// group and did its work after mush was gone — while a *detached* job, the
/// same process group by another name, died correctly (finding S4). This is the
/// missing half: the registry holds the command for the life of the call, so
/// `kill_all` on quit and `kill_owned` on `Stop`/Ctrl-N reach it exactly as they
/// reach a job.
///
/// It is not a job. It has no id, no line, no output window and no place in the
/// machine-wide budget: the model is the one waiting for its result, and the
/// transcript is where that result is read. What it has is an entry in the
/// registry's foreground map, which is what a kill can find.
pub struct Foreground {
    registry: Arc<Registry>,
    /// The agent whose command this is — the key the registry's `foregrounds`
    /// map records it under. It is carried here so [`Launch::held`] reads the
    /// owner off the handle instead of being told it a second time: two
    /// statements of one owner is how a job comes to be recorded against an
    /// agent that did not start it (refactor R20).
    owner: u64,
    live: Live,
}

impl Foreground {
    /// Whether mush stopped this command from outside its own watcher — a quit,
    /// a Ctrl-N, or a `Stop` aimed at the agent that started it. The answer the
    /// watcher gives the model must not read as the command's own exit code.
    pub fn stopped(&self) -> bool {
        self.live.stopped()
    }
}

impl Drop for Foreground {
    /// The call is over: its entry in the foreground map goes.
    ///
    /// Nothing is killed here, on purpose. Every path that ends the call early
    /// kills the command itself (`wait_bounded` does, before it returns), and a
    /// command that ended by itself must not be signalled afterwards: its
    /// process group id is free to be handed to somebody else's process, and a
    /// `kill -9 -pgid` that landed there would kill work mush never started.
    /// What keeps a *running* command from escaping is that the entry is
    /// registered for the whole of the call, not this drop.
    fn drop(&mut self) {
        self.registry.forget_foreground(self.owner);
    }
}

impl Job for Foreground {
    fn poll(&mut self) -> Result<Option<End>, String> {
        self.live.poll()
    }

    fn written(&self) -> u64 {
        self.live.written()
    }

    fn output(&self, cap: usize) -> (String, String) {
        self.live.output(cap)
    }

    fn tail(&self, cap: usize) -> (String, String) {
        let Ok(job) = self.live.job.lock() else {
            return (String::new(), String::new());
        };
        job.tail(cap)
    }

    fn kill(&mut self) {
        self.live.kill();
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
    ///
    /// This is the road of a *sibling* refusal: the child whose command queued
    /// for `LOCK_QUEUE` and was refused when the lock outlasted the wait — plus
    /// the one refusal whose asker is the holder: its own second exclusive call,
    /// which `take_machine` refuses under the registry's lock and therefore
    /// never queues. A sibling refusal that never queued at all takes
    /// [`Refused::unqueued_message`], which owns that road's words.
    pub fn message(&self, asker: u64) -> String {
        match self {
            // The asker already owns the machine: a second exclusive command
            // would interleave exactly what the lock exists to keep apart. No
            // queue is involved — the lock is the asker's own — so the sentence
            // says what it may do about it instead.
            Refused::Machine(held) if held.agent == asker => format!(
                "you hold the machine with an exclusive command ({}); wait for it (wait) or stop \
                 it (control stop) before starting another",
                truncate(&held.command, REFUSAL_COMMAND_COLUMNS)
            ),
            // A sibling's lock. The one thing it must not read as is "try again
            // now": retrying the identical call is what mush's own loop guard
            // counts, and it killed two agents that only met a locked machine
            // (finding H13). Who holds it, what they are running, and the one
            // call that spans the hold: `wait` blocks while another agent holds
            // the machine (`agent::wait_tool`), so this refusal has a road back
            // that is not a retry loop — the wait the first H13 fix could not
            // offer, because there was none. The asker here is never the root
            // (`root_message` takes that road), so the blocking wait is its own:
            // the order the wait answers in is the one thing this sentence must
            // not get wrong, since a wait with an unread result to hand over
            // comes back with the lock still held (`machine_held`) rather than
            // waiting it out — and "make this call once more" would then be a
            // second refusal.
            Refused::Machine(held) => format!(
                "#{} holds the machine with an exclusive command ({}); this call queued and the lock \
                 was still held. {}",
                held.agent,
                truncate(&held.command, REFUSAL_COMMAND_COLUMNS),
                Self::lock_road()
            ),
            // The budget is machine-wide, so the two moves are not always the
            // asker's to make: a sibling's jobs are neither its to stop
            // (`Registry::stop` refuses another agent's job) nor its to wait for
            // (`wait` covers what the agent owns). Saying so is the difference
            // between an instruction and a wild goose chase.
            Refused::Budget => format!(
                "cannot detach: {MAX_JOBS} commands are already running as jobs (the limit is \
                 machine-wide). Stop one of your own with control, or wait for one of your own with \
                 wait; a sibling's job is neither — work without it until a slot frees"
            ),
            Refused::Thread(error) => format!("could not start the job: {error}"),
        }
    }

    /// What the **root** is told when its *exclusive* call meets a sibling's
    /// lock.
    ///
    /// The root is the human's own hands and is exempt from the lock: it
    /// commands beside a held one and is told so (`agent.rs`'s `beside_note`),
    /// but two claims to own the machine is the one thing the lock forbids. Its
    /// refusal is immediate — `LOCK_QUEUE` is a *sibling's* road, and the root is
    /// never sent down the queue — so the sentence must not claim the call
    /// queued. The holder's own refusal (an asker that already holds the lock)
    /// stays the holder's sentence whatever road it took.
    pub fn root_message(&self, asker: u64) -> String {
        match self {
            Refused::Machine(held) if held.agent != asker => format!(
                "#{} holds the machine with an exclusive command ({}); you are the root — your \
                 exclusive call was refused at once, without queueing, and your other commands run \
                 beside it. Do other work and make this claim once more after it ends; do not retry \
                 it in a loop",
                held.agent,
                truncate(&held.command, REFUSAL_COMMAND_COLUMNS)
            ),
            other => other.message(asker),
        }
    }

    /// What a *sibling* is told when its exclusive claim met a lock that was
    /// taken between the lock check and the claim: nobody queued this call, and
    /// the queued road's "this call queued and the lock was still held" would
    /// be a sentence about a wait that never happened (the same falsehood
    /// `root_message` exists to prevent for the root).
    ///
    /// Its road back is the queued road's, because the asker is a subagent and
    /// the holder is another agent — a subagent's `wait` blocks on the machine
    /// however the refusal was reached. That shared half has one home
    /// ([`Refused::lock_road`]); only the clause about the queue differs.
    pub fn unqueued_message(&self, asker: u64) -> String {
        match self {
            Refused::Machine(held) if held.agent != asker => format!(
                "#{} holds the machine with an exclusive command ({}); this call was refused at \
                 once, without queueing — the lock was taken in between. {}",
                held.agent,
                truncate(&held.command, REFUSAL_COMMAND_COLUMNS),
                Self::lock_road()
            ),
            other => other.message(asker),
        }
    }

    /// The road back from a sibling's lock, word for word — the one part the two
    /// sibling sentences share. It is a separate function so a wait's own
    /// ordering cannot be spelled twice and drift.
    fn lock_road() -> &'static str {
        "wait blocks until the machine is free — a result the wait hands over first says the lock \
         is still held, so wait again — and then make this call once more; do not retry it in a \
         loop"
    }
}

/// What to start. Built by the caller (`run_command`), which owns the decision
/// to detach; the registry owns admission and watching.
///
pub struct Launch {
    pub owner: u64,
    pub command: String,
    /// Whether this command owns the machine for its whole life.
    pub exclusive: bool,
    /// A command that has never been held, or one a foreground call was already
    /// holding (finding S4): either way this is the process group the registry
    /// watches from here on. A foreground command is handed over, never
    /// re-spawned and never unheld, so the process group is in the registry's
    /// reach every moment of its life — its entry in the foreground map goes
    /// when [`Registry::launch`] has written the job's record, not on the way
    /// in.
    source: Source,
    /// Where the completion lands: the owner's own mailbox, exactly as a child's
    /// completion does.
    pub mailbox: Sender<AgentMsg>,
}

/// Where a `Launch`'s running process group comes from.
enum Source {
    /// Started by the caller and handed over now — what `detach: true` does.
    Started(Box<dyn Job>),
    /// Held by a `Foreground` the caller has finished with: the command keeps
    /// running, and the hold it carried becomes this job's record.
    Held(Foreground),
}

impl Launch {
    /// A command that has just been started.
    pub fn started(
        owner: u64,
        command: String,
        exclusive: bool,
        mailbox: Sender<AgentMsg>,
        job: Box<dyn Job>,
    ) -> Self {
        Self {
            owner,
            command,
            exclusive,
            source: Source::Started(job),
            mailbox,
        }
    }

    /// A command a tool call was holding and has outlived `CMD_DETACH_AFTER`
    /// for: the same process group, watched from now on as a job. The hold is
    /// released once the job's record exists: there is no moment where it is in
    /// neither the foreground map nor the job list, so a kill that lands in
    /// this window still reaches it.
    ///
    /// The owner is the held command's own ([`Foreground::owner`]), not a
    /// parameter: the registry already recorded who started it, and a second
    /// statement of the same fact is one that can disagree (refactor R20).
    pub fn held(
        command: String,
        exclusive: bool,
        mailbox: Sender<AgentMsg>,
        held: Foreground,
    ) -> Self {
        Self {
            owner: held.owner,
            command,
            exclusive,
            source: Source::Held(held),
            mailbox,
        }
    }
}

impl Source {
    /// The registry's grip on the process group this launch is about, and the
    /// foreground hold that still has it, if any.
    ///
    /// A held command's entry is *not* released here. It goes when the job's
    /// record exists ([`Registry::launch`]), because this runs before the
    /// registry lock is taken: an entry freed on the way in would leave a window
    /// in which the command is in neither map, and a `kill_all` there would
    /// miss the very process group it exists to kill (finding S4).
    fn into_live(self) -> (Live, Option<Foreground>) {
        match self {
            Source::Started(job) => (Live::new(job), None),
            Source::Held(held) => (held.live.clone(), Some(held)),
        }
    }
}

/// Every live job in one conversation, and the machine-wide lock.
pub struct Registry {
    clock: Arc<dyn Clock>,
    events: Arc<dyn Events>,
    /// The conversation's id counters, shared with the agents: the *job* half
    /// is what a launch draws from, so a job and an agent can no longer be
    /// handed the same number by one counter.
    ids: Ids,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// In id order: at most `MAX_JOBS` live plus `JOB_HISTORY` finished, so a
    /// handful of records.
    jobs: BTreeMap<JobId, Record>,
    /// The agent holding the machine, its command, and the job holding it while
    /// that job runs.
    holder: Option<(u64, String, Option<JobId>)>,
    /// The commands running as tool calls, keyed by the agent that started
    /// each: what [`Registry::kill_all`] and [`Registry::kill_owned`] reach
    /// beyond the job list. In agent order, one per agent that is running a
    /// command, so a handful at most.
    foregrounds: BTreeMap<u64, Live>,
}

impl Registry {
    pub fn new(clock: Arc<dyn Clock>, events: Arc<dyn Events>, ids: Ids) -> Arc<Self> {
        Arc::new(Self {
            clock,
            events,
            ids,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// The registry's own bookkeeping.
    ///
    /// A panic elsewhere must not take the reader down with it: `live_for` runs
    /// on every frame (`ui.rs`), so an `unwrap` here would turn one actor's
    /// panic into a dead UI thread. A poisoned lock does not corrupt the
    /// records — the panic was somewhere else — so the value is taken as it is.
    /// The only alternatives are worse than the truth: a panic, or an empty
    /// registry that says nothing is running. (The house shape for a lock whose
    /// failure must not be fatal; see `app::settings`.)
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            Ids::default(),
        )
    }

    /// The jobs still running for `owner`, oldest first: what a row, a footer or
    /// the bar shows. No command handle is touched, so the painter may ask.
    ///
    /// A command running as a *tool call* is deliberately not in here: it has no
    /// id to name and no window to show — the transcript's `⚙` line and the
    /// model's own result are where it is read. It is stopped like a job
    /// (finding S4), but it is not one.
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

    /// Hold a command that is running as a tool call, so that everything which
    /// stops mush's work can reach it (finding S4).
    ///
    /// The handle returned *is* the tool call's grip: while it lives, the
    /// process group is in the registry; when it is dropped, the call is over
    /// and its entry goes. Nothing here is admission — a foreground command is
    /// not a job, does not spend the machine-wide budget, and is not refused —
    /// because the command is already running when this is called.
    ///
    /// The map is keyed by owner because an agent holds at most one command at
    /// a time: a tool batch is a `for` loop over its calls, and `run_shell` in
    /// `agent.rs` has one caller, so the owner is the whole identity and a
    /// release always belongs to the command it finds. Slots — a second
    /// identity beside the key, never reused — existed to police a stale
    /// release, which needs two overlapping commands from one agent.
    pub fn hold(self: &Arc<Self>, owner: u64, job: Box<dyn Job>) -> Foreground {
        let live = Live::new(job);
        self.inner().foregrounds.insert(owner, live.clone());
        Foreground {
            registry: Arc::clone(self),
            owner,
            live,
        }
    }

    /// The command a finished tool call held. Idempotent: the handover releases
    /// it and the handle's own `Drop` releases it again.
    fn forget_foreground(&self, owner: u64) {
        self.inner().foregrounds.remove(&owner);
    }

    /// Every command running as a tool call, as `(owner, live)` pairs — a copy,
    /// so nothing is killed or read while the registry's own lock is held.
    fn foregrounds(&self) -> Vec<(u64, Live)> {
        self.inner()
            .foregrounds
            .iter()
            .map(|(owner, live)| (*owner, live.clone()))
            .collect()
    }

    /// Whether this agent is still holding a command as a tool call. Test-only:
    /// the screen's question about a *job* is `live_for`, and this is the same
    /// question about the call the model is waiting on.
    #[cfg(test)]
    pub fn holding_foreground(&self, owner: u64) -> bool {
        self.inner().foregrounds.contains_key(&owner)
    }

    /// How many jobs are alive right now — the number the budget is about, and
    /// the number the pane title's `N jobs` and a row's `⚙N` both count.
    pub fn running(&self) -> usize {
        self.jobs().iter().filter(|record| record.running()).count()
    }

    /// Whether there is room for one more job, as of right now.
    ///
    /// The question is asked at two different moments, and only the first of
    /// them is before a process exists: `run_command` asks it up front for
    /// `detach: true` — a command that cannot be watched must not be started —
    /// while a *foreground* command asks it only to decide whether a command
    /// that outlives `CMD_DETACH_AFTER` has somewhere to go. On that path the
    /// process is already running, and the budget is really decided when
    /// [`Registry::launch`] re-tests it under the registry's own lock; a
    /// refusal there kills the process it cannot watch, so the model still gets
    /// the refusal and no orphan survives. The check here cannot carry that
    /// weight: another agent's job can take the slot between it and the
    /// deadline.
    pub fn has_room(&self) -> bool {
        self.running() < MAX_JOBS
    }

    /// Whether `agent` may run a command at all. Only an exclusive command takes
    /// the lock, but *every* command respects it: a benchmark with a sibling's
    /// `cargo build` on the other cores is not a benchmark. The holder's own
    /// *non-exclusive* commands are its business — it holds the machine and can
    /// decide — while a second *exclusive* claim is refused by
    /// [`Registry::take_machine`], because two claims to own the machine is the
    /// one thing the lock exists to prevent.
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
    ///
    /// A lock already held is refused whoever holds it — including the asker.
    /// This is the only writer of the holder record, and the record names the
    /// claim in flight (`Some(job)` while an exclusive *job* holds it), so
    /// overwriting it would both lose that name and let the first call's own
    /// release free a machine its job still holds: `exclusive=true` was not
    /// exclusive against its own owner.
    pub fn take_machine(&self, agent: u64, command: &str) -> Result<(), Held> {
        let mut inner = self.inner();
        match &inner.holder {
            Some((holder, held, _)) => Err(Held {
                agent: *holder,
                command: held.clone(),
            }),
            None => {
                inner.holder = Some((agent, command.to_string(), None));
                Ok(())
            }
        }
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
    ///
    /// The claim a call releases is always the one `take_machine` wrote for it
    /// (`claimed: None`): a call that never took the lock — the holder's own
    /// non-exclusive command beside an exclusive job — has nothing here to
    /// release, and a job's claim is not a call's to give up.
    pub fn release_machine(&self, agent: u64) {
        let mut inner = self.inner();
        if matches!(&inner.holder, Some((holder, _, None)) if *holder == agent) {
            inner.holder = None;
        }
    }

    /// Who holds the machine, if anyone: the agent, its command, and the job
    /// holding it when the holder is a detached job rather than a live tool
    /// call.
    pub fn held(&self) -> Option<(u64, String, Option<JobId>)> {
        let inner = self.inner();
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
    pub fn launch(self: &Arc<Self>, launch: Launch) -> Result<JobId, Refused> {
        let Launch {
            owner,
            command,
            exclusive,
            mailbox,
            source,
        } = launch;
        let (live, held) = source.into_live();
        let admitted = {
            let mut inner = self.inner();
            let refusal = if exclusive {
                match &inner.holder {
                    // Somebody else's claim, or a *job* of the owner's already
                    // holding the machine: two exclusive claims may not overlap.
                    // `claimed.is_some()` even for the owner is a second job —
                    // the handover this arm exists for replaces the owner's own
                    // *foreground* claim (`None`), and nothing else.
                    //
                    // The sibling half is a guard, not a road (finding H29):
                    // every exclusive caller reaches `launch` through
                    // `run_command`'s `take_machine`, which refuses any existing
                    // holder — the owner included — so between that claim and
                    // this handover no other agent can hold the machine. It
                    // stays because the alternative, on the day it *is*
                    // reachable, is two agents each believing it owns the
                    // machine.
                    Some((holder, held, claimed)) if *holder != owner || claimed.is_some() => {
                        Some(Refused::Machine(Held {
                            agent: *holder,
                            command: held.clone(),
                        }))
                    }
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
                    let id = self.ids.next_job();
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
                            state: State::Running(live.clone()),
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
        // The job's record exists, so the job list is what names this command
        // from here on: the hold a foreground call was carrying can go. Not
        // before — the window in between is one a `kill_all` falls into.
        drop(held);
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
            let mut inner = self.inner();
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
    pub fn stop(&self, owner: u64, id: JobId) -> Result<String, String> {
        let record = self.jobs().into_iter().find(|record| record.id == id);
        match record {
            None => Err(format!("no such job {id} — status lists yours")),
            Some(record) if record.owner != owner => {
                Err(format!("job {id} belongs to agent #{}", record.owner))
            }
            Some(record) => match &record.state {
                State::Running(live) => {
                    live.kill();
                    Ok(format!("stopping job {id}"))
                }
                // A job that ended has the line its owner reads, and that line
                // *is* the answer; there is no second sentence to invent for a
                // state the record cannot be in.
                State::Ended { line, .. } => Ok(line.clone()),
            },
        }
    }

    /// Stop every job one agent started, and every command it is running as a
    /// tool call. A `Stop` aimed at an agent means "stop the work in flight",
    /// and a `run_command` the agent is waiting on is work in flight.
    pub fn kill_owned(&self, owner: u64) {
        self.kill(Some(owner));
    }

    /// Stop everything. This is what quitting mush runs, where a build an agent
    /// started used to outlive a clean quit — and, until finding S4, an ordinary
    /// foreground command with it: the process group was spawned for the length
    /// of a tool call and registered nowhere, so nothing on the way out could
    /// see it.
    pub fn kill_all(&self) {
        self.kill(None);
    }

    /// Stop what `owner` started — both its jobs and the commands it is running
    /// as tool calls — or, with `None`, everything this registry reaches.
    ///
    /// The reach is two maps, and this is the one walk over them: a killer that
    /// walked only `jobs` would be a second, weaker rule, and the foreground
    /// half is exactly what the weaker rule misses (finding S4).
    fn kill(&self, owner: Option<u64>) {
        // `map_or`, not `is_none_or`: the workspace declares Rust 1.74, where
        // the latter does not exist yet (clippy's msrv lint keeps this honest).
        for (holder, live) in self.foregrounds() {
            if owner.map_or(true, |owner| owner == holder) {
                live.kill();
            }
        }
        for record in self.jobs() {
            if owner.map_or(true, |owner| owner == record.owner) {
                if let State::Running(live) = &record.state {
                    live.kill();
                }
            }
        }
    }

    /// Bounded on its own terms, windows first: they share [`STATUS_WINDOW`], so
    /// a status spends the same budget on one job or on sixteen. Each job keeps
    /// its headline — what the model chooses between — and the command in it is
    /// cut to [`STATUS_COMMAND_COLUMNS`], because a command is uncapped upstream
    /// and a headline is not a place to spend a 2 KB script.
    ///
    /// The jobs `owner` should know about: what is running, and what recently
    /// ended. One line each with the window under it — read live from the
    /// command while it runs, so `status` is never a stale copy.
    ///
    /// `None` is "this owner has no jobs", which used to be the sentinel line
    /// `"no jobs"` that the caller compared with `!=` — a string that meant a
    /// count, and that any job whose own text happened to read `no jobs` would
    /// have collided with. Whether there are jobs is a value now.
    pub fn status_for(&self, owner: u64) -> Option<String> {
        let now = self.clock.now();
        let held = self.held();
        let mine: Vec<Record> = self
            .jobs()
            .into_iter()
            .filter(|record| record.owner == owner)
            .collect();
        if mine.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        // The windows share one budget: every job's headline — what the model
        // actually acts on — always fits, while `MAX_JOBS` running plus
        // `JOB_HISTORY` finished jobs at a full `JOB_TAIL` each would be 32 KB
        // of tool result, past every other cap in the tree. A status is a list
        // to choose from, not a log to read, so a lone job still gets the whole
        // window it always did and sixteen get a slice each.
        let per_job = (STATUS_WINDOW / mine.len()).min(JOB_TAIL);
        for record in mine {
            let (head, tail) = match &record.state {
                State::Running(live) => {
                    let holds = matches!(&held, Some((_, _, Some(job))) if *job == record.id);
                    (
                        format!(
                            "{} running {}{} · {}",
                            record.id,
                            short_age(now.saturating_duration_since(record.started)),
                            if holds { " · holds the machine" } else { "" },
                            truncate(&record.command, STATUS_COMMAND_COLUMNS)
                        ),
                        live.tail(per_job),
                    )
                }
                // The line a job ended with carries its outcome, its age and its
                // cut command, so it is the headline as it stands.
                State::Ended { line, tail } => (line.clone(), tail_for_model(tail, per_job)),
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
        Some(lines.join("\n"))
    }

    /// A copy of the records, so nothing is read or killed while the registry's
    /// own lock is held. The watch thread takes the handle lock before this one,
    /// and holding both in the other order would deadlock.
    fn jobs(&self) -> Vec<Record> {
        self.inner().jobs.values().cloned().collect()
    }

    /// A job has ended: keep the line its owner reads and the window it kept,
    /// release the machine if it was the holder, and forget the oldest ended job
    /// if there are too many.
    ///
    /// Called from the job's own thread, which holds the job's own `Live` and is
    /// the only thing that ends its record — the one caller, so there is no "no
    /// such job" to report and no line to invent for one. The line is built
    /// before this, by the thread that watched the job end.
    fn finish(&self, id: JobId, line: String, tail: String) {
        let mut inner = self.inner();
        // `watch` is the only caller, and only a record that has already ended
        // is ever forgotten — here, just below.
        if let Some(record) = inner.jobs.get_mut(&id) {
            record.state = State::Ended { line, tail };
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
    }
}

impl Drop for Registry {
    /// The backstop for the paths that drop a tree with nothing running: no
    /// process group this registry still reaches outlives it.
    ///
    /// The reach is narrower than "whatever path ends the tree", and the
    /// difference is what made Ctrl-N leak: a *running* job's watch thread
    /// holds its own `Arc<Registry>` ([`Registry::launch`]), so dropping the
    /// tree's handle does not drop the registry — the walk below cannot run
    /// until that job ends, and the job it would have killed is the one keeping
    /// it alive. Every path that ends a tree while jobs may run therefore kills
    /// explicitly: quitting (`App`'s `Drop`), Ctrl-N (`App::new_chat`), and a
    /// cut-off owner whose actor is gone (`App::report_cut_off`). What is left
    /// here is the path that forgets and has no job running — a panic inside an
    /// actor, a test — where there is nothing left to kill.
    ///
    /// It kills through [`Registry::kill`], the same walk `Stop`, Ctrl-N and
    /// quitting take, so the backstop is the rule and not a second copy of it:
    /// walking the job list alone left the commands a tool call is holding —
    /// the ones finding S4 is about — outside a drop that is meant to be
    /// everything it *can* reach.
    fn drop(&mut self) {
        self.kill(None);
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
    pub id: JobId,
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
    id: JobId,
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
                Ok(Some(end)) => (job.written(), Ok(end)),
                Ok(None) => (job.written(), Err(None)),
                Err(error) => (job.written(), Err(Some(error))),
            }
        };
        match ended {
            Ok(end) => {
                // A kill that landed between the poll and the flag is what the
                // flag is for: report it as a stop rather than as the command's
                // own end — its code or the signal mush sent it.
                break if live.stop.load(Ordering::SeqCst) {
                    JobOutcome::Stopped
                } else {
                    JobOutcome::from(end)
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
        match stopping(written, waited, None, Some(JOB_MAX_AGE), stop) {
            Some(Stopped::TooMuchOutput) => {
                live.kill();
                break JobOutcome::TooMuchOutput;
            }
            Some(Stopped::RanTooLong) => {
                live.kill();
                break JobOutcome::RanTooLong;
            }
            Some(_) => break JobOutcome::Stopped,
            None => registry.clock.sleep(POLL),
        }
    };
    let mut tail = live.tail(JOB_TAIL);
    if !note.is_empty() {
        tail = if tail.is_empty() {
            note
        } else {
            format!("{tail}\n{note}")
        };
    }
    // The line is built here, by the thread that watched the job end, from the
    // command and the moment it was handed: nothing has to come back out of the
    // record it ends, so `finish` has no case where there is no record to report
    // — the old fallback for one of those spelled the age as `0s`.
    let age = registry.clock.now().saturating_duration_since(started);
    let line = outcome.line(id, &command, age, &tail);
    registry.finish(id, line.clone(), tail);
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

/// Render the two streams of a command into one window of at most `cap` bytes:
/// stdout, then stderr under a heading, exactly as a foreground result reads —
/// so the same bytes describe a command however it ended and whoever asks.
fn preview(stdout: &str, stderr: &str, cap: usize) -> String {
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
    tail_for_model(&out, cap)
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
    use mush_core::tools::ToolName;

    /// The fixed part of a status headline — the id, the state (or the outcome),
    /// the age and the separators — with room to spare.
    const HEADLINE_FURNITURE: usize = 64;

    /// The most one status headline can be, from the code's own numbers: the cut
    /// command (`STATUS_COMMAND_COLUMNS`), that furniture, and — on a job that
    /// has ended — the one-line tail `preview_tail` keeps (`JOB_LINE_TAIL`).
    fn headline_bound() -> usize {
        STATUS_COMMAND_COLUMNS + HEADLINE_FURNITURE + JOB_LINE_TAIL
    }

    /// A registry over a scripted machine and an advanceable clock, so a job's
    /// whole life is asserted without a subprocess and without waiting.
    fn registry() -> (Arc<Registry>, Arc<Recorder>, Arc<Advanceable>) {
        let clock = Arc::new(Advanceable::new());
        let events = Recorder::new();
        let registry = Registry::new(clock.clone(), events.clone(), Ids::default());
        (registry, events, clock)
    }

    /// Start a scripted command as a job for `owner`, and hand back its id and
    /// the mailbox its completion will arrive in. The script the command
    /// follows is the machine's, written by the test that built it.
    fn launch(
        registry: &Arc<Registry>,
        machine: &Arc<ScriptedMachine>,
        owner: u64,
    ) -> (JobId, Receiver<AgentMsg>) {
        let job = machine
            .spawn(&ShellCommand {
                command: "cargo build",
                root: Path::new("/tmp"),
            })
            .unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let id = registry
            .launch(Launch::started(
                owner,
                "cargo build".to_string(),
                false,
                tx,
                job,
            ))
            .unwrap();
        (id, rx)
    }

    /// The four ways mush stops a command, decided in one place so the
    /// foreground watcher and a job's thread cannot disagree.
    #[test]
    fn stopping_names_the_four_ways_a_command_is_killed() {
        let nothing = Duration::from_secs(0);
        let waited = Duration::from_secs(1);
        assert_eq!(
            stopping(0, nothing, None, None, false),
            None,
            "it is still running"
        );
        assert_eq!(
            stopping(0, waited, Some(nothing), None, false),
            Some(Stopped::TimedOut)
        );
        assert_eq!(
            stopping(0, nothing, None, None, true),
            Some(Stopped::Cancelled)
        );
        assert_eq!(
            stopping(CMD_OUTPUT_LIMIT + 1, nothing, None, None, false),
            Some(Stopped::TooMuchOutput)
        );
        // A job's ceiling is the one limit it has, and it is not a tool call's
        // timeout: exactly at it the command still runs, one moment past it is
        // the ceiling's answer.
        assert_eq!(
            stopping(0, JOB_MAX_AGE, None, Some(JOB_MAX_AGE), false),
            None
        );
        assert_eq!(
            stopping(0, JOB_MAX_AGE + waited, None, Some(JOB_MAX_AGE), false),
            Some(Stopped::RanTooLong)
        );
        // The order is fixed, because each answer means a different thing: the
        // timeout outranks a cancel, a cancel outranks the writer, and the
        // ceiling — the only limit a job has — outranks a cancel aimed at it.
        assert_eq!(
            stopping(CMD_OUTPUT_LIMIT + 1, waited, Some(nothing), None, true),
            Some(Stopped::TimedOut)
        );
        assert_eq!(
            stopping(
                CMD_OUTPUT_LIMIT + 1,
                JOB_MAX_AGE + waited,
                None,
                Some(JOB_MAX_AGE),
                true
            ),
            Some(Stopped::RanTooLong)
        );
    }

    /// The line every reader of a job sees, in one place: how it ended, how long
    /// it took, what it was, and the end of what it said.
    #[test]
    fn a_completion_line_names_the_status_the_age_the_command_and_the_tail() {
        let tail = "running 12 tests\ntest result: ok. 12 passed";
        let line =
            JobOutcome::Exited(0).line(JobId(2), "cargo test", Duration::from_secs(192), tail);
        assert_eq!(
            line,
            "#c2 done: exit 0 · 3m12s · cargo test — running 12 tests · test result: ok. 12 passed"
        );
        assert!(JobOutcome::Exited(0).is_news(), "a result nobody has read");
        // A signal death is its own outcome and its own sentence: `-1` was not
        // an exit code, and it said nothing about what ended the job — an OOM
        // kill and a `SIGSEGV` read the same through it (finding B6).
        let signalled =
            JobOutcome::Signalled(9).line(JobId(2), "cargo test", Duration::from_secs(192), "");
        assert_eq!(signalled, "#c2 killed by signal 9 · 3m12s · cargo test");
        assert!(
            JobOutcome::Signalled(9).is_news(),
            "a result nobody has read, whoever ended it"
        );
        let stopped = JobOutcome::Stopped.line(JobId(2), "cargo test", Duration::from_secs(4), "");
        assert_eq!(stopped, "#c2 stopped after 4s · cargo test");
        assert!(!JobOutcome::Stopped.is_news(), "a kill is not a result");
        assert!(JobOutcome::TooMuchOutput
            .line(JobId(2), "yes", Duration::from_secs(1), "y")
            .contains("wrote past"));
        assert!(
            JobOutcome::TooMuchOutput.is_news(),
            "a job mush killed past the output limit is a line its owner has to act on"
        );
        // The ceiling says what it was, because the one thing its owner needs
        // to know is that the command was still running after four hours.
        let long = JobOutcome::RanTooLong.line(JobId(3), "cargo run", JOB_MAX_AGE, "");
        assert_eq!(
            long,
            "#c3 killed: it ran past the 4h ceiling · 4h00m · cargo run"
        );
        assert!(
            JobOutcome::RanTooLong.is_news(),
            "four hours of silence is a reason to wake the owner, not a line to sit on"
        );
        // The end a platform's status does not name: no code, no signal. Its own
        // outcome and its own sentence, because the `exit -1` it used to be was
        // a number a command could have returned and a reader could act on
        // (finding H26). No job mush starts ends this way — `machine::ended`
        // reads an exit or a death by signal from the statuses `wait` produces
        // — and the line is what a status naming neither would report.
        let unknown =
            JobOutcome::Unknown.line(JobId(2), "cargo test", Duration::from_secs(192), tail);
        assert_eq!(
            unknown,
            "#c2 ended without an exit code or a signal · 3m12s · cargo test — running 12 \
             tests · test result: ok. 12 passed"
        );
        assert!(
            JobOutcome::Unknown.is_news(),
            "a job that ended is a result, however namelessly"
        );
        assert!(
            !unknown.contains("-1"),
            "no sentinel standing in for a code: {unknown}"
        );
        assert!(
            matches!(JobOutcome::from(End::Unknown), JobOutcome::Unknown),
            "the seam carries a status that names neither as neither, not as -1"
        );

        // The kept window is a tail, so a long one keeps its *end* and says
        // where it was cut off — the head is what a foreground result keeps.
        let long = format!("start{}{}", "x".repeat(4000), "the end that matters");
        let line =
            JobOutcome::Exited(0).line(JobId(3), "cargo build", Duration::from_secs(1), &long);
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
        assert_eq!(
            id,
            JobId(1),
            "the first job draws from the job counter, at 1"
        );

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
        let listed = registry
            .status_for(7)
            .expect("the finished job stays listed");
        assert!(
            listed.contains("exit 3"),
            "a finished job stays listed: {listed}"
        );
        // The UI heard too, so the badge on the owner's row goes out.
        assert!(events
            .events_for(AgentId(7))
            .iter()
            .any(|event| matches!(event, AgentEvent::JobDone { .. })));
    }

    /// A job a signal killed — the OOM killer's `9`, the `SIGSEGV` of a crashed
    /// binary — reaches its owner as that signal and not as an exit code: the
    /// machine's distinction is the one the line carries, from the poll that
    /// learned it to the bar and the transcript that paint it (finding B6).
    #[test]
    fn a_job_a_signal_killed_reports_the_signal() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::signalled(9)));
        let (registry, _events, _clock) = registry();
        let (id, mailbox) = launch(&registry, &machine, 7);

        let line = match mailbox.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone {
                id: reported,
                line,
                news,
            }) => {
                assert_eq!(reported, id);
                assert!(news, "a job that ended is news, however it ended");
                line
            }
            Ok(_) => panic!("the completion must be a `CommandDone`"),
            Err(error) => panic!("the owner was never told: {error}"),
        };
        assert!(line.starts_with("#c1 killed by signal 9 · "), "{line}");
        assert!(line.ends_with("cargo build"), "{line}");
        assert_eq!(machine.kills(), 0, "nobody in mush killed it");
        let listed = registry
            .status_for(7)
            .expect("the finished job stays listed");
        assert!(
            listed.contains("killed by signal 9"),
            "and `status` says the same thing: {listed}"
        );
    }

    /// The two id spaces are separate: a launch draws from the *job* counter,
    /// so a job never spends a child's number — and `#1` and `#c1` may name a
    /// child and a job at the same time. One shared counter used to make the
    /// first job take the first child's id.
    #[test]
    fn a_job_launch_leaves_the_agent_counter_untouched() {
        let ids = Ids::default();
        let clock = Arc::new(Advanceable::new());
        let registry = Registry::new(clock, Recorder::new(), ids.clone());
        let machine = Arc::new(ScriptedMachine::new().runs(Script::exits(0)));

        let (job, _mailbox) = launch(&registry, &machine, 7);

        assert_eq!(job, JobId(1), "the first job is #c1");
        assert_eq!(ids.agents_floor(), 1, "the agent counter did not move");
        assert_eq!(
            ids.next_agent(),
            AgentId(1),
            "and the first child is #1, not #2"
        );
        registry.kill_all();
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
        let listed = registry.status_for(7).expect("the running job is listed");
        assert!(
            listed.contains("running"),
            "and it is still running: {listed}"
        );

        // `control stop` is the same act, with an answer for the model:
        // it names the job, and a job that already ended is an answer rather
        // than an error.
        assert_eq!(registry.stop(7, first).unwrap(), "stopping job #c1");
        assert!(registry.stop(7, first).is_ok(), "stopping twice is fine");
        assert!(
            registry.stop(9, first).is_err(),
            "another agent's job is not"
        );
        assert!(
            registry.stop(7, JobId(99)).is_err(),
            "and an id nobody ran is not"
        );
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

    /// The painter path must not panic. `live_for` is called every frame, and a
    /// panic while the registry's lock was held — by a job's thread, by a
    /// reader — used to make the next frame's `unwrap` take the UI thread down
    /// with it.
    ///
    /// A poisoned lock does not corrupt the records, so the reader gets the
    /// truth: what is running, and who holds the machine.
    #[test]
    fn a_poisoned_registry_still_answers_the_painter() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let (registry, _events, _clock) = registry();
        let (id, _mailbox) = launch(&registry, &machine, 7);

        // Another thread panicking with the bookkeeping locked is exactly how a
        // lock gets poisoned.
        let clone = registry.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.inner.lock().unwrap();
            panic!("a thread died with the registry locked");
        })
        .join();
        assert!(registry.inner.is_poisoned(), "the lock is poisoned");

        let live = registry.live_for(7);
        assert_eq!(live.len(), 1, "the painter still knows what runs");
        assert_eq!(live[0].command, "cargo build");
        assert_eq!(registry.running(), 1);
        assert!(registry.has_room());
        assert_eq!(registry.held(), None);
        let status = registry.status_for(7).expect("the job is still listed");
        assert!(status.starts_with("#c1 running "), "{status}");
        assert!(status.ends_with("cargo build"), "{status}");
        // Every writer too: admission, the lock, and a stop are still answered
        // rather than panicking on the way in.
        assert_eq!(
            registry.stop(7, id).unwrap(),
            "stopping job #c1",
            "a stop is still a stop"
        );
        assert!(registry.machine_free_for(9).is_ok());
        registry.kill_all();
    }

    /// `status` is one bounded tool result, however many jobs there
    /// are. It used to carry the full `JOB_TAIL` window of every job: sixteen
    /// jobs — `MAX_JOBS` running plus `JOB_HISTORY` finished — were 32 KB in
    /// one answer, past every cap in the tree.
    ///
    /// Every job keeps its headline, because what the model does with a status
    /// is choose one to wait for or stop, and a job dropped from the list
    /// cannot be chosen. The windows share the rest.
    #[test]
    fn a_status_is_one_bounded_result_however_many_jobs_there_are() {
        let window = "0123456789".repeat(400); // 4000 bytes, past JOB_TAIL
        let mut scripted = ScriptedMachine::new();
        for _ in 0..MAX_JOBS {
            scripted = scripted.runs(Script::exits(0).says("done"));
        }
        for _ in 0..MAX_JOBS {
            scripted = scripted.runs(Script::hangs().says(&window));
        }
        let machine = Arc::new(scripted);
        let (registry, _events, _clock) = registry();

        // Eight jobs that have ended, then eight that are still running: the
        // whole list a status can be asked for.
        let mut ends = Vec::new();
        for _ in 0..MAX_JOBS {
            let (_, mailbox) = launch(&registry, &machine, 7);
            ends.push(mailbox);
        }
        for mailbox in ends {
            assert!(matches!(
                mailbox.recv_timeout(Duration::from_secs(5)),
                Ok(AgentMsg::CommandDone { .. })
            ));
        }
        for _ in 0..MAX_JOBS {
            launch(&registry, &machine, 7);
        }

        let status = registry.status_for(7).expect("the jobs are listed");
        for id in 1..=2 * MAX_JOBS as u64 {
            let name = JobId(id).to_string();
            assert!(
                status.contains(&name),
                "{name} is missing from a {}-byte status",
                status.len()
            );
        }
        // The windows share `STATUS_WINDOW` — sixteen jobs together are bounded
        // by that one budget, not by sixteen `JOB_TAIL`s — and what rides on top
        // of it is one bounded headline per job. This is the bound the test used
        // to miss: it padded with 256 bytes a job, which a command of any size
        // could spend with the assertion none the wiser.
        assert!(
            status.len() <= STATUS_WINDOW + (MAX_JOBS + JOB_HISTORY) * headline_bound(),
            "one status must stay inside the window it spends, plus one bounded headline a job: {} bytes",
            status.len()
        );
        assert!(
            !status.contains(&"0123456789".repeat(100)),
            "the windows were shared, not each shown whole: {} bytes",
            status.len()
        );
        registry.kill_all();

        // A lone job still gets the window it always had — the budget is a
        // ceiling, not a tax on the ordinary case.
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs().says(&window)));
        let lone = Registry::bare();
        launch(&lone, &machine, 7);
        let status = lone.status_for(7).expect("the lone job is listed");
        assert!(
            status.contains(&"0123456789".repeat(100)),
            "a single job's window is the full JOB_TAIL: {} bytes",
            status.len()
        );
        lone.kill_all();
    }

    /// A command is uncapped upstream — a `run_command` may carry a 2 KB script
    /// — and a status headline used to name one whole, on top of
    /// `STATUS_WINDOW`: the bound above was only kept by padding with 256 bytes
    /// a job. The headline cuts the command, like every other line the model
    /// reads that names one.
    #[test]
    fn a_long_command_stays_outside_a_status_headline() {
        let long = format!("cargo bench --profile release -- {}", "x".repeat(2000));
        let machine = Arc::new(
            ScriptedMachine::new()
                .runs(Script::exits(0).says("done"))
                .runs(Script::hangs()),
        );
        let (registry, _events, _clock) = registry();
        let start = |command: &str| {
            let job = machine
                .spawn(&ShellCommand {
                    command,
                    root: Path::new("/tmp"),
                })
                .unwrap();
            let (tx, rx) = crossbeam_channel::unbounded();
            let id = registry
                .launch(Launch::started(7, command.to_string(), false, tx, job))
                .unwrap();
            (id, rx)
        };
        // One that has ended and one still running: both are headlines in the
        // same status, and both used to carry the whole command.
        let (ended, mailbox) = start(&long);
        assert!(matches!(
            mailbox.recv_timeout(Duration::from_secs(5)),
            Ok(AgentMsg::CommandDone { .. })
        ));
        let (running, _mailbox) = start(&long);

        let status = registry.status_for(7).expect("both jobs are listed");
        assert!(status.contains(&running.to_string()), "{status}");
        assert!(status.contains(&ended.to_string()), "{status}");
        assert!(
            !status.contains(&"x".repeat(STATUS_COMMAND_COLUMNS)),
            "the command is cut, not carried: {} bytes",
            status.len()
        );
        // The rule the code keeps, per line: a headline is the fixed furniture
        // (id, state, age, separators) plus the cut command and, on a job that
        // has ended, the one-line tail. A whole command breaks it, which is why
        // this is asserted on the lines and not only on the total.
        for line in status.lines().filter(|line| !line.starts_with("  ")) {
            assert!(
                line.len() <= headline_bound(),
                "a headline stays bounded: {} bytes: {line}",
                line.len()
            );
        }
        registry.kill_all();
    }

    /// A command running as a *tool call* is held where the same kills reach it
    /// (finding S4) — but it is not a job: it has no id in the list, no line,
    /// no window, and no place in the machine-wide budget. Two owners' held
    /// commands, one `Stop` and one quit.
    #[test]
    fn a_held_command_is_killed_by_its_owners_stop_and_by_kill_all() {
        let (registry, _events, _clock) = registry();
        let machine = Arc::new(
            ScriptedMachine::new()
                .runs(Script::hangs())
                .runs(Script::hangs()),
        );
        let hold = |owner: u64| {
            let job = machine
                .spawn(&ShellCommand {
                    command: "cargo build",
                    root: Path::new("/tmp"),
                })
                .unwrap();
            registry.hold(owner, job)
        };
        let mine = hold(7);
        let other = hold(8);
        assert!(registry.holding_foreground(7) && registry.holding_foreground(8));
        assert_eq!(
            registry.live_for(7),
            Vec::new(),
            "a tool call is not a job: it has no id to list and no window to show"
        );
        assert_eq!(registry.running(), 0, "and it spends no job budget");

        // A `Stop` aimed at one agent reaches that agent's command, and only
        // that one.
        registry.kill_owned(7);
        assert_eq!(machine.kills(), 1, "the owner's command was stopped");
        assert!(!other.stopped(), "and a sibling's keeps running");

        // Quitting stops everything, wherever the command was spawned.
        registry.kill_all();
        assert_eq!(machine.kills(), 2, "quitting killed the other one too");
        // The call is over: the entries go, so a later quit cannot find them.
        drop(mine);
        drop(other);
        assert!(!registry.holding_foreground(7) && !registry.holding_foreground(8));
        registry.kill_all();
        assert_eq!(
            machine.kills(),
            2,
            "and a finished call leaves nothing to kill"
        );
    }

    /// A tool call that outlives `CMD_DETACH_AFTER` becomes the job of the agent
    /// that held it: the owner comes off the handle the registry wrote it into,
    /// not off a second parameter, so the record and the hold cannot name two
    /// agents (refactor R20).
    #[test]
    fn a_handed_over_command_belongs_to_the_agent_that_held_it() {
        // The real clock here: a scripted command that never ends is a thread
        // with nothing to wait for on the fake one.
        let registry = Registry::new(
            Arc::new(crate::clock::System),
            Recorder::new(),
            Ids::default(),
        );
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let held = registry.hold(
            7,
            machine
                .spawn(&ShellCommand {
                    command: "cargo build",
                    root: Path::new("/tmp"),
                })
                .unwrap(),
        );
        let (tx, _rx) = crossbeam_channel::unbounded();

        let id = registry
            .launch(Launch::held("cargo build".to_string(), false, tx, held))
            .unwrap();

        let mine = registry.live_for(7);
        assert_eq!(mine.len(), 1, "the holder owns the job it handed over");
        assert_eq!(mine[0].id, id);
        assert!(registry.live_for(8).is_empty(), "and nobody else does");
        assert!(
            !registry.holding_foreground(7),
            "the job's record names it from here on, so the hold is gone"
        );
        registry.kill_all();
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
            Ids::default(),
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
        let refused = registry.launch(Launch::started(
            8,
            "cargo build".to_string(),
            false,
            tx,
            job,
        ));
        assert_eq!(refused, Err(Refused::Budget));
        assert_eq!(
            machine.kills(),
            1,
            "the command handed to the refused launch is stopped, not orphaned"
        );
        assert_eq!(registry.running(), MAX_JOBS, "and it took no slot");
        assert!(
            Refused::Budget
                .message(8)
                .contains(ToolName::Control.as_str()),
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
            registry.launch(Launch::started(9, "cargo build".to_string(), true, tx, job,)),
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
        // The words must not read as "try again now": a repeated identical
        // call is what the loop guard counts, and two agents died to a lock
        // refusal counted as a loop (finding H13). They name the holder, what
        // it runs, and the one wait that spans the hold — `wait` blocks while
        // another agent holds the machine — so a refused call has a road back
        // that is not a retry. And the order that wait answers in is said too:
        // a wait with an unread result hands it over and comes back with the
        // lock still held, so the model must wait again rather than read the
        // next refusal as the wait having failed.
        let refusal = Refused::Machine(held).message(4);
        assert!(refusal.starts_with("#3 holds the machine"), "{refusal}");
        assert!(refusal.contains("cargo bench"), "{refusal}");
        assert!(refusal.contains("do not retry"), "{refusal}");
        assert!(
            refusal.contains("wait blocks until the machine is free")
                && refusal.contains("says the lock is still held, so wait again"),
            "{refusal}"
        );
        // Only the holder can release it: a release from anyone else is a no-op
        // rather than a way to unlock a sibling.
        registry.release_machine(4);
        assert!(registry.machine_free_for(4).is_err());
        registry.release_machine(3);
        assert!(registry.machine_free_for(4).is_ok());
    }

    /// `exclusive=true` is not exclusive against its own owner: a second claim
    /// from the holder is refused, and the first claim's record — which names
    /// the job it became — is not overwritten. Overwriting it used to erase the
    /// `Some(job)`, so the first call's own release freed a machine the
    /// benchmark still held, and the sibling this lock refuses ran beside it.
    #[test]
    fn the_holder_cannot_claim_the_machine_itself_twice() {
        let registry = Registry::bare();
        registry.take_machine(3, "cargo bench").unwrap();

        // The holder's own second exclusive claim: refused, in words that name
        // the lock it already owns rather than a queue it never sat in.
        let held = registry
            .take_machine(3, "cargo bench --other")
            .expect_err("the lock is not a re-entrant one");
        assert_eq!(held.agent, 3, "the refusal names the holder: the asker");
        let refusal = Refused::Machine(held).message(3);
        assert!(refusal.starts_with("you hold the machine"), "{refusal}");
        assert!(refusal.contains("cargo bench"), "{refusal}");
        assert!(refusal.contains("control stop"), "{refusal}");
        assert!(
            !refusal.contains("queued"),
            "the holder never queues for its own lock: {refusal}"
        );

        // And the record is exactly the first claim's, not the second's.
        assert_eq!(
            registry.held(),
            Some((3, "cargo bench".to_string(), None)),
            "a claim the holder did not take was not overwritten"
        );
    }

    /// The one admission door holds the same rule as `take_machine`: an
    /// exclusive *job* claim is not replaced by another exclusive job, even for
    /// the same owner. The legitimate handover — a foreground call giving its
    /// own claim to the job it became — replaces a `None` claim and nothing
    /// else.
    #[test]
    fn a_second_exclusive_job_is_refused_even_for_its_owner() {
        let machine = Arc::new(
            ScriptedMachine::new()
                .runs(Script::hangs())
                .runs(Script::hangs()),
        );
        let (registry, _events, _clock) = registry();
        assert!(registry.machine_free_for(7).is_ok());

        // The owner's first exclusive job: the claim now names it.
        let first = machine
            .spawn(&ShellCommand {
                command: "cargo bench",
                root: Path::new("/tmp"),
            })
            .unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let id = registry
            .launch(Launch::started(
                7,
                "cargo bench".to_string(),
                true,
                tx,
                first,
            ))
            .unwrap();
        assert_eq!(
            registry.held(),
            Some((7, "cargo bench".to_string(), Some(id)))
        );

        // A second exclusive job from the same owner is refused, and the
        // command it was handed is killed rather than orphaned.
        let second = machine
            .spawn(&ShellCommand {
                command: "cargo bench --other",
                root: Path::new("/tmp"),
            })
            .unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded();
        assert_eq!(
            registry.launch(Launch::started(
                7,
                "cargo bench --other".to_string(),
                true,
                tx,
                second,
            )),
            Err(Refused::Machine(Held {
                agent: 7,
                command: "cargo bench".to_string(),
            })),
        );
        assert_eq!(machine.kills(), 1, "the refused command's group is killed");
        assert_eq!(
            registry.held(),
            Some((7, "cargo bench".to_string(), Some(id))),
            "the first job still holds the machine, under its own name"
        );
        registry.kill_all();
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

    /// A job may not live forever. Past `JOB_MAX_AGE` the job's own thread ends
    /// it, and the owner is told what happened and why — a ceiling nobody is
    /// told about is a command that vanished.
    #[test]
    fn a_job_that_runs_past_the_ceiling_is_killed_and_its_owner_is_told() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let (registry, _events, clock) = registry();
        let (id, mailbox) = launch(&registry, &machine, 7);

        // The job's thread sleeps on this same clock, so advancing past the
        // ceiling is what lets the poll that sees it happen at all.
        clock.advance(JOB_MAX_AGE + Duration::from_secs(1));
        match mailbox.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone {
                id: done,
                line,
                news,
            }) => {
                assert_eq!(done, id);
                assert!(line.contains("ran past the 4h ceiling"), "{line}");
                assert!(
                    news,
                    "the owner's run must start so it reads why its job died: {line}"
                );
            }
            Ok(_) => panic!("a job's completion is a CommandDone"),
            Err(error) => panic!("no completion arrived: {error}"),
        }
        assert!(
            registry.live_for(7).is_empty(),
            "the slot, the process group and the scratch are given back"
        );
    }
}
