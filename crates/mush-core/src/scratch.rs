//! A test's own scratch root under the temp directory, removed when it drops.
//!
//! Every test that needs files on disk makes a root of its own, named
//! `mush-<label>-<pid>`: the label names the test, and the pid names the test
//! *binary*, so two tests in one binary cannot read each other's files (the
//! label is unique per binary — [`Scratch::new`] refuses a second live one) and
//! two worktrees' suites cannot either. The root is a directory under
//! [`std::env::temp_dir`], not a [`tempfile::TempDir`], because the tests that
//! use it pass the path to git, to worktrees and to a *second* process, and
//! those roads want a path that reads like a workspace, not an owned handle.
//!
//! # Why `Drop`, and not a removal at the end of the test
//!
//! A test that fails stops where it failed: every line after the failing
//! assertion — including any `remove_dir_all` written there — never runs, and
//! the files it made are left behind. A `Drop` does run while the test unwinds,
//! so the root goes whether the test passes or panics; that is the whole point
//! of the type, and the reason a caller must hold the guard (through the whole
//! test) rather than drop it early. The sweep ([`sweep_dead_roots`]) is the
//! second net: it reaps what a dead pid left — the roots, and the strays older
//! builds wrote — which covers a test process killed outright, where no `Drop`
//! runs at all.
//!
//! # Why this module is not `#[cfg(test)]`
//!
//! `#[cfg(test)]` is set for the crate's own test binary, not for a crate it is
//! a dependency of, so a `#[cfg(test)]` module here would be invisible to
//! `mush`'s tests — which hold most of the sites. The module is therefore behind
//! the `test-support` feature: the crate's own tests see it through `cfg(test)`,
//! and `mush`'s tests through a `dev-dependencies` entry that enables the
//! feature, so a product build (`cargo build`) never compiles it.

use std::ffi::OsStr;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};

/// A scratch root a test owns, removed when the guard drops.
///
/// `mush-<label>-<pid>` under the temp directory, created by [`Scratch::new`]
/// (an older root of the same name is removed first, so a pid reused by a
/// later run cannot merge two runs' files). The label must be unique among the
/// *live* guards of a test binary: two tests sharing one root would have the
/// first `Drop` delete the other's files, so a duplicate is refused with a
/// panic that names the label rather than allowed to flake later.
///
/// A test that only needs the path holds the guard itself; a helper that builds
/// an `App` or an `Actor` on a fresh root hands the value back inside
/// [`Held`], because the caller has no name for a guard the helper created.
#[must_use = "the root is removed when the guard drops, so it must live through the test"]
pub struct Scratch {
    label: String,
    root: PathBuf,
}

/// The labels of the live guards in this process, so a second one for the same
/// label is refused instead of quietly sharing a root. A `Vec` because a
/// process holds a handful of labels at a time and a `Vec::new` is the one
/// empty collection a `static` can be built from without a latch.
static LIVE: Mutex<Vec<String>> = Mutex::new(Vec::new());

impl Scratch {
    /// A root of its own: `mush-<label>-<pid>` under the temp directory.
    ///
    /// The label is the test's own name (a second live guard with the same
    /// label panics — see the type's doc), and it must not start with `cmd-`,
    /// which is the product's scratch-file prefix.
    pub fn new(label: &str) -> Scratch {
        let root = std::env::temp_dir().join(format!("mush-{label}-{}", std::process::id()));
        let mut live = LIVE.lock().unwrap_or_else(|poison| poison.into_inner());
        assert!(
            !live.iter().any(|held| held == label),
            "two live tests share the scratch label `{label}`: each test needs its own, or \
             the first one to end deletes the other's root"
        );
        live.push(label.to_string());
        drop(live);
        // The dead runs' roots go first, so this process starts from a temp
        // directory with its own litter only.
        sweep_once();
        // The root is ours now (the label above says so), so anything already
        // under this process's name is a stale run's — a pid reused by this
        // process, which the sweep cannot tell from a leftover because the pid
        // is alive again.
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a scratch root can be created");
        Scratch {
            label: label.to_string(),
            root,
        }
    }

    /// The root, for the callers that want the path spelled out.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// The value a test built on this root, together with the root: a helper
    /// that creates the root can return `scratch.hold(app)`, and the caller
    /// holds the guard without naming it.
    pub fn hold<T>(self, value: T) -> Held<T> {
        Held {
            value,
            scratch: self,
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let mut live = LIVE.lock().unwrap_or_else(|poison| poison.into_inner());
        live.retain(|held| held != &self.label);
    }
}

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.root
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.root
    }
}

/// So a root reads as a path to [`std::process::Command::arg`] too, which asks
/// for `AsRef<OsStr>`: a test that hands the root to `git -C` spells it the
/// same way it spells every other path.
impl AsRef<OsStr> for Scratch {
    fn as_ref(&self) -> &OsStr {
        self.root.as_os_str()
    }
}

/// A value built on a [`Scratch`], kept together with it.
///
/// A test helper that makes the root itself — `test_app`, `test_actor` and the
/// rest — cannot hand back a bare guard: the caller would have to name a value
/// the helper built, and every call site would grow a line for it. It hands
/// back the value *and* the guard instead, and the root outlives the value
/// because the guard is a field of the value's own return. Deref makes the
/// result read like the value it carries, so a call site does not change when
/// the helper starts guarding its root.
#[must_use = "the root is removed when the guard drops, so it must live through the test"]
pub struct Held<T> {
    /// Declared before the guard so it drops first: the files are gone before
    /// the root that holds them is.
    value: T,
    scratch: Scratch,
}

impl<T> Held<T> {
    /// The root this value was built on.
    pub fn path(&self) -> &Path {
        self.scratch.path()
    }

    /// The value and its guard, apart: for the callers that must move the value
    /// somewhere that cannot carry the guard (a function taking it by value);
    /// the caller then keeps the guard for as long as the value is used.
    pub fn into_parts(self) -> (T, Scratch) {
        (self.value, self.scratch)
    }
}

impl<T> Deref for Held<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Held<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T, U> AsRef<U> for Held<T>
where
    T: AsRef<U>,
    U: ?Sized,
{
    fn as_ref(&self) -> &U {
        self.value.as_ref()
    }
}

/// Remove the scratch roots of pids that are gone.
///
/// [`Scratch`]'s `Drop` covers a test that fails — the unwind runs it — but not
/// a test *process* that dies outright (a `Ctrl-C`ed `cargo test`, a `SIGKILL`):
/// no destructor runs at all, and the root stays where it was, named after the
/// dead process. That pid is what makes the leftovers findable, so a later run
/// can ask the kernel which processes are alive and remove only a dead one's
/// root.
///
/// The safe direction is the whole rule: **a live pid's root is never touched**.
/// Several worktrees run their suites on this machine at the same time, and a
/// live pid's scratch is somebody's running test — deleting it would break a
/// test that did nothing wrong. A pid that a dead test once held and an
/// unrelated process now holds reads as alive, which leaves one root behind:
/// the harmless direction, and the same one the product's own reaper takes
/// (`machine::reap_dead_scratch`).
///
/// A candidate is anything named `mush-<label>-<pid>`: a *directory* is a
/// scratch root, and a *file* is the litter of an older build, which wrote a
/// fixture straight into the temp directory rather than into a root of its own
/// (`mush-test-outside-<name>-<pid>.png`, `mush-clipboard-<name>-<pid>`). The
/// pid is read the same way in both — the last field of the name, before a
/// trailing file extension — and the liveness rule is the same, so a dead
/// fixture goes where nothing else would ever have reaped it. The product's own
/// scratch files are `mush-cmd-*`, and their reaping belongs to the product, so
/// they are skipped by name whatever follows. A symlink, or anything else that
/// is neither a file nor a directory, is not this sweep's to judge.
pub fn sweep_dead_roots() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(label) = name.strip_prefix("mush-") else {
            continue;
        };
        // The product's own scratch files: a pid is not the first field of
        // `mush-cmd-<pid>-<random>-<kind>`, and their reaping is its own.
        if label.starts_with("cmd-") {
            continue;
        }
        let Some(pid) = pid_in(name) else {
            continue;
        };
        if pid == 0 || alive(pid) {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        // A root goes whole; a stray file goes by itself. Anything else (a
        // symlink, a socket) is left where it is.
        if kind.is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        } else if kind.is_file() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The pid a leftover carries where this rule reads one: the last `-<digits>`
/// field of its name, before a trailing file extension (`…-1234`, `…-1234.png`).
///
/// The extension is stripped only when it is all letters, so an odd name whose
/// label holds a dot (`mush-thing-1.2`) keeps its fields as they are.
fn pid_in(name: &str) -> Option<u32> {
    let stem = match name.rsplit_once('.') {
        Some((stem, ext)) if !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphabetic()) => {
            stem
        }
        _ => name,
    };
    let (_, pid) = stem.rsplit_once('-')?;
    pid.parse().ok()
}

/// The sweep runs on the first guard a process makes, not at exit.
///
/// A process that is killed *is* the case the sweep is for, and it runs no
/// exit code at all — so the work is put where it is known to happen, the next
/// process's first use. Once per process is enough (nothing this process has
/// made can be older than its own first guard) and it costs one read of the
/// temp directory, where a sweep before every root would read it thousands of
/// times in a suite.
fn sweep_once() {
    static SWEPT: Once = Once::new();
    SWEPT.call_once(sweep_dead_roots);
}

/// Whether the kernel says this pid is alive.
///
/// `/proc/<pid>` is that answer on Linux, where this suite runs (the product's
/// own tests read `/proc` the same way). A system with no `/proc` at all has no
/// answer here, and `alive` then says *every* pid is alive: sweeping nothing is
/// the only safe reading of a question that cannot be asked, because guessing
/// wrong would take a live test's files.
fn alive(pid: u32) -> bool {
    if !Path::new("/proc").is_dir() {
        return true;
    }
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The root is where the sweep and the tests both look, named after the
    /// test and the process that made it — and it is a real directory.
    #[test]
    fn a_scratch_root_is_named_after_its_test_and_its_process() {
        let scratch = Scratch::new("scratch-named");
        assert_eq!(
            scratch.path(),
            std::env::temp_dir().join(format!("mush-scratch-named-{}", std::process::id()))
        );
        assert!(
            scratch.path().is_dir(),
            "the root is created with the guard"
        );
        std::fs::write(scratch.path().join("a-file"), b"made").unwrap();
    }

    /// A guard that drops removes what the test made under it — the passing
    /// case.
    #[test]
    fn a_dropped_guard_takes_its_root_with_it() {
        let scratch = Scratch::new("scratch-dropped");
        let root = scratch.path().to_path_buf();
        std::fs::write(root.join("work"), b"made").unwrap();
        drop(scratch);
        assert!(!root.exists(), "the root goes when the guard does");
    }

    /// The failure this type exists for: a guard dropped while a failing
    /// assertion unwinds removes its root, where a plain `remove_dir_all` at
    /// the end of the test would never run. The proof is a real assertion
    /// failure raised and caught *inside* the test: `catch_unwind` returning
    /// `Err` is the unwind, the root is checked to exist at the moment of the
    /// panic, and only the guard's `Drop` can have removed it afterwards.
    #[test]
    fn a_guard_removes_its_root_while_a_failing_test_unwinds() {
        let scratch = Scratch::new("scratch-unwinds");
        let root = scratch.path().to_path_buf();
        std::fs::write(root.join("evidence"), b"made before the failure").unwrap();

        let at_panic = root.clone();
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _scratch = scratch;
            assert!(
                at_panic.is_dir(),
                "the root is there when the test is about to fail"
            );
            assert_eq!(
                2 + 2,
                5,
                "a failing assertion, exactly as a test would fail"
            );
        }));

        assert!(failed.is_err(), "the assertion really failed");
        assert!(
            !root.exists(),
            "the unwind dropped the guard, and the guard removed the root"
        );
    }

    /// A second live guard with the same label is refused rather than allowed
    /// to share a root the first one's `Drop` would delete under the second
    /// test's feet; once it is dropped the label is free again.
    #[test]
    #[should_panic(expected = "two live tests share the scratch label")]
    fn a_second_live_guard_for_one_label_is_refused() {
        let _first = Scratch::new("scratch-shared");
        let _second = Scratch::new("scratch-shared");
    }

    /// The sweep's rule, both halves at once: a root whose pid is gone goes,
    /// and a root whose pid is *alive* stays. The live half is this test's own
    /// pid — alive by definition — and the guard holds a root named exactly the
    /// way the sweep looks for, so the assertion is about the sweep and not
    /// about a name it would skip anyway.
    #[test]
    fn a_sweep_removes_a_dead_pids_root_and_leaves_a_live_one() {
        // A pid that is really gone: a child of this test, waited for.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let dead_root = std::env::temp_dir().join(format!("mush-sweep-dead-{dead}"));
        std::fs::create_dir_all(&dead_root).unwrap();
        std::fs::write(dead_root.join("work"), b"a killed run's file").unwrap();

        let live = Scratch::new("sweep-live");

        sweep_dead_roots();

        assert!(
            !dead_root.exists(),
            "a dead pid's root is what the sweep is for"
        );
        assert!(
            live.path().is_dir(),
            "a live pid's root is somebody's running test"
        );
    }

    /// A stray *file* of a dead pid goes the same way a root does, and one of a
    /// live pid stays put: older builds wrote fixtures straight into the temp
    /// directory, and nothing else would ever reap them.
    #[test]
    fn a_sweep_removes_a_dead_pids_stray_file_and_leaves_a_live_one() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let dead_file = std::env::temp_dir().join(format!("mush-sweep-dead-file-{dead}.png"));
        std::fs::write(&dead_file, b"a killed run's fixture").unwrap();
        let live_file =
            std::env::temp_dir().join(format!("mush-sweep-live-file-{}.png", std::process::id()));
        std::fs::write(&live_file, b"this test's own fixture").unwrap();

        sweep_dead_roots();

        assert!(
            !dead_file.exists(),
            "a dead pid's stray file is litter of the same rule"
        );
        assert!(
            live_file.exists(),
            "a live pid's file is somebody's running test"
        );
        let _ = std::fs::remove_file(&live_file);
    }

    /// A name that is not `mush-<label>-<pid>` is not a candidate at all: the
    /// sweep does not guess at a pid it cannot read out of a name.
    #[test]
    fn a_sweep_leaves_a_name_it_cannot_read_a_pid_out_of() {
        let nameless = std::env::temp_dir().join("mush-sweep-without-a-pid");
        std::fs::create_dir_all(&nameless).unwrap();
        let two_field = std::env::temp_dir().join("mush-sweep-1234-not-a-pid");
        std::fs::create_dir_all(&two_field).unwrap();

        sweep_dead_roots();

        assert!(nameless.is_dir(), "no pid, no judgement");
        assert!(
            two_field.is_dir(),
            "a pid that is not the last field is not read"
        );
        let _ = std::fs::remove_dir_all(&nameless);
        let _ = std::fs::remove_dir_all(&two_field);
    }

    /// The product's own scratch files are not this sweep's to touch: they are
    /// `mush-cmd-*`, and the product reaps them itself
    /// (`machine::reap_dead_scratch`). The skip is by name, so it holds even for
    /// a shape whose last field *is* a pid — which is exactly what the dead-pid
    /// rule would otherwise take.
    #[test]
    fn a_sweep_does_not_touch_the_products_scratch_files() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let mine = std::process::id();
        let names = [
            // The legacy shape, with no pid where the reaper reads one.
            format!("mush-cmd-out-legacy-{mine}"),
            // The product's shape today, whose last field is the stream.
            format!("mush-cmd-{dead}-aaaaaa-out"),
            // A name whose last field *is* a dead pid: the sweep must still
            // leave it alone, because the family is the product's.
            format!("mush-cmd-out-{dead}"),
        ];
        for name in &names {
            std::fs::write(std::env::temp_dir().join(name), b"a command's output").unwrap();
        }

        sweep_dead_roots();

        for name in &names {
            assert!(
                std::env::temp_dir().join(name).exists(),
                "the product's own reaper owns `mush-cmd-*`: {name}"
            );
            let _ = std::fs::remove_file(std::env::temp_dir().join(name));
        }
    }
}
