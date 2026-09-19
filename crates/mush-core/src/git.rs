//! Reading the repository the way a glance needs it: which branch, how dirty,
//! how many lines. One `git` process per question, cached by the caller — the
//! UI never shells out while painting (docs/mush.md §8).
//!
//! Everything here is best-effort: a workspace that is not a repository, or a
//! `git` binary that is missing, answers `None` rather than failing a caller.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The line delta of some change set. git's counts are 64-bit, so a diff of
/// billions of lines is reported, not truncated to `±0` by a failed parse.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub files: u64,
    pub added: u64,
    pub removed: u64,
}

impl Stat {
    pub fn is_empty(&self) -> bool {
        self.files == 0 && self.added == 0 && self.removed == 0
    }

    /// `+12−3`, or `±0` when nothing changed.
    pub fn compact(&self) -> String {
        if self.is_empty() {
            "±0".to_string()
        } else {
            format!("+{}−{}", self.added, self.removed)
        }
    }
}

/// What the main worktree looks like right now: its branch, how many paths have
/// uncommitted changes, and the uncommitted line delta against `HEAD`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoStatus {
    pub branch: String,
    pub dirty: usize,
    pub stat: Stat,
}

/// One `git` call with the terminal untouched (`output()`, never `status()`:
/// the TUI owns stdout). `None` on any failure, including a missing git.
///
/// `LC_ALL=C` keeps the output parseable: a localized `--shortstat` would not
/// match the English words the parser knows, and would read as `±0`.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The reason `run` gives when the git process could not be started at all.
///
/// It is a constant because [`has_commits`] has to tell "git answered no" from
/// "git never answered", and comparing a bare string literal would make that
/// decision depend on a copy of the text: rename the message in `run` and a
/// machine with no git would report itself as a repository without commits.
pub const GIT_UNAVAILABLE: &str = "git binary unavailable";

/// Run a git command that *changes* the repository and return its trimmed
/// stdout. Unlike [`status`] and friends this can fail for a reason the human
/// needs to read (a merge conflict, a worktree that is still checked out), so
/// the error carries git's own message instead of collapsing to `None`.
pub fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
    run_named(dir, args.first().copied().unwrap_or(""), args)
}

/// [`run`] with the failing command named by the caller. The one failure git
/// itself says nothing more about is a nonzero exit with both streams empty,
/// and there the argv is not enough: `git worktree add`'s first word names a
/// subcommand group, so it would read `git worktree failed` while the human
/// needs the verb that failed. The name is passed in, never matched back out of
/// the error text — a reworded message here would silently retire such a match
/// (see [`GIT_UNAVAILABLE`] for what a copy of a message costs).
///
/// The one invocation style for mutating verbs: `-C` so the caller names the
/// repository, and `LC_ALL=C` so a conflict or error reads the same everywhere.
fn run_named(dir: &Path, name: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .map_err(|_| GIT_UNAVAILABLE.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(if detail.is_empty() {
            format!("git {name} failed")
        } else {
            detail
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The subject line of the commit `name` points at.
///
/// This is how a worktree left behind by an earlier session is identified: the
/// commit mush made for it carries the agent's id, the task it was given, and
/// how the run ended, so the UI can show a real brief instead of a placeholder.
pub fn subject_of(dir: &Path, name: &str) -> Option<String> {
    let sha = resolve(dir, name)?;
    let subject = git(dir, &["log", "-1", "--format=%s", &sha])?;
    if subject.is_empty() {
        None
    } else {
        Some(subject)
    }
}

/// The checked-out branch, or `None` when detached or outside a repository.
pub fn branch(dir: &Path) -> Option<String> {
    let name = git(dir, &["symbolic-ref", "--short", "-q", "HEAD"])?;
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Uncommitted work in a worktree. Untracked files count as dirty paths but not
/// as line changes — they have no committed counterpart to diff against.
pub fn status(dir: &Path) -> Option<RepoStatus> {
    Some(RepoStatus {
        branch: branch(dir).unwrap_or_default(),
        dirty: dirty_paths(dir)?,
        stat: diff_stat(dir, &["diff", "--shortstat", "HEAD"]).unwrap_or_default(),
    })
}

/// How many paths in `dir` are uncommitted — the `dirty` half of [`status`],
/// without the line delta a caller asking "is this checkout clean" does not
/// need. One process instead of two, which matters on the path that asks about
/// every worktree in the repository ([`unlandable`]).
fn dirty_paths(dir: &Path) -> Option<usize> {
    let porcelain = git(dir, &["status", "--porcelain"])?;
    Some(
        porcelain
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
    )
}

/// The work a branch adds on top of its base: exactly that branch's own commits,
/// even when it was branched from another agent's branch (the merge-base is the
/// point it forked from, so nested work is never counted twice).
///
/// Both names are resolved to commit ids first: a branch name is untrusted
/// input, and git would read one that begins with `-` (say
/// `--output=/tmp/x`) as an option to `diff` rather than as a revision.
pub fn branch_stat(dir: &Path, base: &str, branch: &str) -> Option<Stat> {
    let base = resolve(dir, base)?;
    let branch = resolve(dir, branch)?;
    diff_stat(dir, &["diff", "--shortstat", &format!("{base}...{branch}")])
}

/// Where an isolated agent's worktree lives, under the repository root. This
/// constant is the *only* spelling of that directory: the path a worktree is
/// created at, the path a row prints, and the path `/discard` removes used to be
/// three separate `format!`s, and a divergence between them is a worktree nobody
/// can reclaim (docs/refactor.md §3.6).
pub const WORKTREE_DIR: &str = ".mush/wt";

/// The worktree of agent `id`, relative to the repository root:
/// `<WORKTREE_DIR>/<id>`. One spelling, shared by the path a worktree is
/// created at, the path a row prints, and the path `/discard` removes — three
/// `format!`s used to drift (docs/refactor.md §3.6, R27).
pub fn worktree_rel(id: u64) -> String {
    format!("{WORKTREE_DIR}/{id}")
}

/// The worktree of agent `id`: `<root>/.mush/wt/<id>`.
pub fn worktree_path(root: &Path, id: u64) -> PathBuf {
    root.join(worktree_rel(id))
}

/// The branch namespace every isolated agent's branch lives in: `mush/<id>`.
/// One spelling, for the same reason [`WORKTREE_DIR`] is one: the name a
/// worktree is created on, the id read back out of it, and the ref prefix
/// [`isolated_ids`] asks git for used to be three copies — and a divergence
/// between them is a branch nobody can find or reclaim.
const BRANCH_PREFIX: &str = "mush/";

/// The branch an isolated agent's worktree is checked out on: `mush/<id>`.
/// Mush creates it and mush reclaims it, so its name is not the human's to
/// choose — [`worktree_id`] reads the id back out of it.
pub fn branch_name(id: u64) -> String {
    format!("{BRANCH_PREFIX}{id}")
}

/// How many isolated worktrees one repository may hold before an isolated spawn
/// is refused (finding H10, H17).
///
/// The number is deliberately above the window of children a reaped session
/// keeps: the cap bounds what a run leaves on disk, and it must never be the
/// thing that refuses a delegation the history could still hold. It counts
/// checkouts that are **not landable** — the ones no sweep will take — so a
/// worktree whose work is already merged, or one whose run never committed,
/// never spends a slot on its way out.
pub const MAX_WORKTREES: usize = 70;

/// The agent id in a `mush/<id>` branch name, `None` for any other name. A
/// branch the human made by hand must not be adopted as mush's leftover, so
/// everything that is not exactly this shape stays unnamed.
pub fn worktree_id(branch: &str) -> Option<u64> {
    branch.strip_prefix(BRANCH_PREFIX)?.parse().ok()
}

/// One entry of `git worktree list --porcelain`: where the checkout is, the
/// branch that is out (absent when HEAD is detached), and — for mush's own
/// `mush/<id>` worktrees — the agent id, so a worktree left behind by an
/// earlier session is registered under the id its branch claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub id: Option<u64>,
}

impl Worktree {
    /// Whether the checkout is really there. Git's registry entry outlives the
    /// directory — that is how `git worktree list` keeps naming a worktree a
    /// human deleted with `rm -rf` — so a caller must ask this before treating
    /// one as work on disk (finding P13).
    pub fn on_disk(&self) -> bool {
        self.path.exists()
    }
}

/// Every worktree of the repository at `dir`, in git's order (the main checkout
/// first). `None` when git is missing, fails, or `dir` is not a repository: a
/// caller that cannot ask git has no worktrees to reclaim, and must leave the
/// ones it already knows about alone rather than drop them.
pub fn worktrees(dir: &Path) -> Option<Vec<Worktree>> {
    let text = git(dir, &["worktree", "list", "--porcelain"])?;
    Some(parse_worktrees(&text))
}

/// Parse `git worktree list --porcelain`: blank-line-separated blocks, each
/// starting with `worktree <path>` and carrying `branch refs/heads/<name>` when
/// a branch is out. A detached or bare worktree simply has no branch line, and
/// that is not an error — it is a worktree with nothing for mush to name. Pure,
/// so the shapes git can emit are testable without a repository.
pub fn parse_worktrees(text: &str) -> Vec<Worktree> {
    text.split("\n\n")
        .filter_map(|block| {
            let mut path = None;
            let mut branch = None;
            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("worktree ") {
                    path = Some(PathBuf::from(rest));
                } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
                    branch = Some(rest.to_string());
                }
            }
            // A block without a path is not a worktree: a stray blank line at
            // the end of the listing must not become an entry pointing nowhere.
            let path = path?;
            let id = branch.as_deref().and_then(worktree_id);
            Some(Worktree { path, branch, id })
        })
        .collect()
}

/// Whether the repository at `dir` has a commit at all. A fresh `git init` does
/// not, and there is nothing to branch an isolated agent from — a caller asks
/// this *before* trying, so it can report that instead of relaying whatever
/// `worktree add` says about an unborn HEAD.
///
/// `Some(false)` is git answering "no", `None` is git not answering at all (no
/// binary). The two want different words in front of a human, so they are not
/// collapsed into one `bool`.
pub fn has_commits(dir: &Path) -> Option<bool> {
    head_answer(run(dir, &["rev-parse", "--verify", "-q", "HEAD"]))
}

/// Read the answer to the HEAD probe. Split out so the three outcomes are
/// testable without a machine that has no git: the point of the ternary is that
/// the two failures mean different things, and that distinction is what this
/// function *is*.
fn head_answer(probe: Result<String, String>) -> Option<bool> {
    match probe {
        Ok(_) => Some(true),
        // See `GIT_UNAVAILABLE`.
        Err(error) if error == GIT_UNAVAILABLE => None,
        Err(_) => Some(false),
    }
}

/// Create the worktree at [`worktree_path`] on a new [`branch_name`], based on
/// `base` — the parent agent's branch, or `HEAD` when the caller has none.
/// Returns the path and the branch, both from the formatters above, so no caller
/// ever spells `.mush/wt/<id>` or `mush/<id>` itself.
///
/// The three ways this can refuse each carry a reason a human has to read: no
/// repository, no commit to start from, and git's own message when the add
/// itself fails (an id whose branch or directory is still taken). The spawn
/// tool treats every one of them as a refused delegation: a base is a promise
/// about history, and a child running on the wrong one is worse than no child.
pub fn worktree_add(dir: &Path, id: u64, base: Option<&str>) -> Result<(PathBuf, String), String> {
    if !dir.join(".git").exists() {
        return Err("not a git repository".to_string());
    }
    match (base, has_commits(dir)) {
        (None, Some(false)) => {
            return Err("the repo has no commits yet — commit first or drop isolated".to_string())
        }
        // A missing git is not a missing commit, and saying so would send a
        // human looking for a `git commit` they cannot run either.
        (None, None) => return Err(GIT_UNAVAILABLE.to_string()),
        _ => {}
    }
    let path = worktree_path(dir, id);
    let branch = branch_name(id);
    // The name is `worktree add`, not `run`'s `worktree`: a silent failure has
    // to name the subcommand that failed, and this is the call that knows it.
    run_named(
        dir,
        "worktree add",
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            path.to_str().unwrap_or(""),
            base.unwrap_or("HEAD"),
        ],
    )
    .map(|_| (path, branch))
}

/// What one look at the worktree of agent `id` found: the decision [`reclaim`]
/// would make, read-only.
///
/// The read and the removal are two functions because only the caller knows
/// *when* a directory may be taken: the UI reads the repository on its git
/// worker and removes on the thread that owns the tree, so a node whose agent
/// started running in between keeps the worktree it is working in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reclaimable {
    /// Neither the checkout nor the branch is there: nothing to reclaim, and
    /// nothing to say.
    Nothing,
    /// The branch adds nothing to its base (its work is merged, or the run
    /// never committed) and the checkout has nothing uncommitted: [`reclaim`]
    /// removes both.
    Landable,
    /// Work a removal could destroy. `why` names the branch or the checkout and
    /// the reason, in words a human can act on — the branch is the only thing
    /// that still says where the work is.
    Kept(String),
}

/// What [`reclaim`] did, told apart the way a caller has to use it: removed,
/// kept with a reason, or nothing there at all. A `Kept` that cannot be told
/// from a removal — or a failure that cannot be told from either — is a row
/// claiming a worktree is gone while it is still on disk (finding H10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reclaimed {
    /// The checkout is gone. `branch_kept` names the branch git would not
    /// delete: `branch -d`, never `-D`, only deletes a branch whose work is in
    /// the current HEAD, so a nested child whose base is its parent's unmerged
    /// branch leaves the ref behind. That residue costs nothing —
    /// [`isolated_ids`] still names it, so the id floor still reserves it — and
    /// forcing it away is exactly the deletion this rule exists to prevent.
    Removed { branch_kept: Option<String> },
    /// Left alone, for the reason `why` names.
    Kept(String),
    /// Neither checkout nor branch: there was nothing to reclaim.
    Nothing,
}

/// Whether the worktree of agent `id` can be reclaimed right now, read-only.
///
/// `base` is the ref the branch was forked from: the parent agent's branch, or
/// `HEAD` for a child of the root. A branch that is an ancestor of its base adds
/// nothing to it — merging it by hand makes it one, and a run that committed
/// nothing never stopped being one — so that is the whole merge test, and it is
/// deliberately the only one: a squashed or cherry-picked copy of the work
/// leaves the branch a stranger to its base, and mush keeps it and says why
/// rather than guessing. **An unmerged branch is never deleted**, and nothing
/// here merges anything.
pub fn reclaimable(root: &Path, id: u64, base: &str) -> Reclaimable {
    // The base is a caller's name and may begin with `-`; resolving it to a
    // commit id is the one way a name is allowed near a command line
    // (`branch_stat` says the same about its two names).
    let Some(base_sha) = resolve(root, base) else {
        return Reclaimable::Kept(format!(
            "{base} is not a revision mush can resolve — nothing can be shown merged into it"
        ));
    };
    probe(root, id, base, &base_sha)
}

/// The rule itself, with the base already resolved to a commit id: one home for
/// it, and one process saved per worktree when a caller asks about many of them
/// against one base ([`unlandable`]).
fn probe(root: &Path, id: u64, base: &str, base_sha: &str) -> Reclaimable {
    let rel = worktree_rel(id);
    let branch = branch_name(id);
    let named = resolve(root, &branch).is_some();
    let on_disk = worktree_path(root, id).exists();
    if !named && !on_disk {
        return Reclaimable::Nothing;
    }
    if !named {
        // A checkout whose branch is gone is nobody's to remove: mush deletes a
        // branch and a checkout together, and half of that pair is a directory
        // it cannot account for.
        return Reclaimable::Kept(format!("{rel} is a checkout whose {branch} branch is gone"));
    }
    match ahead_of(root, &branch, base_sha) {
        Some(0) => {}
        Some(count) => {
            return Reclaimable::Kept(format!(
                "{branch} has {} nobody merged into {base}",
                counted(count, "commit", "commits")
            ))
        }
        // git refusing to answer is never "merged": the one answer this must
        // not invent is the answer that deletes a branch.
        None => {
            return Reclaimable::Kept(format!(
                "git could not say whether {branch} is merged into {base}"
            ))
        }
    }
    if on_disk {
        match dirty_paths(&worktree_path(root, id)) {
            Some(0) => {}
            Some(count) => {
                return Reclaimable::Kept(format!(
                    "{rel} has {} in it",
                    counted(count as u64, "uncommitted path", "uncommitted paths")
                ))
            }
            None => {
                return Reclaimable::Kept(format!("{rel} — git could not say whether it is clean"))
            }
        }
    }
    Reclaimable::Landable
}

/// How many commits `branch` carries that `base` does not: `0` is "everything
/// this branch added is in the base", the only state mush reclaims. `None` is
/// git refusing to answer, which is never a `0`.
fn ahead_of(root: &Path, branch: &str, base_sha: &str) -> Option<u64> {
    // Resolved even though the caller built the name out of an id: `resolve` is
    // the one door a name goes through, and a branch spelled `-q` must not
    // reach `rev-list` as an option.
    let tip = resolve(root, branch)?;
    git(
        root,
        &["rev-list", "--count", &format!("{base_sha}..{tip}")],
    )?
    .parse()
    .ok()
}

/// Reclaim the worktree of agent `id`: remove the checkout and delete the
/// branch, but only when [`reclaimable`] says that removes no work — the branch
/// is merged into `base` (or the run never committed), the checkout is clean,
/// and nothing unmerged or dirty is ever touched.
///
/// This is the one place mush removes a worktree or deletes a branch outside its
/// own tests, so the two refusals it is built from are the whole of the
/// guarantee: `git worktree remove --force` (the `--force` is for git's own
/// lock-file bookkeeping, not for a dirty checkout — that case never gets here)
/// and `git branch -d`, never `-D`, so a branch git will not certify as deleted
/// is a branch mush leaves alone (finding H10).
pub fn reclaim(root: &Path, id: u64, base: &str) -> Reclaimed {
    match reclaimable(root, id, base) {
        Reclaimable::Nothing => Reclaimed::Nothing,
        Reclaimable::Kept(why) => Reclaimed::Kept(why),
        Reclaimable::Landable => remove(root, id),
    }
}

/// Take a checkout [`probe`] called landable, checkout first and branch second:
/// git refuses to delete a branch that is checked out anywhere, so the order is
/// not a preference. A removal that fails leaves the branch alone — nothing
/// happened, and the caller must not read it as `Removed`.
fn remove(root: &Path, id: u64) -> Reclaimed {
    let rel = worktree_rel(id);
    if worktree_path(root, id).exists() {
        if let Err(error) = run(root, &["worktree", "remove", "--force", &rel]) {
            return Reclaimed::Kept(format!("{rel} could not be removed: {error}"));
        }
    } else {
        // The checkout is already gone, and git still has it registered: git
        // refuses to delete a branch that is checked out *anywhere*, so an
        // entry pointing at a directory nobody has holds the name for good —
        // which is H10's specimen, `mush/2` with no `.mush/wt/2` beside it.
        // `prune` drops exactly those entries and touches nothing else.
        let _ = run(root, &["worktree", "prune"]);
    }
    let branch = branch_name(id);
    let branch_kept = run(root, &["branch", "-d", &branch])
        .is_err()
        .then_some(branch);
    Reclaimed::Removed { branch_kept }
}

/// Every agent id git still names with a `mush/<id>` branch, checkout or not,
/// lowest first.
///
/// A branch outlives the directory git registered it against, which is how a
/// merged child that was never reclaimed keeps the next `git worktree add -b
/// mush/<id>` refusing (finding H10, P13). Naming every one of them lets a
/// caller reclaim the merged ones and reserve the rest.
pub fn isolated_ids(dir: &Path) -> Option<Vec<u64>> {
    let refs = format!("refs/heads/{BRANCH_PREFIX}");
    let text = git(dir, &["for-each-ref", "--format=%(refname:short)", &refs])?;
    let mut ids: Vec<u64> = text.lines().filter_map(worktree_id).collect();
    ids.sort_unstable();
    ids.dedup();
    Some(ids)
}

/// The isolated worktrees that exist and are **not** landable: what
/// [`MAX_WORKTREES`] counts.
///
/// A landable worktree is one the next sweep takes, so it must not be what
/// refuses a spawn — the cap exists to turn today's failure, a `git worktree add`
/// that dies *after* the id was spent, into a planned refusal that names what to
/// clear (finding H17). Read-only, so a caller may ask without changing the
/// repository, and it answers with nothing when git cannot answer at all: a
/// count that cannot be taken is not a hundred worktrees, it is no answer.
pub fn unlandable(root: &Path) -> Vec<u64> {
    let Some(worktrees) = worktrees(root) else {
        return Vec::new();
    };
    let Some(base_sha) = resolve(root, "HEAD") else {
        return Vec::new();
    };
    let mut ids: Vec<u64> = worktrees
        .iter()
        .filter(|worktree| worktree.on_disk())
        .filter_map(|worktree| worktree.id)
        .filter(|id| matches!(probe(root, *id, "HEAD", &base_sha), Reclaimable::Kept(_)))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// `1 commit`, `2 commits` — a count that reads as English, because these
/// lines are read by a human deciding what to keep.
fn counted(count: u64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// Commit everything in the worktree `dir` under `subject`, and answer the short
/// revision — or `None` when the run changed nothing, so a clean worktree costs
/// no empty commit.
///
/// The identity and the message are supplied here (`-c user.name=…`,
/// `--no-verify`) so a commit never depends on the human's git configuration and
/// never runs their hooks. The index belongs to this worktree, so committing
/// here cannot contend with the human's own git commands in the main checkout.
pub fn commit_all(dir: &Path, subject: &str) -> Result<Option<String>, String> {
    if run(dir, &["status", "--porcelain"])?.is_empty() {
        return Ok(None);
    }
    run(dir, &["add", "-A"])?;
    run(
        dir,
        &[
            "-c",
            "user.name=mush",
            "-c",
            "user.email=mush@local",
            "commit",
            "--no-verify",
            "-qm",
            subject,
        ],
    )?;
    Ok(Some(run(dir, &["rev-parse", "--short", "HEAD"])?))
}

/// A revision resolved to its commit id, or `None` when it does not exist. The
/// id is what gets passed on: it cannot be mistaken for an option, and this is
/// the one home for resolving an untrusted name to a sha.
pub fn resolve(dir: &Path, name: &str) -> Option<String> {
    let sha = git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ],
    )?;
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

fn diff_stat(dir: &Path, args: &[&str]) -> Option<Stat> {
    let text = git(dir, args)?;
    Some(parse_shortstat(&text))
}

/// Parse `git diff --shortstat`: ` 3 files changed, 12 insertions(+), 4 deletions(-)`.
/// Any part may be missing — git only prints the lines that apply — and an
/// empty string is a clean tree.
pub fn parse_shortstat(text: &str) -> Stat {
    if text.trim().is_empty() {
        return Stat::default();
    }
    let mut stat = Stat::default();
    let words: Vec<&str> = text
        .split_whitespace()
        .map(|word| word.trim_end_matches(','))
        .collect();
    for (index, word) in words.iter().enumerate() {
        // Every count precedes its noun: `12 insertions(+)`.
        let count = || {
            words
                .get(index.saturating_sub(1))
                .and_then(|n| n.parse().ok())
        };
        match *word {
            "file" | "files" => stat.files = count().unwrap_or(stat.files),
            "insertion(+)" | "insertions(+)" => stat.added = count().unwrap_or(stat.added),
            "deletion(-)" | "deletions(-)" => stat.removed = count().unwrap_or(stat.removed),
            _ => {}
        }
    }
    stat
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    #[test]
    fn shortstat_parses_every_shape() {
        assert_eq!(parse_shortstat(""), Stat::default());
        assert_eq!(
            parse_shortstat(" 1 file changed, 1 insertion(+)"),
            Stat {
                files: 1,
                added: 1,
                removed: 0
            }
        );
        assert_eq!(
            parse_shortstat(" 3 files changed, 12 insertions(+), 4 deletions(-)"),
            Stat {
                files: 3,
                added: 12,
                removed: 4
            }
        );
        assert_eq!(
            parse_shortstat(" 2 files changed, 5 deletions(-)"),
            Stat {
                files: 2,
                added: 0,
                removed: 5
            }
        );
    }

    #[test]
    fn compact_reads_like_a_diff_line() {
        assert_eq!(
            Stat {
                files: 1,
                added: 12,
                removed: 3
            }
            .compact(),
            "+12−3"
        );
        assert_eq!(Stat::default().compact(), "±0");
    }

    #[test]
    fn a_huge_diff_keeps_its_count() {
        let stat = parse_shortstat(" 1 file changed, 5000000000 insertions(+)");
        assert_eq!(stat.files, 1);
        assert_eq!(stat.added, 5_000_000_000, "u32 would have read this as 0");
        assert_eq!(stat.removed, 0);
    }

    /// A branch name is untrusted input: it must reach git as a revision, never
    /// as an option. `--output=…` used to make git write a file and report a
    /// silent zero diff.
    #[test]
    fn a_ref_name_is_never_read_as_an_option() {
        let dir = init_repo("options");
        assert_eq!(branch_stat(&dir, "-x", "master"), None);
        assert_eq!(branch_stat(&dir, "master", "-x"), None);
        assert_eq!(
            branch_stat(&dir, "--output=evil", "master"),
            None,
            "an option-shaped ref must not reach the diff"
        );
        assert!(
            !dir.join("evil...master").exists(),
            "git wrote a file named after a ref"
        );
        // The ordinary case still works.
        let stat = branch_stat(&dir, "HEAD", "master").unwrap();
        assert!(stat.is_empty(), "HEAD is master here: {stat:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A mutating verb reports git's own message, and the subject it wrote is
    /// readable back: this is the pair a leftover worktree is identified by.
    #[test]
    fn run_commits_and_the_subject_reads_back() {
        let dir = init_repo("verb");
        fs::write(dir.join("b.txt"), "two\n").unwrap();
        run(&dir, &["add", "-A"]).unwrap();
        run(
            &dir,
            &[
                "-c",
                "user.name=mush",
                "-c",
                "user.email=mush@local",
                "commit",
                "-qm",
                "mush #4: port the parser",
            ],
        )
        .unwrap();
        assert_eq!(
            subject_of(&dir, "HEAD").as_deref(),
            Some("mush #4: port the parser")
        );
        // A revision that does not exist is not a subject, and neither is a
        // missing branch.
        assert_eq!(subject_of(&dir, "mush/99"), None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A failing verb must carry git's reason rather than an empty error: that
    /// message is what tells a human a merge conflicted.
    #[test]
    fn a_failed_verb_reports_why() {
        let dir = init_repo("verb-fail");
        let error = run(&dir, &["merge", "no-such-branch"]).unwrap_err();
        assert!(!error.is_empty());
        assert!(
            error.contains("no-such-branch") || error.contains("not something we can merge"),
            "{error}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The one failure git says nothing about is named by the caller, not read
    /// back out of the error: `worktree_add` passes `worktree add`, so a silent
    /// add cannot be reported as the group it lives in. This is the mechanism
    /// that replaced matching the error against a copy of its own text.
    #[test]
    fn a_silent_failure_is_named_by_the_caller() {
        let dir = init_repo("verb-silent");
        // `--quiet` and an unresolvable revision: exit 1 with both streams
        // empty, which is the silent path.
        let error = run_named(
            &dir,
            "worktree add",
            &["rev-parse", "--verify", "-q", "no-such-ref"],
        )
        .unwrap_err();
        assert_eq!(error, "git worktree add failed");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The porcelain listing has shapes a repository can really be in, and the
    /// parser has to survive all of them: a detached checkout (no branch line),
    /// a block split by blank lines, and a branch that is not mush's.
    #[test]
    fn porcelain_worktrees_parse_in_every_shape() {
        let text = "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\n\
                    worktree /repo/.mush/wt/3\nHEAD def\nbranch refs/heads/mush/3\n\n\
                    worktree /repo/detached\nHEAD 0123\ndetached\n\n\
                    worktree /repo/mine\nHEAD 4567\nbranch refs/heads/feature/x\n\n\
                    worktree /repo/.mush/wt/9\nHEAD fff\nbranch refs/heads/mush/9\nprunable gitdir file points to non-existent location\n\n";
        let list = parse_worktrees(text);
        assert_eq!(list.len(), 5, "{list:?}");
        assert_eq!(list[0].path, PathBuf::from("/repo"));
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert_eq!(list[0].id, None, "main is not an agent's branch");
        assert_eq!(list[1].path, PathBuf::from("/repo/.mush/wt/3"));
        assert_eq!(list[1].branch.as_deref(), Some("mush/3"));
        assert_eq!(list[1].id, Some(3));
        assert_eq!(list[2].branch, None, "a detached worktree has no branch");
        assert_eq!(list[2].id, None);
        assert_eq!(list[3].id, None, "someone else's branch stays unnamed");
        // `prunable` is a line, not a worktree: git still names the branch and
        // the id, and nothing in the parse treats the block as missing.
        assert_eq!(list[4].id, Some(9), "a prunable entry keeps its branch");
        // A trailing blank line, and a listing that is only whitespace.
        assert_eq!(parse_worktrees("\n\n").len(), 0);
        assert_eq!(parse_worktrees("").len(), 0);
    }

    /// The path and the branch are one formatting rule, and the id round-trips
    /// through the branch name: that is what lets a leftover be registered.
    #[test]
    fn the_path_and_branch_are_one_rule() {
        let root = Path::new("/repo");
        assert_eq!(worktree_rel(12), ".mush/wt/12");
        assert_eq!(worktree_path(root, 12), PathBuf::from("/repo/.mush/wt/12"));
        assert_eq!(branch_name(12), "mush/12");
        assert_eq!(worktree_id(&branch_name(12)), Some(12));
        assert_eq!(worktree_id("mush/"), None);
        assert_eq!(worktree_id("mush/x"), None);
        assert_eq!(worktree_id("main"), None);
        assert_eq!(worktree_id("refs/heads/mush/1"), None);
    }

    /// "No commits yet" and "no git at all" are different answers for a human:
    /// one is a command they can run, the other is a program that is not
    /// installed. They must not collapse into one `false` — which is what a
    /// renamed message in `run` would do, silently, so the distinction is
    /// pinned here rather than left to a string comparison nobody tests.
    #[test]
    fn a_missing_git_is_not_a_repository_without_commits() {
        assert_eq!(head_answer(Ok("abc123".into())), Some(true));
        assert_eq!(head_answer(Err(GIT_UNAVAILABLE.to_string())), None);
        assert_eq!(
            head_answer(Err("fatal: Needed a single revision".to_string())),
            Some(false),
            "git answered: the repository is just empty"
        );
        // The message `has_commits` compares against is the one `run` writes.
        assert_eq!(GIT_UNAVAILABLE, "git binary unavailable");
    }

    /// The two mutating worktree verbs against a real repository: a worktree is
    /// created where the formatter says, and its work commits once — the second
    /// call finds nothing and must not make an empty commit.
    #[test]
    fn a_worktree_is_added_and_committed_once() {
        let dir = init_repo("worktree-verbs");
        assert_eq!(has_commits(&dir), Some(true));
        let (path, branch) = worktree_add(&dir, 5, None).unwrap();
        assert_eq!(path, worktree_path(&dir, 5));
        assert_eq!(branch, branch_name(5));
        assert!(path.join(".git").exists(), "the worktree is a checkout");

        fs::write(path.join("work.txt"), "the work\n").unwrap();
        let revision = commit_all(&path, "mush #5: do the thing").unwrap();
        assert!(revision.is_some(), "the worktree had work to commit");
        assert_eq!(
            subject_of(&path, "HEAD").as_deref(),
            Some("mush #5: do the thing")
        );
        assert_eq!(
            commit_all(&path, "mush #5: do the thing").unwrap(),
            None,
            "a clean worktree must not be committed again"
        );

        // A directory that is not a repository refuses before touching git's
        // worktree state, and says so.
        let plain = std::env::temp_dir().join(format!("mush-git-plain-{}", std::process::id()));
        let _ = fs::remove_dir_all(&plain);
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(
            worktree_add(&plain, 6, None).unwrap_err(),
            "not a git repository"
        );
        let _ = fs::remove_dir_all(&plain);

        // A repository with no commit yet is the other refusal: there is
        // nothing to branch from, and the reason says so.
        let unborn = std::env::temp_dir().join(format!("mush-git-unborn-{}", std::process::id()));
        let _ = fs::remove_dir_all(&unborn);
        fs::create_dir_all(&unborn).unwrap();
        let init = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&unborn)
                .args(args)
                .output()
                .unwrap();
        };
        init(&["init", "-q"]);
        assert_eq!(has_commits(&unborn), Some(false));
        assert_eq!(
            worktree_add(&unborn, 7, None).unwrap_err(),
            "the repo has no commits yet — commit first or drop isolated"
        );
        // With a base branch named, the refusal is git's own.
        assert!(worktree_add(&unborn, 7, Some("HEAD")).is_err());
        let _ = fs::remove_dir_all(&unborn);
        let _ = fs::remove_dir_all(&dir);
    }

    fn init_repo(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mush-git-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
        };
        run(&["-c", "init.defaultBranch=master", "init", "-q"]);
        run(&["config", "user.email", "mush@test"]);
        run(&["config", "user.name", "mush"]);
        fs::write(dir.join("a.txt"), "one\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "init"]);
        dir
    }

    /// The list has to answer `git` questions about a real repository, not just
    /// its own parser: that is the whole point of the module.
    #[test]
    fn a_real_repository_answers() {
        let dir = init_repo("status");
        let clean = status(&dir).unwrap();
        assert_eq!(clean.branch, "master");
        assert_eq!(clean.dirty, 0);
        assert!(clean.stat.is_empty());

        fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        fs::write(dir.join("new.txt"), "untracked\n").unwrap();
        let dirty = status(&dir).unwrap();
        assert_eq!(dirty.dirty, 2, "one modified, one untracked");
        assert_eq!(dirty.stat.added, 1, "untracked files have no line stat");
        assert_eq!(dirty.stat.files, 1);

        // A branch's own work, measured from the point it forked.
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["checkout", "-qb", "mush/1"])
            .output()
            .unwrap();
        fs::write(dir.join("b.txt"), "x\ny\n").unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["add", "b.txt"]) // only the branch's own work
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["commit", "-qm", "work"])
            .output()
            .unwrap();
        let stat = branch_stat(&dir, "master", "mush/1").unwrap();
        assert_eq!(stat.files, 1);
        assert_eq!(stat.added, 2);
        assert_eq!(stat.removed, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------------- reclaim
    //
    // H10's tests: every one of them is a real repository, a real worktree and
    // real git, because the whole question is what git still names after mush
    // has been through — and the answer a caller reads decides whether a branch
    // is deleted.

    /// Run git in `dir`, failing the test if it does.
    fn git_in(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    /// A worktree for agent `id` on `mush/<id>` with one commit of its own —
    /// the state every reclaim test starts from, and the state a finished run
    /// leaves behind.
    fn isolated_worktree(dir: &Path, id: u64) {
        worktree_add(dir, id, Some("HEAD")).unwrap();
        let path = worktree_path(dir, id);
        // The content is the id's: a worktree forked from a base that already
        // merged an earlier test's `work.txt` would otherwise be clean, and
        // `commit_all` would answer `None` — which is the *other* test's case.
        fs::write(path.join("work.txt"), format!("work {id}\n")).unwrap();
        assert!(
            commit_all(&path, &format!("mush #{id}: work"))
                .unwrap()
                .is_some(),
            "the run's work must really be committed"
        );
    }

    /// Land `branch` in the main checkout, the way a human does it by hand.
    fn merge_into_head(dir: &Path, branch: &str) {
        git_in(dir, &["merge", "--no-edit", branch]);
    }

    /// A merged branch's checkout and branch both go, and `Removed` says so
    /// instead of leaving the caller to guess. This is the reclamation the
    /// specimen asked for: `mush/<id>` merged into HEAD, still named by git,
    /// still fatal to the next `worktree add -b mush/<id>` (finding H10).
    #[test]
    fn a_merged_worktree_and_branch_are_removed() {
        let dir = init_repo("reclaim-merged");
        isolated_worktree(&dir, 1);
        merge_into_head(&dir, "mush/1");
        let landed = resolve(&dir, "HEAD").unwrap();

        assert_eq!(
            reclaim(&dir, 1, "HEAD"),
            Reclaimed::Removed { branch_kept: None }
        );

        assert!(!worktree_path(&dir, 1).exists(), "the checkout is gone");
        assert_eq!(resolve(&dir, "mush/1"), None, "and so is the branch");
        assert_eq!(
            git(&dir, &["cat-file", "-t", &landed]).as_deref(),
            Some("commit"),
            "the work is in HEAD, where the merge put it"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A run that committed nothing leaves a branch standing on its base: there
    /// is no work to keep, so the checkout and the branch go the way a merged
    /// one does. Without this, every isolated run that changed nothing leaves a
    /// worktree behind for good.
    #[test]
    fn a_clean_run_that_committed_nothing_is_reclaimed() {
        let dir = init_repo("reclaim-clean");
        worktree_add(&dir, 4, Some("HEAD")).unwrap();
        // Nothing written and nothing committed: `mush/4` *is* its base, which
        // is the second half of the merge test and the fact a run's own end
        // knows before any of this is asked.

        assert_eq!(
            reclaim(&dir, 4, "HEAD"),
            Reclaimed::Removed { branch_kept: None }
        );
        assert!(!worktree_path(&dir, 4).exists());
        assert_eq!(resolve(&dir, "mush/4"), None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The rule the whole patch is built on: an unmerged branch is **kept**, and
    /// the refusal names it. `branch -D` is never run, so the commit is still
    /// there, reachable from its branch, with its checkout on disk — the test's
    /// point is not the return value but what is left in the repository.
    #[test]
    fn an_unmerged_branch_is_kept_and_named() {
        let dir = init_repo("reclaim-unmerged");
        isolated_worktree(&dir, 2);
        let tip = resolve(&dir, "mush/2").unwrap();

        match reclaim(&dir, 2, "HEAD") {
            Reclaimed::Kept(why) => {
                assert!(why.contains("mush/2"), "the refusal names it: {why}");
                assert!(why.contains("1 commit"), "and what holds it: {why}");
                assert!(
                    why.contains("HEAD"),
                    "and what it was measured against: {why}"
                );
            }
            other => panic!("an unmerged branch must be kept, got {other:?}"),
        }

        assert!(worktree_path(&dir, 2).exists(), "the checkout stays");
        assert_eq!(
            resolve(&dir, "mush/2"),
            Some(tip.clone()),
            "the branch stays"
        );
        assert_eq!(
            git(&dir, &["cat-file", "-t", &tip]).as_deref(),
            Some("commit"),
            "and its commit is still in the repository"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A merged branch with uncommitted work in its checkout is kept: the merge
    /// makes the *branch* landable, not the file, and that file is the only copy
    /// there is. Dirty loses to nothing.
    #[test]
    fn a_dirty_checkout_is_kept_even_when_its_branch_is_merged() {
        let dir = init_repo("reclaim-dirty");
        isolated_worktree(&dir, 3);
        merge_into_head(&dir, "mush/3");
        fs::write(worktree_path(&dir, 3).join("work.txt"), "edited again\n").unwrap();

        match reclaim(&dir, 3, "HEAD") {
            Reclaimed::Kept(why) => {
                assert!(why.contains(".mush/wt/3"), "it names the checkout: {why}");
                assert!(why.contains("uncommitted path"), "and the reason: {why}");
            }
            other => panic!("a dirty checkout must be kept, got {other:?}"),
        }

        assert!(
            worktree_path(&dir, 3).join("work.txt").exists(),
            "the edit is still there"
        );
        assert!(resolve(&dir, "mush/3").is_some(), "and so is the branch");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Nothing there is its own answer, and an unresolvable base is a refusal
    /// rather than a removal: the one answer that deletes a branch is not one to
    /// infer from a name git could not resolve.
    #[test]
    fn nothing_there_is_neither_removed_nor_kept() {
        let dir = init_repo("reclaim-nothing");
        assert_eq!(reclaimable(&dir, 9, "HEAD"), Reclaimable::Nothing);
        assert_eq!(reclaim(&dir, 9, "HEAD"), Reclaimed::Nothing);

        worktree_add(&dir, 9, Some("HEAD")).unwrap();
        match reclaim(&dir, 9, "no-such-base") {
            Reclaimed::Kept(why) => assert!(why.contains("no-such-base"), "{why}"),
            other => panic!("an unresolvable base must keep the worktree, got {other:?}"),
        }
        assert!(worktree_path(&dir, 9).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A merged branch whose checkout a human already deleted is reclaimed all
    /// the same. This is H10's specimen exactly: `mush/2` and `mush/3` were
    /// merged, their directories long gone, and the next isolated spawn died on
    /// `a branch named 'mush/2' already exists` until a human deleted them.
    #[test]
    fn a_dir_less_merged_branch_is_deleted() {
        let dir = init_repo("reclaim-residue");
        isolated_worktree(&dir, 5);
        merge_into_head(&dir, "mush/5");
        fs::remove_dir_all(worktree_path(&dir, 5)).unwrap();

        assert_eq!(
            reclaim(&dir, 5, "HEAD"),
            Reclaimed::Removed { branch_kept: None }
        );
        assert_eq!(resolve(&dir, "mush/5"), None, "the name is free again");
        assert!(
            !isolated_ids(&dir).unwrap().contains(&5),
            "and git no longer names it"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The other residue: a checkout that went away on its own, whose work was
    /// never merged. The branch stays, and it is the reason the id floor exists.
    #[test]
    fn a_dir_less_unmerged_branch_is_kept() {
        let dir = init_repo("reclaim-residue-unmerged");
        isolated_worktree(&dir, 6);
        fs::remove_dir_all(worktree_path(&dir, 6)).unwrap();

        match reclaim(&dir, 6, "HEAD") {
            Reclaimed::Kept(why) => assert!(why.contains("mush/6"), "{why}"),
            other => panic!("an unmerged residue must be kept, got {other:?}"),
        }
        assert!(resolve(&dir, "mush/6").is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The one case that proves `-D` is never reached: a nested child's branch
    /// is merged into its *parent's* branch, and `branch -d` measures against
    /// the root checkout's HEAD, which does not have it. The checkout goes, the
    /// ref stays, and `Removed` says which is which — a caller told "removed"
    /// while a ref it cannot see is still standing is a caller that will lie on
    /// a row.
    #[test]
    fn a_merged_branch_git_will_not_delete_is_reported_as_left_behind() {
        let dir = init_repo("reclaim-nested");
        // The parent's branch, checked out in its own worktree — mush's own
        // shape for a nested agent, so the root checkout stays on master.
        worktree_add(&dir, 9, Some("HEAD")).unwrap();
        let parent = worktree_path(&dir, 9);
        fs::write(parent.join("parent.txt"), "parent\n").unwrap();
        commit_all(&parent, "mush #9: parent work").unwrap();
        // The child, forked from the parent's branch and merged back into it.
        worktree_add(&dir, 10, Some("mush/9")).unwrap();
        let child = worktree_path(&dir, 10);
        fs::write(child.join("child.txt"), "child\n").unwrap();
        commit_all(&child, "mush #10: child work").unwrap();
        git_in(&parent, &["merge", "--no-edit", "mush/10"]);
        let base = resolve(&dir, "mush/9").unwrap();

        match reclaim(&dir, 10, &base) {
            Reclaimed::Removed { branch_kept } => assert_eq!(
                branch_kept.as_deref(),
                Some("mush/10"),
                "the ref git would not delete is named, not forced"
            ),
            other => panic!("the child's work is merged into its base, got {other:?}"),
        }

        assert!(!child.exists(), "the checkout went");
        assert!(
            resolve(&dir, "mush/10").is_some(),
            "and the branch mush must not force is still there"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// `isolated_ids` is what the startup pass sweeps and what the id floor
    /// reserves: every `mush/<id>` git still names, checkout or not, and nothing
    /// else — a branch the human made by hand must not be adopted as mush's.
    #[test]
    fn isolated_ids_names_branches_with_and_without_a_checkout() {
        let dir = init_repo("reclaim-ids");
        isolated_worktree(&dir, 1);
        isolated_worktree(&dir, 5);
        fs::remove_dir_all(worktree_path(&dir, 5)).unwrap();
        git_in(&dir, &["branch", "mush/handmade"]);
        git_in(&dir, &["branch", "feature"]);

        assert_eq!(isolated_ids(&dir).unwrap(), vec![1, 5]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The cap counts what a sweep will not take. A worktree whose work is
    /// already merged is leaving on its own, so counting it would refuse a spawn
    /// over a worktree that is about to stop existing — which is the failure the
    /// cap was written to end (finding H17).
    #[test]
    fn the_cap_counts_the_worktrees_that_are_not_landable() {
        let dir = init_repo("reclaim-count");
        isolated_worktree(&dir, 1); // unmerged work: counted
        isolated_worktree(&dir, 2);
        merge_into_head(&dir, "mush/2"); // merged and clean: not counted
        isolated_worktree(&dir, 3);
        merge_into_head(&dir, "mush/3");
        fs::write(worktree_path(&dir, 3).join("later.txt"), "later\n").unwrap(); // dirty: counted

        assert_eq!(unlandable(&dir), vec![1, 3]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A string is what a human reads here, so one commit is not `1 commits`.
    #[test]
    fn a_count_reads_as_english() {
        assert_eq!(counted(1, "commit", "commits"), "1 commit");
        assert_eq!(counted(2, "commit", "commits"), "2 commits");
        assert_eq!(counted(0, "commit", "commits"), "0 commits");
    }
}
