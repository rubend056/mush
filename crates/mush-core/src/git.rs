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
///
/// The one invocation style for mutating verbs: `-C` so the caller names the
/// repository, and `LC_ALL=C` so a conflict or error reads the same everywhere.
pub fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
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
            format!("git {} failed", args.first().unwrap_or(&""))
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
    let porcelain = git(dir, &["status", "--porcelain"])?;
    let dirty = porcelain
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    Some(RepoStatus {
        branch: branch(dir).unwrap_or_default(),
        dirty,
        stat: diff_stat(dir, &["diff", "--shortstat", "HEAD"]).unwrap_or_default(),
    })
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

/// The worktree of agent `id`: `<root>/.mush/wt/<id>`.
pub fn worktree_path(root: &Path, id: u64) -> PathBuf {
    root.join(format!("{WORKTREE_DIR}/{id}"))
}

/// The branch an isolated agent's worktree is checked out on: `mush/<id>`.
/// Mush creates it and mush reclaims it, so its name is not the human's to
/// choose — [`worktree_id`] reads the id back out of it.
pub fn branch_name(id: u64) -> String {
    format!("mush/{id}")
}

/// The agent id in a `mush/<id>` branch name, `None` for any other name. A
/// branch the human made by hand must not be adopted as mush's leftover, so
/// everything that is not exactly this shape stays unnamed.
pub fn worktree_id(branch: &str) -> Option<u64> {
    branch.strip_prefix("mush/")?.parse().ok()
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
/// itself fails (an id whose branch or directory is still taken). Callers treat
/// every one of them as "isolate in place" rather than as a failed delegation.
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
    run(
        dir,
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
    .map_err(|error| {
        // `run` names the verb it failed at, and a silent failure reads
        // "git worktree failed"; the human needs the subcommand that failed.
        if error == "git worktree failed" {
            "git worktree add failed".to_string()
        } else {
            error
        }
    })
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
    parse_shortstat(&text)
}

/// Parse `git diff --shortstat`: ` 3 files changed, 12 insertions(+), 4 deletions(-)`.
/// Any part may be missing — git only prints the lines that apply — and an
/// empty string is a clean tree.
pub fn parse_shortstat(text: &str) -> Option<Stat> {
    if text.trim().is_empty() {
        return Some(Stat::default());
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
    Some(stat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    #[test]
    fn shortstat_parses_every_shape() {
        assert_eq!(parse_shortstat("").unwrap(), Stat::default());
        assert_eq!(
            parse_shortstat(" 1 file changed, 1 insertion(+)").unwrap(),
            Stat {
                files: 1,
                added: 1,
                removed: 0
            }
        );
        assert_eq!(
            parse_shortstat(" 3 files changed, 12 insertions(+), 4 deletions(-)").unwrap(),
            Stat {
                files: 3,
                added: 12,
                removed: 4
            }
        );
        assert_eq!(
            parse_shortstat(" 2 files changed, 5 deletions(-)").unwrap(),
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
        let stat = parse_shortstat(" 1 file changed, 5000000000 insertions(+)").unwrap();
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

    /// The porcelain listing has shapes a repository can really be in, and the
    /// parser has to survive all of them: a detached checkout (no branch line),
    /// a block split by blank lines, and a branch that is not mush's.
    #[test]
    fn porcelain_worktrees_parse_in_every_shape() {
        let text = "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\n\
                    worktree /repo/.mush/wt/3\nHEAD def\nbranch refs/heads/mush/3\n\n\
                    worktree /repo/detached\nHEAD 0123\ndetached\n\n\
                    worktree /repo/mine\nHEAD 4567\nbranch refs/heads/feature/x\n\n";
        let list = parse_worktrees(text);
        assert_eq!(list.len(), 4, "{list:?}");
        assert_eq!(list[0].path, PathBuf::from("/repo"));
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert_eq!(list[0].id, None, "main is not an agent's branch");
        assert_eq!(list[1].path, PathBuf::from("/repo/.mush/wt/3"));
        assert_eq!(list[1].branch.as_deref(), Some("mush/3"));
        assert_eq!(list[1].id, Some(3));
        assert_eq!(list[2].branch, None, "a detached worktree has no branch");
        assert_eq!(list[2].id, None);
        assert_eq!(list[3].id, None, "someone else's branch stays unnamed");
        // A trailing blank line, and a listing that is only whitespace.
        assert_eq!(parse_worktrees("\n\n").len(), 0);
        assert_eq!(parse_worktrees("").len(), 0);
    }

    /// The path and the branch are one formatting rule, and the id round-trips
    /// through the branch name: that is what lets a leftover be registered.
    #[test]
    fn the_path_and_branch_are_one_rule() {
        let root = Path::new("/repo");
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
}
