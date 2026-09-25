//! mush-core — the agent-agnostic heart of mush.
//!
//! This crate is pure domain logic: OpenAI-compatible message types, the simple
//! system prompt, conversation persistence under `.mush/`, and safe workspace
//! file operations. It performs file I/O but never touches a terminal, a
//! socket, or a thread. That is what makes it easy to test and hard to break.

pub mod config;
pub mod git;
pub mod message;
pub mod outline;
pub mod prompt;
pub mod provider;
/// A test's own scratch root, removed when it drops.
///
/// Not part of the product: the module is compiled only for this crate's tests
/// (`cfg(test)`) or for `mush`'s, which reach it through the `test-support`
/// feature its `dev-dependencies` entry turns on.
#[cfg(any(test, feature = "test-support"))]
pub mod scratch;
pub mod secrets;
pub mod session;
pub mod text;
pub mod tools;
pub mod transcript;
pub mod usages;
pub mod userconfig;
pub mod whole_disk;
pub mod workspace;

pub use config::{Config, Overrides};
pub use git::{RepoStatus, Stat, Worktree};
pub use message::{FunctionCall, Image, Message, ToolCall, Usage};
pub use provider::Provider;
pub use session::Session;
pub use userconfig::UserConfig;
pub use workspace::Workspace;

/// The process environment's `GIT_ALLOW_PROTOCOL`, owned by one test at a
/// time.
///
/// `std::env::set_var` writes the process's one environment, every thread at
/// once, and four tests in this binary put `file` in it — five sites, because
/// `git::tests::a_submodule_that_cannot_be_placed_is_left_empty_and_the_worktree_is_clean`
/// installs it twice. Each site saved the value it found and put it back
/// inline after its spawns, so run together they race both ways: a restore
/// landing while another test's git child is still to spawn takes the `file`
/// transport away from the submodule clone it was installed for — git then
/// refuses the clone, and the test fails, or, in the two tests whose subject
/// *is* a clone that cannot be placed, it refuses for the wrong reason and the
/// test proves less than it says — and a save can record a sibling's `file` as
/// the value to restore, leaving the protocol allowed for the rest of the
/// binary. Inline restores are also skipped by a panic on the road to them,
/// which left the protocol allowed with nothing that would ever say so.
///
/// A test that sets the variable holds this for as long as the probe is in the
/// environment — [`ProtocolInProcess`] puts back what was there on the way
/// out, panic or not — so two windows can never overlap. The same
/// one-mutex-per-global shape as `mush`'s `PANIC_HOOK_LOCK`
/// (`crates/mush/src/main.rs`), its `MUSH_API_KEY_LOCK`, and
/// `crates/mush/src/machine.rs`'s `tests::PATH_LOCK` (the process's `PATH`).
///
/// Measured with a temporary detector test holding a window of its own beside
/// these five sites: 276 of 300 runs failed with `GIT_ALLOW_PROTOCOL` unset
/// inside that window — the `file` transport gone while a clone that needed it
/// was still to spawn — and 10 of 300 under twelve extra busy loops. The
/// assertions added to the sites' own spawn moments (also temporary) never
/// fired in those 600 runs: the windows overlap and change value under each
/// other, and whether a site's own spawn lands in the broken stretch is what
/// decides its test.
#[cfg(test)]
pub(crate) static GIT_ALLOW_PROTOCOL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The process environment's `MUSH_API_KEY`, owned by one test at a time.
///
/// `std::env::set_var` writes the process's one environment, every thread at
/// once, and `git::tests::a_git_child_never_sees_mushs_key` puts a probe key in
/// it while git commits through a hook that prints the variable: the restore
/// was hand-written, so a panic on the road to it — a failed `git add` is
/// enough — left `sk-probe-inheritance-…` in the environment of every test
/// after it in this binary. A second writer breaks the window itself: a
/// restore landing between the `set_var` and the commit leaves the hook's file
/// empty with no key in the process at all, so the empty file proves the key
/// was gone rather than that the spawn scrubbed it.
///
/// A test that sets the variable holds this for as long as the probe is in the
/// environment — [`KeyInProcess`] puts back what was there on the way out,
/// panic or not — so two can never overlap. The same one-mutex-per-global
/// shape as `mush`'s `MUSH_API_KEY_LOCK` and
/// `crates/mush/src/machine.rs`'s `tests::PATH_LOCK` (the process's `PATH`).
///
/// Measured beside this one site with a temporary detector test holding a
/// probe of its own over the same window: 300 of 300 runs failed idle and 296
/// of 300 under load, the process value inside the window being the site's own
/// `sk-probe-inheritance-…` rather than the detector's probe — two writers
/// cannot share the variable. With only this site present the restore's panic
/// road above is the whole of what bites today; the lock is also where a
/// second writer would have to take its turn.
#[cfg(test)]
pub(crate) static MUSH_API_KEY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The `file` transport allowed for one test, put back when it ends — panic or
/// not.
///
/// The previous value is kept, so the restore puts back exactly what was there
/// and an unset variable stays unset. The caller holds
/// [`GIT_ALLOW_PROTOCOL_LOCK`] from before the set until after this guard
/// drops: the lock is what keeps a second test's window from opening inside
/// this one.
#[cfg(test)]
pub(crate) struct ProtocolInProcess(Option<std::ffi::OsString>);

#[cfg(test)]
impl ProtocolInProcess {
    /// Allow git's `file` transport for as long as this guard lives.
    pub(crate) fn allow_file() -> Self {
        let previous = std::env::var_os("GIT_ALLOW_PROTOCOL");
        std::env::set_var("GIT_ALLOW_PROTOCOL", "file");
        Self(previous)
    }
}

#[cfg(test)]
impl Drop for ProtocolInProcess {
    fn drop(&mut self) {
        match &self.0 {
            Some(previous) => std::env::set_var("GIT_ALLOW_PROTOCOL", previous),
            None => std::env::remove_var("GIT_ALLOW_PROTOCOL"),
        }
    }
}

/// `MUSH_API_KEY` set for one test, put back when it ends — panic or not.
///
/// The previous value is kept, so the restore puts back exactly what was there
/// and an unset variable stays unset. The caller holds [`MUSH_API_KEY_LOCK`]
/// from before the set until after this guard drops: the lock is what keeps a
/// second writer out of this window.
#[cfg(test)]
pub(crate) struct KeyInProcess(Option<std::ffi::OsString>);

#[cfg(test)]
impl KeyInProcess {
    pub(crate) fn set(value: &str) -> Self {
        let previous = std::env::var_os("MUSH_API_KEY");
        std::env::set_var("MUSH_API_KEY", value);
        Self(previous)
    }
}

#[cfg(test)]
impl Drop for KeyInProcess {
    fn drop(&mut self) {
        match &self.0 {
            Some(previous) => std::env::set_var("MUSH_API_KEY", previous),
            None => std::env::remove_var("MUSH_API_KEY"),
        }
    }
}

/// Ceiling for bytes of command output handed to a model in one result. The
/// command is no longer the only road by which a big text result reaches the
/// model — `read_file` is back, and its window is cut to the same cap — so this
/// is still the ceiling `Config::cmd_cap` scales down to the room a cut leaves
/// between its stopping point (four fifths of the history budget) and the
/// ceiling for a window too small to hold it.
pub const CMD_CAP: usize = 16_000;
/// How long a shell command may run before it is killed.
pub const CMD_TIMEOUT_SECS: u64 = 120;
