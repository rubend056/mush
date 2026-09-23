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
//! The road is the one `Ctrl-Q` takes, reached through flags. A handler for
//! SIGTERM, SIGHUP and SIGINT does exactly one async-signal-safe thing — write
//! one byte into a self-pipe ([`signal_hook::low_level::pipe`]) — and a watcher
//! thread reads those bytes, counts them and sets the flags. The event loop
//! reads [`quit_requested`] on every frame (it already wakes every 30 ms for
//! input) and runs [`App::signal_quit`](crate::app::App::signal_quit): the loop
//! returns, and `main` runs the exit road
//! ([`App::shutdown`](crate::app::App::shutdown)) — the flush, the actors'
//! endings, the quit fence and the kill walk, the writer's thread — *before*
//! the terminal and the socket are handed back (finding R3). Nothing in a
//! handler touches a lock, a channel or the registry: what a handler may do is
//! async-signal-safe and small, and one byte into a pipe is that.
//!
//! **Why a watcher thread and not a handler.** A handler can set a flag and
//! nothing else; it cannot count presses, and it cannot be asked a question by
//! a thread that is waiting. The watcher is an ordinary thread, so counting and
//! answering `forced()` are ordinary code, and the signal context owns no state
//! at all — the pipe is the whole hand-over, and there is nothing in it to
//! race.
//!
//! **Why `signal-hook`.** The dependency is already in the tree — crossterm
//! links it — so this is a name for code the build already compiles, not a new
//! crate. The other in-tree syscall crate, `rustix`, has no safe signal road:
//! its `sigaction` lives in a `doc(hidden)` module, is `unsafe`, documents
//! itself as "highly experimental" and unusable beside libc, and this workspace
//! forbids `unsafe` (`Cargo.toml`). `low_level::pipe::register` is the handler
//! that writes the byte, and nothing else.
//!
//! **The first press asks for the quit, the second hurries it, the third ends
//! mush where it stands.** [`quit_requested`] is what the event loop polls;
//! [`forced`] is what the exit road's bounded waits poll, so a second signal
//! cannot skip a step of the road — it only means no wait on it outlives its
//! next poll (see the `forced` parameter of `session_save`'s flush and join and
//! of `machine`'s reap). The third press is [`Delivery::Death`]: [`die`] raises
//! SIGKILL and exits, because a human who has pressed three times has stopped
//! believing mush will ever finish, and a process that cannot be killed is
//! worse than one that dies with its cleanup unfinished.
//!
//! **What that costs, written down (finding PM6).** The third press, and any
//! SIGKILL — which no handler can see — end mush with the terminal still raw
//! and in the alternate screen, and the human's shell then needs `reset` or
//! `stty sane`. Before this module counted presses, that was the price of the
//! *second* signal, sent by the person most likely to send one: the screen
//! says mush is gone, and it is not. Now the second press only hurries, and
//! the terminal is handed back when the road it hurried finishes; the raw death
//! is the third press's, and an insistent third is allowed to abandon the
//! screen on purpose.
//!
//! **A signal while a question is on screen or a turn is in flight** takes the
//! same road: there is no arm and no confirmation, because a signal cannot be
//! pressed twice by the app — the quit is what the human asked for, and what it
//! costs is exactly what a confirmed `Ctrl-Q` costs (the in-flight turn dies,
//! and the exit flush writes what the debounce had not). The one thing a signal
//! must not do is die raw, which is the behaviour this module removes.

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};

use signal_hook::consts::{SIGHUP, SIGINT, SIGKILL, SIGTERM};
use signal_hook::low_level;
use signal_hook::SigId;

/// Set by the watcher, read by the event loop. Process-wide and process-long:
/// the watcher lives for the whole run, and so does the question "was mush
/// asked to end".
fn quit_flag() -> &'static AtomicBool {
    static FLAG: OnceLock<AtomicBool> = OnceLock::new();
    FLAG.get_or_init(|| AtomicBool::new(false))
}

/// Set by the second press, read by the exit road's waits.
fn force_flag() -> &'static AtomicBool {
    static FLAG: OnceLock<AtomicBool> = OnceLock::new();
    FLAG.get_or_init(|| AtomicBool::new(false))
}

/// How many presses have arrived since the handlers were installed. One
/// counter for the process, written by the one watcher thread.
fn presses() -> &'static AtomicU64 {
    static COUNT: OnceLock<AtomicU64> = OnceLock::new();
    COUNT.get_or_init(|| AtomicU64::new(0))
}

/// Whether a signal has asked mush to end. The event loop's own question; the
/// watcher is the only writer.
pub fn quit_requested() -> bool {
    quit_flag().load(Ordering::SeqCst)
}

/// Whether a second signal has asked the exit road not to wait.
///
/// The waits the road is made of call this beside their own deadline; `false`
/// — the ordinary case — never changes anything.
pub fn forced() -> bool {
    force_flag().load(Ordering::SeqCst)
}

/// Take the hurry flag, if it is up, and clear it.
///
/// `main` calls this as the exit road's first line. Presses that arrived before
/// the road began asked for nothing it can hurry, and a `true` left standing
/// there would cut the first wait — the flush of the human's own words — short
/// on the strength of a double-tap the UI never saw. A press *after* that line
/// does shorten the waits.
pub fn take_force() -> bool {
    take(force_flag())
}

/// What the `presses`-th press means, one-based.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Delivery {
    /// The first press: run the quit road.
    Quit,
    /// The second: the road must not wait past its next poll.
    Hurry,
    /// The third, and every one after it: mush ends here, cleanup unfinished.
    Death,
}

/// The meaning of a press count.
///
/// A pure function of the number of presses, so every arm can be read by a test
/// without touching a process-global flag — the flags belong to the process,
/// and a test that wrote them would be racing every other test in the binary.
fn delivery(presses: u64) -> Delivery {
    if presses >= 3 {
        Delivery::Death
    } else if presses == 2 {
        Delivery::Hurry
    } else {
        // Zero presses cannot reach here (the watcher counts from one), and one
        // press is the quit; both are the same answer.
        Delivery::Quit
    }
}

/// Apply one press to the flags it names.
///
/// Split out from the watcher so a test can read what each meaning does with
/// flags of its own, and so the watcher's body is a call to this.
fn deliver(delivery: Delivery, quit: &AtomicBool, force: &AtomicBool) {
    match delivery {
        Delivery::Quit => quit.store(true, Ordering::SeqCst),
        Delivery::Hurry => force.store(true, Ordering::SeqCst),
        // Nothing is left to remember.
        Delivery::Death => die(),
    }
}

/// Take a flag: read it and clear it in one step, so exactly one caller sees
/// `true` and the next one reads the state after the take.
fn take(flag: &AtomicBool) -> bool {
    flag.swap(false, Ordering::SeqCst)
}

/// End mush at once, for a press that says mush will not finish.
///
/// `raise(SIGKILL)`, not `exit`: the signal cannot be caught or blocked, so
/// this is the death the kernel would have dealt, dealt by the kernel — the
/// human's `kill` and this press end the same way. `low_level::exit` is the
/// fallback for a raise that returned, which SIGKILL's does not; a function
/// that returns `!` has to end somewhere.
fn die() -> ! {
    let _ = low_level::raise(SIGKILL);
    low_level::exit(1)
}

/// Read the self-pipe until every write end is gone, and make each byte a
/// press.
///
/// One byte is one delivery: the handler writes exactly one, and drops it
/// whenever the pipe is full — a burst of the same signal is collated by the
/// kernel, and what the count answers is "how many times did a human press",
/// not an audit. A read of zero is the end of the road: [`Signals::drop`]
/// unregisters the handlers first, and the write ends belong to the
/// registrations, so a watcher still blocked here is woken with an EOF and
/// returns.
fn watch(mut reader: UnixStream) {
    let mut buffer = [0u8; 64];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return,
            Ok(bytes) => {
                for _ in 0..bytes {
                    let count = presses().fetch_add(1, Ordering::SeqCst) + 1;
                    deliver(delivery(count), quit_flag(), force_flag());
                }
            }
            // A signal arriving mid-read interrupts it; the byte is still in the
            // pipe, and the next read takes it.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

/// Take every registration in `ids` away, which closes their write ends.
fn unregister_all(ids: &mut Vec<SigId>) {
    for id in ids.drain(..) {
        low_level::unregister(id);
    }
}

/// Hold the handlers and their watcher for as long as this value lives.
///
/// `signal_hook`'s `SigId` is an id, not a guard, so this `Drop` is what takes
/// the handlers away — and the write ends with them, which is what ends the
/// watcher's read. `#[must_use]` because a dropped guard is a vanished signal
/// road, not a value nobody wanted.
#[must_use = "the handlers are deregistered when the guard drops"]
pub struct Signals {
    ids: Vec<SigId>,
    watcher: Option<JoinHandle<()>>,
}

impl Drop for Signals {
    fn drop(&mut self) {
        // First the registrations: no press can arrive after this, and each
        // unregister closes the write end it owned.
        unregister_all(&mut self.ids);
        // Then the watcher. The join has no deadline because it does not need
        // one: the read that was blocking on the pipe now sees an EOF, so the
        // thread has no work left to do but end. It is joined rather than
        // detached so a panic in it — there is none — would be the test's to
        // see, not the next test's.
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

/// Install the three signals that end mush.
///
/// One self-pipe, one write end cloned per signal, and one watcher thread
/// reading it. The error is the caller's to report: a mush that cannot install
/// handlers keeps the behaviour that existed before this module — the kernel's
/// default — and the caller decides whether that is worth refusing to start
/// for. A half-installed road is worse than none, so a failure in the middle
/// takes the registrations already made away before the error travels.
pub fn install() -> Result<Signals, String> {
    let (reader, writer) =
        UnixStream::pair().map_err(|error| format!("could not open the signal pipe: {error}"))?;
    let mut ids = Vec::with_capacity(3);
    for signal in [SIGTERM, SIGHUP, SIGINT] {
        let end = match writer.try_clone() {
            Ok(end) => end,
            Err(error) => {
                unregister_all(&mut ids);
                return Err(format!(
                    "could not clone the signal pipe for signal {signal}: {error}"
                ));
            }
        };
        match low_level::pipe::register(signal, end) {
            Ok(id) => ids.push(id),
            Err(error) => {
                unregister_all(&mut ids);
                return Err(format!(
                    "could not install the handler for signal {signal}: {error}"
                ));
            }
        }
    }
    // The registrations own a clone each; this end is nobody's, and holding it
    // open would keep the watcher's read from ever seeing the EOF that ends it.
    drop(writer);
    let watcher = thread::Builder::new()
        .name("mush-signals".to_string())
        .spawn(move || watch(reader));
    match watcher {
        Ok(watcher) => Ok(Signals {
            ids,
            watcher: Some(watcher),
        }),
        // No watcher means the bytes would pile up in the pipe and no flag
        // would ever be set: an installed road that leads nowhere. Take it
        // away, and let the caller run with the kernel's disposition.
        Err(error) => {
            unregister_all(&mut ids);
            Err(format!("could not start the signal watcher: {error}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One press asks for the quit, a second asks the road not to wait, and a
    /// third has stopped asking. The count is what the whole module is for, and
    /// it is read here as a pure function because the flags are the process's.
    #[test]
    fn a_press_is_counted_and_the_count_decides() {
        assert_eq!(delivery(1), Delivery::Quit);
        assert_eq!(delivery(2), Delivery::Hurry);
        assert_eq!(delivery(3), Delivery::Death);
        assert_eq!(delivery(4), Delivery::Death, "and it never goes back");
    }

    /// What each meaning does to the flags, on a flag of the test's own: quit
    /// raises the quit, hurry raises the hurry, and neither touches the other.
    #[test]
    fn a_delivery_sets_the_flag_it_names_and_no_other() {
        let quit = AtomicBool::new(false);
        let force = AtomicBool::new(false);
        deliver(Delivery::Quit, &quit, &force);
        assert!(quit.load(Ordering::SeqCst), "the first press asks to quit");
        assert!(!force.load(Ordering::SeqCst), "and asks for nothing else");
        deliver(Delivery::Hurry, &quit, &force);
        assert!(force.load(Ordering::SeqCst), "the second press hurries");
        // `Death` is not here: it ends the process, which a test cannot read.
    }

    /// Taking a flag is a read and a clear in one step: the caller that takes
    /// `true` is the one that spends the hurry, and the next one reads the
    /// state after it — which is what stops a pre-road double-tap from cutting
    /// a wait twice.
    #[test]
    fn a_take_reads_a_flag_and_clears_it_once() {
        let flag = AtomicBool::new(true);
        assert!(take(&flag), "the first take sees the flag");
        assert!(!take(&flag), "the second sees it cleared");
        assert!(!take(&flag), "and it stays cleared");
    }
}
