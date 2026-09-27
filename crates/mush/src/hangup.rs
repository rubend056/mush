//! The terminal that went away, and the road that ends mush when it does.
//!
//! A terminal dies the way a window dies: sshd drops the session's pty, the
//! emulator is closed, the multiplexer's client is killed. mush's input then
//! comes from a descriptor whose `read` answers `Ok(0)` for ever, and crossterm
//! — which mush reads its events through — does not treat that as an end:
//! `crossterm-0.28.1/src/event/source/unix/mio.rs`,
//! `UnixInternalEventSource::try_read`'s `TTY_TOKEN` arm loops on `read` and
//! falls through on `Ok(0)` (and on every error that is neither `WouldBlock`
//! nor `Interrupted`), so `event::poll` never returns. Every road out of mush
//! lives behind that call — the `tick` that saves the session, the signal flags
//! the event loop reads, the attach socket's answers — and with the UI thread
//! inside it, none of them ran again.
//!
//! Measured, in a real session (2026-09-27; finding U16, §8.114 of
//! `docs/findings.md`): pid 3866512 in
//! `p/tiny` lost its ssh pty at 16:05. The UI thread spun at ~9.7M `read()`/s
//! (~100 % CPU, three quarters of it in the kernel; `/proc/PID/io` gained 19.3M
//! `syscr` in 2 s with `rchar` flat) for 19 minutes. `session.json` stayed
//! frozen at its 16:05:41 snapshot, because the debounce's write needs a tick;
//! SIGTERM, SIGHUP and SIGINT were inert — [`crate::signals`] set its flag, and
//! the only reader of that flag was the wedged loop — and the third press's raw
//! death, raised by the signal watcher thread and not by the loop, was what
//! ended it; every attach request timed out after 30 s; recovery
//! was `kill -9` and a start that restored from `session.json`. crossterm
//! 0.29.0's `mio.rs` is byte-identical to 0.28.1's (both
//! `md5 a4d49587057d0c13e6c578dc5f41669a`, fetched and diffed), and the source
//! is `pub(crate)` behind a `pub(crate)` trait, so there is no version to bump
//! and no reader for mush to swap out.
//!
//! **The policy: a terminal that is gone ends mush** — promptly, and as cleanly
//! as the moment allows. Survivability (attaching to a detached mush, running
//! headless, taking a session over) is deliberately not built here: mush is a
//! surface on a terminal, and a human who wants the work to outlive the
//! terminal wants a wrapper — tmux — that owns the terminal and can hand mush a
//! new one. What the hard path costs is the exit road it could not run: job
//! process groups are orphaned, and the terminal is left as a hard death leaves
//! it. That is the accepted price of ending promptly; the accepted price is
//! written down in the finding, not hidden here.
//!
//! **The road.** A watcher thread polls the descriptor crossterm will read
//! ([`tty`], which mirrors crossterm's own choice in
//! `terminal::sys::file_descriptor::tty_fd`: stdin when stdin is a tty, else
//! `/dev/tty` opened read/write) for `POLLHUP | POLLERR` — and nothing else, so
//! a terminal that is merely idle never wakes the thread and a keypress is
//! never stolen from crossterm. The block is `poll(2)` with no timeout, and the
//! only other descriptor in the set is the stand-down pair, so the thread costs
//! a sleeping task and nothing else while the terminal lives.
//!
//! Linux reports the hangup on a pty's *slave* the moment the last master
//! closes, and keeps reporting it: `POLLIN|POLLHUP|POLLERR`, with the `read`
//! that follows returning 0 — the very `Ok(0)` the crossterm loop spins on.
//! That is not read from the kernel's manual: this module's own test
//! (`tests::a_pty_whose_master_closes_is_a_hangup` in the source) allocates a
//! real pty pair and asserts both halves.
//!
//! On the hangup, in this order:
//!
//! 1. the flag [`take`] answers is raised, so an event loop that can still run
//!    leaves through the *ordinary* road — the quit `Ctrl-Q` and a signal take,
//!    with `App::shutdown`'s flush, actors' endings, kill walk and socket
//!    unlink behind it (finding R3). The loop asks at the top of a frame, and
//!    `main` asks again when the frame itself is what failed: a paint whose
//!    write answers `EIO` because the pty is hung up is this hangup by the
//!    other door, not a failure to report, so it ends through the same road;
//! 2. [`TAKE_BOUND`] is waited out for the take. A hangup that lands between
//!    frames is taken within a frame (the loop asks every 30 ms), so the bound
//!    is only ever spent when nothing *can* take it;
//! 3. if it is still untaken, the UI thread is inside crossterm's spin and no
//!    other road out exists, so the process ends here: `std::process::exit(0)`.
//!    Not the `raise(SIGKILL)` `signals`'s third press uses — this is a
//!    condition and not a human's insistence, and
//!    because the take in step 2 and this exit are two halves of *one* fact:
//!    the terminal is gone, and mush ends. A shell sees the same status either
//!    way, so a script cannot tell whether the wedge path was needed. It is
//!    still a hard death: no destructor runs, so the attach socket stays on
//!    disk for the next start to clear, job process groups are orphaned, and
//!    the keyboard-enhancement frame mush pushed stays on the emulator's stack
//!    (`docs/mush.md` §4, `printf '\033[<1u'` at the shell) — though in
//!    practice the terminal is gone by then, which is what this module is
//!    about.
//!
//! **The stand-down.** `main` drops the guard *before* `App::shutdown`, so an
//! exit already in progress — a `Ctrl-Q`, a signal's quit — is never cut short
//! by step 3. Dropping closes the stand-down pair, which ends a poll blocked on
//! the terminal alone, and sets the flag the watcher re-reads inside its bound;
//! the join is what makes the promise: once `Drop` returns, the thread that
//! could end the process is provably gone.
//!
//! **A failed arm is not fatal**, the same shape as
//! [`crate::attach::serve`]'s bind: a mush whose terminal cannot be watched is
//! the mush that existed before this module, and refusing to start would trade
//! a wedge for no mush at all. [`watch`] returns the reason, and `main` says it
//! on stderr and runs on.

use std::fs::OpenOptions;
use std::io::{self, IsTerminal};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rustix::event::{poll, PollFd, PollFlags};
use rustix::io::Errno;

/// Where the terminal is when it is not stdin, exactly as crossterm reaches it.
const DEV_TTY: &str = "/dev/tty";

/// How long the watcher waits for its flag to be taken before it ends the
/// process where it stands.
///
/// The wait is not a grace period for a *clean* exit — that took one frame, and
/// a frame is 30 ms — it is the width of the honest doubt about the UI thread:
/// a thread that is merely busy (a huge paste, a slow draw) must not be
/// mistaken for one wedged in crossterm. A second is tens of frames of slack,
/// and short enough that a human watching a dead window does not reach for
/// `kill` first.
const TAKE_BOUND: Duration = Duration::from_secs(1);

/// How often the wait above re-reads the flag and the stand-down.
///
/// The event loop takes the flag within a frame, so this is only the resolution
/// of the *bound*, not of the ordinary road; 20 ms of a sleeping thread for one
/// second is nothing, and it keeps the stand-down prompt too.
const TAKE_POLL: Duration = Duration::from_millis(20);

/// The thread's name, so a stray one is named in `ps` the way the rest of the
/// tree's threads are.
const THREAD_NAME: &str = "mush-hangup";

/// What the watcher and the guard share: the flag between them, and the fence
/// that says whose process this is.
///
/// One struct rather than two globals because the two facts are read together
/// and must agree: `gone` is "the terminal went away", `down` is "the exit road
/// owns the process now", and the second is what makes the first harmless.
#[derive(Default)]
struct Shared {
    /// Raised by the watcher, taken by the event loop ([`take`]) — or left
    /// standing, which is what step 3 is for.
    gone: AtomicBool,
    /// Set once, by the guard's `Drop`, and never cleared: the process has an
    /// exit road running and no watcher may end it.
    ///
    /// It is a mutex because it is also the *fence* around step 3: the watcher
    /// decides to die while holding it, and `Drop` sets it while holding it, so
    /// the two cannot both be in flight — one of them is first, and the other
    /// reads the world the first one left. A plain flag would leave a window
    /// where the watcher reads "not stood down" and the guard stands it down a
    /// microsecond later, and the exit road would be cut short after all.
    down: Mutex<bool>,
}

impl Shared {
    /// Whether the exit road has taken the process over.
    fn stood_down(&self) -> bool {
        *self.down.lock().unwrap()
    }

    /// Take whatever the watcher raised: read the flag and clear it in one
    /// step, so exactly one caller sees `true` (the shape
    /// [`crate::signals::take_force`] uses).
    fn take(&self) -> bool {
        self.gone.swap(false, Ordering::SeqCst)
    }
}

/// The watcher this process armed, if it armed one.
///
/// Process-wide like [`crate::signals`]'s flags, and for the same reason: the
/// watcher lives for the run and the question "has the terminal gone" outlives
/// every thread but the process. A test that arms a watcher of its own keeps
/// its own [`Shared`] and never touches this one.
fn armed() -> &'static OnceLock<Arc<Shared>> {
    static ARMED: OnceLock<Arc<Shared>> = OnceLock::new();
    &ARMED
}

/// Whether the terminal has gone away, taken and cleared.
///
/// The event loop's own question, asked where it asks
/// [`crate::signals::quit_requested`]. `false` — the ordinary case, and every
/// case in a process that never armed a watcher — changes nothing.
pub fn take() -> bool {
    match armed().get() {
        Some(shared) => shared.take(),
        None => false,
    }
}

/// Watch the terminal crossterm will read, for as long as this value lives.
///
/// The guard is the arming: dropping it stands the watcher down (see the
/// module docs), and it must be dropped before the exit road, not after it —
/// `main` is the only caller and its comment says where.
#[must_use = "the terminal is watched only while the guard lives"]
pub struct Watcher {
    /// The write end of the stand-down pair. Closing it is what wakes a poll
    /// that is blocked on the terminal alone; the read end is the watcher's.
    wake: Option<UnixStream>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // The flag first, then the wake. A watcher already past its poll reads
        // the flag before it decides anything, and one blocked in the poll is
        // let go by the close — so neither order can leave a thread that is
        // about to end a process that is already ending.
        *self.shared.down.lock().unwrap() = true;
        drop(self.wake.take());
        // No deadline on the join: the close above ends the poll at once, and a
        // watcher inside its bound sees the flag at its next poll of it. The
        // join is the promise the ordering rests on — once this returns, the
        // thread that could end the process is gone, so `App::shutdown` below
        // cannot be cut short by it.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Arm the watcher on the terminal, or say why it cannot be armed.
///
/// The error is the caller's to report: mush runs without a watcher exactly as
/// it ran before this module (see the module docs on the failed arm).
pub fn watch() -> Result<Watcher, String> {
    let fd = tty()?;
    arm(fd, TAKE_BOUND)
}

/// Arm a watcher on `fd` and publish it as the one [`take`] answers for.
///
/// Split from [`watch`] — which is this plus the descriptor's choice — so the
/// event loop's own road can be walked by a test: the flag a real watcher
/// raises, in the process-global slot the loop reads, rather than a flag a test
/// made up.
fn arm(fd: OwnedFd, bound: Duration) -> Result<Watcher, String> {
    let shared = Arc::new(Shared::default());
    let watcher = watch_fd(fd, bound, shared.clone())?;
    // Only once the watcher is up: a watcher that could not start must not be
    // the one the event loop's `take` reaches.
    let _ = armed().set(shared);
    Ok(watcher)
}

/// The descriptor crossterm reads, chosen the way crossterm chooses it.
///
/// `Terminal::new` builds its event source from `tty_fd()`, so watching any
/// other descriptor would be watching a descriptor nobody reads: a dup of stdin
/// when stdin is a terminal, and `/dev/tty` — opened read/write, as crossterm
/// opens it — when it is not. The dup is deliberate: this thread holds the
/// descriptor for the whole run, and a close of fd 0 elsewhere must not turn
/// its poll into `EBADF` (the dup shares the open file description, so it is
/// the same terminal either way).
fn tty() -> Result<OwnedFd, String> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return stdin
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| format!("stdin cannot be duplicated ({error})"));
    }
    open_tty(Path::new(DEV_TTY))
}

/// `/dev/tty`, read/write, as an owned descriptor.
///
/// Split from [`tty`] so the failure can be a test's — a path that is not a
/// terminal — without a process that has no controlling terminal to run in.
fn open_tty(path: &Path) -> Result<OwnedFd, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map(OwnedFd::from)
        .map_err(|error| {
            format!(
                "stdin is not a terminal and {} cannot be opened ({error})",
                path.display()
            )
        })
}

/// [`watch`] over a descriptor a test chose, with a bound a test chose.
///
/// The seam is the descriptor and the bound, not a trait: a watcher is a thread
/// polling an fd, and what a test needs to make real is the fd — a socketpair
/// whose peer closes, a real pty's slave whose master closes — plus a bound it
/// can wait out. `shared` is the test's own, so nothing here races the flags of
/// a watcher another test armed.
fn watch_fd(fd: OwnedFd, bound: Duration, shared: Arc<Shared>) -> Result<Watcher, String> {
    let (wake_reader, wake_writer) = UnixStream::pair()
        .map_err(|error| format!("the stand-down pair cannot be made ({error})"))?;
    let for_thread = shared.clone();
    let thread = thread::Builder::new()
        .name(THREAD_NAME.to_string())
        .spawn(move || follow(fd, wake_reader, for_thread, bound));
    match thread {
        Ok(thread) => Ok(Watcher {
            wake: Some(wake_writer),
            shared,
            thread: Some(thread),
        }),
        // No thread means nothing can watch the terminal: the caller says so
        // and mush runs with the signal road alone.
        Err(error) => Err(format!("the watcher thread cannot be started ({error})")),
    }
}

/// The watcher: block on the terminal until it hangs up or the guard lets go.
///
/// A blocking `poll(2)` with no timeout, on two descriptors: the terminal, and
/// the stand-down pair the guard closes. Nothing here returns while the
/// terminal lives, and nothing here touches a lock, a flag or an allocation
/// until it does not — the thread's whole cost while the terminal is fine is a
/// sleeping task.
///
/// `poll` and not `select`: the descriptors are a tty and a socket, and what is
/// asked of them is a *condition* (`POLLHUP`/`POLLERR`) rather than
/// readability, which is what `poll` reports and `select` does not.
fn follow(fd: OwnedFd, wake: UnixStream, shared: Arc<Shared>, bound: Duration) {
    // The descriptor is this thread's for its whole life: the poll borrows it,
    // and nothing else may close it. It is dropped with the thread.
    let mut fds = [
        PollFd::new(&fd, PollFlags::HUP | PollFlags::ERR),
        PollFd::new(&wake, PollFlags::HUP | PollFlags::ERR | PollFlags::IN),
    ];
    loop {
        match poll(&mut fds, None) {
            Ok(_) => {}
            // A signal arriving mid-poll interrupts it; the terminal is still
            // watchable and the next poll waits on the same condition.
            Err(Errno::INTR) => continue,
            // The descriptor cannot be polled at all. Nothing here can watch
            // the terminal any more, and there is no road left but
            // [`crate::signals`]'s — so the thread ends rather than spinning
            // on a descriptor that will keep answering the same way.
            Err(_) => return,
        }
        // The stand-down first: a guard on its way out closes the wake pair,
        // and from that moment the process is the exit road's. A hangup
        // reported in the same wake is not this thread's to end.
        if fds[1].revents() != PollFlags::empty() {
            return;
        }
        // Any event on the terminal is an end. The set is HUP|ERR and poll
        // reports `POLLNVAL` whatever is asked of it, so the only events that
        // can appear here are "the terminal is gone" and "the descriptor is not
        // one" — and a mush whose input descriptor is not a descriptor has no
        // event loop to leave either.
        if fds[0].revents() != PollFlags::empty() {
            break;
        }
    }
    end(&shared, bound)
}

/// The three steps a hangup takes, in order: raise, wait, and — if the wait runs
/// out — end it here.
///
/// This is the module's whole decision, and it is one function so that the
/// bound between the roads is read in one place.
fn end(shared: &Shared, bound: Duration) {
    // (a) The flag, so an event loop that can still run takes the ordinary
    // road: the quit a signal asks for, with the flush and the kill walk
    // behind it.
    shared.gone.store(true, Ordering::SeqCst);
    // (b) Then the wait. Not a sleep: a take at any point in it ends this
    // thread's whole business.
    let deadline = Instant::now() + bound;
    loop {
        // Taken: the UI thread is running the exit road, and everything left to
        // do is its.
        if !shared.gone.load(Ordering::SeqCst) {
            return;
        }
        // Stood down: an exit was already in progress when the terminal went,
        // and `main` has taken the process over.
        if shared.stood_down() {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(TAKE_POLL);
    }
    // (c) Nothing took it, so the UI thread is inside the crossterm loop. The
    // fence, held across the decision: `Drop` sets `down` under this same lock,
    // so a stand-down either already happened — read below — or has not begun,
    // and cannot begin until this process is gone. Either way exactly one of
    // the two roads ends mush.
    let down = shared.down.lock().unwrap();
    if *down || !shared.gone.load(Ordering::SeqCst) {
        return;
    }
    process::exit(0)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    /// A bound a test can wait out whole.
    const SHORT: Duration = Duration::from_millis(150);

    /// How long a test waits for something a thread must do, and for the
    /// absence of something it must not. Generous on purpose: these are
    /// scheduler crossings, and a test that is slow is a test that still says
    /// what it says.
    const BREATH: Duration = Duration::from_millis(300);

    /// The environment variable that puts this test binary on the *death* road:
    /// a watcher whose flag nobody takes, which must end the process at its
    /// bound. Set by the parent on its child only; a plain run never sees it.
    const END_WHEN_GONE: &str = "MUSH_HANGUP_TEST_END_WHEN_GONE";

    /// The test the re-exec'd binary runs, by its full name with `--exact`.
    const DEATH_TEST: &str = "hangup::tests::a_wedge_ends_the_process_at_the_bound";

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("a socketpair the test can close one end of")
    }

    /// Wait for `check`, up to `bound`, at a scale coarser than [`TAKE_POLL`].
    fn wait_for(check: impl Fn() -> bool, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        loop {
            if check() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Watch one end of a pair, as `watch` walks the production road.
    fn armed_on(fd: OwnedFd, shared: &Arc<Shared>, bound: Duration) -> Watcher {
        watch_fd(fd, bound, shared.clone()).expect("the watcher arms")
    }

    /// A descriptor of its own for the watcher, so the test keeps the original
    /// to read.
    fn dup(fd: &impl AsFd) -> OwnedFd {
        fd.as_fd().try_clone_to_owned().expect("a duplicate")
    }

    /// A real pty: the master and the slave, from the `pty` road of the rustix
    /// the tree already links (the same crate `flock` comes from).
    ///
    /// `TIOCGPTPEER` rather than a `ptsname` and an `open`: it is one step, it
    /// cannot land on another terminal's name, and it is the Linux road the
    /// kernel documents for exactly this.
    ///
    /// **`CLOEXEC` is load-bearing, and its absence was a measured flake.** This
    /// test binary forks children of its own — `lock`'s holder and this
    /// module's re-exec'd death road — and a descriptor without it is copied
    /// into a child and *survives the exec*, so a master a live child is
    /// holding is a master that has not closed: the slave is not hung up until
    /// that child dies, which is longer than the bound below (measured
    /// directly: a forked copy of the master held `POLLHUP` off the slave for
    /// the copy's whole life, and the slave reported `POLLHUP|POLLERR` the
    /// moment it died). One run in the suite's own parallelism read red before
    /// this, with the death test's child the only other fork in it.
    /// Close-on-exec leaves only the fork-to-exec window, and a copy that dies
    /// at `exec` still leaves the hangup reported a few milliseconds later —
    /// so the property is asserted below rather than trusted.
    fn pty_pair() -> (OwnedFd, OwnedFd) {
        use rustix::io::{fcntl_getfd, FdFlags};
        use rustix::pty::{ioctl_tiocgptpeer, openpt, unlockpt, OpenptFlags};
        let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
        let master = openpt(flags).expect("a pty master");
        // A fresh pty is *locked*: its slave cannot be opened — `TIOCGPTPEER`
        // included — until this, which is the modern spelling of the
        // `grantpt`/`unlockpt` pair (`grantpt` is a no-op on Linux).
        unlockpt(&master).expect("the pair unlocks");
        let slave = ioctl_tiocgptpeer(&master, flags).expect("the master's slave");
        for fd in [&master, &slave] {
            let set = fcntl_getfd(fd).expect("the descriptor's flags");
            assert!(
                set.contains(FdFlags::CLOEXEC),
                "a pty descriptor another test's child could hold: {set:?}"
            );
        }
        (master, slave)
    }

    /// A peer that goes away raises the flag, and exactly once: the raising is a
    /// swap, so the caller that takes it is the one that spends it.
    ///
    /// The order of the last three lines is the rule every test here follows:
    /// *take the flag and stand the watcher down before asserting anything*.
    /// A watcher still waiting at an assertion is a watcher that ends the
    /// process at its bound — and a red assertion would take every other test
    /// in the binary with it.
    #[test]
    fn a_peer_that_goes_away_fires_the_watcher() {
        let (held, peer) = pair();
        let shared = Arc::new(Shared::default());
        let watcher = armed_on(dup(&held), &shared, SHORT);

        assert!(
            !shared.gone.load(Ordering::SeqCst),
            "a live peer raises nothing"
        );
        drop(peer);
        let raised = wait_for(|| shared.gone.load(Ordering::SeqCst), BREATH * 3);
        let taken = shared.take();
        drop(watcher);
        assert!(raised, "the close raises the flag");
        assert!(taken, "and the raise is there to be taken");
        assert!(!shared.take(), "and taking it clears it");
    }

    /// A terminal that is only *open* raises nothing, ever: the thread blocks on
    /// a condition, not on readability, so an idle terminal costs a sleeping
    /// task and a keypress is never eaten by the watcher.
    #[test]
    fn a_live_terminal_raises_nothing() {
        let (held, peer) = pair();
        let shared = Arc::new(Shared::default());
        let watcher = armed_on(dup(&held), &shared, SHORT);

        // Writing on the peer is the "a key was pressed" shape: it must not
        // look like an end, and the byte must still be there for whoever reads
        // the terminal.
        (&peer).write_all(b"a keypress").expect("a write");
        thread::sleep(BREATH * 3);
        let raised = shared.take();
        drop(watcher);
        assert!(!raised, "an open terminal is not a hangup");

        // And a stood-down watcher stays down: the peer going away afterwards
        // is nobody's business of its own.
        drop(peer);
        thread::sleep(BREATH);
        assert!(!shared.take(), "a stood-down watcher raises nothing");
    }

    /// The stand-down is what keeps the hard path out of an exit already in
    /// progress: the terminal goes, nobody can take the flag — the UI thread is
    /// inside crossterm — and `main`'s stand-down, on its way to
    /// `App::shutdown`, is what must decide the process's fate.
    ///
    /// There is no assertion to make about a death that must not happen beyond
    /// this: the test *running* past the bound is the assertion, and it is the
    /// only one that could be made in this process.
    #[test]
    fn a_stand_down_ends_a_hangup_already_in_flight() {
        let (held, peer) = pair();
        let shared = Arc::new(Shared::default());
        let watcher = armed_on(dup(&held), &shared, SHORT);

        drop(peer);
        // The watcher is at or near its poll; the guard takes the process over
        // at once, exactly as `main` does between the loop and the exit road.
        drop(watcher);
        // Twice the bound the death would have come at, and then an assertion:
        // the line after this one running at all is the real proof, because a
        // watcher that had died at its bound would have ended this process.
        thread::sleep(SHORT * 4);
        assert!(
            shared.stood_down(),
            "the stand-down is a fact the watcher can read"
        );
    }

    /// The loop's own road, in this process: a watcher armed the way [`watch`]
    /// arms one — published in the process-global slot [`take`] reads — and
    /// asked with the free [`take`] the event loop calls from
    /// [`crate::take_signal_quit`].
    ///
    /// The socketpair is this test's and the global is only ever read beside
    /// it, so nothing here races a sibling; and the global being *set* is the
    /// point — the pointer the loop holds is the thing production wiring could
    /// get wrong, and it is the only road the other tests cannot walk.
    #[test]
    fn the_loop_takes_the_flag_a_real_watcher_raised() {
        let (held, peer) = pair();
        let watcher = arm(dup(&held), SHORT).expect("the watcher arms");
        assert!(!take(), "a live terminal raises nothing the loop can take");
        drop(peer);
        let raised = wait_for(take, BREATH * 3);
        let taken_again = take();
        drop(watcher);
        assert!(raised, "the loop's own question sees the hangup");
        assert!(!taken_again, "and taking it clears it for the next frame");
    }

    /// The real condition, on a real pty: the master closes and the slave is
    /// hung up.
    ///
    /// The socketpair above proves the watcher; this proves the *signal it waits
    /// for*. Linux's rule — a hangup is reported on a pty's slave the moment the
    /// last master closes — is the whole premise of the module, and it is not
    /// taken from a manual here: the pair is allocated, the master is closed,
    /// and both halves of what the incident measured are asserted: the watcher
    /// fires, and the slave's `read` answers `Ok(0)` — the answer crossterm's
    /// `TTY_TOKEN` arm loops on for ever instead of leaving.
    #[test]
    fn a_pty_whose_master_closes_is_a_hangup() {
        use rustix::io::read;
        let (master, slave) = pty_pair();
        let shared = Arc::new(Shared::default());
        let watcher = armed_on(dup(&slave), &shared, SHORT);

        drop(master);
        let raised = wait_for(|| shared.gone.load(Ordering::SeqCst), BREATH * 3);
        // The read is the incident's other half, taken while the descriptor is
        // still the hung-up slave.
        let mut buffer = [0u8; 16];
        let read = read(&slave, &mut buffer);
        let taken = shared.take();
        drop(watcher);
        assert!(raised, "a master closing is a hangup on its slave");
        // The measured answer, asserted as the measurement it is: Linux's pty
        // slave reads 0 once the master is gone, which is exactly the `Ok(0)`
        // crossterm's inner loop cannot leave. A kernel that answered `EIO`
        // instead would leave the same loop spinning by its other arm, and this
        // line would say so rather than assume it.
        match read {
            Ok(0) => {}
            Ok(bytes) => {
                panic!("the hung-up slave read {bytes} bytes, not the 0 crossterm spins on")
            }
            Err(error) => panic!("the hung-up slave read failed with {error}, not 0 bytes"),
        }
        assert!(taken, "and the raise is there to be taken");
    }

    /// A terminal that is not there to be watched is a *reason*, never a
    /// refusal: `main` says it and runs without a watcher, the shape
    /// `attach::serve`'s failed bind has.
    ///
    /// What is asserted is the failure road of [`tty`]'s second half: a process
    /// with no controlling terminal — a cron job, a daemon — reaches
    /// `/dev/tty` with nothing behind the name, and that is the shape the error
    /// has to carry. The choosing half (stdin when it is a terminal, `/dev/tty`
    /// when it is not) is crossterm's rule mirrored, and cannot be walked from
    /// inside a test process whose stdin is decided by whoever ran the suite.
    #[test]
    fn a_terminal_that_cannot_be_opened_is_a_reason() {
        let error = open_tty(Path::new("/nonexistent/tty")).expect_err("no such file");
        assert!(
            error.contains("/nonexistent/tty"),
            "the reason names it: {error}"
        );
        assert!(
            error.contains("stdin is not a terminal"),
            "and says which half of the choice failed: {error}"
        );
    }

    /// The hard path, in a process of its own: a watcher whose flag nobody takes
    /// must end the process at its bound, and not before it.
    ///
    /// This is the finding's own road — the UI thread wedged inside crossterm's
    /// read loop, unable to take anything — and a test cannot walk it in its own
    /// process, because the road's whole point is that the process ends. So this
    /// test is the `lock` module's pattern: the binary re-execs itself, the
    /// environment variable [`END_WHEN_GONE`] selects the road inside this test,
    /// and the parent measures what the child did.
    ///
    /// What the parent asserts, and what each assertion is for: the child says it
    /// is armed and that the terminal went (so the road was really walked, not
    /// skipped), it never says the line after the sleep (so the death was the
    /// watcher's and not the child's road ending by itself — which exits 3), the
    /// status is 0 (the same status the ordinary road would leave), and the
    /// elapsed time is at least the bound (so step 2 really waited) and far
    /// short of the sleep behind it.
    #[test]
    fn a_wedge_ends_the_process_at_the_bound() {
        if std::env::var_os(END_WHEN_GONE).is_some() {
            end_when_gone();
        }
        use std::process::{Command, Stdio};
        let mut child = Command::new(std::env::current_exe().expect("this test binary's path"))
            .args(["--exact", DEATH_TEST, "--nocapture"])
            .env(END_WHEN_GONE, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the child starts");
        let started = Instant::now();
        // Bounded, so a child that never dies fails this test instead of
        // hanging the suite: the bound is the whole point of the road.
        let status = wait_for_child(&mut child, TAKE_BOUND * 10);
        let elapsed = started.elapsed();
        let mut said = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read as _;
            let _ = stderr.read_to_string(&mut said);
        }
        let status = status.unwrap_or_else(|| {
            let _ = child.kill();
            panic!("the child never ended; it said: {said:?}")
        });
        assert!(
            said.contains("gone"),
            "the child reached the hangup: {said:?}"
        );
        assert!(
            !said.contains("still here"),
            "the child's own road ended it, not the watcher: {said:?}"
        );
        assert_eq!(
            status.code(),
            Some(0),
            "the death leaves the status a quit does"
        );
        assert!(
            elapsed >= TAKE_BOUND,
            "the bound is waited out before the death: {elapsed:?}"
        );
        assert!(
            elapsed < TAKE_BOUND * 5,
            "and promptly after it: {elapsed:?}"
        );
    }

    /// Wait for a child to end, up to `bound`, and reap it. `None` is a child
    /// that outlived its bound (still running, and the caller's to kill).
    fn wait_for_child(
        child: &mut std::process::Child,
        bound: Duration,
    ) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + bound;
        loop {
            match child.try_wait().expect("the child is waitable") {
                Some(status) => return Some(status),
                None if Instant::now() >= deadline => return None,
                None => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    /// The death road, run by this test binary re-exec'd with [`END_WHEN_GONE`]
    /// set: arm a watcher, take nothing, and let the bound run out.
    ///
    /// The peer is a socketpair rather than a pty because the road under test is
    /// the *bound* and the death, not the hangup — the pty is
    /// [`a_pty_whose_master_closes_is_a_hangup`]'s. The sleep is long enough that
    /// nothing but the watcher can end this process inside the parent's window.
    fn end_when_gone() -> ! {
        let (held, peer) = pair();
        let shared = Arc::new(Shared::default());
        let _watcher = armed_on(dup(&held), &shared, TAKE_BOUND);
        eprintln!("mush-hangup-child: armed");
        drop(peer);
        eprintln!("mush-hangup-child: gone");
        thread::sleep(TAKE_BOUND * 10);
        eprintln!("mush-hangup-child: still here");
        process::exit(3)
    }
}
