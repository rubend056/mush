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
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use tempfile::NamedTempFile;

use mush_core::workspace::truncate_for_model;

/// What to run, and where.
///
/// The timeout and the output limit are deliberately *not* here: they decide
/// when the watcher stops waiting, not how the command is started, and the
/// watcher is the only thing that can reach the clock.
pub struct ShellCommand<'a> {
    pub command: &'a str,
    pub root: &'a Path,
}

/// One command that has started, as the watcher sees it.
pub trait Job: Send {
    /// How it ended, if it has: the exit code, or `-1` when a signal ended it
    /// (the same spelling the report has always used). `None` while it runs.
    fn poll(&mut self) -> Result<Option<i32>, String>;

    /// Bytes written to stdout and stderr together — the number the output
    /// limit is measured against. Read from the files, so a background job
    /// that inherited them is accounted for too.
    fn written(&self) -> u64;

    /// The first `cap` bytes of each stream, marked when truncated, exactly as
    /// the model would read them.
    fn output(&self, cap: usize) -> (String, String);

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

impl Job for Running {
    fn poll(&mut self) -> Result<Option<i32>, String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Ok(Some(status.code().unwrap_or(-1))),
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

    fn size(&self) -> u64 {
        self.file
            .as_file()
            .metadata()
            .map(|meta| meta.len())
            .unwrap_or(0)
    }
}
