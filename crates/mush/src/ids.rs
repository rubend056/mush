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
use std::sync::{Arc, Mutex};

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
/// does not leave a permanent hole in the numbering. Past this, the oldest lost
/// number is forgotten and simply stays a gap, which costs nothing: gaps are
/// what the counter is for.
const LOST_POOL: usize = 8;

/// The two counters of one conversation, shared by every actor in its tree and
/// by the UI's own handles.
///
/// `Clone` is the sharing: the actors, the tree, the registry and the root
/// handle each hold a copy of the same pair of atomics, because there is one
/// conversation and therefore one answer to "which id is next".
#[derive(Clone)]
pub struct Ids {
    /// The next agent number if none was handed back.
    agents: Arc<AtomicU64>,
    /// The next job number. Jobs are never handed back: a launch that got as
    /// far as an id has a record and possibly a process group behind it.
    jobs: Arc<AtomicU64>,
    /// Numbers a failed spawn gave back, newest last: see [`Ids::lose_agent`].
    /// A `Mutex` rather than a channel because the pool is read exactly once
    /// per allocation and never waited on.
    lost: Arc<Mutex<Vec<u64>>>,
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
            agents: Arc::new(AtomicU64::new(1)),
            jobs: Arc::new(AtomicU64::new(1)),
            lost: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Ids {
    /// The next agent id: a number a failed spawn gave back, or a fresh one.
    ///
    /// The pool is popped first so a retry after a refusal is consecutive —
    /// nothing was created under the lost number, so nothing can collide with
    /// it. A pooled number below the counter's floor (a leftover branch naming
    /// it was found after it was lost) is not ours any more and is dropped
    /// rather than handed out.
    pub fn next_agent(&self) -> AgentId {
        let mut lost = self.lost();
        while let Some(id) = lost.pop() {
            if id < self.agents.load(Ordering::SeqCst) {
                return AgentId(id);
            }
        }
        AgentId(self.agents.fetch_add(1, Ordering::SeqCst))
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
    pub fn lose_agent(&self, id: AgentId) {
        let mut lost = self.lost();
        if id.0 >= self.agents.load(Ordering::SeqCst) || lost.len() >= LOST_POOL {
            return;
        }
        lost.push(id.0);
    }

    /// Keep the agent counter above `floor`.
    ///
    /// Every id the repository already names — a leftover worktree, a restored
    /// session, a `mush/<id>` branch whose checkout is gone — has to be below
    /// the next spawn, or two nodes share an id and every id-keyed lookup hits
    /// the wrong one (finding B1). Numbers still in the lost pool that fall
    /// below the new floor are dropped: they are numbers with no record *here*,
    /// but the repository has since named them, which is the invariant's line.
    pub fn reserve_agents(&self, floor: u64) {
        self.agents.fetch_max(floor, Ordering::SeqCst);
        self.lost().retain(|id| *id >= floor);
    }

    /// The number the next fresh agent draw would take, without taking it.
    ///
    /// The counter, not the next id: a lost number below it may still be handed
    /// out first. This is what a test asserts a floor against — reading must
    /// not spend a number — so it exists only under `cfg(test)`.
    #[cfg(test)]
    pub fn agents_floor(&self) -> u64 {
        self.agents.load(Ordering::SeqCst)
    }

    /// The next job id. One counter for the machine's jobs, never a pool: a job
    /// id reaches `status`, the screen and a transcript line the moment it is
    /// drawn, so it counts as a record even when the launch is refused.
    pub fn next_job(&self) -> JobId {
        JobId(self.jobs.fetch_add(1, Ordering::SeqCst))
    }

    /// The lost-number pool. A lock poisoned by a panic elsewhere is taken as
    /// it is — the house shape for a lock whose failure must not be fatal: the
    /// records are not corrupted by someone else's panic, and the alternative
    /// is a panic or a leaked id.
    fn lost(&self) -> std::sync::MutexGuard<'_, Vec<u64>> {
        self.lost
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
        assert_eq!(ids.next_agent(), AgentId(1));
        assert_eq!(ids.next_agent(), AgentId(2));
        assert_eq!(ids.next_job(), JobId(1), "the job space starts at 1 too");
        assert_eq!(ids.next_job(), JobId(2));
        assert_eq!(
            ids.next_agent(),
            AgentId(3),
            "drawing jobs does not move the agent counter"
        );
    }

    /// A number given back is handed straight out again, so a refused spawn
    /// leaves no hole; a number that was never lost is not.
    #[test]
    fn a_lost_number_is_the_next_one_handed_out() {
        let ids = Ids::default();
        let burned = ids.next_agent();
        assert_eq!(burned, AgentId(1));
        ids.lose_agent(burned);
        assert_eq!(ids.next_agent(), AgentId(1), "the retry is consecutive");
        assert_eq!(ids.next_agent(), AgentId(2));
    }

    /// The floor is a floor: it never goes down, and it retires any lost number
    /// underneath it (the repository has named that number since).
    #[test]
    fn a_floor_retires_lost_numbers_below_it() {
        let ids = Ids::default();
        ids.lose_agent(AgentId(1));
        ids.reserve_agents(9);
        assert_eq!(ids.agents_floor(), 9);
        assert_eq!(ids.next_agent(), AgentId(9), "the lost #1 is not reused");
        ids.reserve_agents(3);
        assert_eq!(
            ids.agents_floor(),
            10,
            "a lower floor never lowers the counter (the draw above moved it)"
        );
    }
}
