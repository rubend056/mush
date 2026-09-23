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
//! latest state, and an older snapshot can never land after a newer one.
//! [`SessionSave::flush`] is the other half: the transitions that mean "this
//! must be on disk" wait for their own write, a cost paid once per human
//! action instead of once per response.

use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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

/// Where a snapshot of the session is written.
///
/// The snapshot is the caller's; the serialization and the disk are the
/// implementation's, on the implementation's own thread. Nothing here is
/// allowed to make the UI thread wait for a file unless the caller asked to
/// wait (see [`SessionSave::flush`]).
pub trait SessionSave: Send + Sync {
    /// Hand a snapshot over, without waiting for it. Cheap enough for the
    /// message path: the newest snapshot wins, so a burst of messages costs one
    /// write that carries the latest state.
    fn save(&self, session: Session);

    /// Wait until everything handed over so far is on disk. For the call sites
    /// that mean "this must not be lost" — the human's own message, a command
    /// that changed what is stored, quitting — rather than for a streamed
    /// response, which is covered by the debounce and the exit flush.
    fn flush(&self);

    /// The last write's failure, once. A write that failed on the writer's
    /// thread has no caller to return to, and swallowing it would let a
    /// workspace mush cannot write to look saved. `None` when nothing failed.
    fn take_error(&self) -> Option<String>;
}

/// The real seam: one writer thread, and the newest snapshot.
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
    wake: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
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
            wake: None,
            worker: None,
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
                self.worker = Some(worker);
                self.wake = Some(wake);
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
        if let Some(wake) = &self.wake {
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
        let _ = self.flush_within(FLUSH_DEADLINE);
    }

    fn take_error(&self) -> Option<String> {
        self.inner.failed.lock().unwrap().take()
    }
}

impl Writer {
    /// [`SessionSave::flush`], with the deadline a parameter so a test can hold
    /// the clock instead of the clock holding the test.
    ///
    /// Three things end the wait, and none of them is "as long as it takes":
    /// the worker answering (the file is then current), the worker being gone
    /// (a waiter nothing can wake is never parked), or the deadline (the write
    /// is still in flight; the waiter is taken back out of the queue and the
    /// failure is left in [`SessionSave::take_error`]). The deadline is not
    /// sticky — it does not mark the worker dead — so the next flush parks a
    /// fresh waiter and asks again, which is what makes a write that lands
    /// late still land.
    fn flush_within(&self, deadline: Duration) -> Result<(), String> {
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
        // The worker sends on the waiter after the write in front of it lands,
        // so a `recv` that returns means the file is current; a `recv` that
        // disconnects means the worker died without answering.
        match waited.recv_timeout(deadline) {
            Ok(()) => Ok(()),
            Err(RecvTimeoutError::Timeout) => {
                // The worker may still be inside the write; the answer it owes
                // is owed to whoever waits then, not to this caller, and a
                // waiter left in the queue would grow the queue once per
                // timed-out flush. Take ours back out. The worker holds the
                // queue only long enough to empty it, so this cannot become a
                // second unbounded wait.
                self.inner
                    .pending
                    .lock()
                    .unwrap()
                    .waiting
                    .retain(|waiter| !waiter.same_channel(&done));
                if !self.inner.alive.load(Ordering::SeqCst) {
                    return Err(self
                        .inner
                        .fail("the session writer is gone — the session was not saved"));
                }
                Err(self.inner.fail(format!(
                    "the session writer did not answer within {deadline:?} — the write is still in flight, and the next flush will ask again"
                )))
            }
            Err(RecvTimeoutError::Disconnected) => Err(self.inner.fail(
                "the session writer died before the write landed — the session was not saved",
            )),
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Closing the wake channel ends the worker once it has written whatever
        // was pending. Joining it keeps a write from outliving the app that
        // asked for it — the exit flush's snapshot is on disk before this
        // returns, which is what makes the last thing said survivable.
        self.wake.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Write snapshots until nothing is waiting, then sleep until poked again.
fn writer(inner: Arc<Inner>, woken: Receiver<()>) {
    loop {
        // The sender going away is the writer being dropped; the drain below
        // still runs, so a hand-over that raced the drop is not lost.
        let woke = woken.recv().is_ok();
        drain(&inner);
        if !woke {
            return;
        }
    }
}

/// Write everything handed over so far, then wake whoever waited for it.
///
/// One thread, so one write at a time: at most one snapshot is in flight, and
/// the only one that can follow it is the newest state.
fn drain(inner: &Inner) {
    loop {
        let (snapshot, waiting) = inner.pending.lock().unwrap().take();
        if snapshot.is_none() && waiting.is_empty() {
            return;
        }
        if let Some(session) = snapshot {
            // The store is one writer's only while the lock's name still leads
            // to the inode this process locked. The model's write road refuses
            // the store's own names, but the name is on the human's own disk:
            // a whole-file write or an `mv` over it replaces it in one step,
            // and from then on a second mush owns the store. Writing the
            // conversation into somebody else's store is exactly the damage the
            // lock exists to prevent, so a save that cannot prove the store is
            // still this mush's writes nothing and leaves the refusal where the
            // UI's tick reads it.
            if let Some(Err(why)) = inner.lock.as_ref().map(lock::Identity::still_mine) {
                *inner.failed.lock().unwrap() = Some(why);
            } else {
                let attempt = session.save(&inner.root);
                #[cfg(test)]
                inner.writes.fetch_add(1, Ordering::SeqCst);
                // Nobody is waiting on a background write, so the failure is left
                // where the UI's next tick will find it. A later success does not
                // clear it: the tick polls every frame, and a failure that arrives
                // and is overwritten unseen is a failure that was swallowed.
                if let Err(error) = attempt {
                    *inner.failed.lock().unwrap() = Some(error.to_string());
                }
            }
        }
        // After the write, never before it: a waiter is asking for a file it can
        // read, not for a promise.
        for waiter in waiting {
            let _ = waiter.send(());
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
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use mush_core::message::Message;
    use mush_core::session::{session_path, Session};

    use super::{SessionSave, Writer, FLUSH_DEADLINE};

    /// A directory to write a session into, and to leave behind nothing.
    fn root(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mush-writer-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
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
        let writer = Writer::new(root.clone(), None).expect("the worker starts");
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
        let mut writer = Writer::parked(root.clone(), None);
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
        let writer = Writer::new(root.clone(), None).expect("the worker starts");
        writer.flush();
        assert!(!session_path(&root).exists(), "nothing was handed over");
        writer.save(saying("after"));
        writer.flush();
        assert_eq!(last_message(&root), "after");
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

        let never = Arc::new(Writer::parked(root.clone(), None));
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

        let died = Arc::new(Writer::new(root.clone(), None).expect("the worker starts"));
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
        let writer = Writer::new(root.clone(), None).expect("the worker starts");
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
        let writer = Writer::parked(root.clone(), None);
        // The shape of a worker stuck inside `Session::save`: the thread is
        // there (the flag says so), the answer is not. Nothing is parked on the
        // queue, so the wait can only end at the deadline — which the test
        // holds, because `flush_within` takes it.
        writer.inner.alive.store(true, Ordering::SeqCst);
        writer.save(saying("lost"));

        let started = Instant::now();
        let first = writer
            .flush_within(Duration::from_millis(50))
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

        let _ = writer.flush_within(Duration::from_millis(50));
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
        let writer = Writer::new(root.clone(), None).expect("the worker starts");
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
        let writer = Writer::new(root.clone(), Some(guard.identity())).expect("the worker starts");

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
}
