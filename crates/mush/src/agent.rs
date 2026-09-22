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
use mush_core::config::vision_capable;
use mush_core::git;
use mush_core::message::{ChatRequest, ChatResponse};
use mush_core::text::{first_line, sanitize, truncate, truncate_flag};
use mush_core::tools::ToolName;
use mush_core::transcript::{
    needs_compaction, repair_tool_pairs, sanitize_tool_calls, trim_history, trim_target,
    COMPACT_INSTRUCTION, COMPACT_REPLY_TOKENS,
};
use mush_core::workspace::{truncate_for_model, SEARCH_FILE_CAP};
use mush_core::{prompt, tools, Config, Image, Message, Workspace, CMD_TIMEOUT_SECS};

use crate::app::{tokens_label, Compacting, ConfigHandle, ConversationId, Msg, WindowSource};
use crate::clock;
use crate::events::{Events, Ui};
use crate::ids::{AgentId, Ids, JobId};
use crate::jobs::{self, Refused};
use crate::machine::{End, Job, Machine, Shell, ShellCommand};
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
/// a *too big* answer — a big edit in one call, or a long reasoning
/// pass — not a broken model, so the run asks for smaller pieces and carries
/// on. It is bounded because a model that cannot write small enough is not
/// going to start now.
const TRUNCATION_ROUNDS: usize = 3;
/// How deep subagent chains may go (0 = root agent only).
pub const MAX_DEPTH: usize = 3;
/// Hard ceiling on simultaneously running agents across the whole tree.
const MAX_AGENTS: u64 = 16;
/// Default `wait` timeout in seconds; 0 means forever.
const WAIT_TIMEOUT_SECS: u64 = 600;
/// How wide one line of a `status` digest may be, in columns. A listing
/// is for telling children apart and knowing what is unread; the body itself
/// travels through the delivery roads, once (see [`Outcome::digest`]).
const DIGEST_COLUMNS: usize = 100;
/// Why a cancelled run ends. Internal to the actor: a run that ends with this
/// becomes `Outcome::Stopped(..)` at the actor boundary, so no other layer has
/// to compare result text to know what happened.
const CANCELLED: &str = "cancelled";

/// Who or what ended a run that a stop cut short.
///
/// Three roads set the same cancel flag, and the parent reading `#30 stopped:`
/// has to be able to tell them apart, because they mean three different things
/// to it: the human's own key is an intervention it did not ask for, its own
/// `control stop` is a decision it has already made, and mush reclaiming the
/// actor's thread (a park, `Ctrl-N`) is bookkeeping — that run did not *end*,
/// its thread was taken away, and nothing about it is news.
///
/// Before this existed the child could say only "stopped", and the rule was
/// built on that gap: no stop woke a napping parent, on the reasoning that a
/// stop is the human's doing. The human's own words, watching a root miss two
/// children it was depending on: "even if a parent is depending on that child
/// for information — which they usually are — it SHOULD be news". It is now,
/// and the line says which hand did it; a park still is not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    /// The human stopped it: `Ctrl-C` in the tree.
    Human,
    /// The agent's own parent stopped it: `control stop`.
    Parent,
    /// Mush ended the run itself: `App::park_history` reclaimed the thread, or
    /// the whole tree is going away (`Ctrl-N`). Not an ending, and not news.
    Reclaimed,
    /// A stop that came back from a stored session, where who asked is not
    /// recorded — the session keeps how a run *ended*, not the hand that ended
    /// it. Its line names no stopper rather than guessing one.
    Unrecorded,
}

impl Stop {
    /// The words the parent's line carries: what this does to the result it was
    /// waiting for, and whose doing it was. One home, so the sentence a parent
    /// reads and the sentence a listing reports cannot describe one stop two
    /// ways.
    fn words(self) -> &'static str {
        match self {
            Stop::Human => "the human stopped the run before it finished, so no result is coming",
            Stop::Parent => "you stopped the run before it finished, so no result is coming",
            Stop::Reclaimed => {
                "mush reclaimed its thread while the run was in flight — it is parked, not ended"
            }
            Stop::Unrecorded => "the run ended before it finished, so no result is coming",
        }
    }
}

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
    /// The run was stopped, by the hand [`Stop`] names. Not a result and not a
    /// failure: the actor is idle and resumable, and whether a napping parent is
    /// woken for it depends on who asked — a park is not news, the human's own
    /// key is (`is_news`).
    Stopped(Stop),
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

/// How a run left its worktree, when it had one: the fact a parent deciding
/// whether to merge is missing, and the one `.mush/session.json` could never
/// carry (finding H1).
///
/// It travels beside the run's outcome on [`AgentMsg::Work`], never inside it:
/// an outcome is delivered exactly once and marks a read, while this is a
/// *listing* fact a parent may re-read as often as it likes without re-arming
/// anything.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Work {
    /// The run committed its work on `branch`.
    Committed { branch: String, revision: String },
    /// The run changed nothing: the branch stands clean where it was.
    Clean { branch: String },
    /// The commit itself failed; the worktree may be dirty and unlanded.
    Uncommitted { branch: String, error: String },
}

impl Work {
    /// The line the UI prints when this happened. One home for the sentence,
    /// so the transcript's status line and the listing agree about the same
    /// commit.
    fn status_line(&self) -> Option<String> {
        match self {
            Work::Committed { branch, revision } => {
                Some(format!("committed {revision} on {branch}"))
            }
            Work::Clean { .. } => None,
            Work::Uncommitted { error, .. } => {
                Some(format!("could not commit the worktree: {error}"))
            }
        }
    }

    /// A bounded suffix for `status`: where the work is and whether it
    /// is committed. Never the body of anything, and never a read.
    fn digest(&self) -> String {
        match self {
            Work::Committed { branch, revision } => format!(" · committed {revision} on {branch}"),
            Work::Clean { branch } => format!(" · {branch} clean — nothing changed"),
            Work::Uncommitted { branch, error } => {
                format!(" · {branch} uncommitted ({})", truncate(error, 60))
            }
        }
    }
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
            Outcome::Stopped(_) => Committed::Stopped,
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
/// the task ("create a file called iso.txt…" — the reasons live below), and the
/// line is [`mush_core::text::first_line`]'s: one collapse of whitespace for the
/// subject, an agent's title, a job's handle and a tool label alike (refactor
/// R11). The word boundary because [`truncate`] alone ends a subject mid-word
/// (`isolated w…`), which neither reads as English nor matches the brief; the
/// whole word that does not fit is dropped and the `…` says so. A first line
/// with no space to cut on keeps the hard cut — a clipped subject is better
/// than no subject. Whether the line was cut at all is
/// [`truncate_flag`]'s answer, never the `…`'s: a brief that ends in an ellipsis
/// of its own would otherwise lose the word in front of it.
fn subject_brief(brief: &str) -> String {
    let first = first_line(brief);
    let (cut, cut_short) = truncate_flag(&first, SUBJECT_COLUMNS);
    if !cut_short {
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
            Outcome::Stopped(whose) => format!(
                "#{id} stopped: {} — this agent is idle, not done; `control message` resumes it",
                whose.words()
            ),
            Outcome::CutOff => format!(
                "#{id} cut off: the run never ended — nothing was committed; \
                 its work is where it left it"
            ),
            Outcome::Failed(error) => format!("#{id} failed: {error}"),
        }
    }

    /// Whether this is news worth waking a napping parent for.
    ///
    /// Every ending is. A finished run produced a result the parent is waiting
    /// for; a failed or cut-off one means the result is not coming, and the work
    /// may be sitting uncommitted; and a *stop* is that same shape — the parent
    /// delegated a task and the task will not answer. This used to be false for
    /// a stop, on the reasoning that stopping is the human's doing and the line
    /// would keep until the parent ran again. The human's ruling is the other
    /// one: a parent depending on that child has to be told, whichever hand
    /// ended it — and the line names the hand ([`Stop::words`]).
    ///
    /// The one stop that is not news is the one no hand asked for. A park is
    /// mush's own bookkeeping: `App::park_history` reclaims a thread the window
    /// is not using, the agent is at rest and resumable, and the run it was
    /// holding was not lost — waking a parent for that would be waking it for
    /// memory management.
    fn is_news(&self) -> bool {
        !matches!(self, Outcome::Stopped(Stop::Reclaimed))
    }

    /// A bounded rendering of this outcome for a *listing* (`status`):
    /// the first line, cut at [`DIGEST_COLUMNS`], plus the size of the whole.
    ///
    /// Never the body. A listing is not a delivery: the body is handed to the
    /// model exactly once, by the roads that ask [`ActorState::record_child`]
    /// and mark it read. `agent_status` used to print every child's entire
    /// final message on every call, so a parent that polled it re-read every
    /// child's report again and again (the replay this digest closes).
    ///
    /// This is also the *one* sentence per ending: a listing and a wait both
    /// read it and only add their own marks around it, because two surfaces
    /// that each spell an ending are two chances to disagree about what a stop
    /// means.
    fn digest(&self, id: u64) -> String {
        let (mark, body) = match self {
            Outcome::Finished(summary) => ("✓", summary.as_str()),
            Outcome::Failed(error) => ("✗", error.as_str()),
            Outcome::Stopped(_) => {
                return format!(
                    "#{id} ⊘ stopped — idle and resumable (control message resumes it)"
                );
            }
            Outcome::CutOff => {
                return format!("#{id} ⚠ cut off — the run never ended; nothing was committed");
            }
        };
        let first = body.lines().next().unwrap_or("").trim();
        let (cut, cut_short) = truncate_flag(first, DIGEST_COLUMNS);
        // The size is named whenever the digest is not the whole body: the first
        // line was cut to fit, or the body runs on past it. The flag is what
        // says the first, because counting the characters cannot — one dropped
        // wide glyph costs two columns and no characters, so the counts tie and
        // the digest used to hide a cut without saying so.
        if cut_short || body.chars().count() > first.chars().count() {
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

/// The run number the books use for a result no actor in this process can
/// report: `ActorState::runs` starts at 0 and is incremented where a run ends,
/// so the first report any actor makes is 1 and 0 is nobody's run. Identity,
/// not arithmetic, exactly as [`CUT_OFF_RUN`] is at the other end of the
/// range: the books of a parked child are moved to it (`note_parked`), and a
/// row seeded from the tree (`AgentMsg::ChildBook`) is recorded under it — in
/// both cases the report is real, and the number must be one no actor will
/// ever claim, so the next run the child takes is news rather than a replay.
const NO_RUN: u64 = 0;

/// Commands sent into an agent actor's mailbox.
///
/// `Clone` and `Debug` because one travels *back* out of an actor: a parent
/// whose send finds no actor behind its child's mailbox hands the command to
/// the UI in an event ([`AgentEvent::ChildAsleep`]), and an event is a value —
/// `crate::events::fake`'s recording sink hands out a copy of everything it saw.
#[derive(Clone, Debug)]
pub enum AgentMsg {
    /// Adopt these messages and run. The actor keeps the transcript, so later
    /// nudges continue the same conversation.
    Run(Vec<Message>),
    /// Keep these messages as this agent's conversation, and do **not** run.
    ///
    /// The one caller is the restore door (`App::restore_agents`): the root's
    /// actor is started with an empty transcript, because its history lives in
    /// the UI and travels with the first [`Run`](Self::Run) — and a child's
    /// completion that reaches it before the human has said anything would fold
    /// into a transcript with no system message and no history, i.e. a run whose
    /// whole request is a bare `#2 done: …` line (§8.39). This is the repair
    /// every hand-over goes through ([`adopted`]) and not a request: a restart is
    /// not a run ([`revive`]), and the next `Run` still wins — its arm replaces
    /// the transcript with the UI's newer copy.
    Adopt(Vec<Message>),
    /// Append a user message the human typed; if idle, run again. The UI echoed
    /// these words before sending them, so the actor folds them in without
    /// telling it to add them again.
    ///
    /// A whole [`Message`] and not a string, because a human steering an agent
    /// with a screenshot is a thing that has to work: a nudge *is* a user
    /// message, and one with images rides in the same content array
    /// (`Message::user_with_images`) whether the model reads it in a run or
    /// mid-run.
    Nudge(Message),
    /// A steering message from another agent — what `control message`
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
    /// that has none yet: a root actor is built with no transcript at all — the
    /// history lives in the UI, and reaches the actor with the first `Run` (or
    /// with `Adopt`, at a restore) — and a fold is not a run, so nothing else
    /// would ever hand it over. Without this the request folded nothing at all,
    /// silently.
    Compact(Vec<Message>),
    /// Cancel the current run. An idle agent ignores it — Stop cancels work,
    /// it does not end an agent. It carries *who* asked, because the child's
    /// `#N stopped:` line is read by its parent, and "the human stopped it",
    /// "you stopped it" and "mush parked it" are three different pieces of news
    /// ([`Stop`]).
    Stop(Stop),
    /// End this actor for good (Ctrl-N). A `Stop` cannot do this: an
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
    /// How the run named by `run` left its worktree, sent with its `ChildDone`.
    /// Not a result and not a delivery: a listing fact (`status`), so it
    /// starts no run and marks nothing read (finding H1).
    Work { id: u64, run: u64, work: Work },
    /// A child that began a run the parent did not start: the human nudged it,
    /// a client did, or the parent's own `control message` started it. The
    /// witnesses are the hand that saw it — the UI, which is the human's — and
    /// the child's own actor, which is the only one that knows a `Steer`
    /// *resumed* it instead of being read mid-run. The parent's books need it
    /// either way: a wait would otherwise answer a stale result, and the
    /// shared-workspace guard would miss a sibling that is working (audit of the
    /// prompt vs behaviour, row 1).
    ChildRunning { id: u64 },
    /// A child's live mailbox, handed to a parent whose books hold the sender a
    /// revival replaced. `agent::revive` builds a *new* mailbox every time a
    /// parked child is woken, and the tree swaps it in — the parent's
    /// `children` book is the only copy of the old one, and without this it
    /// stays there for the rest of the session, so every later `control` from
    /// that parent finds no actor and takes the wake path again (finding H22).
    /// The message lands either way, which is why the stale sender was easy to
    /// leave; the honest book is written here.
    ChildMailbox { id: u64, cmd: Sender<AgentMsg> },
    /// A row the tree holds for a child this parent's books do not name: a
    /// parent restored from a stored session is revived with empty books while
    /// its children's rows are on screen, so `status` answered "no children"
    /// and `control` refused a child the human could see (finding H25). The
    /// books live in the actor, so the tree's row travels as a message; the
    /// outcome (when the tree has one) is recorded under a run no actor can
    /// report, and `read` is the row's `✉` mark.
    ChildBook {
        id: u64,
        cmd: Sender<AgentMsg>,
        outcome: Option<Outcome>,
        read: bool,
        shared: bool,
    },
    /// The tree has forgotten this child — the history window's reap — so the
    /// parent drops it from its books: `status` cannot list a row that is not
    /// on screen and `control` cannot aim at a ghost (finding H19).
    ForgetChild { id: u64 },
    /// A child whose actor thread the UI reclaimed: its node, id and transcript
    /// stayed exactly where they were (`App::park_history`), and the thread that
    /// knew them is gone.
    ///
    /// The parent's books identify a completion by (child, run) — the identity
    /// `docs/findings.md` B24 turns on — and a woken child starts its own
    /// counter over. Without this, the first run after a wake would land on a
    /// number the books have already read and be swallowed as news they have
    /// heard, which is a result the parent's model never gets (§8.21). The
    /// parked outcome itself is untouched: it is what `status` names the child
    /// by, and it stays read, because nothing about the result changed.
    ChildParked { id: u64 },
    /// A job this agent started ended. `line` is the report its owner reads,
    /// rendered once by the registry; `news` says whether it is worth waking a
    /// napping agent for (`ChildDone` and `Outcome::is_news` again: a job mush
    /// killed is the human's doing, not a result). A [`JobId`], so a job's
    /// report can never be filed against a child's id.
    CommandDone { id: JobId, line: String, news: bool },
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
        /// The revision the new worktree was created at, read back from the
        /// fresh checkout; `None` for a shared child. The UI keeps it on the
        /// node, because a sweep that cannot tell a branch with no commit of
        /// its own from a merged one paints "merged" over an ordinary
        /// read-only child.
        fork: Option<String>,
        /// The three-word name the caller chose, if it did; the row falls back
        /// to a handle derived from the brief.
        title: Option<String>,
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
    /// The run was stopped by a request (a Stop, Ctrl-C, Ctrl-N). The actor is
    /// still alive, so the row goes quiet instead of claiming a failure.
    Stopped,
    /// Mush's own sweep took this agent's worktree: its branch adds nothing to
    /// the base the run was forked from — its work was merged, or the run never
    /// committed anything — so the checkout and the branch are gone. `landing`
    /// says which of the two it was, so the row can say it too instead of
    /// painting "merged" over both.
    ///
    /// The row has to hear it — `git::reclaim` is the only thing in production
    /// that removes a worktree, and a row still naming `.mush/wt/<id>` and
    /// `git diff HEAD...mush/<id>` after both are gone is offering two commands
    /// that cannot run (finding U13, H10). Emitted by the actor whose run ended,
    /// because that actor is the only one that knows *when* the worktree became
    /// free.
    Reclaimed {
        landing: git::Landing,
    },
    /// The parent has read a child's result: the line is in its transcript now,
    /// wherever it came from — the fold at a message boundary, the wake-up a
    /// napping parent got, or a `wait` that asked for it.
    ///
    /// Emitted by the *parent* (the id it is tagged with) about the child it
    /// names, because the parent owns the `delivered` set. This is that fact
    /// leaving the actor, so the child's row can stop wearing `✉` on the
    /// parent's reading rather than on a guess from the shape of the rows
    /// (finding H4).
    ResultRead {
        child: u64,
    },
    /// A command a parent could not hand to a child it owns: the mailbox the
    /// parent's books hold has no actor behind it any more.
    ///
    /// That is what a *parked* child leaves behind — `App::park_history` ends
    /// the thread and nothing else, so the node, the id and the transcript stay
    /// exactly where they were — and it is also what a child the history window
    /// has since forgotten leaves, which the parent's books outlive as well
    /// (finding H19). The parent cannot tell the two apart, and cannot wake
    /// either one: only the UI holds the transcript an actor is rebuilt from.
    /// So the command travels here and the UI hands it over through the door a
    /// human's own message uses (`App::deliver_to_actor`), which revives a
    /// parked child and drops a command for an id that really is gone.
    ///
    /// The parent has already answered its model by then — an empty mailbox is
    /// not proof that the child is gone, which is the whole of finding H18 — so
    /// nothing on the UI side writes a result for this: the child's own
    /// `ChildRunning` and `ChildDone` are what settle the parent's books.
    ChildAsleep {
        child: u64,
        command: AgentMsg,
    },
    /// A parent's `control message` sent words to a child it found *at rest*,
    /// so a run is beginning on the child's own thread.
    ///
    /// The parent's books say so the moment the send lands (`ActorState::running`),
    /// and the tree is the other reader of that fact: the child's own `Running`
    /// event is a moment away, and until it lands the row still reads "at rest"
    /// — which is exactly what `park_history` reclaims. One `tick` in that
    /// window sends `Shutdown` and cancels the run the words just started (§8.21).
    /// The mark the UI sets is the optimistic one the human's own nudge sets
    /// ([`AgentTree::nudge`](crate::app::AgentTree::nudge)), and the child's own
    /// events settle it a moment later. A child already mid-run needs none: its
    /// row is busy already, and the words wait for a message boundary.
    ChildResumed {
        child: u64,
    },
    /// A report this agent could not hand to its parent: the mailbox it holds
    /// has no actor behind it (`tell_parent`).
    ///
    /// The parent's books are where a child's run is booked, and the completion
    /// is the one report whose loss leaves them wrong for the rest of the
    /// session: a running child that finished — a `wait` that burns its whole
    /// cap, a shared workspace the guard refuses to a sibling — under a row
    /// that says `✓` (§8.39). A missing actor is not a missing parent: it is a
    /// parent whose thread the UI reclaimed or replaced, and only the UI holds
    /// the transcript a new one is rebuilt from. So the command travels here and
    /// the UI delivers it to the parent the *tree* names
    /// (`App::hand_to_parent`) — the same road [`ChildAsleep`] is, for the other
    /// direction (finding H18).
    ///
    /// The root never emits this: it has no parent, so its reports have no
    /// reader and are dropped where they are made.
    ///
    /// [`ChildAsleep`]: Self::ChildAsleep
    ParentAsleep {
        command: AgentMsg,
    },
    Error(String),
    /// A job this agent started began running in the background. The registry
    /// is where a job lives; this is only what tells the screen to look at it.
    /// The id is a [`JobId`], the type half of the separation `control`'s
    /// `#2`/`#c2` grammar states in words.
    JobStarted {
        job: JobId,
        command: String,
    },
    /// A job ended, with the line its owner reads (`#c2 done: exit 0 · 3m12s ·
    /// cargo test — …`). Emitted by the job's own thread, so a job that ends
    /// while its owner naps still updates the screen.
    JobDone {
        job: JobId,
        line: String,
    },
    /// The window the endpoint itself named when it rejected a request; the UI
    /// adopts it so the bar and the tool caps agree with the agent
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
    /// is machine-wide, the lock is machine-wide, and Ctrl-N kills what the old
    /// tree left running (`crate::jobs`).
    pub registry: Arc<jobs::Registry>,
    /// The clock every wait is measured against. `wait` and a running
    /// command are the two places mush spends real time, so both read it here:
    /// a test can reach a timeout or a deadline by advancing a fake instead of
    /// waiting for the real one.
    pub clock: Arc<dyn clock::Clock>,
    /// The main workspace root; agents whose root differs are isolated.
    pub root: PathBuf,
    /// The conversation's two id counters. A value, not an `Arc<AtomicU64>`:
    /// [`Ids`] is itself the shared handle (its own clones hand out the same
    /// numbers), and this is the type that knows a job id from an agent id.
    pub ids: Ids,
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
    /// The run of each child whose outcome the model has read (via `wait`
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
    /// newer one to re-arm for (`docs/findings.md` B24). Keyed by [`JobId`] so
    /// no code path can hand one of these books a child's number.
    running_jobs: HashSet<JobId>,
    done_jobs: HashMap<JobId, JobReport>,
    delivered_jobs: HashSet<JobId>,
    /// How many runs this actor has finished — the identity a parent records on
    /// `ChildDone { run, .. }`. It counts runs, not turns, and is incremented
    /// where the run's outcome is decided.
    runs: u64,
    /// The children that run in *this* agent's workspace rather than their own
    /// worktree — including one whose isolation degraded. They are what the
    /// one-shared-child rule is about: an isolated sibling edits its own tree
    /// and conflicts with nothing here (audit row 7).
    shared: HashSet<u64>,
    /// The children the history window has reaped: their ids, remembered so a
    /// report still travelling from one of them cannot re-open books the reap
    /// closed (`AgentMsg::ForgetChild`). The ids are never reused (`crate::ids`
    /// hands out one number per agent), so the memory cannot name a *new* child
    /// by mistake. One `u64` per forgotten child, held until the tree hands the
    /// row *back* ([`note_child_book`]): a row can only be handed
    /// over for a node that exists, so it is proof the forget has gone stale,
    /// and the tombstone goes with the books it closed. Until then nothing
    /// clears it — a report can be in flight for as long as an actor lives.
    forgotten: HashSet<u64>,
    /// How each child's last finished run left its worktree, keyed by the run
    /// that left it: the branch, and whether the work is committed. A listing
    /// fact (`status`), never a delivery: reading it marks nothing, and
    /// it is paired with the completion's run so an old branch can never be
    /// read as the newer run's work (finding H1).
    work: HashMap<u64, (u64, Work)>,
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
    /// Who asked for the stop that ended the run, when a stop did: read by the
    /// outcome the parent is told ([`Stop`]). `None` means the cancel flag was
    /// the road — the human's own key sets the flag *and* sends the message, and
    /// the run can end before the message is drained, which is the one road that
    /// reaches the flag alone, and the one road that is the human's.
    stop: Option<Stop>,
    /// How many identical rounds the previous run repeated before the loop
    /// guard stopped it. The next run opens by saying so, so a nudge can
    /// resume the agent instead of repeating the call that stopped it
    /// (finding H14).
    loop_stop: Option<usize>,
    /// Set by a `wait` that slept, taken by the loop guard: a wait that spent
    /// time is not "the same call with nothing changed in between". The one
    /// call whose whole job is to spend time must not be the one call the guard
    /// kills a run for making — a hold can outlast a wait many times over, so
    /// "wait again" is an instruction the guard has to survive (`count_round`).
    waited: bool,
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

    /// How the child's *recorded* run left its worktree, if the child sent the
    /// fact. Paired by run, so work recorded for an older run is not read as
    /// the newer one's history (finding H1).
    fn work_for(&self, id: u64) -> Option<&Work> {
        let recorded = self.completed.get(&id)?.run;
        let (run, work) = self.work.get(&id)?;
        (*run == recorded).then_some(work)
    }

    /// Whether `id`'s latest recorded outcome is one the model has not read.
    /// The one derivation of the `✉` mark: `status` prints it, and the
    /// delivery roads consume it (a run recorded again under a mark that names
    /// it is not fresh). A child with no recorded outcome is not unread — there
    /// is nothing to read.
    fn unread(&self, id: u64) -> bool {
        match self.completed.get(&id) {
            Some(completion) => self.delivered.get(&id) != Some(&completion.run),
            None => false,
        }
    }

    /// Whether the tree has forgotten `id` — a child the history window reaped
    /// (`AgentMsg::ForgetChild`).
    ///
    /// `children` is the book that names this parent's children; this is the
    /// memory that the book was *closed* for an id, so the once-only delivery
    /// rule has an other end: the same run reported twice is swallowed by
    /// `delivered` (`docs/findings.md` B24), while a report from a forgotten
    /// child is not delivered at all — there is no row it could be read
    /// against, and folding it would put a line about a child nobody can see
    /// into the parent's transcript.
    fn is_forgotten(&self, id: u64) -> bool {
        self.forgotten.contains(&id)
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
        // A report from a child the tree has forgotten is not a delivery: there
        // is no row it could be read against (`is_forgotten`), and delivering
        // it would re-open a book the reap just closed.
        if self.is_forgotten(id) || self.delivered.get(&id) == Some(&run) {
            return (line, false);
        }
        self.delivered.insert(id, run);
        (line, true)
    }

    /// The same once-only delivery for a job: record the report and return its
    /// line when the model has not read it, or `None` when it has — a report
    /// recorded again is the *same* report, and folding it again would repeat a
    /// line the model has answered (`docs/findings.md` B24).
    fn record_job(&mut self, id: JobId, line: String, news: bool) -> Option<String> {
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
    /// The *name* of the ref this agent's worktree was forked from: the
    /// caller's `base` argument, or `HEAD` when it gave none. It is kept as a
    /// name because the run-end sweep asks its first question about the base
    /// *now* — a hand merge that happened while the run was going moves the
    /// base's tip onto the branch's work, and re-resolving the name is the
    /// only way to see it (the revision `worktree_add` was handed at the spawn
    /// would look like a branch ahead of its base, and mush would stop
    /// reclaiming merged work).
    ///
    /// `None` for an agent with no worktree, and for one revived from a stored
    /// session — a base is not in the file, and the sweep then reads the root's
    /// `HEAD`, which keeps more than it should rather than less.
    base: Option<String>,
    /// The revision this agent's worktree was created at, resolved once from
    /// the fresh checkout (`spawn_tool` reuses it for the spawn reply's
    /// `at <sha>`). It is the sweep's second question — "did the run commit
    /// anything of its own?" — and without it a branch whose base moved on
    /// since the spawn is indistinguishable from a merged one, which is the
    /// row that told a read-only child its work had been merged.
    ///
    /// `None` for an agent with no worktree, and for a revived agent: the fork
    /// revision is not in the stored session either, and the sweep keeps the
    /// answer it has always given there rather than guessing.
    fork: Option<String>,
    /// The task this agent was given (empty for the root). Used as the commit
    /// subject when an isolated agent's run ends.
    brief: String,
    /// Its own mailbox — where its children report their completions.
    my_tx: Sender<AgentMsg>,
    /// Where it reports its own completion to its parent. `None` for the root,
    /// which has no parent at all — the difference that matters, because a
    /// report whose send *fails* is handed to the UI to deliver
    /// ([`AgentEvent::ParentAsleep`]), and the root must never be handed its
    /// own completion back (§8.39).
    ///
    /// `Some` for every child, live or not: one revived without the mailbox its
    /// parent is listening on carries a dead one ([`dead_mailbox`]), which is
    /// exactly the case the UI's road is for.
    parent_tx: Option<Sender<AgentMsg>>,
    rx: Receiver<AgentMsg>,
}

impl Actor {
    /// Say one thing about this run to whoever owns this agent, or hand it to
    /// the UI when there is nobody at the other end ([`tell_parent`]).
    fn tell_parent(&self, command: AgentMsg) {
        tell_parent(&self.ctx, self.id, self.parent_tx.as_ref(), command);
    }
}

/// The handles every actor in one tree shares: the id counters it draws from,
/// the running-agent count it respects, and the job registry it starts commands
/// in. They travel together, because an agent given two of the three is living
/// in a tree of its own — ids that collide, or a job nobody else can see.
#[derive(Clone)]
pub struct TreeHandles {
    pub ids: Ids,
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
    /// The tree's id counters, so the UI can raise the floor above the highest
    /// id a leftover worktree already occupies (finding B1).
    pub ids: Ids,
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

/// One conversation per Ctrl-N, so stale events can be told apart: an actor
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
    let ids = Ids::default();
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
    let actor = Actor {
        ctx,
        id: 0,
        depth: 0,
        ws,
        branch: None,
        base: None,
        fork: None,
        brief: String::new(),
        my_tx: cmd_tx.clone(),
        // The root has no parent. Its children report into its own mailbox;
        // its own completion has no reader anywhere, and `None` is that fact
        // rather than a mailbox nobody listens on — the road a failed send
        // takes (`tell_parent`) ends in the UI, and the UI must never file a
        // report about the root back into the root's own mailbox (§8.39).
        parent_tx: None,
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
    /// The mailbox this agent's parent is listening on, when the caller has it.
    ///
    /// A session restore passes the tree's own sender for the parent
    /// (`App::restore_agents`): a child that comes back is one whose row is on
    /// screen and whose parent's books already say it is running, so its
    /// completion is the only thing that can ever say it stopped — and a
    /// restored child used to report into a dead channel, which left the books
    /// holding a running child that had finished (a `wait` that burned its whole
    /// 600 s cap, a shared workspace the guard refused to a sibling) while the
    /// row said `✓` (§8.39). The same for a child woken *inside* a live tree by
    /// the human's own message (`App::deliver_to_actor`).
    ///
    /// `None` is not silence: the actor is given a dead mailbox and every send
    /// that fails is handed to the UI, which finds the parent in the tree and
    /// delivers it ([`dead_mailbox`], [`AgentEvent::ParentAsleep`]).
    pub parent: Option<Sender<AgentMsg>>,
}

/// The branch an agent can still work on: one whose worktree is on disk.
///
/// A merged or discarded branch is not this agent's any more — its work is in
/// the main checkout, and its actor would commit into the human's own tree if
/// it kept the name (`revive` sends it to the root, where the work now is).
/// The node and the actor must read the same answer, or the row offers a
/// branch for a reclaimed directory while the actor runs in the checkout
/// (finding U13); this one function is where both get it.
pub fn live_branch(root: &Path, id: u64, branch: Option<String>) -> Option<String> {
    branch.filter(|_| git::worktree_path(root, id).exists())
}

/// Bring back an agent whose actor is gone — one restored from a stored session,
/// or a worktree found on disk — seeded with the transcript it had.
///
/// It comes back at rest, and it reports where the caller names it: the parent
/// it is handed owns the rows this agent's completion settles, so reviving a
/// child inside a live tree is what lets its `✓` reach its parent's books at all
/// (§8.39). It joins the same tree as the root, which is why its handles come in
/// one value (see [`TreeHandles`]).
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
        parent,
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
    let actor = Actor {
        ctx,
        id,
        depth,
        ws,
        branch,
        // A revived agent's base is not in the stored session, and the sweep
        // then measures against the root's `HEAD`: that keeps more than it
        // should rather than less, which is the only way this patch may err.
        // Neither is its fork revision, so the sweep keeps the single answer it
        // has always given there — called `merged` — rather than turning an
        // unknowable into a guess about a run nobody watched.
        base: None,
        fork: None,
        brief: brief.clone(),
        my_tx: cmd_tx.clone(),
        // Its completions go where the caller said they belong: to its parent's
        // live mailbox when the tree still holds one (a child restored from a
        // session, a parked child woken inside the tree — §8.21, §8.39), and
        // into a dead one when the caller has no mailbox to give, where the
        // failed send hands the report to the UI rather than dropping it.
        parent_tx: Some(parent.unwrap_or_else(dead_mailbox)),
        rx: cmd_rx,
    };
    // The system prompt is regenerated, and an agent with no transcript but a
    // known brief is seeded with the task — so a worktree found on disk resumes
    // knowing what it was for, even though it has no memory of the run.
    let mut transcript = vec![Message::system(prompt::subagent_prompt(
        &ws_root_str,
        depth,
        isolated.is_some(),
        depth < MAX_DEPTH,
    ))];
    if messages.is_empty() && !brief.is_empty() {
        transcript.push(Message::user(brief));
    } else {
        transcript.extend(adopted(messages));
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

/// A mailbox nobody is listening on: what a child is given when its caller
/// cannot name the mailbox its parent is listening on — a parent whose actor is
/// gone, or one whose row never had an actor at all (a worktree found on disk).
///
/// A send into it **fails**, and that failure is the whole fact the UI needs:
/// this agent *has* a parent (so the report is somebody's news) and its actor
/// cannot reach it, so the UI — the one hand holding the tree and the transcript
/// a new actor is rebuilt from — delivers it (`tell_parent`,
/// [`AgentEvent::ParentAsleep`]). The root gets `None` instead, and that road
/// must never carry a report about it (§8.39).
fn dead_mailbox() -> Sender<AgentMsg> {
    let (tx, rx) = crossbeam_channel::unbounded::<AgentMsg>();
    drop(rx);
    tx
}

/// Tell this agent's parent one fact about its run — that it started, how it
/// left its worktree, how it ended — or hand the fact to the UI when the mailbox
/// the parent left behind has no actor behind it.
///
/// Every one of these sends used to be a `let _ =`, and the one that mattered
/// was the last: a child restored from a session reported its completion into a
/// dead channel, so its parent's books kept a running child that had finished —
/// a `wait` burning its whole 600 s cap, a shared workspace the guard refused to
/// a sibling — while the row said `✓` (§8.39). A mailbox with no actor behind it
/// is not proof the parent is gone: it is a parent whose thread the UI reclaimed
/// or replaced, and the UI is the only hand that holds the transcript a new actor
/// is rebuilt from. So the command travels there in an event — the same road a
/// parent's own undeliverable command takes in the other direction
/// ([`AgentEvent::ChildAsleep`], finding H18).
///
/// `None` is the root, and is a different fact: it has no parent at all, its
/// reports have no reader, and the one thing that must never happen is the UI
/// filing them back into its own mailbox (§8.39).
fn tell_parent(ctx: &AgentCtx, id: u64, parent_tx: Option<&Sender<AgentMsg>>, command: AgentMsg) {
    let Some(parent_tx) = parent_tx else {
        return;
    };
    if parent_tx.send(command.clone()).is_err() {
        ctx.emit(id, AgentEvent::ParentAsleep { command });
    }
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
        tell_parent(
            &ctx,
            id,
            parent_tx.as_ref(),
            AgentMsg::ChildDone {
                id,
                // The run it never got to take: its first, and only.
                run: 1,
                outcome: Outcome::Failed(summary),
            },
        );
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
        // Say it to the parent too, and *before* the run does anything: a run
        // starting is the child's own news, and a parent that infers it from
        // its own books can be wrong. Its own `control message` is the case: a
        // `Steer` the parent read as "mid-run" may have arrived after the run
        // ended, so it *starts* one — and the completion of the run before it
        // is still travelling to the parent, which would then record it and
        // leave a working child marked at rest (a `status` line, the
        // one-shared-child guard, and the next `wait` all read that). The
        // message order closes it: the parent drains the completion first, then
        // this, and the books end where the child really is. The root has no
        // parent, so it says none of this (`tell_parent`).
        actor.tell_parent(AgentMsg::ChildRunning { id: actor.id });
        actor.ctx.live.fetch_add(1, Ordering::SeqCst);
        let result = run_loop(&actor, &mut state, &mut transcript, &cancel);
        actor.ctx.live.fetch_sub(1, Ordering::SeqCst);
        // How the run ended decides both the commit subject and what the parent
        // is told. A stopped run still has work worth keeping, but its commit
        // must not read like a finished one.
        let outcome = match result {
            Ok(Some(text)) => Outcome::Finished(text),
            Ok(None) => Outcome::Finished("(finished)".to_string()),
            // Which of the three stops this was decides one thing to the books
            // and three things to the parent: a park is not an ending and not
            // news, while the human's own key is an intervention the parent has
            // to hear about (`Stop`, `Outcome::is_news`). `shutdown` first: an
            // actor that is going away is mush's doing whatever else arrived.
            Err(error) if error == CANCELLED => Outcome::Stopped(if state.shutdown {
                Stop::Reclaimed
            } else {
                state.stop.take().unwrap_or(Stop::Human)
            }),
            Err(error) => Outcome::Failed(error),
        };
        // An isolated agent's branch *is* the deliverable — the thing a human
        // lands with git — so its work is committed here instead
        // of being left as untracked files in the worktree. Before the parent is
        // told, so a diff or merge it triggers already sees the work.
        let work = actor.branch.clone().map(|branch| {
            match commit_worktree(actor.ws.root(), actor.id, &actor.brief, &outcome) {
                Ok(Some(revision)) => Work::Committed { branch, revision },
                Ok(None) => Work::Clean { branch },
                Err(error) => Work::Uncommitted { branch, error },
            }
        });
        if let Some(line) = work.as_ref().and_then(Work::status_line) {
            actor.ctx.emit(actor.id, AgentEvent::Status(line));
        }
        // …and the run's own worktree, swept now that the run is finished with
        // it: a branch that adds nothing to the base this run was forked from —
        // nothing was committed, or the human merged it while the run was still
        // going — has no work to keep, so its checkout and its branch go. Most of
        // H10's residue is never made because of this line: without it, every
        // isolated run that changed nothing leaves a worktree behind for good.
        // Anything unmerged or dirty is left alone, and the sweep says so
        // instead of doing it. The landing travels with the event, so the UI
        // can mark the row with what really happened to the branch.
        if let Some(landing) = reclaim_own_worktree(&actor, &state) {
            actor.ctx.emit(actor.id, AgentEvent::Reclaimed { landing });
        }
        // This run is over, and this is its number: a parent that hears the
        // same run again has heard this report twice, while a run after it is
        // news even when the two read identically (`docs/findings.md` B24).
        state.runs += 1;
        // The worktree fact goes before the report, so a parent draining its
        // mailbox in order has it by the time it renders the listing (finding
        // H1). It is listed, never delivered: it marks nothing read.
        if let Some(work) = &work {
            actor.tell_parent(AgentMsg::Work {
                id: actor.id,
                run: state.runs,
                work: work.clone(),
            });
        }
        actor.tell_parent(AgentMsg::ChildDone {
            id: actor.id,
            run: state.runs,
            outcome: outcome.clone(),
        });
        match outcome {
            Outcome::Failed(error) => actor.ctx.emit(actor.id, AgentEvent::Error(error)),
            Outcome::Stopped(_) => actor.ctx.emit(actor.id, AgentEvent::Stopped),
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
                let aimed_at_this_run = match &command {
                    AgentMsg::Stop(whose) => Some(*whose),
                    _ => None,
                };
                match absorb(actor, state, transcript, command) {
                    Fold::End => return false,
                    _ if aimed_at_this_run.is_some() => {
                        state.stop = aimed_at_this_run;
                        state.stop_requested = true;
                    }
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

/// A conversation the actor did not build, repaired before it becomes this
/// actor's own: the transcript the UI stores, and the one a restart restores.
///
/// Both shapes a strict server rejects arrive through these doors — a call
/// whose result was never recorded (the process went away between the
/// assistant's message and its results) or the human's own words between a call
/// and them (they typed while the tools ran) — and a door that forgets is a
/// request the endpoint answers with a complaint about `tool_call_ids` instead
/// of with work. The fold is the sharpest reader of that, because a `/compact`
/// on a restored agent is the first request the stored conversation ever
/// travels in; one helper, so no door can repair differently from another.
fn adopted(mut messages: Vec<Message>) -> Vec<Message> {
    repair_tool_pairs(&mut messages);
    messages
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
        // and a Stop aimed at an agent means "stop the work in flight". It
        // records no cause: nothing is running, and a stop that labels a run
        // which has not started yet would name the wrong hand at the next
        // cancellation.
        AgentMsg::Stop(_) => {
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
            // ours to announce. The hand-over is repaired before anything reads
            // it: what the UI stored can interleave the human's steering with a
            // tool batch, or hold a call whose run never recorded a result (see
            // [`adopted`]), and a strict server rejects both shapes.
            *transcript = adopted(messages);
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
            let announced_jobs: Vec<JobId> = state
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
        // The UI's own copy of the conversation, for an actor that has none.
        // Only a restored root is ever in that state: its history is the UI's
        // until the human's next message hands it over (`Run`), and a child's
        // completion reaching it first would fold into a transcript with no
        // system message and no history — a request that is a bare `#2 done: …`
        // and nothing else (§8.39). It is not a run: a restart is not a request,
        // and the next `Run` replaces this copy with the UI's newer one.
        //
        // An actor that already has a conversation keeps it: its own copy is the
        // newer one, which is the same reason `Run` is only folded in at an idle
        // boundary (`drain_mailbox`).
        AgentMsg::Adopt(messages) => {
            if transcript.is_empty() {
                *transcript = adopted(messages);
            }
            Fold::Idle
        }
        AgentMsg::Nudge(message) => {
            if worktree_gone(actor) {
                actor
                    .ctx
                    .emit(actor.id, AgentEvent::Notice(worktree_gone_line(actor.id)));
                return Fold::Idle;
            }
            transcript.push(message);
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
            // An actor that has never run has no transcript: the UI holds the
            // conversation and hands it over with the first `Run` — or, for a
            // restored root, with `Adopt` at the restore door. A fold is not a
            // run, so this is the other hand-over: without it the command folded
            // nothing and said nothing. It is repaired like every hand-over
            // ([`adopted`]): the summarize request is the first
            // one the stored conversation ever travels in, so a call left
            // dangling by the process that wrote the file is exactly what the
            // endpoint would refuse here.
            if transcript.is_empty() {
                *transcript = adopted(messages);
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
        // How a run left its worktree: a listing fact, not a result. It starts
        // no run and marks nothing read, whichever order it arrives in
        // (finding H1).
        AgentMsg::Work { id, run, work } => {
            note_work(state, id, run, work);
            Fold::Idle
        }
        // The child's actor thread was replaced: its run numbering starts over,
        // so the books' identity for it does too (see `note_parked`). Nothing
        // is delivered and nothing is folded — the outcome stands, and it is
        // read.
        AgentMsg::ChildParked { id } => {
            note_parked(state, id);
            Fold::Idle
        }
        // A child the human resumed begins a run: the parent's books follow,
        // and nothing else happens — no line, no wake (audit row 1).
        AgentMsg::ChildRunning { id } => {
            note_running(state, id);
            Fold::Idle
        }
        // The mailbox of a child the parent already knows is live again: the
        // book is refreshed, nothing is delivered and no run is started — a
        // book-keeping command, so an idle parent stays idle (finding H22).
        AgentMsg::ChildMailbox { id, cmd } => {
            note_mailbox(state, id, cmd);
            Fold::Idle
        }
        // The row of a child the books have never held: seeded where the actor
        // learns who its children are, so `status` and `control` agree with
        // the rows on screen instead of assuming empty books (finding H25).
        AgentMsg::ChildBook {
            id,
            cmd,
            outcome,
            read,
            shared,
        } => {
            note_child_book(state, id, cmd, outcome, read, shared);
            Fold::Idle
        }
        // The history window has forgotten this child: its row is off the
        // screen, so the books that name it go too (finding H19). Nothing is
        // folded and nothing ends: this is a book-keeping command.
        AgentMsg::ForgetChild { id } => {
            forget_child(state, id);
            Fold::Idle
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

/// Sweep the worktree an isolated run has just finished in, and answer which
/// landing took it — the fact the UI needs to mark the row landed, so the row
/// stops offering a `git diff` against a directory that is gone and a branch
/// that is gone with it (finding U13, H10).
///
/// The run's own end knows both questions' inputs exactly: the base's *name*
/// (asked now, so a merge that happened while the run was going is seen) and the
/// fork revision `worktree_add` created the branch at (so "this run committed
/// nothing" is the git shape of the branch, not a memory of what the commit
/// step thought). A revived agent has neither in its stored session, and the
/// sweep then reads the root's `HEAD` with no fork: it keeps more than it should
/// rather than less, and the one landing it can name is `Merged`.
///
/// An actor that still has work of its own out — a child that will wake it, or a
/// job whose report does — keeps its worktree: that wake starts a run *in this
/// directory*, and a run in a directory that is gone recreates it as a plain
/// path no surface can see (finding S1). The next run's end sweeps it instead.
fn reclaim_own_worktree(actor: &Actor, state: &ActorState) -> Option<git::Landing> {
    if actor.branch.is_none() || !state.running.is_empty() || !state.running_jobs.is_empty() {
        return None;
    }
    let base = actor.base.clone().unwrap_or_else(|| "HEAD".to_string());
    match git::reclaim(&actor.ctx.root, actor.id, &base, actor.fork.as_deref()) {
        git::Reclaimed::Removed { landing, .. } => Some(landing),
        _ => None,
    }
}

/// Whether this actor is an isolated agent whose worktree has been reclaimed
/// (a hand-run `git worktree remove`, or a `landed` agent restored from an old
/// session).
///
/// It must not run again: its file tools resolve their directory from the
/// workspace it was spawned with, so a write would recreate the dead path as a
/// plain directory that no surface — not `git status`, not `git diff`, not
/// `git merge` — can show, diff or land (finding S1). `App::deliver` refuses the
/// human's own message before it is sent; this is the backstop for every other
/// sender (a parent's `control` message).
fn worktree_gone(actor: &Actor) -> bool {
    actor.branch.is_some() && !actor.ws.root().exists()
}

/// What such an actor reports when work arrives anyway: nothing ran, and where
/// to work instead. All three ways mush settles a worktree are named — a run
/// that committed nothing lands the same way a merge or a discard does —
/// because this line cannot see which one took *this* worktree, and claiming
/// "merged" about a run that never committed is the lie the row stopped telling.
fn worktree_gone_line(id: u64) -> String {
    format!(
        "agent #{id}'s worktree is gone (it was merged, discarded or never committed) — \
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

/// The one spelling of a reply whose *framing* broke before it could be read —
/// used by the run's own turn and by a fold, so the two cannot describe the same
/// class of failure differently.
///
/// It names the reply, never the endpoint's opinion: this error comes from bytes
/// that failed to frame themselves (a status line, a header, a chunk size that
/// is not one), not from an answer the endpoint sent — [`ModelError::Refused`]
/// is the one where the endpoint really did answer and mush is the one saying no
/// (a body past the cap). Reporting the two as the same sentence is what told a
/// human their endpoint refused a request it had answered (finding B27).
fn reply_broke(base_url: &str, error: &str) -> String {
    format!("the reply from {base_url} broke before it could be read: {error}")
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
/// A round that did nothing is not a repeat — nothing happened, so nothing is
/// being repeated — and there are two of them. One is a batch that asked for
/// something and was *refused* before anything ran: counting it is what killed a
/// fixer and an integrator whose only crime was retrying a locked machine
/// (finding H13). The other is a `wait` that slept (`state.waited`): the call
/// asked the world to move on and the world answered "not yet", which is the one
/// answer that is not "nothing changed in between". An exclusive hold can
/// outlast a single wait many times over, so the road back the refusal names has
/// to survive being taken more than once. Either way the count is cleared:
/// rounds that did run before it are not evidence about this one.
fn count_round(last_batch: &mut String, repeats: &mut usize, batch: &str, did_nothing: bool) {
    if batch == last_batch {
        if did_nothing {
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
    // Whether the batch just run was refused before anything ran, and whether a
    // blocking `wait` in it actually slept — the two rounds the guard must not
    // read as a model repeating itself (finding H13, and the road back from a
    // lock: a hold can outlive many waits).
    let mut refused_round = false;
    let mut waited_round = false;
    // The flag itself is the *call's*, and a run that ended between two calls
    // of a batch — a Stop, a cut-off reply — can leave it set. It means "the
    // call just run slept", so a run starts with none behind it: a stale one
    // would exempt this run's first repeat from the guard.
    state.waited = false;
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
        // Keep the whole request inside the endpoint's context window. The
        // trimmer stops at its watermark — `trim_target` of the budget, not the
        // budget itself: a cut to the brim is one the next turn's growth makes
        // again, and every cut rewrites the front of the prompt, which is the
        // prefix a provider's cache warmed. The room the watermark leaves below
        // the fold's trigger is room the next growth is folded in rather than
        // cut.
        trim_history(messages, trim_target(budget));

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
            // A refusal — a body past `MAX_BODY_BYTES` — is not a connection
            // failure: the endpoint answered, and saying so is the difference
            // between "check the URL" and "the reply was too big".
            Err(ModelError::Refused(error)) => {
                return Err(format!("the endpoint's reply was refused: {error}"));
            }
            // A reply whose framing broke is neither: it is bytes that never
            // framed themselves, on a connection `http.rs` has already
            // dropped. It must not be reported as the endpoint's refusal —
            // that is what told the human to blame a healthy endpoint for a
            // chunk line mush could not account for (finding B27).
            Err(ModelError::Framing(error)) => {
                return Err(reply_broke(&cfg.base_url, &error));
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
            // rather than a broken model (a whole file in one `run_command`, or
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
            // One door, so both see it: the instruction is in the request the
            // model answers, and a line that reached `messages` alone would be
            // a line the human's copy cannot account for — the actor's
            // transcript is replaced by the UI's at the next idle `Run`.
            push_line(actor, messages, TRUNCATION_INSTRUCTION.to_string());
            continue;
        }
        // A reply that was not cut off ends the run of them: the guard counts
        // *consecutive* truncations, and four scattered over a long run are not
        // "in a row" (audit row 16).
        cut_offs = 0;

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
        // Two rounds are exceptions, because neither one moved the world: a
        // batch the machine *refused* (nothing ran, so nothing is repeating —
        // finding H13) and a batch whose `wait` slept (the call spent the round
        // and was told "not yet", which is the whole road back from a lock).
        if !tool_calls.is_empty() {
            let batch = tool_calls
                .iter()
                .map(|call| format!("{}:{}", call.function.name, call.function.arguments))
                .collect::<Vec<_>>()
                .join("\n");
            count_round(
                &mut last_batch,
                &mut repeats,
                &batch,
                refused_round || waited_round,
            );
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
        // Whether every call in this batch was refused *before it ran*, and
        // whether a blocking `wait` in it actually waited, are the loop guard's
        // business (H13), so both are collected as the batch runs and handed to
        // the next round's guard.
        let mut all_refused = true;
        waited_round = false;
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

            let (output, images, refused) = match result {
                Ok(output) => (output.text, output.images, false),
                Err(ToolError::Refused(why)) => (format!("error: {why}"), Vec::new(), true),
                Err(ToolError::Failed(error)) => (format!("error: {error}"), Vec::new(), false),
            };
            // A `wait` that slept says so in the state it set; taking it here
            // is what keeps the guard from reading the same blocking call again
            // as a loop (see [`count_round`]). Every other call leaves it
            // false, so a round of them counts as it always did.
            waited_round |= std::mem::take(&mut state.waited);
            // Every call in this batch refused before it ran: the round counts
            // as nothing attempted, which is what keeps the loop guard from
            // condemning a model waiting on a locked machine (H13).
            all_refused &= refused;
            let tool_message = Message::tool_with_images(call.id.clone(), output, images);
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
ran. Do the same work in smaller steps: one file or edit per call, a few hundred \
lines at a time (create a file with a heredoc — `cat > file <<'EOF'` — then \
extend it with `edit_file`). Do not repeat work you already completed in \
earlier calls.";

/// What a human who typed `/compact` is told when there is nothing to fold.
///
/// A fold of `[system, the opening message]` costs a request and can only
/// re-summarize the summary, so it is refused — but never silently: the human
/// asked, and a status line that fades into nothing is the failure mode this
/// whole command exists to avoid. Anything bigger is folded as asked.
const NOTHING_TO_COMPACT: &str =
    "nothing to compact — this transcript is already short enough to send whole";

/// A fold that came to nothing, said the one way: the refusal when the human is
/// the one who asked, and the end of the phase either way.
///
/// One door, so neither arm can answer the human differently from the other and
/// neither can leave a `compacting…` on the row — a fold that came to nothing
/// must not outlive the request that justified it (finding U11), and the
/// automatic trigger, which nobody asked for, has nothing to report (refactor
/// R16).
fn nothing_to_compact(actor: &Actor, in_run: bool, asked: bool) {
    if asked {
        actor
            .ctx
            .emit(actor.id, AgentEvent::Notice(NOTHING_TO_COMPACT.to_string()));
    }
    actor
        .ctx
        .emit(actor.id, AgentEvent::CompactingEnded { in_run });
}

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
        nothing_to_compact(actor, in_run, asked);
        return Ok(false);
    }
    // Nothing left to fold: system + one message is already minimal
    // (usually a previous summary), so compacting again would just cost a
    // request and re-summarize the summary. A human who asked for it is told
    // so rather than left watching a status line that never ends.
    if messages.len() <= 2 {
        nothing_to_compact(actor, in_run, asked);
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
        Err(ModelError::Unreachable(error)) | Err(ModelError::Transport(error)) => {
            return Err(format!("cannot reach {}: {error}", cfg.base_url));
        }
        // A reply that broke on the way in is not a connection failure either:
        // say what it was, in the run's own words.
        Err(ModelError::Framing(error)) => return Err(reply_broke(&cfg.base_url, &error)),
        Err(ModelError::Refused(error)) => {
            return Err(format!("the endpoint's reply was refused: {error}"));
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
            AgentMsg::Stop(whose) => {
                cancel.store(true, Ordering::SeqCst);
                // The hand that asked travels with the cancellation: the
                // outcome the parent is told has to name it ([`Stop`]).
                state.stop = Some(whose);
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
            // A listing fact, not a signal: it starts nothing, ends nothing,
            // and is folded nowhere (finding H1).
            AgentMsg::Work { id, run, work } => note_work(state, id, run, work),
            // The child's actor thread went away and its run numbering with it
            // (see `note_parked`). A signal about the books rather than about
            // the run in flight, so it is honoured mid-run like any other.
            AgentMsg::ChildParked { id } => note_parked(state, id),
            // A child the human resumed. Nothing to fold: the parent's book of
            // what is running is the whole point (audit row 1).
            AgentMsg::ChildRunning { id } => note_running(state, id),
            // A book-keeping command, honoured mid-run like `ChildParked`: the
            // parent's mailbox for a live child, or a row seeded from the tree,
            // or a child the tree has forgotten. None of the three is work to
            // fold, and none of them may wait for a message boundary — the
            // books are what a `wait`/`control` inside this same run reads
            // (`run_loop`'s boundaries drain this queue first).
            AgentMsg::ChildMailbox { id, cmd } => note_mailbox(state, id, cmd),
            AgentMsg::ChildBook {
                id,
                cmd,
                outcome,
                read,
                shared,
            } => note_child_book(state, id, cmd, outcome, read, shared),
            AgentMsg::ForgetChild { id } => forget_child(state, id),
            AgentMsg::CommandDone { id, line, news } => {
                note_job(state, id, line, news);
            }
            // A conversation for an actor that has none — and this one has a
            // run in flight, which is a conversation already. Dropped here
            // rather than parked for a boundary that would drop it too
            // (`absorb`'s arm holds the rule both doors read).
            AgentMsg::Adopt(_) => {}
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
            AgentMsg::Nudge(message) => messages.push(message),
            // A parent's steering was never echoed anywhere: this is the only
            // way it reaches the human's copy of this agent's transcript.
            AgentMsg::Steer(text) => push_line(actor, messages, text),
            // A message-boundary job like a nudge: the transcript is folded
            // into a summary at the next turn, never between an assistant's
            // tool calls and their results. The transcript the request carries
            // is for an actor that has none (see `absorb`); mid-run, the
            // transcript this actor owns is the newer one.
            AgentMsg::Compact(_) => state.compact_requested = true,
            AgentMsg::Stop(whose) => {
                cancel.store(true, Ordering::SeqCst);
                state.stop = Some(whose);
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
            // The worktree fact of the run just recorded. It is not a result:
            // nothing is pushed and no boundary is moved (finding H1).
            AgentMsg::Work { id, run, work } => note_work(state, id, run, work),
            // The child's actor thread was replaced, so the books' run
            // identity for it restarts with it (see `note_parked`). It carries
            // no work to fold: the words belong to the child's own actor.
            AgentMsg::ChildParked { id } => note_parked(state, id),
            // A child the human resumed: the parent's books say it is running
            // again, and nothing enters the transcript (audit row 1).
            AgentMsg::ChildRunning { id } => note_running(state, id),
            // The three book-keeping commands, at a message boundary as in the
            // idle drain: none of them is a line for the transcript (findings
            // H19, H22, H25).
            AgentMsg::ChildMailbox { id, cmd } => note_mailbox(state, id, cmd),
            AgentMsg::ChildBook {
                id,
                cmd,
                outcome,
                read,
                shared,
            } => note_child_book(state, id, cmd, outcome, read, shared),
            AgentMsg::ForgetChild { id } => forget_child(state, id),
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
            // A conversation for an actor that has none: this one has the newer
            // copy — the run's own, mid-flight — and replacing it would erase
            // the turn being answered. The rule is `absorb`'s; this door only
            // sees it one boundary late.
            AgentMsg::Adopt(_) => {}
        }
    }
}

/// Record a child's completion and return the line the model reads.
///
/// A newer run supersedes the recorded outcome, and the delivery and running
/// marks share one rule: only a run the books have not heard of is an ending,
/// so recording the same run again clears neither (`docs/findings.md` B24,
/// audit row 1).
fn note_completion(state: &mut ActorState, id: u64, run: u64, outcome: Outcome) -> String {
    let line = outcome.line(id);
    // A report from a child the history window has forgotten is not this
    // parent's news (`AgentMsg::ForgetChild`): recording it would re-open a
    // book no reader can reach and arm a fold the tree has no row for.
    if state.is_forgotten(id) {
        return line;
    }
    if state.completed.get(&id).map(|completion| completion.run) != Some(run) {
        state.running.remove(&id);
        state.completed.insert(id, Completion { run, outcome });
    }
    line
}

/// Write a child's live mailbox into the parent's books (finding H22).
///
/// A revival builds the child a fresh `Sender<AgentMsg>` and the tree swaps it
/// in (`App::deliver_to_actor`); the parent's `ActorState::children` still holds
/// the dead one, so its next `control` finds no actor and takes the wake path
/// again — and again, for the rest of the session. The message lands either
/// way, which is why the stale sender was easy to leave; the book is written
/// here, where the mailbox changes hands, and nowhere else.
fn note_mailbox(state: &mut ActorState, id: u64, cmd: Sender<AgentMsg>) {
    // A child the tree has dropped has no row a mailbox could belong to, and
    // the tombstone is the last word about it: writing the book back would put
    // a name into `status` with nothing on screen behind it, and hand `control`
    // an actor nobody can see. The rule the other books of a child follow
    // (`note_running`, `note_work`, `record_child`) belongs on this one too;
    // only `note_child_book` clears a tombstone, by handing the row back.
    if state.is_forgotten(id) {
        return;
    }
    state.children.insert(id, cmd);
}

/// Seed a child's books from the row the tree holds for it (finding H25).
///
/// A parent restored from a stored session is revived with an empty
/// `ActorState` while its children's rows are on screen: `status` answered "no
/// children and no jobs" and `control` refused a child the human could see.
/// The books live in the actor and the rows in the UI, so the UI hands the row
/// over — which is why this is a message, and not a second read of the tree
/// from inside the actor.
///
/// The outcome is recorded under [`NO_RUN`]: the run the tree's row reports is
/// one no actor in this process has numbered, and the child's next report must
/// not be swallowed as a run the books have already read (`note_parked` makes
/// the same move for the same reason). `read` is whether the row's result has
/// already been read — the `✉` is the mark that it has *not* — so an unread
/// result stays unread and a `wait` hands it over exactly once.
fn note_child_book(
    state: &mut ActorState,
    id: u64,
    cmd: Sender<AgentMsg>,
    outcome: Option<Outcome>,
    read: bool,
    shared: bool,
) {
    state.children.insert(id, cmd);
    // The tree naming the child is the one proof that an earlier forget is
    // stale — a row can only be handed over for a node that exists — so the
    // tombstone goes with the books it closed.
    state.forgotten.remove(&id);
    // A child with no branch of its own runs in its parent's workspace: the
    // one-shared-child rule and `control`'s worktree check both read this book.
    if shared {
        state.shared.insert(id);
    }
    if let Some(outcome) = outcome {
        note_completion(state, id, NO_RUN, outcome);
        if read {
            state.delivered.insert(id, NO_RUN);
        }
    }
}

/// Drop a child from the parent's books: the history window has forgotten it
/// (`App::reap_history`), so its row is off the screen and nothing about it may
/// be named again (finding H19).
///
/// The books are the actor's, so the UI can only *ask*: the message is sent
/// into the parent's mailbox, and a send that finds no actor is dropped on the
/// floor. That is the intended answer — a dormant parent has no books left to
/// correct — and it is why a parent whose session is restored later re-derives
/// them from the tree (`note_child_book`) rather than trusting a name that
/// outlived its row.
///
/// `children` goes first, because it is the book that says whose reports are
/// news. The id then goes into `forgotten`, which is what keeps it so: a
/// forgotten child must not be able to re-deliver anything, and after this
/// there is no book left to deliver into *and* no report of its that any road
/// will accept. Every other per-child book follows, so no listing (`work`,
/// `completed`), no wait (`running`) and no shared-workspace guard (`shared`)
/// can read a child the tree has dropped.
fn forget_child(state: &mut ActorState, id: u64) {
    state.children.remove(&id);
    state.forgotten.insert(id);
    state.completed.remove(&id);
    state.delivered.remove(&id);
    state.running.remove(&id);
    state.shared.remove(&id);
    state.work.remove(&id);
}

/// A child the human resumed begins a run: the parent's books say it is
/// running, and nothing enters the transcript (audit row 1).
///
/// A resume sent before the reap and drained after it would otherwise write a
/// running entry back into books the tree has closed — the one book with no
/// row to list it under, since `status`, `wait` and the shared-workspace guard
/// all read `running` *through* `children` or `shared`, which
/// [`forget_child`] has emptied. `reclaim_own_worktree` is the exception: it
/// counts the set itself, so the ghost would pin this actor's worktree for the
/// rest of the session.
fn note_running(state: &mut ActorState, id: u64) {
    if state.is_forgotten(id) {
        return;
    }
    state.running.insert(id);
}

/// The books of a child whose actor thread was parked and replaced.
///
/// A completion is identified by (child, run) — two runs that read the same are
/// still two runs, and the same run reported twice is one piece of news
/// (`docs/findings.md` B24) — while a woken child's counter starts over at 1
/// (`agent::revive` builds a fresh `ActorState`). The run the books know is
/// therefore moved to one no actor can report: `state.runs` counts a run as it
/// *ends*, so the first report any actor makes is run 1 and 0 is nobody's. The
/// child's next ending then takes a number the books cannot have read, which is
/// what makes it news.
///
/// What the books hold about the result itself is left alone. The outcome is
/// what `status` names the child by — a child with no recorded outcome reads as
/// `#N ◐ running`, and a parked child is not running — and it stays *read*: the
/// pair (outcome, delivered) moves together, so the fact that the parent has
/// seen this result is exactly as true as it was a moment ago.
fn note_parked(state: &mut ActorState, id: u64) {
    // A run number no actor reports: see [`NO_RUN`].
    if let Some(completion) = state.completed.get_mut(&id) {
        completion.run = NO_RUN;
    }
    if let Some(run) = state.delivered.get_mut(&id) {
        *run = NO_RUN;
    }
    // The worktree fact is paired with the run it belongs to (`ActorState::work_for`),
    // so it moves with the pair above: a listing that lost the branch of the
    // last run would be the one fact a parent deciding on a merge is missing
    // (finding H1).
    if let Some((run, _)) = state.work.get_mut(&id) {
        *run = NO_RUN;
    }
    // A parked child is at rest — that is the window's own condition — so a
    // book that still says it is running holds a report the parked actor had
    // already sent and this message outran. Left standing, it is a `wait` that
    // blocks for the whole timeout on a child that has finished.
    state.running.remove(&id);
}

/// Record how a run left its worktree. Kept by run, and only the newest run's
/// fact survives: a `Work` is never delivered (nothing reads it as a result),
/// so this is a plain latest-value book (finding H1).
fn note_work(state: &mut ActorState, id: u64, run: u64, work: Work) {
    // A worktree fact for a forgotten child has no row to be listed under
    // (`AgentMsg::ForgetChild`), and the entry would sit there for the life of
    // the actor.
    if state.is_forgotten(id) {
        return;
    }
    match state.work.get(&id) {
        Some((known, _)) if *known > run => {}
        _ => {
            state.work.insert(id, (run, work));
        }
    }
}

/// The same bookkeeping for a job: it is no longer running, and its report is
/// the line the model reads. A job ends once, under an id nothing else reuses,
/// so a report recorded again is the *same* report: the delivery mark stands,
/// and it is not cleared here. Clearing it unconditionally is what let a job's
/// line fold twice (`docs/findings.md` B24, `note_completion`'s twin).
fn note_job(state: &mut ActorState, id: JobId, line: String, news: bool) -> String {
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
/// `wait` in time" (docs/mush.md §5.5), and it runs at *every* message
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
    let jobs: Vec<(JobId, String, bool)> = state
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
    // summary it asked for, even of a child that was stopped. `Outcome::is_news`
    // decides only whether a *napping* parent is woken into a fresh run — this
    // fold happens inside a run somebody already paid for.
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

/// What a tool call hands back to the model: the text it reads, plus any images
/// that travel inside the same tool message.
///
/// Almost every call is text alone, and `Deref<Target = str>` (with the three
/// impls beside it) keeps those call sites and every test reading exactly as
/// they did. One producer attaches an image — `read_file`, when the file is a
/// picture — and a tool message carrying both is not a second kind of result:
/// it is what the spec's vision form asks a tool result to be.
#[derive(Debug, Default)]
struct ToolOutput {
    text: String,
    images: Vec<Image>,
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl std::ops::Deref for ToolOutput {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl PartialEq<&str> for ToolOutput {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl PartialEq<String> for ToolOutput {
    fn eq(&self, other: &String) -> bool {
        &self.text == other
    }
}

impl std::fmt::Display for ToolOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Why a tool call produced no result.
///
/// `Refused` is the machine saying *not now* — the lock is held, the job budget
/// is full — so nothing ran and nothing changed. `Failed` is the call itself
/// going wrong. The loop guard reads the difference: a batch of refusals is not
/// a model repeating itself, and counting it as one killed an integrator and a
/// fixer whose only mistake was retrying a locked machine (finding H13). Both
/// variants travel to the transcript as text under `error: `; only the guard
/// cares which road produced it.
#[derive(Debug)]
enum ToolError {
    Refused(String),
    Failed(String),
}

impl ToolError {
    /// The sentence the model reads, either way. Tests assert on it without
    /// caring which road produced it; production code does care ([`count_round`]
    /// reads the variant), so this lives only where it is read.
    #[cfg(test)]
    fn text(&self) -> &str {
        match self {
            ToolError::Refused(why) | ToolError::Failed(why) => why,
        }
    }
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
) -> Result<ToolOutput, ToolError> {
    // `run_command` is the one tool that can be refused before anything runs
    // (the machine lock, the job budget), so it returns the verdict itself;
    // every other tool either ran or failed. `read_file` owns its output type
    // for the same kind of reason: it is the one tool whose result can be an
    // image as well as text.
    let answer = match tool {
        ToolName::RunCommand => {
            return run_command(actor, state, args, cancel).map(ToolOutput::from)
        }
        ToolName::ReadFile => return read_tool(actor, args).map_err(ToolError::Failed),
        ToolName::SpawnAgent => spawn_tool(actor, state, args),
        ToolName::Status => status_tool(actor, state),
        ToolName::Control => control_tool(actor, state, args),
        ToolName::Wait => match wait_target(args)? {
            Some(target) => wait_on_tool(actor, state, cancel, target),
            None => wait_tool(actor, state, cancel),
        },
        // The file tools take no lock and start no process: they are the roads
        // that keep working while another agent holds the machine, and the only
        // road that can carry an image (finding H31).
        ToolName::EditFile => edit_tool(&actor.ws, args),
        ToolName::WriteFile => write_tool(actor, args),
        ToolName::ListFiles => list_tool(actor, args),
        ToolName::Search => search_tool(actor, args),
    };
    answer.map_err(ToolError::Failed).map(ToolOutput::from)
}

/// The refusal a spawn gets when the repository already holds `MAX_WORKTREES`
/// worktrees that no sweep will take: which ones, where they are, and the
/// commands that clear one. Named rather than counted — a number a human cannot
/// act on is exactly what this check exists to replace (finding H17).
fn too_many_worktrees(held: &[u64]) -> String {
    /// How many worktrees the refusal names before it counts the rest: the four
    /// fit a tool result's line, and the model needs the shape, not the roster.
    const NAMED: usize = 4;
    let named = held
        .iter()
        .take(NAMED)
        .map(|id| format!("#{id} ({})", git::worktree_rel(*id)))
        .collect::<Vec<_>>()
        .join(", ");
    let rest = held.len().saturating_sub(NAMED);
    let more = if rest > 0 {
        format!(" and {rest} more")
    } else {
        String::new()
    };
    format!(
        "cannot spawn: {} isolated worktrees already exist and none of them is landable \
         (the limit is {}) — each holds an unmerged branch or uncommitted work: {named}{more}. \
         Land or drop one first: merge or delete its branch, then \
         `git worktree remove --force .mush/wt/<id>` and `git branch -d mush/<id>`.",
        held.len(),
        git::MAX_WORKTREES
    )
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
             Wait for one with wait before spawning another."
        ));
    }
    let brief = tools::arg_string(args, "brief")?;
    // A base is the isolation switch: with one, the child gets its own worktree
    // and branch forked from that ref; without one it shares this workspace.
    // A base is a promise about history, so it is resolved before anything is
    // created and never silently dropped (finding H7). The resolved id is what
    // `worktree_add` gets; the *name* is what the actor keeps, because the
    // run-end sweep asks whether the base contains the branch's work — and a
    // merge that happened while the run was going moved the base's tip, which
    // only re-resolving the name can see.
    let named = args.get("base").and_then(Value::as_str);
    let base: Option<String> = match named {
        Some(name) => Some(git::resolve(&ctx.root, name).ok_or_else(|| {
            format!("unknown base `{name}`: no commit, branch or tag by that name")
        })?),
        None => None,
    };
    let base_name = named.map(str::to_string);
    let isolated = base.is_some();
    // The name the caller chose for the row, trimmed; blank means none, and the
    // row falls back to its handle from the brief.
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    if !isolated {
        // Decide this *before* writing the brief: the check can only fail after
        // the brief exists, so the rule is stated in the tool schema and the
        // system prompt as well.
        //
        // Only a *shared* child conflicts with this workspace: an isolated
        // sibling edits its own worktree, so a running one must not block a
        // shared spawn — the old guard counted it and then said something false
        // about this workspace (audit row 7).
        let mut running_shared: Vec<u64> = state
            .shared
            .iter()
            .copied()
            .filter(|id| state.running.contains(id))
            .collect();
        if !running_shared.is_empty() {
            running_shared.sort_unstable();
            let names = running_shared
                .iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "cannot spawn: {names} already runs in this shared workspace, and only one shared child \
                 may run at a time. Pass base=<branch or commit> to give a sibling its own worktree, or \
                 wait for it first."
            ));
        }
    }

    // The cap on what a run leaves on disk, checked here — before the id is
    // taken and before `worktree add` runs. Today's failure is git's own, a
    // fatal a moment later with the number already spent and a gap on the screen
    // that nothing explains (finding H17); this is the same refusal, planned,
    // naming what to clear. Only an isolated spawn pays it (a shared child makes
    // no worktree), and it counts what no sweep will take, so a worktree whose
    // work is merged — one that is leaving on its own — never refuses anyone
    // (finding H10).
    if isolated {
        let held = git::unlandable(&ctx.root);
        if held.len() >= git::MAX_WORKTREES {
            return Err(too_many_worktrees(&held));
        }
    }

    let id = ctx.ids.next_agent();
    let (child_ws, branch) = match named {
        // A worktree on `mush/<id>`, forked from the base. A base is a promise
        // about history: if git cannot make the worktree, the delegation fails
        // rather than running the brief in the wrong tree (finding H7).
        //
        // git is the line `lose_agent` stops at: its refusal means no worktree
        // and no branch was made, so the number is handed back and the retry
        // after a bad `base` is consecutive. A failure *after* this arm —
        // `Workspace::new` — leaves the worktree git just made, so the number
        // stays spent: the invariant is "reusable only if nothing was created",
        // and a `mush/<id>` branch is something.
        Some(name) => match git::worktree_add(&ctx.root, id.0, base.as_deref()) {
            Ok((path, branch)) => match Workspace::new(&path) {
                Ok(child_ws) => (child_ws, Some(branch)),
                Err(error) => return Err(format!("cannot start from `{name}`: {error}")),
            },
            Err(reason) => {
                ctx.ids.lose_agent(id);
                return Err(format!("cannot start from `{name}`: {reason}"));
            }
        },
        None => (actor.ws.clone(), None),
    };
    // The branch the parent will need to land the work, said where it is born:
    // the parent chose the worktree, and a child whose branch it never learned
    // is a child it cannot diff or merge by hand (finding H1). The commit is
    // read back from the new worktree, so the reply names the history the child
    // *got*, not the one that was asked for (finding H7).
    let on = branch
        .as_deref()
        .map(|branch| format!(" on {branch}"))
        .unwrap_or_default();
    // The one resolve of the new checkout's `HEAD`, reused for the reply's
    // `at <sha>` and kept as the actor's fork revision: a run that commits
    // nothing leaves the branch standing exactly here, and this is the only
    // thing that can tell such a branch from one whose work was merged.
    let fork = branch
        .as_ref()
        .and_then(|_| git::resolve(&git::worktree_path(&ctx.root, id.0), "HEAD"));
    let at = fork
        .as_deref()
        .map(|sha| format!(" at {}", short_revision(sha)))
        .unwrap_or_default();
    // A child with no base runs in this workspace: it is one of the children
    // the one-shared-child rule is about.
    let shares_workspace = branch.is_none();

    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<AgentMsg>();
    // The UI hears about the child before any of its events can arrive, so
    // every later event has a node to land on.
    ctx.emit(
        parent,
        AgentEvent::Spawned {
            child: id.0,
            parent,
            brief: brief.clone(),
            depth: depth + 1,
            branch: branch.clone(),
            fork: fork.clone(),
            title,
            cmd: cmd_tx.clone(),
        },
    );

    // Who the child is goes in the system prompt; the parent's task is the
    // first user message, mirroring the root's system+user shape. Some
    // servers' chat templates also reject a system-only first request.
    let whoami = prompt::subagent_prompt(
        &child_ws.root_str(),
        depth + 1,
        branch.is_some(),
        depth + 1 < MAX_DEPTH,
    );
    let initial = if brief.trim().is_empty() {
        vec![Message::system(whoami), Message::user(prompt::BEGIN_TASK)]
    } else {
        vec![Message::system(whoami), Message::user(brief.clone())]
    };
    // The child's own mailbox is where grandchildren report; the parent's
    // mailbox is where this child reports its completion.
    let child = Actor {
        ctx: ctx.clone(),
        id: id.0,
        depth: depth + 1,
        ws: child_ws,
        branch,
        // The base's name, for question 1 at the run's end; the fork revision
        // below is question 2 there. Neither is a memory of what the commit
        // step thought: both are git facts (finding H10).
        base: base_name.clone(),
        fork: fork.clone(),
        brief: brief.clone(),
        my_tx: cmd_tx.clone(),
        parent_tx: Some(actor.my_tx.clone()),
        rx: cmd_rx,
    };
    start(child, initial, true);

    state.children.insert(id.0, cmd_tx);
    state.running.insert(id.0);
    // Which children share this workspace: the ones the one-shared-child rule
    // is about.
    if shares_workspace {
        state.shared.insert(id.0);
    }
    // A run is bounded by progress, not by a turn count: it ends when the model
    // stops calling tools, and is cut short only if it starts looping
    // (`LOOP_ROUNDS` identical rounds). The only hard ceiling is a runaway
    // guard far past any real task, so this is not a budget to size a brief
    // against any more.
    Ok(format!(
        "spawned agent {id}{on}{at} · runs until it stops calling tools · wait returns its summary"
    ))
}

/// The short form of a commit id, for a line a model reads: the same shape
/// `git rev-parse --short` gives a commit message.
fn short_revision(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
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

/// The answer to a `wait` that was asked with nothing behind it — nothing of
/// this agent's running and no result nobody has read, and (for everyone but the
/// root, which is exempt from the lock) no other agent holding the machine. One
/// spelling, two roads out of `wait_tool`: the entry guard, and the digest that
/// came back empty. It does *not* claim "no children": a child whose result the
/// model has already read is nothing to wait for, and the entry guard tests the
/// books that can still move, not the rows on the screen.
const NOTHING_TO_WAIT_FOR: &str = "nothing to wait for: nothing of yours is running or unread";

/// Why a `wait` keeps waiting after everything the agent owns has finished:
/// another agent holds the machine.
///
/// The root is exempt from the lock — it commands beside a held one and is
/// *told* — so a sibling's hold is never something it has to wait for, and
/// blocking the human's own hands on it would be the blindness finding H13 was
/// about. Every other agent *is* blocked by it, and `wait` is the one call
/// that can span the hold.
fn machine_wait(actor: &Actor) -> Option<jobs::Held> {
    if actor.id == AgentId::ROOT.0 {
        return None;
    }
    let (agent, command, _) = actor.ctx.registry.held()?;
    (agent != actor.id).then_some(jobs::Held { agent, command })
}

/// How a wait names the machine's holder, cut the way every refusal cuts the
/// command it names ([`jobs::REFUSAL_COMMAND_COLUMNS`]): one spelling, so the
/// sentence the model read from the refusal and the one it reads from the wait
/// cannot disagree about who held it.
fn machine_holding(held: &jobs::Held) -> String {
    format!(
        "#{}'s exclusive command ({})",
        held.agent,
        truncate(&held.command, jobs::REFUSAL_COMMAND_COLUMNS)
    )
}

/// What a wait says when the machine it was waiting on comes free: the fact the
/// model needs next. The sentence does not say "the call that was refused …",
/// because nothing ties a wait to a refusal — an agent that waited for its own
/// results while a sibling benchmarked reads this too — and a model told about a
/// refusal it never made may re-issue a command it had already given up on.
fn machine_free(held: &jobs::Held) -> String {
    format!(
        "the machine is free now — {} ended; the lock is free for your next command",
        machine_holding(held)
    )
}

/// What a wait says when the hold outlasts it. The timeout is not a lost wait:
/// the holder is named, and the one move that can end *that* hold is named with
/// it. A child of this agent is the one holder `control` reaches — its own
/// `stop` lands as `kill_owned`, which kills the job holding the lock — so the
/// sentence says so; a sibling's or a parent's hold is ended by nobody this
/// agent can call, and then the two remaining moves are work without the shell
/// or an honest end to the run.
fn machine_timed_out(held: &jobs::Held, mine: bool) -> String {
    let move_left = if mine {
        format!(
            "it is your own child — `control stop #{}` ends the hold",
            held.agent
        )
    } else {
        // Not "nothing you can call ends it": the hold is not ended, but it is
        // *waited out*, and a hold can outlive many waits (`WAIT_TIMEOUT_SECS`
        // against a job's four hours), so a sentence that named only "do other
        // work" and "give up" would send the model away from the road the
        // refusal just named — and the loop guard lets a repeated wait through
        // for exactly this reason ([`count_round`]).
        "nothing you can call ends it — wait again (a hold can outlive many waits), or do work that \
         needs no shell, or finish this run and say you are blocked"
            .to_string()
    };
    format!(
        "wait timed out — {} still holds the machine; {move_left}",
        machine_holding(held)
    )
}

/// What a wait says when it hands a result over while another agent is still
/// holding the machine. The model asked for the result, not for the lock, so
/// the lock is a fact beside the answer rather than a reason to delay it — and
/// it is the fact the model's next move turns on, because the call that was
/// refused for the lock will be refused again until the wait has waited it out.
fn machine_held(held: &jobs::Held) -> String {
    format!(
        "the machine is still held by {} — wait again to wait it out, or work without the shell",
        machine_holding(held)
    )
}

/// Whether a `wait` has a result nobody has read to hand over: a child whose
/// body the model has not been given yet. A job's line is not here — it was
/// folded into the transcript when the job ended (`note_job`), so handing it
/// over again is a recap, not news.
fn unread_result(state: &ActorState) -> bool {
    state.children.keys().any(|id| state.unread(*id))
}

fn wait_tool(actor: &Actor, state: &mut ActorState, cancel: &AtomicBool) -> Result<String, String> {
    // `wait` has no arguments: "everything I own has finished" is the only
    // thing the call can mean now (finding H15 — a model that reasoned
    // `ids`/`all`/`timeout` wrong waited nine minutes on a child that was
    // already dead). The release rule is the whole name.
    //
    // What this call can still be for: something of its own — running, or a
    // result the transcript is owed — or, for a subagent, the machine, which
    // another agent's exclusive command is holding. The machine is the reason
    // the lock refusal points at (see [`machine_wait`]), so "nothing to wait
    // for" is only the truth once all of them are answered.
    // A read child is still something to wait for — it can run again, and its
    // digest is the answer — but a read job is not: its line is delivered once
    // and never recapped ([`wait_digest`]), so counting it here would answer a
    // wait with the recap instead of the truth that nothing is left.
    let owned = !state.children.is_empty()
        || !state.running_jobs.is_empty()
        || state
            .done_jobs
            .keys()
            .any(|job| !state.delivered_jobs.contains(job));
    let mut holding = machine_wait(actor);
    if !owned && holding.is_none() {
        return Ok(NOTHING_TO_WAIT_FOR.to_string());
    }
    let clock = actor.ctx.clock.as_ref();
    let deadline = clock.now() + Duration::from_secs(WAIT_TIMEOUT_SECS);
    loop {
        match wait_tick(actor, state, cancel, |state| {
            // What the wait was for, said the way it is: a wait the *machine*
            // is keeping alive has nothing of this agent's running, and "your
            // work is still running" was the false half of this sentence in
            // exactly the case the lock road creates.
            if !in_flight(state).is_empty() {
                "your work is still running; ".to_string()
            } else {
                match machine_wait(actor) {
                    Some(held) => format!("{} still holds the machine; ", machine_holding(&held)),
                    None => String::new(),
                }
            }
        }) {
            Tick::Go => {}
            Tick::Cancelled => return Err(CANCELLED.to_string()),
            Tick::Answer(answer) => return Ok(answer),
        }
        let running = in_flight(state);
        if running.is_empty() {
            // A result nobody has read comes first, whatever the machine is
            // doing: waiting is what a model does when it wants a result, and
            // parking one behind a sibling's benchmark is the blindness H13 is
            // about. Everything else a digest would carry — an already-read
            // body, a job's line — is a recap of something the transcript
            // already holds, so it is not a reason to refuse to wait.
            if unread_result(state) {
                let answers = wait_digest(actor, state, false).join("\n");
                return Ok(match machine_wait(actor) {
                    Some(held) => format!("{answers}\n{}", machine_held(&held)),
                    None => answers,
                });
            }
            match machine_wait(actor) {
                Some(held) => {
                    if clock.now() >= deadline {
                        let mine = state.children.contains_key(&held.agent);
                        return Ok(with_digest(actor, state, machine_timed_out(&held, mine)));
                    }
                    holding = Some(held);
                }
                None => {
                    // The machine let go, or was never the wait's business:
                    // hand over whatever the call has, plus the fact the
                    // refused command can run when the wait is what freed it.
                    let answers = wait_digest(actor, state, false);
                    return Ok(match (answers.is_empty(), holding.take()) {
                        (false, Some(held)) => {
                            format!("{}\n{}", answers.join("\n"), machine_free(&held))
                        }
                        (false, None) => answers.join("\n"),
                        (true, Some(held)) => machine_free(&held),
                        (true, None) => NOTHING_TO_WAIT_FOR.to_string(),
                    });
                }
            }
        } else if clock.now() >= deadline {
            // What is known is returned, and what is not is named: a wait that
            // timed out is not a wait that lost the results. Only the *unread*
            // ones travel — a result the model has already read is the past,
            // and a timeout is not the moment to report it as news (H15).
            let note = format!("wait timed out — {} still running", running.join(", "));
            return Ok(with_digest(actor, state, note));
        }
        // The wait slept: it asked the world to move on and the world said "not
        // yet", so the loop guard must not read the next identical `wait` as a
        // repeat of this one (see [`count_round`]). Set on the way into the
        // sleep rather than on the way out, because the answer that ends it —
        // the deadline, the release, a message — all of them follow a sleep.
        state.waited = true;
        clock.sleep(Duration::from_millis(50));
    }
}

/// What one tick at the top of a wait found. The two waits — everything, and
/// one named target — share the two rules that outrank the wait itself: a
/// cancellation stops the run, and words said to the agent must not sit behind
/// a call that can last ten minutes. They differ in what they block *on*, and
/// in what a result does to them.
enum Tick {
    /// Nothing outranks the wait: carry on.
    Go,
    /// A `Stop`/`Shutdown` arrived; the run ends.
    Cancelled,
    /// Something was said to this agent, and this is the answer.
    Answer(String),
}

/// One tick of either wait: take in whatever the mailbox holds, and stop for
/// the two things that outrank the wait itself. `still` says what the wait was
/// for — `wait_tool`'s work or machine, `wait_on_tool`'s target — which is the
/// half of the interrupted sentence only the caller knows.
fn wait_tick(
    actor: &Actor,
    state: &mut ActorState,
    cancel: &AtomicBool,
    still: impl FnOnce(&ActorState) -> String,
) -> Tick {
    // This is the one tool that blocks for minutes, so it is also the one
    // that must notice a cancellation (and a completion) promptly.
    drain_signals(actor, cancel, state);
    if cancel.load(Ordering::SeqCst) {
        return Tick::Cancelled;
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
        return Tick::Answer(format!(
            "interrupted — {who} while you waited; it is in your transcript. Answer it; \
             {}use wait again when you need it.",
            still(state)
        ));
    }
    Tick::Go
}

/// `wait({ on })`: wait for one thing while the rest runs (finding H34). The
/// shape comes from the model's own report — `wait` blocks on everything it
/// owns, `status` is a listing, and a `sleep` was the only "look at just this
/// one" it could find.
///
/// What it blocks *on* is the target alone: the rest of the books run on, and
/// the machine lock is never this call's business (bare `wait` remains the one
/// road back the lock refusal names). What it does *not* narrow is its
/// attention: a cancellation, a message, and above all a result nobody has
/// read — a child that failed, a job that ended — all end the wait and are
/// handed over, so a targeted wait can never sit on news for ten minutes.
fn wait_on_tool(
    actor: &Actor,
    state: &mut ActorState,
    cancel: &AtomicBool,
    target: Target,
) -> Result<String, String> {
    if !target.owned(state) {
        return Err(target.unknown());
    }
    let label = target.label();
    let clock = actor.ctx.clock.as_ref();
    let deadline = clock.now() + Duration::from_secs(WAIT_TIMEOUT_SECS);
    loop {
        match wait_tick(actor, state, cancel, |state| {
            if target.running(state) {
                format!("{label} is still running; ")
            } else {
                String::new()
            }
        }) {
            Tick::Go => {}
            Tick::Cancelled => return Err(CANCELLED.to_string()),
            Tick::Answer(answer) => return Ok(answer),
        }
        // The target is what the call named, so its result is the answer even
        // when the model has read it before: a named target may say "already
        // read" — the once-only rule is about unsolicited replays (H34), not
        // about answering a question.
        if target.ready(state) && !target.running(state) {
            let mut answers = wait_digest(actor, state, true);
            if answers.is_empty() {
                answers.push(target.read_answer(state));
            }
            return Ok(answers.join("\n"));
        }
        // And anything else that needs attention: a result nobody has read
        // ends the wait whatever the target is doing, with the target's own
        // state named beside it so the answer can never be mistaken for its.
        let news = wait_digest(actor, state, true);
        if !news.is_empty() {
            return Ok(if target.running(state) {
                format!("{}\n{label} is still running", news.join("\n"))
            } else {
                news.join("\n")
            });
        }
        if !target.running(state) {
            // Neither running nor holding a result: the books cannot move, and
            // a wait here would be ten minutes spent on a row that will never
            // change (the listing may still call a seeded child running — H30).
            return Ok(format!(
                "nothing to wait for: {label} is not running and has no result"
            ));
        }
        if clock.now() >= deadline {
            return Ok(format!("wait timed out — {label} still running"));
        }
        state.waited = true;
        clock.sleep(Duration::from_millis(50));
    }
}

/// A timeout's answer: whatever results the wait has not already handed over,
/// then the sentence that says what is still going on. The two timeout roads —
/// its own work, and the machine — read the same, so a model that gets one
/// learns the same shape.
fn with_digest(actor: &Actor, state: &mut ActorState, note: String) -> String {
    let answers = wait_digest(actor, state, true);
    if answers.is_empty() {
        note
    } else {
        format!("{}\n{note}", answers.join("\n"))
    }
}

/// Everything this agent owns that is still running, as the labels the model
/// reads: its children (`#2`) and its jobs (`#c2`). One `wait` means all of it —
/// the two id spaces are separate, so a child and a job may share a number and
/// both must be counted — and this is the set that keeps blocking.
fn in_flight(state: &ActorState) -> Vec<String> {
    let mut out = Vec::new();
    let mut children: Vec<u64> = state
        .children
        .keys()
        .copied()
        .filter(|id| state.running.contains(id))
        .collect();
    children.sort_unstable();
    out.extend(children.into_iter().map(|id| format!("#{id}")));
    let mut jobs: Vec<JobId> = state.running_jobs.iter().copied().collect();
    jobs.sort_unstable();
    out.extend(jobs.into_iter().map(|job| job.to_string()));
    out
}

/// The digest a repeat `wait` answers with: the same sentence the listing
/// carries, plus the mark that it is not news.
fn already_read(digest: &str) -> String {
    format!("{digest} (already read — no new run since)")
}

/// Every result this agent has, in id order: children first, then jobs. A
/// child's result nobody has read comes in full and is marked read; one already
/// read comes as its digest, never the body again, and never the past reported
/// as news (finding H15/B26). A job's report is its line, handed over once: a
/// job ends once and cannot run again, so a repeat `wait` has no digest to give
/// it — the line stays in the transcript, and the listing still shows the job.
///
/// `fresh_only` is the timeout's answer: only results nobody has read, because
/// the wait did not finish and a digest of something already answered is not
/// what the model is waiting for.
fn wait_digest(actor: &Actor, state: &mut ActorState, fresh_only: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut children: Vec<u64> = state
        .completed
        .keys()
        .copied()
        .filter(|id| state.children.contains_key(id))
        .collect();
    children.sort_unstable();
    for id in children {
        if fresh_only && !state.unread(id) {
            continue;
        }
        let Some(completion) = state.completed.get(&id).cloned() else {
            continue;
        };
        let digest = completion.outcome.digest(id);
        let (body, fresh) = state.record_child(id, completion.run, completion.outcome);
        if fresh {
            actor
                .ctx
                .emit(actor.id, AgentEvent::ResultRead { child: id });
            out.push(body);
        } else {
            out.push(already_read(&digest));
        }
    }
    let mut jobs: Vec<JobId> = state.done_jobs.keys().copied().collect();
    jobs.sort_unstable();
    for id in jobs {
        if let Some(report) = state.done_jobs.get(&id).cloned() {
            if let Some(line) = state.record_job(id, report.line, report.news) {
                out.push(line);
            }
        }
    }
    out
}

/// `status`: what this agent's children *and* jobs are doing, in one listing.
///
/// The two used to be two calls (`agent_status`, `command_status`), which is
/// the wrong shape for the question a model actually has — "what have I got in
/// flight?" — and made it poll both. A listing is not a delivery: each child's
/// outcome is a digest, each job's is its current line, `✉` marks the results
/// nobody has read, and `wait` is what hands them over.
fn status_tool(actor: &Actor, state: &ActorState) -> Result<String, String> {
    let jobs = actor.ctx.registry.status_for(actor.id);
    let mut sections = Vec::new();
    if !state.children.is_empty() {
        sections.push(format!("agents:\n{}", child_listing(state)));
    }
    // `None` is "no jobs": a third section, not a string to compare against —
    // a job really named `no jobs` used to be able to hide itself here.
    if let Some(jobs) = jobs {
        sections.push(format!("jobs:\n{jobs}"));
    }
    if sections.is_empty() {
        return Ok("no children and no jobs".to_string());
    }
    Ok(sections.join("\n"))
}

/// The child half of `status`: one line per child, in id order.
fn child_listing(state: &ActorState) -> String {
    let mut lines = Vec::new();
    let mut ids: Vec<u64> = state.children.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        // A child that is running again after its last report is *running*: the
        // recorded outcome is history, and printing it made a resumed child
        // read as stopped while it worked (audit row 1). An isolated child
        // works on the branch its id derives (the same `mush/<id>` every other
        // surface names), so a parent with several children can tell which is
        // which; a shared child has no branch of its own, and inventing a
        // second name for it here is not this listing's to do.
        if state.running.contains(&id) {
            let on = if state.shared.contains(&id) {
                String::new()
            } else {
                format!(" on {}", git::branch_name(id))
            };
            lines.push(format!("#{id} ◐ running{on}"));
            continue;
        }
        // The same `✉` the tree rows carry (H4): a result nobody has read.
        let unread = if state.unread(id) { "✉ " } else { "" };
        // Where the recorded run left its worktree, when its actor sent that
        // fact: the branch and whether the work is committed, which is what a
        // parent deciding whether to merge is missing (finding H1). Paired by
        // run, so an older branch never reads as the newer run's work.
        let work = state.work_for(id).map(Work::digest).unwrap_or_default();
        match state.outcome(id) {
            // One sentence per ending, and the ending owns it: the listing
            // composes only its marks around what `digest` says — the `✉` of a
            // result nobody has read, and the work fact — for every variant.
            // Each state kept its own sentence here once, which is how a
            // stopped child came to read one way to a listing and another to a
            // `wait` (finding H2).
            Some(outcome) => lines.push(format!("{unread}{}{work}", outcome.digest(id))),
            None => lines.push(format!("#{id} ◐ running")),
        }
    }
    lines.join("\n")
}

/// `control`: stop or message one thing this agent owns — a child or a job.
///
/// The target is named the way `status` lists it, because the two id spaces are
/// separate: a child is `#2` (and `2`), a job is `#c2` (and `c2`). An integer id
/// could not tell a child #2 from a job #c2, and both can be this agent's at
/// once, so the label carries which.
fn control_tool(actor: &Actor, state: &mut ActorState, args: &Value) -> Result<String, String> {
    let target = tools::arg_string(args, "id")?;
    let action = tools::arg_string(args, "action")?;
    match parse_target(&target)? {
        Target::Job(id) => match action.as_str() {
            "stop" => actor.ctx.registry.stop(actor.id, id),
            other => Err(format!(
                "unknown action `{other}` for a job ({id}) — a job can only be stopped"
            )),
        },
        Target::Agent(id) => match action.as_str() {
            "stop" => stop_agent(actor, state, id),
            "message" => message_agent(actor, state, args, id),
            other => Err(format!(
                "unknown action `{other}` for agent #{id} (stop or message)"
            )),
        },
    }
}

/// A `control` target, parsed from what `status` printed.
enum Target {
    Agent(u64),
    Job(JobId),
}

impl Target {
    /// The name `status` prints, so every answer about a target can carry it.
    fn label(&self) -> String {
        match self {
            Target::Agent(id) => format!("#{id}"),
            Target::Job(id) => id.to_string(),
        }
    }

    /// Whether this label names something of *this* agent's at all. A
    /// `control` target is looked up when the action runs and the sentence it
    /// gets is the action's; a wait must refuse before it sleeps.
    fn owned(&self, state: &ActorState) -> bool {
        match self {
            Target::Agent(id) => state.children.contains_key(id),
            Target::Job(id) => state.running_jobs.contains(id) || state.done_jobs.contains_key(id),
        }
    }

    /// Whether the target is working right now. A child that was resumed reads
    /// as running even though an older outcome is still recorded, and that
    /// outcome is history — the listing says the same (audit row 1).
    fn running(&self, state: &ActorState) -> bool {
        match self {
            Target::Agent(id) => state.running.contains(id),
            Target::Job(id) => state.running_jobs.contains(id),
        }
    }

    /// Whether the target has a result to hand over.
    fn ready(&self, state: &ActorState) -> bool {
        match self {
            Target::Agent(id) => state.completed.contains_key(id),
            Target::Job(id) => state.done_jobs.contains_key(id),
        }
    }

    /// The refusal for a target this agent owns nothing under — the sentences
    /// `control` gives, because it is the same mistake.
    fn unknown(&self) -> String {
        match self {
            Target::Agent(id) => unknown_child(*id),
            Target::Job(id) => jobs::unknown_job(*id),
        }
    }

    /// The answer about a result the model has already read: the child's
    /// digest or the job's line, marked. It is named, so it is not a recap.
    fn read_answer(&self, state: &ActorState) -> String {
        match self {
            Target::Agent(id) => match state.completed.get(id) {
                Some(completion) => already_read(&completion.outcome.digest(*id)),
                None => NOTHING_TO_WAIT_FOR.to_string(),
            },
            Target::Job(id) => match state.done_jobs.get(id) {
                Some(report) => already_read(&report.line),
                None => NOTHING_TO_WAIT_FOR.to_string(),
            },
        }
    }
}

/// `wait`'s one optional argument: the single target the call is narrowed to,
/// named the way `status` prints it (`2` a child, `c2` a job), or nothing,
/// which is what the call always meant. A wrong *type* is refused rather than
/// ignored: H15's trap was a shape a model could reason into another meaning,
/// and a list here would leave "everything" as a silent default.
fn wait_target(args: &Value) -> Result<Option<Target>, String> {
    match args.get("on") {
        None => Ok(None),
        Some(Value::String(raw)) => Ok(Some(parse_target(raw)?)),
        Some(other) => Err(format!(
            "`on` must be one target as status names it (`2` a child, `c2` a job); got {other}"
        )),
    }
}

/// The refusal for a child this agent does not own — one sentence for the
/// three callers that can be handed an id that is not theirs (`control`'s two
/// roads and a targeted `wait`).
fn unknown_child(id: u64) -> String {
    format!("no such child agent #{id} — status lists yours")
}

/// Read `control`'s `id`: `#c2`/`c2` names a job, `#2`/`2` a child agent. The
/// leading `#` is optional because `status` prints one and a model often copies
/// it; the `c` is not, because a bare number could be either.
fn parse_target(raw: &str) -> Result<Target, String> {
    let text = raw.trim().trim_start_matches('#');
    match text.strip_prefix('c') {
        Some(digits) => digits.parse::<u64>().map(|id| Target::Job(JobId(id))),
        None => text.parse::<u64>().map(Target::Agent),
    }
    .map_err(|_| {
        format!("`{raw}` is not a target; status names one as `2` (a child) or `c2` (a job)")
    })
}

/// The child a human's own message was aimed at is not there to take it: the
/// UI reached for it and found no node at all, so there is nothing to revive
/// (`App::deliver_to_actor`). A *parent's* `control` no longer lands here: the
/// mailbox it holds being empty is a parked child, which the UI can wake
/// ([`AgentEvent::ChildAsleep`]). Takes the typed id, so the sentence's `#`
/// comes from [`AgentId`]'s `Display` alone.
pub(crate) fn gone(id: AgentId) -> String {
    format!("agent {id} is gone")
}

/// Stop a child this agent owns. Stopping is not finishing: the child keeps its
/// context and work, and a later `control message` resumes it.
fn stop_agent(actor: &Actor, state: &mut ActorState, id: u64) -> Result<String, String> {
    let Some(cmd) = state.children.get(&id) else {
        return Err(unknown_child(id));
    };
    // An empty mailbox is a child whose *actor* is gone, never a child that is
    // gone: parking reclaims the thread of a finished child and leaves the row
    // and the transcript the human is reading (finding H18). The command goes to
    // the UI, the only hand that can rebuild the actor a Stop needs — and the
    // revival is what takes it, since a parked child is at rest by the window's
    // own condition.
    //
    // A parent's stop says so (`Stop::Parent`): the child's line is read by this
    // very agent, and "you stopped it" is the one reading that tells a parent
    // what it did rather than what happened to it.
    match cmd.send(AgentMsg::Stop(Stop::Parent)) {
        Ok(()) => Ok(format!("stopping agent #{id}")),
        Err(_) => {
            hand_to_ui(actor, id, AgentMsg::Stop(Stop::Parent));
            Ok(format!(
                "stopping agent #{id} — its actor was parked, so mush is waking one to take the stop"
            ))
        }
    }
}

/// Hand the UI a command the parent could not deliver, so that a parked child
/// can be woken to take it.
///
/// A mailbox with no actor behind it used to be read as "the child is gone",
/// which is the one thing it does not say: parking ends a finished child's
/// *thread* and leaves its node, its id and its transcript exactly where they
/// were (`App::park_history`). The parent holds no transcript and so cannot
/// revive — the UI can, and delivers through the same door a human's own
/// message uses (`App::deliver_to_actor`, finding H18). What the reply says is
/// the caller's: this only makes sure the command is not lost on the way there.
fn hand_to_ui(actor: &Actor, child: u64, command: AgentMsg) {
    actor
        .ctx
        .emit(actor.id, AgentEvent::ChildAsleep { child, command });
}

/// Message a child this agent owns. The words resume an idle child, so the
/// parent's own book says it is running: a wait must not answer the old result,
/// and the shared-workspace guard must see it (audit row 1).
///
/// A landed child's actor is still alive but its worktree is gone, and that
/// actor drops a steer on the floor with a `Notice` only the UI sees — so a
/// reply promising a resume would leave the parent waiting for a result that
/// can never arrive. The human's own path refuses the same message up front
/// (`App::worktree_gone`); this is the parent's half of that rule, in the words
/// the child itself reports for a message that reached a gone worktree.
///
/// A *parked* child is the other shape a missing actor takes, and there the
/// answer is the opposite one: the words are handed to the UI, which wakes the
/// child to take them ([`hand_to_ui`], finding H18).
fn message_agent(
    actor: &Actor,
    state: &mut ActorState,
    args: &Value,
    id: u64,
) -> Result<String, String> {
    let Some(cmd) = state.children.get(&id) else {
        return Err(unknown_child(id));
    };
    // An isolated child's worktree is where a run would write; a *shared* child
    // has none of its own and runs in this workspace, which is still here.
    if !state.shared.contains(&id) && !git::worktree_path(&actor.ctx.root, id).exists() {
        return Err(worktree_gone_line(id));
    }
    // Whether the words are read *now* or at the child's next message boundary
    // is the parent's own book (`running`), and the reply says which: "messaged
    // agent #N" claimed delivery with no way to tell a child that resumes from
    // one that is mid-run (finding H5).
    let at_rest = !state.running.contains(&id);
    let text = tools::arg_string(args, "text")?;
    let sent = cmd.send(AgentMsg::Steer(text.clone()));
    match sent {
        Ok(()) if at_rest => {
            // The words resume the child, so the parent's own books say it is
            // running: a wait must not answer the old result, and the
            // shared-workspace guard must see it (audit row 1).
            state.running.insert(id);
            // And the *tree's* row still reads "at rest" until the child's own
            // `Running` event lands: one `tick` in that window is a
            // `park_history` whose `Shutdown` cancels this very run. The mark
            // travels now, with the send, because the send is the fact.
            actor
                .ctx
                .emit(actor.id, AgentEvent::ChildResumed { child: id });
            Ok(format!(
                "messaged agent #{id} — it was at rest, so this resumes it"
            ))
        }
        Ok(()) => Ok(format!(
            "messaged agent #{id} — it is mid-run, so it reads this at its next step"
        )),
        Err(_) => {
            // No actor behind the mailbox: a parked child, whose thread the UI
            // reclaimed and whose transcript is the one on screen (finding H18).
            // "Gone" was the one thing the empty mailbox did not say, and it
            // made the parent give up on a child the human was looking at — so
            // the words go to the UI, the hand that can rebuild the actor they
            // need, and the books follow the words rather than the mailbox they
            // bounced off: an at-rest child is resumed by them exactly as the
            // branch above resumes one, so a `wait` must not answer the result
            // of the run that ended before them (audit row 1).
            hand_to_ui(actor, id, AgentMsg::Steer(text));
            state.running.insert(id);
            Ok(format!(
                "messaged agent #{id} — its actor was parked, so mush is waking one: this resumes it"
            ))
        }
    }
}

/// Commit whatever an isolated agent left in its worktree, so the branch the
/// row names actually carries the work. Returns the
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

/// `read_file`: a window of a text file, or an image.
///
/// It takes no lock and runs no process, which is what makes it the read that
/// survives another agent's exclusive command — and the reason it exists at all
/// after the six-tool cut assumed the shell would always be there to read. It is
/// also the one road an image can travel by: a shell hands back bytes as text,
/// and a picture is not text.
fn read_tool(actor: &Actor, args: &Value) -> Result<ToolOutput, String> {
    let path = tools::arg_string(args, "path")?;
    if let Some(image) = actor.ws.read_image(&path)? {
        // Vision is a per-model fact ([`Config::model`]'s row in the provider
        // table), and an image sent to a model that cannot see it is a rejected
        // request — a whole turn, and the human's money. The refusal names what
        // to do instead, because "this model cannot see" is not something the
        // model can change.
        let model = actor.ctx.cfg.config()?.model;
        if !vision_capable(&model) {
            let format = image.mime.strip_prefix("image/").unwrap_or(&image.mime);
            return Err(format!(
                "{path} is a {format} image ({} bytes), and `{model}` is not a model mush knows \
                 to accept images — so its bytes cannot travel. Work from the file's path, and if \
                 the picture itself is what the task turns on, say so in your summary: a model \
                 whose row documents vision can read it",
                image.bytes.len()
            ));
        }
        let format = image.mime.strip_prefix("image/").unwrap_or(&image.mime);
        // The text is a label for the transcript, not a description of the
        // picture: the bytes are what the model looks at. It is what survives
        // when the session writer sheds them, so it must say which file they
        // came from.
        let text = format!(
            "read {path} — a {format} image, {} bytes",
            image.bytes.len()
        );
        return Ok(ToolOutput {
            text,
            images: vec![image],
        });
    }
    let offset = tools::arg_usize(args, "offset", 1)?;
    let limit = tools::arg_usize(args, "limit", usize::MAX)?;
    actor
        .ws
        .read_window(&path, offset, limit, result_cap(actor))
        .map(ToolOutput::from)
}

/// `write_file`: create or replace a whole file. The answer is one line naming
/// what changed, because the model already knows what it wrote.
fn write_tool(actor: &Actor, args: &Value) -> Result<String, String> {
    let path = tools::arg_string(args, "path")?;
    let content = tools::arg_string(args, "content")?;
    let cap = result_cap(actor);
    if content.len() > cap {
        return Err(format!(
            "content is {} bytes — over the {cap}-byte cap on one write; write the first part, \
             then extend it with edit_file",
            content.len()
        ));
    }
    // Existence, not readability. A file this tool is about to replace is a
    // file whether or not its bytes are text: answering "(new)" over a binary
    // file — an image, a blob — records a history fact that is simply false.
    let existed = actor.ws.exists(&path);
    let before = actor
        .ws
        .read_file(&path)
        .ok()
        .map(|text| text.lines().count());
    actor.ws.write_file(&path, &content)?;
    let after = content.lines().count();
    // "1 lines" is the kind of small wrongness a model copies into its own
    // summary, so the count is spelled.
    let after = if after == 1 {
        "1 line".to_string()
    } else {
        format!("{after} lines")
    };
    Ok(match (existed, before) {
        (_, Some(before)) => format!("wrote {path} — {before} → {after}"),
        (true, None) => format!("wrote {path} — {after} (replaced a file that is not text)"),
        (false, None) => format!("wrote {path} — {after} (new)"),
    })
}

/// `list_files`: the workspace's files under a path, one per line.
fn list_tool(actor: &Actor, args: &Value) -> Result<String, String> {
    let rel = tools::arg_path(args, "path")?;
    let (files, truncated) = actor.ws.list_files(&rel, LIST_LIMIT)?;
    if files.is_empty() {
        return Ok(format!("{}: no files", shown_path(&rel)));
    }
    let mut out = files.join("\n");
    if truncated {
        out.push_str(&format!(
            "\n[mush: the first {LIST_LIMIT} files — list a narrower path to see the rest]"
        ));
    }
    Ok(truncate_for_model(out, result_cap(actor)))
}

/// `search`: a literal string in the workspace's text files.
fn search_tool(actor: &Actor, args: &Value) -> Result<String, String> {
    let pattern = tools::arg_string(args, "pattern")?;
    let rel = tools::arg_path(args, "path")?;
    let ignore_case = tools::arg_bool(args, "ignore_case", false)?;
    let found = actor.ws.search(&pattern, &rel, ignore_case, SEARCH_LIMIT)?;
    if found.matches.is_empty() {
        // A miss that never opened every file is not a miss. "No match" is
        // what a model reads as "it is not there", so the files the walk
        // skipped are named along with the road to them.
        let under = shown_path(&rel);
        return Ok(if found.skipped == 0 {
            format!("no match for `{pattern}` under {under}")
        } else {
            format!(
                "no match for `{pattern}` under {under} — {}",
                skipped_note(found.skipped)
            )
        });
    }
    let mut out = found.matches.join("\n");
    let mut notes = Vec::new();
    if found.more {
        notes.push(format!(
            "the first {SEARCH_LIMIT} matches — narrow the pattern or the path"
        ));
    }
    if found.skipped > 0 {
        notes.push(skipped_note(found.skipped));
    }
    if !notes.is_empty() {
        out.push_str(&format!("\n[mush: {}]", notes.join("; ")));
    }
    Ok(truncate_for_model(out, result_cap(actor)))
}

/// The files a search did not open, as a sentence. One function for both
/// numbers, because English will not take one: the note is not decoration — it
/// is the difference between "the symbol is not here" and "I did not look".
fn skipped_note(skipped: usize) -> String {
    let cap = SEARCH_FILE_CAP / (1024 * 1024);
    if skipped == 1 {
        format!("1 file was skipped (binary or over {cap} MB); run_command (`rg`) reads it")
    } else {
        format!(
            "{skipped} files were skipped (binary or over {cap} MB); run_command (`rg`) reads them"
        )
    }
}

/// How many files a listing names, and how many matches a search returns,
/// before saying there are more. Sized to a readable result rather than to the
/// workspace: a listing is a map, not the territory.
const LIST_LIMIT: usize = 400;
const SEARCH_LIMIT: usize = 200;

/// How a path argument is named back to the model: the workspace root has no
/// relative spelling, and "under " with nothing after it reads as a bug.
fn shown_path(rel: &str) -> String {
    if rel.trim().is_empty() {
        "the workspace root".to_string()
    } else {
        format!("`{rel}`")
    }
}

/// `edit_file`: exact and unique replacement, all-or-nothing, one shape.
///
/// It is not a shell line for a reason: `sed -i` has no notion of "exactly
/// once", so a wrong edit is impossible here and a missing or ambiguous match
/// is a refusal the model can correct. The shape is `edits` only — a list, of
/// which a lone edit is the list of one — see [`tools::edits_arg`] for why the
/// second, top-level spelling is gone.
fn edit_tool(ws: &Workspace, args: &Value) -> Result<String, String> {
    let rel = tools::arg_string(args, "path")?;
    let edits = tools::edits_arg(args)?;
    let current = ws.read_file(&rel)?;
    // One read, one write: every edit lands or none do, so the file cannot be
    // left half-changed and the edits see each other's results in order.
    let updated = tools::edit_text_many(&current, &edits, &rel)?;
    ws.write_file(&rel, &updated)?;
    Ok(match edits.len() {
        1 => format!("edited {rel}"),
        count => format!("edited {rel} — {count} edits"),
    })
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
/// can decide to distrust a timing-sensitive result, or wait next time. The
/// command is cut to [`jobs::REFUSAL_COMMAND_COLUMNS`], the bound the refusal
/// sentence beside it uses: one home for how much of a command such a sentence
/// may carry.
fn beside_note(text: String, held: Option<&jobs::Held>) -> String {
    match held {
        None => text,
        Some(held) => format!(
            "{text}\n(ran while #{} held the machine for an exclusive command ({}) — \
             timing-sensitive results from its run may be perturbed)",
            held.agent,
            truncate(&held.command, jobs::REFUSAL_COMMAND_COLUMNS)
        ),
    }
}

/// The sentence a `Refused::Machine` gets in `run_command`, chosen by the road
/// the asker actually took: the root is exempt from the lock and never queues
/// (see [`Refused::root_message`]), a sibling queues for `LOCK_QUEUE` and is
/// refused only when the lock outlasts it ([`Refused::message`]), and a sibling
/// whose *exclusive* claim lost a race — another agent took the lock between
/// the check and the claim — never sat in that queue at all
/// ([`Refused::unqueued_message`]). Choosing by the road, not by the asker
/// alone, is what keeps a refusal from claiming a queue that never happened.
fn machine_refusal(actor: &Actor, held: jobs::Held, queued: bool) -> String {
    let refused = Refused::Machine(held);
    if actor.id == AgentId::ROOT.0 {
        refused.root_message(actor.id)
    } else if queued {
        refused.message(actor.id)
    } else {
        refused.unqueued_message(actor.id)
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
    // prevent. A sibling queues, bounded, instead of refusing on sight. The
    // *holder's* own exclusive call is refused too — by `take_machine`, the
    // only writer of the holder record — because the lock is not re-entrant:
    // a second claim would overwrite the record naming the first one (and the
    // job it became), and the first call's release would then free a machine
    // its job still holds.
    let mut beside: Option<jobs::Held> = None;
    if let Err(held) = registry.machine_free_for(actor.id) {
        if actor.id == AgentId::ROOT.0 {
            if exclusive {
                return Err(ToolError::Refused(machine_refusal(actor, held, false)));
            }
            beside = Some(held);
        } else if let Err(held) = wait_for_machine(actor, cancel) {
            return Err(ToolError::Refused(machine_refusal(actor, held, true)));
        }
    }
    if detach && !registry.has_room() {
        return Err(ToolError::Refused(Refused::Budget.message(actor.id)));
    }
    if exclusive {
        // A refusal here is the holder's own second claim, or a sibling that
        // took the lock between the check above and this line — and the second
        // of those never queued, so it must not read the queued road's words.
        // Both go through the same chooser, with the road named.
        registry
            .take_machine(actor.id, command)
            .map_err(|held| ToolError::Refused(machine_refusal(actor, held, false)))?;
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
) -> Result<JobId, ToolError> {
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
fn detached_line(id: JobId) -> String {
    format!("[still running — detached as {id}; you will be told when it finishes]")
}

/// Hard ceiling on what one command may write to its scratch files. The model
/// only ever sees the first `result_cap` bytes, so a command that gets here is
/// not communicating, it is running away — and it must not fill the disk. The
/// size is checked every few milliseconds (see `wait_bounded`), so a fast writer
/// can overshoot by a few tens of MB before the kill lands. It lives in
/// `crate::jobs` beside the other rule a job and a tool call share.
use crate::jobs::CMD_OUTPUT_LIMIT;

/// The bytes one tool result may carry.
///
/// Every big-text road uses it — a command's output, a file read, a listing, a
/// search — so a result is bounded by the context budget rather than by a fixed
/// number: a quarter of what the history can hold, capped at `CMD_CAP` (see
/// `Config::cmd_cap`). A result that hits it says so and says what to do
/// (`truncate_for_model`), and the file tools' windows are cut to it as they
/// are built, so the sentence names the way on rather than a lost tail.
fn result_cap(actor: &Actor) -> usize {
    actor
        .ctx
        .cfg
        .config()
        .map(|cfg| cfg.cmd_cap())
        .unwrap_or(mush_core::CMD_CAP)
}

/// Why a command stopped running.
enum Ended {
    /// It ended by itself, with this exit code.
    Exited(i32),
    /// A signal killed it — the OOM killer's `9`, the `SIGSEGV` of a crashed
    /// binary — with this signal's number. A variant of its own because a
    /// signal death has no exit code: the `-1` this arm used to hold was a
    /// number no command returns, and it told a crash and an OOM kill apart
    /// from neither (finding B6).
    Signalled(i32),
    /// The platform's status named neither an exit code nor a signal
    /// ([`End::Unknown`]). A state of its own, not the `-1` sentinel
    /// that read as a real exit code (finding H26); no child mush starts
    /// produces such a status, and a seam that is handed one says so rather than
    /// naming a number nothing returned.
    Unknown,
    /// mush stopped it. The reason is [`jobs::Stopped`]'s, not a second copy of
    /// the same three variants: the watcher already decides between them with
    /// `jobs::stopping`, and a fourth reason added there must reach the model's
    /// sentence without a translation table to update in step (refactor R15).
    Stopped(jobs::Stopped),
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
    // this actor's: quitting mush, a `Stop` and Ctrl-N all reach it (finding
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
            // Read what it has written before handing it over. If the registry
            // refuses the job (the budget filled between the decision and now),
            // its refusal kills the process group, and the launch owns the only
            // handle to the output — a report built after that would have
            // nothing to show (audit row 2).
            let (stdout, stderr) = running.output(result_cap(actor));
            match detach_now(
                actor,
                registry,
                jobs::Launch::held(command.to_string(), exclusive, actor.my_tx.clone(), running),
            ) {
                Ok(id) => {
                    state.running_jobs.insert(id);
                    return Ok(detached_line(id));
                }
                Err(ToolError::Refused(why)) => {
                    let mut report = command_report(&stdout, &stderr);
                    report.push_str(&format!(
                        "[ran {}s and could not become a job: {why} — it was stopped; the output above is \
                         what it wrote]",
                        jobs::CMD_DETACH_AFTER.as_secs()
                    ));
                    return Ok(report);
                }
                Err(other) => return Err(other),
            }
        }
    }
    // A kill that arrived from *outside* the watcher — a quit, which is what
    // finding S4 is about, or the registry's half of a `Stop` — must not be
    // reported as the command's own exit: `-1` is a signal nobody asked about.
    let ended = ending(ended, running.stopped());
    let cap = result_cap(actor);
    let (stdout, stderr) = running.output(cap);
    let mut report = command_report(&stdout, &stderr);
    report.push_str(&end_note(
        &ended,
        timeout,
        matches!(detach, Detach::Job { .. }),
        cap,
    ));
    Ok(report)
}

/// The model's sentence for how its command ended: `[exit 0]`, `[cancelled]`,
/// the timeout, the output cap, the signal that killed it.
///
/// One home for the whole translation table, so every reason mush stops a
/// command has exactly one sentence and a new one cannot be added to one
/// translator and missed by another (refactor R15). Pure: the one arm no run
/// reaches — a command handed to the job registry builds no report at all — is
/// read by the unit test beside the others rather than left to chance.
fn end_note(ended: &Ended, timeout: Duration, detachable: bool, cap: usize) -> String {
    match ended {
        Ended::Exited(code) => format!("[exit {code}]"),
        // A signal death is not an exit code, and the sentence says what it is:
        // `[exit -1]` was this arm's spelling for a `SIGSEGV` and for an OOM
        // kill alike, and it read as the command's own doing (finding B6).
        Ended::Signalled(signal) => format!("[killed by signal {signal}]"),
        // Neither a code nor a signal: the state names what is unknown rather
        // than the `-1` sentinel, which read as a code a command can return
        // (finding H26). No child mush starts ends this way — `machine::ended`
        // reads an exit or a death by signal from the statuses `wait` produces —
        // and the arm is honest anyway.
        Ended::Unknown => "[no exit status: neither an exit code nor a signal]".to_string(),
        Ended::Stopped(jobs::Stopped::TimedOut) => {
            let mut note = format!("[timed out after {}s", timeout.as_secs());
            // The one case where "a long command detaches by itself" cannot
            // happen: the machine-wide job budget is full. Saying only "timed
            // out" hid the reason the model was never told about (audit row 2).
            if !detachable {
                note.push_str(&format!(
                    "; the {}-job budget is full, so it could not detach — stop one with \
                     control or wait for one",
                    jobs::MAX_JOBS
                ));
            }
            note.push(']');
            note
        }
        Ended::Stopped(jobs::Stopped::Cancelled) => "[cancelled]".to_string(),
        Ended::Stopped(jobs::Stopped::TooMuchOutput) => {
            format!("[killed: output passed {CMD_OUTPUT_LIMIT} bytes; the first {cap} are above]")
        }
        // A job's ceiling, never a tool call's: a foreground command has no
        // ceiling, because past `CMD_DETACH_AFTER` it is handed to the registry
        // instead. The second arm no run reaches, and here for the same reason
        // as the first: every reason in the table gets exactly one sentence.
        Ended::Stopped(jobs::Stopped::RanTooLong) => format!(
            "[killed: it ran past the {}h ceiling]",
            jobs::JOB_MAX_AGE.as_secs() / 3600
        ),
        // Not an end, and not reachable from `run_shell`: a command that may
        // detach is handed to the registry before a report is built. It used to
        // print the timeout's sentence, which is one thing this cannot be (a
        // detached command was never stopped); it says what would be true
        // instead.
        Ended::Detached => "[mush: the command was handed to the job registry]".to_string(),
    }
}

/// What a command wrote, with no `$ {command}` echo: the tool call is already
/// rendered from the assistant message that made it (`⚙ run_command …`), so
/// printing it here again put the same command in the transcript twice — and,
/// because tool results are stored, in the saved session twice as well.
fn command_report(stdout: &str, stderr: &str) -> String {
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
    report
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
///   Ctrl-N, or the registry's half of a `Stop` — is reported as a cancel. The
///   process died of the signal mush sent it, and a death by signal handed to
///   the model as an exit code (`-1`, once) reads as the command's own doing;
///   this is the arm finding S4's fix needs.
/// - A command that ended by itself keeps its real exit status — a signal death
///   from anyone else's hand included: nobody in mush asked for those.
fn ending(ended: Ended, stopped_from_outside: bool) -> Ended {
    match ended {
        // Either shape a dead command comes back in — its own code, or the
        // signal that killed it — is a cancel when the flag was set from
        // outside the watcher.
        Ended::Exited(_) | Ended::Signalled(_) if stopped_from_outside => {
            Ended::Stopped(jobs::Stopped::Cancelled)
        }
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
            Ok(Some(End::Exited(code))) => return Ok(Ended::Exited(code)),
            Ok(Some(End::Signalled(signal))) => return Ok(Ended::Signalled(signal)),
            Ok(Some(End::Unknown)) => return Ok(Ended::Unknown),
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
            // No ceiling: a foreground command cannot reach one — past
            // `detach_after` it becomes a job, and the job's own thread is what
            // watches it from there.
            None,
            cancel.load(Ordering::SeqCst),
        ) {
            job.kill();
            return Ok(Ended::Stopped(stopped));
        }
        actor.ctx.clock.sleep(jobs::POLL);
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
    // `control {id, action, text?}`, which names none of the three above: the
    // tools a human watching a tree most needs to read are exactly the calls
    // that steer the run, and they rendered as a bare `⚙ control` otherwise
    // (R4's "`⚙ name summarized-args`" vacuous for them). The id is the target
    // and the action is what is being done to it.
    if let Some(id) = args.get("id").and_then(Value::as_str) {
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
    // `status` and `wait` take no arguments: there is nothing to summarize, and
    // the label is the tool's own name alone.
    String::new()
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

    /// The `control` tool carries no path, command or brief, so it would render
    /// as a bare `⚙ control` — for exactly the calls an orchestrator uses to
    /// steer a tree, which is where a human most needs to know *whom* and what.
    #[test]
    fn a_control_call_summarizes_its_target() {
        assert_eq!(
            summarize(&json!({"id": "2", "action": "message", "text": "keep the steps small"})),
            "#2 message \"keep the steps small\""
        );
        assert_eq!(
            summarize(&json!({"id": "c3", "action": "stop"})),
            "#c3 stop",
            "a job's label carries the `c` status printed"
        );
        // A tool with no arguments has nothing to summarize, and says so by
        // summarizing nothing: `status`, `wait`.
        assert_eq!(summarize(&json!({})), "");
    }

    /// A tool label is one line by definition. Truncating a command's raw text
    /// kept its newlines, so a heredoc turned one row into several. The
    /// collapse itself is `mush_core::text::first_line`'s, tested there beside
    /// the other string arithmetic (refactor R11).
    #[test]
    fn a_command_summary_collapses_to_one_line() {
        let label = summarize(&json!({
            "command": "cd /w && python3 - <<'PY'\nimport io\nprint('x')\nPY"
        }));
        assert!(!label.contains('\n'), "{label:?} must be one line");
        assert!(label.starts_with("cd /w && python3"), "{label}");
    }

    /// A nudge parked during a run that was cancelled is already in the UI's
    /// transcript, which echoes every human message. Adopting that transcript
    /// must not deliver the parked copy a second time.
    #[test]
    fn adopting_a_transcript_drops_parked_nudges() {
        let (actor, _mailbox) = test_actor("parked-nudge");
        let mut state = ActorState::default();
        let mut transcript = vec![Message::system("sys")];
        state.deferred.push(AgentMsg::Nudge("said once".into()));

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
                name: "edit_file".into(),
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
    ///
    /// The sentence is written **once**, for all four endings: the listing is
    /// that sentence plus the marks it composes around it (`✉`, and the work
    /// fact when the run left one), and the wait's digest is the same sentence
    /// again — a reader that re-spelled an ending could say something about a
    /// stop that no other surface said.
    #[test]
    fn status_distinguishes_stopped_from_done_and_failed() {
        let (actor, _mailbox) = test_actor("status-marks");
        let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let mut state = ActorState::default();
        let outcomes = [
            Outcome::Stopped(Stop::Human),
            Outcome::CutOff,
            Outcome::Finished("did the thing".into()),
            Outcome::Failed("no route".into()),
        ];
        for (index, outcome) in outcomes.iter().enumerate() {
            let id = index as u64 + 1;
            state.children.insert(id, tx.clone());
            note_completion(&mut state, id, 1, outcome.clone());
        }

        // Nothing has been read, so every line carries the `✉` and no work
        // fact has been sent: the line *is* the outcome's sentence plus that
        // one mark.
        let listing = child_listing(&state);
        let lines: Vec<&str> = listing.lines().collect();
        for (index, outcome) in outcomes.iter().enumerate() {
            let id = index as u64 + 1;
            assert_eq!(
                lines[index],
                format!("✉ {}", outcome.digest(id)),
                "the listing is the outcome's sentence, marked"
            );
        }
        let lines = status_tool(&actor, &state).unwrap();
        assert!(lines.contains("#1 ⊘ stopped"), "a stop is not a ✓: {lines}");
        assert!(lines.contains("#2 ⚠ cut off"), "{lines}");
        assert!(lines.contains("#3 ✓ did the thing"), "{lines}");
        assert!(lines.contains("#4 ✗ no route"), "{lines}");
        // The old shape — a sentinel string leaking into the parent's view —
        // reported a stopped child as a *finished* one.
        assert!(!lines.contains("#1 ✓"), "{lines}");
        assert!(!lines.contains("cancelled"), "{lines}");

        // The work fact is the other mark, hung on the same sentence.
        note_work(
            &mut state,
            1,
            1,
            Work::Clean {
                branch: "mush/1".into(),
            },
        );
        let listing = child_listing(&state);
        let first = listing.lines().next().unwrap();
        assert_eq!(
            first,
            format!(
                "✉ {}{}",
                Outcome::Stopped(Stop::Human).digest(1),
                Work::Clean {
                    branch: "mush/1".into()
                }
                .digest()
            ),
            "the listing adds the work fact to the same sentence"
        );

        // And the other reader: `wait` answers an already-read result with the
        // same digest, so both roads say one thing about one ending.
        for id in 1..=outcomes.len() as u64 {
            state.delivered.insert(id, 1);
        }
        let answers = wait_digest(&actor, &mut state, false);
        for (index, outcome) in outcomes.iter().enumerate() {
            let id = index as u64 + 1;
            assert_eq!(
                answers[index],
                already_read(&outcome.digest(id)),
                "the wait's digest is the same sentence"
            );
        }
    }

    /// A finished isolated child's listing says where its work is and whether
    /// it is committed — the fact a parent deciding whether to merge was
    /// missing (finding H1) — and it is paired with the run it came from, so an
    /// older branch can never ride a newer result.
    #[test]
    fn the_listing_carries_where_the_work_is() {
        let (actor, _mailbox) = test_actor("status-work");
        let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let mut state = ActorState::default();
        state.children.insert(1, tx);
        note_completion(&mut state, 1, 2, Outcome::Finished("wrote it".into()));
        note_work(
            &mut state,
            1,
            2,
            Work::Committed {
                branch: "mush/1".into(),
                revision: "abc1234".into(),
            },
        );
        let lines = status_tool(&actor, &state).unwrap();
        assert!(lines.contains("#1 ✓ wrote it"), "{lines}");
        assert!(lines.contains("committed abc1234 on mush/1"), "{lines}");

        // A newer run has no work fact yet: the older run's branch must not be
        // read as its history.
        note_completion(&mut state, 1, 4, Outcome::Finished("again".into()));
        let lines = status_tool(&actor, &state).unwrap();
        assert!(!lines.contains("committed abc1234"), "{lines}");

        // And when the newer run's own fact arrives, it is the one shown.
        note_work(
            &mut state,
            1,
            4,
            Work::Clean {
                branch: "mush/1".into(),
            },
        );
        let lines = status_tool(&actor, &state).unwrap();
        assert!(lines.contains("mush/1 clean — nothing changed"), "{lines}");
    }

    /// A running child has no outcome to print, and `#N ◐ running` alone does
    /// not tell a parent with three children which is which. An isolated
    /// child's branch is derivable from its id — the same `mush/<id>` every
    /// other surface names — so the listing carries it; a shared child has no
    /// branch of its own and gets no invented one. (The schema's "title" half
    /// lives in the UI tree and is not a fact this listing holds.)
    #[test]
    fn a_running_child_names_the_branch_it_works_on() {
        let (actor, _mailbox) = test_actor("status-running-branch");
        let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let mut state = ActorState::default();
        // #1 is isolated (not in `shared`), #2 runs in this workspace.
        state.children.insert(1, tx.clone());
        state.running.insert(1);
        state.children.insert(2, tx);
        state.running.insert(2);
        state.shared.insert(2);

        let lines = status_tool(&actor, &state).unwrap();
        assert!(lines.contains("#1 ◐ running on mush/1"), "{lines}");
        assert!(
            lines.ends_with("#2 ◐ running"),
            "a shared child has no branch to name: {lines}"
        );
        assert!(!lines.contains("mush/2"), "{lines}");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The work fact is a listing, not a signal: it starts no run and changes
    /// no transcript.
    #[test]
    fn a_work_fact_starts_nothing() {
        let (actor, _mailbox) = test_actor("work-quiet");
        let mut state = ActorState::default();
        let mut transcript = vec![Message::system("you are mush")];
        let folded = absorb(
            &actor,
            &mut state,
            &mut transcript,
            AgentMsg::Work {
                id: 1,
                run: 1,
                work: Work::Committed {
                    branch: "mush/1".into(),
                    revision: "abc1234".into(),
                },
            },
        );
        assert!(matches!(folded, Fold::Idle), "not work to answer");
        assert_eq!(transcript.len(), 1, "nothing is pushed into the transcript");
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
        let stopped = Outcome::Stopped(Stop::Human).line(3);
        assert!(stopped.starts_with("#3 stopped"), "{stopped}");
        // It must not be *formatted* as a done line. (The words "not done" do
        // appear, deliberately: they are what tells the parent it is not one.)
        assert!(!stopped.starts_with("#3 done"), "{stopped}");
        // And it names the hand that stopped it: the parent is being told what
        // happened to a task it delegated, and "the human did it" is not the
        // same fact as "you did it" (a park's line says neither, because mush
        // did it — see `is_news`).
        assert!(stopped.contains("the human stopped"), "{stopped}");
        assert!(
            Outcome::Stopped(Stop::Parent)
                .line(3)
                .contains("you stopped"),
            "a parent's own stop is read by that parent"
        );
        let parked = Outcome::Stopped(Stop::Reclaimed).line(3);
        assert!(parked.contains("mush reclaimed its thread"), "{parked}");
        assert!(
            Outcome::Stopped(Stop::Unrecorded)
                .line(3)
                .contains("the run ended before it finished"),
            "a restored stop names no hand rather than guessing one"
        );
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

    /// A stop is news now, whichever hand asked for it: a parent that delegated a
    /// task is depending on that child for information, and the result is not
    /// coming — so it is woken and told, and the line says who stopped it. The
    /// one stop that still is not news is mush's own: a park reclaims a thread
    /// the window is not using, and waking a parent for memory management would
    /// be waking it for nothing. A finish is news, unchanged.
    #[test]
    fn a_stop_wakes_a_napping_parent_but_a_park_does_not() {
        let (actor, _mailbox) = test_actor("napping");
        let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let mut state = ActorState::default();
        state.children.insert(1, tx.clone());
        let mut messages = vec![Message::system("you are mush")];

        // The human's key.
        assert!(matches!(
            absorb(
                &actor,
                &mut state,
                &mut messages,
                AgentMsg::ChildDone {
                    id: 1,
                    run: 1,
                    outcome: Outcome::Stopped(Stop::Human)
                }
            ),
            Fold::Run
        ));
        let line = messages.last().unwrap().text().to_string();
        assert!(line.contains("#1 stopped"), "{line}");
        assert!(line.contains("the human stopped"), "{line}");

        // The parent's own `control stop` is news too, and reads as its own
        // doing rather than as somebody else's.
        assert!(matches!(
            absorb(
                &actor,
                &mut state,
                &mut messages,
                AgentMsg::ChildDone {
                    id: 1,
                    run: 2,
                    outcome: Outcome::Stopped(Stop::Parent)
                }
            ),
            Fold::Run
        ));
        assert!(messages.last().unwrap().text().contains("you stopped"));

        // A park: the line is in the transcript for the next run, and no run is
        // paid for.
        assert!(matches!(
            absorb(
                &actor,
                &mut state,
                &mut messages,
                AgentMsg::ChildDone {
                    id: 1,
                    run: 3,
                    outcome: Outcome::Stopped(Stop::Reclaimed)
                }
            ),
            Fold::Idle
        ));
        assert!(
            messages.last().unwrap().text().contains("parked"),
            "a park says what it was: {}",
            messages.last().unwrap().text()
        );

        assert!(matches!(
            absorb(
                &actor,
                &mut state,
                &mut messages,
                AgentMsg::ChildDone {
                    id: 1,
                    run: 4,
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
            (Outcome::Stopped(Stop::Human), Committed::Stopped),
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

    /// A first line that ends in `…` was not cut by anyone: the ellipsis is the
    /// brief's own character. Reading it as the cut's mark swallowed the word
    /// before it — `fix the …` came out as `fix the…`, a subject about a
    /// different sentence — which is what a caller guessing at "was this cut?"
    /// from the string itself costs. The cut knows, and now says.
    #[test]
    fn a_brief_that_ends_in_an_ellipsis_keeps_its_words() {
        assert_eq!(
            commit_subject(7, "fix the …", &Outcome::Finished("done".into())),
            "mush #7: fix the …"
        );
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
    /// parent's `control message` puts in a child's transcript are
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

        // Sent the way `control message` sends it. The child runs in this
        // workspace (a shared child): only an isolated child has a worktree of
        // its own that can be gone.
        let (child_tx, child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child_tx);
        state.shared.insert(1);
        let sent = exec_tool(
            &actor,
            &mut state,
            ToolName::Control,
            &json!({ "id": "1", "action": "message", "text": text }),
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
            ToolName::Control,
            &json!({ "id": "1", "action": "message", "text": "keep going" }),
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

    /// A landed child's worktree is gone, and its actor drops a steer with a
    /// `Notice` only the UI sees — so `control message` must refuse instead of
    /// promising a resume the parent would wait `wait`'s whole timeout for. The
    /// refusal uses the words the child itself reports for a message that
    /// reached a gone worktree, so parent and child say one thing about it.
    #[test]
    fn messaging_a_child_whose_worktree_is_gone_is_refused() {
        let (actor, _events, _mailbox) = recording_actor("gone-message");
        let mut state = ActorState::default();
        let (child_tx, child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child_tx);

        let refused = exec_tool(
            &actor,
            &mut state,
            ToolName::Control,
            &json!({ "id": "1", "action": "message", "text": "carry on" }),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        let ToolError::Failed(refused) = refused else {
            panic!("a message that cannot run is a failure, not a retry-later refusal");
        };
        assert!(refused.contains("worktree is gone"), "{refused}");
        assert!(refused.contains("did not run"), "{refused}");
        assert!(
            child_rx.try_recv().is_err(),
            "nothing is delivered to a child that cannot run it"
        );
        assert!(
            !state.running.contains(&1),
            "and the parent's books do not claim the run that will never happen"
        );

        // A shared child has no worktree of its own: the same message lands,
        // and the books follow it.
        state.shared.insert(1);
        let sent = exec_tool(
            &actor,
            &mut state,
            ToolName::Control,
            &json!({ "id": "1", "action": "message", "text": "carry on" }),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(sent.contains("resumes it"), "{sent}");
        assert!(state.running.contains(&1));
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A send that *resumes* a child the tree still reads as at rest travels
    /// with its mark: the child's own `Running` event is a moment behind, and
    /// one `tick` in between is a `park_history` whose `Shutdown` would cancel
    /// the run the words just started (§8.21). The parent's books are one
    /// reader of that fact; the tree's row is the other.
    #[test]
    fn resuming_a_child_reports_the_mark_the_tree_needs() {
        let (actor, events, _mailbox) = recording_actor("resume-mark");
        let mut state = ActorState::default();
        let (child_tx, child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child_tx);
        state.shared.insert(1);

        exec_tool(
            &actor,
            &mut state,
            ToolName::Control,
            &json!({ "id": "1", "action": "message", "text": "carry on" }),
            &AtomicBool::new(false),
        )
        .unwrap();

        assert!(
            matches!(child_rx.try_recv(), Ok(AgentMsg::Steer(words)) if words == "carry on"),
            "the words land in the child's mailbox"
        );
        assert!(
            events
                .events_for(AgentId(7))
                .iter()
                .any(|event| matches!(event, AgentEvent::ChildResumed { child: 1 })),
            "and so does the mark the tree's row needs: {:?}",
            events.events_for(AgentId(7))
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A parked child's mailbox is still there and its actor is not: parking
    /// ends the thread and nothing else (`App::park_history`). The parent holds
    /// no transcript, so it cannot rebuild the actor its words need — the UI
    /// can — and until this travelled there the model was told its child was
    /// *gone* about a row and a transcript the human could see (finding H18).
    #[test]
    fn a_message_to_a_parked_child_travels_to_the_ui_that_can_wake_it() {
        let (actor, events, _mailbox) = recording_actor("parked-message");
        let mut state = ActorState::default();
        // Exactly what parking leaves: the mailbox the books hold, nobody at the
        // other end of it. A shared child, so the worktree check above it is out
        // of the way.
        let (child_tx, parked) = crossbeam_channel::unbounded();
        drop(parked);
        state.children.insert(1, child_tx);
        state.shared.insert(1);

        let sent = exec_tool(
            &actor,
            &mut state,
            ToolName::Control,
            &json!({ "id": "1", "action": "message", "text": "carry on" }),
            &AtomicBool::new(false),
        )
        .unwrap();

        assert!(!sent.contains("gone"), "{sent}");
        assert!(sent.contains("resumes it"), "{sent}");
        // The words themselves are what travels, not a sentence about them: the
        // actor that resumes is rebuilt from the UI's copy of the transcript,
        // and a line the UI could not hand over would be a resume that never
        // happened.
        let handed: Vec<(u64, AgentMsg)> = events
            .events_for(AgentId(7))
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::ChildAsleep { child, command } => Some((child, command)),
                _ => None,
            })
            .collect();
        assert_eq!(handed.len(), 1, "one command, one hand-over: {handed:?}");
        assert_eq!(handed[0].0, 1, "for the child the parent aimed at");
        assert!(
            matches!(&handed[0].1, AgentMsg::Steer(words) if words == "carry on"),
            "and as steering, the way a live child gets it"
        );
        // The books follow the words: they resume the child, so a `wait` must
        // not answer the result of the run that ended before them.
        assert!(state.running.contains(&1));
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The other half: a `stop` aimed at a parked child reaches it too. The
    /// child is at rest by parking's own condition, so nothing is cancelled —
    /// but a gone sentence here is just as false, and it leaves the parent
    /// believing the child it was told to stop no longer exists (finding H18).
    #[test]
    fn a_stop_aimed_at_a_parked_child_is_handed_over_rather_than_called_gone() {
        let (actor, events, _mailbox) = recording_actor("parked-stop");
        let mut state = ActorState::default();
        let (child_tx, parked) = crossbeam_channel::unbounded();
        drop(parked);
        state.children.insert(1, child_tx);
        state.shared.insert(1);

        let sent = exec_tool(
            &actor,
            &mut state,
            ToolName::Control,
            &json!({ "id": "1", "action": "stop" }),
            &AtomicBool::new(false),
        )
        .unwrap();

        assert!(!sent.contains("gone"), "{sent}");
        assert!(sent.contains("stopping agent #1"), "{sent}");
        let handed: Vec<(u64, AgentMsg)> = events
            .events_for(AgentId(7))
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::ChildAsleep { child, command } => Some((child, command)),
                _ => None,
            })
            .collect();
        assert_eq!(handed.len(), 1, "one command, one hand-over: {handed:?}");
        assert_eq!(handed[0].0, 1);
        assert!(
            matches!(handed[0].1, AgentMsg::Stop(Stop::Parent)),
            "the stop is what the child is woken to take, and it says who asked"
        );
        assert!(
            !state.running.contains(&1),
            "a stop puts nobody to work, so the books stay where they were"
        );
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
        let result = wait_tool(&actor, &mut state, &cancel).unwrap();

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
        note_completion(&mut state, 1, 1, Outcome::Stopped(Stop::Human));
        assert_eq!(state.outcome(1), Some(&Outcome::Stopped(Stop::Human)));
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

        // 3. The road the model asked for: `wait` hands the result over
        //    itself, so the mark goes out with the line.
        note_completion(&mut state, 3, 1, Outcome::Finished("waited for".into()));
        let cancel = AtomicBool::new(false);
        let waited = wait_tool(&actor, &mut state, &cancel).unwrap();
        assert!(waited.contains("#3 done"), "{waited}");
        assert_eq!(read(&events), vec![1, 2, 3], "and so is a wait");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A completion that the incoming transcript already carries (as a
    /// `wait` result, say) is not news: announcing it again would spend
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
        state.running_jobs.insert(JobId(1));
        let mut messages = vec![Message::system("you are mush")];
        let line = "#c1 done: exit 0 · 3m12s · cargo test — test result: ok";

        let folded = absorb(
            &actor,
            &mut state,
            &mut messages,
            AgentMsg::CommandDone {
                id: JobId(1),
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

    /// A child whose actor thread the UI parked comes back on a *fresh*
    /// `ActorState`, so its next run lands on a number the books have already
    /// read — 1 — while a completion is identified by (child, run) and a run the
    /// books have read is not news (`docs/findings.md` B24). `ChildParked` is
    /// what moves the identity out of the way, and this is the reason it exists:
    /// without it the result of the run the human's own message started is
    /// swallowed, and `wait` answers "(already read — no new run since)" about a
    /// run nobody ever read.
    #[test]
    fn a_woken_childs_next_report_is_still_news() {
        let mut state = ActorState::default();
        let (child, _child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child);
        state.running.insert(1);
        // Its first run ended, was reported, and the model read it.
        let (line, fresh) = state.record_child(1, 1, Outcome::Finished("wrote the lexer".into()));
        assert!(fresh, "the first report is news: {line}");

        // The UI reclaimed the thread and told the parent so.
        note_parked(&mut state, 1);
        assert!(
            !state.unread(1),
            "the result the model read is as read as it was"
        );
        assert!(
            !state.running.contains(&1),
            "a parked child is not running, whatever the books said"
        );

        // The human's message wakes it, and its second run is numbered 1 again.
        let (line, fresh) = state.record_child(1, 1, Outcome::Finished("and the tests".into()));
        assert!(
            fresh,
            "the second run is news even on the number the first one used: {line}"
        );
        assert!(line.contains("and the tests"), "{line}");
        assert!(
            state.outcome(1).is_some(),
            "and the listing still names the child by its last outcome"
        );
    }

    /// The history window reaped a child: its row is off the screen and the
    /// tree has forgotten its id, so the parent's books have to let go of it
    /// too — `status` listed a row nobody could see and `control` aimed at a
    /// ghost (finding H19).
    ///
    /// Every per-child book goes, and a report already in flight when the row
    /// went is swallowed rather than writing one back: the books are the only
    /// place an id can be named, and after this there is none.
    #[test]
    fn forgetting_a_child_drops_its_books_and_swallows_a_late_report() {
        let (actor, _mailbox) = test_actor("forget-books");
        let mut state = ActorState::default();
        let (child, _child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child);
        state.shared.insert(1);
        state.running.insert(1);
        note_completion(
            &mut state,
            1,
            1,
            Outcome::Finished("wrote the lexer".into()),
        );
        note_work(
            &mut state,
            1,
            1,
            Work::Clean {
                branch: "mush/1".into(),
            },
        );
        assert!(
            status_tool(&actor, &state).unwrap().contains("#1 ✓"),
            "a child with books reads as one"
        );

        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::ForgetChild { id: 1 },
        );

        assert_eq!(
            status_tool(&actor, &state).unwrap(),
            "no children and no jobs",
            "the listing names nothing the tree has dropped"
        );
        assert!(
            state.children.is_empty()
                && state.completed.is_empty()
                && state.delivered.is_empty()
                && state.running.is_empty()
                && state.shared.is_empty()
                && state.work.is_empty(),
            "every book that is keyed by a child id goes with the row"
        );

        // The report was already on its way: it is not news, and it cannot
        // re-open the child it names.
        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::ChildDone {
                id: 1,
                run: 1,
                outcome: Outcome::Finished("and the parser".into()),
            },
        );
        assert!(state.completed.is_empty(), "no book for a forgotten child");
        let mut messages = vec![Message::system("you are mush")];
        assert!(
            !fold_completions(&actor, &mut state, &mut messages),
            "and nothing to fold"
        );
        assert_eq!(messages.len(), 1, "no line for a child nobody can see");

        // Nor does the run that started just before the reap come back:
        // `reclaim_own_worktree` counts `running` itself rather than reading it
        // through a book a forget empties, so a ghost entry would pin this
        // actor's worktree for the rest of the session.
        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::ChildRunning { id: 1 },
        );
        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::Work {
                id: 1,
                run: 1,
                work: Work::Clean {
                    branch: "mush/1".into(),
                },
            },
        );
        assert!(
            state.running.is_empty() && state.work.is_empty(),
            "the tree has no row to list either fact under"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A forget is not permanent: the tree is the truth about which rows exist,
    /// so a row handed over afterwards is proof the earlier forget is stale and
    /// the child is bookable again — otherwise a reap that outran a restore
    /// would leave the parent unable to name a child it can see (findings H19,
    /// H25).
    #[test]
    fn a_row_handed_over_after_a_forget_reopens_the_child() {
        let (actor, _mailbox) = test_actor("forget-then-row");
        let mut state = ActorState::default();
        let (dead, dead_rx) = crossbeam_channel::unbounded();
        drop(dead_rx);
        state.children.insert(1, dead);
        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::ForgetChild { id: 1 },
        );
        assert_eq!(
            status_tool(&actor, &state).unwrap(),
            "no children and no jobs"
        );

        let (live, live_rx) = crossbeam_channel::unbounded();
        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::ChildBook {
                id: 1,
                cmd: live,
                outcome: Some(Outcome::Finished("wrote the lexer".into())),
                read: true,
                shared: true,
            },
        );

        let lines = status_tool(&actor, &state).unwrap();
        assert!(
            lines.contains("#1 ✓ wrote the lexer"),
            "the row is on screen, so it is in the books: {lines}"
        );
        assert!(
            state.shared.contains(&1),
            "and the books know it runs in this workspace"
        );
        let sent = message_agent(&actor, &mut state, &json!({ "text": "more" }), 1).unwrap();
        assert!(sent.contains("messaged agent #1"), "{sent}");
        assert!(
            matches!(live_rx.try_recv(), Ok(AgentMsg::Steer(text)) if text == "more"),
            "the row's mailbox is the live one"
        );

        // The tombstone goes with the closed books, not with the id: the child
        // is bookable again, so a run of it that starts, reports and works is
        // recorded rather than swallowed as a report of something the tree has
        // already dropped.
        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::ChildRunning { id: 1 },
        );
        assert!(
            state.running.contains(&1),
            "the run just started is booked as started: {:?}",
            state.running
        );
        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::Work {
                id: 1,
                run: 2,
                work: Work::Clean {
                    branch: "mush/1".into(),
                },
            },
        );
        let mut folded = Vec::new();
        absorb(
            &actor,
            &mut state,
            &mut folded,
            AgentMsg::ChildDone {
                id: 1,
                run: 2,
                outcome: Outcome::Finished("and the parser".into()),
            },
        );
        let mut messages = vec![Message::system("you are mush")];
        assert!(
            state.work.contains_key(&1),
            "a live row's work is listed again: {:?}",
            state.work.keys()
        );
        assert!(
            state.completed.get(&1).map(|done| done.run) == Some(2),
            "and its report is recorded rather than swallowed as the tree's"
        );
        assert!(
            folded.iter().any(|m| m.text().contains("and the parser")),
            "the line reaches the transcript: {folded:?}"
        );
        assert!(
            !fold_completions(&actor, &mut state, &mut messages),
            "and a report delivered once is not delivered again"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A mailbox handed over for a child the tree has already dropped keeps no
    /// book: the tombstone is the last word, and the one thing that clears it
    /// is the tree handing the row back (finding H19).
    ///
    /// The other three books a revival can touch are guarded the same way
    /// (`note_running`, `note_work`, `record_child`); a `ChildMailbox` that
    /// slipped past its tombstone would name a child in `status` with no row
    /// on screen to point at, which is exactly what the reap emptied the books
    /// to prevent.
    #[test]
    fn a_forgotten_child_handed_a_mailbox_keeps_no_book() {
        let (actor, _mailbox) = test_actor("forget-mailbox");
        let mut state = ActorState::default();
        let (dead, dead_rx) = crossbeam_channel::unbounded();
        drop(dead_rx);
        state.children.insert(1, dead);
        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::ForgetChild { id: 1 },
        );
        assert_eq!(
            status_tool(&actor, &state).unwrap(),
            "no children and no jobs",
            "the row is off the screen and the book is closed"
        );

        // The revive was already in flight when the row went: the tree swapped
        // the child's mailbox and told the parent which sender is live.
        let (live, live_rx) = crossbeam_channel::unbounded();
        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::ChildMailbox { id: 1, cmd: live },
        );

        assert!(
            state.children.is_empty(),
            "no book for a forgotten child: {:?}",
            state.children.keys()
        );
        assert_eq!(
            status_tool(&actor, &state).unwrap(),
            "no children and no jobs",
            "so the listing still names nothing the tree has dropped"
        );
        assert!(
            state.is_forgotten(1),
            "and the tombstone stands: only the row handed back clears one"
        );
        drop(live_rx);
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The actor thread of a parked child is replaced when a parent's `control`
    /// wakes it, and the revival builds a *new* mailbox. The parent's `children`
    /// book kept the dead sender, so its next message found no actor and took
    /// the wake path again — every time, for the rest of the session, while the
    /// message landed anyway (finding H22). `ChildMailbox` is the tree saying
    /// which sender is live, and the reply tells the parent which half of the
    /// `messaged` contract it got.
    #[test]
    fn a_revived_childs_mailbox_replaces_the_dead_one_in_the_parents_books() {
        let (actor, _mailbox) = test_actor("child-mailbox");
        let mut state = ActorState::default();
        let (dead, dead_rx) = crossbeam_channel::unbounded();
        drop(dead_rx);
        state.children.insert(1, dead);
        // A shared child has no worktree of its own, so this parent's own
        // workspace is where its runs write — which is still there.
        state.shared.insert(1);
        state.running.insert(1);

        let (live, live_rx) = crossbeam_channel::unbounded();
        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::ChildMailbox { id: 1, cmd: live },
        );

        let said = message_agent(&actor, &mut state, &json!({ "text": "keep going" }), 1).unwrap();
        assert!(
            said.starts_with("messaged agent #1 — it is mid-run"),
            "the words reached the live actor rather than the park path: {said}"
        );
        assert!(
            matches!(live_rx.try_recv(), Ok(AgentMsg::Steer(text)) if text == "keep going"),
            "the live mailbox is the one the books hold"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The batch the human saw: three children failed while the parent worked,
    /// one of them had already been answered through a `wait`, and then
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

        // The model asks for the results: `wait` answers all three, and that
        // answer is what the model has read.
        let answered = exec_tool(&actor, &mut state, ToolName::Wait, &json!({}), &cancel).unwrap();
        for id in [4u64, 5, 6] {
            assert!(
                answered.contains(&format!("#{id} failed: {error}")),
                "#{id} is delivered in full: {answered}"
            );
        }
        assert_eq!(
            state.delivered.get(&4),
            Some(&1),
            "an answer from `wait` is a delivery"
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
        // A *new* completion nobody has read is still news.
        let (tx, _rx) = crossbeam_channel::unbounded();
        state.children.insert(7, tx);
        mailbox
            .send(AgentMsg::ChildDone {
                id: 7,
                run: 1,
                outcome: Outcome::Failed(error.into()),
            })
            .unwrap();
        drain_mailbox(&actor, &AtomicBool::new(false), &mut messages, &mut state);

        assert!(
            fold_completions(&actor, &mut state, &mut messages),
            "only the unread result is news"
        );
        let folded: Vec<&str> = messages.iter().map(Message::text).collect();
        for id in [4u64, 5, 6] {
            assert_eq!(
                folded
                    .iter()
                    .filter(|line| line.starts_with(&format!("#{id} failed")))
                    .count(),
                0,
                "the failure the wait answered with is not folded again: {folded:?}"
            );
        }
        let line = format!("#7 failed: {error}");
        assert_eq!(
            folded.iter().filter(|folded| **folded == line).count(),
            1,
            "one line for the unread result: {folded:?}"
        );
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
            let mut answer = ToolOutput::default();
            match road {
                "wait" => {
                    answer = exec_tool(
                        &actor,
                        &mut state,
                        ToolName::Wait,
                        &json!({}),
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
                ToolName::Wait,
                &json!({}),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert!(
                !again.contains(body),
                "{road}: the repeated wait replayed it: {again}"
            );
            assert!(again.contains("already read"), "{road}: {again}");
            // The listing never carries the body, read or unread.
            let listing = status_tool(&actor, &state).unwrap();
            assert!(
                !listing.contains(body),
                "{road}: the listing carried the body"
            );
            let _ = fs::remove_dir_all(actor.ws.root());
        }
    }

    /// The digest names the size of what it hides exactly when it hides
    /// something: a body the column holds whole gains no `(N chars total)`, and
    /// a body the cut shortened always does — even when the `…` stands in for a
    /// dropped *wide* glyph, where the character counts tie and a count-based
    /// guess fell silent about the very cut it was there to report.
    #[test]
    fn a_digest_names_its_size_only_when_it_hides_something() {
        let whole = "x".repeat(DIGEST_COLUMNS);
        assert_eq!(
            Outcome::Finished(whole.clone()).digest(1),
            format!("#1 ✓ {whole}"),
            "a body exactly the column width is whole"
        );

        // One wide glyph past the budget: the cut keeps 99 columns of `x` and
        // spends the last on the ellipsis, so the digest has as many
        // *characters* as the body and the count comparison saw nothing hidden.
        let wide = format!("{}你", "x".repeat(DIGEST_COLUMNS - 1));
        let digest = Outcome::Finished(wide.clone()).digest(1);
        assert!(digest.contains('…'), "the cut says so: {digest}");
        assert!(
            digest.ends_with(&format!("({} chars total)", wide.chars().count())),
            "the digest says how much it hid: {digest}"
        );
    }

    /// `status` is a listing, not a delivery: a child's whole final
    /// message used to be printed on every call, so a parent that polled its
    /// children re-read every report, and the fold then re-delivered it as the
    /// same text a second time. Now the line is a bounded digest with the size
    /// of what it is not showing, and `✉` says whether the body is still
    /// waiting — the fold remains the one road that hands it over.
    #[test]
    fn a_listing_digests_a_result_and_says_what_is_unread() {
        let (actor, _mailbox) = test_actor("listing-digest");
        let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let mut state = ActorState::default();
        state.children.insert(1, tx);
        let long = format!("first line of a long report\n{}", "detail ".repeat(900));
        note_completion(&mut state, 1, 1, Outcome::Finished(long.clone()));

        let listing = status_tool(&actor, &state).unwrap();
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
        let mut transcript = vec![Message::system("you are mush")];
        assert!(fold_completions(&actor, &mut state, &mut transcript));
        let listing = status_tool(&actor, &state).unwrap();
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
        note_completion(&mut state, 3, 1, Outcome::Stopped(Stop::Human));
        note_completion(&mut state, 4, 1, Outcome::CutOff);
        let mut messages = Vec::new();
        let fresh = vec![
            Message::system("you are mush"),
            Message::user("task"),
            // The results need the batch that asked for them: adoption drops a
            // `tool` message whose call is not above it, exactly as a strict
            // server would reject one, and the scan these lines feed reads what
            // a real session stores.
            assistant_calling(&["a", "b", "c"]),
            Message::tool("a", "#2 failed: no route"),
            Message::tool("b", Outcome::Stopped(Stop::Human).line(3)),
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
        state.running_jobs.insert(JobId(1));
        let mut messages = vec![Message::system("you are mush")];
        let line = "#c1 done: exit 0 · 3m12s · cargo test — test result: ok";
        let report = || AgentMsg::CommandDone {
            id: JobId(1),
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
        state.running_jobs.insert(JobId(1));
        let mut messages = vec![Message::system("you are mush")];
        let line = "#c1 stopped after 1s · npm run dev";
        mailbox
            .send(AgentMsg::CommandDone {
                id: JobId(1),
                line: line.into(),
                news: false,
            })
            .unwrap();

        drain_mailbox(&actor, &AtomicBool::new(false), &mut messages, &mut state);

        assert_eq!(messages.last().unwrap().text(), line);
        assert!(state.delivered_jobs.contains(&JobId(1)));
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

        assert!(report.contains("timed out after 5s"), "{report}");
        assert!(
            report.contains("budget is full"),
            "and the reason it could not detach is named: {report}"
        );
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
        mailbox.send(AgentMsg::Stop(Stop::Human)).unwrap();

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
        let cap = result_cap(&actor);
        assert!(report.len() < cap * 2, "report grew: {}", report.len());
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
            report.contains("output truncated at"),
            "cap was not marked: {report}"
        );
        let cap = result_cap(&actor);
        assert!(
            report.len() < cap * 2,
            "report grew past the cap: {}",
            report.len()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A standalone actor over a scratch workspace, for exercising the mailbox
    /// plumbing with no model, no UI, and no threads.
    ///
    /// Several edits to the *same* file, one call after another, must all land:
    /// every file tool re-reads from disk, so the second edit sees the first
    /// one's result instead of clobbering it with a stale copy.
    #[test]
    fn several_edits_to_one_file_in_a_batch_all_land() {
        let (actor, _mailbox) = test_actor("multi-edit");
        fs::write(actor.ws.root().join("f.rs"), "let a = 1;\nlet b = 2;\n").unwrap();

        // Three calls, each the one-element list the schema asks for.
        for (old, new) in [
            ("let a = 1;", "let a = 10;"),
            ("let b = 2;", "let b = 20;"),
            ("let b = 20;", "let b = 21;"),
        ] {
            edit_tool(
                &actor.ws,
                &json!({ "path": "f.rs", "edits": [{ "old_string": old, "new_string": new }] }),
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
        fs::write(actor.ws.root().join("f.rs"), "x = 1;\nx = 2;\n").unwrap();

        let error = edit_tool(
            &actor.ws,
            &json!({ "path": "f.rs", "edits": [{ "old_string": "x = ", "new_string": "y = " }] }),
        )
        .unwrap_err();
        assert!(error.contains("2 times"), "{error}");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The schema's sentence promises that `replace_all` changes every
    /// occurrence, and the ambiguity refusal tells the model to set it. The flag
    /// lives on every entry of `edits` — one shape — so the call that took the
    /// refusal's advice changes every occurrence.
    ///
    /// It used to live on a second, top-level spelling too, and the two drifted
    /// exactly as a fact with two homes does: the schema declared it inside
    /// `edits` while the code honoured a top-level one, so a model that set the
    /// flag the schema named was refused anyway (H20 item 3). The second
    /// spelling is gone with the `edit_file` simplification.
    #[test]
    fn replace_all_changes_every_occurrence_in_the_one_shape() {
        let (actor, _mailbox) = test_actor("replace-all");
        fs::write(actor.ws.root().join("f.rs"), "old();\nold(arg);\n").unwrap();

        let refused = edit_tool(
            &actor.ws,
            &json!({ "path": "f.rs", "edits": [{ "old_string": "old", "new_string": "new" }] }),
        )
        .unwrap_err();
        assert!(
            refused.contains("replace_all"),
            "the refusal names the way out: {refused}"
        );

        let report = edit_tool(
            &actor.ws,
            &json!({
                "path": "f.rs",
                "edits": [{ "old_string": "old", "new_string": "new", "replace_all": true }]
            }),
        )
        .unwrap();
        assert_eq!(report, "edited f.rs");
        assert_eq!(
            fs::read_to_string(actor.ws.root().join("f.rs")).unwrap(),
            "new();\nnew(arg);\n"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The regression the file tools exist for: a sibling holds the machine and
    /// every command is refused — while reading, listing, searching and writing
    /// keep working. Before these tools came back, an agent in this state could
    /// not read a file, list a directory, or create one: it could only wait.
    #[test]
    fn the_file_tools_work_while_the_machine_is_held() {
        let (actor, _mailbox) = test_actor("beside-the-lock");
        fs::create_dir_all(actor.ws.root().join("src")).unwrap();
        fs::write(actor.ws.root().join("src/lib.rs"), "fn held() {}\n").unwrap();
        actor.ctx.registry.take_machine(2, "cargo bench").unwrap();
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut call =
            |tool: ToolName, args: Value| exec_tool(&actor, &mut state, tool, &args, &cancel);

        let read = call(ToolName::ReadFile, json!({ "path": "src/lib.rs" })).unwrap();
        assert!(read.contains("fn held()"), "{read}");
        let listed = call(ToolName::ListFiles, json!({})).unwrap();
        assert!(listed.contains("src/lib.rs"), "{listed}");
        let found = call(ToolName::Search, json!({ "pattern": "held" })).unwrap();
        assert!(found.contains("src/lib.rs:1"), "{found}");
        let wrote = call(
            ToolName::WriteFile,
            json!({ "path": "src/new.rs", "content": "fn fresh() {}\n" }),
        )
        .unwrap();
        assert_eq!(wrote, "wrote src/new.rs — 1 line (new)");
        let edited = call(
            ToolName::EditFile,
            json!({ "path": "src/new.rs", "edits": [{ "old_string": "fresh", "new_string": "edited" }] }),
        )
        .unwrap();
        assert_eq!(edited, "edited src/new.rs");

        // And the contrast: the shell is what the lock refuses.
        let refused =
            call(ToolName::RunCommand, json!({ "command": "cat src/lib.rs" })).unwrap_err();
        assert!(
            refused.text().contains("holds the machine"),
            "{}",
            refused.text()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A read is a window, and the window says what it left without numbering
    /// the lines: a number beside the text is a string a model pastes into
    /// `edit_file`, where it cannot match.
    #[test]
    fn a_read_is_a_window_that_says_what_it_left() {
        let (actor, _mailbox) = test_actor("read-window");
        let body: String = (1..=9).map(|n| format!("line {n}\n")).collect();
        fs::write(actor.ws.root().join("f.txt"), body).unwrap();
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));

        let first = exec_tool(
            &actor,
            &mut state,
            ToolName::ReadFile,
            &json!({ "path": "f.txt", "limit": 3 }),
            &cancel,
        )
        .unwrap();
        assert_eq!(
            first, "line 1\nline 2\nline 3\n[mush: lines 1–3 of 9 — read on with offset=4]",
            "the window is the text and one sentence"
        );

        let last = exec_tool(
            &actor,
            &mut state,
            ToolName::ReadFile,
            &json!({ "path": "f.txt", "offset": 8 }),
            &cancel,
        )
        .unwrap();
        assert_eq!(
            last, "line 8\nline 9\n[mush: lines 8–9 of 9 — end of file]",
            "{last}"
        );

        // A whole small file is its own content, with no note at all.
        let whole = exec_tool(
            &actor,
            &mut state,
            ToolName::ReadFile,
            &json!({ "path": "f.txt", "limit": 9 }),
            &cancel,
        )
        .unwrap();
        assert!(whole.ends_with("line 9"), "{whole}");
        assert!(!whole.contains("mush:"), "{whole}");

        // An offset past the end is a refusal that names the file's size rather
        // than an empty read that looks like an empty file.
        let past = exec_tool(
            &actor,
            &mut state,
            ToolName::ReadFile,
            &json!({ "path": "f.txt", "offset": 40 }),
            &cancel,
        )
        .unwrap_err();
        assert!(past.text().contains("9 lines"), "{}", past.text());

        // A `limit` of zero is not "everything": it is a window with no lines
        // in it, which would print a backwards range. It is refused instead.
        let none = exec_tool(
            &actor,
            &mut state,
            ToolName::ReadFile,
            &json!({ "path": "f.txt", "limit": 0 }),
            &cancel,
        )
        .unwrap_err();
        assert!(none.text().contains("at least 1"), "{}", none.text());
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The reason the file tools came back: `read_file` is the one road an
    /// image can travel by. A png is sniffed from its own first bytes — never
    /// from its name, which is a claim — and the result carries both the bytes
    /// and the one line a saved session keeps when it sheds them.
    #[test]
    fn read_file_hands_back_an_image_when_the_model_can_see() {
        const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4];
        // The one model mush knows to accept images, so the read is allowed to
        // hand one over; `test_cfg`'s model is not documented for them.
        let cfg = ConfigHandle::own(Config::new("http://127.0.0.1:1", "deepseek-flash", None));
        let (actor, _events, _mailbox) =
            build_actor("read-image", Arc::new(HttpModel::new(cfg.clone())), cfg);
        fs::write(actor.ws.root().join("shot.png"), PNG).unwrap();
        // A png that lies about its name is still a png; a text file that calls
        // itself one is still text.
        fs::write(actor.ws.root().join("lies.txt"), PNG).unwrap();
        fs::write(actor.ws.root().join("not.png"), "plain text\n").unwrap();
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut read = |path: &str| {
            exec_tool(
                &actor,
                &mut state,
                ToolName::ReadFile,
                &json!({ "path": path }),
                &cancel,
            )
        };

        let seen = read("shot.png").unwrap();
        assert_eq!(seen.images.len(), 1, "the bytes travel with the result");
        assert_eq!(seen.images[0].mime, "image/png");
        assert_eq!(seen.images[0].bytes, PNG, "the bytes are the file's");
        assert_eq!(seen.images[0].path, "shot.png", "and the path is the name");
        assert_eq!(seen.text, "read shot.png — a png image, 12 bytes");

        // The message the model is sent: one tool result carrying the label and
        // the picture, in the vision form's own shape.
        let message = Message::tool_with_images("call_1", seen.text.clone(), seen.images.clone());
        let wire = serde_json::to_string(&message).unwrap();
        assert!(
            wire.starts_with(r#"{"role":"tool","content":[{"type":"text","text":"read shot.png"#),
            "{wire}"
        );
        assert!(wire.contains(r#""type":"image_url""#), "{wire}");
        assert!(wire.contains(r#""tool_call_id":"call_1""#), "{wire}");

        let misnamed = read("lies.txt").unwrap();
        assert_eq!(
            misnamed.images.len(),
            1,
            "the magic number decides, not the extension"
        );
        let plain = read("not.png").unwrap();
        assert!(plain.images.is_empty(), "a text file named .png is text");
        assert_eq!(
            plain.text, "plain text",
            "and it is read as the window it is"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The model's own road fills the size in too: `read_file` on a picture
    /// hands back an image whose pixels come from the header, so the budget
    /// counts a tool result the way it counts an attachment — the same 724 KB
    /// screenshot is ~2.8k tokens whichever road it arrived by.
    #[test]
    fn a_read_image_carries_the_size_its_header_names() {
        let cfg = ConfigHandle::own(Config::new("http://127.0.0.1:1", "deepseek-flash", None));
        let (actor, _events, _mailbox) = build_actor(
            "read-image-size",
            Arc::new(HttpModel::new(cfg.clone())),
            cfg,
        );
        fs::write(actor.ws.root().join("screen.png"), png_of(1_920, 1_080)).unwrap();
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));

        let seen = exec_tool(
            &actor,
            &mut state,
            ToolName::ReadFile,
            &json!({ "path": "screen.png" }),
            &cancel,
        )
        .unwrap();

        assert_eq!(seen.images.len(), 1, "the bytes travel with the result");
        assert_eq!(
            seen.images[0].pixels,
            Some((1_920, 1_080)),
            "and the size the budget prices them by"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A png whose IHDR names `width × height`, for the tests that need the
    /// size in the header as well as the magic number: signature, IHDR, the
    /// two big-endian dimensions, and the five bytes of IHDR payload left over.
    fn png_of(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
        ];
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        bytes.extend([8, 6, 0, 0, 0]);
        bytes
    }

    /// Two refusals an image can meet, each naming a move that exists: a model
    /// that cannot see it (nothing the model can change — it says so in its
    /// summary) and an image past the 2 MB cap (downscale it, and read that).
    /// Neither sentence may borrow the other's road.
    #[test]
    fn an_image_that_cannot_travel_is_refused_with_the_move_that_can() {
        const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4];
        let (actor, _mailbox) = test_actor("read-image-off");
        fs::write(actor.ws.root().join("shot.png"), PNG).unwrap();
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));

        let refused = exec_tool(
            &actor,
            &mut state,
            ToolName::ReadFile,
            &json!({ "path": "shot.png" }),
            &cancel,
        )
        .unwrap_err();
        let why = refused.text();
        assert!(why.contains("`test`"), "it names the model: {why}");
        assert!(why.contains("cannot travel"), "{why}");
        assert!(
            why.contains("summary"),
            "and the road is the run's end: {why}"
        );
        assert!(!why.contains("Downscale"), "not the size refusal's: {why}");

        // The same file, to a model whose row documents vision, but too big.
        let cfg = ConfigHandle::own(Config::new("http://127.0.0.1:1", "deepseek-flash", None));
        let (seer, _events, _mailbox) =
            build_actor("read-image-big", Arc::new(HttpModel::new(cfg.clone())), cfg);
        let mut big = PNG.to_vec();
        big.resize(mush_core::workspace::IMAGE_FILE_CAP as usize + 1, 0);
        fs::write(seer.ws.root().join("big.png"), big).unwrap();
        let refused = exec_tool(
            &seer,
            &mut state,
            ToolName::ReadFile,
            &json!({ "path": "big.png" }),
            &cancel,
        )
        .unwrap_err();
        let why = refused.text();
        assert!(why.contains("past the 2 MB cap"), "{why}");
        assert!(why.contains("convert big.png"), "it names the road: {why}");
        assert!(!why.contains("summary"), "not the vision refusal's: {why}");
        let _ = fs::remove_dir_all(actor.ws.root());
        let _ = fs::remove_dir_all(seer.ws.root());
    }

    /// `write_file` creates what is not there — the one road that was missing
    /// while a lock was held — and answers with what it replaced.
    #[test]
    fn write_file_creates_and_says_what_it_replaced() {
        let (actor, _mailbox) = test_actor("write-file");
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut write =
            |args: Value| exec_tool(&actor, &mut state, ToolName::WriteFile, &args, &cancel);

        let new = write(json!({ "path": "src/deep/new.rs", "content": "a\n" })).unwrap();
        assert_eq!(new, "wrote src/deep/new.rs — 1 line (new)");

        let more = write(json!({ "path": "src/deep/new.rs", "content": "a\nb\n" })).unwrap();
        assert_eq!(more, "wrote src/deep/new.rs — 1 → 2 lines");
        assert_eq!(
            fs::read_to_string(actor.ws.root().join("src/deep/new.rs")).unwrap(),
            "a\nb\n"
        );

        let replaced = write(json!({ "path": "src/deep/new.rs", "content": "a\nb\nc\n" })).unwrap();
        assert_eq!(replaced, "wrote src/deep/new.rs — 2 → 3 lines");

        let root = write(json!({ "path": ".", "content": "x" })).unwrap_err();
        assert!(root.text().contains("workspace root"), "{}", root.text());
        let big =
            write(json!({ "path": "big.txt", "content": "x".repeat(result_cap(&actor) + 1) }))
                .unwrap_err();
        assert!(big.text().contains("cap"), "{}", big.text());
        assert!(
            !actor.ws.root().join("big.txt").exists(),
            "nothing was written"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// `list_files` and `search` read the same walk: build output and VCS
    /// metadata are skipped, hidden files are not, and a search says where a
    /// line is without the model writing a regex.
    #[test]
    fn list_files_and_search_read_the_workspace() {
        let (actor, _mailbox) = test_actor("list-and-search");
        fs::create_dir_all(actor.ws.root().join("target/debug")).unwrap();
        fs::create_dir_all(actor.ws.root().join("src")).unwrap();
        fs::write(actor.ws.root().join("target/debug/junk"), "needle\n").unwrap();
        fs::write(actor.ws.root().join("src/lib.rs"), "fn needle() {}\n").unwrap();
        fs::write(actor.ws.root().join("src/other.rs"), "// NEEDLE here\n").unwrap();
        fs::write(actor.ws.root().join(".gitignore"), "/target\n").unwrap();
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));

        let listed =
            exec_tool(&actor, &mut state, ToolName::ListFiles, &json!({}), &cancel).unwrap();
        assert_eq!(listed, ".gitignore\nsrc/lib.rs\nsrc/other.rs", "{listed}");
        // A path that names a file lists that file, rather than claiming the
        // directory the model did not name is empty.
        let one = exec_tool(
            &actor,
            &mut state,
            ToolName::ListFiles,
            &json!({ "path": "src/lib.rs" }),
            &cancel,
        )
        .unwrap();
        assert_eq!(one, "src/lib.rs", "{one}");
        let sub = exec_tool(
            &actor,
            &mut state,
            ToolName::ListFiles,
            &json!({ "path": "src" }),
            &cancel,
        )
        .unwrap();
        assert_eq!(sub, "src/lib.rs\nsrc/other.rs", "{sub}");

        let found = exec_tool(
            &actor,
            &mut state,
            ToolName::Search,
            &json!({ "pattern": "needle" }),
            &cancel,
        )
        .unwrap();
        assert_eq!(found, "src/lib.rs:1: fn needle() {}", "{found}");

        let any_case = exec_tool(
            &actor,
            &mut state,
            ToolName::Search,
            &json!({ "pattern": "needle", "ignore_case": true }),
            &cancel,
        )
        .unwrap();
        assert!(
            any_case.contains("src/other.rs:1: // NEEDLE here"),
            "{any_case}"
        );
        assert!(
            !any_case.contains("target/"),
            "build output is not workspace text: {any_case}"
        );

        let none = exec_tool(
            &actor,
            &mut state,
            ToolName::Search,
            &json!({ "pattern": "nothing-like-this" }),
            &cancel,
        )
        .unwrap();
        assert!(none.contains("no match"), "{none}");
        assert!(none.contains("workspace root"), "{none}");

        // Searching one named file searches that file.
        let inside = exec_tool(
            &actor,
            &mut state,
            ToolName::Search,
            &json!({ "pattern": "needle", "path": "src/lib.rs" }),
            &cancel,
        )
        .unwrap();
        assert_eq!(inside, "src/lib.rs:1: fn needle() {}", "{inside}");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A search that never opened a file must not answer "no match": a model
    /// reads a miss as "the symbol is not there", and a file the walk skipped
    /// is not evidence of absence. The count travels back with the matches,
    /// singular and plural, and rides along even when something did match —
    /// the list is then complete-looking but is not.
    #[test]
    fn a_search_says_which_files_it_never_opened() {
        let (actor, _mailbox) = test_actor("search-skips");
        // Past `SEARCH_FILE_CAP`, so the walk never opens it; and a binary one
        // that contains the needle as plain bytes.
        fs::write(
            actor.ws.root().join("big.txt"),
            format!("{}needle\n", "x".repeat(SEARCH_FILE_CAP as usize + 1)),
        )
        .unwrap();
        fs::write(actor.ws.root().join("blob.bin"), b"needle\0\0").unwrap();
        fs::write(actor.ws.root().join("small.txt"), "nothing here\n").unwrap();
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut search =
            |args: Value| exec_tool(&actor, &mut state, ToolName::Search, &args, &cancel);

        let miss = search(json!({ "pattern": "needle" })).unwrap();
        assert!(
            miss.contains("2 files were skipped (binary or over 2 MB)"),
            "{miss}"
        );
        assert!(miss.contains("run_command"), "names the road: {miss}");

        fs::remove_file(actor.ws.root().join("blob.bin")).unwrap();
        let one = search(json!({ "pattern": "needle" })).unwrap();
        assert!(
            one.contains("1 file was skipped") && one.contains("reads it"),
            "one file reads as one: {one}"
        );

        // A hit does not make the search complete: the skip still shows.
        fs::write(actor.ws.root().join("small.txt"), "a needle\n").unwrap();
        let hit = search(json!({ "pattern": "needle" })).unwrap();
        assert!(hit.starts_with("small.txt:1: a needle"), "{hit}");
        assert!(hit.contains("1 file was skipped"), "{hit}");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// "Empty" and "not there" are different facts: a listing or a search of a
    /// path that does not exist must not read as an empty one, and a `path`
    /// that is not a string must be refused rather than read as the root.
    #[test]
    fn a_path_that_is_not_there_is_not_an_empty_one() {
        let (actor, _mailbox) = test_actor("no-such-path");
        fs::write(actor.ws.root().join("f.txt"), "x\n").unwrap();
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut call =
            |tool: ToolName, args: Value| exec_tool(&actor, &mut state, tool, &args, &cancel);

        let listed = call(ToolName::ListFiles, json!({ "path": "nope/deeper" })).unwrap_err();
        assert!(listed.text().contains("no such path"), "{}", listed.text());
        let searched = call(
            ToolName::Search,
            json!({ "pattern": "x", "path": "nope/deeper" }),
        )
        .unwrap_err();
        assert!(
            searched.text().contains("no such path"),
            "{}",
            searched.text()
        );

        // A number is not a path: silently listing the root would answer a
        // question the model did not ask.
        for tool in [ToolName::ListFiles, ToolName::Search] {
            let wrong = call(tool, json!({ "pattern": "x", "path": 7 })).unwrap_err();
            assert!(
                wrong.text().contains("must be a string"),
                "{}",
                wrong.text()
            );
        }
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A file this tool is about to replace is a file whether or not its bytes
    /// are text: `(new)` over a blob is a false history fact, and the over-cap
    /// refusal must not tell the model to do the thing that just failed.
    #[test]
    fn write_file_does_not_call_a_replaced_blob_new() {
        let (actor, _mailbox) = test_actor("write-blob");
        fs::write(actor.ws.root().join("blob.bin"), b"\0\0\0\0").unwrap();
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut write =
            |args: Value| exec_tool(&actor, &mut state, ToolName::WriteFile, &args, &cancel);

        let replaced = write(json!({ "path": "blob.bin", "content": "text\n" })).unwrap();
        assert_eq!(
            replaced,
            "wrote blob.bin — 1 line (replaced a file that is not text)"
        );

        let over =
            write(json!({ "path": "big.txt", "content": "x".repeat(result_cap(&actor) + 1) }))
                .unwrap_err();
        assert!(
            over.text().contains("write the first part"),
            "the road is a split, not a repeat: {}",
            over.text()
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    #[test]
    fn edit_file_demands_the_one_shape() {
        let (actor, _mailbox) = test_actor("edit-shape");
        fs::write(actor.ws.root().join("f.rs"), "let a = 1;\n").unwrap();

        let missing = edit_tool(&actor.ws, &json!({ "path": "f.rs" })).unwrap_err();
        assert!(missing.contains("`edits`"), "names the shape: {missing}");
        let wrong = edit_tool(&actor.ws, &json!({ "path": "f.rs", "edits": "let a" })).unwrap_err();
        assert!(wrong.contains("list"), "{wrong}");
        let empty = edit_tool(&actor.ws, &json!({ "path": "f.rs", "edits": [] })).unwrap_err();
        assert!(empty.contains("empty"), "{empty}");

        // The bare object is the list of one, because that is what it means.
        let sugar = edit_tool(
            &actor.ws,
            &json!({ "path": "f.rs", "edits": { "old_string": "let a = 1;", "new_string": "let a = 2;" } }),
        )
        .unwrap();
        assert_eq!(sugar, "edited f.rs");
        assert_eq!(
            fs::read_to_string(actor.ws.root().join("f.rs")).unwrap(),
            "let a = 2;\n"
        );
        // And a real batch is one call, one write, one line naming the count.
        let batch = edit_tool(
            &actor.ws,
            &json!({ "path": "f.rs", "edits": [
                { "old_string": "a = 2", "new_string": "a = 3" },
                { "old_string": "a = 3", "new_string": "a = 4" }
            ] }),
        )
        .unwrap();
        assert_eq!(batch, "edited f.rs — 2 edits");
        assert_eq!(
            fs::read_to_string(actor.ws.root().join("f.rs")).unwrap(),
            "let a = 4;\n"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// `status` with nothing behind it: the registry answers `None` rather than
    /// a sentinel line the caller compares with a string, and the listing is the
    /// one sentence a model can act on. The sentinel was a count spelled as
    /// text — a job really named `no jobs` could hide behind it.
    #[test]
    fn a_status_with_no_jobs_is_none_and_no_section() {
        let (actor, _mailbox) = test_actor("status-none");
        assert_eq!(
            actor.ctx.registry.status_for(actor.id),
            None,
            "an owner with no jobs has none, not a line saying so"
        );
        assert_eq!(
            status_tool(&actor, &ActorState::default()).unwrap(),
            "no children and no jobs"
        );
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
        let ids = Ids::default();
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
        let actor = Actor {
            ctx,
            id: 7,
            depth: 0,
            ws: Workspace::new(&root).unwrap(),
            branch: None,
            fork: None,
            base: None,
            brief: String::new(),
            my_tx: my_tx.clone(),
            // No parent: the standalone actor has nobody to report to, which is
            // the one `parent_tx` value that says nothing at all — a test that
            // wants the UI's road builds the actor with a dead mailbox of its
            // own (`Some(dead_mailbox())`, §8.39).
            parent_tx: None,
            rx,
        };
        (actor, recorder, my_tx)
    }

    /// The root napping on `wait` must hear the human. Parking their
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
        let result = wait_tool(&actor, &mut state, &cancel).unwrap();
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
            matches!(state.deferred.first(), Some(AgentMsg::Nudge(message)) if message.text() == "what about the tests?"),
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
        let result = wait_tool(&actor, &mut state, &cancel).unwrap();

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

    /// `wait` reaches the jobs this agent started on its own books as well as
    /// its children: two jobs are started, the wait returns both reports in one
    /// digest, and an agent with neither children nor jobs is answered at once.
    #[test]
    fn wait_returns_a_jobs_report() {
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
        // this clock the deadline elapses in microseconds, so a report whose
        // thread had not yet been given the CPU made the wait answer "wait
        // timed out — #c1 still running" — a fact about the scheduler, not
        // about `wait`. The recorded event is what the wait reads
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

        // One call, one digest: both reports, each in the job's own words.
        let both = exec_tool(&actor, &mut state, ToolName::Wait, &json!({}), &cancel).unwrap();
        assert!(both.contains("exit 0"), "{both}");
        assert!(both.contains("make build"), "{both}");
        assert!(both.contains("make test"), "{both}");
        let build = both.find("make build").expect("build is named");
        let test = both.find("make test").expect("test is named");
        assert!(build < test, "in id order: {both}");

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
                ToolName::Wait,
                &json!({}),
                &cancel
            )
            .unwrap(),
            "nothing to wait for: nothing of yours is running or unread"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
        let _ = fs::remove_dir_all(idle.ws.root());
    }

    /// One `wait` means everything: with two results recorded and nothing in
    /// flight, a single call answers both — an unread result in full, an
    /// already-read one as a line — and the schema has no `all`/`ids` for a
    /// model to reason wrong (finding H15).
    #[test]
    fn one_wait_means_everything() {
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
                outcome: Outcome::Finished("wrote the parser\nand the tests".into()),
            },
        );
        state.completed.insert(
            2,
            Completion {
                run: 1,
                outcome: Outcome::Failed("no route\nto the server".into()),
            },
        );

        // Nothing is running, so the call returns at once with both bodies —
        // both were unread, so both are delivered in full.
        let every = exec_tool(&actor, &mut state, ToolName::Wait, &json!({}), &cancel).unwrap();
        assert!(every.contains("#1 done: wrote the parser"), "{every}");
        assert!(
            every.contains("#2 failed: no route"),
            "the second child's body is not swallowed: {every}"
        );
        assert!(
            every.contains("and the tests") && every.contains("to the server"),
            "an unread result comes over in full, not as its first line: {every}"
        );
        let one = every.find("#1").expect("#1 is named");
        let two = every.find("#2").expect("#2 is named");
        assert!(one < two, "in id order: {every}");
        assert!(!state.unread(1) && !state.unread(2), "both are read now");

        // A second wait is not a replay: both come back as lines, never the
        // bodies again (finding H15/B26).
        let again = exec_tool(&actor, &mut state, ToolName::Wait, &json!({}), &cancel).unwrap();
        assert!(again.contains("already read"), "{again}");
        assert!(
            !again.contains("and the tests") && !again.contains("to the server"),
            "a body the model has read is not handed over twice — only its first line: {again}"
        );
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

        // A job whose line the model has already read is not the timeout's to
        // report (finding H34): the deadline answers with what is *unread*.
        state.done_jobs.insert(
            JobId(2),
            JobReport {
                line: "#c2 done: exit 0 · 2s · ls — already in the transcript".into(),
                news: true,
            },
        );
        state.delivered_jobs.insert(JobId(2));

        let begun = Instant::now();
        let result = exec_tool(&actor, &mut state, ToolName::Wait, &json!({}), &cancel).unwrap();

        assert!(result.contains("wait timed out"), "{result}");
        assert!(result.contains("#c1 still running"), "{result}");
        assert!(
            clock.elapsed() >= Duration::from_secs(600),
            "the deadline ended it: {:?}",
            clock.elapsed()
        );
        assert!(
            begun.elapsed() < Duration::from_secs(1),
            "and it was reached without waiting for it: {:?}",
            begun.elapsed()
        );
        assert!(
            !result.contains("#c2"),
            "the timeout does not reprint a read job: {result}"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A job's line is a delivery, not a listing: `wait` hands it over once and
    /// never recaps it (finding H34). A bare recap was invisible — a read
    /// report and an unread one arrived in the same words — and it grew with
    /// every job the session had ever run, so a wait whose only book entry was
    /// read answered the truth with history.
    #[test]
    fn a_wait_hands_a_job_report_over_once_and_never_recaps_it() {
        let (actor, _mailbox) = scripted_tools_actor(
            "wait-job-once",
            Arc::new(ScriptedMachine::new()),
            Arc::new(Advanceable::new()),
        );
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let line = "#c2 done: exit 0 · 12s · cargo test — 586 passed";

        note_job(&mut state, JobId(2), line.into(), true);
        let first = wait_tool(&actor, &mut state, &cancel).unwrap();
        assert_eq!(first, line, "the unread report travels, once");
        assert!(state.delivered_jobs.contains(&JobId(2)));

        // Read is read: this call has nothing of its own left, and says the
        // truth instead of reprinting what the transcript already holds.
        let again = wait_tool(&actor, &mut state, &cancel).unwrap();
        assert_eq!(again, NOTHING_TO_WAIT_FOR);
        assert!(!again.contains(line), "no recap: {again}");

        // A read child keeps its marked digest — it can run again — while the
        // read job beside it stays unsaid.
        let mut state = ActorState::default();
        let (one, _one_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, one);
        note_completion(&mut state, 1, 1, Outcome::Finished("old news".into()));
        state.delivered.insert(1, 1);
        state.done_jobs.insert(
            JobId(3),
            JobReport {
                line: "#c3 done: exit 0 · 2s · ls".into(),
                news: true,
            },
        );
        state.delivered_jobs.insert(JobId(3));
        let mixed = wait_tool(&actor, &mut state, &cancel).unwrap();
        assert!(mixed.contains("already read"), "{mixed}");
        assert!(
            !mixed.contains("#c3"),
            "the read job is not recapped beside it: {mixed}"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// `wait({ on })`: one named target while the rest of the books run on, and
    /// the machine lock out of it (finding H34). What the call does *not*
    /// narrow is its attention — a result nobody has read, a failure first
    /// among them, and the human's words all end it the way they end a bare
    /// wait, so sitting on news for ten minutes is not a shape it has.
    #[test]
    fn an_optional_target_waits_for_one_thing_and_lets_the_rest_run() {
        let clock = Arc::new(Advanceable::new());
        let (actor, mailbox) =
            scripted_tools_actor("wait-on", Arc::new(ScriptedMachine::new()), clock.clone());
        let cancel = AtomicBool::new(false);
        let mut state = ActorState::default();

        // One name, or nothing: a wrong shape is refused where it stands — a
        // list can never fall back to "everything" — and an id this agent owns
        // nothing under gets the sentence `control` gives, not a timeout.
        let refused = exec_tool(
            &actor,
            &mut state,
            ToolName::Wait,
            &json!({ "on": ["c3"] }),
            &cancel,
        )
        .unwrap_err();
        assert!(
            refused.text().contains("`on` must be one target"),
            "{}",
            refused.text()
        );
        let refused = exec_tool(
            &actor,
            &mut state,
            ToolName::Wait,
            &json!({ "on": "c9" }),
            &cancel,
        )
        .unwrap_err();
        assert_eq!(refused.text(), "no such job #c9 — status lists yours");
        let refused = exec_tool(
            &actor,
            &mut state,
            ToolName::Wait,
            &json!({ "on": "9" }),
            &cancel,
        )
        .unwrap_err();
        assert_eq!(
            refused.text(),
            "no such child agent #9 — status lists yours"
        );

        // #1 and #2 work, and a failed child and a failed job are already in
        // the books. The call names #1; the failures end the wait anyway, with
        // the target's own state named beside them.
        let (one, _one_rx) = crossbeam_channel::unbounded();
        let (two, _two_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, one);
        state.children.insert(2, two);
        state.running.insert(1);
        state.running.insert(2);
        note_completion(&mut state, 2, 1, Outcome::Failed("the gate is red".into()));
        note_job(
            &mut state,
            JobId(3),
            "#c3 done: exit 1 · 4s · cargo test — 2 failed".into(),
            true,
        );
        let news = wait_on_tool(&actor, &mut state, &cancel, Target::Agent(1)).unwrap();
        assert!(news.contains("#2 failed: the gate is red"), "{news}");
        assert!(news.contains("#c3 done: exit 1"), "{news}");
        assert!(news.contains("#1 is still running"), "{news}");
        assert!(
            state.delivered_jobs.contains(&JobId(3)),
            "the line is read now"
        );

        // A named child that finished and was read: the answer is its digest
        // wearing the mark — the model asked by name, so it is not a recap.
        let mut state = ActorState::default();
        let (one, _one_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, one);
        note_completion(&mut state, 1, 1, Outcome::Finished("the parser".into()));
        state.delivered.insert(1, 1);
        let again = wait_on_tool(&actor, &mut state, &cancel, Target::Agent(1)).unwrap();
        assert!(again.contains("#1 ✓ the parser"), "{again}");
        assert!(again.contains("already read"), "{again}");

        // The same for a job target: its line is the answer when it ends, and
        // the second call says it has been read.
        let mut state = ActorState::default();
        state.running_jobs.insert(JobId(5));
        note_job(
            &mut state,
            JobId(5),
            "#c5 done: exit 0 · 2s · cargo fmt".into(),
            true,
        );
        let answer = wait_on_tool(&actor, &mut state, &cancel, Target::Job(JobId(5))).unwrap();
        assert_eq!(answer, "#c5 done: exit 0 · 2s · cargo fmt");
        let again = wait_on_tool(&actor, &mut state, &cancel, Target::Job(JobId(5))).unwrap();
        assert!(again.contains("already read"), "{again}");

        // The human's words end it, and the sentence names what was waited on.
        let mut state = ActorState::default();
        let (one, _one_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, one);
        state.running.insert(1);
        mailbox.send(AgentMsg::Nudge("status?".into())).unwrap();
        let started = Instant::now();
        let told = wait_on_tool(&actor, &mut state, &cancel, Target::Agent(1)).unwrap();
        assert!(told.contains("interrupted"), "{told}");
        assert!(told.contains("#1 is still running"), "{told}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the message ends it, not the timeout: {:?}",
            started.elapsed()
        );

        // A sibling's lock is not this call's business: a ready target answers
        // without the clock moving while the machine is held.
        let mut state = ActorState::default();
        let (one, _one_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, one);
        note_completion(&mut state, 1, 1, Outcome::Finished("done it".into()));
        actor.ctx.registry.take_machine(2, "cargo bench").unwrap();
        let before = clock.elapsed();
        let answer = wait_on_tool(&actor, &mut state, &cancel, Target::Agent(1)).unwrap();
        assert!(answer.contains("#1 done: done it"), "{answer}");
        assert_eq!(clock.elapsed(), before, "the lock is not waited for");
        actor.ctx.registry.release_machine(2);

        // A row the books cannot move — a seeded child (H30) — is named for
        // what it is instead of being slept on for ten minutes.
        let mut state = ActorState::default();
        let (one, _one_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, one);
        let before = clock.elapsed();
        let answer = wait_on_tool(&actor, &mut state, &cancel, Target::Agent(1)).unwrap();
        assert_eq!(
            answer,
            "nothing to wait for: #1 is not running and has no result"
        );
        assert_eq!(clock.elapsed(), before);

        // With nothing to hand over it still blocks on the target alone, and
        // the clock is what ends it — naming the one name.
        let mut state = ActorState::default();
        let (one, _one_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, one);
        state.running.insert(1);
        let answer = wait_on_tool(&actor, &mut state, &cancel, Target::Agent(1)).unwrap();
        assert_eq!(answer, "wait timed out — #1 still running");
        assert!(
            clock.elapsed() >= Duration::from_secs(600),
            "the deadline ended it: {:?}",
            clock.elapsed()
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
        let result = wait_tool(&actor, &mut state, &cancel).unwrap();
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

    /// The same rule, end to end: the root delegates, parks in `wait`,
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
    fn a_human_message_reaches_a_root_parked_in_a_wait() {
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
                .calls(vec![tool_call("c1", "wait", json!({}))])
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
        // spawn result — the request whose answer calls `wait`.
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
    /// `run_command`, or a long reasoning pass). The run must not die on it: it
    /// answers the dangling calls, asks for smaller steps, and carries on.
    #[test]
    fn a_cut_off_reply_is_answered_with_smaller_steps() {
        let scripted = Arc::new(
            Scripted::new()
                // The first reply is cut off mid-tool-call: the half-written
                // call it started must never run.
                .cut_off_call(
                    "run_command",
                    "{\"command\": \"cat > big.rs <<'EOF'\\nfn main(",
                )
                // The model then does as it was told, in smaller pieces.
                .calls(vec![tool_call(
                    "c1",
                    "run_command",
                    json!({ "command": "cat > big.rs <<'EOF'\nfn main() {}\nEOF" }),
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
                    tool_call("", "run_command", json!({ "command": "true" })),
                    tool_call("dup", "run_command", json!({ "command": "true" })),
                    tool_call("dup", "edit_file", json!({ "path": "missing.rs" })),
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
                            "run_command",
                            json!({ "command": "printf hi > note.txt" }),
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
        local.reasoning_effort = Some(ReasoningEffort::Max);
        local.thinking = Some(ThinkingMode::Off);
        let (actor, _events, _mailbox) =
            build_actor("knobs", scripted.clone(), ConfigHandle::own(local));
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![Message::system("you are mush"), Message::user("hi")];

        let result = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();
        assert_eq!(result.as_deref(), Some("done"));
        let asked = scripted.asked();
        assert_eq!(asked[0].reasoning_effort.as_deref(), Some("max"));
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

    /// The third case of the class B23 and B25 each closed half of: a reply
    /// whose *framing* broke used to reach the run as `Refused`, so the human
    /// read "the endpoint's reply was refused" about a chunk line that may just
    /// as well have been mush's own leftover — and no bounded retry touched it
    /// (finding B27). Now the reply gets the retries the wire's failures get,
    /// each one announced in B23's shape, and the final line names what broke:
    /// the *reply*, never the endpoint's opinion.
    #[test]
    fn a_broken_frame_is_retried_and_named_as_a_broken_reply() {
        let broken = "malformed chunk size: \"\"";
        let frame = || ModelError::Framing(broken.to_string());
        let scripted = Arc::new(Scripted::new().fails(frame()).fails(frame()).fails(frame()));
        let clock = Arc::new(Advanceable::new());
        let (actor, events, mailbox) = scripted_actor_on_clock("framing", &scripted, clock);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("say something"),
        ];

        let error = run_loop(&actor, &mut state, &mut messages, &cancel).unwrap_err();

        assert!(error.contains("broke before it could be read"), "{error}");
        assert!(error.contains(broken), "the cause is not hidden: {error}");
        assert!(
            !error.contains("refused"),
            "a broken frame is not the endpoint refusing the request: {error}"
        );
        assert_eq!(scripted.asked().len(), 3, "the bounded retry ran");
        let notices: Vec<String> = events
            .events_for(AgentId(7))
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::Notice(line) => Some(line),
                _ => None,
            })
            .collect();
        assert_eq!(notices.len(), 2, "each retry was announced: {notices:?}");
        assert!(notices[0].contains("retrying (2/3)"), "{notices:?}");
        assert!(notices[1].contains("retrying (3/3)"), "{notices:?}");
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
            "run_command",
            json!({ "command": "printf x > secret.txt" }),
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
                .calls(vec![tool_call(
                    "c1",
                    "run_command",
                    json!({ "command": "ls" }),
                )])
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

    /// The cut-off counter counts *consecutive* replies: two truncations, a
    /// turn that lands, then two more must not end the run as "four in a row"
    /// (audit row 16).
    #[test]
    fn a_good_turn_resets_the_cut_off_count() {
        let scripted = Arc::new(
            Scripted::new()
                .cut_off("one")
                .cut_off("two")
                .calls(vec![tool_call(
                    "w",
                    "run_command",
                    json!({ "command": "printf 'a small piece' > piece.txt" }),
                )])
                .cut_off("three")
                .cut_off("four")
                .says("done"),
        );
        let (actor, _rx, _mailbox) = scripted_actor("cut-reset", &scripted);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![Message::user("write everything")];

        let result = run_loop(&actor, &mut state, &mut messages, &cancel);
        assert!(
            result.is_ok(),
            "scattered cut-offs are not a row: {result:?}"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The cap a request carries is the config's, window and all: a run
    /// budgeted against a 120k window asks for the share of it the config
    /// derives rather than the 20_480 that cut a real run off mid-task, and the
    /// number travels under the name the config chose. `Asked` records what the
    /// endpoint was really sent, so this is the number a reply would be cut off
    /// at.
    #[test]
    fn a_request_carries_the_cap_its_window_derives() {
        let scripted = Arc::new(Scripted::new().says("done"));
        let mut cfg = Config::new("http://127.0.0.1:1", "test", None);
        cfg.provider = mush_core::config::Provider::DeepSeek;
        cfg.set_context(120_000);
        // What the *config* derives, not a number spelled again: the arithmetic
        // is `Config`'s and is pinned in its own tests, while what this test is
        // for is that the request carries it (finding T2 §19's class).
        let cap = cfg.reply_cap();
        let (actor, _events, _mailbox) =
            build_actor("reply-cap", scripted.clone(), ConfigHandle::own(cfg));
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut messages = vec![Message::user("hi")];

        run_loop(&actor, &mut state, &mut messages, &cancel).unwrap();
        let asked = scripted.asked();
        assert_ne!(cap, 20_480, "not the fixed cap that cut a real run off");
        assert_eq!(
            asked[0].max_tokens, cap,
            "the window's own cap, under the field this config chose"
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
        state.deferred.push(AgentMsg::Stop(Stop::Human));
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
        root_tx.send(AgentMsg::Stop(Stop::Human)).unwrap();
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
        mailbox.send(AgentMsg::Stop(Stop::Human)).unwrap();

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
    /// ends one — which is what keeps Ctrl-N from leaving an orphan root
    /// behind that still answers to agent #0.
    #[test]
    fn stop_cancels_but_shutdown_ends() {
        let (actor, mailbox) = test_actor("shutdown");
        let mut state = ActorState::default();
        let mut messages = vec![Message::system("you are mush"), Message::user("task")];

        mailbox.send(AgentMsg::Stop(Stop::Human)).unwrap();
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
            absorb(
                &actor,
                &mut state,
                &mut messages,
                AgentMsg::Stop(Stop::Human)
            ),
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
        mailbox.send(AgentMsg::Stop(Stop::Human)).unwrap();

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
            absorb(
                &actor,
                &mut state,
                &mut transcript,
                AgentMsg::Stop(Stop::Human)
            ),
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
            state.running_jobs.contains(&JobId(1)),
            "the owner's books know about it"
        );
        assert!(
            clock.elapsed() >= jobs::CMD_DETACH_AFTER,
            "the deadline is what moved it, not the end of the command: {:?}",
            clock.elapsed()
        );

        // And its end lands in the owner's own mailbox, once, saying it was
        // stopped rather than blamed on an exit code.
        let stopped = actor.ctx.registry.stop(actor.id, JobId(1)).unwrap();
        assert_eq!(stopped, "stopping job #c1");
        match actor.rx.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone { id, line, news }) => {
                assert_eq!(id, JobId(1));
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
            Some((7, "cargo bench".to_string(), Some(JobId(1)))),
            "the job holds the machine, not just its first sixty seconds"
        );

        // A sibling is refused, and told who has it — the whole point of the
        // lock is that everyone else knows what to wait for.
        let held = actor.ctx.registry.machine_free_for(9).unwrap_err();
        let refusal = Refused::Machine(held).message(9);
        assert!(refusal.starts_with("#7 holds the machine"), "{refusal}");
        assert!(
            refusal.contains("do not retry"),
            "the refusal must not read as try-again-now (H13): {refusal}"
        );

        // And the job gives it up when it ends, not before.
        assert_eq!(
            actor.ctx.registry.stop(actor.id, JobId(1)).unwrap(),
            "stopping job #c1"
        );
        match actor.rx.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone { id, .. }) => assert_eq!(id, JobId(1)),
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

    /// `exclusive=true` is not exclusive against its own owner: a second
    /// exclusive call from the agent that already holds the machine is refused.
    /// The first claim's record — which names the *job* it became — is not
    /// overwritten, so the first call's own release at its end cannot free a
    /// machine its job still holds and let the sibling it refuses run beside it.
    #[test]
    fn a_second_exclusive_call_from_the_holder_is_refused() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("holder-exclusive", machine, clock);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));

        // The first call outlives `CMD_DETACH_AFTER`, so it hands its claim to
        // the job it becomes (the handover test above): the tool call ends, the
        // claim is `Some(job)`.
        let report = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "cargo bench", "exclusive": true }),
            &cancel,
        )
        .unwrap();
        assert!(report.contains("detached as #c1"), "{report}");
        let held = Some((7, "cargo bench".to_string(), Some(JobId(1))));
        assert_eq!(actor.ctx.registry.held(), held);

        // A second exclusive call, from the same agent, is refused at once —
        // the lock is its own, so there is no queue to sit in.
        let refused = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "cargo bench --other", "exclusive": true }),
            &cancel,
        )
        .unwrap_err();
        let ToolError::Refused(why) = refused else {
            panic!("its own lock is a refusal, not a failure of the work");
        };
        assert!(why.starts_with("you hold the machine"), "{why}");
        assert!(why.contains("cargo bench"), "{why}");
        assert!(why.contains("control stop"), "{why}");
        assert!(
            !why.contains("queued"),
            "the holder never queues for its own lock: {why}"
        );

        // The machine is still held, under the first job's name: the refusal
        // changed nothing, and the first call's release did not take the job's
        // claim away.
        assert_eq!(
            actor.ctx.registry.held(),
            held,
            "the first claim (and the job it names) survived"
        );
        assert!(
            actor.ctx.registry.machine_free_for(9).is_err(),
            "and a sibling is still refused"
        );
        actor.ctx.registry.kill_all();
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// And the holder's *non-exclusive* work runs beside its own exclusive job:
    /// the lock coordinates siblings, so the call is admitted — and it must not
    /// re-write the holder record, because a claim it did not take would be
    /// released at its end (the bug above), freeing the machine while the job
    /// still runs.
    #[test]
    fn a_holder_runs_beside_its_own_exclusive_job_without_losing_it() {
        let machine = Arc::new(
            ScriptedMachine::new()
                .runs(Script::hangs())
                .runs(Script::exits(0).says("renamed it")),
        );
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("holder-beside-own", machine, clock);
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

        // Its own ordinary command runs beside the job it owns.
        let report = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "git diff" }),
            &cancel,
        )
        .unwrap();
        assert!(report.contains("renamed it"), "{report}");
        assert!(
            !report.contains("held the machine"),
            "the holder's own lock is not a note it needs: {report}"
        );

        assert_eq!(
            actor.ctx.registry.held(),
            Some((7, "cargo bench".to_string(), Some(JobId(1)))),
            "the exempt call took no claim and released none"
        );
        assert!(
            actor.ctx.registry.machine_free_for(9).is_err(),
            "so a sibling is still refused while the benchmark runs"
        );
        actor.ctx.registry.kill_all();
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
        assert!(why.contains("do not retry"), "{why}");
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

    /// The wait the refusal points at: a subagent holds nothing of its own and
    /// a sibling holds the machine, so `wait` blocks — on the clock here, which
    /// lets the lock go on its first slice — and then says which hold it waited
    /// out, because the refused call can now run.
    #[test]
    fn a_wait_for_the_machine_returns_when_the_holder_lets_go() {
        let clock = Arc::new(ReleasesOnSleep::new());
        let (actor, _mailbox) = scripted_tools_actor(
            "wait-machine",
            Arc::new(ScriptedMachine::new()),
            clock.clone(),
        );
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        actor.ctx.registry.take_machine(2, "cargo bench").unwrap();
        clock.release_after(actor.ctx.registry.clone(), 2);

        let answer = exec_tool(&actor, &mut state, ToolName::Wait, &json!({}), &cancel).unwrap();
        assert!(answer.contains("the machine is free now"), "{answer}");
        assert!(answer.contains("#2's exclusive command"), "{answer}");
        assert!(answer.contains("cargo bench"), "{answer}");
        assert!(
            answer.contains("the lock is free for your next command"),
            "the answer is the fact the model needs next: {answer}"
        );
        assert!(
            actor.ctx.registry.held().is_none(),
            "the clock's release is what freed it"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// And a hold that outlasts the wait: the timeout names the holder, the road
    /// back (`wait` again — a hold can outlive many waits) and the two moves for
    /// when it cannot: work that needs no shell, or an honest end to the run.
    /// Nothing this agent can call ends *another agent's* hold — the holder here
    /// is a sibling, which no `control` of this agent's reaches (the holder that
    /// is its own child is the one road with a call, pinned below). The clock,
    /// not a real ten minutes, is what reaches the deadline.
    #[test]
    fn a_wait_that_no_holder_ends_times_out_and_names_it() {
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor(
            "wait-machine-timeout",
            Arc::new(ScriptedMachine::new()),
            clock.clone(),
        );
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        actor.ctx.registry.take_machine(2, "cargo bench").unwrap();

        let answer = exec_tool(&actor, &mut state, ToolName::Wait, &json!({}), &cancel).unwrap();
        assert!(answer.contains("wait timed out"), "{answer}");
        assert!(answer.contains("#2's exclusive command"), "{answer}");
        assert!(answer.contains("still holds the machine"), "{answer}");
        assert!(
            answer.contains("wait again"),
            "the road back the refusal named is not withdrawn at the timeout: {answer}"
        );
        assert!(
            answer.contains("do work that needs no shell")
                && answer.contains("finish this run and say you are blocked"),
            "and the two moves for when it cannot are named: {answer}"
        );
        assert!(
            clock.elapsed() >= Duration::from_secs(WAIT_TIMEOUT_SECS),
            "the deadline ended it: {:?}",
            clock.elapsed()
        );
        assert!(
            actor.ctx.registry.machine_free_for(9).is_err(),
            "the refusal took nothing from the holder"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The one holder whose hold the agent can end itself. A child of its own
    /// can take the lock — a detached benchmark job of a child is still that
    /// child's work, and `control stop` on the child lands as `kill_owned`,
    /// which kills the job and gives the machine back — so the timeout names
    /// that call instead of the two moves left for a hold nothing it can call
    /// reaches. The state is an ordinary one: a job dies with its owner, not
    /// with its run, so a child at rest whose job holds the lock is exactly
    /// what a wait on a busy machine meets.
    #[test]
    fn a_timeout_held_by_the_agents_own_child_names_the_stop_that_ends_it() {
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor(
            "wait-machine-own-child",
            Arc::new(ScriptedMachine::new()),
            clock.clone(),
        );
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let (child, _child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child);
        // The child's result has been read, so the wait has nothing to hand
        // over first: what is left of this call is the machine.
        state.completed.insert(
            1,
            Completion {
                run: 1,
                outcome: Outcome::Finished("started the benchmark".into()),
            },
        );
        state.delivered.insert(1, 1);
        actor.ctx.registry.take_machine(1, "cargo bench").unwrap();

        let answer = exec_tool(&actor, &mut state, ToolName::Wait, &json!({}), &cancel).unwrap();
        assert!(answer.contains("wait timed out"), "{answer}");
        assert!(answer.contains("#1's exclusive command"), "{answer}");
        assert!(
            answer.contains("it is your own child — `control stop #1` ends the hold"),
            "the one call that ends this hold is named: {answer}"
        );
        assert!(
            !answer.contains("nothing you can call ends it"),
            "a child's hold is not one of the holds nothing reaches: {answer}"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// And what the interrupted-wait sentence is allowed to claim about why the
    /// wait was still going. A wait the *machine* alone keeps alive has nothing
    /// of this agent's running — that is the state the lock road creates, an
    /// agent waiting on a sibling with no work of its own — and "your work is
    /// still running" was the false half of the sentence exactly there.
    #[test]
    fn an_interrupted_wait_says_what_it_was_waiting_for() {
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor(
            "wait-interrupt-machine",
            Arc::new(ScriptedMachine::new()),
            clock.clone(),
        );
        let cancel = Arc::new(AtomicBool::new(false));
        actor.ctx.registry.take_machine(2, "cargo bench").unwrap();
        let mut state = ActorState::default();
        state
            .deferred
            .push(AgentMsg::Nudge("are you there?".into()));

        let answer = exec_tool(&actor, &mut state, ToolName::Wait, &json!({}), &cancel).unwrap();
        assert!(
            answer.contains("interrupted — the human wrote to you while you waited"),
            "{answer}"
        );
        assert!(
            !answer.contains("your work is still running"),
            "nothing of this agent's is running; the machine is what kept the wait alive: {answer}"
        );
        assert!(
            answer.contains("#2's exclusive command") && answer.contains("still holds the machine"),
            "so the holder is what it names: {answer}"
        );

        // And with work of its own in flight, the old half is the true one.
        let (child, _child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child);
        state.running.insert(1);
        let answer = exec_tool(&actor, &mut state, ToolName::Wait, &json!({}), &cancel).unwrap();
        assert!(
            answer.contains("your work is still running"),
            "a wait on its own work says so: {answer}"
        );
        assert!(!answer.contains("still holds the machine"), "{answer}");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A sibling's `exclusive` claim can lose a race: the lock check passes,
    /// another agent takes the lock, and `take_machine` refuses — a call that
    /// never sat in `LOCK_QUEUE`. Its sentence must not claim the queue it never
    /// joined (the falsehood `root_message` was written to prevent for the
    /// root), while keeping the road back a subagent really has: the holder is
    /// another agent, so `wait` blocks on the machine.
    #[test]
    fn a_sibling_refusal_never_claims_a_queue_it_did_not_join() {
        let (actor, _mailbox) = test_actor("unqueued-refusal");
        let held = || jobs::Held {
            agent: 2,
            command: "cargo bench".to_string(),
        };

        let queued = machine_refusal(&actor, held(), true);
        assert!(
            queued.contains("this call queued and the lock was still held"),
            "{queued}"
        );

        let unqueued = machine_refusal(&actor, held(), false);
        assert!(
            unqueued.contains("refused at once, without queueing"),
            "{unqueued}"
        );
        assert!(!unqueued.contains("this call queued"), "{unqueued}");
        assert!(
            unqueued.contains("wait blocks until the machine is free")
                && unqueued.contains("do not retry it in a loop"),
            "the same road back, whichever way it was refused: {unqueued}"
        );

        // The holder's own second claim reads the same on either road: it never
        // queues, and its sentence is about the lock it already owns.
        let own = jobs::Held {
            agent: actor.id,
            command: "cargo bench".to_string(),
        };
        let mine = machine_refusal(&actor, own, false);
        assert!(mine.starts_with("you hold the machine"), "{mine}");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The root works beside a held lock, so a sibling's hold is not a wait it
    /// sits through: with nothing of its own behind the call, its `wait` still
    /// answers at once. Blocking here would be finding H13's blindness again,
    /// with the orchestrator parked on a child's benchmark instead of refused
    /// by it.
    #[test]
    fn the_roots_wait_does_not_block_on_a_siblings_lock() {
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor(
            "root-wait-machine",
            Arc::new(ScriptedMachine::new()),
            clock.clone(),
        );
        // The helper builds agent 7; only the root wears id 0, and the
        // exemption is about that id.
        let actor = Actor {
            id: AgentId::ROOT.0,
            ..actor
        };
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        actor.ctx.registry.take_machine(2, "cargo bench").unwrap();

        let answer = exec_tool(&actor, &mut state, ToolName::Wait, &json!({}), &cancel).unwrap();
        assert_eq!(answer, NOTHING_TO_WAIT_FOR);
        assert_eq!(clock.elapsed(), Duration::ZERO, "no wait was spent");
        assert!(
            actor.ctx.registry.machine_free_for(9).is_err(),
            "and the wait took nothing from the holder"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The machine is only what a wait has *left* once it has nothing unread
    /// to hand over: a finished child's result comes over at once, even while a
    /// sibling holds the lock, because the model asked for the result — not for
    /// the machine. The lock still rides along as a fact, so the next move is
    /// not a blind retry.
    #[test]
    fn a_finished_result_is_handed_over_before_the_machine_is_waited_out() {
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor(
            "wait-machine-result",
            Arc::new(ScriptedMachine::new()),
            clock.clone(),
        );
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let (child, _child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child);
        state.completed.insert(
            1,
            Completion {
                run: 1,
                outcome: Outcome::Finished("wrote the parser".into()),
            },
        );
        actor.ctx.registry.take_machine(2, "cargo bench").unwrap();

        let answer = exec_tool(&actor, &mut state, ToolName::Wait, &json!({}), &cancel).unwrap();
        assert!(answer.contains("#1 done: wrote the parser"), "{answer}");
        assert!(
            answer.contains("the machine is still held by #2's exclusive command")
                && answer.contains("cargo bench"),
            "the lock rides along with the result, so the next move is informed: {answer}"
        );
        assert_eq!(clock.elapsed(), Duration::ZERO, "no wait was spent");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A recap is not news: a result the model has already read does not take a
    /// wait away from the machine. That is the road the lock refusal points at,
    /// and an agent that has read everything it owns must still be able to take
    /// it — the earlier shape of this rule asked whether the agent owned *any*
    /// child or job, so a single finished job locked it out of the wait for
    /// good.
    #[test]
    fn a_read_result_does_not_take_a_wait_away_from_the_machine() {
        let clock = Arc::new(ReleasesOnSleep::new());
        let (actor, _mailbox) = scripted_tools_actor(
            "wait-machine-recap",
            Arc::new(ScriptedMachine::new()),
            clock.clone(),
        );
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let (child, _child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child);
        state.completed.insert(
            1,
            Completion {
                run: 1,
                outcome: Outcome::Finished("wrote the parser".into()),
            },
        );
        state.delivered.insert(1, 1);
        actor.ctx.registry.take_machine(2, "cargo bench").unwrap();
        clock.release_after(actor.ctx.registry.clone(), 2);

        let answer = exec_tool(&actor, &mut state, ToolName::Wait, &json!({}), &cancel).unwrap();
        assert!(answer.contains("the machine is free now"), "{answer}");
        assert!(
            answer.contains("wrote the parser"),
            "the recap rides along with the fact the machine is free: {answer}"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// The root is exempt from the lock, not from the rule: its *exclusive*
    /// call meets a sibling's lock as a refusal. That refusal is immediate —
    /// `LOCK_QUEUE` is a sibling's road and the root is never sent down it — so
    /// the sentence must not claim the call queued.
    #[test]
    fn the_roots_exclusive_call_is_refused_without_queueing() {
        // No script on the machine: a command that runs at all has spent its
        // refusal on nothing.
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor(
            "root-exclusive-immediate",
            Arc::new(ScriptedMachine::new()),
            clock.clone(),
        );
        let actor = Actor {
            id: AgentId::ROOT.0,
            ..actor
        };
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        actor.ctx.registry.take_machine(2, "cargo bench").unwrap();

        let refused = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "cargo bench", "exclusive": true }),
            &cancel,
        )
        .unwrap_err();
        let ToolError::Refused(why) = refused else {
            panic!("a held lock refuses the root's exclusive claim");
        };
        assert!(why.starts_with("#2 holds the machine"), "{why}");
        assert!(why.contains("you are the root"), "{why}");
        assert!(why.contains("without queueing"), "{why}");
        assert!(
            !why.contains("this call queued"),
            "the root never reaches the queue, so its refusal must not claim one: {why}"
        );
        assert!(
            clock.elapsed() < LOCK_QUEUE,
            "no queue was spent on the root: {:?}",
            clock.elapsed()
        );
        assert!(
            actor.ctx.registry.machine_free_for(9).is_err(),
            "and the refusal took nothing from the holder"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A job mush *killed* is news: four hours of silence or a run past the
    /// output limit is a reason to act, and the line saying why must reach the
    /// owner's run. A *stop* is the human's doing — the line waits in the
    /// transcript, and the owner's run is not paid for by it.
    #[test]
    fn a_job_mush_killed_wakes_its_owner_but_a_stop_does_not() {
        let machine = Arc::new(
            ScriptedMachine::new()
                .runs(Script::hangs())
                .runs(Script::hangs()),
        );
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("job-news", machine, clock.clone());
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut transcript = vec![Message::system("you are mush")];

        // The ceiling: the job's own thread ends it, and the completion it
        // sends must be news — the owner's run starts so it reads why.
        let report = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "cargo bench", "detach": true }),
            &cancel,
        )
        .unwrap();
        assert!(report.contains("detached as #c1"), "{report}");
        clock.advance(jobs::JOB_MAX_AGE + Duration::from_secs(1));
        let (line, news) = match actor.rx.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone { id, line, news }) => {
                assert_eq!(id, JobId(1));
                (line, news)
            }
            _ => panic!("the job must report its own end"),
        };
        assert!(line.contains("ran past the 4h ceiling"), "{line}");
        assert!(news, "the owner has to read this: {line}");
        let folded = absorb(
            &actor,
            &mut state,
            &mut transcript,
            AgentMsg::CommandDone {
                id: JobId(1),
                line,
                news,
            },
        );
        assert!(
            matches!(folded, Fold::Run),
            "so the run that reads it starts"
        );

        // A stop: `control stop` on the second job. The line still arrives —
        // the owner reads it whenever it next runs — but it wakes nobody.
        let report = exec_tool(
            &actor,
            &mut state,
            ToolName::RunCommand,
            &json!({ "command": "npm run dev", "detach": true }),
            &cancel,
        )
        .unwrap();
        assert!(report.contains("detached as #c2"), "{report}");
        assert_eq!(
            actor.ctx.registry.stop(actor.id, JobId(2)).unwrap(),
            "stopping job #c2"
        );
        let (line, news) = match actor.rx.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone { id, line, news }) => {
                assert_eq!(id, JobId(2));
                (line, news)
            }
            _ => panic!("a stopped job reports its own end too"),
        };
        assert!(line.contains("stopped after"), "{line}");
        assert!(!news, "a stop is the human's doing: no run starts: {line}");
        let folded = absorb(
            &actor,
            &mut state,
            &mut transcript,
            AgentMsg::CommandDone {
                id: JobId(2),
                line,
                news,
            },
        );
        assert!(
            matches!(folded, Fold::Idle),
            "the line is in the transcript, and the run is not paid for"
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
        let status = call(ToolName::Status, json!({})).unwrap();
        assert!(status.contains("#c1 running "), "{status}");
        assert!(status.contains("npm run dev"), "{status}");

        let stopped = call(ToolName::Control, json!({ "id": "c1", "action": "stop" })).unwrap();
        assert_eq!(stopped, "stopping job #c1");
        // A stop is a request to the job's own thread; the report is what the
        // owner reads next, and `status` then says it ended.
        match actor.rx.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone { line, .. }) => assert!(line.contains("stopped after")),
            other => panic!("the stop must be reported: {:?}", other.is_ok()),
        }
        let status = call(ToolName::Status, json!({})).unwrap();
        assert!(status.contains("stopped after"), "{status}");
        // An id that was never a job is an error the model can correct, and
        // another action is one it cannot use.
        assert!(call(ToolName::Control, json!({ "id": "c99", "action": "stop" })).is_err());
        assert!(call(ToolName::Control, json!({ "id": "c1", "action": "poke" })).is_err());
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
        let minute = Duration::from_secs(60);
        // A kill from outside the watcher: the process died of a signal, and
        // that is a cancel — not `[killed by signal 9]`, which is the kill's
        // own death and reads as somebody else's doing. This is the arm finding
        // S4 added.
        assert!(matches!(
            ending(Ended::Signalled(9), true),
            Ended::Stopped(jobs::Stopped::Cancelled)
        ));
        // A command that ended by itself keeps its status, whoever else's signal
        // it was: its own exit code, or the signal that killed it with nobody
        // in mush asking (finding B6).
        assert!(matches!(ending(Ended::Exited(3), false), Ended::Exited(3)));
        assert!(matches!(
            ending(Ended::Signalled(11), false),
            Ended::Signalled(11)
        ));
        // The watcher's own kills keep their own reasons. They set the same
        // flag on the way out, so an arm that keyed off the flag rather than
        // off the *shape* of the end would report a timeout as a cancel — which
        // is exactly what happened when this was written, and what
        // `a_command_that_runs_forever_is_killed_on_time` and
        // `a_runaway_writer_is_stopped_at_the_output_limit` caught.
        for reason in [
            jobs::Stopped::TimedOut,
            jobs::Stopped::TooMuchOutput,
            jobs::Stopped::Cancelled,
            jobs::Stopped::RanTooLong,
        ] {
            assert!(matches!(
                ending(Ended::Stopped(reason), true),
                Ended::Stopped(kept) if kept == reason
            ));
        }

        // And every arm of the table has its own sentence: the three ways the
        // watcher stops a command, an exit, the signal that killed it, the end
        // that is not one — a command handed to the job registry, which
        // `run_shell` returns from before it builds a report, so no run reads it
        // — and the nameless end a platform with no exit status produces, which
        // no unix child can (refactor R15, finding H26).
        let notes = vec![
            end_note(&Ended::Exited(0), minute, true, mush_core::CMD_CAP),
            end_note(&Ended::Signalled(9), minute, true, mush_core::CMD_CAP),
            end_note(
                &Ended::Stopped(jobs::Stopped::TimedOut),
                minute,
                true,
                mush_core::CMD_CAP,
            ),
            end_note(
                &Ended::Stopped(jobs::Stopped::Cancelled),
                minute,
                true,
                mush_core::CMD_CAP,
            ),
            end_note(
                &Ended::Stopped(jobs::Stopped::TooMuchOutput),
                minute,
                true,
                4_096,
            ),
            end_note(
                &Ended::Stopped(jobs::Stopped::RanTooLong),
                minute,
                true,
                mush_core::CMD_CAP,
            ),
            end_note(&Ended::Detached, minute, true, mush_core::CMD_CAP),
            end_note(&Ended::Unknown, minute, true, mush_core::CMD_CAP),
        ];
        let mut unique = notes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            notes.len(),
            "two ends share a sentence: {notes:?}"
        );
        assert_eq!(notes[0], "[exit 0]");
        assert_eq!(
            notes[1], "[killed by signal 9]",
            "a signal death is not an exit code"
        );
        assert!(notes[2].starts_with("[timed out after 60s"), "{}", notes[2]);
        assert_eq!(notes[3], "[cancelled]");
        assert!(
            notes[4].starts_with("[killed: output passed"),
            "{}",
            notes[4]
        );
        assert!(notes[5].starts_with("[killed: it ran past"), "{}", notes[5]);
        assert!(
            notes[6].contains("registry"),
            "a detached command is not a timed-out one: {}",
            notes[6]
        );
        assert_eq!(
            notes[7], "[no exit status: neither an exit code nor a signal]",
            "an end with no status says so instead of naming a code nothing returns"
        );
        // The timeout's sentence says why it could not detach when the budget
        // was the reason.
        let full = end_note(
            &Ended::Stopped(jobs::Stopped::TimedOut),
            minute,
            false,
            mush_core::CMD_CAP,
        );
        assert!(full.contains("budget is full"), "{full}");
        assert!(!notes[2].contains("budget"), "{}", notes[2]);
    }

    /// The quit half of finding S4, at the seam: a command killed from *outside*
    /// the watcher — nothing sets the run's own cancel flag, which is the shape
    /// a quit has — is reported as a cancel rather than as the signal that
    /// killed it. The kill is the registry's, the same call `App::drop` makes.
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
        mailbox.send(AgentMsg::Stop(Stop::Human)).unwrap();

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
    /// `status`/`control`/`wait`, so the schema has to
    /// carry them, and the executors have to answer a leaf exactly as they
    /// answer the root.
    #[test]
    fn a_leaf_agent_has_no_delegation_but_still_manages_its_own_job() {
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
        assert!(names.contains(&"status"), "{names:?}");
        assert!(names.contains(&"control"), "{names:?}");
        assert!(names.contains(&"wait"), "{names:?}");
        assert!(!names.contains(&"spawn_agent"), "{names:?}");

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
        let status = call(ToolName::Status, json!({})).unwrap();
        assert!(status.contains("#c1 running "), "{status}");
        assert!(status.contains("npm run dev"), "{status}");
        assert_eq!(
            call(ToolName::Control, json!({ "id": "c1", "action": "stop" })).unwrap(),
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
        state.running_jobs.insert(JobId(1));
        state.running_jobs.insert(JobId(2));
        let mut messages = vec![Message::system("you are mush")];

        let folded = absorb(
            &actor,
            &mut state,
            &mut messages,
            AgentMsg::CommandDone {
                id: JobId(1),
                line: "#c1 done: exit 0 · 3m12s · cargo test — test result: ok".into(),
                news: true,
            },
        );
        assert!(matches!(folded, Fold::Run), "a result is work to answer");
        assert_eq!(
            messages.last().unwrap().text(),
            "#c1 done: exit 0 · 3m12s · cargo test — test result: ok"
        );
        assert!(
            state.delivered_jobs.contains(&JobId(1)),
            "and it counts as read"
        );
        assert!(
            !state.running_jobs.contains(&JobId(1)),
            "and as no longer running"
        );

        let folded = absorb(
            &actor,
            &mut state,
            &mut messages,
            AgentMsg::CommandDone {
                id: JobId(2),
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
        state.running_jobs.insert(JobId(2));
        state.done_jobs.insert(
            JobId(2),
            JobReport {
                line: "#c2 done: exit 0 · 12s · npm test — ok".into(),
                news: true,
            },
        );
        state.running_jobs.insert(JobId(3));
        state.done_jobs.insert(
            JobId(3),
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
        assert!(
            state.delivered_jobs.contains(&JobId(2)) && state.delivered_jobs.contains(&JobId(3))
        );

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
                .calls(vec![tool_call(
                    "c1",
                    "run_command",
                    json!({ "command": "ls" }),
                )])
                .held(gate.clone())
                .calls(vec![tool_call(
                    "c2",
                    "run_command",
                    json!({ "command": "ls" }),
                )])
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
        // asks for it: `wait` is never called.
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
                    "run_command",
                    json!({ "command": "printf hello > note.txt" }),
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
        assert!(
            !asked[0].tool_schemas.is_empty(),
            "the first turn offered the tools"
        );
        let carried = asked[1].messages.last().unwrap();
        assert_eq!(carried.role, "tool");
        assert_eq!(
            carried.text(),
            "[exit 0]",
            "the command ran; it wrote no output for the tool result to carry"
        );
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
                .calls(vec![tool_call(
                    "c0",
                    "run_command",
                    json!({ "command": "ls" }),
                )])
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
                    "run_command",
                    json!({ "command": "printf 'isolated work' > iso.txt" }),
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
                        "base": "main"
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
        assert!(merged.is_ok(), "a merge must merge: {merged:?}");
        assert!(
            root.join("iso.txt").exists(),
            "after the merge the file must be in the human's workspace"
        );
        let removed = git::run(&root, &["worktree", "remove", ".mush/wt/1"]);
        assert!(
            removed.is_ok(),
            "`git worktree remove` must remove the worktree: {removed:?}"
        );
        let deleted = git::run(&root, &["branch", "-D", "mush/1"]);
        assert!(
            deleted.is_ok(),
            "`git branch -D` must delete the branch: {deleted:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A run that committed nothing leaves nothing on disk. The worktree and the
    /// branch are reclaimed at the run's own end, and the UI is told — the row
    /// must stop naming `.mush/wt/<id>` and `git diff HEAD...mush/<id>`, both of
    /// which are gone (finding H10, U13). Without this, every isolated run that
    /// changed nothing left a worktree behind for good, and its branch was what
    /// the next `worktree add -b mush/<id>` died on.
    #[test]
    fn a_run_that_committed_nothing_leaves_no_worktree_behind() {
        let root = init_git_repo("clean-run");
        let scripted = Arc::new(
            Scripted::new()
                // The child: asked, answered, nothing written and nothing run.
                .when(|asked: &Asked| asked.depth() == Some(1))
                .says("nothing to do — b.txt is already there")
                .when(|asked: &Asked| asked.saw("#1 done"))
                .says("child finished")
                .when(|asked: &Asked| asked.saw("spawned agent"))
                .calls(vec![tool_call("c1", "wait", json!({}))])
                .calls(vec![tool_call(
                    "c0",
                    "spawn_agent",
                    json!({
                        "brief": "check whether b.txt exists in the repository",
                        "base": "main"
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
                Message::user("delegate a look at the repository".to_string()),
            ]))
            .unwrap();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 2),
            "the child and the woken root must each finish: {seen:?}"
        );
        assert_eq!(seen.errors, Vec::<String>::new());

        assert!(
            !git::worktree_path(&root, 1).exists(),
            "the run committed nothing, so its worktree is gone"
        );
        assert_eq!(
            git_rev_parse(&root, "mush/1"),
            None,
            "and the branch with it: the name is free for the next child"
        );
        assert!(
            events.events().iter().any(|(id, event)| id.0 == 1
                && matches!(
                    event,
                    AgentEvent::Reclaimed {
                        landing: git::Landing::NothingCommitted
                    }
                )),
            "the row is told what really happened — the branch never gained a commit \
             of its own — or `merged` is painted over a run only ever made of reads"
        );
        // Nothing was written anywhere: the root's own checkout is untouched.
        assert_eq!(
            git::run(&root, &["status", "--porcelain"]).unwrap_or_default(),
            "",
            "a run that changed nothing changed nothing"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The other half, at the same call site: a run that *did* commit keeps its
    /// worktree and its branch, and no reclamation is reported. The sweep is not
    /// a cleanup of everything a run leaves — it is the one case where there is
    /// provably nothing to keep (finding H10).
    #[test]
    fn a_run_that_committed_keeps_its_worktree_and_says_nothing_was_reclaimed() {
        let root = init_git_repo("kept-run");
        let scripted = Arc::new(
            Scripted::new()
                .when(|asked: &Asked| asked.depth() == Some(1))
                .calls(vec![tool_call(
                    "c1",
                    "run_command",
                    json!({ "command": "printf 'kept work' > kept.txt" }),
                )])
                .when(|asked: &Asked| asked.depth() == Some(1) && asked.saw("kept.txt"))
                .says("wrote kept.txt in my worktree")
                .when(|asked: &Asked| asked.saw("#1 done"))
                .says("child finished")
                .when(|asked: &Asked| asked.saw("spawned agent"))
                .calls(vec![tool_call("c2", "wait", json!({}))])
                .calls(vec![tool_call(
                    "c0",
                    "spawn_agent",
                    json!({
                        "brief": "create a file called kept.txt containing exactly: kept work",
                        "base": "main"
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
                Message::user("delegate a file".to_string()),
            ]))
            .unwrap();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 2),
            "the child and the woken root must each finish: {seen:?}"
        );

        assert_eq!(
            fs::read_to_string(root.join(".mush/wt/1/kept.txt"))
                .ok()
                .as_deref(),
            Some("kept work"),
            "unmerged work stays exactly where the run left it"
        );
        assert!(
            git_rev_parse(&root, "mush/1").is_some(),
            "and its branch stays readable — the human lands the work with it"
        );
        assert!(
            !events
                .events()
                .iter()
                .any(|(_, event)| matches!(event, AgentEvent::Reclaimed { .. })),
            "nothing was reclaimed, so nothing says it was"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The cap is a refusal *before* anything is created. With `MAX_WORKTREES`
    /// worktrees that no sweep will take, an isolated spawn is refused by name —
    /// the ids, the paths and the commands that clear one — and the number it
    /// would have drawn is not spent, so the retry after a human clears one is
    /// consecutive (finding H17, H10). The failure this replaces is git's own,
    /// after the fact: a fatal `worktree add` with the id already burned.
    #[test]
    fn the_worktree_cap_refuses_a_spawn_before_the_id_is_taken() {
        let root = init_git_repo("cap");
        // Every worktree holds an uncommitted file, which is the cheapest way to
        // be unlandable — the sweep will not take them and the cap counts them.
        for id in 1..=git::MAX_WORKTREES as u64 {
            let path = git::worktree_path(&root, id);
            git::run(
                &root,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    &git::branch_name(id),
                    path.to_str().unwrap(),
                    "HEAD",
                ],
            )
            .expect("the repository can hold this worktree");
            fs::write(path.join("uncommitted.txt"), "not committed\n").unwrap();
        }
        // The refusal rule comes first and is the only one with a matcher: the
        // catch-all spawn rule answers the run's opening call, and the refusal
        // text in the transcript is what the *second* call is answered with.
        let scripted = Arc::new(
            Scripted::new()
                .when(|asked: &Asked| asked.saw("none of them is landable"))
                .says("the spawn was refused")
                .calls(vec![tool_call(
                    "c0",
                    "spawn_agent",
                    json!({ "brief": "create a file called cap.txt", "base": "main" }),
                )]),
        );
        let events = Recorder::new();
        let handle = spawn_scripted(
            Config::new("http://127.0.0.1:1", "scripted", None),
            events.clone(),
            root.clone(),
            scripted.clone(),
        );
        handle
            .tx
            .send(AgentMsg::Run(vec![
                Message::system(prompt::system_prompt(root.to_str().unwrap())),
                Message::user("spawn an isolated child".to_string()),
            ]))
            .unwrap();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done >= 1),
            "the root's run must end: {seen:?}"
        );
        assert_eq!(seen.errors, Vec::<String>::new());

        let refusal = events
            .events()
            .into_iter()
            .find_map(|(_, event)| match event {
                AgentEvent::Message(message)
                    if message.text().contains("none of them is landable") =>
                {
                    Some(message.text().to_string())
                }
                _ => None,
            })
            .expect("the refusal reaches the model as a tool result");
        assert!(
            refusal.contains(&format!("{} isolated worktrees", git::MAX_WORKTREES)),
            "the count and the limit are both in it: {refusal}"
        );
        assert!(refusal.contains("#1 (.mush/wt/1)"), "{refusal}");
        assert!(refusal.contains("git worktree remove --force"), "{refusal}");
        assert!(refusal.contains("git branch -d mush/<id>"), "{refusal}");

        assert!(
            !events
                .events()
                .iter()
                .any(|(_, event)| matches!(event, AgentEvent::Spawned { .. })),
            "no child was created"
        );
        assert_eq!(
            handle.ids.agents_floor(),
            1,
            "and the number the refusal saved is still there for the next spawn"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A tree whose isolated child has finished its first run, left `iso.txt` on
    /// `mush/1`, and is now idle — the state a hand-run merge or discard starts
    /// from.
    ///
    /// Returns the repository, the recorded events, and the *live* child's
    /// mailbox, so a test can do what the commands do and then nudge it. The
    /// child's script answers the nudge with a `run_command` that writes
    /// `extra.txt`, so a test that forgot to land the worktree would see the
    /// file really written.
    fn finished_isolated_child(label: &str) -> (PathBuf, Arc<Recorder>, Sender<AgentMsg>) {
        let root = init_git_repo(label);
        let scripted = Arc::new(
            Scripted::new()
                .when(|asked: &Asked| asked.depth() == Some(1) && !asked.saw("wrote iso.txt"))
                .calls(vec![tool_call(
                    "c1",
                    "run_command",
                    json!({ "command": "printf 'isolated work' > iso.txt" }),
                )])
                // The nudge turn: only ever reached if the worktree-nudge is
                // allowed to run, which is the bug this test pins.
                .when(|asked: &Asked| asked.depth() == Some(1) && asked.saw("write extra.txt"))
                .calls(vec![tool_call(
                    "c2",
                    "run_command",
                    json!({ "command": "printf phantom > extra.txt" }),
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
                        "base": "main"
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

    /// Do what a merge by hand does to a child, with real git: land the branch,
    /// then reclaim the worktree and the branch.
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

    /// Do what a discard by hand does: reclaim the worktree and delete the
    /// branch, without merging.
    fn land_with_discard(root: &Path) {
        let worktree = git::worktree_path(root, 1);
        git::run(
            root,
            &["worktree", "remove", "--force", worktree.to_str().unwrap()],
        )
        .expect("reclaim the worktree");
        git::run(root, &["branch", "-D", "mush/1"]).expect("delete the branch");
    }

    /// After a merge by hand, a nudge to the child must be refused: its actor is
    /// alive but its worktree is gone, so a run would recreate `.mush/wt/1` as a
    /// plain directory where no surface could see, diff or land the file — the work
    /// would exist somewhere nothing can reach (finding S1). The test asserts
    /// the refusal, the untouched main tree, and no recreated directory.
    #[test]
    fn a_nudge_to_a_merged_child_is_refused_not_run_in_the_phantom_path() {
        let (root, events, child_tx) = finished_isolated_child("s1-merged");
        let worktree = git::worktree_path(&root, 1);
        land_with_merge(&root);

        child_tx
            .send(AgentMsg::Nudge("write extra.txt".into()))
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

    /// The same after a discard by hand: the work was thrown away on purpose, and
    /// a nudge must not quietly recreate the path it was thrown from.
    #[test]
    fn a_nudge_to_a_discarded_child_is_refused_not_run_in_the_phantom_path() {
        let (root, events, child_tx) = finished_isolated_child("s1-discarded");
        let worktree = git::worktree_path(&root, 1);
        land_with_discard(&root);

        child_tx
            .send(AgentMsg::Nudge("write extra.txt".into()))
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

    /// A `base` that cannot become a worktree fails the whole delegation: the
    /// parent's call comes back as an error naming the ref, no child is
    /// spawned, and no event pretends otherwise. The old contract degraded to a
    /// child in the shared checkout instead — two agents in one tree while the
    /// parent believed it had given one its own (finding H7).
    #[test]
    fn a_spawn_that_cannot_make_its_worktree_is_refused() {
        let (actor, _mailbox) = scripted_tools_actor(
            "spawn-worktree-refused",
            Arc::new(ScriptedMachine::new()),
            Arc::new(Advanceable::new()),
        );
        let root = actor.ctx.root.clone();
        git_in(&root, &["init", "-q", "-b", "main"]);
        git_in(&root, &["config", "user.email", "t@t"]);
        git_in(&root, &["config", "user.name", "t"]);
        fs::write(root.join("base.txt"), "base\n").unwrap();
        git_in(&root, &["add", "-A"]);
        git_in(&root, &["commit", "-qm", "init"]);
        // The child's id already owns a real directory there, which is the one
        // thing `git worktree add` refuses.
        fs::create_dir_all(root.join(".mush/wt/1")).unwrap();
        fs::write(root.join(".mush/wt/1/in the way.txt"), "mine\n").unwrap();
        let mut state = ActorState::default();

        let error = exec_tool(
            &actor,
            &mut state,
            ToolName::SpawnAgent,
            &json!({ "brief": "do the thing", "base": "main" }),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        let ToolError::Failed(error) = error else {
            panic!("a worktree that cannot be made is a failed call");
        };
        assert!(error.contains("cannot start from `main`"), "{error}");
        assert!(state.children.is_empty(), "nothing may be spawned");
        let _ = fs::remove_dir_all(&root);
    }

    /// A spawn refused *before* git could create anything gives its number back,
    /// so the next child is consecutive rather than one past a hole. The invariant
    /// (`crate::ids`) draws the line at the worktree: here `git worktree add`
    /// failed, so no branch and no directory exist and #1 is free; a failure
    /// after that line — `Workspace::new` over a worktree git did make — must
    /// *not* hand the number back, or the next `add -b mush/1` would meet the
    /// branch the refused spawn left.
    #[test]
    fn a_failed_isolated_spawn_leaves_the_next_childs_id_consecutive() {
        let (actor, _mailbox) = scripted_tools_actor(
            "spawn-id-burned",
            Arc::new(ScriptedMachine::new()),
            Arc::new(Advanceable::new()),
        );
        let root = actor.ctx.root.clone();
        git_in(&root, &["init", "-q", "-b", "main"]);
        git_in(&root, &["config", "user.email", "t@t"]);
        git_in(&root, &["config", "user.name", "t"]);
        fs::write(root.join("base.txt"), "base\n").unwrap();
        git_in(&root, &["add", "-A"]);
        git_in(&root, &["commit", "-qm", "init"]);
        // The one thing `git worktree add` refuses: the path is taken.
        fs::create_dir_all(root.join(".mush/wt/1")).unwrap();
        fs::write(root.join(".mush/wt/1/in the way.txt"), "mine\n").unwrap();
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);

        let error = exec_tool(
            &actor,
            &mut state,
            ToolName::SpawnAgent,
            &json!({ "brief": "do the thing", "base": "main" }),
            &cancel,
        )
        .unwrap_err();
        let ToolError::Failed(error) = error else {
            panic!("a worktree that cannot be made is a failed call");
        };
        assert!(error.contains("cannot start from `main`"), "{error}");
        assert!(state.children.is_empty(), "nothing was spawned");

        // The number came back: a shared child needs no git and takes #1. A
        // consumed id would make this reply say `#2`.
        let reply = exec_tool(
            &actor,
            &mut state,
            ToolName::SpawnAgent,
            &json!({ "brief": "second" }),
            &cancel,
        )
        .unwrap();
        assert!(
            reply.contains("spawned agent #1"),
            "the refused spawn's id is handed back: {reply}"
        );
        assert!(state.children.contains_key(&1), "and the child owns it");
        let _ = fs::remove_dir_all(&root);
    }

    /// Root -> child -> grandchild, each isolated: the grandchild's file must
    /// land in `.mush/wt/2/` on a branch that carries it, branched off the
    /// child's worktree (`mush/2` based on `mush/1`), and the summaries bubble up
    /// through wait. Three actors ask one scripted model at once; each
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
                    "run_command",
                    json!({ "command": "printf 'deep work' > deep.txt" }),
                )])
                .when(|asked: &Asked| asked.depth() == Some(2))
                .says("created deep.txt")
                // The child only delegates: spawn its own, wait, report.
                .when(|asked: &Asked| asked.depth() == Some(1) && asked.saw("#2 done"))
                .says("chain child done")
                .when(|asked: &Asked| asked.depth() == Some(1) && asked.saw("spawned agent"))
                .calls(vec![tool_call("c1b", "wait", json!({}))])
                .when(|asked: &Asked| asked.depth() == Some(1))
                .calls(vec![tool_call(
                    "c1a",
                    "spawn_agent",
                    json!({
                        "brief": "create a file called deep.txt containing exactly: deep work; \
                                  you must delegate this to your own subagent",
                        "base": "mush/1"
                    }),
                )])
                // The root: delegate, wait for the child, report.
                .when(|asked: &Asked| asked.saw("#1 done"))
                .says("chain root done")
                .when(|asked: &Asked| asked.saw("spawned agent"))
                .calls(vec![tool_call("c0b", "wait", json!({}))])
                .calls(vec![tool_call(
                    "c0a",
                    "spawn_agent",
                    json!({
                        "brief": "delegate file creation to your own subagent: spawn one with \
                                  base mush/1, brief 'create a file called deep.txt containing \
                                  exactly: deep work', then wait for it, then report",
                        "base": "main"
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
        // carries its work.
        let grandchild_files =
            git::run(&root, &["diff", "--name-only", "HEAD...mush/2"]).unwrap_or_default();
        assert!(
            grandchild_files.contains("deep.txt"),
            "mush/2 must carry the grandchild's work, got {grandchild_files:?}"
        );
        // The child only delegated, so *its* run committed nothing: its own
        // worktree and branch are reclaimed at the run's end (finding H10). The
        // grandchild's branch was forked from the commit `mush/1` stood at, so
        // its work is untouched by the reclamation — and the root's HEAD does
        // not move.
        assert!(
            !root.join(".mush/wt/1").exists(),
            "a run that committed nothing leaves no worktree behind"
        );
        assert_eq!(
            git_rev_parse(&root, "mush/1"),
            None,
            "and no branch either: the name went with the worktree"
        );
        assert!(
            root.join(".mush/wt/2/deep.txt").exists(),
            "while the grandchild's own worktree is untouched"
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
                    "run_command",
                    json!({ "command": "printf 'isolated work' > iso.txt" }),
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
                        "base": "main"
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
        root_tx.send(AgentMsg::Nudge("carry on".into())).unwrap();
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
                    "run_command",
                    json!({ "command": "printf 'worth folding' > note.txt" }),
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
            fold.saw("[exit 0]"),
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
                    "run_command",
                    json!({ "command": "printf 'worth folding' > note.txt" }),
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
                    "run_command",
                    json!({ "command": "printf 'written after the fold' > after.txt" }),
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

    /// A root actor starts with no transcript — the UI holds the conversation
    /// until a `Run` hands it over, or an `Adopt` at a restore. A fold is not a
    /// run, so the request carries it: without that a `/compact` on an actor
    /// that has never run folded nothing, and said nothing about it
    /// (finding U11).
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

    /// A stored conversation arrives in whatever shape the process that wrote
    /// it left behind: the human's own words between a call and its results
    /// (they typed while it ran — the UI stores them where they arrived), and a
    /// call whose result was never recorded at all (the run was cut off between
    /// the assistant's message and its results). A fold is the first request a
    /// restored transcript ever travels in, and a strict endpoint rejects both
    /// shapes with a complaint about `tool_call_ids` — so the hand-over repairs
    /// them, exactly as a `Run` does.
    #[test]
    fn a_compact_request_repairs_the_call_pairs_a_stored_transcript_is_missing() {
        let root = scratch_dir("compact-repaired");
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
        let calls = |id: &str| Message {
            role: "assistant".to_string(),
            tool_calls: Some(vec![tool_call(
                id,
                "run_command",
                json!({ "command": "true" }),
            )]),
            ..Default::default()
        };
        // No `Run` first: this is the actor a restart leaves behind, and the
        // transcript is one a process that went away mid-batch would write —
        // the human's words between a call and its result, and one call never
        // answered at all.
        root_tx
            .send(AgentMsg::Compact(vec![
                Message::system("you are mush"),
                Message::user("the old task".to_string()),
                calls("call_1"),
                Message::user("typed while the batch ran".to_string()),
                Message::tool("call_1", "the late result"),
                calls("call_2"),
                Message::assistant("done"),
            ]))
            .unwrap();

        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| !seen.summaries.is_empty()),
            "the fold must run on the stored transcript: {seen:?}"
        );
        let asked = scripted.asked();
        assert_eq!(asked.len(), 1, "one summarize call");
        // Every call in the request is answered by the message right after its
        // batch, in the call's own id order: the shape a strict server accepts.
        let messages = &asked[0].messages;
        let mut index = 0;
        while index < messages.len() {
            let batch = messages[index].tool_calls();
            if batch.is_empty() {
                index += 1;
                continue;
            }
            for (offset, call) in batch.iter().enumerate() {
                let answer = &messages[index + 1 + offset];
                assert_eq!(
                    answer.role, "tool",
                    "call {} is answered right after its batch: {messages:?}",
                    call.id
                );
                assert_eq!(answer.tool_call_id.as_deref(), Some(call.id.as_str()));
            }
            index += 1 + batch.len();
        }
        // The unanswered call got the sentence a missing result is given, not
        // silence; the interrupted batch kept the result that did arrive (in the
        // place the repair put it, behind the human's words); and those words
        // still reached the model.
        assert!(
            messages.iter().any(|message| message
                .tool_call_id
                .as_deref()
                .is_some_and(|id| id == "call_2")
                && message.text().contains("no result was recorded")),
            "the dangling call is answered: {messages:?}"
        );
        assert!(
            messages.iter().any(|message| message
                .tool_call_id
                .as_deref()
                .is_some_and(|id| id == "call_1")
                && message.text() == "the late result"),
            "the result that arrived late is handed to the call it answers: {messages:?}"
        );
        assert!(asked[0].saw("typed while the batch ran"));
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
        root_tx.send(AgentMsg::Nudge("STEERME".into())).unwrap();
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
    /// `run_command`, with the arguments differing each turn so the run is not
    /// stopped early as a loop instead.
    #[test]
    fn the_turn_limit_ends_with_a_summary() {
        const WRAPPED_UP: &str = "wrapped up: the work done so far is in the workspace";
        let root = scratch_dir("turns");

        let mut scripted = Scripted::new();
        for turn in 0..RUNAWAY_TURNS - 1 {
            scripted = scripted.calls(vec![tool_call(
                "call",
                "run_command",
                json!({ "command": format!("printf 'turn {turn}' > notes.txt") }),
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
        // A `wait` that slept is the other round that did nothing: the model is
        // not repeating itself, it is waiting out a hold that can outlast many
        // waits, so the count is cleared the same way.
        for _ in 0..LOOP_ROUNDS + 3 {
            count_round(&mut last, &mut repeats, "wait:{}", true);
        }
        assert_eq!(repeats, 0, "a wait that waited never accumulates");
        // A wait that came straight back is a different round: nothing moved,
        // nothing was asked of the world, and it counts like any other repeat.
        for _ in 0..LOOP_ROUNDS {
            count_round(&mut last, &mut repeats, "wait:{}", false);
        }
        assert_eq!(repeats, LOOP_ROUNDS, "a spinning wait still trips it");
        // And a different batch starts over, refusals or not.
        count_round(&mut last, &mut repeats, "edit_file:{}", false);
        assert_eq!(repeats, 0);
    }

    /// The road back has to survive being taken more than once. An exclusive
    /// hold can outlive a single wait many times over — a job's four hours
    /// against the wait's ten minutes — so a model that waits again and again for
    /// a locked machine is doing what the refusal and the `MACHINE` block told it
    /// to do, and the loop guard, which counts identical batches, used to stop it
    /// as a loop after five. A wait that slept is a round in which the world was
    /// asked to move on and answered "not yet": not a repeat. The same call with
    /// nothing to wait for still counts, because that one comes straight back and
    /// changes nothing at all.
    #[test]
    fn a_repeated_blocking_wait_is_not_a_loop() {
        let waits = LOOP_ROUNDS + 3;
        let script = || {
            let mut scripted = Scripted::new();
            for _ in 0..waits {
                scripted = scripted.calls(vec![tool_call("call", "wait", json!({}))]);
            }
            Arc::new(scripted.says("the machine never came free"))
        };
        let run = |label: &str, scripted: &Arc<Scripted>, held: bool| {
            let clock = Arc::new(Advanceable::new());
            let (actor, _events, _mailbox) =
                scripted_actor_on_clock(label, scripted, clock.clone());
            if held {
                // A sibling, not this agent: the wait has something to wait for.
                actor.ctx.registry.take_machine(42, "cargo bench").unwrap();
            }
            let mut state = ActorState::default();
            let cancel = Arc::new(AtomicBool::new(false));
            let mut messages = vec![
                Message::system("you are mush"),
                Message::user("wait for the machine"),
            ];
            let outcome = run_loop(&actor, &mut state, &mut messages, &cancel);
            let timeouts = messages
                .iter()
                .filter(|message| message.text().contains("wait timed out"))
                .count();
            let _ = fs::remove_dir_all(actor.ws.root());
            (outcome, timeouts)
        };

        let held = script();
        let (outcome, timeouts) = run("wait-loop-held", &held, true);
        assert_eq!(
            outcome.unwrap().as_deref(),
            Some("the machine never came free"),
            "a run that waits the hold out is not a loop"
        );
        assert_eq!(
            timeouts, waits,
            "every wait spent its ten fake minutes and gave up"
        );

        // The other direction: with nothing to wait for, the same call comes
        // back at once and the guard does what it is for.
        let empty = script();
        let (outcome, _) = run("wait-loop-empty", &empty, false);
        let error = outcome.expect_err("a spinning wait is still a loop");
        assert!(error.contains("stopped as a loop"), "{error}");
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
                "run_command",
                json!({ "command": "printf same > same.txt" }),
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

    /// A run starting is told to the parent by the actor that starts it.
    ///
    /// The parent's own books are a guess otherwise: `control message` decides
    /// whether it is resuming a child or interrupting one from the mark it
    /// holds, and the mark can be one completion behind — a `Steer` that reads
    /// as "mid-run" can be the thing that starts the run. The child is the only
    /// witness, so it says so as the run begins, and the completion of the run
    /// before it (drained first, same channel) cannot leave a working child
    /// marked at rest.
    #[test]
    fn a_starting_run_tells_its_parent() {
        let model = Arc::new(Scripted::new().says("done"));
        let (actor, _events, _mailbox) = scripted_actor("child-running", &model);
        let (parent_tx, parent_rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let child = Actor {
            parent_tx: Some(parent_tx),
            ..actor
        };
        start(child, vec![Message::user("do the thing")], true);

        // The run's start, before the run's report — which is the order that
        // makes the parent's mark exact rather than a guess.
        let first = parent_rx
            .recv_timeout(WAIT)
            .expect("the parent is told the run started");
        assert!(
            matches!(first, AgentMsg::ChildRunning { id: 7 }),
            "the mark, not the report"
        );
        let second = parent_rx.recv_timeout(WAIT).expect("then the run reports");
        assert!(
            matches!(second, AgentMsg::ChildDone { id: 7, run: 1, .. }),
            "and the report follows"
        );

        // The other end of that order, on the parent's own books: the
        // completion of the run *before* the one just started, folded first,
        // leaves the child marked at rest for exactly one message — and the
        // start that follows puts the mark back, because the child really is
        // running.
        let (actor, _mailbox) = test_actor("stale-then-running");
        let mut state = ActorState::default();
        let (child, _child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child);
        state.running.insert(1);
        note_completion(&mut state, 1, 1, Outcome::Stopped(Stop::Human));
        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::ChildDone {
                id: 1,
                run: 2,
                outcome: Outcome::Stopped(Stop::Human),
            },
        );
        assert!(
            !state.running.contains(&1),
            "the run that ended is booked as ended"
        );
        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::ChildRunning { id: 1 },
        );
        assert!(
            state.running.contains(&1),
            "and the run that started is booked as started"
        );
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A report whose send finds nobody behind the parent's mailbox is handed to
    /// the UI rather than dropped: the parent's books are where a child's run is
    /// booked, and the completion is the one report whose loss leaves them
    /// holding a running child that has finished — a `wait` burning its whole
    /// cap, a shared workspace the guard refuses to a sibling, under a row that
    /// says `✓` (§8.39).
    ///
    /// The root is the one agent that says nothing at all: it has no parent, and
    /// an event about it would be the UI filing the root's own completion back
    /// into its own mailbox — a wake-up that could never end.
    #[test]
    fn a_report_with_no_actor_behind_the_parent_goes_to_the_ui() {
        // A child whose parent's actor is gone: the mailbox is there and nobody
        // reads it, which is what a replaced or reclaimed parent leaves.
        let (actor, events, _mailbox) = build_actor(
            "parent-asleep",
            Arc::new(Scripted::new().says("done")),
            test_cfg(),
        );
        let child = Actor {
            parent_tx: Some(dead_mailbox()),
            ..actor
        };
        start(child, vec![Message::user("do the thing")], true);
        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done == 1),
            "the run must end: {seen:?}"
        );
        let told: Vec<AgentMsg> = events
            .events()
            .into_iter()
            .filter_map(|(_, event)| match event {
                AgentEvent::ParentAsleep { command } => Some(command),
                _ => None,
            })
            .collect();
        assert!(
            matches!(told.first(), Some(AgentMsg::ChildRunning { id: 7 })),
            "the start is offered to the UI first, in the order a parent reads it: {told:?}"
        );
        assert!(
            matches!(
                told.last(),
                Some(AgentMsg::ChildDone { id: 7, run: 1, outcome: Outcome::Finished(text) }) if text == "done"
            ),
            "and so is the completion the parent's books are waiting for: {told:?}"
        );

        // The same actor with no parent at all: `build_actor`'s own
        // `parent_tx: None`, which is the root's shape.
        let (root, events, _mailbox) = build_actor(
            "root-silent",
            Arc::new(Scripted::new().says("done")),
            test_cfg(),
        );
        start(root, vec![Message::user("do the thing")], true);
        let mut seen = Watched::default();
        assert!(
            seen.wait(&events, WAIT, |seen| seen.done == 1),
            "the run must end: {seen:?}"
        );
        assert!(
            !events
                .events()
                .into_iter()
                .any(|(_, event)| matches!(event, AgentEvent::ParentAsleep { .. })),
            "the root's own completion must not become a report somebody could deliver"
        );
    }

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

    /// A named base is the history the child gets: the worktree forks from that
    /// commit even when HEAD has moved on, and the reply names the commit it
    /// really started from (finding H7).
    #[test]
    fn a_spawn_forks_from_the_named_base_and_says_so() {
        let (actor, _mailbox) = scripted_tools_actor(
            "spawn-base",
            Arc::new(ScriptedMachine::new()),
            Arc::new(Advanceable::new()),
        );
        let root = actor.ctx.root.clone();
        let git = |args: &[&str]| git_in(&root, args);
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        fs::write(root.join("first.txt"), "first\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "first"]);
        let first = git_rev_parse(&root, "HEAD").unwrap();
        // HEAD moves on: the base must beat it.
        fs::write(root.join("second.txt"), "second\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "second"]);
        assert_ne!(git_rev_parse(&root, "HEAD").unwrap(), first);

        let mut state = ActorState::default();
        let report = exec_tool(
            &actor,
            &mut state,
            ToolName::SpawnAgent,
            &json!({
                "brief": "start from the first commit",
                "base": first.clone(),
            }),
            &AtomicBool::new(false),
        )
        .unwrap();

        assert!(report.contains("on mush/1"), "{report}");
        assert!(
            report.contains(&first[..7]),
            "the reply names the commit the child started from: {report}"
        );
        let worktree = git::worktree_path(&root, 1);
        assert_eq!(
            git_rev_parse(&worktree, "HEAD").as_deref(),
            Some(first.as_str()),
            "the worktree forked from the named commit, not from HEAD"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A named base is resolved to a commit before anything is created: a
    /// scratch workspace that is no repository at all refuses the call, and no
    /// child is spawned on some other history (finding H7).
    #[test]
    fn a_named_base_is_resolved_before_anything_is_created() {
        let (actor, _mailbox) = scripted_tools_actor(
            "spawn-base-errors",
            Arc::new(ScriptedMachine::new()),
            Arc::new(Advanceable::new()),
        );
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);

        // The scratch workspace is no repository at all: an unresolvable base
        // is a refusal, not a silent fall back.
        let error = exec_tool(
            &actor,
            &mut state,
            ToolName::SpawnAgent,
            &json!({ "brief": "b", "base": "main" }),
            &cancel,
        )
        .unwrap_err();
        let ToolError::Failed(error) = error else {
            panic!("an unknown base is a failed call");
        };
        assert!(error.contains("unknown base"), "{error}");
        assert!(state.children.is_empty(), "nothing may be spawned");
    }

    /// A child the parent resumed with a message counts as running again: the
    /// shared-workspace guard must not let a second shared child in behind its
    /// back (audit of the prompt vs behaviour, row 1).
    #[test]
    fn a_resumed_child_arms_the_shared_guard_again() {
        let (actor, _mailbox) = scripted_tools_actor(
            "shared-resume",
            Arc::new(ScriptedMachine::new()),
            Arc::new(Advanceable::new()),
        );
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let spawn = |state: &mut ActorState, brief: &str| {
            exec_tool(
                &actor,
                state,
                ToolName::SpawnAgent,
                &json!({ "brief": brief }),
                &cancel,
            )
        };

        spawn(&mut state, "first").unwrap();
        // The child reported and is at rest: a second shared child is fine.
        note_completion(&mut state, 1, 1, Outcome::Stopped(Stop::Human));
        spawn(&mut state, "second").unwrap();

        // The parent resumes the first with a message. It is running again,
        // even though nothing has reported yet.
        exec_tool(
            &actor,
            &mut state,
            ToolName::Control,
            &json!({ "id": "1", "action": "message", "text": "again" }),
            &cancel,
        )
        .unwrap();
        let refused = spawn(&mut state, "third").unwrap_err();
        let ToolError::Failed(refused) = refused else {
            panic!("a second shared child is refused, not a memory error");
        };
        assert!(refused.contains("#1"), "{refused}");
        assert!(refused.contains("shared workspace"), "{refused}");
    }

    /// `spawn_agent` can name its child: the name travels in the `Spawned`
    /// event, trimmed, and a blank or missing one leaves the row to derive its
    /// own handle from the brief (finding U14).
    #[test]
    fn a_spawn_carries_the_name_its_caller_gave() {
        let (actor, events, _mailbox) = build_actor_about(
            "spawn-title",
            Arc::new(
                Scripted::new()
                    .when(|asked: &Asked| asked.depth() == Some(1))
                    .says("done"),
            ),
            test_cfg(),
            Arc::new(ScriptedMachine::new()),
            Arc::new(Advanceable::new()),
        );
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let spawn = |state: &mut ActorState, args: Value| {
            exec_tool(&actor, state, ToolName::SpawnAgent, &args, &cancel)
        };

        spawn(
            &mut state,
            json!({ "brief": "port the parser", "title": "  parser port  " }),
        )
        .unwrap();
        note_completion(&mut state, 1, 1, Outcome::Stopped(Stop::Human));
        spawn(&mut state, json!({ "brief": "port the lexer" })).unwrap();
        note_completion(&mut state, 2, 1, Outcome::Stopped(Stop::Human));
        spawn(
            &mut state,
            json!({ "brief": "port the loader", "title": "   " }),
        )
        .unwrap();

        let titles: Vec<Option<String>> = events
            .events()
            .into_iter()
            .filter_map(|(_, event)| match event {
                AgentEvent::Spawned { title, .. } => Some(title),
                _ => None,
            })
            .collect();
        assert_eq!(
            titles,
            vec![Some("parser port".to_string()), None, None],
            "only a real name is carried, and it is trimmed"
        );
    }

    /// An isolated sibling edits its own worktree, so it must not block a
    /// shared spawn — the old guard counted it and then said something false
    /// about this workspace (audit row 7).
    #[test]
    fn an_isolated_sibling_does_not_block_a_shared_spawn() {
        let (actor, _mailbox) = scripted_tools_actor(
            "shared-vs-isolated",
            Arc::new(ScriptedMachine::new()),
            Arc::new(Advanceable::new()),
        );
        let root = actor.ctx.root.clone();
        git_in(&root, &["init", "-q", "-b", "main"]);
        git_in(&root, &["config", "user.email", "t@t"]);
        git_in(&root, &["config", "user.name", "t"]);
        fs::write(root.join("base.txt"), "base\n").unwrap();
        git_in(&root, &["add", "-A"]);
        git_in(&root, &["commit", "-qm", "init"]);
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);

        exec_tool(
            &actor,
            &mut state,
            ToolName::SpawnAgent,
            &json!({ "brief": "in my own worktree", "base": "main" }),
            &cancel,
        )
        .unwrap();
        let report = exec_tool(
            &actor,
            &mut state,
            ToolName::SpawnAgent,
            &json!({ "brief": "in the shared tree" }),
            &cancel,
        )
        .expect("a running isolated sibling does not share this workspace");
        assert!(report.contains("spawned agent #2"), "{report}");
        let _ = fs::remove_dir_all(&root);
    }

    /// A child the human resumed is *running*, whatever its last report said:
    /// the listing says so, and a wait does not answer the stale result as if
    /// it had just finished (audit row 1 and 3).
    ///
    /// Both shapes of that report: one the model has read, and one nobody has —
    /// the second is the one the timeout's fresh path walks, recording the same
    /// result again, and a result recorded again is not the child coming to
    /// rest.
    #[test]
    fn a_resumed_child_reads_as_running_and_a_wait_holds() {
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor(
            "resumed-wait",
            Arc::new(ScriptedMachine::new()),
            clock.clone(),
        );
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let (child, _child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child);
        note_completion(&mut state, 1, 1, Outcome::Stopped(Stop::Human));
        state.delivered.insert(1, 1);

        // The human nudged it: the parent's books are told.
        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::ChildRunning { id: 1 },
        );
        let lines = status_tool(&actor, &state).unwrap();
        assert!(lines.contains("#1 ◐ running"), "{lines}");

        // A wait no longer answers the old stopped digest: it waits, and the
        // clock is what ends it.
        let report = wait_tool(&actor, &mut state, &cancel).unwrap();
        assert!(report.starts_with("wait timed out"), "{report}");
        assert!(!report.contains("stopped"), "{report}");
        assert!(
            clock.elapsed() >= Duration::from_secs(600),
            "the deadline is what ended it: {:?}",
            clock.elapsed()
        );

        // The same with a result nobody has read: the timeout hands it over (it
        // is the one road a fresh result travels) while the child runs on. The
        // completion the fresh path records is the one the nudge resumed *past*,
        // so the running mark stays — otherwise the parent's books say a running
        // child is at rest, and `status`, the one-shared-child guard and the
        // next `wait` all read that.
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor(
            "resumed-unread",
            Arc::new(ScriptedMachine::new()),
            clock.clone(),
        );
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let (child, _child_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, child);
        note_completion(
            &mut state,
            1,
            1,
            Outcome::Finished("run one's result".into()),
        );
        assert!(state.unread(1), "nobody has read the result yet");

        absorb(
            &actor,
            &mut state,
            &mut Vec::new(),
            AgentMsg::ChildRunning { id: 1 },
        );

        let report = wait_tool(&actor, &mut state, &cancel).unwrap();
        assert!(
            report.contains("wait timed out") && report.contains("run one's result"),
            "the timeout hands over the unread result and names what still runs: {report}"
        );
        assert!(
            state.running.contains(&1),
            "recording a result again is not the child coming to rest: {:?}",
            state.running
        );
        let lines = status_tool(&actor, &state).unwrap();
        assert!(lines.contains("#1 ◐ running"), "{lines}");
    }

    /// A result the model has already read is not what a wait returns while a
    /// sibling is still running: the wait blocks for news, and only answers the
    /// read result once nothing is left to run (audit row 3).
    #[test]
    fn a_wait_does_not_answer_a_read_result_while_a_sibling_runs() {
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor(
            "wait-fresh",
            Arc::new(ScriptedMachine::new()),
            clock.clone(),
        );
        let mut state = ActorState::default();
        let cancel = AtomicBool::new(false);
        let (one, _one_rx) = crossbeam_channel::unbounded();
        let (two, _two_rx) = crossbeam_channel::unbounded();
        state.children.insert(1, one);
        state.children.insert(2, two);
        // #1 finished and was read; #2 is still working.
        note_completion(&mut state, 1, 1, Outcome::Finished("old news".into()));
        state.delivered.insert(1, 1);
        state.running.insert(2);

        let report = wait_tool(&actor, &mut state, &cancel).unwrap();
        assert!(
            report.starts_with("wait timed out") && report.contains("#2"),
            "it waits for the running child, not the read one: {report}"
        );

        // Once nothing runs, what is recorded is all there is: the read result
        // comes back, marked as read.
        state.running.clear();
        note_completion(&mut state, 2, 1, Outcome::Finished("new news".into()));
        let report = wait_tool(&actor, &mut state, &cancel).unwrap();
        assert!(report.contains("#2 done: new news"), "{report}");
    }

    /// A long command that cannot become a job — the budget filled after the
    /// decision — must not be reported as its own timeout with its output
    /// thrown away: it ran, it was stopped, and this is what it wrote (audit
    /// row 2).
    #[test]
    fn a_launch_refused_at_the_deadline_keeps_the_output() {
        use crate::machine::{Machine, ShellCommand};

        let mut builder = ScriptedMachine::new();
        for _ in 0..=jobs::MAX_JOBS {
            builder = builder.runs(Script::hangs().says("built 40%\n"));
        }
        let machine = Arc::new(builder);
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("launch-refused", machine.clone(), clock);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        // Fill the machine-wide budget, so the launch at the deadline is
        // refused while `run_shell` was told a job was possible.
        for _ in 0..jobs::MAX_JOBS {
            let job = machine
                .spawn(&ShellCommand {
                    command: "keep busy",
                    root: std::path::Path::new("/tmp"),
                })
                .unwrap();
            let (mailbox, _rx) = crossbeam_channel::unbounded();
            actor
                .ctx
                .registry
                .launch(jobs::Launch::started(
                    actor.id,
                    "keep busy".to_string(),
                    false,
                    mailbox,
                    job,
                ))
                .unwrap();
        }

        let report = run_shell(
            "cargo build --release",
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

        assert!(
            report.contains("built 40%"),
            "the output survives: {report}"
        );
        assert!(
            report.contains("could not become a job"),
            "and the refusal is named: {report}"
        );
        assert!(!report.contains("timed out after"), "{report}");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A command that times out with no room for a job says why it could not
    /// detach, instead of a bare timeout (audit row 2).
    #[test]
    fn a_timed_out_command_names_the_full_job_budget() {
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let clock = Arc::new(Advanceable::new());
        let (actor, _mailbox) = scripted_tools_actor("budget-timeout", machine, clock);
        let mut state = ActorState::default();
        let cancel = Arc::new(AtomicBool::new(false));

        let report = run_shell(
            "cargo build --release",
            &std::env::temp_dir(),
            Duration::from_secs(CMD_TIMEOUT_SECS),
            Detach::No,
            &cancel,
            &actor,
            &mut state,
        )
        .unwrap();

        assert!(report.contains("timed out after"), "{report}");
        assert!(report.contains("budget is full"), "{report}");
        let _ = fs::remove_dir_all(actor.ws.root());
    }

    /// A scratch git repo with one initial commit, ready for worktrees. The
    /// label keeps parallel tests from sharing a directory.
    fn init_git_repo(label: &str) -> PathBuf {
        let root = scratch_dir(&format!("git-{label}"));
        git_in(&root, &["init", "-q", "-b", "main"]);
        git_in(&root, &["config", "user.email", "t@t"]);
        git_in(&root, &["config", "user.name", "t"]);
        fs::write(root.join("base.txt"), "base\n").unwrap();
        git_in(&root, &["add", "-A"]);
        git_in(&root, &["commit", "-qm", "init"]);
        root
    }

    /// Run git in `dir`, failing the test if it does.
    fn git_in(dir: &Path, args: &[&str]) {
        use std::process::Command;

        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
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
