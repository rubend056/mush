//! The agent tree: the one owner of agents, ids, phases and focus.
//!
//! Every fact about an agent's life lives here — which ids exist, what each one
//! is doing now, what it produced, which mailbox steers it, where the focus and
//! the cursor are. "There is a node with id N" and "there is a mailbox, cancel
//! flag and stat for N" therefore cannot disagree, and a phase only ever moves
//! through a transition this module defines: `app::mod` routes events into
//! these methods, it never writes a node's fields itself. What each agent has
//! said is not here: that is the conversation, and it lives in [`super::chat`].

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use mush_core::git;
use mush_core::message::Message;
use mush_core::prompt;
use mush_core::text::truncate;
use mush_core::tools::ToolName;

use crate::agent::{AgentMsg, RootHandle, TreeHandles};
use crate::ids::{AgentId, Ids};
use crate::jobs::{self, JobView};

/// Which conversation an agent tree belongs to. One per Ctrl-N, so an event
/// from an actor left over from the previous chat can be recognised as stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConversationId(pub u64);

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
    /// The last thing the agent reported doing: a tool call's own label
    /// (`edit_file src/lib.rs`, `run_command cargo test`), or how the run's
    /// worktree ended (`committed abc123 on mush/1`).
    Activity(String),
    /// The conversation is being folded into a summary (context compaction),
    /// or a request to do so is queued behind the run in flight.
    Compacting(Compacting),
    /// A Stop is on its way and the actor has not yielded yet.
    Cancelling,
    /// A Stop landed: the run ended with no result, but the actor is still
    /// alive and a nudge resumes it. Its own state, because `Idle` (never ran),
    /// `Done` (produced a result) and `Stopped` (produced nothing, resumable)
    /// are three different things and blanking a stop to `Idle` lost the one
    /// fact the human needed: that work was interrupted mid-flight.
    Stopped,
    /// The run never ended: the process went away with it in flight, or the
    /// actor's mailbox is dead and no reply can ever come. Not `Stopped` — a
    /// stop is the human's doing and the actor is alive to be nudged again;
    /// a run that was cut off is not resumable and **nothing was committed by
    /// it**, which is the fact a human needs before trusting its worktree
    /// (`docs/findings.md` H2). A restored session whose stored status was
    /// `Running` is the clearest case, and it is the one a restart proves.
    CutOff,
    /// The run finished; `summary` holds what it produced.
    Done,
    /// The run failed; the payload is what the human needs to read.
    Failed(String),
}

impl Phase {
    /// Whether work is in flight. `Idle`, `Done`, and `Failed` are at rest.
    ///
    /// A fold counts: a summarize call is a request on the wire like any other,
    /// and the agent that makes it is not at rest while it waits. That is what
    /// keeps a fold in the repaint tick, out of the "Ctrl-C stops nothing"
    /// answer, and honest about being cancellable.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Phase::Thinking | Phase::Activity(_) | Phase::Compacting(_) | Phase::Cancelling
        )
    }

    /// What this phase is parked on, if it is parked at all.
    ///
    /// A run can be in flight with no model call behind it: `wait` blocks for
    /// minutes on children and jobs together, and an hourglass is not the same
    /// thing as a spinner. The word "working" for both is how a napping
    /// orchestrator came to look like a busy model (finding U7) — this is the
    /// derivation that tells them apart, made once from the label the actor
    /// wrote (which is the tool's own name, from `ToolName`), so the row, the
    /// footer and the transcript foot all read the same answer. The distinction
    /// itself is [`jobs::Waited`], the value the wait tool answers with.
    pub fn waiting(&self) -> Option<jobs::Waited> {
        let Phase::Activity(label) = self else {
            return None;
        };
        match ToolName::parse(label.split_whitespace().next()?) {
            Some(ToolName::Wait) => Some(jobs::Waited),
            _ => None,
        }
    }

    /// Whether this phase is a fold, and which kind.
    ///
    /// The same shape as [`Phase::waiting`], for the same reason: `compacting…`
    /// is not a model call the human can mistake for the run's own, and not a
    /// tool label either, so the row glyph, the row's words, the status bar and
    /// the transcript's foot all read this one answer instead of each guessing
    /// from the text the actor happened to write (finding U11).
    pub fn compacting(&self) -> Option<Compacting> {
        match self {
            Phase::Compacting(kind) => Some(*kind),
            _ => None,
        }
    }

    /// A stable, one-word name for this phase, for a reader that is not the
    /// painter: the attach protocol's roster (M3). The glyph and the row's own
    /// words stay the painter's (`app/screen.rs`); this is the same distinction
    /// in text, so a client can tell a `thinking` agent from a `working`,
    /// `compacting`, `stopped` one without parsing a glyph.
    pub fn label(&self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Thinking => "thinking",
            Phase::Activity(_) => "working",
            Phase::Compacting(_) => "compacting",
            Phase::Cancelling => "cancelling",
            Phase::Stopped => "stopped",
            Phase::CutOff => "cut off",
            Phase::Done => "done",
            Phase::Failed(_) => "failed",
        }
    }

    /// What this phase is, in the fewest words, for the one line that has to
    /// name every live agent at once; every phase answers with [`Phase::label`]
    /// but [`Phase::Activity`], where the bar fits the tool's own name instead
    /// of the roster's `working` (finding H9, refactor R22).
    pub fn doing(&self) -> &str {
        match self {
            Phase::Activity(what) => what.split_whitespace().next().unwrap_or("working"),
            phase => phase.label(),
        }
    }
}

/// A fold of an agent's conversation: the summarize call `/compact` asks for,
/// and the one the window filling up triggers by itself.
///
/// Two facts the screen has to tell apart, because they are two different
/// things to wait for: the fold has not started yet (a run is in flight, and a
/// fold never lands between an assistant's tool calls and their results), or
/// the summarize request is on the wire right now. Which of the two the human
/// *asked* for is the third variant, and it is what the words say: a fold the
/// window triggered is not a fold somebody is waiting on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compacting {
    /// Accepted while a run was in flight: the fold happens at the next
    /// message boundary. The actor says so the moment it takes the request, so
    /// a `/compact` that is going to wait is never silent about it.
    Parked,
    /// The summarize call is on the wire, because the human asked (`/compact`).
    Requested,
    /// The summarize call is on the wire, because the history is three
    /// quarters of the window: nobody asked, and the reason is the history.
    NearlyFull,
}

impl Compacting {
    /// The verb for what is happening: a parked fold *folds* (it has not
    /// started yet), an in-flight one *compacts*. The word every surface that
    /// names the fold out loud reads — the status bar and `/compact`'s own
    /// acknowledgement — so none of them can call a fold the row calls
    /// `folding at the next step…` "compacting" (refactor R8).
    pub fn verb(self) -> &'static str {
        match self {
            Compacting::Parked => "folding",
            Compacting::Requested | Compacting::NearlyFull => "compacting",
        }
    }

    /// What the row and the transcript's foot say: the verb above, spelled out
    /// with what makes this fold the one it is. One spelling, so the two cannot
    /// describe the same fold differently, and one that contains its own verb,
    /// which the fold's unit test reads.
    pub fn words(self) -> &'static str {
        match self {
            Compacting::Parked => "folding at the next step…",
            Compacting::Requested => "compacting…",
            Compacting::NearlyFull => "context nearly full — compacting…",
        }
    }
}

/// How wide an agent's title may be. A title is a handle, not a sentence:
/// `#2 tests` tells two children apart at a glance, and the brief — one row
/// below in the cursor row's footer, and again as the transcript's opening line
/// — is where the sentence lives.
const TITLE_COLUMNS: usize = 24;

/// Words a brief opens with that say nothing about the task.
///
/// A brief is written *for a model* — "please create a file called deep.txt…"
/// — so the words that identify the job are usually not the first ones. The
/// list is small on purpose: a word that is missing costs a slightly worse
/// title, and a word wrongly in it costs the title entirely.
const FILLER: &[&str] = &[
    "a",
    "an",
    "the",
    "and",
    "or",
    "to",
    "of",
    "for",
    "in",
    "on",
    "with",
    "then",
    "please",
    "must",
    "make",
    "sure",
    "i",
    "we",
    "want",
    "need",
    "you",
    "your",
    "own",
    "it",
    "its",
    "this",
    "that",
    "their",
    "them",
    "our",
    "us",
    "create",
    "add",
    "write",
    "build",
    "fix",
    "update",
    "implement",
    "refactor",
    "remove",
    "delete",
    "run",
    "handle",
    "ensure",
    "check",
    "test",
    "document",
    "profile",
    "guard",
    "measure",
    "sweep",
    "delegate",
    "report",
    "use",
    "call",
    "called",
];

/// Whether a word names a file or a directory: a slash anywhere, or a dot with
/// a word on each side (`deep.txt`). A trailing sentence full stop is not part
/// of the name.
fn is_path_like(word: &str) -> bool {
    let word = bare_word(word).trim_end_matches('.');
    if word.contains('/') {
        return true;
    }
    word.split_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && ext.chars().all(char::is_alphabetic))
}

/// A word without the punctuation the sentence hung on it — `(deep.txt),` and
/// `deep.txt` are the same word, and a row should name the file either way.
fn bare_word(word: &str) -> &str {
    word.trim_matches(|c: char| !c.is_alphanumeric() && !"/._-".contains(c))
}

/// Whether a word says nothing about the task (see [`FILLER`]).
fn is_filler(word: &str) -> bool {
    FILLER.contains(&bare_word(word).to_ascii_lowercase().as_str())
}

/// Where an isolated agent's work ended up, once the human landed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landed {
    /// Merged into the main branch; the worktree and the branch were reclaimed.
    Merged,
    /// Thrown away on purpose; the worktree and the branch were reclaimed.
    Discarded,
}

impl Landed {
    /// The past tense of what happened, as a word.
    ///
    /// The one spelling of it, so the refusal that tells the human why a nudge
    /// cannot run and the row that says where the work went cannot tell the
    /// same story in two different words (refactor R12). It is the word and not
    /// the sentence: each surface paints its own prose around it
    /// (`app::screen::agent_detail` for the row).
    pub fn past(self) -> &'static str {
        match self {
            Landed::Merged => "merged",
            Landed::Discarded => "discarded",
        }
    }
}

/// One entry in the agent tree. In [`AgentTree::agents`] the order is *spawn*
/// order — the order things happened, which is what a stored session keeps;
/// the order a pane paints is derived from it by [`AgentTree::rows`]. Ids are
/// stable, so positions do not shift while agents are alive.
pub struct AgentNode {
    pub id: AgentId,
    pub parent: Option<AgentId>,
    pub depth: usize,
    pub brief: String,
    /// The name the caller gave this agent, if it gave one: what the row
    /// shows instead of [`Self::title`]'s derived handle.
    pub title: Option<String>,
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
    /// Set in a stored session whose work was landed by hand; nothing in this
    /// session sets it.
    pub landed: Option<Landed>,
    /// Why mush looked at this agent's worktree and left it alone: its branch is
    /// not merged into the base it was forked from, or its checkout has
    /// uncommitted work in it. Set by the sweep that reclaims landed worktrees
    /// (`git::reclaim`), and read by the row — a human who expected a merged
    /// worktree to go learns here why it did not instead of finding nothing
    /// said (finding H10).
    ///
    /// Never stored: it is what git says right now, and a stored copy would be
    /// the half of the pair that cannot be re-derived.
    pub kept: Option<String>,
    /// The agent's last run ended and its parent has not read the result yet.
    ///
    /// It is a *mirror* of the actor's own `delivered` set — the one owner of
    /// "has the model read this line" — kept here because the row has to paint
    /// it every frame, and it is only ever moved by events from that owner
    /// ([`AgentTree::result_read`], which the parent's actor emits as it hands
    /// the line over) or by the thing that supersedes the result itself (a new
    /// run, exactly as `note_completion` re-arms a delivery). Nothing else
    /// clears it, so a `✉` means one thing: the parent has not read this yet
    /// (finding H4).
    pub result_unread: bool,
}

impl AgentNode {
    /// A short handle for this agent: the name its caller gave it, else one
    /// derived from the brief on read.
    ///
    /// A row used to spend its identity columns on the brief's first clause,
    /// which is usually boilerplate: `create a file called deep.txt …` and
    /// `create a file called wide.txt …` read the same for fifteen columns, and
    /// the human could not tell two children apart without opening them (finding
    /// U6). A named agent is exactly what its parent called it; otherwise two
    /// rules, in the order the value is in:
    ///
    /// 1. a *path* in the brief is the artifact the agent was asked to make —
    ///    `deep.txt`, `crates/mush/src/ui.rs` — and it tells two children apart
    ///    at a glance. The same reason `agent::summarize` reads a tool call's
    ///    `path` before anything else;
    /// 2. otherwise the first word that is not filler: `build a lexer for the
    ///    config format` is `lexer`, not `build a lexer for`.
    ///
    /// Never stored when it is derived: the brief is the fact and this is a
    /// view of it, and a stored copy is one more thing that can disagree with
    /// the transcript's opening line. A *given* title is stored, because
    /// nothing else can produce it.
    pub fn title(&self) -> String {
        if let Some(given) = &self.title {
            return truncate(given, TITLE_COLUMNS);
        }
        // The brief's first line, collapsed: [`mush_core::text::first_line`],
        // the same line the commit subject and a job's handle are cut from
        // (refactor R11).
        let first = mush_core::text::first_line(&self.brief);
        let words: Vec<&str> = first.split_whitespace().collect();
        if let Some(path) = words.iter().find(|word| is_path_like(word)) {
            return truncate(bare_word(path).trim_end_matches('.'), TITLE_COLUMNS);
        }
        let first = words
            .iter()
            .find(|word| !is_filler(word))
            .or_else(|| words.first());
        match first {
            Some(word) => truncate(bare_word(word), TITLE_COLUMNS),
            None => String::new(),
        }
    }
}

/// How many agents are in each of the states the pane title names.
///
/// A struct rather than a live count beside the list it counts: the two
/// buckets are derived together, from one walk over the phases, so the title
/// cannot add up a different set than the rows show (finding U2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Roster {
    /// Runs in flight: `Thinking`, `Activity`, `Cancelling`.
    pub working: usize,
    /// At rest with children working — waiting to be woken by a completion.
    /// "Children" are its own, the same unit its row's `⏸N` mark counts: a
    /// grandchild's work is its own parent's to wait for.
    pub waiting: usize,
}

/// A child actor that now exists, as its parent reported it: everything the
/// tree needs to give it a row, a mailbox, and an opening line.
pub struct Spawn {
    pub id: AgentId,
    pub parent: AgentId,
    pub brief: String,
    pub depth: usize,
    pub branch: Option<String>,
    pub cmd: Sender<AgentMsg>,
}

/// A child that now exists: the row the tree gave it, and the line that opens
/// its transcript. The brief is that line — the model sees the brief, so the
/// human should too (finding B13) — and the conversation is handed it with
/// [`super::chat::Chat::push_message`], because the text of a conversation is
/// not the tree's to keep.
pub struct Opened {
    pub id: AgentId,
    pub opening: Message,
}

/// A node that already exists outside this session: an agent restored from a
/// stored session (with a live mailbox) or a leftover worktree found on disk
/// (with none).
pub struct Existing {
    pub id: AgentId,
    pub parent: Option<AgentId>,
    pub depth: usize,
    pub brief: String,
    /// The name the caller gave the agent, when one was stored with the
    /// session; the row falls back to a handle derived from the brief.
    pub title: Option<String>,
    pub phase: Phase,
    pub branch: Option<String>,
    pub summary: Option<String>,
    pub leftover: bool,
    pub landed: Option<Landed>,
    /// Whether this agent's result was unread when the workspace stored it.
    /// `false` for a worktree found on disk: a branch set aside on an earlier
    /// run has no parent in this process, so there is no reader and no read to
    /// owe (finding H1).
    pub result_unread: bool,
    pub tx: Option<Sender<AgentMsg>>,
}

/// A cancel is acknowledged quickly — the actor yields, or the run ends. A `⊘`
/// older than this means the acknowledgement will never arrive (the actor's
/// mailbox is dead), and a row that spins forever is worse than an idle one
/// (finding B6).
const STALE_CANCEL: Duration = Duration::from_secs(10);

/// How many finished children a tree keeps before it starts forgetting the
/// oldest ones.
///
/// Everything needs a cap, even a high one (§8.21), and a run's memory had
/// none: `MAX_AGENTS` counts the agents *running* (`agent.rs`), `MAX_DEPTH`
/// bounds depth rather than breadth, and `reap` had one caller, at startup — so
/// a node, a transcript and a `mush-agent-{id}` thread accumulated for every
/// child a session ever spawned, and the file `session_snapshot` writes grew
/// with all of them (finding H16). The window is the *finished* children
/// [`AgentTree::past_history`] may drop; a child that must be kept — an unread
/// result, an unlanded branch — does not spend one of these slots, so it can
/// never push an older, droppable one out of the window.
///
/// **No archive.** The reaped transcript is *gone*, not written anywhere: an
/// archive would be one more lifetime to reason about, and the stored copy of
/// each transcript is already capped at 256 KiB, so dropping a row drops at
/// most that from the next save (the human's decision, §8.21).
pub const CHILD_HISTORY: usize = 50;

/// How many of the newest children keep their actor thread.
///
/// A thread is the resource, not a node: `mush-agent-{id}` lives until a
/// `Shutdown` arrives, and every finished child used to hold one for the life
/// of the session. The newest few stay warm, so an ordinary nudge — the human
/// typing at the child they just watched finish — costs a message instead of a
/// thread; everything older is parked, node and transcript untouched
/// ([`AgentTree::parkable`], `App::park_history`).
const WARM_CHILDREN: usize = 8;

/// The agents of one conversation, and everything keyed by their ids.
pub struct AgentTree {
    /// Spawn order: the root first, then each agent as it was spawned. Not tree
    /// order — a grandchild spawned before its uncle sits before it here, and
    /// [`Self::rows`] is what turns this into the pre-order the pane paints.
    pub agents: Vec<AgentNode>,
    /// The row the agent pane highlights.
    pub agent_cursor: usize,
    /// The agent whose transcript the chat shows and whose mailbox typing
    /// targets.
    pub focused: AgentId,
    /// Steering handles: one mailbox per agent, keyed by id.
    pub agent_tx: HashMap<AgentId, Sender<AgentMsg>>,
    /// Each running agent's cancellation flag. The HTTP reader polls it, so a
    /// Ctrl-C stops a model call that has not answered yet — the mailbox alone
    /// cannot: the actor is blocked inside the request.
    pub agent_cancel: HashMap<AgentId, Arc<AtomicBool>>,
    /// Each isolated agent's own work, measured on its branch. Refreshed by
    /// events, never computed while painting.
    pub agent_stats: HashMap<AgentId, git::Stat>,
    /// The tree's id counters, shared with the actors. Leftover worktrees are
    /// registered under their own ids, so the next spawn must start above them
    /// or two nodes share an id and every id-keyed lookup hits the wrong one
    /// (finding B1). A value rather than an `Arc<AtomicU64>`: [`Ids`] owns its
    /// own sharing, and knows the agent counter from the job one.
    ids: Ids,
    /// The tree-wide running count, shared with the actors so an agent revived
    /// from a stored session is counted against the ceiling like any other.
    live: Arc<AtomicU64>,
    /// Which conversation the live actor tree belongs to; events tagged with
    /// any other are from an abandoned tree and are ignored.
    conversation: ConversationId,
    /// Every job this tree's agents started, shared with the actors. The tree
    /// holds it for the same two reasons it holds `ids` and `live`: a row reads
    /// its owner's jobs from the one registry (so the badge cannot disagree
    /// with the machine), and quitting kills them through it.
    jobs: Arc<jobs::Registry>,
}

impl AgentTree {
    /// A tree holding just the root agent, wired to that actor's mailbox, id
    /// counter and running count.
    pub fn rooted(root: RootHandle) -> Self {
        Self::with_root(
            ConversationId(root.conversation),
            root.ids.clone(),
            root.live.clone(),
            root.jobs.clone(),
            root.tx,
        )
    }

    /// A tree with one idle root and a live mailbox, for the transition rules'
    /// own tests.
    ///
    /// The receive half is deliberately leaked: a `Sender` whose receiver has
    /// been dropped fails every send, and half the rules here are about what
    /// happens when one does, so the tests need a mailbox that works without
    /// spawning an actor.
    #[cfg(test)]
    pub fn bare() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<AgentMsg>();
        std::mem::forget(rx);
        Self::with_root(
            ConversationId(1),
            Ids::default(),
            Arc::new(AtomicU64::new(0)),
            jobs::Registry::bare(),
            tx,
        )
    }

    fn with_root(
        conversation: ConversationId,
        ids: Ids,
        live: Arc<AtomicU64>,
        jobs: Arc<jobs::Registry>,
        tx: Sender<AgentMsg>,
    ) -> Self {
        let mut tree = Self {
            agents: Vec::new(),
            agent_cursor: 0,
            focused: AgentId::ROOT,
            agent_tx: HashMap::from([(AgentId::ROOT, tx)]),
            agent_cancel: HashMap::new(),
            agent_stats: HashMap::new(),
            ids,
            live,
            conversation,
            jobs,
        };
        tree.agents.push(AgentNode {
            id: AgentId::ROOT,
            parent: None,
            depth: 0,
            brief: "you (root agent)".to_string(),
            // The root is at rest until the human sends it something; it is
            // never "busy" just for existing.
            phase: Phase::Idle,
            since: Instant::now(),
            branch: None,
            summary: None,
            leftover: false,
            title: None,
            // The root has no parent, so there is nobody to read its result.
            result_unread: false,
            landed: None,
            kept: None,
        });
        tree
    }

    /// Which conversation's events this tree accepts.
    pub fn conversation(&self) -> ConversationId {
        self.conversation
    }

    /// Everything an actor revived into this tree needs to join it: the id
    /// counter, the running count and the job registry, in one value so it
    /// cannot be half-joined ([`TreeHandles`]).
    pub fn handles(&self) -> TreeHandles {
        TreeHandles {
            ids: self.ids.clone(),
            live: self.live.clone(),
            jobs: self.jobs.clone(),
        }
    }

    /// The jobs `id` still has running, straight from the registry. Derived on
    /// read, so a count on a row and a list in a footer are one fact rather
    /// than two.
    pub fn live_jobs(&self, id: AgentId) -> Vec<JobView> {
        self.jobs.live_for(id.0)
    }

    /// How many jobs the whole tree has running right now, for the pane title's
    /// machine picture: every parallel worktree builds its own artifacts, so
    /// the number is what says *why* a dozen agents feel slow (finding H8).
    ///
    /// It counts exactly what the rows count — a job is a command that outlived
    /// its tool call, one `⚙` each. It used to sum in the commands a tool call
    /// was still holding, which is one number for something no row can name and
    /// no job id can list: the title said `1 job` over a tree with no job in it.
    pub fn live_job_count(&self) -> usize {
        self.jobs.running()
    }

    /// Whether this agent has work in flight: its own run, or a job of its own.
    ///
    /// The one per-node derivation of the fact. Four places used to spell it —
    /// the reaper's keep, the parker's hold, the bar's `busy` count and `c` on
    /// a row — and they could only ever drift apart. It is read from the
    /// registry, so a run that ended while its `cargo bench` still runs is not
    /// idle on the machine.
    pub fn in_flight(&self, node: &AgentNode) -> bool {
        self.in_flight_with(node, self.live_job_count() > 0)
    }

    /// [`Self::in_flight`] with "is there any job at all" already derived, for
    /// the two walks that ask it once per node.
    ///
    /// The `jobs_live` pre-check is a **cost filter, not a second rule**: with
    /// an empty registry the per-node lookup can only return nothing, so the
    /// filter only saves taking the registry's lock once per node per frame.
    fn in_flight_with(&self, node: &AgentNode, jobs_live: bool) -> bool {
        node.phase.is_busy() || (jobs_live && !self.live_jobs(node.id).is_empty())
    }

    /// Keep the agent counter above `floor`. A leftover worktree or a restored
    /// agent holds an id the counter has never seen, so the next spawn has to
    /// start above it or two nodes share one (finding B1).
    pub fn reserve_agents(&mut self, floor: u64) {
        self.ids.reserve_agents(floor);
    }

    /// A child actor now exists. It is thinking (its parent just started it),
    /// it has no summary of its own yet, and its brief opens its transcript:
    /// the model sees the brief, so the human should too (finding B13). The
    /// opening line is handed back rather than stored: transcripts belong to
    /// the conversation, not to the tree.
    pub fn insert(&mut self, spawn: Spawn) -> Opened {
        let opening = if spawn.brief.trim().is_empty() {
            prompt::BEGIN_TASK.to_string()
        } else {
            spawn.brief.clone()
        };
        self.agent_tx.insert(spawn.id, spawn.cmd);
        self.agents.push(AgentNode {
            id: spawn.id,
            parent: Some(spawn.parent),
            depth: spawn.depth,
            brief: spawn.brief,
            phase: Phase::Thinking,
            since: Instant::now(),
            branch: spawn.branch,
            summary: None,
            leftover: false,
            landed: None,
            // Nothing has swept this agent's worktree yet.
            kept: None,
            title: None,
            // Its run has not produced anything yet: there is no result to read.
            result_unread: false,
        });
        Opened {
            id: spawn.id,
            opening: Message::user(opening),
        }
    }

    /// Adopt a node that already exists: a subagent restored from a stored
    /// session, or a worktree found on disk. Its phase and summary are the
    /// stored ones — nothing here is a transition.
    pub fn register(&mut self, node: Existing) {
        if let Some(tx) = node.tx {
            self.agent_tx.insert(node.id, tx);
        }
        self.agents.push(AgentNode {
            id: node.id,
            parent: node.parent,
            depth: node.depth,
            brief: node.brief,
            title: node.title,
            phase: node.phase,
            since: Instant::now(),
            branch: node.branch,
            summary: node.summary,
            leftover: node.leftover,
            landed: node.landed,
            // A sweep runs on the next git read, not on registration: a row for
            // a worktree found on disk says where the work is until git has
            // been asked, and mush does not shell out while painting.
            kept: None,
            // The stored file is where this fact survives a restart: a result
            // its parent had not read comes back wearing `✉` (finding H1). A
            // node registered from something other than the session file
            // (`discover_worktrees`) passes `false`, because there is no
            // conversation behind it to have read or not read.
            result_unread: node.result_unread,
        });
    }

    /// Give an agent the name its caller chose. One fact, one home: the row,
    /// the wire roster and the session snapshot all render [`AgentNode::title`]
    /// from the node, so the name lands there and nowhere else.
    pub fn named(&mut self, id: AgentId, title: String) {
        if let Some(node) = self.node_mut(id) {
            node.title = Some(title);
        }
    }

    /// Mush's own sweep took this agent's worktree and branch: the work is in the
    /// base the branch was forked from (or the run never committed anything), so
    /// the row says where it went instead of offering a `git diff` against a
    /// branch and a checkout that are both gone (finding U13, H10).
    ///
    /// The branch goes with the checkout. A name git no longer has is a diff
    /// that cannot work, the refresh stops asking about an id whose work is
    /// settled, and `landed` alone is what `App::worktree_gone` reads before
    /// refusing a nudge that would recreate the reclaimed path as a plain
    /// directory (finding S1). `landed` is stored, so a restart comes back with
    /// the same row rather than reviving an agent whose worktree is gone.
    pub fn mark_reclaimed(&mut self, id: AgentId) {
        if let Some(node) = self.node_mut(id) {
            node.landed = Some(Landed::Merged);
            node.branch = None;
            node.kept = None;
        }
    }

    /// What the last reclamation sweep found at this agent's worktree: the reason
    /// it was left alone, or `None` when there was nothing there to hold.
    ///
    /// One answer per node, replaced by the next sweep: the reason is a fact
    /// about git *now*, and a node must not keep yesterday's wording — or keep
    /// saying why a checkout that is no longer there was kept.
    pub fn mark_kept(&mut self, id: AgentId, why: Option<String>) {
        if let Some(node) = self.node_mut(id) {
            node.kept = why;
        }
    }

    /// A run began, possibly one the UI did not ask for (an idle agent woken by
    /// a child's result). The last run's summary belongs to that run, not this
    /// one (finding B14).
    ///
    /// `cancel` is the run's flag when the actor reported it; a run the UI
    /// started optimistically marks the phase first and gets the flag with the
    /// actor's `Running` event, so `None` leaves any previous flag alone.
    pub fn begin(&mut self, id: AgentId, cancel: Option<Arc<AtomicBool>>) {
        if let Some(cancel) = cancel {
            // Keep the run's flag: a Stop must be able to reach a model call
            // that is still waiting, not just the actor's mailbox.
            self.agent_cancel.insert(id, cancel);
        }
        if let Some(node) = self.node_mut(id) {
            node.phase = Phase::Thinking;
            node.since = Instant::now();
            node.summary = None;
            // A new run supersedes the last result — the actor's
            // `note_completion` re-arms delivery for exactly this reason — so
            // the mark that result wore goes with it.
            node.result_unread = false;
            // A landed agent that runs again is not a landed agent: its
            // worktree is gone (which is why the restored node dropped its
            // branch), so the new run happens in the main checkout, and a
            // footer still saying `merged into HEAD` would be describing the
            // run before this one while the row shows work in flight (finding
            // P7). What the merge did is in the transcript, where history
            // lives.
            node.landed = None;
            // The same for the sweep's verdict: it described a worktree this
            // run is about to change, and the next read of git will have a new
            // one.
            node.kept = None;
        }
    }

    /// What the agent says it is doing now. Only a run in flight can report
    /// activity: a status that arrives after the run's own end (a late or
    /// duplicated commit line) is dropped, so a finished agent is never put
    /// back to work (finding B5).
    ///
    /// A fold is never replaced by a label, and it is not a status being
    /// honoured: the run behind a *parked* fold keeps announcing `edit_file …`,
    /// and those labels must not erase the request the human is waiting for. A
    /// fold ends where it began — its own `Compacting` event, the transcript
    /// replacing `Compact`, or one of the run's own endings — and what the run
    /// was doing is still in the transcript, where every tool call writes an
    /// `⚙` line.
    pub fn activity(&mut self, id: AgentId, label: impl Into<String>) {
        if !self.is_busy(id) {
            return;
        }
        if let Some(node) = self.node_mut(id) {
            if node.phase.compacting().is_some() {
                return;
            }
            node.phase = Phase::Activity(label.into());
            node.since = Instant::now();
        }
    }

    /// A fold of this agent's conversation: accepted (and parked behind the run
    /// in flight), or on the wire now.
    ///
    /// An accepted fold is visible from rest, which is the whole point (finding
    /// U11): `activity` refuses a status from an agent that is not already
    /// busy, so the one setter that could have carried `compacting…` dropped it
    /// — an agent paying for a summarize request while its row said `✓`.
    ///
    /// `cancel` is the fold's own flag when the fold owns one (an idle
    /// `/compact` has no run to cancel, so this is the only handle a Stop can
    /// reach); `None` leaves the run's flag in place for a fold inside a run.
    pub fn compacting(&mut self, id: AgentId, kind: Compacting, cancel: Option<Arc<AtomicBool>>) {
        if let Some(cancel) = cancel {
            self.agent_cancel.insert(id, cancel);
        }
        if let Some(node) = self.node_mut(id) {
            node.phase = Phase::Compacting(kind);
            node.since = Instant::now();
        }
    }

    /// The fold landed: the conversation it summarized is the one the pane now
    /// shows, and the summary it produced is read where it now lives, in the
    /// transcript.
    ///
    /// `in_run` is whether the fold was part of a run that is still going: that
    /// run wears `thinking…` (the phase between its request and the tool it
    /// names) until it names one, where an idle fold's agent goes back to
    /// `✓ done` with the last reply it produced.
    pub fn compacted(&mut self, id: AgentId, in_run: bool) {
        self.fold_over(id, in_run, Phase::Done);
    }

    /// The fold is over and the transcript is unchanged — the summarize call
    /// failed, returned no summary, or answered something mush could not read
    /// as one. The `compacting…` phase must not outlive the request that
    /// justified it: a failed fold that left the row claiming work forever is
    /// the same lie as a fold nobody can see (finding U11). What went wrong is
    /// the notice the actor emitted, not a `✗` on a row that ran nothing.
    ///
    /// `in_run` is whether the run the fold was part of is still going: it
    /// wears `thinking…` (the phase between a request and the tool it names),
    /// where an idle fold's agent is back at rest.
    pub fn compacting_ended(&mut self, id: AgentId, in_run: bool) {
        self.fold_over(id, in_run, Phase::Idle);
    }

    /// End a fold: `at_rest` is the phase the agent wears when the fold was not
    /// part of a run (`✓ done` for one that landed, `idle` for one that came to
    /// nothing), and a fold still inside its run wears `thinking…` either way.
    ///
    /// One body for both endings, so the two cannot disagree about the run they
    /// were part of, the flag they hand back, or the clock they restart (R2).
    fn fold_over(&mut self, id: AgentId, in_run: bool, at_rest: Phase) {
        if !self.compacting_over(id, in_run) {
            return;
        }
        if let Some(node) = self.node_mut(id) {
            node.phase = if in_run { Phase::Thinking } else { at_rest };
            node.since = Instant::now();
        }
    }

    /// Forget the fold state of `id`, and the flag it owned. Returns whether
    /// there was a fold at all: one that ended *inside* a run leaves that run's
    /// phase — and the flag the run still needs — alone.
    fn compacting_over(&mut self, id: AgentId, in_run: bool) -> bool {
        let was = self
            .node(id)
            .is_some_and(|node| node.phase.compacting().is_some());
        if was && !in_run {
            // A fold from rest owns its flag: nothing else holds it, so it goes
            // when the fold does. A fold inside a run does not own one — the
            // run's lives in the same place, and a run that is still going must
            // stay stoppable.
            self.agent_cancel.remove(&id);
        }
        was
    }

    /// The run finished: `summary` is what it produced, and it always replaces
    /// the previous one — keeping the first summary described a run that ended
    /// long ago (finding B14). An empty reply replaces it with nothing.
    pub fn finish(&mut self, id: AgentId, summary: Option<String>) {
        if let Some(node) = self.node_mut(id) {
            let unread = node.parent.is_some();
            node.phase = Phase::Done;
            node.since = Instant::now();
            node.summary = summary;
            node.result_unread = unread;
        }
        self.agent_cancel.remove(&id);
    }

    /// The run failed: the error is what the human has to read, and it is the
    /// phase until a later run replaces it. A failure is a result like any
    /// other, so a parent that has not read it says so too.
    pub fn fail(&mut self, id: AgentId, error: String) {
        if let Some(node) = self.node_mut(id) {
            let unread = node.parent.is_some();
            node.phase = Phase::Failed(error);
            node.since = Instant::now();
            node.result_unread = unread;
        }
        self.agent_cancel.remove(&id);
    }

    /// A Stop landed: the run produced nothing, and the actor is alive and
    /// resumable. Its own phase, not `Idle` — that a run was interrupted
    /// mid-flight is the fact the human needs.
    pub fn stopped(&mut self, id: AgentId) {
        if let Some(node) = self.node_mut(id) {
            let unread = node.parent.is_some();
            node.phase = Phase::Stopped;
            node.since = Instant::now();
            // A stopped child's line (`#N stopped: …`) still has to be folded
            // into its parent's transcript before the parent knows there is no
            // result coming, so "unread" is the truth about it as well.
            node.result_unread = unread;
        }
        self.agent_cancel.remove(&id);
    }

    /// The parent has read this agent's result: the line is in its transcript
    /// now, wherever it came from — a fold at the next message boundary, the
    /// wake-up a napping parent got, or a `wait` that asked for it.
    ///
    /// Only its own actor can say this (it owns the `delivered` set), so this is
    /// only ever called from the event that actor emits, and never as a guess
    /// from the shape of the row (finding H4).
    pub fn result_read(&mut self, id: AgentId) {
        if let Some(node) = self.node_mut(id) {
            node.result_unread = false;
        }
    }

    /// Put a node back at rest: an agent whose mailbox is gone is not running,
    /// whatever its row said.
    pub fn idle(&mut self, id: AgentId) {
        if let Some(node) = self.node_mut(id) {
            node.phase = Phase::Idle;
            node.since = Instant::now();
        }
    }

    /// A Stop was asked for: flip the flag the in-flight model call polls and
    /// leave a Stop in the mailbox for everything else (a parked wait, a shell
    /// command, the next message boundary). Returns whether the run was *cut
    /// off* rather than stopped — a cancel that cannot land is a gone actor, and
    /// the row says so instead of showing work that can never finish
    /// (finding B6). A Stop that lands needs no value of its own: the row's
    /// `⊘ cancelling…` says it, and a Stop on an agent with nothing in flight
    /// leaves the row exactly as it was, so there is nothing to say either way.
    ///
    /// Only a run *in flight* is marked `⊘ cancelling…`. An agent that is
    /// idle, done, failed or already stopped has nothing to cancel: the actor
    /// absorbs the Stop and says nothing, so the mark and the `busy` flag that
    /// comes with it would outlive the act they describe — a row claiming work
    /// that will never happen, until a ten-second timer quietly cleared it
    /// (finding B6). What the actor *does* answer is a Stop on a run it really
    /// has: the run ends and its end-of-run event clears the mark at once.
    ///
    /// A dead mailbox on a run in flight is the one case that is not a stop at
    /// all: the human asked, but there was nobody left to ask, and the row says
    /// `⚠` — the run died where it stood and committed nothing — rather than
    /// `⊘`, whose whole meaning is "the actor is alive and a message resumes
    /// it" (finding H2). `true` is that case, and it tells the caller to tell
    /// the agent's parent, which is waiting for a result that will never come.
    pub fn cancel_requested(&mut self, id: AgentId) -> bool {
        if let Some(flag) = self.agent_cancel.get(&id) {
            flag.store(true, Ordering::SeqCst);
        }
        let heard = self
            .agent_tx
            .get(&id)
            .map(|tx| tx.send(AgentMsg::Stop).is_ok())
            .unwrap_or(false);
        let mut cut_off = false;
        if let Some(node) = self.node_mut(id) {
            if node.phase.is_busy() {
                // Say so immediately: the actor may be mid-request, and a row
                // that keeps spinning looks like the Stop was never heard.
                node.phase = if heard {
                    Phase::Cancelling
                } else {
                    // Nothing will ever answer, so the work it was showing can
                    // never finish — and what it *was* is the other thing the
                    // row has to say: it was cut off, not stopped.
                    cut_off = true;
                    Phase::CutOff
                };
                node.since = Instant::now();
            }
            // Otherwise the phase, and its clock, are left exactly as they
            // were: a Stop is not news about an agent that was not working.
        }
        cut_off
    }

    /// Retire `⊘` marks whose acknowledgement will never arrive, so a row
    /// cannot spin forever (finding B6). Only a mark on a run in flight can be
    /// here — `cancel_requested` never makes one anywhere else — so this is the
    /// backstop for a run that ended without saying so, not the normal path.
    /// Returns whether anything moved.
    pub fn expire_cancels(&mut self) -> bool {
        let stale: Vec<AgentId> = self
            .agents
            .iter()
            .filter(|node| {
                matches!(node.phase, Phase::Cancelling) && node.since.elapsed() > STALE_CANCEL
            })
            .map(|node| node.id)
            .collect();
        if stale.is_empty() {
            return false;
        }
        for id in stale {
            self.idle(id);
            self.agent_cancel.remove(&id);
        }
        true
    }

    /// A nudge is on its way: the row shows the agent thinking. Returns the
    /// phase it replaced, so a delivery that fails can put it back (B10).
    ///
    /// A fold is not replaced by it: the words land after the summarize call,
    /// not instead of it, and a row that started saying `thinking…` while the
    /// fold is still on the wire is the same lie the fold phase exists to stop.
    /// The run the nudge starts announces itself with `Running` when it does
    /// start.
    pub fn nudge(&mut self, id: AgentId) -> Option<Phase> {
        let previous = self.node(id).map(|node| node.phase.clone());
        if previous
            .as_ref()
            .is_some_and(|phase| phase.compacting().is_some())
        {
            return previous;
        }
        if let Some(node) = self.node_mut(id) {
            node.phase = Phase::Thinking;
            node.since = Instant::now();
        }
        previous
    }

    /// The nudge could not be delivered. Put the row back exactly as it was:
    /// leaving a `thinking…` on an agent nothing is running is a lie, and
    /// rewriting it to some other phase loses what the agent had achieved
    /// (finding B10).
    pub fn nudge_failed(&mut self, id: AgentId, was: Option<Phase>) {
        if let (Some(node), Some(was)) = (self.node_mut(id), was) {
            node.phase = was;
        }
    }

    /// The finished children past the history window: the nodes, transcripts
    /// and stored rows the tree may drop, oldest first.
    ///
    /// The window counts the children it is *allowed* to drop, not the children
    /// that exist: an agent whose result nobody has read does not spend one of
    /// the [`CHILD_HISTORY`] slots, so it cannot push an older, droppable child
    /// out of the window and make a run's memory look bounded when it is not.
    ///
    /// Oldest first, so what survives is the newest window: a run that spawned
    /// fifty-one children loses its first row and keeps numbers 2..=51, a gap
    /// at the *end* of the history where it reads as history. Reaping from the
    /// middle — whichever node happened to become eligible first — would leave
    /// the same count with holes in it, and a hole is the one shape a row list
    /// cannot explain.
    ///
    /// Nothing kept hangs under any of these rows ([`Self::reapable`]), so a row
    /// that goes never takes a reader with it. Its own eligible children do
    /// stay — a node does not inherit its parent's age — and a link to a parent
    /// that is not in the tree is a top-level row in the painted order
    /// ([`Self::rows`]), so a forgotten parent can never hide a child.
    pub fn past_history(&self) -> Vec<AgentId> {
        let jobs_live = self.live_job_count() > 0;
        // One walk for the whole question "is anything below this node owed
        // something", rather than a scan per candidate (finding R29).
        let kept_above = self.kept_above(jobs_live);
        let mut eligible: Vec<AgentId> = self
            .agents
            .iter()
            .filter(|node| self.reapable(node, &kept_above, jobs_live))
            .map(|node| node.id)
            .collect();
        // The vector is spawn order, oldest first, so the oldest `over` of them
        // are exactly the ones the window is over: what is left in it after the
        // truncation is the list of children to drop.
        let over = eligible.len().saturating_sub(CHILD_HISTORY);
        eligible.truncate(over);
        eligible
    }

    /// Whether the history window may forget this node — the whole predicate,
    /// in one place, because a wrong `true` here is a transcript nobody can
    /// open again and there is no archive to fall back on (§8.21).
    ///
    /// Everything it asks is "can news still arrive here", and ineligible is
    /// always the safe answer. The rules split in two, which is what the two
    /// halves of the test below are:
    ///
    /// 1. nothing about *this* node needs it: [`Self::kept`] — not the root, not
    ///    the agent whose transcript the human is reading ([`Self::focused`]),
    ///    not a run in flight, not a job of its own that a `Shutdown` would
    ///    kill, not a result its parent's model has not been handed (`✉`, the
    ///    `wait`/fold protocol, finding H4), and not an isolated agent whose
    ///    work was never landed — the row is the only thing that names
    ///    `mush/<id>`;
    /// 2. nothing *below* it is owed anything: nothing it hangs over is kept
    ///    ([`Self::kept_above`]) — a napping agent's node is where its
    ///    children's completions fold in, and a descendant wearing `✉` is news
    ///    this node's transcript is the only place to read.
    ///
    /// A leftover worktree found on disk needs no rule of its own: it is
    /// isolated by construction (`branch` set, `landed` unset), so rule 1 keeps
    /// it and its row.
    fn reapable(&self, node: &AgentNode, kept_above: &HashSet<AgentId>, jobs_live: bool) -> bool {
        !self.kept(node, jobs_live) && !kept_above.contains(&node.id)
    }

    /// Whether the window must keep this node for its own sake: the six rules of
    /// [`Self::reapable`], in the one place either window reads them, because a
    /// wrong `false` here is a transcript nobody can open again and there is no
    /// archive to fall back on (§8.21).
    ///
    /// Everything it asks is "can news still arrive here", and kept is always
    /// the safe answer: the root and the agent whose transcript the human is
    /// reading ([`Self::focused`], whose pane a reap would blank); work in
    /// flight, which has a result coming ([`Self::in_flight`]) — a job of its
    /// own included, which a `Shutdown` would kill (`kill_owned`,
    /// `agent::absorb`); a result its parent's model
    /// has not been handed (`✉`, [`Self::result_read`], finding H4); and an
    /// isolated agent whose work was never landed, whose row is the only thing
    /// that names `mush/<id>` — a branch a human may still have to look at.
    ///
    /// Exempting the unlanded is safe to be conservative about: a sibling patch
    /// bounds worktrees at `MAX_WORKTREES = 70`, *above* this window, precisely
    /// so that a spawn can never be refused for want of a slot (§8.21); the row
    /// is planted on the worktree, not the other way round.
    fn kept(&self, node: &AgentNode, jobs_live: bool) -> bool {
        node.id == AgentId::ROOT
            || node.id == self.focused
            || self.in_flight_with(node, jobs_live)
            || node.result_unread
            || (node.branch.is_some() && node.landed.is_none())
    }

    /// Every node with something *kept* below it, at any depth: the ancestors of
    /// the nodes [`Self::kept`] refuses, derived in one walk up the parent links
    /// from each of them.
    ///
    /// The subtree and not just the direct children, because a row is the whole
    /// of what a parent is: a napping agent's node is where its children's
    /// completions fold, a parent still wearing `✉` is owed a read, and an
    /// isolated branch under it is work nobody has landed. Forgetting an
    /// ancestor of any of those leaves a report with no reader — a completion
    /// notifies a mailbox that no longer exists — and a row whose reader cannot
    /// ever fold it in. Nor does a node inherit safety from its parent: what is
    /// kept below is kept *up*, which is why this is the ancestors rather than
    /// the descendants.
    fn kept_above(&self, jobs_live: bool) -> HashSet<AgentId> {
        let mut above: HashSet<AgentId> = HashSet::new();
        for node in self.agents.iter().filter(|node| self.kept(node, jobs_live)) {
            // The root has no parent, and `kept` names it only so that no
            // caller has to ask twice; the walk below is a no-op for it.
            let mut at = node.parent;
            while let Some(id) = at {
                // An ancestor already marked has its own ancestors marked too:
                // this is what keeps the walk linear in the tree rather than in
                // the number of kept nodes.
                if !above.insert(id) {
                    break;
                }
                at = self.node(id).and_then(|node| node.parent);
            }
        }
        above
    }

    /// The children whose actor thread is no longer worth keeping, oldest
    /// first: the ones `App::park_history` may send a `Shutdown` to.
    ///
    /// Ranks are counted over the children in spawn order, oldest first, so
    /// "the newest [`WARM_CHILDREN`]" is a promise about the rows a human has
    /// just been watching — the child they are most likely to nudge. Past the
    /// [`CHILD_HISTORY`] window the read mark stops mattering as well: the node
    /// and the transcript keep the result either way, and the next message wakes
    /// the actor back into it.
    ///
    /// One condition is about not *losing* anything, and it yields to no age
    /// however far outside a window a child is:
    ///
    /// - **children.** A parent is a *reader*: its transcript is where its
    ///   children's completions fold, and its mailbox is the channel they
    ///   travel on (a child clones its parent's mailbox at spawn). Parking it
    ///   would drop those reports on the floor, and a revived parent could not
    ///   get them back — its children hold the mailbox the old actor had. So the
    ///   *leaf* is the unit of parking, which is also where the threads are: a
    ///   wide tree of fifty children has one parent.
    ///
    /// That one rule is also what keeps a napping agent's thread: a leaf has no
    /// descendants, so "nothing kept below it" — the second half of the history
    /// window's predicate — needs no second copy here. The pane the human is
    /// reading is kept warm as well ([`Self::may_park`]), one thread that costs
    /// nothing and is the one about to be typed at.
    pub fn parkable(&self) -> Vec<AgentId> {
        // Every node that is some child's reader, in one pass: whether a
        // candidate is a leaf is asked of each one, and a scan per candidate
        // would make one frame quadratic in the size of the history the window
        // just capped.
        let readers: HashSet<AgentId> = self.agents.iter().filter_map(|node| node.parent).collect();
        // The registry with nothing running — the ordinary state of a finished
        // tree — cannot hold a job for any of these children, so the per-
        // candidate lookup is only worth its lock when there is something to
        // find. (This is asked on every frame.)
        let jobs_live = self.live_job_count() > 0;
        let children: Vec<&AgentNode> = self
            .agents
            .iter()
            .filter(|node| node.parent.is_some())
            .collect();
        // A rank counted from the oldest child, so `len - N` is the first of the
        // newest N: the warm window and the reap window are one arithmetic.
        let warm = children.len().saturating_sub(WARM_CHILDREN);
        let window = children.len().saturating_sub(CHILD_HISTORY);
        children
            .iter()
            .enumerate()
            .filter(|(rank, node)| {
                *rank < warm && self.may_park(*rank < window, node, &readers, jobs_live)
            })
            .map(|(_, node)| node.id)
            .collect()
    }

    /// Whether one child's thread may be parked: at rest, alone, and — unless it
    /// is past the reap window — done being listened to. See [`Self::parkable`]
    /// for why each of these exists.
    fn may_park(
        &self,
        past_window: bool,
        node: &AgentNode,
        readers: &HashSet<AgentId>,
        jobs_live: bool,
    ) -> bool {
        // Inside the window, an unread result is what a nudge is *for*: the
        // child keeps its thread until its parent's model has been handed the
        // line. Past the window the result is a fact on the node and a line in
        // the transcript like any other, and a message brings the actor back to
        // it.
        let result_read = !node.result_unread || past_window;
        // The one agent whose thread is never reclaimed: the pane the human is
        // reading is the child they are most likely to type at this moment, and
        // this window runs on a tick — a phase can be one message stale, so a
        // park could otherwise cancel the very run those words just started.
        // The reaper excludes it for the same reason (`Self::kept`).
        let focused = node.id == self.focused;
        !focused
            && !self.in_flight_with(node, jobs_live)
            && !readers.contains(&node.id)
            && result_read
    }

    /// Drop nodes that are gone — a leftover whose worktree no longer exists,
    /// an agent the history window forgot — and everything keyed by their ids.
    /// The focus and the cursor are put back on a node that is really there:
    /// reaping used to leave them on a ghost, so the pane stayed titled
    /// `agent #4` while typing reported that the agent was gone (finding B11).
    pub fn reap(&mut self, gone: &[AgentId]) {
        if gone.is_empty() {
            return;
        }
        self.agents.retain(|node| !gone.contains(&node.id));
        for id in gone {
            // Dropping the last sender ends the actor: an idle agent whose
            // mailbox is gone has nothing left to wait for.
            self.agent_tx.remove(id);
            self.agent_cancel.remove(id);
            self.agent_stats.remove(id);
        }
        self.repair_focus();
    }

    /// Point the focus and the cursor at nodes that exist.
    pub fn repair_focus(&mut self) {
        if !self.has(self.focused) {
            self.focused = AgentId::ROOT;
        }
        self.agent_cursor = self.agent_cursor.min(self.agents.len().saturating_sub(1));
    }

    /// The tree's rows, in the order the pane paints them: pre-order over the
    /// parent links, so every child sits directly under its parent — above that
    /// parent's later siblings — and its own children under it (finding U4).
    ///
    /// Derived on read rather than stored beside `agents`: that vector is spawn
    /// order (it is the order things happened, and the order a stored session
    /// keeps), and a second copy of "the order" is one more thing that can
    /// disagree with the tree the human is looking at. A node whose parent is
    /// not in the tree — a leftover worktree, an agent whose parent was reaped
    /// — is a top-level row, so a broken link can never hide an agent.
    pub fn rows(&self) -> Vec<&AgentNode> {
        let mut rows = Vec::with_capacity(self.agents.len());
        for node in &self.agents {
            if self.parent_in_tree(node).is_none() {
                self.grow(node, &mut rows);
            }
        }
        // A link that pointed back up its own line (which nothing here can
        // build) would leave part of the tree unreachable, and a missing row is
        // an agent the human cannot see: whatever the walk missed is taken in
        // storage order. Every node is therefore painted exactly once.
        for node in &self.agents {
            if !rows.iter().any(|row| row.id == node.id) {
                rows.push(node);
            }
        }
        rows
    }

    /// The parent this node hangs under, when that parent is still in the tree.
    fn parent_in_tree(&self, node: &AgentNode) -> Option<AgentId> {
        node.parent.filter(|parent| self.has(*parent))
    }

    /// `node` and then its subtree, in spawn order among siblings — the order a
    /// later brother appears after the earlier one's whole family.
    fn grow<'a>(&'a self, node: &'a AgentNode, rows: &mut Vec<&'a AgentNode>) {
        if rows.iter().any(|row| row.id == node.id) {
            return;
        }
        rows.push(node);
        for child in self.agents.iter().filter(|n| n.parent == Some(node.id)) {
            self.grow(child, rows);
        }
    }

    /// Whether any agent in the tree is working. Derived from the phases, never
    /// stored: a cached flag is one more thing that can disagree with the rows
    /// it is drawn from (finding B5).
    pub fn busy(&self) -> bool {
        self.agents.iter().any(|node| node.phase.is_busy())
    }

    /// Who is doing what, one bucket per agent.
    ///
    /// The pane title reads these, and they are derived from the phases every
    /// frame: a count is a fact like any other, and the title that counted
    /// agents napping on their children as "running" was reading the wrong
    /// fact (finding U2).
    ///
    /// Each agent lands in at most one bucket, so nothing is counted twice for
    /// having children. A run being cancelled counts as working: the actor has
    /// not yielded and the work really is in flight (its row wears `⊘` and says
    /// `cancelling…`).
    pub fn roster(&self) -> Roster {
        let busy = self.busy_counts();
        let mut roster = Roster::default();
        for node in &self.agents {
            if node.phase.is_busy() {
                roster.working += 1;
            } else if self.napping_with(node.id, &busy) {
                // At rest with work out: §5.5's napping orchestrator, which the
                // row draws as `⏸`. Counted here and *nowhere else* — counting
                // it as working as well is exactly what the title did wrong
                // (U2), and a stop or a failure is not a reason to say it waits
                // for nothing (U12: the mailbox is just as alive).
                roster.waiting += 1;
            }
        }
        roster
    }

    /// The per-parent count of children whose run is in flight, derived in one
    /// walk. The pane builds this map once and looks each row up in it, and the
    /// title's `M waiting` and [`Self::napping`] read the same entries. A
    /// derivation, not a stored fact, dropped with the frame (finding R29).
    pub fn busy_counts(&self) -> HashMap<AgentId, usize> {
        let mut busy: HashMap<AgentId, usize> = HashMap::new();
        for node in &self.agents {
            if let (Some(parent), true) = (node.parent, node.phase.is_busy()) {
                *busy.entry(parent).or_insert(0) += 1;
            }
        }
        busy
    }

    /// Whether this agent is at rest with work still out — §5.5's napping
    /// orchestrator: it ended (or stopped, or failed) its turn while children
    /// run, and a completion will fold in and start a fresh run.
    ///
    /// The one predicate the title's `waiting` bucket and the bar's sentence
    /// read, because a stopped or failed agent's mailbox is exactly as alive as
    /// an idle one's: the child's completion folds in and wakes it all the same,
    /// so the title saying `0 waiting` while the bar promises `the root resumes
    /// as they finish` was one fact derived two ways (finding U12).
    pub fn napping(&self, id: AgentId) -> bool {
        self.napping_with(id, &self.busy_counts())
    }

    /// [`Self::napping`] against a `busy` map already derived: one predicate,
    /// whether the frame passes the map it built or a single call scans for its
    /// own (finding R29).
    fn napping_with(&self, id: AgentId, busy: &HashMap<AgentId, usize>) -> bool {
        self.node(id)
            .is_some_and(|node| !node.phase.is_busy() && busy.get(&id).copied().unwrap_or(0) > 0)
    }

    /// The children of `id` whose results it has not read, in tree order.
    ///
    /// The same fact the children's own rows wear as `✉`, read from the other
    /// end: the child's mark says *which* result is unread, and this says who is
    /// owed a read — which is the question a human arrives with ("did #2 see
    /// #6?"), and the only half of it that survives a short pane showing a
    /// window of a big tree (finding H4). Derived from the nodes on every call,
    /// so the two ends cannot disagree.
    pub fn unread_children(&self, id: AgentId) -> Vec<AgentId> {
        self.agents
            .iter()
            .filter(|node| node.parent == Some(id) && node.result_unread)
            .map(|node| node.id)
            .collect()
    }

    /// How many of `id`'s own children have work in flight: the single-node
    /// entry into [`Self::busy_counts`], the one derivation, for the caller
    /// that has no map in hand (the roster's row, `screen.rs`'s `agent_row`).
    ///
    /// It is about the children, never about the parent's own phase: a working
    /// agent whose children work is still working (finding U1).
    pub fn busy_children(&self, id: AgentId) -> usize {
        self.busy_counts().get(&id).copied().unwrap_or(0)
    }

    /// Show one agent's transcript, if it is in the tree.
    pub fn focus(&mut self, id: AgentId) -> bool {
        if !self.has(id) {
            return false;
        }
        self.focused = id;
        true
    }

    /// Focus the agent under the cursor, returning it.
    pub fn focus_cursor(&mut self) -> Option<AgentId> {
        // Through the *painted* rows, not the storage vector: a child sits under
        // its parent, so the two orders differ and `Enter` must focus the row
        // the human is pointing at (finding U4).
        let id = self.cursor_id()?;
        self.focused = id;
        Some(id)
    }

    /// Point the tree cursor at one agent's row, if the tree has one, so a
    /// caller that holds an id rather than a keystroke can act on the row the
    /// human would. Returns whether the agent is in the tree; a caller that
    /// then focuses the cursor (`focus_cursor`) focuses exactly the agent
    /// `Enter` on its row would (M3's `focus`).
    pub fn point_cursor_at(&mut self, id: AgentId) -> bool {
        match self.rows().iter().position(|node| node.id == id) {
            Some(index) => {
                self.agent_cursor = index;
                true
            }
            None => false,
        }
    }

    /// The id of the row under the cursor: the one place a cursor position is
    /// turned into an agent. Every caller that asks "which row is selected"
    /// comes through here, so the row painted with the highlight and the agent
    /// a key acts on cannot be two different rows (finding U4).
    pub fn cursor_id(&self) -> Option<AgentId> {
        self.rows().get(self.cursor()).map(|node| node.id)
    }

    /// Move the tree cursor one row, without leaving the tree. Rows are the
    /// painted ones: `j`/`k` walk the tree the human sees, not the order the
    /// agents happened to be spawned in (finding U4).
    pub fn move_cursor(&mut self, delta: i64) {
        if delta > 0 {
            if self.agent_cursor + 1 < self.agents.len() {
                self.agent_cursor += 1;
            }
        } else {
            self.agent_cursor = self.agent_cursor.saturating_sub(1);
        }
    }

    pub fn cursor_top(&mut self) {
        self.agent_cursor = 0;
    }

    pub fn cursor_bottom(&mut self) {
        // One row per agent, so the storage length is the number of rows.
        self.agent_cursor = self.agents.len().saturating_sub(1);
    }

    /// The row the agent pane paints as selected.
    pub fn cursor(&self) -> usize {
        // Clamped by the storage length, which is the row count: `rows` paints
        // every agent exactly once (finding U4).
        self.agent_cursor.min(self.agents.len().saturating_sub(1))
    }

    /// Whether `id` is in the tree: the question [`Self::node`] also answers.
    pub fn has(&self, id: AgentId) -> bool {
        self.node(id).is_some()
    }

    pub fn node(&self, id: AgentId) -> Option<&AgentNode> {
        self.agents.iter().find(|node| node.id == id)
    }

    fn node_mut(&mut self, id: AgentId) -> Option<&mut AgentNode> {
        self.agents.iter_mut().find(|node| node.id == id)
    }

    fn is_busy(&self, id: AgentId) -> bool {
        self.node(id)
            .map(|node| node.phase.is_busy())
            .unwrap_or(false)
    }

    /// Age a node, so the tests that assert a rendered age do not have to wait.
    #[cfg(test)]
    pub fn age(&mut self, id: AgentId, by: Duration) {
        if let Some(node) = self.node_mut(id) {
            node.since = Instant::now() - by;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::Receiver;

    /// A child with a live mailbox (the receive half is kept alive by the
    /// caller), so the rules about a mailbox that is really there can be told
    /// apart from the ones about a mailbox that is not.
    fn child(tree: &mut AgentTree, id: u64) -> (Opened, Receiver<AgentMsg>) {
        let (tx, rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let opened = tree.insert(Spawn {
            id: AgentId(id),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            cmd: tx,
        });
        (opened, rx)
    }

    /// A child of `parent`, so a test can build a tree whose spawn order is not
    /// its tree order.
    fn spawn(tree: &mut AgentTree, id: u64, parent: u64, depth: usize) -> Receiver<AgentMsg> {
        let (tx, rx) = crossbeam_channel::unbounded::<AgentMsg>();
        tree.insert(Spawn {
            id: AgentId(id),
            parent: AgentId(parent),
            brief: format!("#{id}"),
            depth,
            branch: None,
            cmd: tx,
        });
        rx
    }

    fn leftover(id: u64) -> Existing {
        Existing {
            id: AgentId(id),
            parent: None,
            depth: 1,
            brief: "leftover worktree".to_string(),
            title: None,
            phase: Phase::Done,
            branch: Some(format!("mush/{id}")),
            summary: Some("found on startup".to_string()),
            leftover: true,
            landed: None,
            result_unread: false,
            tx: None,
        }
    }

    /// A child is work in flight: thinking, with no result of its own, and its
    /// brief as the opening line of its transcript (finding B13).
    #[test]
    fn an_inserted_child_is_thinking_with_its_brief_as_the_opening_line() {
        let mut tree = AgentTree::bare();
        let (opened, _rx) = child(&mut tree, 1);
        let id = opened.id;

        let node = tree.node(id).unwrap();
        assert_eq!(node.phase, Phase::Thinking);
        assert_eq!(node.parent, Some(AgentId::ROOT));
        assert_eq!(node.depth, 1);
        assert!(!node.leftover);
        assert_eq!(node.summary, None);
        // The brief is the opening line of the child's transcript, and it is
        // handed to the conversation rather than stored here (finding B13).
        assert_eq!(opened.opening.role, "user");
        assert_eq!(opened.opening.text(), "lexer");
        assert!(tree.busy());
    }

    /// A wait is a different fact from work, and it is derived from the label
    /// the actor wrote — the tool's own name, so a rename cannot leave this
    /// behind (finding U7).
    #[test]
    fn a_parked_run_knows_what_it_is_waiting_on() {
        for phase in [
            Phase::Thinking,
            Phase::Idle,
            Phase::Done,
            Phase::Stopped,
            Phase::Cancelling,
            Phase::Failed("no route".to_string()),
            Phase::Activity("edit_file src/a.rs".to_string()),
            // A label that merely *starts* like a tool name is not a tool.
            Phase::Activity("wait_agent".to_string()),
        ] {
            assert_eq!(phase.waiting(), None, "{phase:?} is not a wait");
        }

        // The actor's label is the tool name plus its summarized arguments,
        // which are empty for a wait with none — hence the trailing space.
        assert_eq!(
            Phase::Activity("wait ".to_string()).waiting(),
            Some(jobs::Waited)
        );
    }

    /// A fold is a phase of its own, and the words that describe it are one
    /// derivation — the row, the footer, the bar and the transcript's foot read
    /// `compacting()`, so none of them can call the same fold something else
    /// (finding U11). Pinned here because the whole point of a state nobody
    /// could see is that a later edit can drop it without a test noticing.
    #[test]
    fn a_fold_is_a_phase_with_words_of_its_own() {
        // The three kinds of fold are told apart, and none of them borrows the
        // words of a run: `working…`/`thinking…` is what the actor is doing
        // instead of folding.
        let kinds = [
            (Compacting::Parked, "folding at the next step…"),
            (Compacting::Requested, "compacting…"),
            (Compacting::NearlyFull, "context nearly full — compacting…"),
        ];
        for (kind, words) in kinds {
            assert_eq!(Phase::Compacting(kind).compacting(), Some(kind));
            assert_eq!(kind.words(), words);
            assert!(
                words.contains(kind.verb()),
                "{kind:?}'s sentence has to say what it is doing: {words:?}"
            );
            assert!(
                Phase::Compacting(kind).is_busy(),
                "a fold is work in flight, not a nap: {kind:?}"
            );
        }
        // Every word is about folding, and the three are three answers.
        let spellings: Vec<&str> = kinds.iter().map(|(kind, _)| kind.words()).collect();
        let mut unique = spellings.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 3, "a fold's three states are three sentences");
        for words in spellings {
            assert!(
                words.contains("compact") || words.contains("fold"),
                "{words}"
            );
        }

        // No other phase claims to be a fold, and a parked fold is not a wait:
        // a status line that merely says "compacting" is still a status line.
        for phase in [
            Phase::Idle,
            Phase::Thinking,
            Phase::Done,
            Phase::Stopped,
            Phase::Cancelling,
            Phase::Failed("no route".to_string()),
            Phase::Activity("edit_file src/a.rs".to_string()),
            Phase::Activity("compacting…".to_string()),
        ] {
            assert_eq!(phase.compacting(), None, "{phase:?} is not a fold");
            assert_eq!(phase.waiting(), None, "{phase:?} is not a wait");
        }
    }

    /// A phase has one name for every reader: the roster's machine name
    /// ([`Phase::label`]) and the form that fits on the quit line
    /// ([`Phase::doing`]) differ for exactly one phase — the tool label, whose
    /// own name the bar has room for — so no surface can call the same phase
    /// two things (refactor R22).
    ///
    /// The collapse this ends: `doing` used to answer `idle` for `Stopped`,
    /// `Done` and `Failed`, so the quit line named a stopped agent that still
    /// owned a running job as `#0 idle + 1 job` (findings §6).
    #[test]
    fn a_phase_has_one_name_for_every_reader() {
        for phase in [
            Phase::Idle,
            Phase::Thinking,
            Phase::Compacting(Compacting::Requested),
            Phase::Cancelling,
            Phase::Stopped,
            Phase::CutOff,
            Phase::Done,
            Phase::Failed("no route".to_string()),
        ] {
            assert_eq!(phase.doing(), phase.label(), "{phase:?}");
        }
        let edit = Phase::Activity("edit_file src/lib.rs".to_string());
        assert_eq!(
            edit.label(),
            "working",
            "the roster carries the machine name"
        );
        assert_eq!(edit.doing(), "edit_file", "the bar carries the tool's own");
        // A label with no words in it is still a word: nothing may read `#0 `.
        assert_eq!(Phase::Activity(String::new()).doing(), "working");
    }

    /// A fold from rest is visible — the hole `activity` could not fill, because
    /// it refuses a status line from an agent that is not already busy — and it
    /// leaves the row when it ends, whichever way it ends (finding U11).
    #[test]
    fn a_fold_from_rest_is_visible_and_leaves_when_it_ends() {
        let mut tree = AgentTree::bare();
        let flag = Arc::new(AtomicBool::new(false));
        tree.finish(AgentId::ROOT, Some("the last reply".to_string()));

        tree.compacting(AgentId::ROOT, Compacting::Requested, Some(flag.clone()));
        assert_eq!(
            tree.node(AgentId::ROOT).unwrap().phase,
            Phase::Compacting(Compacting::Requested),
            "an idle agent's fold is on its row"
        );
        assert!(tree.busy(), "and it counts as work while it runs");
        assert_eq!(
            tree.roster().working,
            1,
            "the pane title counts it as working, like the row says"
        );
        // The handle is where a Stop looks: an idle fold has no run's flag.
        assert!(tree.agent_cancel.contains_key(&AgentId::ROOT));

        // A status that arrives while it folds (a tool label from the run
        // behind a parked one, a late line) does not erase it.
        tree.activity(AgentId::ROOT, "edit_file src/a.rs");
        assert_eq!(
            tree.node(AgentId::ROOT).unwrap().phase,
            Phase::Compacting(Compacting::Requested),
            "the fold is what the human asked for; a label does not replace it"
        );

        // It landed: the row is a finished thing again, and the flag is gone
        // with it.
        tree.compacted(AgentId::ROOT, false);
        assert_eq!(tree.node(AgentId::ROOT).unwrap().phase, Phase::Done);
        assert!(!tree.busy());
        assert!(
            !tree.agent_cancel.contains_key(&AgentId::ROOT),
            "a fold that landed leaves no cancelled-looking flag behind"
        );
    }

    /// A fold that came to nothing cannot leave `compacting…` on the row: a
    /// failed fold is the notice's to report, and a fold that was stopped is a
    /// stop — neither is an agent that is still folding (finding U11).
    #[test]
    fn a_fold_that_failed_or_was_stopped_stops_claiming_to_be_one() {
        let mut tree = AgentTree::bare();
        tree.compacting(AgentId::ROOT, Compacting::Requested, None);
        tree.compacting_ended(AgentId::ROOT, false);
        assert_eq!(
            tree.node(AgentId::ROOT).unwrap().phase,
            Phase::Idle,
            "nothing is in flight, so nothing claims to be"
        );

        // The same inside a run: the run wears `thinking…`, not a fold.
        tree.compacting(AgentId::ROOT, Compacting::NearlyFull, None);
        tree.compacting_ended(AgentId::ROOT, true);
        assert_eq!(tree.node(AgentId::ROOT).unwrap().phase, Phase::Thinking);

        // An event that arrives after the fold already ended changes nothing:
        // the run it belonged to keeps whatever phase it has.
        tree.activity(AgentId::ROOT, "run_command cargo test");
        tree.compacting_ended(AgentId::ROOT, false);
        assert_eq!(
            tree.node(AgentId::ROOT).unwrap().phase,
            Phase::Activity("run_command cargo test".to_string()),
            "a fold that is over does not blank a run that is not"
        );
    }

    /// A fold is not replaced by the labels of the run around it — those are
    /// the labels that used to hide it — and it is not replaced by a stray one
    /// either. What ends it is its own end, or the run's.
    #[test]
    fn a_fold_is_not_replaced_by_the_labels_of_the_run_around_it() {
        let mut tree = AgentTree::bare();
        tree.begin(AgentId::ROOT, None);
        tree.compacting(AgentId::ROOT, Compacting::Parked, None);
        tree.activity(AgentId::ROOT, "edit_file src/a.rs");
        assert_eq!(
            tree.node(AgentId::ROOT).unwrap().phase,
            Phase::Compacting(Compacting::Parked),
            "the request the human is waiting for outranks a tool label"
        );

        // It fires: the summarize call is on the wire, which is the same fold
        // said more precisely, and a label still does not erase it.
        tree.compacting(AgentId::ROOT, Compacting::Requested, None);
        tree.activity(AgentId::ROOT, "edit_file src/b.rs");
        assert_eq!(
            tree.node(AgentId::ROOT).unwrap().phase,
            Phase::Compacting(Compacting::Requested)
        );

        // The run ending is a different fact, and it does end it: the row is
        // about the agent, so a completed run stops showing a fold it finished
        // with.
        tree.finish(AgentId::ROOT, Some("the run's reply".to_string()));
        assert_eq!(tree.node(AgentId::ROOT).unwrap().phase, Phase::Done);
    }

    /// A node carrying a brief, so the title derived from it can be read.
    fn titled(brief: &str) -> String {
        let mut tree = AgentTree::bare();
        let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
        let opened = tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: brief.to_string(),
            depth: 1,
            branch: None,
            cmd: tx,
        });
        tree.node(opened.id).expect("the node was inserted").title()
    }

    /// An agent is not a bare number: its title is the artifact its brief
    /// names, or the first word that says anything about the task (finding
    /// U6). The two briefs below are identical for their first twenty columns
    /// and name two different files.
    #[test]
    fn a_title_is_derived_from_the_brief() {
        assert_eq!(
            titled("create a file called deep.txt containing exactly: deep work"),
            "deep.txt"
        );
        assert_eq!(
            titled("create a file called wide.txt containing exactly: wide work"),
            "wide.txt"
        );
        assert_eq!(
            titled("edit crates/mush/src/ui.rs so the rows fit"),
            "crates/mush/src/ui.rs"
        );

        // No path: the first word that is not filler.
        assert_eq!(titled("build a lexer for the config format"), "lexer");
        assert_eq!(
            titled("you must delegate the lexer work to a subagent"),
            "lexer"
        );
        assert_eq!(titled("write the token table"), "token");

        // Nothing but filler falls back to the first word; a brief with no
        // words has no title rather than a wrong one.
        assert_eq!(titled("the a of"), "the");
        assert_eq!(titled(""), "");

        // A handle, not a sentence: a title is bounded, and a path keeps its
        // head, which is the part that names the directory.
        let long = titled("edit src/very/deep/directory/structure/file.rs now");
        assert!(long.chars().count() <= TITLE_COLUMNS, "{long}");
        assert!(long.starts_with("src/very/"), "{long}");
    }

    /// `busy` is derived, never stored, so it cannot disagree with the rows it
    /// is drawn from (finding B5).
    #[test]
    fn busy_is_derived_from_the_phases() {
        let mut tree = AgentTree::bare();
        assert!(!tree.busy(), "an idle root is not work");

        let (opened, _rx) = child(&mut tree, 1);
        let id = opened.id;
        assert!(tree.busy());

        tree.finish(id, Some("done".to_string()));
        assert!(!tree.busy(), "a finished run is at rest");

        tree.fail(id, "no route to host".to_string());
        assert!(!tree.busy(), "a failed run is at rest too");

        tree.begin(id, None);
        assert!(tree.busy());

        tree.stopped(id);
        assert!(!tree.busy(), "and a stopped one");
    }

    /// The title's counts are derived from the phases, and no agent is in two
    /// buckets: a parent napping on a working child is waiting, never also
    /// working (finding U2).
    #[test]
    fn the_roster_buckets_each_agent_once() {
        let mut tree = AgentTree::bare();
        assert_eq!(tree.roster(), Roster::default(), "nothing is happening yet");

        // One child, working, under an idle root.
        let (opened, _rx) = child(&mut tree, 1);
        assert_eq!(
            tree.roster(),
            Roster {
                working: 1,
                waiting: 1
            },
            "the working child, and the root napping on it"
        );

        // The child's run ends with a grandchild of its own working: the child
        // naps on it (its own children are the unit, the same one its row's
        // `⏸N` counts), and the root — whose own child is done — is in no
        // bucket at all.
        tree.finish(opened.id, Some("spawned #2".to_string()));
        let (tx, _rx2) = crossbeam_channel::unbounded::<AgentMsg>();
        let grandchild = tree.insert(Spawn {
            id: AgentId(2),
            parent: AgentId(1),
            brief: "deep.txt".to_string(),
            depth: 2,
            branch: None,
            cmd: tx,
        });
        assert_eq!(
            tree.roster(),
            Roster {
                working: 1,
                waiting: 1
            },
            "the grandchild works and its parent naps"
        );

        // A stopped agent waits for nothing, so it is in no bucket: its own
        // `⊘` row is where that fact lives.
        tree.stopped(grandchild.id);
        assert_eq!(tree.roster(), Roster::default());
    }

    /// Rows come out in tree order, not in the order the agents were spawned
    /// (finding U4).
    #[test]
    fn rows_are_pre_order_over_the_parent_links() {
        let mut tree = AgentTree::bare();
        // Spawn order that is deliberately not tree order: the root's second
        // child exists before the first child's own child does.
        let _a = spawn(&mut tree, 1, 0, 1);
        let _b = spawn(&mut tree, 2, 0, 1);
        let _c = spawn(&mut tree, 3, 1, 2); // spawned by #1
        let _d = spawn(&mut tree, 4, 3, 3); // spawned by #3

        let ids: Vec<u64> = tree.rows().iter().map(|node| node.id.0).collect();
        assert_eq!(
            ids,
            vec![0, 1, 3, 4, 2],
            "a child under its parent, above its parent's later brothers"
        );

        // The cursor indexes the painted rows, so it lands on the agent the
        // human is pointing at — not on whatever spawn order holds there.
        tree.move_cursor(1);
        tree.move_cursor(1);
        assert_eq!(tree.cursor_id(), Some(AgentId(3)));
        tree.cursor_bottom();
        assert_eq!(tree.cursor_id(), Some(AgentId(2)), "`G` is the last row");
        tree.cursor_top();
        assert_eq!(tree.focus_cursor(), Some(AgentId::ROOT), "`g` is the root");
        tree.cursor_bottom();
        assert_eq!(tree.focus_cursor(), Some(AgentId(2)));
    }

    /// Every agent is painted exactly once, even one whose parent is not in the
    /// tree: a row that cannot be reached would be an agent the human cannot
    /// see (finding U4).
    #[test]
    fn every_agent_is_painted_once_even_without_its_parent() {
        let mut tree = AgentTree::bare();
        let _child = spawn(&mut tree, 1, 0, 1);
        tree.register(leftover(2));
        // A child whose parent is gone: #3 hangs under #9, which is not here.
        let _orphan = spawn(&mut tree, 3, 9, 2);

        let ids: Vec<u64> = tree.rows().iter().map(|node| node.id.0).collect();
        assert_eq!(ids, vec![0, 1, 2, 3]);
        assert_eq!(tree.rows().len(), tree.agents.len());
    }

    /// A status that arrives after the run ended must not put a finished agent
    /// back to work (finding B5).
    #[test]
    fn a_status_after_the_run_ended_is_ignored() {
        let mut tree = AgentTree::bare();
        let (opened, _rx) = child(&mut tree, 1);
        let id = opened.id;

        tree.activity(id, "edit_file src/lex.rs");
        assert_eq!(
            tree.node(id).unwrap().phase,
            Phase::Activity("edit_file src/lex.rs".to_string()),
            "a running agent's activity is what it is doing"
        );

        tree.finish(id, Some("did the work".to_string()));
        tree.activity(id, "committed abc123 on mush/1");
        assert_eq!(
            tree.node(id).unwrap().phase,
            Phase::Done,
            "a late status must not restart a finished agent"
        );

        // The same for a failure, and for an agent that never ran: only work in
        // flight can report activity.
        tree.begin(id, None);
        tree.fail(id, "no route to host".to_string());
        tree.activity(id, "retrying");
        assert_eq!(
            tree.node(id).unwrap().phase,
            Phase::Failed("no route to host".to_string())
        );
        tree.activity(AgentId::ROOT, "reading the tree");
        assert_eq!(tree.node(AgentId::ROOT).unwrap().phase, Phase::Idle);
    }

    /// The row describes the run that just ended, not the first one forever:
    /// `begin` clears the summary, `finish` always replaces it (finding B14).
    #[test]
    fn begin_clears_the_summary_and_finish_always_replaces_it() {
        let mut tree = AgentTree::bare();

        tree.finish(AgentId::ROOT, Some("first result".to_string()));
        assert_eq!(
            tree.node(AgentId::ROOT).unwrap().summary.as_deref(),
            Some("first result")
        );

        tree.begin(AgentId::ROOT, None);
        assert_eq!(
            tree.node(AgentId::ROOT).unwrap().summary,
            None,
            "the last run's summary belongs to that run"
        );

        tree.finish(AgentId::ROOT, Some("second result".to_string()));
        assert_eq!(
            tree.node(AgentId::ROOT).unwrap().summary.as_deref(),
            Some("second result")
        );

        // Even an empty reply replaces it: nothing about the previous run
        // survives into the row.
        tree.begin(AgentId::ROOT, None);
        tree.finish(AgentId::ROOT, None);
        assert_eq!(tree.node(AgentId::ROOT).unwrap().summary, None);
    }

    /// A nudge that cannot be delivered puts the row back exactly as it was,
    /// instead of leaving a `thinking…` on an agent nothing is running
    /// (finding B10).
    #[test]
    fn a_nudge_that_cannot_be_delivered_restores_the_previous_phase() {
        let mut tree = AgentTree::bare();
        let (opened, _rx) = child(&mut tree, 1);
        let id = opened.id;
        tree.finish(id, Some("did the work".to_string()));
        // The actor is gone: its mailbox went with it, so the nudge cannot land.
        tree.agent_tx.remove(&id);

        let was = tree.nudge(id);
        assert_eq!(
            tree.node(id).unwrap().phase,
            Phase::Thinking,
            "a nudge on its way shows immediately"
        );

        tree.nudge_failed(id, was);
        let node = tree.node(id).unwrap();
        assert_eq!(node.phase, Phase::Done, "the ✓ is not rewritten");
        assert_eq!(
            node.summary.as_deref(),
            Some("did the work"),
            "nor is what the agent produced"
        );
    }

    /// A leftover worktree holds an id this session never allocated, so the
    /// counter has to be raised above it or the next spawn hands a live child
    /// an id a leftover already holds (finding B1).
    #[test]
    fn a_leftover_id_raises_the_floor_so_a_fresh_child_cannot_collide() {
        let mut tree = AgentTree::bare();
        tree.register(leftover(7));

        let node = tree.node(AgentId(7)).expect("the leftover is registered");
        assert!(node.leftover);
        assert_eq!(node.phase, Phase::Done, "the commit says how it ended");
        assert_eq!(node.summary.as_deref(), Some("found on startup"));

        assert!(
            tree.handles().ids.agents_floor() < 8,
            "registering a node does not move the counter"
        );
        tree.reserve_agents(8);
        assert!(tree.handles().ids.agents_floor() >= 8);
        assert_ne!(
            tree.handles().ids.next_agent(),
            AgentId(7),
            "the next spawn must not reuse the leftover's id"
        );
    }

    /// Reaping a node takes the focus and the cursor off it, and everything
    /// keyed by its id with it (finding B11).
    #[test]
    fn reaping_never_leaves_the_focus_or_the_cursor_on_a_removed_node() {
        let mut tree = AgentTree::bare();
        let (kept, _kept_rx) = child(&mut tree, 1);
        let (gone, _gone_rx) = child(&mut tree, 2);
        let (kept, gone) = (kept.id, gone.id);
        tree.agent_stats.insert(gone, git::Stat::default());
        tree.focus(gone);
        tree.cursor_bottom();

        tree.reap(&[gone]);

        assert!(!tree.has(gone), "the node is gone");
        assert!(tree.has(kept), "and only that node");
        assert_eq!(
            tree.focused,
            AgentId::ROOT,
            "the focus cannot stay on a ghost"
        );
        assert_eq!(tree.cursor(), tree.agents.len().saturating_sub(1));
        assert!(!tree.agent_tx.contains_key(&gone));
        assert!(!tree.agent_stats.contains_key(&gone));
    }

    /// A finished child of the root: at rest, with a result its parent has read
    /// (which is the state the history window is about), and a live mailbox, so
    /// a test can see the `Shutdown` its reap sends.
    fn finished(tree: &mut AgentTree, id: u64) -> Receiver<AgentMsg> {
        let rx = spawn(tree, id, 0, 1);
        tree.finish(AgentId(id), Some(format!("did {id}")));
        // `finish` arms the `✉` mark — its parent has not read it *yet* — and
        // the read is what takes it off.
        tree.result_read(AgentId(id));
        rx
    }

    /// Fifty-one finished children keep fifty: the window is a prefix of the
    /// *oldest* ones, so the survivors are the newest fifty, contiguous.
    #[test]
    fn the_history_window_forgets_from_the_oldest_end() {
        let mut tree = AgentTree::bare();
        let _mailboxes: Vec<_> = (1..=51).map(|id| finished(&mut tree, id)).collect();

        assert_eq!(
            tree.past_history(),
            vec![AgentId(1)],
            "one child over fifty, and it is the first one"
        );

        // Fifty exactly is not over the window: nothing to forget.
        tree.reap(&[AgentId(1)]);
        assert!(tree.past_history().is_empty());
    }

    /// A result its parent has not read is the `✉` protocol, and the child
    /// wearing it is never a candidate — it does not even spend one of the
    /// fifty slots, so it can never push a droppable child out of the window.
    #[test]
    fn the_window_keeps_a_child_whose_result_is_unread() {
        let mut tree = AgentTree::bare();
        let _mailboxes: Vec<_> = (1..=52).map(|id| finished(&mut tree, id)).collect();
        // #1's last run produced something its parent has not been handed.
        tree.finish(AgentId(1), Some("did 1".to_string()));
        assert!(tree.node(AgentId(1)).unwrap().result_unread);

        let gone = tree.past_history();

        assert!(
            !gone.contains(&AgentId(1)),
            "the oldest child is the one that is owed a read: {gone:?}"
        );
        assert_eq!(
            gone,
            vec![AgentId(2)],
            "and the row beyond the window's fifty eligible ones is the one that goes"
        );
    }

    /// Work somewhere below a node — at any depth — is the completion that node
    /// exists to fold in, so the window keeps it however old it is.
    #[test]
    fn the_window_keeps_a_child_with_work_below_it() {
        let mut tree = AgentTree::bare();
        let _mailboxes: Vec<_> = (1..=52).map(|id| finished(&mut tree, id)).collect();
        // #1's own child is still working: #1 is napping, and its transcript is
        // where #100's completion will fold in.
        let _grandchild = spawn(&mut tree, 100, 1, 2);

        let gone = tree.past_history();

        assert!(
            !gone.contains(&AgentId(1)),
            "a napping parent is the reader of the work below it: {gone:?}"
        );
        assert_eq!(gone, vec![AgentId(2)]);
    }

    /// The rule above, generalized: *nothing* the window must keep may hang
    /// under a row it forgets — not just work in flight. A descendant whose
    /// result its parent has not read is news with one reader, and that reader is
    /// the ancestor's transcript: drop the ancestor and the report has nowhere
    /// left to fold.
    #[test]
    fn the_window_keeps_every_ancestor_of_a_child_it_must_keep() {
        let mut tree = AgentTree::bare();
        // #1 is a finished, read child — eligible on its own — and #100 hangs
        // under it with a result nobody has read yet.
        let _first = finished(&mut tree, 1);
        let _child = spawn(&mut tree, 100, 1, 2);
        tree.finish(AgentId(100), Some("did 100".to_string()));
        // Fifty-two more, so the window is two over even with the rows it must
        // keep: two rows go, and neither is #1.
        let _mailboxes: Vec<_> = (2..=53).map(|id| finished(&mut tree, id)).collect();

        let gone = tree.past_history();

        assert_eq!(
            gone,
            vec![AgentId(2), AgentId(3)],
            "the window went past #1, whose child is still wearing `✉`"
        );

        // Read the child's result and it becomes history like any other: its
        // ancestor's row goes on the next pass, with nothing left orphaned.
        tree.result_read(AgentId(100));
        assert!(
            tree.past_history().contains(&AgentId(1)),
            "a read result is the ancestor's to forget: {:?}",
            tree.past_history()
        );
    }

    /// An isolated child whose work nobody has landed is the row that names
    /// `mush/<id>`: dropping it would lose the only handle a human has on a
    /// branch that may be unmerged. It is the conservative exemption, and it
    /// costs the window nothing but the slot it does not take.
    #[test]
    fn the_window_keeps_an_isolated_child_whose_work_is_not_landed() {
        let mut tree = AgentTree::bare();
        let (tx, _rx) = crossbeam_channel::unbounded::<AgentMsg>();
        tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "port the parser".to_string(),
            depth: 1,
            branch: Some("mush/1".to_string()),
            cmd: tx,
        });
        tree.finish(AgentId(1), Some("did it".to_string()));
        tree.result_read(AgentId(1));
        let _mailboxes: Vec<_> = (2..=52).map(|id| finished(&mut tree, id)).collect();

        let gone = tree.past_history();

        assert!(
            !gone.contains(&AgentId(1)),
            "the row is the only thing that names mush/1: {gone:?}"
        );
        assert_eq!(gone, vec![AgentId(2)]);

        // Landing it is what makes the row droppable like any other: the
        // exemption is `branch` without `landed`, and nothing else.
        tree.node_mut(AgentId(1)).unwrap().landed = Some(Landed::Merged);
        assert!(tree.past_history().contains(&AgentId(1)));
    }

    /// The thread window is a promise about the *newest* children — the ones a
    /// human has just been watching — and an older one past it is parked.
    #[test]
    fn the_parking_window_keeps_the_newest_children_warm() {
        let mut tree = AgentTree::bare();
        let _mailboxes: Vec<_> = (1..=10).map(|id| finished(&mut tree, id)).collect();

        let parked = tree.parkable();

        assert_eq!(
            parked,
            vec![AgentId(1), AgentId(2)],
            "ten children, the newest eight keep their threads"
        );
        assert!(
            parked.iter().all(|id| !(3..=10).any(|n| AgentId(n) == *id)),
            "and only the two oldest are parked: {parked:?}"
        );

        // The pane the human is reading keeps its thread too, however old the
        // child: a phase can be one message stale on the tick this runs, and a
        // park in that moment cancels the run the human's own words just
        // started.
        tree.focus(AgentId(1));
        assert!(
            !tree.parkable().contains(&AgentId(1)),
            "the child the pane is open on is not the window's to reclaim: {:?}",
            tree.parkable()
        );
    }

    /// Parking never cancels work: a run in flight, a parent (whose mailbox is
    /// the channel its children's completions travel on) and a napping agent
    /// over a working child are all left alone, however old they are.
    #[test]
    fn the_parking_window_never_reclaims_a_thread_with_work_to_hear() {
        let mut tree = AgentTree::bare();
        let _mailboxes: Vec<_> = (1..=10).map(|id| finished(&mut tree, id)).collect();
        // #1 has a child that is still working: its completion is on its way to
        // #1's mailbox, so #1 is not a candidate and neither is its own child.
        let _grandchild = spawn(&mut tree, 100, 1, 2);
        // #2 has a child of its own, at rest and read: it is a reader too.
        let _child = spawn(&mut tree, 101, 2, 2);
        tree.finish(AgentId(101), Some("did 101".to_string()));
        tree.result_read(AgentId(101));
        // #3 is working: a `Shutdown` would cancel the run it is in.
        tree.begin(AgentId(3), None);

        let parked = tree.parkable();

        assert!(
            !parked.contains(&AgentId(1)),
            "a napping parent is where a completion folds in: {parked:?}"
        );
        assert!(
            !parked.contains(&AgentId(2)),
            "a parent is the channel its children report on: {parked:?}"
        );
        assert!(
            !parked.contains(&AgentId(3)),
            "a run in flight is not work to reclaim: {parked:?}"
        );
    }

    /// A child that still owns a running job is work in flight, not history:
    /// both windows end a thread with `Shutdown`, and `Shutdown` kills the jobs
    /// its owner started (`agent::absorb`'s arm, the registry's `kill_owned`) —
    /// so a `⚙` a human is still waiting on is the one thing neither may take.
    #[test]
    fn neither_window_touches_a_child_that_still_owns_a_job() {
        use std::path::Path;

        use crate::machine::fake::{Script, Scripted};
        use crate::machine::{Machine, ShellCommand};

        let mut tree = AgentTree::bare();
        // Fifty-two, so the window is one over even *with* the child it must
        // keep: the row it goes on to drop is what says #1 was passed over.
        let _mailboxes: Vec<_> = (1..=52).map(|id| finished(&mut tree, id)).collect();
        let machine = Arc::new(Scripted::new().runs(Script::hangs()));
        let (tx, _rx) = crossbeam_channel::unbounded();
        tree.jobs
            .launch(jobs::Launch::started(
                1,
                "cargo build".to_string(),
                false,
                tx,
                machine
                    .spawn(&ShellCommand {
                        command: "cargo build",
                        root: Path::new("/tmp"),
                    })
                    .unwrap(),
            ))
            .unwrap();

        assert_eq!(
            tree.past_history(),
            vec![AgentId(2)],
            "the oldest row is skipped for the one holding work"
        );
        let parked = tree.parkable();
        assert!(
            !parked.contains(&AgentId(1)),
            "its thread is not reclaimed either: {parked:?}"
        );
        assert!(
            parked.contains(&AgentId(2)),
            "while its sibling with no job is a candidate, so the job is what kept #1: {parked:?}"
        );
        tree.jobs.kill_all();
    }

    /// A Stop reaches a live mailbox; a dead one is a gone actor, and the row
    /// has to say so instead of showing work that can never finish
    /// (finding B6) — but what it says is `⚠`, not `⊘`: the human's Ctrl-C found
    /// nobody to ask, and a stop promises an actor that a message resumes
    /// (finding H2).
    #[test]
    fn a_cancel_that_cannot_be_heard_cuts_the_row_off_rather_than_stopping_it() {
        let mut tree = AgentTree::bare();
        let (opened, rx) = child(&mut tree, 1);
        let id = opened.id;

        assert!(!tree.cancel_requested(id), "a live mailbox hears the Stop");
        assert!(matches!(rx.try_recv(), Ok(AgentMsg::Stop)));
        assert_eq!(tree.node(id).unwrap().phase, Phase::Cancelling);

        tree.agent_tx.remove(&id);
        assert!(
            tree.cancel_requested(id),
            "a dead mailbox with a run in flight is not a stop"
        );
        assert_eq!(tree.node(id).unwrap().phase, Phase::CutOff);
    }

    /// A dead mailbox with nothing in flight has nothing to say: the phase the
    /// agent had is the phase it keeps, exactly as for a live one.
    #[test]
    fn a_cancel_that_cannot_be_heard_leaves_an_at_rest_agent_alone() {
        let mut tree = AgentTree::bare();
        let (opened, _rx) = child(&mut tree, 1);
        let id = opened.id;
        tree.stopped(id);
        tree.agent_tx.remove(&id);

        assert!(
            !tree.cancel_requested(id),
            "a dead mailbox with nothing in flight is not a cut-off"
        );
        assert_eq!(tree.node(id).unwrap().phase, Phase::Stopped);
    }

    /// A Stop on an agent with nothing in flight cancels nothing, so it must
    /// not put the row into `⊘ cancelling…`: that mark is `busy`, it made the
    /// bar claim work that was not happening, and it was only cleared ten
    /// seconds later by the stale timer (finding B6). The phase the agent had
    /// is the phase it keeps — a failed run's error is the thing the human
    /// still has to read.
    #[test]
    fn a_stop_on_an_at_rest_agent_leaves_no_cancelling_mark() {
        let mut tree = AgentTree::bare();
        let (opened, _rx) = child(&mut tree, 1);
        let id = opened.id;

        tree.fail(id, "no route".into());
        assert!(!tree.cancel_requested(id), "the mailbox is alive");
        assert_eq!(
            tree.node(id).unwrap().phase,
            Phase::Failed("no route".into()),
            "the failure is not replaced by a cancel that cancelled nothing"
        );
        assert!(!tree.busy());

        tree.idle(id);
        tree.cancel_requested(id);
        assert_eq!(tree.node(id).unwrap().phase, Phase::Idle);
        assert!(!tree.busy(), "and the bar is not claiming work");
        assert!(
            !tree.expire_cancels(),
            "nothing was marked, so there is nothing to retire"
        );

        // A stop the actor really made: it stays `Stopped`, and a second Stop
        // on it cancels no more than the first did.
        tree.stopped(id);
        tree.cancel_requested(id);
        assert_eq!(tree.node(id).unwrap().phase, Phase::Stopped);
        assert!(!tree.busy());
    }

    /// A Stop that lands clears the mark at once: the actor's end-of-run event
    /// is the acknowledgement the tree is waiting for, and it is the only thing
    /// that ends the `⊘` — a timer is the fallback, not the mechanism
    /// (finding B6).
    #[test]
    fn a_stop_that_lands_clears_the_cancelling_mark() {
        let mut tree = AgentTree::bare();
        let (opened, rx) = child(&mut tree, 1);
        let id = opened.id;

        assert!(!tree.cancel_requested(id), "a run is in flight");
        assert_eq!(tree.node(id).unwrap().phase, Phase::Cancelling);
        assert!(tree.busy(), "and the run is still on");
        assert!(matches!(rx.try_recv(), Ok(AgentMsg::Stop)));

        // The actor yields with no result: `Stopped`, not `Idle` and not `Done`.
        tree.stopped(id);
        assert_eq!(tree.node(id).unwrap().phase, Phase::Stopped);
        assert!(!tree.busy(), "the acknowledgement clears it at once");
        assert!(!tree.agent_cancel.contains_key(&id));

        // A run that ends with a result clears it too, and then a Stop on it
        // cannot start a mark again.
        tree.begin(id, None);
        tree.cancel_requested(id);
        assert_eq!(tree.node(id).unwrap().phase, Phase::Cancelling);
        tree.finish(id, Some("did the thing".into()));
        assert_eq!(tree.node(id).unwrap().phase, Phase::Done);
        assert!(!tree.busy());
    }

    /// A cancel the actor never acknowledges goes quiet rather than spinning
    /// forever (finding B6).
    #[test]
    fn a_cancel_that_is_never_acknowledged_goes_quiet() {
        let mut tree = AgentTree::bare();
        let (opened, _rx) = child(&mut tree, 1);
        let id = opened.id;
        tree.cancel_requested(id);

        tree.age(id, Duration::from_secs(11));
        assert!(tree.expire_cancels(), "the stale mark is retired");
        assert_eq!(tree.node(id).unwrap().phase, Phase::Idle);
        assert!(!tree.busy());
        assert!(
            !tree.expire_cancels(),
            "and there is nothing left to retire"
        );
    }

    /// The cursor is a row index into the tree, so it cannot leave it.
    #[test]
    fn the_cursor_stays_inside_the_tree() {
        let mut tree = AgentTree::bare();
        child(&mut tree, 1);
        let (opened, _rx) = child(&mut tree, 2);
        let id = opened.id;

        tree.move_cursor(1);
        assert_eq!(tree.cursor(), 1);
        tree.move_cursor(1);
        tree.move_cursor(1);
        assert_eq!(tree.cursor(), 2, "the cursor stops at the last row");

        tree.move_cursor(-1);
        assert_eq!(tree.cursor(), 1);
        tree.cursor_top();
        assert_eq!(tree.cursor(), 0);
        tree.move_cursor(-1);
        assert_eq!(tree.cursor(), 0, "and at the first");

        tree.cursor_bottom();
        assert_eq!(tree.focus_cursor(), Some(id), "Enter focuses the row");
        assert_eq!(tree.focused, id);
    }

    /// The chat cannot show an agent that is not in the tree.
    #[test]
    fn focusing_an_agent_that_is_gone_is_refused() {
        let mut tree = AgentTree::bare();
        assert!(!tree.focus(AgentId(7)));
        assert_eq!(tree.focused, AgentId::ROOT);
        assert_eq!(
            tree.focus_cursor(),
            Some(AgentId::ROOT),
            "the root is always a row, so the cursor is never empty"
        );
    }

    /// The pane title's `N jobs` and a row's `⚙N` count one thing: a **job**,
    /// the command that outlived its tool call. A command a tool call is still
    /// holding is not one — it has no id, no line and no window to show
    /// (finding S4) — so the title may not count it and no row may wear it.
    ///
    /// The title used to sum the foreground calls in (`Registry::live_total`),
    /// whose own doc claimed the sum was what a row counts: a held command made
    /// a tree with no job in it read `1 job` above rows with no `⚙`.
    #[test]
    fn the_title_and_the_rows_count_the_same_thing() {
        use std::path::Path;

        use crate::machine::fake::{Script, Scripted};
        use crate::machine::{Machine, ShellCommand};

        let build = || ShellCommand {
            command: "cargo build",
            root: Path::new("/tmp"),
        };
        let tree = AgentTree::bare();

        // A command under a tool call: the model is waiting for its result.
        let machine = Arc::new(Scripted::new().runs(Script::hangs()));
        let held = tree
            .jobs
            .hold(AgentId::ROOT.0, machine.spawn(&build()).unwrap());
        assert_eq!(
            tree.live_job_count(),
            0,
            "a held command is not a job, so the title says none"
        );
        assert!(
            tree.live_jobs(AgentId::ROOT).is_empty(),
            "and its owner's row wears no ⚙"
        );
        drop(held);

        // The same command, outliving its tool call: a job, on both surfaces.
        let machine = Arc::new(Scripted::new().runs(Script::hangs()));
        let (tx, _rx) = crossbeam_channel::unbounded();
        tree.jobs
            .launch(jobs::Launch::started(
                AgentId::ROOT.0,
                "cargo build".to_string(),
                false,
                tx,
                machine.spawn(&build()).unwrap(),
            ))
            .unwrap();
        assert_eq!(tree.live_job_count(), 1, "the title counts the job");
        assert_eq!(
            tree.live_jobs(AgentId::ROOT).len(),
            1,
            "and its owner's row wears ⚙1"
        );
        tree.jobs.kill_all();
    }
}
