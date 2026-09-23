//! Reading the repository the way a glance needs it: which branch, how dirty,
//! how many lines. One `git` process per question, cached by the caller — the
//! UI never shells out while painting (docs/mush.md §8).
//!
//! Everything here is best-effort: a workspace that is not a repository, or a
//! `git` binary that is missing, answers `None` rather than failing a caller.
//!
//! Every child is started through [`scrub`]: git does not talk to the provider,
//! so mush's credential is not in the environment it is handed (finding C1).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::secrets::scrub;

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
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args).env("LC_ALL", "C");
    let output = scrub(&mut command).output().ok()?;
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
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args).env("LC_ALL", "C");
    let output = scrub(&mut command)
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
        // The human's own checkout is not dirty because it built: `target/` and
        // every other path an ignore rule covers is deliberately not theirs to
        // commit. The reclaim probe reads those paths for itself — for a
        // worktree they are work no commit can keep (finding F1).
        dirty: changes(dir)?
            .iter()
            .filter(|change| !change.ignored)
            .count(),
        stat: diff_stat(dir, &["diff", "--shortstat", "HEAD"]).unwrap_or_default(),
    })
}

/// One path `git status --porcelain --ignored=matching` reports in `dir`: its
/// name, and whether it is there only because an ignore rule covers it (an `!!`
/// line).
///
/// Telling the ignored half apart is what lets one reading answer both
/// questions the tree asks of a checkout: whether a commit would take anything
/// from it, and whether it holds paths a commit *cannot* keep. `git status
/// --porcelain` alone answers only the first — a run whose whole deliverable
/// matched the repository's own `.gitignore` read as "clean — nothing changed"
/// and was swept, taking the only copy (finding F1). `--ignored=matching` names
/// an ignored directory once instead of every file inside it, which is the
/// reading a `target/`-sized tree needs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Change {
    path: String,
    ignored: bool,
}

/// The paths git reports in `dir`, in its own order: tracked edits and untracked
/// files first, the ignored ones after them. One process and one parse, so the
/// reclaim probe and [`commit_all`] cannot disagree about whether a run changed
/// anything (finding F1).
fn changes(dir: &Path) -> Option<Vec<Change>> {
    let porcelain = git(dir, &["status", "--porcelain", "--ignored=matching"])?;
    Some(
        porcelain
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let code = line.get(..2).unwrap_or("");
                // A rename is `R  old -> new`: the path that exists is the new
                // one. Everything else is `XY path`, the path from the third
                // byte on (a quoted path keeps its quoting — it is a name in a
                // sentence here, not something mush hands back to git).
                let path = line.get(3..).unwrap_or("");
                let path = path.rsplit_once(" -> ").map(|(_, to)| to).unwrap_or(path);
                Change {
                    path: path.to_string(),
                    ignored: code == "!!",
                }
            })
            .collect(),
    )
}

/// The first one or two of `paths`, and a count for the rest: `a.txt`,
/// `a.txt, b.txt`, `a.txt, b.txt and 3 more`. A row's sentence has to say what
/// the work is without listing a whole `target/` (H10's habit).
pub fn named_paths(paths: &[String]) -> String {
    match paths {
        [] => String::new(),
        [one] => one.clone(),
        [one, two] => format!("{one}, {two}"),
        [one, two, rest @ ..] => format!("{one}, {two} and {} more", rest.len()),
    }
}

/// Why a checkout [`probe`] cannot remove must stay, in words a human can act
/// on: what is in it and, when the paths are ones no commit can keep, which
/// ones — the ignored half is the part a `git add -A` would silently drop, so
/// it is the part a deletion would destroy (finding F1).
fn kept_checkout(rel: &str, found: &[Change]) -> String {
    let ignored: Vec<String> = found
        .iter()
        .filter(|change| change.ignored)
        .map(|change| change.path.clone())
        .collect();
    let uncommitted = found.len() - ignored.len();
    match (uncommitted, ignored.len()) {
        (0, count) => format!(
            "{rel} holds {}, which a commit cannot keep: {} — land or discard it by hand",
            counted(count as u64, "ignored path", "ignored paths"),
            named_paths(&ignored)
        ),
        (count, 0) => format!(
            "{rel} has {} in it",
            counted(count as u64, "uncommitted path", "uncommitted paths")
        ),
        (count, ignored_count) => format!(
            "{rel} has {} and {} in it: {}",
            counted(count as u64, "uncommitted path", "uncommitted paths"),
            counted(ignored_count as u64, "ignored path", "ignored paths"),
            named_paths(&ignored)
        ),
    }
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
/// worktree whose work is already merged, or one whose run committed nothing
/// and left no path a commit cannot keep, never spends a slot on its way out.
///
/// Ignored work does spend one, and that is the price of finding F1's rule: a
/// child that merely compiled has a `target/` no commit can keep, so its
/// checkout is kept until the human discards it — one of these slots, visible
/// on the row, against a silent deletion of a deliverable only that directory
/// holds.
pub const MAX_WORKTREES: usize = 70;

/// The agent id in a `mush/<id>` branch name, `None` for any other name.
///
/// A branch the human made by hand must not be adopted as mush's leftover, so
/// everything that is not exactly this shape stays unnamed. Neither does a name
/// the id space can keep no floor above: the floor mush keeps is one past the
/// largest id the repository has named, so `mush/18446744073709551615` would
/// pin that counter at the end of its space and the next draw would overflow
/// it (`attempt to add with overflow` in a debug build). Refusing the name here
/// — where every road from a branch to an id passes — keeps the floor usable;
/// `saturating_add` at the reservation sites is the belt for the roads a
/// hand-edited file reaches.
///
/// Callers that need to tell a refusable name from a branch that was never
/// mush's ask [`is_child_branch`].
pub fn worktree_id(branch: &str) -> Option<u64> {
    branch
        .strip_prefix(BRANCH_PREFIX)?
        .parse()
        .ok()
        .filter(|id| *id < u64::MAX)
}

/// Whether `branch` sits in mush's own branch namespace — `mush/<…>`, the shape
/// a child's branch is given. A name here that [`worktree_id`] refuses is not a
/// child's, and it is not another program's either: a caller names it for the
/// human, rather than passing it over in silence as a branch that was never
/// mush's.
pub fn is_child_branch(branch: &str) -> bool {
    branch.starts_with(BRANCH_PREFIX)
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

/// Whether an isolated child can be branched from `dir` at all, in the words a
/// human reads when it cannot.
///
/// Two states refuse before any base name matters, and this is their one home:
/// `dir` is no repository git can answer for, or it is a repository with no
/// commit. A fresh `git init` has no `HEAD`, so *every* base name fails to
/// resolve there — and the sentence written for the state is the one to read,
/// not git's own about the name a model happened to speak (finding F16).
///
/// [`worktree_add`] asks this before it looks at a base; a caller that holds a
/// base *name* asks it before resolving the name, so the broken case does not
/// spend a resolve it cannot use. The `.git` test this replaces asked a question
/// git does not: a linked worktree's `.git` is a file, and a workspace that is a
/// subdirectory of a repository has none at all while `rev-parse` answers every
/// question inside it, so the whole isolated road was refused about a directory
/// the human had opened mush in (finding F11). The question is git's own.
pub fn can_branch_from(dir: &Path) -> Result<(), String> {
    match run(dir, &["rev-parse", "--git-dir"]) {
        Ok(_) => {}
        Err(error) if error == GIT_UNAVAILABLE => return Err(error),
        Err(_) => return Err("not a git repository".to_string()),
    }
    match has_commits(dir) {
        Some(true) => Ok(()),
        Some(false) => {
            Err("the repo has no commits yet — commit first or drop isolated".to_string())
        }
        // A missing git is not a missing commit, and saying so would send a
        // human looking for a `git commit` they cannot run either.
        None => Err(GIT_UNAVAILABLE.to_string()),
    }
}

/// Create the worktree at [`worktree_path`] on a new [`branch_name`], based on
/// `base` — the resolved revision the branch forks from, or `HEAD` in `dir`
/// when the caller has none. Returns the path and the branch, both from the
/// formatters above, so no caller ever spells `.mush/wt/<id>` or `mush/<id>`
/// itself.
///
/// A caller that has a *name* (the spawn's `base` argument) resolves it first,
/// in the workspace whose view the name was spoken in: the object store is
/// shared, so a revision named in a nested agent's worktree is the same commit
/// here, while the word `HEAD` is not (finding F9). Nothing about whose `HEAD`
/// a base meant can be decided at this door.
///
/// Whether the repository has a commit *at all* is asked before the base is
/// used: a child needs a fork revision, and the repository that has none must
/// refuse with the sentence written for that state rather than with git's own
/// about whatever name was handed in (finding F16). The answer is one process,
/// asked once, for both roads — with and without a base.
///
/// `dir` is the caller's workspace, and may be any directory *inside* a
/// repository: git's own questions are answered from there and the checkout is
/// made under it, so `mush crates/mush` gets `.mush/wt/<id>` below its own
/// workspace like any root does (finding F11).
///
/// Every way this can refuse carries a reason a human has to read: not a
/// repository, no commit to start from, a path git cannot be handed, and git's
/// own message when the add itself fails (an id whose branch or directory is
/// still taken). The spawn tool treats each of them as a refused delegation: a
/// base is a promise about history, and a child running on the wrong one is
/// worse than no child.
pub fn worktree_add(dir: &Path, id: u64, base: Option<&str>) -> Result<(PathBuf, String), String> {
    // The two states that refuse before any name matters are one question, and
    // [`can_branch_from`] is its one home: a directory git cannot answer for,
    // and a repository with no commit for a branch to fork from. Asking it
    // first makes the sentence written for the state the one a human reads,
    // whatever base was named (finding F16).
    can_branch_from(dir)?;
    let path = worktree_path(dir, id);
    let branch = branch_name(id);
    // A path git cannot be given is refused *before* anything is created: the
    // empty string this used to fall through to (`to_str().unwrap_or("")`) made
    // `git worktree add -b mush/<id> "" HEAD` create the branch and then die on
    // git's own assertion — a partial add whose id the caller has to keep
    // either way (finding F10).
    let Some(path_arg) = path.to_str() else {
        return Err(format!(
            "cannot create a worktree at `{}`: the path is not valid UTF-8, and git cannot be given it",
            path.display()
        ));
    };
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
            path_arg,
            base.unwrap_or("HEAD"),
        ],
    )?;
    // A worktree is a checkout of *refs*, and git's `worktree add` does not
    // populate submodules — there is no flag to ask for it (`git worktree add
    // -h` has no submodule option on git 2.55). A base tree that records a
    // submodule leaves an empty directory in the new checkout while `git
    // status` stays clean, so a child told to build or test pays for a tree it
    // was never told is incomplete (finding F5). The contents are brought in
    // here, at the one moment mush owns the checkout.
    populate_submodules(&path);
    Ok((path, branch))
}

/// Bring the new checkout's submodules in, when its tree records any.
///
/// `git worktree add` copies refs, not submodule contents: a base tree that
/// records one at `lib/sub` leaves that directory empty in the new worktree,
/// and `git status --porcelain` is empty — a tracked directory that is not
/// populated is not a change git reports (measured: `worktree_add` for a
/// repository with one local submodule leaves `lib/sub` empty and the status
/// clean, finding F5). `git submodule update --init --recursive` is git's own
/// road for filling it, run only when the checkout carries `.gitmodules`, so an
/// ordinary spawn spends no process on the question.
///
/// Deliberately best-effort: the branch and the refs are right, and a submodule
/// that cannot be fetched (no network, a private remote, a protocol the human's
/// git refuses) is a fact the child can act on — its prompt names the road it
/// would run by hand — never a reason to lose the worktree a spawn is standing
/// on. There is no surface here to *name* the failure on: the answer a caller
/// gets back is the path and the branch, and the one who reads the empty
/// directory is the child.
fn populate_submodules(worktree: &Path) {
    if !worktree.join(".gitmodules").is_file() {
        return;
    }
    let _ = run(worktree, &["submodule", "update", "--init", "--recursive"]);
}

/// Which of the two ways a branch adds nothing to its base: what one word,
/// "merged", used to say about both, and the two stories that were really
/// there — a run whose work landed, and a run that never committed at all.
///
/// The two are what the reclamation's two questions come to, and neither
/// implies the other:
///
/// * [`Landing::Merged`] — the branch has commits of its own and the base now
///   contains them (a human ran `git merge mush/<id>`, or a nested child's
///   parent merged it), so the branch ref is redundant;
/// * [`Landing::NothingCommitted`] — the branch *is* its fork revision: the
///   worktree was created at that commit and the run never made another, so
///   there is nothing for the base to contain. An ordinary read-only child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landing {
    /// The branch's own commits are in the base: merging it put them there.
    /// This is also — see [`reclaimable`] — the answer when the fork revision
    /// is unknown: the branch adds nothing to the base, and without the fork
    /// revision the two landings are the same git shape, so mush keeps the
    /// answer it has always given rather than guessing the other one.
    Merged,
    /// The branch adds no commit of its own to the fork revision: there was
    /// never anything to merge.
    NothingCommitted,
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
    /// The branch adds nothing to its base — [`Landing`] says which of the two
    /// ways, because "merged" was painted over both of them — and the checkout
    /// has nothing uncommitted: [`reclaim`] removes both.
    Landable(Landing),
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
    /// `landing` is which of the two nothings the branch was, so a caller's row
    /// can say what actually happened to that work ([`Landing`]).
    Removed {
        branch_kept: Option<String>,
        landing: Landing,
    },
    /// Left alone, for the reason `why` names.
    Kept(String),
    /// Neither checkout nor branch: there was nothing to reclaim.
    Nothing,
}

/// Whether the worktree of agent `id` can be reclaimed right now, read-only.
///
/// `base` is the *name* of the ref the branch's work has to land in before its
/// worktree can go — the parent agent's branch, or `HEAD` for a child of the
/// root, the one derivation the actor and the UI share (finding F9) — and it
/// stays a name because the first question is about the base *now*: a hand
/// merge moves the base's tip onto the branch's work, and that is the state a
/// sweep is looking for.
///
/// The spawn's fork may have named another ref (the caller's `base` argument);
/// that is the history the branch was built on, not the question a removal
/// asks. The removal asks whether the parent's tree already has the work, and
/// for a nested child the parent's branch is where the merge happens.
///
/// `fork` is the revision the worktree was created at, when the caller knows
/// it. It answers the second question a removal needs and neither the base name
/// nor the branch can: a branch with no commit of its own *is* its fork
/// revision, so a base that moved on since the spawn leaves it an ancestor of
/// the base for free — the same git shape as a merged branch, and the shape an
/// ordinary read-only child has. With `fork`, the two are told apart
/// ([`Landing`]); with `None` — a leftover discovered at startup, or an agent
/// restored from a session that never stored a base — mush keeps the single
/// answer it has always given rather than inventing a second one out of
/// information it does not have.
///
/// The removal test errs on the conservative side elsewhere too: a squashed or
/// cherry-picked copy of the work is not an ancestor of the base, so mush keeps
/// the branch and says why rather than guessing that the work landed.
/// **An unmerged branch is never deleted**, and nothing here merges anything.
pub fn reclaimable(root: &Path, id: u64, base: &str, fork: Option<&str>) -> Reclaimable {
    // The base is a caller's name and may begin with `-`; resolving it to a
    // commit id is the one way a name is allowed near a command line
    // (`branch_stat` says the same about its two names).
    let Some(base_sha) = resolve(root, base) else {
        return Reclaimable::Kept(format!(
            "{base} is not a revision mush can resolve — nothing can be shown merged into it"
        ));
    };
    probe(root, id, base, &base_sha, fork)
}

/// The rule itself, with the base already resolved to a commit id: one home for
/// it, and one process saved per worktree when a caller asks about many of them
/// against one base ([`unlandable`]).
fn probe(root: &Path, id: u64, base: &str, base_sha: &str, fork: Option<&str>) -> Reclaimable {
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
        // A checkout git still has something in is kept, whatever the branch
        // says: the branch may be merged and the paths may be the only copy of
        // the run's work. The cost is real — a child that merely compiled into
        // `target/` keeps its checkout until the human discards it, spending one
        // of the `MAX_WORKTREES` slots that bound the disk — and it is the trade
        // H10 already makes everywhere else: a bounded, visible cost against a
        // silent deletion of work nothing can account for (finding F1).
        match changes(&worktree_path(root, id)) {
            Some(found) if found.is_empty() => {}
            Some(found) => return Reclaimable::Kept(kept_checkout(&rel, &found)),
            None => {
                return Reclaimable::Kept(format!("{rel} — git could not say whether it is clean"))
            }
        }
    }
    Reclaimable::Landable(landing(root, &branch, fork))
}

/// Which nothing the branch is: the second question, asked only once the first
/// one said the branch may go.
///
/// The fork revision is the commit the worktree was created at (`worktree_add`
/// put the new branch there, and its caller resolves it in the new checkout). A
/// branch that still stands on it has no commit of its own and never gave the
/// base anything to contain; a branch that carries anything else has commits
/// the base now holds.
///
/// With no fork revision, the answer is [`Landing::Merged`]. The two states are
/// the same shape to a base resolved from a name — a branch the base already
/// contains — and the missing revision is the only thing that could tell them
/// apart, so a "nothing committed" verdict would be a guess. It is the guess
/// that costs most: it erases a real run's work from the row, and the restored
/// agent and the leftover that reach this branch are exactly the cases where the
/// branch may be the last record of that work. `Merged` keeps the answer mush
/// has always given, which says only what git did measure: the base needs none
/// of this branch.
fn landing(root: &Path, branch: &str, fork: Option<&str>) -> Landing {
    let Some(fork) = fork else {
        return Landing::Merged;
    };
    match ahead_of(root, branch, fork) {
        Some(0) => Landing::NothingCommitted,
        // A commit of its own in a base the first question already said
        // contains it. `None` — git refusing to answer the second question —
        // lands here too: the branch may still go (the base needs none of it),
        // and a refusal to prove "nothing was committed" is not proof of it.
        _ => Landing::Merged,
    }
}

/// How many commits `branch` carries that `base` does not: `0` is "everything
/// this branch added is in `base`", the one measurement both reclamation
/// questions are built from. `None` is git refusing to answer, which is never a
/// `0`.
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
/// adds nothing to `base` (its work is merged, or the run never committed;
/// [`Reclaimed::Removed`] says which), the checkout is clean, and nothing
/// unmerged or dirty is ever touched.
///
/// This is the one place mush removes a worktree or deletes a branch outside its
/// own tests, so the two refusals it is built from are the whole of the
/// guarantee: `git worktree remove --force` (the `--force` is for git's own
/// lock-file bookkeeping, not for a dirty checkout — that case never gets here)
/// and `git branch -d`, never `-D`, so a branch git will not certify as deleted
/// is a branch mush leaves alone (finding H10).
pub fn reclaim(root: &Path, id: u64, base: &str, fork: Option<&str>) -> Reclaimed {
    match reclaimable(root, id, base, fork) {
        Reclaimable::Nothing => Reclaimed::Nothing,
        Reclaimable::Kept(why) => Reclaimed::Kept(why),
        Reclaimable::Landable(landing) => remove(root, id, landing),
    }
}

/// Take a checkout [`probe`] called landable, checkout first and branch second:
/// git refuses to delete a branch that is checked out anywhere, so the order is
/// not a preference. A removal that fails leaves the branch alone — nothing
/// happened, and the caller must not read it as `Removed`.
fn remove(root: &Path, id: u64, landing: Landing) -> Reclaimed {
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
    Reclaimed::Removed {
        branch_kept,
        landing,
    }
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
        .filter(|id| {
            matches!(
                probe(root, *id, "HEAD", &base_sha, None),
                Reclaimable::Kept(_)
            )
        })
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

/// What [`commit_all`] found in the worktree it was given: the two answers a
/// put-away commit can have, plus the third one that used to be silent.
///
/// The `None` this replaces said "the run changed nothing" about a checkout
/// whose only new files matched the repository's own `.gitignore`, and the sweep
/// then deleted them with the checkout (finding F1). "Nothing changed" must
/// never be said about a run that changed the filesystem, so paths a commit
/// cannot keep are their own answer and a caller has to name them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Commit {
    /// The work is committed. The short revision, for the row.
    Made(String),
    /// Nothing changed at all: no tracked edit, no untracked file, no ignored
    /// path.
    Nothing,
    /// The only paths git saw are ones it ignores, so there was nothing to
    /// commit and nothing a commit could keep. Named, so a row can say where the
    /// deliverable is.
    Ignored(Vec<String>),
}

/// Commit everything in the worktree `dir` under `subject`, and answer what was
/// there — the short revision, [`Commit::Nothing`] when the run changed nothing
/// at all, or the ignored paths when those are all it changed (finding F1).
///
/// It refuses to commit anywhere but the worktree it was given. `git -C <dir>`
/// walks up to the enclosing repository, so a directory under `.mush/wt` that is
/// no longer a worktree — a hand `git worktree remove`, then a write from a run
/// that was already in flight — is an ordinary directory inside the human's
/// checkout: committing there would stage and commit the human's own work, on
/// the human's branch, under mush's subject and identity (finding F8). `dir`
/// must be the root of its own working tree; anything else is refused as
/// "`<dir>` is no longer a worktree" and never commits somewhere up the tree.
///
/// The identity, the message and the *signature* are supplied here (`-c
/// user.name=…`, `-c user.email=…`, `-c commit.gpgsign=false`, `--no-verify`),
/// so a commit does not depend on the human's identity, never runs their
/// commit hooks, and never signs. Signing is configuration, not a hook, and it
/// is the one part of the human's git setup a put-away commit cannot inherit: a
/// machine that signs every commit by default (`commit.gpgsign=true`, in a
/// config or in the repository) has no key here, so without the flag every
/// isolated run ended as uncommitted work — the parent read an error instead of
/// a revision, the worktree was correctly kept for holding it, and the count
/// that refuses spawns at [`MAX_WORKTREES`] rose by one per child (finding
/// F2). The index is that worktree's own, so a commit here cannot touch the
/// human's index either.
pub fn commit_all(dir: &Path, subject: &str) -> Result<Commit, String> {
    if !is_its_own_worktree(dir) {
        return Err(format!("{} is no longer a worktree", dir.display()));
    }
    let found =
        changes(dir).ok_or_else(|| format!("git could not read {} for a commit", dir.display()))?;
    if found.is_empty() {
        return Ok(Commit::Nothing);
    }
    if found.iter().all(|change| change.ignored) {
        return Ok(Commit::Ignored(
            found.iter().map(|change| change.path.clone()).collect(),
        ));
    }
    run(dir, &["add", "-A"])?;
    run(
        dir,
        &[
            "-c",
            "user.name=mush",
            "-c",
            "user.email=mush@local",
            // The one config key a commit must not inherit: a machine that
            // signs by default fails a commit it cannot sign, and this commit
            // has no key and no human to ask for one (finding F2). A `-c`
            // outranks both the repository's and the human's config, so the
            // signing policy never decides whether a run's work is kept.
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--no-verify",
            "-qm",
            subject,
        ],
    )?;
    Ok(Commit::Made(run(dir, &["rev-parse", "--short", "HEAD"])?))
}

/// Whether `dir` is the root of its own working tree — the question [`commit_all`]
/// has to ask before `git -C` goes looking upward for a repository. A linked
/// worktree's `.git` is a file, so its presence is the first half; the second is
/// git's own answer, because a subdirectory of a checkout has a `.git` somewhere
/// above it and would otherwise commit into that repository (finding F8).
fn is_its_own_worktree(dir: &Path) -> bool {
    if !dir.join(".git").exists() {
        return false;
    }
    let Some(top) = git(dir, &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    match (std::fs::canonicalize(dir), std::fs::canonicalize(&top)) {
        (Ok(dir), Ok(top)) => dir == top,
        _ => false,
    }
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
        // The top of the id space has no floor above it, so it names no child
        // mush could hold; one past the space reads as no name at all.
        assert_eq!(worktree_id("mush/18446744073709551615"), None);
        assert_eq!(worktree_id("mush/18446744073709551616"), None);
        assert_eq!(worktree_id("mush/18446744073709551614"), Some(u64::MAX - 1));

        // The namespace and the id are two questions: a refusable name is still
        // in mush's namespace, and a branch outside it never was mush's.
        assert!(is_child_branch("mush/9"));
        assert!(is_child_branch("mush/18446744073709551615"));
        assert!(is_child_branch("mush/x"));
        assert!(!is_child_branch("main"));
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
        let made = commit_all(&path, "mush #5: do the thing").unwrap();
        assert!(
            matches!(made, Commit::Made(_)),
            "the worktree had work to commit: {made:?}"
        );
        assert_eq!(
            subject_of(&path, "HEAD").as_deref(),
            Some("mush #5: do the thing")
        );
        assert_eq!(
            commit_all(&path, "mush #5: do the thing").unwrap(),
            Commit::Nothing,
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
        // With a base revision named, the refusal is the same: the
        // repository's question outranks the base's name, so the human reads
        // the sentence written for this case and not git's own about the
        // revision (finding F16).
        assert_eq!(
            worktree_add(&unborn, 7, Some("HEAD")).unwrap_err(),
            "the repo has no commits yet — commit first or drop isolated"
        );
        let _ = fs::remove_dir_all(&unborn);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A repository with no commit refuses *a base spawn too* with the sentence
    /// written for it, and spends no process on resolving a base that could not
    /// exist: the gate is the repository's own state, asked before git is made
    /// to look at the name — and the production road asks the same gate, in the
    /// same words, before it resolves the name (`spawn_tool`'s residual, fixed
    /// on the actor's side of the door).
    #[test]
    fn a_base_worktree_in_a_repo_without_commits_refuses_with_that_reason() {
        let unborn =
            std::env::temp_dir().join(format!("mush-git-unborn-base-{}", std::process::id()));
        let _ = fs::remove_dir_all(&unborn);
        fs::create_dir_all(&unborn).unwrap();
        run(&unborn, &["init", "-q"]).unwrap();
        assert_eq!(has_commits(&unborn), Some(false));

        assert_eq!(
            worktree_add(&unborn, 9, Some("HEAD")).unwrap_err(),
            "the repo has no commits yet — commit first or drop isolated"
        );
        assert_eq!(
            worktree_add(&unborn, 9, None).unwrap_err(),
            "the repo has no commits yet — commit first or drop isolated"
        );
        assert!(
            !unborn.join(".mush").exists(),
            "and nothing was made before the refusal"
        );
        let _ = fs::remove_dir_all(&unborn);
    }

    /// A worktree is a checkout of refs, and git's `worktree add` does not
    /// populate submodules: a repository whose base tree records one gets an
    /// empty directory in the child's checkout with a clean `git status`, and
    /// the child is told to build in it (finding F5). The add fills it.
    #[test]
    fn a_submodule_repo_gets_its_submodules_in_the_new_worktree() {
        let source = init_repo("submodule-source");
        fs::write(source.join("s.txt"), "the submodule's file\n").unwrap();
        run(&source, &["add", "-A"]).unwrap();
        run(&source, &["commit", "-qm", "the submodule"]).unwrap();

        let dir = init_repo("submodule");
        run(
            &dir,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                source.to_str().unwrap(),
                "lib/sub",
            ],
        )
        .unwrap();
        run(&dir, &["commit", "-qm", "add the submodule"]).unwrap();

        // A local submodule clones over the `file` transport, which git
        // refuses for submodules unless the human says otherwise; the test is
        // that human, through git's own road for saying it. mush itself never
        // sets this: the protocol policy in a child's checkout is the human's
        // (a repository must not be able to make mush clone a local path).
        let previous = std::env::var_os("GIT_ALLOW_PROTOCOL");
        std::env::set_var("GIT_ALLOW_PROTOCOL", "file");
        let added = worktree_add(&dir, 6, Some("HEAD"));
        match previous {
            Some(value) => std::env::set_var("GIT_ALLOW_PROTOCOL", value),
            None => std::env::remove_var("GIT_ALLOW_PROTOCOL"),
        }
        let (path, _branch) = added.unwrap();

        assert_eq!(
            fs::read_to_string(path.join("lib/sub/s.txt")).unwrap(),
            "the submodule's file\n",
            "the new checkout carries what the base tree records"
        );
        assert!(
            changes(&path).unwrap().is_empty(),
            "and the populated submodule is not a change git reports"
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&source);
    }

    /// A put-away commit carries its own identity and its own answer to
    /// signing: a machine whose git signs every commit by default — and has no
    /// key to sign with, which is what a missing signer is — must not be able
    /// to stop a run's work from landing (finding F2).
    #[test]
    fn a_signing_config_does_not_stop_the_commit() {
        let dir = init_repo("gpgsign");
        run(&dir, &["config", "commit.gpgsign", "true"]).unwrap();
        // The signer is not there, the shape a machine with no key has.
        run(&dir, &["config", "gpg.program", "/nonexistent/mush-no-gpg"]).unwrap();
        let (path, _branch) = worktree_add(&dir, 8, Some("HEAD")).unwrap();
        fs::write(path.join("work.txt"), "the work\n").unwrap();

        let made = commit_all(&path, "mush #8: port the parser").unwrap();
        assert!(
            matches!(made, Commit::Made(_)),
            "the work is committed, not left behind by a signing config: {made:?}"
        );
        assert_eq!(
            subject_of(&path, "HEAD").as_deref(),
            Some("mush #8: port the parser")
        );
        assert!(
            changes(&path).unwrap().is_empty(),
            "and the worktree is clean afterwards"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A hook is git's own way for a child to say what environment git handed
    /// it, and it is the evidence that the git children are started through
    /// [`crate::secrets::scrub`] too (finding C1). The hook's `printenv` is
    /// written to a file and the hook then exits on `true`, so the commit
    /// succeeds whatever the probe found — the file is the assertion, not the
    /// exit status. `core.hooksPath` is pointed at a directory the test owns,
    /// because a machine-wide hooks path would otherwise decide what runs.
    #[cfg(unix)]
    #[test]
    fn a_git_child_never_sees_mushs_key() {
        use std::os::unix::fs::PermissionsExt;

        let dir = init_repo("hook-env");
        let hooks = dir.join("test-hooks");
        fs::create_dir_all(&hooks).unwrap();
        run(&dir, &["config", "core.hooksPath", hooks.to_str().unwrap()]).unwrap();
        let seen = dir.join("seen");
        let hook = hooks.join("pre-commit");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintenv MUSH_API_KEY > {}\ntrue\n",
                seen.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

        // The key is put back on the way out, so a failure here does not turn
        // this binary's other tests into readers of a probe credential.
        let previous = std::env::var_os("MUSH_API_KEY");
        std::env::set_var("MUSH_API_KEY", "sk-probe-inheritance-0123456789");
        fs::write(dir.join("b.txt"), "two\n").unwrap();
        run(&dir, &["add", "-A"]).unwrap();
        run(&dir, &["commit", "-qm", "with a hook"]).unwrap();
        match previous {
            Some(previous) => std::env::set_var("MUSH_API_KEY", previous),
            None => std::env::remove_var("MUSH_API_KEY"),
        }

        let read = fs::read_to_string(&seen).unwrap();
        assert_eq!(
            read, "",
            "the git child handed mush's credential to its hook: {read:?}"
        );
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
    /// leaves behind. Returns the fork revision, read the way `spawn_tool`
    /// reads it: the new checkout's `HEAD`, before the run commits anything.
    fn isolated_worktree(dir: &Path, id: u64) -> String {
        worktree_add(dir, id, Some("HEAD")).unwrap();
        let path = worktree_path(dir, id);
        let fork = resolve(&path, "HEAD").expect("a fresh checkout has a HEAD");
        // The content is the id's: a worktree forked from a base that already
        // merged an earlier test's `work.txt` would otherwise be clean, and
        // `commit_all` would answer `Commit::Nothing` — which is the
        // *other* test's case.
        fs::write(path.join("work.txt"), format!("work {id}\n")).unwrap();
        assert!(
            matches!(
                commit_all(&path, &format!("mush #{id}: work")).unwrap(),
                Commit::Made(_)
            ),
            "the run's work must really be committed"
        );
        fork
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
        let fork = isolated_worktree(&dir, 1);
        merge_into_head(&dir, "mush/1");
        let landed = resolve(&dir, "HEAD").unwrap();

        assert_eq!(
            reclaim(&dir, 1, "HEAD", Some(&fork)),
            Reclaimed::Removed {
                branch_kept: None,
                landing: Landing::Merged,
            }
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
    /// worktree behind for good. The landing it reports is `NothingCommitted`,
    /// because the fork revision says so — the branch never became more than
    /// the commit the worktree was created at.
    #[test]
    fn a_clean_run_that_committed_nothing_is_reclaimed() {
        let dir = init_repo("reclaim-clean");
        worktree_add(&dir, 4, Some("HEAD")).unwrap();
        let fork = resolve(&worktree_path(&dir, 4), "HEAD").unwrap();
        // Nothing written and nothing committed: `mush/4` *is* its fork
        // revision, which is one half of the pair a `merged` word used to
        // cover.

        assert_eq!(
            reclaim(&dir, 4, "HEAD", Some(&fork)),
            Reclaimed::Removed {
                branch_kept: None,
                landing: Landing::NothingCommitted,
            }
        );
        assert!(!worktree_path(&dir, 4).exists());
        assert_eq!(resolve(&dir, "mush/4"), None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A base *name* that moved on after the spawn: the branch never committed,
    /// so it is an ancestor of the new base tip for free — the same git shape a
    /// hand merge leaves — and only the fork revision tells the two apart. The
    /// row's answer is "nothing committed", never "merged".
    #[test]
    fn a_base_that_moved_on_after_the_spawn_still_reads_nothing_committed() {
        let dir = init_repo("reclaim-base-moved");
        worktree_add(&dir, 4, Some("HEAD")).unwrap();
        let fork = resolve(&worktree_path(&dir, 4), "HEAD").unwrap();
        // The base moves on the way HEAD does under a session: a commit of the
        // human's own, nothing the run ever did.
        fs::write(dir.join("later.txt"), "the human's own work\n").unwrap();
        assert!(matches!(
            commit_all(&dir, "the human's own work").unwrap(),
            Commit::Made(_)
        ));
        assert_ne!(resolve(&dir, "HEAD").unwrap(), fork, "the base moved on");

        assert_eq!(
            reclaimable(&dir, 4, "HEAD", Some(&fork)),
            Reclaimable::Landable(Landing::NothingCommitted),
            "the branch is its fork revision; nobody merged anything"
        );
        // Without the fork revision the repository is the other shape, and mush
        // keeps the answer it has always given rather than guessing (see
        // `reclaimable`).
        assert_eq!(
            reclaimable(&dir, 4, "HEAD", None),
            Reclaimable::Landable(Landing::Merged),
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The rule the whole patch is built on: an unmerged branch is **kept**, and
    /// the refusal names it. `branch -D` is never run, so the commit is still
    /// there, reachable from its branch, with its checkout on disk — the test's
    /// point is not the return value but what is left in the repository.
    #[test]
    fn an_unmerged_branch_is_kept_and_named() {
        let dir = init_repo("reclaim-unmerged");
        let fork = isolated_worktree(&dir, 2);
        let tip = resolve(&dir, "mush/2").unwrap();

        match reclaim(&dir, 2, "HEAD", Some(&fork)) {
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
        let fork = isolated_worktree(&dir, 3);
        merge_into_head(&dir, "mush/3");
        fs::write(worktree_path(&dir, 3).join("work.txt"), "edited again\n").unwrap();

        match reclaim(&dir, 3, "HEAD", Some(&fork)) {
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
        assert_eq!(reclaimable(&dir, 9, "HEAD", None), Reclaimable::Nothing);
        assert_eq!(reclaim(&dir, 9, "HEAD", None), Reclaimed::Nothing);

        worktree_add(&dir, 9, Some("HEAD")).unwrap();
        match reclaim(&dir, 9, "no-such-base", None) {
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
        let fork = isolated_worktree(&dir, 5);
        merge_into_head(&dir, "mush/5");
        fs::remove_dir_all(worktree_path(&dir, 5)).unwrap();

        assert_eq!(
            reclaim(&dir, 5, "HEAD", Some(&fork)),
            Reclaimed::Removed {
                branch_kept: None,
                landing: Landing::Merged,
            }
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
        let fork = isolated_worktree(&dir, 6);
        fs::remove_dir_all(worktree_path(&dir, 6)).unwrap();

        match reclaim(&dir, 6, "HEAD", Some(&fork)) {
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
        // The fork revision, read the way `spawn_tool` reads it: question 2 is
        // about this revision, and question 1 is about `mush/9` — the parent's
        // branch, never HEAD.
        let fork = resolve(&child, "HEAD").unwrap();
        fs::write(child.join("child.txt"), "child\n").unwrap();
        commit_all(&child, "mush #10: child work").unwrap();
        git_in(&parent, &["merge", "--no-edit", "mush/10"]);

        assert_eq!(
            reclaimable(&dir, 10, "mush/9", Some(&fork)),
            Reclaimable::Landable(Landing::Merged),
            "a child's work merged into its parent's branch reads merged, and \
             the base it is measured against is the parent's branch — not HEAD"
        );

        match reclaim(&dir, 10, "mush/9", Some(&fork)) {
            Reclaimed::Removed {
                branch_kept,
                landing,
            } => {
                assert_eq!(
                    branch_kept.as_deref(),
                    Some("mush/10"),
                    "the ref git would not delete is named, not forced"
                );
                assert_eq!(landing, Landing::Merged);
            }
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

    /// A name list for a row's sentence: the first two, and a count for the
    /// rest — a `target/` with a hundred thousand files under it must not become
    /// a paragraph (finding F1).
    #[test]
    fn a_name_list_stays_a_sentence() {
        assert_eq!(named_paths(&[]), "");
        assert_eq!(named_paths(&["a.txt".into()]), "a.txt");
        assert_eq!(
            named_paths(&["a.txt".into(), "b.txt".into()]),
            "a.txt, b.txt"
        );
        assert_eq!(
            named_paths(&["a".into(), "b".into(), "c".into(), "d".into()]),
            "a, b and 2 more"
        );
    }

    /// A run whose only work is in an ignored path: `git status --porcelain`
    /// calls the worktree clean, the sweep removes it, and the run's only copy
    /// goes with it. The reading is `--ignored=matching`; `commit_all` says the
    /// paths instead of "nothing changed"; the sweep keeps what it cannot
    /// account for and names it (finding F1).
    #[test]
    fn an_ignored_only_worktree_is_kept_and_named() {
        let dir = init_repo("ignored-only");
        // `/ignored/*` ignores the directory's *contents*, so the porcelain
        // names the file a human would go looking for. (`/ignored/` names the
        // directory instead: one line for a `target/`-sized tree, and the
        // paths a commit cannot keep are the ignored boundary either way.)
        fs::write(dir.join(".gitignore"), "/ignored/*\n*.log\n").unwrap();
        git_in(&dir, &["add", ".gitignore"]);
        git_in(&dir, &["commit", "-qm", "ignore the run's output"]);
        worktree_add(&dir, 1, Some("HEAD")).unwrap();
        let path = worktree_path(&dir, 1);
        let fork = resolve(&path, "HEAD").unwrap();
        fs::create_dir_all(path.join("ignored")).unwrap();
        fs::write(path.join("ignored/report.txt"), "the only copy\n").unwrap();
        fs::write(path.join("run.log"), "log\n").unwrap();

        // The run changed the filesystem, so the answer is not "nothing".
        match commit_all(&path, "mush #1: the task").unwrap() {
            Commit::Ignored(paths) => {
                assert!(
                    paths.contains(&"ignored/report.txt".to_string()),
                    "the answer names the deliverable: {paths:?}"
                );
                assert!(paths.contains(&"run.log".to_string()), "{paths:?}");
            }
            other => panic!("the only work is ignored, so nothing was committed: {other:?}"),
        }
        assert_eq!(
            resolve(&path, "HEAD").unwrap(),
            fork,
            "and no commit was made"
        );

        // Kept, with a sentence that names where the work is.
        match reclaimable(&dir, 1, "HEAD", Some(&fork)) {
            Reclaimable::Kept(why) => {
                assert!(
                    why.contains("ignored/report.txt"),
                    "the sentence names it: {why}"
                );
                assert!(why.contains("run.log"), "{why}");
            }
            other => panic!("ignored-only work must be kept, got {other:?}"),
        }
        match reclaim(&dir, 1, "HEAD", Some(&fork)) {
            Reclaimed::Kept(why) => assert!(why.contains("ignored/report.txt"), "{why}"),
            other => panic!("ignored-only work must be kept, got {other:?}"),
        }
        assert!(
            path.join("ignored/report.txt").exists(),
            "the only copy is still there"
        );
        assert!(path.join("run.log").exists());
        assert!(resolve(&dir, "mush/1").is_some(), "and so is the branch");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A workspace that is no longer a worktree is not a place to commit: `git
    /// -C` would walk up to the enclosing repository and stage and commit the
    /// human's own modified and untracked files, on the human's branch, under
    /// mush's subject and identity (finding F8). The refusal names the
    /// directory, and the human's checkout is exactly as it was.
    #[test]
    fn a_commit_never_leaves_the_worktree() {
        let dir = init_repo("commit-guard");
        fs::write(dir.join("a.txt"), "the human's half-finished edit\n").unwrap();
        fs::write(dir.join("notes.txt"), "the human's untracked file\n").unwrap();
        let head = resolve(&dir, "HEAD").unwrap();
        let before = git(&dir, &["status", "--porcelain"]).unwrap();

        // What a missed `git worktree remove` leaves, and what a late write
        // from a run already in flight recreates: a plain directory under
        // `.mush/wt`, which is inside the human's checkout.
        let plain = worktree_path(&dir, 1);
        fs::create_dir_all(&plain).unwrap();
        assert!(
            !plain.join(".git").exists(),
            "the fixture is a plain directory"
        );
        let error = commit_all(&plain, "mush #1: the brief").unwrap_err();
        assert!(error.contains("is no longer a worktree"), "{error}");
        assert!(
            error.contains(".mush/wt/1"),
            "and it names the directory: {error}"
        );

        // A subdirectory of a checkout is the same shape one level in.
        let sub = dir.join("src");
        fs::create_dir_all(&sub).unwrap();
        let error = commit_all(&sub, "mush #1: the brief").unwrap_err();
        assert!(error.contains("is no longer a worktree"), "{error}");

        assert_eq!(
            resolve(&dir, "HEAD").unwrap(),
            head,
            "the human's HEAD did not move"
        );
        assert_eq!(
            git(&dir, &["status", "--porcelain"]).unwrap(),
            before,
            "and their index is untouched"
        );
        assert_ne!(
            subject_of(&dir, "HEAD").as_deref(),
            Some("mush #1: the brief"),
            "no commit carries mush's subject on the human's branch"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// An isolated spawn below the repository root: the workspace is `repo/sub`,
    /// and the same call `spawn_tool` makes creates `repo/sub/.mush/wt/<id>` on
    /// `mush/<id>`, forked from the base. The `.git` test this replaces refused
    /// it with "not a git repository" about a directory git answers every
    /// question inside (finding F11).
    #[test]
    fn an_isolated_spawn_works_below_the_repository_root() {
        let repo = init_repo("spawn-below-root");
        let sub = repo.join("crates").join("mush");
        fs::create_dir_all(&sub).unwrap();
        let base = resolve(&sub, "HEAD").unwrap();

        let (path, name) = worktree_add(&sub, 1, Some("HEAD")).unwrap();
        assert_eq!(
            path,
            sub.join(".mush/wt/1"),
            "the checkout is under the workspace, not the repository root"
        );
        assert_eq!(name, "mush/1");
        assert!(path.join(".git").exists(), "it is a real checkout");
        assert_eq!(
            resolve(&path, "HEAD").unwrap(),
            base,
            "forked from the base git resolved in the workspace"
        );
        assert_eq!(branch(&path).as_deref(), Some("mush/1"));

        // A directory that is not in any repository keeps its own sentence.
        let plain =
            std::env::temp_dir().join(format!("mush-git-below-plain-{}", std::process::id()));
        let _ = fs::remove_dir_all(&plain);
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(
            worktree_add(&plain, 2, None).unwrap_err(),
            "not a git repository"
        );
        let _ = fs::remove_dir_all(&plain);
        let _ = fs::remove_dir_all(&repo);
    }
}
