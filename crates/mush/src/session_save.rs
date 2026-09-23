//! The session-save seam: how the conversation reaches `.mush/session.json`
//! without the UI thread waiting on a disk.
//!
//! The write used to happen in the event handler: every streamed message —
//! every tool result — rebuilt the whole session (the root transcript and each
//! subagent's, which is what makes a follow-up survive a restart) and
//! serialized it on the thread that paints. On a workspace with a dozen agents
//! that is megabytes of JSON per response, and the human feels it as lag on
//! every tool call.
//!
//! Now the UI thread hands a snapshot over and keeps painting. One thread
//! writes: the newest snapshot wins, because one still waiting is *replaced*
//! rather than queued behind — so a burst costs a single write carrying the
//! latest state, and an older snapshot can never land after a newer one. A
//! write that fails does not consume its snapshot: the writer keeps it and
//! tries again on the next wake, and on its own final drain when the app is
//! dropped, so a full disk costs the human a delay and never the conversation
//! tail behind it (finding R2). [`SessionSave::flush`] is the other half: the
//! transitions that mean "this must be on disk" wait for their own write, a
//! cost paid once per human action instead of once per response.

use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};

use mush_core::session::Session;

use crate::lock;

/// How long a flush waits for its write before it reports the session as not
/// saved.
///
/// The number bounds the *caller's* wait, not the disk's work: `flush` runs on
/// the thread that reads every key, resize and `Ctrl-Q`, so a write wedged on
/// an NFS mount or a full disk must not hold the UI at all. Ten seconds is far
/// past what a local write owes and far short of a freeze.
///
/// A flush that reaches the deadline has not failed the write: the worker is
/// still writing, and when it finishes it answers whoever is waiting then. The
/// deadline is reported as a failure anyway — the human asked for "on disk"
/// and it is not — and the next flush parks a fresh waiter, so the write is
/// asked for again; the abandoned waiter is taken back out of the queue, so a
/// run of timeouts cannot grow it without bound. What the deadline buys the
/// human is a report where a freeze used to be.
const FLUSH_DEADLINE: Duration = Duration::from_secs(10);

/// How long the exit road waits for the writer's own thread after the wake
/// channel is closed.
///
/// [`FLUSH_DEADLINE`] bounds the *caller's* wait for one write; this one bounds
/// the wait for the thread itself, which has nothing left to write once the
/// wake channel is gone and is normally already asleep. The number is shorter
/// than the flush's on purpose: the exit road has already flushed what it
/// could, and every moment after that is a moment the human's terminal is still
/// held and the workspace lock is still taken. Two seconds is far past what
/// ending a thread with an empty queue owes. When it expires the process exits
/// anyway — the thread is left behind — and the reason is handed to the caller,
/// which is the exit road, so the human reads it instead of meeting a mush that
/// will not end (finding R4).
const JOIN_DEADLINE: Duration = Duration::from_secs(2);

/// How long a bounded wait sleeps between polls of the thing it waits for.
///
/// `std` has no wait-with-deadline for a thread or a child, so every bound in
/// this tree is a poll loop, and this is the period this module's one uses. It
/// is small because the thing waited for is normally already done.
const POLL: Duration = Duration::from_millis(5);

/// Where a snapshot of the session is written.
///
/// The snapshot is the caller's; the serialization and the disk are the
/// implementation's, on the implementation's own thread. Nothing here is
/// allowed to make the UI thread wait for a file unless the caller asked to
/// wait (see [`SessionSave::flush`]).
pub trait SessionSave: Send + Sync {
    /// Hand a snapshot over, without waiting for it. Cheap enough for the
    /// message path: the newest snapshot wins, so a burst of messages costs one
    /// write that carries the latest state. A write that fails does not consume
    /// the snapshot — the writer keeps it and tries again on its next wake, and
    /// on its own final drain, so the road back is never lost (finding R2).
    fn save(&self, session: Session);

    /// Wait until everything handed over so far is on disk. For the call sites
    /// that mean "this must not be lost" — the human's own message, a command
    /// that changed what is stored, quitting — rather than for a streamed
    /// response, which is covered by the debounce and the exit flush.
    ///
    /// A write that fails is not retried behind the waiter's back: the call
    /// returns with the failure left in [`Self::take_error`], and the snapshot
    /// stays with the writer for its next wake (finding R2).
    ///
    /// The wait is bounded twice over. Its deadline is [`FLUSH_DEADLINE`]
    /// (finding R4), and a signal that has already asked mush to quit and then
    /// been pressed again ends it at the next poll (finding R3): the second
    /// press is the human saying no wait on the exit road survives it.
    fn flush(&self);

    /// The last write's failure, once. A write that failed on the writer's
    /// thread has no caller to return to, and swallowing it would let a
    /// workspace mush cannot write to look saved. `None` when nothing failed.
    fn take_error(&self) -> Option<String>;

    /// Whether the store is still this mush's, asked before a store write that
    /// does not go through the worker.
    ///
    /// The worker gates its own saves with the lock's identity, and that gate
    /// has to cover *every* store write, or a mush whose lock's name was
    /// replaced — the human's `mv`, a restore from a backup — still writes the
    /// road that does not pass through the worker: the `.mush/session.json.previous`
    /// copy Ctrl-N keeps, which is the slot the window that now owns the store
    /// points its own warning at (finding R9). `Ok(())` when the lock's name
    /// still leads to the locked inode, or when there is no lock to ask (a
    /// writer built without one — a test, or a road that never took one).
    fn store_is_mine(&self) -> Result<(), String>;

    /// End the writer: stop taking snapshots and wait, bounded, for the thread
    /// that owns the disk. The reason the thread outlived the bound, for a
    /// caller with somewhere to say it; `None` for the ordinary ending and for
    /// an implementation with no thread of its own.
    ///
    /// This is the exit road's last wait before the process goes, and it is
    /// bounded like the flush it follows (finding R4): a worker wedged inside a
    /// write on a mount that stopped answering must not keep mush alive with
    /// its terminal held and its workspace lock taken. A second signal ends it
    /// at the next poll too (finding R3). The write is not lost by ending the
    /// wait — it never landed — and the thread is left behind, which is the
    /// price of a process that leaves when it was asked to.
    fn close(&self) -> Option<String> {
        None
    }
}

/// The real seam: one writer thread, the newest snapshot, and the one a failed
/// attempt could not write.
///
/// The write itself is `Session::save`, unchanged — same path, same fields,
/// same format, still read by `Session::load`. Pretty, not compact: this file is
/// what a human opens to see what mush remembered, and the pretty-printing now
/// costs a thread that has nothing else to do rather than a dropped frame.
pub struct Writer {
    inner: Arc<Inner>,
    /// Capacity one, and `None` once the worker is being stopped: a wake-up
    /// already queued is enough, because the worker empties the slot before it
    /// sleeps again, so no hand-over can be left unwritten by a lost token.
    ///
    /// Behind a mutex because the exit road stops the writer through the
    /// [`SessionSave`] seam, which takes `&self` ([`Writer::close`], finding
    /// R4); the mutex is never held for longer than a `take`.
    wake: Mutex<Option<Sender<()>>>,
    /// The worker's thread: taken and joined, bounded, by [`Writer::close`], and
    /// by [`Drop`] as the backstop for a writer nobody closed.
    worker: Mutex<Option<JoinHandle<()>>>,
}

struct Inner {
    root: PathBuf,
    /// The lock this store belongs to, when the caller has one: every save asks
    /// it whether the name still leads to the inode that was locked, and writes
    /// nothing when it does not. `None` for a writer built without a lock (a
    /// test, or a road that never took one).
    lock: Option<lock::Identity>,
    pending: Mutex<Pending>,
    /// How many writes have been attempted. Only a test reads it — a burst's
    /// cost is otherwise invisible from outside the writer.
    #[cfg(test)]
    writes: AtomicUsize,
    failed: Mutex<Option<String>>,
    /// Whether the worker thread is there to answer a flush.
    ///
    /// Set when the thread is started and cleared by the [`Alive`] guard on the
    /// thread's stack, so it goes down on *any* way the thread can end — a
    /// panic, an early return — and never for a write that is merely slow. A
    /// flush reads it before it parks a waiter: a worker that is gone will
    /// never wake one, and waiting for it is exactly how the UI froze (finding
    /// E7).
    alive: AtomicBool,
}

impl Inner {
    /// Leave a failure where the UI's next tick will find it, and hand it back
    /// to a caller that can be the test.
    fn fail(&self, message: impl Into<String>) -> String {
        let message = message.into();
        *self.failed.lock().unwrap() = Some(message.clone());
        message
    }

    /// The lock's verdict on the store: whether the name still leads to the
    /// inode this process locked, or `Ok(())` when there is no lock to ask. One
    /// spelling for the two writers that need it — the worker before every save
    /// it makes, and [`SessionSave::store_is_mine`] for a write that does not go
    /// through the worker — so the two gates cannot disagree.
    fn store_is_mine(&self) -> Result<(), String> {
        match &self.lock {
            Some(identity) => identity.still_mine(),
            None => Ok(()),
        }
    }
}

/// What the writer still owes.
#[derive(Default)]
struct Pending {
    /// The newest snapshot handed over. There is only ever one: a newer one
    /// makes an older one pointless to write.
    snapshot: Option<Session>,
    /// Whoever is waiting for the file itself rather than for one write.
    waiting: Vec<Sender<()>>,
}

impl Pending {
    fn take(&mut self) -> (Option<Session>, Vec<Sender<()>>) {
        (self.snapshot.take(), std::mem::take(&mut self.waiting))
    }
}

/// Clears [`Inner::alive`] from the worker's own stack.
///
/// A `Drop` guard rather than a line at the end of [`writer`], because a panic
/// unwinds the stack and skips the line — and a worker that died by panic is
/// exactly the case a flush must not wait on. The guard is made before the loop
/// and dropped after it, however it ends.
struct Alive(Arc<Inner>);

impl Drop for Alive {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::SeqCst);
    }
}

impl Writer {
    /// The writer for `root`. `lock` is the store's lock when the caller has
    /// one: every save then first asks the lock's name whether it still leads
    /// to the inode that was locked, and writes nothing when it does not
    /// ([`lock::Identity::still_mine`]). A whole-file write or an `mv` over the
    /// lock's name is the one way a store comes to have two owners that no
    /// tool-level guard can see (finding E2); `None` is a writer built with no
    /// lock to check (a test, or a road that never took one).
    ///
    /// A thread the OS will not give is returned as the error it is, never
    /// raised (see [`Self::run`]).
    pub fn new(root: PathBuf, lock: Option<lock::Identity>) -> Result<Self, String> {
        let mut writer = Self::parked(root, lock);
        writer.run()?;
        Ok(writer)
    }

    /// A writer that will never write, and the reason why: what `main` keeps
    /// when [`Self::new`]'s thread will not start.
    ///
    /// The session cannot be saved by this writer — `save` parks snapshots
    /// nothing takes, and every `flush` fails at once through `take_error` —
    /// but the app still runs, and the human reads the reason on the status
    /// line instead of a startup that refuses the workspace over a thread. It
    /// carries the lock's identity like any other writer: a store whose lock
    /// was replaced is refused before it is a store with two owners.
    pub fn without_worker(root: PathBuf, lock: Option<lock::Identity>, reason: String) -> Self {
        let writer = Self::parked(root, lock);
        writer.inner.fail(reason);
        writer
    }

    /// Everything but the thread, so a test can hand snapshots over before
    /// anything can write them — which is how what a burst costs is seen
    /// instead of raced. `new` is this and then [`Self::run`].
    fn parked(root: PathBuf, lock: Option<lock::Identity>) -> Self {
        Self {
            inner: Arc::new(Inner {
                root,
                lock,
                pending: Mutex::new(Pending::default()),
                #[cfg(test)]
                writes: AtomicUsize::new(0),
                failed: Mutex::new(None),
                alive: AtomicBool::new(false),
            }),
            wake: Mutex::new(None),
            worker: Mutex::new(None),
        }
    }

    /// Start the writer's thread.
    ///
    /// The thread carries a name — `mush-save`, where every other thread in the
    /// process is `mush-agent-{id}` or `mush-job-{id}` — so a panic on it says
    /// which thread died instead of `<unnamed>`. A thread the OS refuses is
    /// *returned*, not raised: the caller runs with [`Self::without_worker`]
    /// and tells the human, because a process with nowhere to save is a smaller
    /// loss than one that will not start.
    fn run(&mut self) -> Result<(), String> {
        let (wake, woken) = bounded::<()>(1);
        let inner = self.inner.clone();
        // Set *before* the spawn, never after: a worker that ran and died
        // before this line would otherwise leave a stale `true` behind, and the
        // next flush would park a waiter nothing wakes — the freeze this flag
        // exists to prevent. The spawn's failure path stores it back to `false`;
        // the guard on the thread's stack clears it on every other way out.
        self.inner.alive.store(true, Ordering::SeqCst);
        let started = thread::Builder::new()
            .name("mush-save".to_string())
            .spawn(move || {
                let _alive = Alive(inner.clone());
                writer(inner, woken);
            });
        match started {
            Ok(worker) => {
                *self
                    .worker
                    .get_mut()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(worker);
                *self
                    .wake
                    .get_mut()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(wake);
                Ok(())
            }
            Err(error) => {
                self.inner.alive.store(false, Ordering::SeqCst);
                Err(format!("could not start the session writer: {error}"))
            }
        }
    }

    /// Nudge the worker: there is something to write (or someone to wake).
    fn poke(&self) {
        if let Some(wake) = self
            .wake
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            let _ = wake.try_send(());
        }
    }

    /// How many writes have been attempted, so a test can assert that a burst of
    /// messages cost one.
    #[cfg(test)]
    pub fn writes(&self) -> usize {
        self.inner.writes.load(Ordering::SeqCst)
    }
}

impl SessionSave for Writer {
    fn save(&self, session: Session) {
        // Replaced, not queued: two snapshots would be two writes of the same
        // conversation a moment apart, and the file only ever needs the later
        // one. Being the newest is what makes a snapshot safe to drop.
        self.inner.pending.lock().unwrap().snapshot = Some(session);
        self.poke();
    }

    fn flush(&self) {
        let _ = self.flush_within(FLUSH_DEADLINE, crate::signals::forced);
    }

    fn take_error(&self) -> Option<String> {
        self.inner.failed.lock().unwrap().take()
    }

    fn store_is_mine(&self) -> Result<(), String> {
        self.inner.store_is_mine()
    }

    fn close(&self) -> Option<String> {
        self.close_within(JOIN_DEADLINE, crate::signals::forced)
    }
}

impl Writer {
    /// [`SessionSave::close`], with the deadline a parameter so a test can hold
    /// the clock instead of the clock holding the test.
    ///
    /// The wake channel is closed first — the same hand-over [`Drop`] makes:
    /// the worker drains whatever is pending, then ends — and then the thread is
    /// waited for, bounded, instead of for as long as the disk wants (findings
    /// R4, R3: `deadline` is the bound, `forced` the second signal that cuts
    /// the wait short of it).
    fn close_within(&self, deadline: Duration, forced: impl Fn() -> bool) -> Option<String> {
        // Dropping the sender is what tells the worker there is nothing more
        // coming; it drains what is pending on the way out (`writer`).
        drop(
            self.wake
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take(),
        );
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        Self::join_within(worker, deadline, forced)
    }

    /// Wait, bounded, for the worker's thread, saying so when the deadline
    /// passes first.
    ///
    /// `std` has no `join` with a deadline, so this is the poll loop [`POLL`]
    /// exists for. A thread that outlives the deadline is left behind — its
    /// `JoinHandle` is dropped, which detaches it — because the process is
    /// leaving either way and a waited-for thread is the one thing that can
    /// keep it here. `forced` is the second signal (finding R3): a press that
    /// has already asked for the quit and come back ends the wait at the next
    /// poll, before the deadline, and says so in the same shape.
    fn join_within(
        worker: Option<JoinHandle<()>>,
        deadline: Duration,
        forced: impl Fn() -> bool,
    ) -> Option<String> {
        let worker = worker?;
        let started = Instant::now();
        while !worker.is_finished() {
            if forced() {
                return Some(format!(
                    "a second signal hurried the exit — the session writer had not finished after \
                     {:?}, so the last write may not have landed",
                    started.elapsed()
                ));
            }
            if started.elapsed() >= deadline {
                return Some(format!(
                    "the session writer did not finish within {deadline:?} — the last write may \
                     still be in flight"
                ));
            }
            thread::sleep(POLL);
        }
        let _ = worker.join();
        None
    }
    /// [`SessionSave::flush`], with the deadline a parameter so a test can hold
    /// the clock instead of the clock holding the test.
    ///
    /// Four things end the wait, and none of them is "as long as it takes":
    /// the worker answering (the file is then current), the worker being gone
    /// (a waiter nothing can wake is never parked), the deadline (the write is
    /// still in flight; the waiter is taken back out of the queue and the
    /// failure is left in [`SessionSave::take_error`]), or a second signal
    /// (finding R3: the human has said no wait survives their next press). The
    /// deadline is not sticky — it does not mark the worker dead — so the next
    /// flush parks a fresh waiter and asks again, which is what makes a write
    /// that lands late still land.
    ///
    /// The wait is polled in [`POLL`] slices rather than handed to one
    /// `recv_timeout` so the hurry can be read in the same slice as the press:
    /// a thread blocked in a ten-second receive cannot hear anything.
    fn flush_within(&self, deadline: Duration, forced: impl Fn() -> bool) -> Result<(), String> {
        if !self.inner.alive.load(Ordering::SeqCst) {
            return Err(self
                .inner
                .fail("the session writer is gone — the session was not saved"));
        }
        let (done, waited) = bounded(1);
        self.inner
            .pending
            .lock()
            .unwrap()
            .waiting
            .push(done.clone());
        self.poke();
        let started = Instant::now();
        // The worker sends on the waiter after the write in front of it lands,
        // so a `recv` that returns means the file is current; a `recv` that
        // disconnects means the worker died without answering.
        loop {
            if forced() {
                self.give_up(&done);
                return Err(self.inner.fail(format!(
                    "a second signal hurried the exit — the session writer had not answered after \
                     {:?}, so the write may not have landed",
                    started.elapsed()
                )));
            }
            match waited.recv_timeout(POLL) {
                Ok(()) => return Ok(()),
                Err(RecvTimeoutError::Timeout) if started.elapsed() < deadline => continue,
                Err(RecvTimeoutError::Timeout) => {
                    self.give_up(&done);
                    if !self.inner.alive.load(Ordering::SeqCst) {
                        return Err(self
                            .inner
                            .fail("the session writer is gone — the session was not saved"));
                    }
                    return Err(self.inner.fail(format!(
                        "the session writer did not answer within {deadline:?} — the write is still in flight, and the next flush will ask again"
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => return Err(self.inner.fail(
                    "the session writer died before the write landed — the session was not saved",
                )),
            }
        }
    }

    /// Take this caller's waiter back out of the queue, on either way a wait can
    /// end without an answer.
    ///
    /// The worker may still be inside the write; the answer it owes is owed to
    /// whoever waits then, not to this caller, and a waiter left in the queue
    /// would grow the queue once per give-up. The worker holds the queue only
    /// long enough to empty it, so taking ours out cannot become a second
    /// unbounded wait.
    fn give_up(&self, done: &Sender<()>) {
        self.inner
            .pending
            .lock()
            .unwrap()
            .waiting
            .retain(|waiter| !waiter.same_channel(done));
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Closing the wake channel ends the worker once it has written whatever
        // was pending. Joining it keeps a write from outliving the app that
        // asked for it — the exit flush's snapshot is on disk before this
        // returns — but bounded (finding R4): a worker wedged on a mount that
        // stopped answering must not hold the process that is leaving. The exit
        // road closes the writer before this ([`SessionSave::close`]), so this
        // is the backstop for every other way a writer is dropped; the reason a
        // thread outlived the bound has no caller left to return to, and
        // stderr is the one door still open.
        let wake = self
            .wake
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(wake);
        let worker = self
            .worker
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(reason) = Self::join_within(worker, JOIN_DEADLINE, crate::signals::forced) {
            eprintln!("mush: {reason}");
        }
    }
}

/// Write snapshots until nothing is waiting, then sleep until poked again.
fn writer(inner: Arc<Inner>, woken: Receiver<()>) {
    // The snapshot an attempt could not write, kept here on the thread that
    // tried: the failed attempt did not consume it (`Session::save` borrows),
    // so the next wake — a hand-over, a flush, or this thread's final drain
    // when the writer is dropped — writes the same conversation instead of one
    // that was lost with the attempt (finding R2). One is kept, and a newer
    // hand-over replaces it: the newest state is the only one worth writing.
    let mut retry: Option<Session> = None;
    loop {
        // The sender going away is the writer being dropped; the drain below
        // still runs, so a hand-over that raced the drop is not lost.
        let woke = woken.recv().is_ok();
        retry = drain(&inner, retry);
        if !woke {
            return;
        }
    }
}

/// Write everything handed over so far, then wake whoever waited for it.
///
/// One thread, so one write at a time: at most one snapshot is in flight, and
/// the only one that can follow it is the newest state.
///
/// `retry` is what a previous attempt could not write. This call is a wake, so
/// it is tried once at the top — unless a newer snapshot is already waiting,
/// which makes it pointless — and a failure is handed back instead of looped
/// on: a full disk does not refill in the next microsecond, and spinning
/// `Session::save` on the worker would only burn the box that is already out
/// of room. The next wake tries again.
fn drain(inner: &Inner, mut retry: Option<Session>) -> Option<Session> {
    if inner.pending.lock().unwrap().snapshot.is_none() {
        if let Some(session) = retry.take() {
            retry = attempt(inner, session);
        }
    }
    loop {
        let (snapshot, waiting) = inner.pending.lock().unwrap().take();
        if snapshot.is_none() && waiting.is_empty() {
            return retry;
        }
        if let Some(session) = snapshot {
            // A hand-over makes whatever a failed attempt was holding
            // pointless: the newest state is the only one worth writing.
            retry = attempt(inner, session);
        }
        // After the write, never before it: a waiter is asking for a file it can
        // read, not for a promise.
        for waiter in waiting {
            let _ = waiter.send(());
        }
    }
}

/// Write `session`, or say why it did not land.
///
/// `Some(session)` is a write that failed with the snapshot intact: the caller
/// keeps it and tries again on the next wake (finding R2). `None` means there is
/// nothing left to write — the file is current, or the store is not this mush's
/// any more, which no retry can change.
fn attempt(inner: &Inner, mut session: Session) -> Option<Session> {
    // The store is one writer's only while the lock's name still leads to the
    // inode this process locked. The model's write road refuses the store's own
    // names, but the name is on the human's own disk: a whole-file write or an
    // `mv` over it replaces it in one step, and from then on a second mush owns
    // the store. Writing the conversation into somebody else's store is exactly
    // the damage the lock exists to prevent, so a save that cannot prove the
    // store is still this mush's writes nothing and leaves the refusal where the
    // UI's tick reads it.
    if let Err(why) = inner.store_is_mine() {
        *inner.failed.lock().unwrap() = Some(why);
        // The refusal is not a bad moment on a disk that may recover: the name
        // has a different inode behind it for good, so a retry would be an
        // attempt to write into another mush's store. The conversation is
        // dropped rather than kept.
        return None;
    }
    let wrote = session.save(&inner.root);
    #[cfg(test)]
    inner.writes.fetch_add(1, Ordering::SeqCst);
    // Nobody is waiting on a background write, so the failure is left where the
    // UI's next tick will find it. A later success does not clear it: the tick
    // polls every frame, and a failure that arrives and is overwritten unseen is
    // a failure that was swallowed.
    match wrote {
        Ok(()) => None,
        Err(error) => {
            *inner.failed.lock().unwrap() = Some(error.to_string());
            Some(session)
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::sync::{Arc, Mutex};

    use mush_core::session::Session;

    use super::SessionSave;

    /// Records every snapshot handed over, and can be told to fail the next
    /// write. A test asserts what the UI thread did — handed a snapshot over, or
    /// not — with no disk and no thread: "the handler built nothing" is exactly
    /// the absence of entries here.
    #[derive(Default)]
    pub struct Recorder {
        saved: Mutex<Vec<Arc<Session>>>,
        failure: Mutex<Option<String>>,
    }

    impl Recorder {
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        /// Everything handed over so far, oldest first.
        pub fn saved(&self) -> Vec<Arc<Session>> {
            self.saved.lock().unwrap().clone()
        }

        /// How many snapshots were handed over at all.
        pub fn len(&self) -> usize {
            self.saved.lock().unwrap().len()
        }

        /// The next error the UI will be told about, as a writer that could not
        /// write one would leave it.
        pub fn fails(self: Arc<Self>, error: &str) -> Arc<Self> {
            *self.failure.lock().unwrap() = Some(error.to_string());
            self
        }
    }

    impl SessionSave for Recorder {
        fn save(&self, session: Session) {
            self.saved.lock().unwrap().push(Arc::new(session));
        }

        fn flush(&self) {}

        fn take_error(&self) -> Option<String> {
            self.failure.lock().unwrap().take()
        }

        /// The recorder has no lock: every store it writes is its own, which
        /// is what a test that does not set one up means.
        fn store_is_mine(&self) -> Result<(), String> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use mush_core::message::Message;
    use mush_core::scratch::{Held, Scratch};
    use mush_core::session::{session_path, Session};

    use super::{SessionSave, Writer, FLUSH_DEADLINE};

    /// A directory to write a session into, and to leave behind nothing: the
    /// guard comes back with the path, so the root goes when the test ends —
    /// panicking or not.
    fn root(label: &str) -> Held<std::path::PathBuf> {
        let dir = Scratch::new(&format!("writer-{label}"));
        let path = dir.path().to_path_buf();
        dir.hold(path)
    }

    /// A session with one user message, so which snapshot landed is readable.
    fn saying(text: &str) -> Session {
        Session {
            model: "test-model".into(),
            provider: "custom".into(),
            base_url: "http://127.0.0.1:1".into(),
            context: None,
            messages: vec![Message::user(text)],
            agents: Vec::new(),
            notices: Vec::new(),
        }
    }

    fn last_message(root: &std::path::Path) -> String {
        let stored = Session::load(root).expect("the writer wrote a session");
        stored
            .messages
            .last()
            .expect("one message")
            .text()
            .to_string()
    }

    /// Whether a `flush` on its own thread returned within `deadline` — the UI
    /// thread's experience of a flush that parks, kept as a probe: a regression
    /// fails the test rather than hanging the suite.
    fn flushed_within(writer: &Arc<Writer>, deadline: Duration) -> bool {
        let (done, waited) = crossbeam_channel::bounded::<()>(1);
        let writer = writer.clone();
        std::thread::spawn(move || {
            writer.flush();
            let _ = done.send(());
        });
        waited.recv_timeout(deadline).is_ok()
    }

    /// Wait, bounded, for the worker's `Drop` guard to clear the flag it owns:
    /// the wait is on the thread's own stack clearing, not a guess at how long
    /// a panic takes.
    fn wait_until_dead(writer: &Writer) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while writer.inner.alive.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the worker never died");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The write lands where `Session::load` looks, in the format it reads: a
    /// session written on the writer's thread is the same file the UI thread
    /// used to write.
    #[test]
    fn a_handed_over_snapshot_is_on_disk_when_flush_returns() {
        let root = root("roundtrip");
        let writer = Writer::new(root.to_path_buf(), None).expect("the worker starts");
        writer.save(saying("hello"));
        writer.flush();
        assert_eq!(last_message(&root), "hello");
        assert_eq!(writer.writes(), 1, "the write happened once");
        let _ = fs::remove_dir_all(&root);
    }

    /// A snapshot still waiting is replaced, not queued behind a newer one — so
    /// a burst of hand-overs costs one write, and the state that lands is the
    /// last one.
    #[test]
    fn a_burst_of_handovers_costs_one_write_and_lands_the_newest() {
        let root = root("newest");
        let mut writer = Writer::parked(root.to_path_buf(), None);
        writer.save(saying("one"));
        writer.save(saying("two"));
        writer.save(saying("three"));
        writer.run().expect("the worker starts");
        writer.flush();
        assert_eq!(writer.writes(), 1, "three snapshots are one write");
        assert_eq!(last_message(&root), "three", "and the newest one");
        let _ = fs::remove_dir_all(&root);
    }

    /// A flush waits for the file even when nothing was handed over: a caller
    /// that says "must be on disk" is never left reading yesterday's file.
    #[test]
    fn a_flush_with_nothing_pending_still_returns() {
        let root = root("empty");
        let writer = Writer::new(root.to_path_buf(), None).expect("the worker starts");
        writer.flush();
        assert!(!session_path(&root).exists(), "nothing was handed over");
        writer.save(saying("after"));
        writer.flush();
        assert_eq!(last_message(&root), "after");
        let _ = fs::remove_dir_all(&root);
    }

    /// A write that failed is not the end of the road: the attempt did not
    /// consume the snapshot, so the next wake — a flush that hands nothing new
    /// over, and the writer's own final drain when the app is dropped — writes
    /// the same conversation instead of one that was lost with the attempt
    /// (finding R2).
    #[test]
    fn a_failed_write_is_kept_and_lands_on_the_next_wake() {
        let root = root("retained");
        // `.mush` as a *file* is a workspace the session cannot be written to,
        // exactly as a full disk or a read-only checkout would be.
        fs::write(root.join(".mush"), "not a directory").unwrap();
        let writer = Writer::new(root.to_path_buf(), None).expect("the worker starts");
        writer.save(saying("kept"));
        writer.flush();
        assert!(writer.take_error().is_some(), "the attempt is reported");
        assert!(!session_path(&root).exists(), "and nothing landed");

        // The disk comes back: the next wake is this flush, which hands
        // nothing over — the retry is the point.
        fs::remove_file(root.join(".mush")).unwrap();
        writer.flush();
        assert_eq!(last_message(&root), "kept", "the retained snapshot landed");
        assert_eq!(
            writer.writes(),
            2,
            "two attempts: the failed one and the retry"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The writer's own final drain is one more attempt: a snapshot a failed
    /// write left behind is written when the writer is dropped — the road that
    /// still goes out when the UI never saw the failure, or quit before its
    /// next tick could (finding R2).
    #[test]
    fn a_retained_snapshot_lands_when_the_writer_is_dropped() {
        let root = root("drop-retry");
        fs::write(root.join(".mush"), "not a directory").unwrap();
        {
            let writer = Writer::new(root.to_path_buf(), None).expect("the worker starts");
            writer.save(saying("kept"));
            writer.flush();
            assert!(writer.take_error().is_some(), "the attempt is reported");
            // The disk comes back, and nothing else will poke the worker: the
            // drop is the last wake, and its drain carries the snapshot.
            fs::remove_file(root.join(".mush")).unwrap();
        }
        assert_eq!(
            last_message(&root),
            "kept",
            "the writer's final drain wrote the retained snapshot"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A worker that is gone must not take the flush with it: the waiter is
    /// never parked, the call returns at once, and the failure it leaves is the
    /// one the UI's next tick reads.
    ///
    /// Both shapes of "gone" are here: a worker that never started — what a
    /// thread the OS refused leaves behind, and what a worker that died leaves
    /// too — and a worker that died by panic on the queue it took.
    #[test]
    fn a_flush_with_a_dead_writer_returns_with_an_error() {
        let root = root("dead-flush");

        let never = Arc::new(Writer::parked(root.to_path_buf(), None));
        never.save(saying("lost"));
        assert!(
            flushed_within(&never, FLUSH_DEADLINE),
            "a flush with no worker waited past the deadline"
        );
        let error = never.take_error().expect("the missing writer is reported");
        assert!(error.contains("writer"), "the failure names it: {error}");
        assert!(
            error.contains("not saved"),
            "and says what was lost: {error}"
        );

        let died = Arc::new(Writer::new(root.to_path_buf(), None).expect("the worker starts"));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = died.inner.pending.lock().unwrap();
            panic!("poison the queue on purpose");
        }));
        died.poke();
        wait_until_dead(&died);
        assert!(
            flushed_within(&died, FLUSH_DEADLINE),
            "a flush with a dead worker waited past the deadline"
        );
        let error = died.take_error().expect("the death is reported");
        assert!(error.contains("writer"), "the failure names it: {error}");

        let _ = fs::remove_dir_all(&root);
    }

    /// The worker's thread is named, so a panic on it reads
    /// `thread 'mush-save'` where the alternative says `<unnamed>` — every
    /// other thread in the process (`mush-agent-{id}`, `mush-job-{id}`) says
    /// what it is. The name is read from the panicking thread, which is where
    /// the default hook reads it, and formatted the way that hook formats it.
    #[test]
    fn the_writer_thread_is_named() {
        let root = root("named");
        let writer = Writer::new(root.to_path_buf(), None).expect("the worker starts");
        // The worker's own death: the queue it must take is poisoned under it.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = writer.inner.pending.lock().unwrap();
            panic!("poison the queue on purpose");
        }));

        let seen = Arc::new(Mutex::new(None::<String>));
        let previous = Arc::new(std::panic::take_hook());
        {
            let seen = seen.clone();
            let previous = previous.clone();
            std::panic::set_hook(Box::new(move |info| {
                let current = std::thread::current();
                let name = current.name().unwrap_or("<unnamed>");
                if name == "mush-save" {
                    *seen.lock().unwrap() = Some(format!("thread '{name}' {info}"));
                } else {
                    previous(info);
                }
            }));
        }
        writer.poke();
        let deadline = Instant::now() + Duration::from_secs(5);
        while seen.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the worker never panicked");
            std::thread::sleep(Duration::from_millis(1));
        }
        std::panic::set_hook(Box::new(move |info| previous(info)));

        let message = seen.lock().unwrap().clone().expect("the worker panicked");
        assert!(message.contains("mush-save"), "{message}");
        // And the app can read that the worker is gone.
        writer.flush();
        assert!(writer.take_error().is_some(), "the dead worker is reported");
        let _ = fs::remove_dir_all(&root);
    }

    /// A write that is stuck, not dead, is the third way a flush ends: it gives
    /// up at the deadline and reports it, without marking the worker dead and
    /// without leaving its waiter behind — so a later flush parks a fresh one
    /// and asks again, and a run of timeouts cannot grow the queue.
    #[test]
    fn a_timed_out_flush_leaves_the_next_one_able_to_try() {
        let root = root("flush-timeout");
        let writer = Writer::parked(root.to_path_buf(), None);
        // The shape of a worker stuck inside `Session::save`: the thread is
        // there (the flag says so), the answer is not. Nothing is parked on the
        // queue, so the wait can only end at the deadline — which the test
        // holds, because `flush_within` takes it.
        writer.inner.alive.store(true, Ordering::SeqCst);
        writer.save(saying("lost"));

        let started = Instant::now();
        let first = writer
            .flush_within(Duration::from_millis(50), || false)
            .expect_err("a stuck worker is not an answer");
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "the deadline is the clock: {:?}",
            started.elapsed()
        );
        assert!(
            first.contains("in flight"),
            "the report says the write is still going: {first}"
        );
        assert!(
            writer.inner.pending.lock().unwrap().waiting.is_empty(),
            "the abandoned waiter is taken back out"
        );
        assert_eq!(writer.take_error().as_deref(), Some(first.as_str()));

        let _ = writer.flush_within(Duration::from_millis(50), || false);
        assert!(
            writer.inner.pending.lock().unwrap().waiting.is_empty(),
            "and the second timeout does not grow the queue"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A write that fails is not swallowed: it is left for the UI's next tick,
    /// which is the only thing that can put it on the status line.
    #[test]
    fn a_failed_write_is_left_for_the_ui() {
        let root = root("failure");
        // `.mush` as a *file* is a workspace the session cannot be written to,
        // exactly as a full disk or a read-only checkout would be.
        fs::write(root.join(".mush"), "not a directory").unwrap();
        let writer = Writer::new(root.to_path_buf(), None).expect("the worker starts");
        writer.save(saying("lost"));
        writer.flush();
        let error = writer.take_error().expect("the failure is reported");
        assert!(!error.is_empty(), "it says what went wrong: {error}");
        assert!(writer.take_error().is_none(), "and it is reported once");
        let _ = fs::remove_dir_all(&root);
    }

    /// The lock's name is what makes a store this process's. Once it has been
    /// replaced — the human's own `mv`; no tool can do it, `write_file` refuses
    /// the store's own names — this mush's flock is on an orphaned inode and a
    /// second mush owns the store, so the save road refuses to write the
    /// conversation over it and leaves the refusal for the UI (finding E2).
    #[test]
    fn a_save_refuses_a_lock_file_that_was_replaced() {
        let root = root("lock-replaced");
        mush_core::session::ensure_mush_dir(&root).unwrap();
        let guard = crate::lock::acquire(&root).unwrap();
        let writer =
            Writer::new(root.to_path_buf(), Some(guard.identity())).expect("the worker starts");

        // While the name still leads to the locked inode, the writer writes:
        // the check is the lock's identity, not a refusal.
        writer.save(saying("before"));
        writer.flush();
        assert_eq!(last_message(&root), "before");
        assert!(writer.take_error().is_none());

        // A fresh file renamed over the lock's name: the guard's flock is now on
        // a file nobody looks at, and the next start locks this new one.
        let fresh = root.join(".mush/lock.new");
        fs::write(&fresh, "0\n").unwrap();
        fs::rename(&fresh, root.join(".mush/lock")).unwrap();

        writer.save(saying("after the lock was replaced"));
        writer.flush();
        let error = writer.take_error().expect("the refusal is reported");
        assert!(error.contains("was replaced"), "{error}");
        assert_eq!(
            last_message(&root),
            "before",
            "the store still holds the last write it was this mush's to make"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The exit road's bound on the writer's own thread (finding R4): a worker
    /// stuck inside the write — here, on the queue lock the test holds, which is
    /// the shape a wedged `write(2)` leaves — is left at the deadline, and the
    /// reason is handed back for the exit road to say. Before the bound,
    /// [`Drop`]'s `join` waited as long as the disk wanted, holding the terminal
    /// hand-back and the workspace lock with it.
    #[test]
    fn a_stuck_writer_is_left_at_the_deadline_and_the_reason_is_said() {
        let root = root("join-deadline");
        let writer = Writer::new(root.to_path_buf(), None).expect("the worker starts");
        // The worker blocks on the queue, so it is inside its work, not gone.
        let held = writer.inner.pending.lock().unwrap();
        let started = Instant::now();
        let reason = writer
            .close_within(Duration::from_millis(50), || false)
            .expect("a stuck worker outlives the deadline");
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "the deadline is the clock: {:?}",
            started.elapsed()
        );
        assert!(reason.contains("did not finish"), "{reason}");
        // The worker is left behind, not marked dead: what was handed over may
        // still land when the lock frees, and the thread that lands it is the
        // same one (the close is a wait, not a kill).
        assert!(
            writer.inner.alive.load(Ordering::SeqCst),
            "a wait that gave up does not end the worker"
        );
        drop(held);
        let _ = fs::remove_dir_all(&root);
    }

    /// The ordinary ending: closing the writer waits for what was handed over
    /// and says nothing — the same hand-over [`Drop`] makes, bounded (finding
    /// R4).
    #[test]
    fn a_closed_writer_lands_what_was_handed_over() {
        let root = root("close");
        let writer = Writer::new(root.to_path_buf(), None).expect("the worker starts");
        writer.save(saying("last words"));
        assert_eq!(writer.close(), None, "the worker ended, nothing to say");
        assert_eq!(last_message(&root), "last words");
        let _ = fs::remove_dir_all(&root);
    }

    /// A second signal ends the flush at its next poll (finding R3): the write
    /// may still be in flight, the caller is told in the same shape the
    /// deadline's sentence has, and the waiter is taken back out of the queue
    /// exactly as a timed-out one is.
    #[test]
    fn a_hurried_flush_gives_up_at_its_next_poll() {
        let root = root("flush-hurried");
        let writer = Writer::parked(root.to_path_buf(), None);
        // The same stuck worker the deadline test builds: alive, nothing parked
        // on the queue, no answer coming.
        writer.inner.alive.store(true, Ordering::SeqCst);
        writer.save(saying("lost"));

        let started = Instant::now();
        let hurried = writer
            .flush_within(Duration::from_secs(30), || true)
            .expect_err("a hurried wait is not an answer");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the hurry is not the deadline: {:?}",
            started.elapsed()
        );
        assert!(hurried.contains("second signal"), "{hurried}");
        assert!(
            writer.inner.pending.lock().unwrap().waiting.is_empty(),
            "the abandoned waiter is taken back out"
        );
        assert_eq!(writer.take_error().as_deref(), Some(hurried.as_str()));
        let _ = fs::remove_dir_all(&root);
    }

    /// And the writer's own thread: a second signal does not wait out
    /// [`JOIN_DEADLINE`] either.
    #[test]
    fn a_hurried_close_gives_up_at_its_next_poll() {
        let root = root("close-hurried");
        let writer = Writer::new(root.to_path_buf(), None).expect("the worker starts");
        // The worker blocked on the queue lock the test holds, exactly as in
        // the deadline test.
        let held = writer.inner.pending.lock().unwrap();
        let started = Instant::now();
        let reason = writer
            .close_within(Duration::from_secs(30), || true)
            .expect("a hurried close is not an answer");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the hurry is not the deadline: {:?}",
            started.elapsed()
        );
        assert!(reason.contains("second signal"), "{reason}");
        drop(held);
        let _ = fs::remove_dir_all(&root);
    }
}
