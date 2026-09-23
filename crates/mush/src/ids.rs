//! The two id spaces of one conversation, and the one place either is drawn.
//!
//! A conversation names two kinds of thing: agents (`#1`, `#2`) and the jobs
//! those agents start (`#c1`, `#c2`). One atomic used to serve both, indexed
//! from `spawn_agent` and `Registry::launch`, so a job id and an agent id were
//! one number in one space and nothing but care kept `control`'s two targets
//! apart. Two newtypes with two `Display`s make the mistake a type error: a
//! [`JobId`] cannot stand where an [`AgentId`] is expected, and the spelling of
//! each ("#7" against "#c7") lives in exactly one place.
//!
//! The invariant both counters obey: **an id is reusable only if no worktree
//! and no stored record was ever created for it.** A number names a `mush/<id>`
//! branch and a `.mush/wt/<id>` checkout and is written into a stored session;
//! handing it out again while any of those exists puts two agents on one branch
//! or one row (finding B1). So a spawn that fails *before* git could create
//! anything gives its number back ([`Ids::lose_agent`]), one that got as far as
//! a worktree does not, and discovery raises the floor for every number git's
//! worktree list still names, checkout or not ([`Ids::reserve_agents`]).
//!
//! Both counters start at 1: agent 0 is the root, and the job space is its own,
//! so `#1` and `#c1` may be on screen together without either claiming the
//! other's work.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use mush_core::git::MAX_AGENT_ID;

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
    /// `#7` — the one spelling of an agent's name. Callers write `{id}` and not
    /// `#{id}`, so the `#` cannot be doubled or dropped at one site of many.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Which job, in the registry.
///
/// A job is a command that outlived its tool call, and its name has to read as
/// a job and not as an agent: `#c7` is what `status` prints and what the model
/// copies back into `control`. The `c` is the whole difference, so it is part
/// of the type's own `Display` rather than a prefix each caller remembers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

impl fmt::Display for JobId {
    /// `#c7` — a command's id, told from `#7`, an agent's, at a glance.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#c{}", self.0)
    }
}

/// How many failed-spawn numbers are kept for reuse. A failed spawn is a rare
/// event; the pool only exists so the common case — one bad `base`, one retry —
/// does not leave a permanent hole in the numbering. When a ninth loss arrives
/// the oldest entry is the one forgotten, and it simply stays a gap, which
/// costs nothing: gaps are what the counter is for. The newest loss is kept
/// because [`Ids::next_agent`] pops the newest: the retry that follows a
/// failure is the draw that asks for the number just lost, and dropping the
/// incoming id instead would leave that retry the one number it cannot get.
const LOST_POOL: usize = 8;

/// The two counters of one conversation, shared by every actor in its tree and
/// by the UI's own handles.
///
/// `Clone` is the sharing: the actors, the tree, the registry and the root
/// handle each hold a copy of the same pair of counters, because there is one
/// conversation and therefore one answer to "which id is next".
#[derive(Clone)]
pub struct Ids {
    /// The agent numbers: the counter and the numbers handed back, under one
    /// lock, because drawing one and retiring one are one question — see
    /// [`Ids::next_agent`].
    agents: Arc<Mutex<Agents>>,
    /// The next job number. Jobs are never handed back: a launch that got as
    /// far as an id has a record and possibly a process group behind it.
    jobs: Arc<AtomicU64>,
}

/// The agent space: the next number, and the numbers a failed spawn gave back.
///
/// This used to be an atomic beside a locked pool, so a draw read the counter
/// and the pool under two locks while [`Ids::reserve_agents`] retired numbers
/// against the same counter from the other side: a number the repository had
/// just named could be handed out of the pool in the window between the two
/// (finding B1's invariant, one lock late). One lock, one answer.
#[derive(Default)]
struct Agents {
    /// The next agent number if none was handed back.
    counter: u64,
    /// Numbers a failed spawn gave back, newest last and never more than
    /// [`LOST_POOL`] of them: see [`Ids::lose_agent`].
    lost: Vec<u64>,
}

/// The counters start at 1, not at 0: agent 0 is the root, and a first child
/// numbered 0 would collide with it on every id-keyed lookup in the tree.
///
/// (This is written by hand rather than derived: `#[derive(Default)]` would
/// leave both atomics at 0, and the first spawned child would take the root's
/// id.)
impl Default for Ids {
    fn default() -> Self {
        Self {
            agents: Arc::new(Mutex::new(Agents {
                counter: 1,
                lost: Vec::new(),
            })),
            jobs: Arc::new(AtomicU64::new(1)),
        }
    }
}

impl Ids {
    /// The next agent id: a number a failed spawn gave back, or a fresh one —
    /// or `None` when the id space has no id left to draw.
    ///
    /// The pool is popped first so a retry after a refusal is consecutive —
    /// nothing was created under the lost number, so nothing can collide with
    /// it. Everything in the pool is below the counter by construction
    /// ([`Ids::lose_agent`] pushes nothing else and the counter only grows), and
    /// a number the repository has named since is retired by
    /// [`Ids::reserve_agents`] rather than handed out: draw and retire take the
    /// same lock, so neither can happen inside the other. That is also why the
    /// pool is asked before the space is judged spent: a lost number is one the
    /// counter has already passed, so it is inside the id space even while the
    /// counter itself stands above it.
    ///
    /// `None` is the spent id space, and it is the rule rather than a failure:
    /// the counter stands above [`MAX_AGENT_ID`], where a fresh draw would hand
    /// out an id no `mush/<id>` branch can be read back from
    /// ([`mush_core::git::worktree_id`]) and whose own floor, one past it, has no
    /// room for the draw after that — `+ 1` at a floor of `u64::MAX` overflows,
    /// which is an `attempt to add with overflow` panic in a debug build and a
    /// wrap onto the root's own id `0` in release (finding IN2). A counter that
    /// can never draw again is the honest answer to a repository that named
    /// every id: [`Ids::reserve_agents`] puts it there, the caller refuses the
    /// spawn and says so.
    pub fn next_agent(&self) -> Option<AgentId> {
        let mut agents = self.agents();
        if let Some(id) = agents.lost.pop() {
            return Some(AgentId(id));
        }
        if agents.counter > MAX_AGENT_ID {
            return None;
        }
        let id = agents.counter;
        agents.counter += 1;
        Some(AgentId(id))
    }

    /// Give a number back: the spawn it was drawn for failed before git could
    /// create a worktree, a branch or a record.
    ///
    /// This is the *whole* licence for reuse, and it stops at git: once
    /// [`mush_core::git::worktree_add`] has returned, a `mush/<id>` branch or a
    /// `.mush/wt/<id>` directory may exist, and the number must stay spent even
    /// if the spawn fails afterwards (the worktree would be orphaned and the
    /// branch would make the next `add -b` fail — the very residue
    /// [`Ids::reserve_agents`] raises the floor for).
    ///
    /// The pool is bounded at [`LOST_POOL`] and keeps the newest losses: when
    /// it is full the oldest entry is dropped to make room for the incoming
    /// id. The retry the pool exists for follows the failure that just
    /// happened, and that is the newest entry, so the oldest is the one to
    /// forget; it stays a gap rather than being handed out again.
    pub fn lose_agent(&self, id: AgentId) {
        let mut agents = self.agents();
        if id.0 >= agents.counter {
            return;
        }
        if agents.lost.len() >= LOST_POOL {
            agents.lost.remove(0);
        }
        agents.lost.push(id.0);
    }

    /// Keep the agent counter above `floor`.
    ///
    /// Every id the repository already names — a leftover worktree, a restored
    /// session, a `mush/<id>` branch whose checkout is gone — has to be below
    /// the next spawn, or two nodes share an id and every id-keyed lookup hits
    /// the wrong one (finding B1). Numbers still in the lost pool that fall
    /// below the new floor are dropped: they are numbers with no record *here*,
    /// but the repository has since named them, which is the invariant's line.
    ///
    /// A floor of [`MAX_AGENT_ID`] is still a drawable id; one above it is the
    /// spent space — the counter stands past the last id the repository could
    /// name, and [`Ids::next_agent`] then answers `None` instead of a number no
    /// `mush/<id>` branch can be read back from.
    pub fn reserve_agents(&self, floor: u64) {
        let mut agents = self.agents();
        agents.counter = agents.counter.max(floor);
        agents.lost.retain(|id| *id >= floor);
    }

    /// The number the next fresh agent draw would take, without taking it.
    ///
    /// The counter, not the next id: a lost number below it may still be handed
    /// out first. This is what a test asserts a floor against — reading must
    /// not spend a number — so it exists only under `cfg(test)`.
    #[cfg(test)]
    pub fn agents_floor(&self) -> u64 {
        self.agents().counter
    }

    /// The next job id. One counter for the machine's jobs, never a pool: a job
    /// id reaches `status`, the screen and a transcript line the moment it is
    /// drawn, so it counts as a record even when the launch is refused.
    pub fn next_job(&self) -> JobId {
        JobId(self.jobs.fetch_add(1, Ordering::SeqCst))
    }

    /// Keep the job counter above `floor`.
    ///
    /// The job space has no pool, but it does have a *record* beyond the
    /// process: a job's name is written into its owner's transcript
    /// (`#c2 done: …`), and that transcript outlives the run. Every process
    /// starts this counter at 1, so a conversation restored over a transcript
    /// that names `#c7` would hand the next launch's first job `#c1` — and a
    /// `control stop #c1` the model reads out of the restored conversation
    /// would address a command the id never named (finding A22). The floor is
    /// read from the names the copy carries (see `agent::raise_job_floor`),
    /// because the books are fresh after a restore: the transcript is the only
    /// surviving record.
    ///
    /// `fetch_max`, so a stale floor can never lower a counter a live tree has
    /// already moved.
    pub fn reserve_jobs(&self, floor: u64) {
        self.jobs.fetch_max(floor, Ordering::SeqCst);
    }

    /// The agent space — the counter and the pool, one lock. A lock poisoned by
    /// a panic elsewhere is taken as it is: the house shape for a lock whose
    /// failure must not be fatal, because the records are not corrupted by
    /// someone else's panic, and the alternatives are a panic or a leaked id.
    fn agents(&self) -> MutexGuard<'_, Agents> {
        self.agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first child takes 1, not the root's 0: the derivation is where the
    /// two counters start, and a derive would start the first child at the
    /// root's id.
    #[test]
    fn the_counters_start_above_the_root_and_apart_from_each_other() {
        let ids = Ids::default();
        assert_eq!(ids.next_agent(), Some(AgentId(1)));
        assert_eq!(ids.next_agent(), Some(AgentId(2)));
        assert_eq!(ids.next_job(), JobId(1), "the job space starts at 1 too");
        assert_eq!(ids.next_job(), JobId(2));
        assert_eq!(
            ids.next_agent(),
            Some(AgentId(3)),
            "drawing jobs does not move the agent counter"
        );
    }

    /// A number given back is handed straight out again, so a refused spawn
    /// leaves no hole; a number that was never lost is not.
    #[test]
    fn a_lost_number_is_the_next_one_handed_out() {
        let ids = Ids::default();
        let burned = ids.next_agent().expect("the space is fresh");
        assert_eq!(burned, AgentId(1));
        ids.lose_agent(burned);
        assert_eq!(
            ids.next_agent(),
            Some(AgentId(1)),
            "the retry is consecutive"
        );
        assert_eq!(ids.next_agent(), Some(AgentId(2)));
    }

    /// The pool full, the oldest forgotten, the newest drawn first: nine
    /// losses put #1 permanently out of reach — the gap — while #9, the newest
    /// and so the number a retry actually draws, is the first one handed back.
    #[test]
    fn a_pool_full_names_which_lost_number_is_the_gap() {
        let ids = Ids::default();
        for n in 1..=9 {
            assert_eq!(ids.next_agent(), Some(AgentId(n)), "draw #{n}");
        }
        for n in 1..=9 {
            ids.lose_agent(AgentId(n));
        }
        let drawn: Vec<AgentId> = (0..9)
            .map(|_| ids.next_agent().expect("the space is fresh"))
            .collect();
        let expected: Vec<AgentId> = [9, 8, 7, 6, 5, 4, 3, 2, 10]
            .into_iter()
            .map(AgentId)
            .collect();
        assert_eq!(
            drawn, expected,
            "#1 is the gap, #9 comes first, then down to #2, then a fresh #10"
        );
    }

    /// The floor is a floor: it never goes down, and it retires any lost number
    /// underneath it (the repository has named that number since).
    #[test]
    fn a_floor_retires_lost_numbers_below_it() {
        let ids = Ids::default();
        let given = ids.next_agent().expect("the space is fresh");
        ids.lose_agent(given);
        ids.reserve_agents(9);
        assert_eq!(ids.agents_floor(), 9);
        assert_eq!(
            ids.next_agent(),
            Some(AgentId(9)),
            "the lost #1 is not reused"
        );
        ids.reserve_agents(3);
        assert_eq!(
            ids.agents_floor(),
            10,
            "a lower floor never lowers the counter (the draw above moved it)"
        );

        // A lost number the floor does not reach is still the next one out: the
        // retire and the draw are one lock apart, not two.
        let ids = Ids::default();
        for _ in 0..5 {
            let _ = ids.next_agent();
        }
        ids.lose_agent(AgentId(5));
        ids.reserve_agents(4);
        assert_eq!(ids.agents_floor(), 6);
        assert_eq!(
            ids.next_agent(),
            Some(AgentId(5)),
            "the pool is still a pool"
        );
    }

    /// The id space ends at [`MAX_AGENT_ID`]: a floor there is the last id the
    /// counter can hand out and still count from, and a floor above it is the
    /// spent space — `None`, not a panic and not a name no `mush/<id>` branch
    /// can be read back from (finding IN2). `Ids::default()` with a floor of
    /// `u64::MAX - 1` drew `#18446744073709551614`, left the counter at
    /// `u64::MAX`, and the next draw's `+ 1` was `attempt to add with overflow`.
    #[test]
    fn a_draw_with_no_room_above_its_id_answers_none() {
        let ids = Ids::default();
        ids.reserve_agents(MAX_AGENT_ID);
        assert_eq!(
            ids.next_agent(),
            Some(AgentId(MAX_AGENT_ID)),
            "the last holdable id is still drawn"
        );
        assert_eq!(ids.next_agent(), None, "and the id space above it is spent");

        let ids = Ids::default();
        ids.reserve_agents(MAX_AGENT_ID + 1);
        assert_eq!(
            ids.next_agent(),
            None,
            "a floor past the last holdable id leaves nothing to draw"
        );
        assert_eq!(ids.agents_floor(), MAX_AGENT_ID + 1, "without panicking");
    }

    /// The job floor is a floor too: the floor is the next number out, and a
    /// stale (lower) one never lowers the counter a live tree has moved — the
    /// rule a restored conversation's `#cN` names depend on (finding A22).
    #[test]
    fn the_job_counter_respects_a_floor_and_never_lowers_one() {
        let ids = Ids::default();
        ids.reserve_jobs(8);
        assert_eq!(ids.next_job(), JobId(8), "a floor is the next number out");
        ids.reserve_jobs(3);
        assert_eq!(
            ids.next_job(),
            JobId(9),
            "a lower floor never lowers the counter"
        );
    }
}
