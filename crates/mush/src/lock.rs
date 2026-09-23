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
//! clean up after a `kill -9`, and no pid-reuse question to answer. The pid is
//! *written* inside the file so the refusal can name who to quit — it is never
//! the thing being tested.
//!
//! The file is deliberately never unlinked, not even on a clean exit. Unlinking
//! it is what makes a lock file racy: a third process that opens the path
//! between the unlink and the next `open` gets a *new* inode, locks that, and
//! two processes believe they hold the workspace. A file that is always there
//! cannot be raced, and it holds one pid at most, so it never grows.
//!
//! Keeping the *name* is the other half, and it is not this module's alone:
//! the lock is a file inside the workspace, and the workspace's own roads
//! replace names — `write_file` is a temp file plus `rename` — which puts a
//! fresh inode at the path and leaves this process's flock on an orphaned one.
//! The model's write road refuses the store's own names
//! ([`mush_core::session::STORE_FILES`], where this file's name lives now), so
//! a tool call cannot do it (finding E2).
//!
//! A filesystem that cannot `flock` is refused, not ignored: a lock that
//! silently does nothing is the silent damage this module exists to prevent,
//! and the human is better told the workspace cannot be locked at all.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

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
}

/// Take the workspace lock, or say who holds it.
///
/// `root` is the workspace root; the lock lives beside the session it protects.
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
                    "another mush is already running in this workspace (pid {pid}) — quit it \
                     first, or ask it things with `mush agents`"
                ),
                None => "another mush is already running in this workspace".to_string(),
            });
        }
        Err(error) => return Err(format!("could not lock {}: {error}", path.display())),
    }
    // Ours: say so inside, for whoever is refused next. Truncated first, so a
    // shorter pid cannot leave the tail of a longer one behind.
    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = writeln!(file, "{}", std::process::id());
    let _ = file.flush();
    Ok(Guard { _file: file })
}

/// The pid the holder wrote, when it is readable. Read-only, best effort: the
/// refusal is worth saying even when the file has nothing in it yet (a holder
/// that died between `open` and `write` leaves it empty).
fn holder(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mush-lock-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        mush_core::session::ensure_mush_dir(&dir).unwrap();
        dir
    }

    fn lock_path(root: &Path) -> PathBuf {
        mush_core::session::mushroom_dir(root).join(LOCK_FILE)
    }

    /// The second process is refused, told whose workspace it is, and the
    /// refusal goes away with the process that held it.
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
