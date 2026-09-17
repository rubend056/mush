//! Reading the repository the way a glance needs it: which branch, how dirty,
//! how many lines. One `git` process per question, cached by the caller — the
//! UI never shells out while painting (docs/mush.md §8).
//!
//! Everything here is best-effort: a workspace that is not a repository, or a
//! `git` binary that is missing, answers `None` rather than failing a caller.

use std::path::Path;
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
        .map_err(|_| "git binary unavailable".to_string())?;
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
    let sha = commit(dir, name)?;
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
    let base = commit(dir, base)?;
    let branch = commit(dir, branch)?;
    diff_stat(dir, &["diff", "--shortstat", &format!("{base}...{branch}")])
}

/// A revision resolved to its commit id, or `None` when it does not exist.
/// The id is what gets passed on: it cannot be mistaken for an option.
fn commit(dir: &Path, name: &str) -> Option<String> {
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
