//! The signals that mean "end mush", and the one road they take.
//!
//! A TUI dies the way a terminal dies: the window closes (SIGHUP), a session
//! manager stops the unit (SIGTERM), the human's terminal sends SIGINT. Every
//! one of those used to get the kernel's default disposition, and the process
//! was gone before a single destructor ran: the session was not flushed, the
//! writer was not joined, the attach socket was left on disk, and every process
//! group mush started — the `cargo build` holding `target/`, the dev server
//! holding a port — was left in nobody's list (finding E1). The socket's
//! survival is the proof: it is removed by exactly one `Drop`, so a killed mush
//! that leaves the file behind has run *no* cleanup at all.
//!
//! The road is the one `Ctrl-Q` takes, reached through a flag. A handler for
//! SIGTERM, SIGHUP and SIGINT does exactly one thing — set an [`AtomicBool`] —
//! and the event loop reads that flag on every frame (it already wakes every
//! 30 ms for input) and runs [`App::signal_quit`]: the loop returns, `App`'s
//! `Drop` flushes the session and kills every job and held command, the writer
//! is joined, and the attach guard removes the socket. Nothing in a handler
//! touches a lock, a channel or the registry: what a handler may do is
//! async-signal-safe and small, and this is that.
//!
//! **Why `signal-hook`.** The dependency is already in the tree — crossterm
//! links it — so this is a name for code the build already compiles, not a new
//! crate. The other in-tree syscall crate, `rustix`, has no safe signal road:
//! its `sigaction` lives in a `doc(hidden)` module, is `unsafe`, documents
//! itself as "highly experimental" and unusable beside libc, and this workspace
//! forbids `unsafe` (`Cargo.toml`). `signal_hook::flag::register` is the
//! handler that sets a flag, and nothing else.
//!
//! **The second signal dies at once.** After the flag is set, the same signals
//! are registered with [`signal_hook::flag::register_conditional_default`]:
//! the next one restores the signal's default disposition and re-raises it, so
//! mush dies immediately instead of waiting out a flush or a writer's join. The
//! first signal asks for the graceful end; the second is the human insisting,
//! and an insisting human must never meet a process that cannot be killed
//! because its cleanup is stuck.
//!
//! **A signal while a question is on screen or a turn is in flight** takes the
//! same road: there is no arm and no second press, because a signal cannot be
//! pressed twice by the app — the quit is what the human asked for, and what it
//! costs is exactly what a confirmed `Ctrl-Q` costs (the in-flight turn dies,
//! and the exit flush writes what the debounce had not). The one thing a signal
//! must not do is die raw, which is the behaviour this module removes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::flag;
use signal_hook::SigId;

/// Set by the handlers, read by the event loop. Process-wide and process-long:
/// the handlers live for the whole run, and so does the question "was mush
/// asked to end".
fn quit_flag() -> Arc<AtomicBool> {
    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    FLAG.get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone()
}

/// Whether a signal has asked mush to end. The event loop's own question; the
/// handlers are the only writers.
pub fn quit_requested() -> bool {
    quit_flag().load(Ordering::SeqCst)
}

/// Hold the handlers for as long as this value lives.
///
/// Dropping it deregisters them — `signal_hook`'s `SigId` is an id, not a
/// guard, so this `Drop` is what takes the handlers away — and the caller must
/// therefore keep it for the whole run. `#[must_use]` because a dropped guard
/// is a vanished signal road, not a value nobody wanted.
#[must_use = "the handlers are deregistered when the guard drops"]
pub struct Signals(Vec<SigId>);

impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.0.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

/// Install the three signals that end mush.
///
/// Order matters, and signal-hook's own docs name it: the actions for one
/// signal run in registration order, so the conditional default is registered
/// *first* — it reads the flag as it stood before this signal — and the
/// flag-setting action second. The other order would set the flag and then ask
/// the condition, and the first signal would kill mush raw.
///
/// The error is the caller's to report: a mush that cannot install handlers
/// keeps the behaviour that existed before this module — the kernel's default —
/// and the caller decides whether that is worth refusing to start for.
pub fn install() -> Result<Signals, String> {
    let flag = quit_flag();
    let mut ids = Vec::with_capacity(6);
    for signal in [SIGTERM, SIGHUP, SIGINT] {
        let registered = flag::register_conditional_default(signal, flag.clone()).and_then(|id| {
            ids.push(id);
            flag::register(signal, flag.clone())
        });
        match registered {
            Ok(id) => ids.push(id),
            Err(error) => {
                // A half-installed road is worse than none: drop what was
                // registered, so the caller's report — mush runs without
                // handlers — is the truth.
                for id in ids.drain(..) {
                    signal_hook::low_level::unregister(id);
                }
                return Err(format!(
                    "could not install the handler for signal {signal}: {error}"
                ));
            }
        }
    }
    Ok(Signals(ids))
}
