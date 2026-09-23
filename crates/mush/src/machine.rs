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
//! job behind would pin the agent thread past its timeout), and a `SIGKILL` to
//! the whole process group — sent in process ([`kill_group`]), so no `kill`
//! program on `PATH` can stand between mush and a group it must end — when the
//! command's end is mush's doing ([`Job::kill`]) and when the command ended by
//! itself with the group still standing ([`Job::end_group`]). One thing is not
//! as it always was: the child is handed the inherited environment **minus
//! mush's secrets**, so a command cannot read the provider credential
//! ([`Shell`]).
//!
//! The fake scripts end states, output sizes and kills, so the timeout, the
//! cancellation and the output cap — the three ways a command *stops* — are
//! asserted in process: no `sh`, no `sleep`, no `yes`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};

use tempfile::{Builder, NamedTempFile};

use mush_core::secrets::scrub;
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

    /// Stop it and everything it started. Idempotent for real: the second call
    /// is a no-op, because the first reaped the leader and the id a group call
    /// would aim at is then free to be handed to somebody else's process group
    /// (finding E6).
    ///
    /// A group the kill could not end is not swallowed: [`Job::kill_failure`]
    /// owes the one sentence about it, and the job's own window is where the
    /// watcher says it.
    fn kill(&mut self);

    /// The one thing the last [`Job::kill`] could not finish, if anything: the
    /// command's process group could not be signalled, so the kill took the
    /// leader and left the rest of the group behind. A question of its own
    /// rather than `kill`'s return value because the failure is read in one
    /// other place — the watcher that builds the job's completion — and the
    /// second kill is a no-op with nothing to report (finding E6).
    ///
    /// `None` for the ordinary kill, and for a job whose kill cannot fail.
    fn kill_failure(&self) -> Option<String> {
        None
    }

    /// End what a command that has already ended left behind: the processes
    /// still in its process group, and how many of them that took.
    ///
    /// A command's end is its leader's end ([`Job::poll`]), and a command that
    /// ended by itself was never signalled — so `cmd &` (the shell exits, its
    /// child stays in the group mush gave it) and a script that double-forks
    /// would leave a process running in a group mush made, in no registry and
    /// on no clock: not `stop`, not `kill_all`, not the age ceiling, and not
    /// the output cap, whose watcher has gone (finding E3). The group is mush's
    /// by construction (`process_group(0)`), so the watcher takes it here,
    /// before it reports the command's own end.
    ///
    /// `Ok(0)` is the ordinary answer and means there was nothing left to end.
    /// The leader must already be reaped: while it runs, the group is led by a
    /// live process and [`Job::kill`] is the road that takes it — which is also
    /// the whole answer for a command mush *stopped* rather than one that
    /// finished, so a stop road calls nothing here and reports nothing about
    /// the group. An implementation that cannot prove the group is still this
    /// command's must answer `Ok(0)` rather than signal an id the kernel may
    /// have reissued — a pid is not reissued while a process group still holds
    /// it, so finding a member is the proof.
    fn end_group(&mut self) -> Result<usize, String>;
}

/// How a command is started. One method: there is nothing else the watcher
/// needs to know about a machine to run a command on it.
pub trait Machine: Send + Sync {
    fn spawn(&self, cmd: &ShellCommand) -> Result<Box<dyn Job>, String>;
}

/// The real one: `sh -c`, its own process group, output to scratch files — and
/// **without mush's secrets**.
///
/// A command sees the environment the human's own shell would have handed it —
/// `PATH`, `HOME`, `LANG`, `EDITOR`, their tooling — minus
/// [`mush_core::secrets::SECRET_ENV`]: `MUSH_API_KEY` is not in it. The
/// credential is mush's, its one road is the wire, and this child's output is a
/// tool result — the store keeps that verbatim and the attach socket hands it
/// to any local user, so a command that could read the key could put it
/// somewhere durable in one turn (finding C1). The removal is [`scrub`]'s —
/// one list, and every child mush starts for itself is taken through it.
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
        // The command is the model's, the credential is mush's: `Shell`'s doc
        // says what is taken out and why.
        scrub(&mut shell);
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
        Ok(Box::new(Running {
            child,
            out,
            err,
            killed: false,
            failure: None,
        }))
    }
}

/// A command the real [`Shell`] is running.
struct Running {
    child: Child,
    out: Scratch,
    err: Scratch,
    /// Whether [`Job::kill`] has already run. The first kill reaps the leader,
    /// which frees the group id; a second signal aimed at it would land on
    /// whatever group the kernel handed the number to next (finding E6).
    killed: bool,
    /// The one sentence a kill that could not end the group owes, read through
    /// [`Job::kill_failure`] (finding E6).
    failure: Option<String>,
}

/// How a finished child ended.
///
/// `ExitStatus::code()` is `None` exactly when a signal ended the process, and
/// on unix the signal is there to be named instead. The `#[cfg]` is the one the
/// rest of this module is written around (the process group, the in-process
/// `SIGKILL` that ends it): a death by signal is a unix death, and a platform
/// without one has only the code its own status carries.
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
        // A second kill is a no-op (finding E6): the first reaped the leader,
        // so the id a group signal would aim at is free to be reused, and a
        // `SIGKILL` at a reused id kills work mush never started. The flag is
        // the fake's own idempotence made real.
        if self.killed {
            return;
        }
        self.killed = true;
        let _ = self.child.kill();
        #[cfg(unix)]
        {
            if let Err(error) = kill_group(self.child.id()) {
                // `ESRCH` is "the group is already gone": the leader's own
                // death took its last member with it, and there is nothing left
                // to end. Anything else is the failure the old `kill`
                // subprocess's discarded `let _` swallowed — a signal that did
                // not land must not read as one that did (finding E6).
                if error != rustix::io::Errno::SRCH {
                    self.failure = Some(format!(
                        "could not signal the process group {}: {error}",
                        self.child.id()
                    ));
                }
            }
        }
        let _ = self.child.wait();
    }

    fn kill_failure(&self) -> Option<String> {
        self.failure.clone()
    }

    fn end_group(&mut self) -> Result<usize, String> {
        // The leader must have ended: while it runs, its group is led by a live
        // process and `kill` is the road that takes it. `try_wait` on a reaped
        // child answers its cached status, so this is the same question `poll`
        // answered, asked again.
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => return Err("the command is still running — kill it instead".to_string()),
            Err(error) => return Err(format!("could not wait for command: {error}")),
        }
        let group = self.child.id();
        let members = group_members(group);
        if members.is_empty() {
            return Ok(0);
        }
        match kill_group(group) {
            Ok(()) => Ok(members.len()),
            // The members the read found are gone: between the read and the
            // signal the group died, and claiming to have stopped them would be
            // a kill that did not happen.
            Err(rustix::io::Errno::SRCH) => Ok(0),
            // The failure is the completion line's, not the log's: the owner is
            // told the group may still be running (finding E6's silent `let _`
            // is what this arm exists not to repeat).
            Err(error) => Err(format!(
                "could not signal the process group {group}: {error}"
            )),
        }
    }
}

/// `SIGKILL` to a whole process group, in process.
///
/// This used to be `Command::new("kill")` with `-9 -<pgid>`: a fork/exec whose
/// failure was an `ExitStatus` a `let _` could swallow, and which did nothing at
/// all on a machine where `kill` is not on `PATH` (finding E6). The signal is a
/// syscall, and `rustix` wraps it without `unsafe`; nothing about ending a group
/// mush started should depend on a program the environment can hide.
///
/// `ESRCH` — "the group is already gone" — is returned rather than read here:
/// [`Job::kill`] reads it as a kill that had nothing left to take, and
/// [`Job::end_group`] as nothing left to end.
#[cfg(unix)]
fn kill_group(group: u32) -> Result<(), rustix::io::Errno> {
    let pgid = rustix::process::Pid::from_raw(group as i32)
        .expect("a child's pid is never 0, and a process group id is a pid");
    rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL)
}

/// The pids a process group still holds, read from `/proc`.
///
/// A process group id *is* its leader's pid, and `/proc/<pid>/stat`'s fourth
/// field is the group each process is in (`pid (comm) state ppid pgrp …`). The
/// `comm` in the middle may contain spaces and parentheses, so the parse starts
/// after its last `)`. Nothing is signalled to ask the question and nothing is
/// signalled on a guess: a pid is not reissued while a process group still
/// holds it, so a member found here is proof that the id is still the
/// command's own (see [`Job::end_group`]).
///
/// A platform without `/proc` cannot be asked; the honest answer there is the
/// empty one, because this file will not signal an id it cannot prove.
pub(crate) fn group_members(pgid: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut members = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        if rest
            .split_whitespace()
            .nth(2)
            .and_then(|group| group.parse::<u32>().ok())
            == Some(pgid)
        {
            members.push(pid);
        }
    }
    members
}

/// A command's output file, removed when it is dropped.
///
/// `NamedTempFile` picks the name and creates it exclusively, so a name another
/// process guessed can never redirect or read what a command prints — the
/// property the hand-rolled counter and `0600` tried to buy. It is named
/// (rather than an O_TMPFILE handle) because the child has to inherit a path it
/// can write to — and the name carries the pid of the mush it belongs to
/// (`mush-cmd-<pid>-<random>-<kind>`), which is what lets a later start tell a
/// dead mush's leftovers from a live one's and reap the first
/// ([`reap_dead_scratch`]).
struct Scratch {
    file: NamedTempFile,
}

/// What every scratch file's name starts with, before the owning mush's pid.
const SCRATCH_PREFIX: &str = "mush-cmd-";

impl Scratch {
    fn new(kind: &str) -> Result<Self, String> {
        Builder::new()
            .prefix(&format!("{SCRATCH_PREFIX}{}-", std::process::id()))
            .suffix(&format!("-{kind}"))
            .tempfile()
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

/// Reap the scratch files a mush that is gone left behind.
///
/// A `NamedTempFile` is removed by `Drop` and by nothing else, and a mush that
/// dies without unwinding runs no `Drop` — a killed mush (finding E1), or any
/// exit that skips destructors — so the pair stays where it was, with the
/// orphaned command still writing into it and no watcher left to cap it
/// (finding E5). The name carries the pid of the mush that owned the file, so a
/// start can ask the kernel which mushes are alive and reap only a dead one's:
/// never a live mush's pair, on any workspace or on none.
///
/// The files live in the temp directory rather than under `.mush/`, which mush
/// also owns, because they are *process* scratch and not workspace state: they
/// exist for as long as one command runs, they hold what that command printed,
/// and a workspace is a place the file tools, the search and the model's own
/// tree read. Under `.mush/` a command's output would become part of what the
/// tools can open and replace, and a store road that pruned or rewrote an
/// unknown entry could unlink a file a *live* command is writing — the readers
/// reopen by path, so their answer would be an empty window. The temp directory
/// is also where these files already were; the pid in the name is the only
/// change a stranger needs to reap them.
pub fn reap_dead_scratch() {
    let dir = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // A name from before the pid was in it (or anything else): not ours to
        // judge, and a live mush of an older build could still be writing one.
        let Some(rest) = name.strip_prefix(SCRATCH_PREFIX) else {
            continue;
        };
        let Some((pid, _)) = rest.split_once('-') else {
            continue;
        };
        let Ok(pid) = pid.parse::<i32>() else {
            continue;
        };
        // `from_raw` refuses 0 and negatives: `kill(0)` is the caller's own
        // process group, and no mush ever wrote a name like that.
        let Some(pid) = rustix::process::Pid::from_raw(pid) else {
            continue;
        };
        // `test_kill` is the question "could this pid be signalled": `Ok` and
        // `EPERM` both mean the process exists (the second is one that is not
        // ours to signal), and only `ESRCH` means nobody is there. A pid that a
        // dead mush once held and an unrelated process now holds reads as
        // alive, which leaves a file behind — the safe direction.
        match rustix::process::test_kill_process(pid) {
            Ok(()) | Err(rustix::io::Errno::PERM) => continue,
            Err(_) => {}
        }
        let _ = std::fs::remove_file(entry.path());
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
        /// Processes the command leaves in its group when its leader ends —
        /// the `cmd &` shape, scripted, so the completion line and the kill
        /// are asserted without a real `sleep`.
        pub left_behind: usize,
        /// The sentence a kill answers with instead of ending the group: the
        /// shape finding E6's report is asserted on. `None` is the ordinary
        /// kill, and either way a second kill is a no-op.
        pub kill_error: Option<String>,
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

        /// A command that ends with `processes` still in its group: the
        /// `sleep 60 & echo done` shape (finding E3).
        pub fn leaves(mut self, processes: usize) -> Self {
            self.left_behind = processes;
            self
        }

        /// A kill that cannot end the group, saying this (finding E6).
        pub fn kill_fails(mut self, why: &str) -> Self {
            self.kill_error = Some(why.to_string());
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
                failure: None,
                kills: self.kills.clone(),
            }))
        }
    }

    struct ScriptedJob {
        script: Script,
        polls: usize,
        written: u64,
        killed: bool,
        /// What a kill that could not end the group said; read once by the
        /// watcher building the completion (finding E6).
        failure: Option<String>,
        kills: Arc<AtomicUsize>,
    }

    impl Job for ScriptedJob {
        fn poll(&mut self) -> Result<Option<End>, String> {
            self.written += self.script.grows;
            let polls = self.polls;
            self.polls += 1;
            // A killed command is a *dead* command, and the real shell says so:
            // the child is reaped with no exit code at all, which is a death by
            // signal — `Running::kill` sends `SIGKILL` to the child and to its
            // group in process. A fake that kept a killed command
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
                self.failure = self.script.kill_error.clone();
            }
        }

        fn kill_failure(&self) -> Option<String> {
            self.failure.clone()
        }

        fn end_group(&mut self) -> Result<usize, String> {
            // Answered once, like the real one: after the kill the group is
            // empty, and a second ask finds nothing.
            let left = self.script.left_behind;
            self.script.left_behind = 0;
            Ok(left)
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

    /// Run one command through the real [`Shell`] and wait for it to end.
    ///
    /// The wait is bounded and the job killed on the way out: a command that
    /// never ends here is a defect in the test, not a hung suite.
    #[cfg(unix)]
    fn run_to_end(command: &str, root: &std::path::Path) -> End {
        use super::{Machine, Shell, ShellCommand};

        let mut job = Shell
            .spawn(&ShellCommand { command, root })
            .expect("the real shell starts");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match job.poll().expect("a status is readable") {
                Some(end) => return end,
                None if std::time::Instant::now() >= deadline => {
                    job.kill();
                    panic!("`{command}` did not end within 10s");
                }
                None => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
    }

    /// `MUSH_API_KEY` set for one test, put back when it ends — panic or not.
    /// The environment belongs to the whole process, and a probe key left
    /// behind would be read by every test that runs after this one.
    #[cfg(unix)]
    struct KeyInProcess(Option<std::ffi::OsString>);

    #[cfg(unix)]
    impl KeyInProcess {
        fn set(value: &str) -> Self {
            let previous = std::env::var_os("MUSH_API_KEY");
            std::env::set_var("MUSH_API_KEY", value);
            Self(previous)
        }
    }

    #[cfg(unix)]
    impl Drop for KeyInProcess {
        fn drop(&mut self) {
            match &self.0 {
                Some(previous) => std::env::set_var("MUSH_API_KEY", previous),
                None => std::env::remove_var("MUSH_API_KEY"),
            }
        }
    }

    /// The key is mush's, not the model's (finding C1). A command run through
    /// the real [`Shell`] cannot read `MUSH_API_KEY`, even while the process
    /// that spawned it holds one: the command writes what `printenv` printed to
    /// a file in the job's root, and the file is empty. The exit status is
    /// `printenv`'s for a name that is not set — the two halves of "unset",
    /// where an empty variable would still be a variable.
    #[cfg(unix)]
    #[test]
    fn a_command_never_sees_mushs_key() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("key");
        let _key = KeyInProcess::set("sk-probe-inheritance-0123456789");
        let end = run_to_end(
            &format!("printenv MUSH_API_KEY > {}", out.display()),
            dir.path(),
        );
        let seen = std::fs::read_to_string(&out).unwrap();
        assert_eq!(
            seen, "",
            "a command read mush's credential out of its environment: {seen:?}"
        );
        assert_eq!(end, End::Exited(1), "printenv exits 1 for an unset name");
    }

    /// The scrub is a removal, not `env_clear`: the command keeps the
    /// environment the human's own tooling was written against. `PATH` is the
    /// one every command needs, and it reaches the child byte for byte.
    #[cfg(unix)]
    #[test]
    fn a_command_keeps_the_environment_it_needs() {
        // The `PATH` swap finding E6's shim tests do is process-wide: hold the
        // same lock, so the child's `PATH` and the parent's are read in one
        // state.
        let _path = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("path");
        let end = run_to_end(&format!("printenv PATH > {}", out.display()), dir.path());
        let seen = std::fs::read_to_string(&out).unwrap();
        assert_eq!(
            seen.trim_end_matches('\n'),
            std::env::var("PATH").unwrap(),
            "the command's PATH is the human's"
        );
        assert_eq!(end, End::Exited(0), "printenv found PATH");
    }

    /// The name is the contract a later start reaps by: the pid of the mush
    /// that owns the file, the stream it carries, and the temp directory the
    /// reaper sweeps (finding E5).
    #[test]
    fn a_scratch_file_is_named_after_the_mush_that_owns_it() {
        let scratch = super::Scratch::new("out").unwrap();
        let path = scratch.file.path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with(&format!("mush-cmd-{}-", std::process::id())),
            "the owning mush is in the name: {name}"
        );
        assert!(name.ends_with("-out"), "and so is the stream: {name}");
        assert_eq!(
            path.parent(),
            Some(std::env::temp_dir().as_path()),
            "the sweep and the file agree on the directory"
        );
    }

    /// A start reaps a dead mush's pair — the road a SIGTERM'ed mush's own
    /// files leave — and never touches a live mush's, or a name it cannot read
    /// a pid out of (finding E5).
    #[test]
    fn a_start_reaps_a_dead_mushs_scratch() {
        // A pid that is really gone: a child of this test, waited for.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let live = std::process::id();
        let names = [
            format!("mush-cmd-{dead}-aaaaaa-out"),
            format!("mush-cmd-{dead}-bbbbbb-err"),
            format!("mush-cmd-{live}-cccccc-out"),
            format!("mush-cmd-{live}-dddddd-err"),
            // The old shape, without a pid: a live mush of an older build could
            // still be writing one, so the reaper does not guess.
            "mush-cmd-out-legacy".to_string(),
        ];
        let paths: Vec<std::path::PathBuf> = names
            .iter()
            .map(|name| std::env::temp_dir().join(name))
            .collect();
        for path in &paths {
            std::fs::write(path, b"scratch").unwrap();
        }

        super::reap_dead_scratch();

        assert!(
            !paths[0].exists() && !paths[1].exists(),
            "the dead mush's pair is gone"
        );
        assert!(
            paths[2].exists() && paths[3].exists(),
            "the live pair is untouched"
        );
        assert!(paths[4].exists(), "a name without a pid is not guessed at");
        for path in &paths {
            let _ = std::fs::remove_file(path);
        }
    }

    /// The `PATH` is process-wide and the tests run in parallel: the ones that
    /// prepend a shim hold this, and so does the one that asserts a child's
    /// `PATH` is the human's — a swap must not be read halfway.
    #[cfg(unix)]
    static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A `PATH` with `dir` first and the human's own after it: a program in
    /// `dir` shadows one name (`kill`) without taking `sh`, `sleep` or
    /// `printenv` out of every other test's environment. The guard puts the
    /// human's `PATH` back, panic or not.
    #[cfg(unix)]
    struct PrependedPath(Option<std::ffi::OsString>);

    #[cfg(unix)]
    impl PrependedPath {
        fn new(dir: &std::path::Path) -> Self {
            let previous = std::env::var_os("PATH");
            let mut paths = vec![dir.to_path_buf()];
            paths.extend(
                previous
                    .as_deref()
                    .map(std::env::split_paths)
                    .into_iter()
                    .flatten(),
            );
            std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
            Self(previous)
        }
    }

    #[cfg(unix)]
    impl Drop for PrependedPath {
        fn drop(&mut self) {
            match &self.0 {
                Some(previous) => std::env::set_var("PATH", previous),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    /// A `kill` program in `dir`, ahead of the real one on `PATH`, that logs
    /// every invocation and then does what `body` says. The logging is how a
    /// test sees the fork/exec road (finding E6): once the group signal is in
    /// process, the program is never run and the log stays empty.
    #[cfg(unix)]
    fn kill_shim(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let log = dir.join("kill.log");
        let shim = dir.join("kill");
        std::fs::write(
            &shim,
            format!("#!/bin/sh\necho \"kill $*\" >> {}\n{body}\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        log
    }

    /// The real `kill` program's path, found before the shim shadows the name:
    /// the body a shim delegates to when the test wants the group to die by the
    /// program's own hand — the road the fix removes.
    #[cfg(unix)]
    fn real_kill() -> std::path::PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join("kill"))
            .find(|path| path.is_file())
            .expect("this box has a kill program")
    }

    /// A process group that must not outlive its test: an assertion that fails
    /// before the kill would otherwise leave a spinning shell behind, so the
    /// guard signals the group in process the way the fix does.
    #[cfg(unix)]
    struct GroupGuard(i32);

    #[cfg(unix)]
    impl Drop for GroupGuard {
        fn drop(&mut self) {
            let _ = super::kill_group(self.0 as u32);
        }
    }

    /// One command running in its own process group with a member the leader
    /// does not wait on, so a group signal has something to end: the shape
    /// finding E6 is about. The command writes its group id to `pgid` in
    /// `root` first.
    #[cfg(unix)]
    fn waiting_group(root: &std::path::Path) -> (Box<dyn super::Job>, i32) {
        use super::{Machine, Shell, ShellCommand};

        let job = Shell
            .spawn(&ShellCommand {
                command: "echo $$ > pgid; while :; do :; done & wait",
                root,
            })
            .expect("the real shell starts");
        let pgid_file = root.join("pgid");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !pgid_file.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let pgid = std::fs::read_to_string(&pgid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        (job, pgid)
    }

    /// Two kills signal the group once. The first takes the leader and its
    /// group in process; the second is a no-op, because the id the first freed
    /// may already be somebody else's process group (finding E6). The shim logs
    /// every time the `kill` program runs — the old road ran it twice, the
    /// second time at a freed group; the fix runs it not at all.
    #[cfg(unix)]
    #[test]
    fn a_second_kill_signals_nothing() {
        use super::group_members;

        let _path = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let real = real_kill();
        let log = kill_shim(dir.path(), &format!("exec {} \"$@\"", real.display()));
        let _shimmed = PrependedPath::new(dir.path());

        let root = tempfile::tempdir().unwrap();
        let (mut job, pgid) = waiting_group(root.path());
        let _guard = GroupGuard(pgid);
        assert!(!group_members(pgid as u32).is_empty(), "the group is up");

        job.kill();
        job.kill();

        assert!(
            crate::jobs::wait_group_gone(pgid).is_empty(),
            "the first kill ended the group"
        );
        assert_eq!(
            std::fs::read_to_string(&log).unwrap_or_default(),
            "",
            "no `kill` program ran: the group signal is in process, once"
        );
        assert_eq!(
            job.kill_failure(),
            None,
            "the second call is a no-op with nothing to report"
        );
    }

    /// A group mush must end is ended even where the `kill` program cannot do
    /// it: the shim answers what a machine without one answers (`exit 127`),
    /// and the group dies anyway, because the signal is a syscall now (finding
    /// E6's silent half). The old road's discarded `ExitStatus` left the
    /// spinner running.
    #[cfg(unix)]
    #[test]
    fn a_kill_without_the_kill_program_still_ends_the_group() {
        use super::group_members;

        let _path = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let log = kill_shim(dir.path(), "exit 127");
        let _shimmed = PrependedPath::new(dir.path());

        let root = tempfile::tempdir().unwrap();
        let (mut job, pgid) = waiting_group(root.path());
        let _guard = GroupGuard(pgid);
        assert!(!group_members(pgid as u32).is_empty(), "the group is up");

        job.kill();

        assert!(
            crate::jobs::wait_group_gone(pgid).is_empty(),
            "the group ended without a working `kill` program"
        );
        assert_eq!(
            std::fs::read_to_string(&log).unwrap_or_default(),
            "",
            "and the program was never asked"
        );
    }
}
