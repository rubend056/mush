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
//! test) rather than drop it early.
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
use std::sync::Mutex;

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
        // The root is ours now (the label above says so), so anything already
        // under this process's name is a stale run's — a pid reused by this
        // process, which cannot be told from a live run's leftover by the pid
        // alone.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The root is named after the test and the process that made it, and it
    /// is a real directory the test can work in.
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
}
