//! One mush per workspace.
//!
//! Two mush processes on one directory write the same `session.json`, and that
//! write is a whole-file replace — a whole conversation every time, on a
//! minute's debounce — so the two sessions overwrite each other in turn and
//! whichever saved last is the one that survived the crash. The socket says
//! *something* about a second process (`bind` fails against a live listener),
//! but a failed attach is deliberately not fatal, so the second process started
//! anyway and the damage was silent.
//!
//! `flock(2)` rather than a pid file: the kernel drops the lock when the
//! process dies, so there is no stale-lock case to reason about, nothing to
//! clean up after a `kill -9`, and no pid-reuse question to answer — the lock is
//! the open descriptions, not a number. The pid is *written* inside the file
//! only as a hint at the last holder to write it, for a refusal to offer a
//! human — the flock is the test, and the number can be an earlier life's.
//!
//! The file is deliberately never unlinked, not even on a clean exit. Unlinking
//! it is what makes a lock file racy: a third process that opens the path
//! between the unlink and the next `open` gets a *new* inode, locks that, and
//! two processes believe they hold the workspace. A file that is always there
//! cannot be raced *that way*, and it holds one pid at most, so it never grows.
//! The other road to a fresh inode is a whole-file *write* over the name —
//! `rename` needs no unlink window at all — and that is the one the next
//! paragraph is about.
//!
//! Keeping the *name* is therefore the other half, and it is not this module's
//! alone: the lock is a file inside the workspace, and the workspace's own roads
//! replace names — `write_file` is a temp file plus `rename` — which puts a
//! fresh inode at the path and leaves this process's flock on an orphaned one.
//! The model's write road refuses the store's own names
//! ([`mush_core::session::STORE_FILES`], where this file's name lives now), so
//! a tool call cannot do it; what no tool-level guard can stop is the human's
//! own `mv` over the path, and that is what [`Identity::still_mine`] is for —
//! the session's save road asks it before every write, so a mush whose lock was
//! taken stops writing instead of sharing the store (finding E2).
//!
//! A filesystem that cannot `flock` is refused, not ignored: a lock that
//! silently does nothing is the silent damage this module exists to prevent,
//! and the human is better told the workspace cannot be locked at all.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{flock, FlockOperation};

use mush_core::session::LOCK_FILE;

/// The workspace, for as long as this process is the one mush in it.
///
/// Dropping it releases the lock; so does dying, which is the point.
#[derive(Debug)]
pub struct Guard {
    /// Kept open for the life of the process: the lock is on this description,
    /// and closing the file would release it.
    _file: File,
    /// The name the lock was taken on and the inode that was locked: what
    /// [`Guard::identity`] answers with, for a road that has to ask later
    /// whether the name still leads to the locked file.
    path: PathBuf,
    inode: u64,
}

impl Guard {
    /// The identity of the lock this guard holds, for a road that has to ask
    /// later — the session's writer asks before every save
    /// ([`Identity::still_mine`]).
    pub fn identity(&self) -> Identity {
        Identity {
            path: self.path.clone(),
            inode: self.inode,
        }
    }
}

/// The lock as this process took it: the name it locked and the inode behind
/// it. The guard holds the open description; this holds the answer to "does the
/// name still lead to the file I locked".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    path: PathBuf,
    inode: u64,
}

impl Identity {
    /// Whether the name still leads to the inode that was locked, or why it
    /// does not.
    ///
    /// The lock file is never unlinked by mush (see the module docs), but the
    /// *name* is a name in the human's own directory: a whole-file write, or an
    /// `mv` of a backup over it, replaces it with a fresh inode — and the flock
    /// this process holds is then on an orphaned one, while a second mush locks
    /// the new file and owns the store. The model's write road refuses the
    /// store's own names ([`mush_core::session::STORE_FILES`]); what no
    /// tool-level guard can stop is the human's own hand, so the session's save
    /// road asks this before it writes.
    pub fn still_mine(&self) -> Result<(), String> {
        match std::fs::metadata(&self.path) {
            Ok(meta) if meta.ino() == self.inode => Ok(()),
            Ok(_) => Err(format!(
                "{} was replaced by another file — this mush's lock is on the orphaned one, so \
                 the session was not written (another mush may own this workspace)",
                self.path.display()
            )),
            Err(error) => Err(format!(
                "{} cannot be read ({error}) — this mush's lock is not the store's any more, so \
                 the session was not written",
                self.path.display()
            )),
        }
    }
}

/// Take the workspace lock, or say what can be said about the holder.
///
/// `root` is the workspace root; the lock lives beside the session it protects.
///
/// The pid goes in *after* the flock, deliberately. Writing it *before* — the
/// audit's suggested fix — would clobber the hint instead of improving it: a
/// refused acquirer has the lock file open too, so it would overwrite the
/// holder's pid with its own and the *next* refusal would name the process that
/// was refused, not the one holding the flock. The flock is the lock; the pid
/// is only ever a hint at the last real holder to write it.
pub fn acquire(root: &Path) -> Result<Guard, String> {
    let path = mush_core::session::mushroom_dir(root).join(LOCK_FILE);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    // Exclusive, and fail rather than wait: a second mush has nothing useful to
    // do while the first runs, and a startup that blocks is worse than one that
    // says why it will not.
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return Err(match holder(&mut file) {
                Some(pid) => format!(
                    "another mush is already running in this workspace — pid {pid} is the \
                     lock file's last known holder; the flock is the lock, so that number \
                     may be out of date — quit the running mush first, or ask it things \
                     with `mush agents`"
                ),
                None => "another mush is already running in this workspace".to_string(),
            });
        }
        Err(error) => return Err(format!("could not lock {}: {error}", path.display())),
    }
    // Ours: say so inside, for whoever is refused next. Truncated first, so a
    // shorter pid cannot leave the tail of a longer one behind.
    let inode = file
        .metadata()
        .map_err(|error| format!("could not read {}: {error}", path.display()))?
        .ino();
    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = writeln!(file, "{}", std::process::id());
    let _ = file.flush();
    Ok(Guard {
        _file: file,
        path,
        inode,
    })
}

/// The pid the last holder to write the file left, when it is readable. A hint
/// and never a fact: the file is deliberately never unlinked, so the number can
/// be an earlier life's, and the flock is what says a holder exists. Read-only,
/// best effort — the refusal is worth saying without a number too (a lock file
/// no holder has written yet).
fn holder(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::OnceLock;

    use mush_core::scratch::{Held, Scratch};

    use super::*;

    /// A pid that is really gone — a child of this test binary, waited for — for
    /// a test that needs a number no process owns. The child is forked once, by
    /// the first `root()` call, and that timing is the point: `fork` copies the
    /// process's open descriptions into the child, so a fork beside a live
    /// `Guard` leaves the child holding a copy of that flock until it execs —
    /// and then the sibling lock tests' own drop-and-reacquire assertions go red
    /// on a lock that is perfectly free (measured: 16–17 of 20 runs of this
    /// filter when the child was forked inside the dead-holder test; 12 of 12
    /// green with that test skipped).
    fn dead_pid() -> u32 {
        static DEAD: OnceLock<u32> = OnceLock::new();
        *DEAD.get_or_init(|| {
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap();
            pid
        })
    }

    fn root(label: &str) -> Held<PathBuf> {
        // Force the dead pid's fork before this test — or any sibling — can hold
        // a lock (see `dead_pid`).
        dead_pid();
        // The root carries the label and the process, in that order, so the
        // sweep can read the pid off the end of the name.
        let dir = Scratch::new(&format!("lock-{label}"));
        mush_core::session::ensure_mush_dir(&dir).unwrap();
        let path = dir.path().to_path_buf();
        dir.hold(path)
    }

    fn lock_path(root: &Path) -> PathBuf {
        mush_core::session::mushroom_dir(root).join(LOCK_FILE)
    }

    /// The second acquire is refused, told the lock file's last known holder,
    /// and the refusal goes away with the process that held it.
    #[test]
    fn a_second_acquire_is_refused_and_drop_releases_the_workspace() {
        let root = root("second");
        let first = acquire(&root).expect("the first acquire takes it");
        let error = acquire(&root).expect_err("the second is refused");
        assert!(
            error.contains(&std::process::id().to_string()),
            "the refusal names the holder: {error}"
        );
        assert!(error.contains("already running"), "{error}");
        drop(first);
        let third = acquire(&root);
        assert!(third.is_ok(), "the lock goes with the holder: {third:?}");
    }

    /// A pid left in the lock file by a holder that is gone is a hint at the
    /// last holder, not a statement about who holds the lock now: the flock is
    /// what refuses, and the refusal must not tell the human to quit a process
    /// that is already dead.
    #[test]
    fn a_refusal_does_not_name_a_dead_holder() {
        let root = root("dead-holder");
        let first = acquire(&root).expect("the first acquire takes it");
        let dead = dead_pid();
        std::fs::write(lock_path(&root), format!("{dead}\n")).unwrap();

        let error = acquire(&root).expect_err("the lock is still held");
        assert!(
            error.contains(&format!("pid {dead} is the lock file's last known holder")),
            "the refusal calls the dead pid the lock file's last known holder: {error}"
        );
        assert!(error.contains("already running"), "{error}");

        // The flock, not the text in the file, is what refused: with the holder
        // gone, the dead pid no longer stands between anyone and the workspace.
        drop(first);
        assert!(
            acquire(&root).is_ok(),
            "a dead pid in the file cannot refuse a workspace"
        );
    }

    /// The positive half, honestly named: an in-process holder wrote its own pid
    /// a moment ago, so the number is that holder's — and the refusal still
    /// calls it the lock file's *last known* holder, because that is all the
    /// file can know.
    #[test]
    fn the_pid_the_refusal_names_is_the_holder() {
        let root = root("live-holder");
        let _first = acquire(&root).expect("the first acquire takes it");
        let error = acquire(&root).expect_err("the second is refused");
        assert!(
            error.contains(&format!(
                "pid {} is the lock file's last known holder",
                std::process::id()
            )),
            "the refusal names this process as the lock file's last known holder: {error}"
        );
        assert!(error.contains("already running"), "{error}");
    }

    /// The lock file outlives its holder on purpose: unlinking it is what lets
    /// two processes end up locking two different inodes.
    #[test]
    fn the_lock_file_is_not_removed_when_the_holder_leaves() {
        let root = root("kept");
        let path = lock_path(&root);
        let guard = acquire(&root).unwrap();
        drop(guard);
        assert!(path.exists(), "{} must stay on disk", path.display());
        // And a lock that was released this way is takeable again.
        assert!(acquire(&root).is_ok());
    }

    /// The lock's *name* is as load-bearing as its shape: `write_file` is a
    /// temp file plus `rename`, so it puts a fresh inode at `.mush/lock` — the
    /// flock this process holds is then on an orphaned inode, the next process
    /// locks the new file, and two mushes write whole-file sessions over each
    /// other on one store. The model's write road refuses the store's own
    /// names, so the road is closed where the tool call lands; the positive
    /// twin keeps the rest of `.mush/` the human's (finding E2).
    #[test]
    fn a_write_cannot_replace_the_workspace_lock() {
        let root = root("store");
        let first = acquire(&root).expect("the first acquire takes it");
        let ws = mush_core::workspace::Workspace::new(&root).unwrap();

        let error = ws
            .write_file(".mush/lock", "a file, not a lock\n")
            .expect_err("the lock's own name is refused");
        assert!(
            error.contains(".mush/lock"),
            "the refusal names the file: {error}"
        );
        assert!(
            error.contains("second mush"),
            "the refusal says what a replace would cost: {error}"
        );

        // The lock the first acquire holds is still the one at the path, and
        // the flock is still the test: a second process is refused for the same
        // reason it was before the write was attempted.
        let error = acquire(&root).expect_err("the second acquire is still refused");
        assert!(error.contains("already running"), "{error}");

        // The positive twin: a name that is not the store's still writes, so the
        // refusal is the store's own files and not the directory.
        ws.write_file(".mush/note.txt", "a note\n")
            .expect("an ordinary file under .mush/ writes");
        assert_eq!(
            std::fs::read_to_string(root.join(".mush/note.txt")).unwrap(),
            "a note\n"
        );

        drop(first);
        assert!(acquire(&root).is_ok(), "the lock goes with its holder");
    }

    /// Two workspaces do not see each other's lock: the store is the unit.
    #[test]
    fn separate_workspaces_have_separate_locks() {
        let one = root("one");
        let two = root("two");
        let _first = acquire(&one).unwrap();
        assert!(acquire(&two).is_ok(), "another store is another workspace");
    }
}
