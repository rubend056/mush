//! The agent tree: the one owner of agents, ids, phases and focus.
//!
//! Every fact about an agent's life lives here — which ids exist, what each one
//! is doing now, what it produced, which mailbox steers it, where the focus and
//! the cursor are. "There is a node with id N" and "there is a mailbox, cancel
//! flag and stat for N" therefore cannot disagree, and a phase only ever moves
//! through a transition this module defines: `app::mod` routes events into
//! these methods, it never writes a node's fields itself. What each agent has
//! said is not here: that is the conversation, and it lives in [`super::chat`].

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use mush_core::git;
use mush_core::message::Message;

use crate::agent::{AgentMsg, RootHandle, TreeHandles};
use crate::jobs::{self, JobView};

/// Which agent, in the tree.
///
/// Ids used to be bare `u64`s, shared with branch names (`mush/7`) and with the
/// conversation tag, so `Msg::Agent` took two indistinguishable numbers and
/// `discover_worktrees` could hand a fresh child an id a leftover already held
/// (finding B1). With a newtype the two cannot be swapped by accident.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentId(pub u64);

impl AgentId {
    /// The root agent: the one whose transcript is the chat.
    pub const ROOT: AgentId = AgentId(0);
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Which conversation an agent tree belongs to. One per `/new`, so an event
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

/// Where an isolated agent's work ended up, once the human landed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landed {
    /// Merged into the main branch; the worktree and the branch were reclaimed.
    Merged,
    /// Thrown away on purpose; the worktree and the branch were reclaimed.
    Discarded,
}

/// One entry in the agent tree. Order in the vector is tree order; ids are
/// stable, so positions do not shift while agents are alive.
pub struct AgentNode {
    pub id: AgentId,
    pub parent: Option<AgentId>,
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
    pub phase: Phase,
    pub branch: Option<String>,
    pub summary: Option<String>,
    pub leftover: bool,
    pub landed: Option<Landed>,
    pub tx: Option<Sender<AgentMsg>>,
}

/// A cancel is acknowledged quickly — the actor yields, or the run ends. A `⊘`
/// older than this means the acknowledgement will never arrive (the actor's
/// mailbox is dead), and a row that spins forever is worse than an idle one
/// (finding B6).
const STALE_CANCEL: Duration = Duration::from_secs(10);

/// The agents of one conversation, and everything keyed by their ids.
pub struct AgentTree {
    /// Tree order: the root first, then children as they were spawned.
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
    /// The tree's id counter, shared with the actors. Leftover worktrees are
    /// registered under their own ids, so the next spawn must start above them
    /// or two nodes share an id and every id-keyed lookup hits the wrong one
    /// (finding B1).
    ids: Arc<AtomicU64>,
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
            Arc::new(AtomicU64::new(1)),
            Arc::new(AtomicU64::new(0)),
            jobs::Registry::bare(),
            tx,
        )
    }

    fn with_root(
        conversation: ConversationId,
        ids: Arc<AtomicU64>,
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
            landed: None,
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

    /// Keep the id counter above `floor`. A leftover worktree or a restored
    /// agent holds an id the counter has never seen, so the next spawn has to
    /// start above it or two nodes share one (finding B1).
    pub fn reserve_ids(&mut self, floor: u64) {
        self.ids.fetch_max(floor, Ordering::SeqCst);
    }

    /// A child actor now exists. It is thinking (its parent just started it),
    /// it has no summary of its own yet, and its brief opens its transcript:
    /// the model sees the brief, so the human should too (finding B13). The
    /// opening line is handed back rather than stored: transcripts belong to
    /// the conversation, not to the tree.
    pub fn insert(&mut self, spawn: Spawn) -> Opened {
        let opening = if spawn.brief.trim().is_empty() {
            "Begin the task now.".to_string()
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
            phase: node.phase,
            since: Instant::now(),
            branch: node.branch,
            summary: node.summary,
            leftover: node.leftover,
            landed: node.landed,
        });
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
        }
    }

    /// What the agent says it is doing now. Only a run in flight can report
    /// activity: a status that arrives after the run's own end (a late or
    /// duplicated commit line) is dropped, so a finished agent is never put
    /// back to work (finding B5).
    pub fn activity(&mut self, id: AgentId, label: impl Into<String>) {
        if !self.is_busy(id) {
            return;
        }
        if let Some(node) = self.node_mut(id) {
            node.phase = Phase::Activity(label.into());
            node.since = Instant::now();
        }
    }

    /// The run finished: `summary` is what it produced, and it always replaces
    /// the previous one — keeping the first summary described a run that ended
    /// long ago (finding B14). An empty reply replaces it with nothing.
    pub fn finish(&mut self, id: AgentId, summary: Option<String>) {
        if let Some(node) = self.node_mut(id) {
            node.phase = Phase::Done;
            node.since = Instant::now();
            node.summary = summary;
        }
        self.agent_cancel.remove(&id);
    }

    /// The run failed: the error is what the human has to read, and it is the
    /// phase until a later run replaces it.
    pub fn fail(&mut self, id: AgentId, error: String) {
        if let Some(node) = self.node_mut(id) {
            node.phase = Phase::Failed(error);
            node.since = Instant::now();
        }
        self.agent_cancel.remove(&id);
    }

    /// A Stop landed: the run produced nothing, and the actor is alive and
    /// resumable. Its own phase, not `Idle` — that a run was interrupted
    /// mid-flight is the fact the human needs.
    pub fn stopped(&mut self, id: AgentId) {
        if let Some(node) = self.node_mut(id) {
            node.phase = Phase::Stopped;
            node.since = Instant::now();
        }
        self.agent_cancel.remove(&id);
    }

    /// Put a node back at rest: an agent whose mailbox is gone is not running,
    /// whatever its row said.
    pub fn idle(&mut self, id: AgentId) {
        if let Some(node) = self.node_mut(id) {
            node.phase = Phase::Idle;
            node.since = Instant::now();
        }
    }

    /// Record that `/merge` or `/discard` reclaimed this agent's worktree.
    pub fn land(&mut self, id: AgentId, landed: Landed) {
        if let Some(node) = self.node_mut(id) {
            node.landed = Some(landed);
        }
    }

    /// A Stop was asked for: flip the flag the in-flight model call polls and
    /// leave a Stop in the mailbox for everything else (a parked wait, a shell
    /// command, the next message boundary). Returns whether the actor was
    /// still there to hear it — a cancel that cannot land is a gone actor, and
    /// the row says so instead of showing work that can never finish
    /// (finding B6).
    ///
    /// Only a run *in flight* is marked `⊘ cancelling…`. An agent that is
    /// idle, done, failed or already stopped has nothing to cancel: the actor
    /// absorbs the Stop and says nothing, so the mark and the `busy` flag that
    /// comes with it would outlive the act they describe — a row claiming work
    /// that will never happen, until a ten-second timer quietly cleared it
    /// (finding B6). What the actor *does* answer is a Stop on a run it really
    /// has: the run ends and its end-of-run event clears the mark at once.
    pub fn cancel_requested(&mut self, id: AgentId) -> bool {
        if let Some(flag) = self.agent_cancel.get(&id) {
            flag.store(true, Ordering::SeqCst);
        }
        let heard = self
            .agent_tx
            .get(&id)
            .map(|tx| tx.send(AgentMsg::Stop).is_ok())
            .unwrap_or(false);
        if let Some(node) = self.node_mut(id) {
            if node.phase.is_busy() {
                // Say so immediately: the actor may be mid-request, and a row
                // that keeps spinning looks like the Stop was never heard.
                node.phase = if heard {
                    Phase::Cancelling
                } else {
                    // Nothing will ever answer, so the work it was showing can
                    // never finish.
                    Phase::Stopped
                };
                node.since = Instant::now();
            }
            // Otherwise the phase, and its clock, are left exactly as they
            // were: a Stop is not news about an agent that was not working.
        }
        heard
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
    pub fn nudge(&mut self, id: AgentId) -> Option<Phase> {
        let previous = self.node(id).map(|node| node.phase.clone());
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

    /// Drop nodes that are gone — a leftover whose worktree no longer exists,
    /// an agent the human forgot — and everything keyed by their ids. The focus
    /// and the cursor are put back on a node that is really there: reaping used
    /// to leave them on a ghost, so the pane stayed titled `agent #4` while
    /// typing reported that the agent was gone (finding B11).
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
        let mut roster = Roster::default();
        for node in &self.agents {
            if node.phase.is_busy() {
                roster.working += 1;
            } else if matches!(node.phase, Phase::Idle | Phase::Done)
                && self.busy_children(node.id) > 0
            {
                // At rest with work out: §5.5's napping orchestrator, which the
                // row draws as `⏸`. Counted here and *nowhere else* — counting
                // it as working as well is exactly what the title did wrong.
                // A failed or stopped agent waits for nothing, so it is in no
                // bucket: its own `✗`/`⊘` row is where that fact lives.
                roster.waiting += 1;
            }
        }
        roster
    }

    /// How many of `id`'s own children have work in flight.
    ///
    /// One derivation, read by the row's `⏸N` mark and by the title's count,
    /// because "this agent has children working" is one fact and two copies of
    /// it are two things that can disagree (finding U1). It is about the
    /// children, never about the parent's own phase: a working agent whose
    /// children work is still working.
    pub fn busy_children(&self, id: AgentId) -> usize {
        self.agents
            .iter()
            .filter(|node| node.parent == Some(id) && node.phase.is_busy())
            .count()
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
        let id = self.agents.get(self.agent_cursor)?.id;
        self.focused = id;
        Some(id)
    }

    /// Move the tree cursor one row, without leaving the tree.
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
        self.agent_cursor = self.agents.len().saturating_sub(1);
    }

    /// The row the agent pane paints as selected.
    pub fn cursor(&self) -> usize {
        self.agent_cursor.min(self.agents.len().saturating_sub(1))
    }

    pub fn has(&self, id: AgentId) -> bool {
        self.agents.iter().any(|node| node.id == id)
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

    fn leftover(id: u64) -> Existing {
        Existing {
            id: AgentId(id),
            parent: None,
            depth: 1,
            brief: "leftover worktree".to_string(),
            phase: Phase::Done,
            branch: Some(format!("mush/{id}")),
            summary: Some("found on startup".to_string()),
            leftover: true,
            landed: None,
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
            tree.handles().ids.load(Ordering::SeqCst) < 8,
            "registering a node does not move the counter"
        );
        tree.reserve_ids(8);
        assert!(tree.handles().ids.load(Ordering::SeqCst) >= 8);
        assert_ne!(
            tree.handles().ids.fetch_add(1, Ordering::SeqCst),
            7,
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

    /// A Stop reaches a live mailbox; a dead one is a gone actor, and the row
    /// has to say so instead of showing work that can never finish
    /// (finding B6).
    #[test]
    fn a_cancel_that_cannot_be_heard_marks_the_row_stopped() {
        let mut tree = AgentTree::bare();
        let (opened, rx) = child(&mut tree, 1);
        let id = opened.id;

        assert!(tree.cancel_requested(id), "a live mailbox hears the Stop");
        assert!(matches!(rx.try_recv(), Ok(AgentMsg::Stop)));
        assert_eq!(tree.node(id).unwrap().phase, Phase::Cancelling);

        tree.agent_tx.remove(&id);
        assert!(!tree.cancel_requested(id), "a dead mailbox is a gone actor");
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
        assert!(tree.cancel_requested(id), "the mailbox is alive");
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

        assert!(tree.cancel_requested(id), "a run is in flight");
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
}
