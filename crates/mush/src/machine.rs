//! The shell seam: starting a command, and watching one.
//!
//! `run_command` asks a running command exactly three questions — has it
//! ended, how much has it written, what did it write — and performs exactly one
//! action on it: stop it, and everything it started. [`Machine`] + [`Job`] are
//! those four things and nothing else.
//!
//! The real impl is the shell as it always was: `sh -c` in the workspace, its
//! own process group, output to scratch *files* rather than pipes (a pipe is
//! only complete once every holder exits, so a command that leaves a background
//! job behind would pin the agent thread past its timeout), and
//! `kill -9 -pgid` to take the whole group down.
//!
//! The fake scripts end states, output sizes and kills, so the timeout, the
//! cancellation and the output cap — the three ways a command *stops* — are
//! asserted in process: no `sh`, no `sleep`, no `yes`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};

use tempfile::NamedTempFile;

use mush_core::workspace::{tail_for_model, truncate_for_model};

/// What to run, and where.
///
/// The timeout and the output limit are deliberately *not* here: they decide
/// when the watcher stops waiting, not how the command is started, and the
/// watcher is the only thing that can reach the clock.
pub struct ShellCommand<'a> {
    pub command: &'a str,
    pub root: &'a Path,
}

/// How a command ended, as the machine can tell it.
///
/// A process a signal killed has no exit code at all — the shell spells such a
/// death `128 + n` — and mush spelled it `-1`, a number no command returns and
/// one that says nothing about what ended it: an OOM kill (`9`) and the
/// command's own `SIGSEGV` (`11`) read identically (finding B6). Telling the
/// two apart is the machine's to do; what to *make* of either death is the
/// surfaces' business, not this module's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// It ended by itself, with this exit code.
    Exited(i32),
    /// A signal killed it, and this is the signal's number.
    Signalled(i32),
    /// The status names neither an exit code nor a signal. A state of its own
    /// rather than a sentinel code: `-1` was the old spelling and it reads as a
    /// real exit code, one a reader could act on (finding H26). A stopped wait
    /// status with no stop signal is one such status — `code` and `signal` both
    /// answer `None` for it — and what this buys is that the machine says so
    /// instead of naming a number nothing returned.
    Unknown,
}

/// One command that has started, as the watcher sees it.
pub trait Job: Send {
    /// How it ended, if it has: its own exit code, or the signal that killed
    /// it (see [`End`]). `None` while it runs.
    fn poll(&mut self) -> Result<Option<End>, String>;

    /// Bytes written to stdout and stderr together — the number the output
    /// limit is measured against. Read from the files, so a background job
    /// that inherited them is accounted for too.
    fn written(&self) -> u64;

    /// The first `cap` bytes of each stream, marked when truncated, exactly as
    /// the model would read them.
    fn output(&self, cap: usize) -> (String, String);

    /// The *last* `cap` bytes of each stream, marked when truncated. This is
    /// the window a detached job keeps: a job ends, and what it ended with is
    /// the part worth reading (see `crate::jobs`). A running job is read from
    /// here too, so what `status` shows and what the completion reports
    /// are the same bytes.
    fn tail(&self, cap: usize) -> (String, String);

    /// Stop it and everything it started. Idempotent.
    fn kill(&mut self);
}

/// How a command is started. One method: there is nothing else the watcher
/// needs to know about a machine to run a command on it.
pub trait Machine: Send + Sync {
    fn spawn(&self, cmd: &ShellCommand) -> Result<Box<dyn Job>, String>;
}

/// The real one: `sh -c`, its own process group, output to scratch files.
pub struct Shell;

impl Machine for Shell {
    fn spawn(&self, cmd: &ShellCommand) -> Result<Box<dyn Job>, String> {
        let out = Scratch::new("out")?;
        let err = Scratch::new("err")?;
        let mut shell = Command::new("sh");
        shell
            .arg("-c")
            .arg(cmd.command)
            .current_dir(cmd.root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out.writer()?))
            .stderr(Stdio::from(err.writer()?));
        // Its own process group, so a signal aimed at mush never lands on a
        // build and cleanup can target everything the command started.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            shell.process_group(0);
        }
        let child = shell
            .spawn()
            .map_err(|e| format!("could not run command: {e}"))?;
        Ok(Box::new(Running { child, out, err }))
    }
}

/// A command the real [`Shell`] is running.
struct Running {
    child: Child,
    out: Scratch,
    err: Scratch,
}

/// How a finished child ended.
///
/// `ExitStatus::code()` is `None` exactly when a signal ended the process, and
/// on unix the signal is there to be named instead. The `#[cfg]` is the one the
/// rest of this module is written around (the process group, `kill -9 -pgid`):
/// a death by signal is a unix death, and a platform without one has only the
/// code its own status carries.
fn ended(status: ExitStatus) -> End {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return End::Signalled(signal);
        }
    }
    // Neither an exit code nor a signal — the `-1` that used to stand here read
    // as a code a command could have returned. Every child `mush` starts ends
    // one of two ways (an exit, or a death by signal), but this seam takes any
    // status a platform can produce, and a stopped status with no stop signal is
    // one that names neither: the honest answer is a state that says unknown.
    match status.code() {
        Some(code) => End::Exited(code),
        None => End::Unknown,
    }
}

impl Job for Running {
    fn poll(&mut self) -> Result<Option<End>, String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Ok(Some(ended(status))),
            Ok(None) => Ok(None),
            Err(error) => Err(format!("could not wait for command: {error}")),
        }
    }

    fn written(&self) -> u64 {
        self.out.size() + self.err.size()
    }

    fn output(&self, cap: usize) -> (String, String) {
        (self.out.read(cap), self.err.read(cap))
    }

    fn tail(&self, cap: usize) -> (String, String) {
        (self.out.read_tail(cap), self.err.read_tail(cap))
    }

    fn kill(&mut self) {
        let group = self.child.id();
        let _ = self.child.kill();
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-9", &format!("-{group}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = self.child.wait();
    }
}

/// A command's output file, removed when it is dropped.
///
/// `NamedTempFile` picks the name and creates it exclusively, so a guessable
/// name in a shared temp directory can never redirect or read what a command
/// prints — the property the hand-rolled counter and `0600` tried to buy. It is
/// named (rather than an O_TMPFILE handle) because the child has to inherit a
/// path it can write to.
struct Scratch {
    file: NamedTempFile,
}

impl Scratch {
    fn new(kind: &str) -> Result<Self, String> {
        NamedTempFile::with_prefix(format!("mush-cmd-{kind}-"))
            .map(|file| Self { file })
            .map_err(|error| format!("cannot create a scratch file: {error}"))
    }

    /// An independent write handle for the child to inherit.
    fn writer(&self) -> Result<File, String> {
        self.file
            .reopen()
            .map_err(|error| format!("cannot open the scratch file: {error}"))
    }

    /// What was written, capped for the model. Reads one byte past the cap so
    /// a truncated result is marked as such.
    fn read(&self, cap: usize) -> String {
        let mut bytes = Vec::new();
        if let Ok(file) = self.file.reopen() {
            let _ = file.take(cap as u64 + 1).read_to_end(&mut bytes);
        }
        truncate_for_model(String::from_utf8_lossy(&bytes).into_owned(), cap)
    }

    /// The end of what was written. Seeking from the end (rather than reading
    /// the whole file and slicing it) is what keeps this bounded for a command
    /// that has printed megabytes: the job only ever holds a window.
    fn read_tail(&self, cap: usize) -> String {
        let size = self.size();
        let mut bytes = Vec::new();
        if let Ok(file) = self.file.reopen() {
            let skip = size.saturating_sub(cap as u64);
            let mut reader = &file;
            if skip > 0 && reader.seek(SeekFrom::Start(skip)).is_err() {
                return String::new();
            }
            let _ = reader.read_to_end(&mut bytes);
        }
        tail_for_model(&String::from_utf8_lossy(&bytes), cap)
    }

    fn size(&self) -> u64 {
        self.file
            .as_file()
            .metadata()
            .map(|meta| meta.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use mush_core::workspace::{tail_for_model, truncate_for_model};

    use super::{End, Job, Machine, ShellCommand};

    /// What a scripted command does while it "runs".
    ///
    /// How long it runs for is deliberately *not* here: elapsed time is the
    /// clock's business (see `crate::clock::fake`), and a script only says how
    /// many polls pass before it ends. `grows` is how many bytes each poll adds
    /// to the written total — that is how the output cap is reached without a
    /// `yes` and without filling a disk.
    #[derive(Default)]
    pub struct Script {
        pub stdout: String,
        pub stderr: String,
        pub grows: u64,
        /// Polls that pass before it exits; `None` never exits — a command that
        /// has to be timed out or killed.
        pub exits_after: Option<usize>,
        pub code: i32,
        /// The signal that killed it instead of an exit code. The two are one
        /// or the other, as they are in a real status (finding B6).
        pub signal: Option<i32>,
    }

    impl Script {
        /// A command that ends on its own, with this exit code.
        pub fn exits(code: i32) -> Self {
            Self {
                code,
                exits_after: Some(0),
                ..Self::default()
            }
        }

        /// A command a signal kills with nobody in mush asking — a `SIGSEGV` of
        /// its own, an OOM killer's `SIGKILL`. It is the death an exit code
        /// cannot spell, so the script states the signal rather than a code.
        pub fn signalled(signal: i32) -> Self {
            Self {
                signal: Some(signal),
                exits_after: Some(0),
                ..Self::default()
            }
        }

        /// A command that never ends: the shape a timeout and a cancellation
        /// are about.
        pub fn hangs() -> Self {
            Self::default()
        }

        /// What it prints.
        pub fn says(mut self, stdout: &str) -> Self {
            self.stdout = stdout.to_string();
            self
        }

        /// What it complains about on stderr.
        pub fn complains(mut self, stderr: &str) -> Self {
            self.stderr = stderr.to_string();
            self
        }

        /// A writer that never stops: `grows` bytes a poll, forever.
        pub fn writes_without_end(mut self, grows: u64) -> Self {
            self.grows = grows;
            self.exits_after = None;
            self
        }
    }

    /// A machine that runs whatever the test wrote down, in order.
    ///
    /// It records every command it was asked to run and every job it was asked
    /// to kill, so "the runaway writer was stopped" is an assertion about what
    /// happened rather than about how long something took.
    #[derive(Default)]
    pub struct Scripted {
        scripts: Mutex<VecDeque<Script>>,
        spawned: Mutex<Vec<String>>,
        kills: Arc<AtomicUsize>,
    }

    impl Scripted {
        pub fn new() -> Self {
            Self::default()
        }

        /// The next command run behaves like this.
        pub fn runs(self, script: Script) -> Self {
            self.scripts.lock().unwrap().push_back(script);
            self
        }

        /// The commands that were started, in order.
        pub fn spawned(&self) -> Vec<String> {
            self.spawned.lock().unwrap().clone()
        }

        /// How many jobs were killed.
        pub fn kills(&self) -> usize {
            self.kills.load(Ordering::SeqCst)
        }
    }

    impl Machine for Scripted {
        fn spawn(&self, cmd: &ShellCommand) -> Result<Box<dyn Job>, String> {
            self.spawned.lock().unwrap().push(cmd.command.to_string());
            let script = self.scripts.lock().unwrap().pop_front().ok_or_else(|| {
                format!(
                    "no scripted job for {:?}: the test did not say what it does",
                    cmd.command
                )
            })?;
            Ok(Box::new(ScriptedJob {
                script,
                polls: 0,
                written: 0,
                killed: false,
                kills: self.kills.clone(),
            }))
        }
    }

    struct ScriptedJob {
        script: Script,
        polls: usize,
        written: u64,
        killed: bool,
        kills: Arc<AtomicUsize>,
    }

    impl Job for ScriptedJob {
        fn poll(&mut self) -> Result<Option<End>, String> {
            self.written += self.script.grows;
            let polls = self.polls;
            self.polls += 1;
            // A killed command is a *dead* command, and the real shell says so:
            // the child is reaped with no exit code at all, which is a death by
            // signal — `kill` sends `SIGKILL`, and `Running::kill` follows with
            // `kill -9 -pgid` for the group. A fake that kept a killed command
            // "running" forever could not tell a watcher that noticed the kill
            // from one that slept through it — which is finding S4's whole
            // question — so the death is scripted here too, signal and all.
            if self.killed {
                return Ok(Some(End::Signalled(9)));
            }
            // `exits_after` counts the polls that pass *before* it ends; a kill
            // lands before the poll that would have ended it.
            match self.script.exits_after {
                Some(after) if polls >= after => Ok(Some(match self.script.signal {
                    Some(signal) => End::Signalled(signal),
                    None => End::Exited(self.script.code),
                })),
                _ => Ok(None),
            }
        }

        fn written(&self) -> u64 {
            self.written
        }

        fn output(&self, cap: usize) -> (String, String) {
            (
                truncate_for_model(self.script.stdout.clone(), cap),
                truncate_for_model(self.script.stderr.clone(), cap),
            )
        }

        fn tail(&self, cap: usize) -> (String, String) {
            (
                tail_for_model(&self.script.stdout, cap),
                tail_for_model(&self.script.stderr, cap),
            )
        }

        fn kill(&mut self) {
            if !self.killed {
                self.killed = true;
                self.kills.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ended, End};

    /// The one reading of a real status: an exit code, or the signal that killed
    /// the process. `ExitStatusExt::from_raw` is the inverse of the `into_raw` a
    /// wait(2) status carries, so both shapes are built here from the numbers a
    /// shell would report — no `sh`, no `kill` — and the seam every surface
    /// reads through is pinned rather than left to a subprocess (finding B6).
    #[cfg(unix)]
    #[test]
    fn a_finished_child_reads_as_its_exit_code_or_its_signal() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;

        // A wait status is the code in the high byte, or the signal in the low
        // seven bits.
        assert_eq!(ended(ExitStatus::from_raw(3 << 8)), End::Exited(3));
        assert_eq!(ended(ExitStatus::from_raw(0)), End::Exited(0));
        assert_eq!(ended(ExitStatus::from_raw(9)), End::Signalled(9));
        assert_eq!(ended(ExitStatus::from_raw(11)), End::Signalled(11));
    }

    #[cfg(unix)]
    #[test]
    fn a_status_that_names_no_end_is_not_read_as_a_code() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;

        // A stopped wait status with no stop signal (`WIFSTOPPED` with
        // `WSTOPSIG` zero) is the one status that is neither: `code()` and
        // `signal()` both answer `None` for it. The old last resort spelled it
        // `Exited(-1)` — a code no command returns and one a reader could act on
        // — and its own state is the whole point (finding H26).
        let nameless = ExitStatus::from_raw(0x7f);
        assert_eq!(nameless.code(), None);
        assert_eq!(nameless.signal(), None);
        assert_eq!(ended(nameless), End::Unknown);
        assert!(
            !matches!(ended(nameless), End::Exited(_)),
            "an unknown end is not an exit code spelled -1"
        );
    }
}
