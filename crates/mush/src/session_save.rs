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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crossbeam_channel::{bounded, Receiver, Sender};

use mush_core::session::Session;

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
    pending: Mutex<Pending>,
    /// How many writes have been attempted. Only a test reads it — a burst's
    /// cost is otherwise invisible from outside the writer.
    #[cfg(test)]
    writes: AtomicUsize,
    failed: Mutex<Option<String>>,
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

impl Writer {
    pub fn new(root: PathBuf) -> Self {
        let mut writer = Self::parked(root);
        writer.run();
        writer
    }

    /// Everything but the thread, so a test can hand snapshots over before
    /// anything can write them — which is how what a burst costs is seen
    /// instead of raced. `new` is this and then [`Self::run`].
    fn parked(root: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                root,
                pending: Mutex::new(Pending::default()),
                #[cfg(test)]
                writes: AtomicUsize::new(0),
                failed: Mutex::new(None),
            }),
            wake: None,
            worker: None,
        }
    }

    /// Start the writer's thread.
    fn run(&mut self) {
        let (wake, woken) = bounded::<()>(1);
        let inner = self.inner.clone();
        self.worker = Some(std::thread::spawn(move || writer(inner, woken)));
        self.wake = Some(wake);
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
        let (done, waited) = bounded(1);
        self.inner.pending.lock().unwrap().waiting.push(done);
        self.poke();
        // The worker drops the sender when it takes the waiter, which happens
        // after the write in front of it lands — so this returns with the file
        // current. Blocking here is the point of the call; blocking *forever*
        // would need the worker to die holding the waiter, and the one thing it
        // calls that could panic out (`Session::save`) returns its errors
        // instead.
        let _ = waited.recv();
    }

    fn take_error(&self) -> Option<String> {
        self.inner.failed.lock().unwrap().take()
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

    use mush_core::message::Message;
    use mush_core::session::{session_path, Session};

    use super::{SessionSave, Writer};

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

    /// The write lands where `Session::load` looks, in the format it reads: a
    /// session written on the writer's thread is the same file the UI thread
    /// used to write.
    #[test]
    fn a_handed_over_snapshot_is_on_disk_when_flush_returns() {
        let root = root("roundtrip");
        let writer = Writer::new(root.clone());
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
        let mut writer = Writer::parked(root.clone());
        writer.save(saying("one"));
        writer.save(saying("two"));
        writer.save(saying("three"));
        writer.run();
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
        let writer = Writer::new(root.clone());
        writer.flush();
        assert!(!session_path(&root).exists(), "nothing was handed over");
        writer.save(saying("after"));
        writer.flush();
        assert_eq!(last_message(&root), "after");
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
        let writer = Writer::new(root.clone());
        writer.save(saying("lost"));
        writer.flush();
        let error = writer.take_error().expect("the failure is reported");
        assert!(!error.is_empty(), "it says what went wrong: {error}");
        assert!(writer.take_error().is_none(), "and it is reported once");
        let _ = fs::remove_dir_all(&root);
    }
}
