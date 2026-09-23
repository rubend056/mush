# The processes and the resources mush owns, audited

A blind audit of the layer where a defect costs a machine rather than a line:
the job registry and the commands it keeps, the shell seam that spawns, watches
and kills them, the machine's one lock, and the thread that writes the session.
"Blind" here means no doc comment is taken as true: each one is a claim, and
what follows is what the code does.

**Base:** `5889590` (`merge: the secrets, the session store and the
configuration, audited blind`), my own worktree. Nothing here was fixed — the
audit file is the only file I created or changed, every probe was deleted, and
`git status` is clean.

**How.** `crates/mush/src/jobs.rs` (2 352 lines), `machine.rs` (486), `lock.rs`
(148) and `session_save.rs` (379) were read end to end — every line, tests
included — and the job-watching roads into `app/mod.rs`, `main.rs` and `agent.rs`
were read as far as they reach those four files (the census at the end says
which). Then probes, all throwaway and all deleted: a `#[cfg(test)] mod
audit_probe` declared from `main.rs`, one appended test in `session_save.rs`, a
PATH shim that logs `kill(1)` invocations, and the real binary driven from a
pty. Every measurement quoted below is a real run on this machine (Linux,
`cargo test -p mush` in the debug profile, plus `target/debug/mush` from a pty,
`/proc` readings and `ps`). The full suite on this base is green: **721 passed,
4 ignored, 0 failed** in 30.16 s.

**Not re-reported.** `docs/audits/tools-and-workspace.md` (B1–B16) and
`docs/audits/secrets-session-config.md` (C1–C12) were read first, and
`docs/findings.md` §8.44–§8.50 with ledger H41–H48: nothing closed is repeated
here. Where a mechanism is the same I write "as in B3" and move to what the
audit does not own; where a *fix* in flight or proposed does not reach this area
I say so (§"the env road, and C1's fix" and E2's attack on B3's remedy).

**The verdict in one line.** Ten findings, three of them major, and the two
biggest are the same fact from two sides: **a command's process group outlives
mush on every road that is not the human's own Ctrl-Q** — a killed mush (E1), a
command that backgrounds a child (E3) — and the exit-cleanup that does run is
enforced by nothing but `Drop`. No blocker was found: every ordinary path keeps
its word, and the losses are processes, disk and a lock, not a file's content.

---

## Priority order

What to fix first, in the human's terms.

1. **E1 — a killed mush leaves every process group running.** Closing the
   terminal, `systemctl --user stop`, `kill`, an OOM kill: mush dies at once,
   its children do not, and nothing ever tells the human which. This is the
   harm S4/H9 fixed for the clean quit, left open on the road that happens when
   a terminal window is closed.
2. **E2 — the workspace lock is a file the file tools can replace.** One
   `write_file(".mush/lock", …)` and two mush processes own one store, writing
   whole-file sessions over each other — the damage `lock.rs` exists to
   prevent, and end-to-end reproducible.
3. **E3 — a command that backgrounds a child escapes the registry.** The job
   says `done: exit 0`, the group keeps running, and Stop, Ctrl-N, Ctrl-X, the
   quit and the 4 h ceiling all walk past it: a ghost the job system was built
   to prevent, reachable with a trailing `&`.
4. **E4 — a panicking actor orphans its command and leaves the machine lock
   held.** The recovery is worse than the bug: no surface lists the process,
   and every sibling's `run_command` is refused for the rest of the session by
   an agent that is gone (suspected trigger, proven mechanism).
5. **E5 — `/tmp` scratch files outlive a mush that does not unwind**, with an
   orphan still writing into one; the disk bound died with the watcher.
6. **E6 — the group kill is issued again at a freed process-group id, and its
   failure is silent.** A small window, a machine-wide blast radius, one flag
   to fix.
7. **E7 — the session writer's `flush` has no deadline and no dead-worker
   check**: a worker that died by panic freezes the UI inside a flush and loses
   the transcript with no word.
8. **E8 — the lock's one sentence to a human can name a dead pid**, and the
   recorded `lock::tests` flake cannot be a stale lock — the shapes that remain.
9. **E9 — a handed-over job's age starts at the handover**: `running 0s` for a
   command that has burned a minute of the machine.
10. **E10 — the global panic hook restores the terminal from any thread**: a
    worker's death smears a running TUI into the human's shell.

---

## Census of the area

| File | Lines | Read |
|---|---|---|
| `crates/mush/src/jobs.rs` | 2 352 | end to end (registry, lock record, `Live`/`Foreground`, `watch`, `preview`, all tests) |
| `crates/mush/src/machine.rs` | 486 | end to end (`Shell`, `Running`, `ended`, `Scratch`, the fake) |
| `crates/mush/src/lock.rs` | 148 | end to end (`acquire`, `Guard`, `holder`, the three tests) |
| `crates/mush/src/session_save.rs` | 379 | end to end (`Writer`, `Pending`, `writer`/`drain`, the fake, the four tests) |
| `crates/mush/src/main.rs` | 2 039 | ~530: `main`'s startup/lock/app/terminal order, `install_panic_hook`, the event loop, `--print-config`'s no-lock road |
| `crates/mush/src/app/mod.rs` | 15 831 | ~825: `App::drop`, `new_chat`, `stop_all`, `park_history`/`parkable`'s job filter, `request_quit`/`what_a_quit_kills`/`interrupt`/`stop_one`, `report_cut_off`, `flush_session`/`save_session`/`tick`'s error poll, the `JobStarted`/`JobDone` arms, `live_jobs`/`job_lines` |
| `crates/mush/src/agent.rs` | 15 867 | the job roads (~1 100): `run_command`, `run_shell`, `wait_bounded`, `ending`, `end_note`, `detach_now`, `detach_line`, `wait_for_machine`, `beside_note`, `machine_refusal`, `wait_tool`/`wait_tick`, `absorb`/`drain_signals`/`drain_mailbox`'s `Stop`/`Shutdown`/`CommandDone` arms, `start`, `reclaim_own_worktree` |

The four files this audit owns are 3 365 lines. `mush-core`'s
`workspace.rs::write_file`/`atomic_write` and `session.rs::save` were read where
they carry the lock file and the store.

---

## E1 — a killed mush leaves every process group running (major, proven)

**What is wrong.** Every command mush starts is put in its own process group
with `/dev/null` for stdin (`machine.rs:104`, `machine.rs:112`), and the entire
duty of ending them — `kill_all` for jobs *and* for the commands a tool call is
holding, the writer's join and the session's flush, the attach socket's removal
— lives in `Drop` (`app/mod.rs:4395` → `4410`, `main.rs:921`–`934` for the
writer, the app and the socket guard whose drop order is the cleanup,
`session_save.rs:169`). There is no signal road: `install_panic_hook`
(`main.rs:1103`) is the only `set_hook` in the crate,
`grep -rn "SIGTERM\|SIGHUP\|sigaction" crates/` answers nothing outside prose,
`crates/mush/Cargo.toml`
has no `libc` and no `signal-hook`/`ctrlc` (the only syscall crate is `rustix`,
`features = ["fs"]`, and its `process`/`signal` modules are not enabled).
SIGTERM, SIGHUP and SIGINT therefore get the kernel's default disposition: the
process is gone before any destructor runs, every thread with it — and the
children, in their own process groups, with no terminal of their own and no
stdin, are in nobody's list.

**Evidence** (three real runs).

1. The real binary in a pty (the shape C's audit used): mush running as pid
   2781362 with `.mush/mush.sock` bound; `kill -TERM 2781362`; the process is
   gone — and the socket is still there:
   `socket after: /tmp/mush-audit-ws2/.mush/mush.sock`. That file is removed by
   exactly one thing, `attach`'s guard `Drop`, so its survival is proof that
   **every** `Drop` in the process was skipped, `App`'s `kill_all` included.
2. A parent that dies of the signal, with a child spawned the way mush spawns
   one (`os.setpgrp`, stdin `/dev/null`, stdout to a file):
   `parent exit=143` / `child 2781888 alive after the parent's SIGTERM? yes;
   ticks now: 31 bytes` and, one second later, `51 bytes` — still writing. The
   same with `SIGHUP`: `parent exit=129`, `child 2782021 alive ... yes`.
3. My own mush-shaped probe (the real `Shell`, then `std::process::exit(0)`,
   which is what a signal gives): the process died and its
   `sh -c 'echo tick; while :; do echo tick; sleep 0.2; done # mush-audit-tick'`
   was still running **two minutes** later — pid/pgid 2757634,
   `/proc/2757634/fd/1 -> /tmp/mush-cmd-out-iPNHkk`, the file grown to 3 020
   bytes. `CMD_OUTPUT_LIMIT` and `JOB_MAX_AGE` are the watcher's rules, and the
   watcher is a thread of the dead process.

**Blast radius.** A cold `cargo build` (holding `target/`), a dev server
(holding a fixed port), a benchmarking run (holding every core) — the ghosts
`docs/mush.md` §5.6 promises are killed "on exit" and finding S4/H9 removed for
the clean quit — survive the terminal being closed (the commonest way a TUI
dies), a session manager's stop, an OOM kill, or an IDE's stop button. The
human cannot find them without `ps`: mush holds no record, and the workspace
lock is released (correctly) so the next mush starts on the same workspace and
says nothing about what is still running in it. The exit flush is skipped too,
so up to `SESSION_DEBOUNCE` of chat is lost — the store itself cannot be torn,
because `Session::save` is a rename (`session.rs:328`–`339`), which is the one
part of this that is already safe.

**Suggested fix.** One signal road, in the shape the tree already uses for
"everything that ends the program": a handler for SIGTERM/SIGHUP/SIGINT (via
`rustix`'s `process`/`signal` modules, already an in-tree dependency, or a
self-pipe) that sets an `AtomicBool` the 30 ms event loop reads, so the signal
takes the *same* road as `Ctrl-Q`: `App::drop` → flush → `kill_all` → writer
join → socket removal. `catch_unwind` is not needed: the unwinding roads already
run `Drop`, and the panic hook is in the way of one of them (E10).

**Acceptance test.** `a_sigterm_takes_the_quit_road`: run the real binary in a
pty, start a detached `sleep 600; touch marker`, `kill -TERM` the mush pid, and
assert (a) the job's process group is gone within a bounded wait, (b) `marker`
was never created, (c) the messages the debounce had not written are in
`session.json`, (d) `.mush/mush.sock` is gone.

---

## E2 — the workspace lock is a file the file tools replace: two mushes on one store (major, proven)

**What is wrong.** `lock.rs` builds its whole argument on the file's
permanence: "The file is deliberately never unlinked, not even on a clean exit.
Unlinking it is what makes a lock file racy: a third process that opens the path
between the unlink and the next `open` gets a *new* inode, locks that, and two
processes believe they hold the workspace. A file that is always there cannot be
raced" (`lock.rs:17`–`21`). The guard is an fd on that inode (`lock.rs:43`), and
the lock is `flock(2)` on it (`lock.rs:61`). But the *name* is inside the
workspace, and the workspace's own tools replace names: `write_file`
(`mush-core/src/workspace.rs:989`–`1000`) refuses only the workspace root and
then `atomic_write`s, which is a temp file plus `rename`
(`workspace.rs:1315`) — a **new inode at the same path**. `.mush/lock` is an
ordinary regular file, so no shape check refuses it (B3's socket/FIFO remedy
does not reach it: it *is* a regular file). The first mush keeps the flock on
the now-unlinked inode and believes it owns the workspace; the second opens the
new file, locks it, and also believes it.

**Evidence.**

1. In-process probe: `lock::acquire(&root)` → a live guard;
   `Workspace::write_file(".mush/lock", "a file, not a lock\n")` → `Ok(())`;
   `lock::acquire(&root)` again →
   `PROBE second acquire with the first guard alive: Ok("Ok — both think they own the workspace")`.
2. End to end with the real binary: mush A (pid 2790331) holding a workspace;
   the lock file replaced the way `atomic_write` does (write `lock.new`, `mv`);
   mush B started in the same directory — and **both are alive**:
   `A alive? yes  B alive? yes`, the lock file now naming B (2790472). The only
   complaint on B's screen was the socket:
   `mush: attach disabled — could not bind /tmp/mush-audit-ws3/.mush/mush.sock: Address already in use (os error 98)`
   — and `lock.rs:9`–`11` says in as many words that the socket "is deliberately
   not fatal", so the second guard did not hold either.

**Blast radius.** Two whole-file session writes on one `.mush/session.json`,
alternating on a minute's debounce: whichever saved last is the conversation
that survived a restart, and the other session's work — a child's transcript, a
long run's results — is simply gone, with no word in either. That is verbatim
the damage the module exists to prevent, reached by one tool call ("reset
`.mush`", "clean up stale locks", "recreate the lock file"). The trigger can also
be the human's own `mv` of a backup over the path.

**Suggested fix.** Guard the name, not only the shape: `write_file`/`edit_file`
refuse a target that is `mushroom_dir(root).join("lock")` (and, while there, the
session file, whose replacement loses the conversation differently — C's
territory), with a sentence naming the file and why; `atomic_write`'s callers
under `.mush/` are only mush's own. A cheap belt on top: `Guard` can keep the
path and the inode it locked, and the app can check identity before a save
(`metadata(path).ino() != locked_ino` → refuse to write, say so), so a mush
whose lock was replaced stops writing instead of silently sharing the store.

**Acceptance test.** `a_write_cannot_replace_the_workspace_lock`: a workspace
with `lock::acquire` held; `write_file(".mush/lock", …)` refused with a sentence
naming the file; a second `acquire` still refused; and a positive twin — an
ordinary file under `.mush/` still writes.

**Against B3's fix.** B3's remedy is a shape check in `write_file` ("if
`symlink_metadata` says it is neither a regular file nor a symlink-that-resolves
to one, refuse"). It closes the socket and the FIFO and leaves this open, because
the lock file is a regular file by construction. The write road needs a
*name*-level refusal for the store's own files; the shape check is the other
half, not this one.

---

## E3 — a command that backgrounds a child escapes the registry, the ceiling and the quit (major, proven)

**What is wrong.** A job's end is the *direct child's* end: `Running::poll` is
`self.child.try_wait()` (`machine.rs:155`–`162`), the group leader. A command
whose shell exits while its own children live — `cmd &`, a script that
double-forks, a `daemonize`d helper — therefore reports `Exited(0)` at once. The
record moves to `State::Ended` (`jobs.rs:1248`–`1266`), and every road that can
end a process walks `State::Running` only: `Registry::kill` (`jobs.rs:1152`) for
`kill_owned`/`kill_all`, the `JOB_MAX_AGE` ceiling inside `watch`
(`jobs.rs:1327`–`1395`), `Registry::stop`. The child sits in the leader's own
process group — the group `Running::kill` exists to take down — and no mush road
looks at it again. (This is the "kill and cancel: what a shell's own children
do" question, answered by the code with "they are forgotten".)

**Evidence** (probes over the real `Shell`).

1. `job.poll()` answered `Some(Exited(0))` while the group still held a live
   `sleep`: `PROBE poll said Some(Exited(0)) while 2760884 still holds [(2760887, "sleep")]`.
2. Through the registry: after the leader exited,
   `PROBE list a quit walks: []` — `live_for(7)` was empty — and after
   `registry.kill_all()` (what `Ctrl-Q`, `Ctrl-N` and `report_cut_off` run) the
   background child was still there:
   `PROBE group after kill_all: [(2760919, "sleep")]`.

**Blast radius.** A build, server or benchmark started with a trailing `&` never
appears in `status`, never wakes its owner, never spends the `MAX_JOBS` budget,
never hits the four-hour ceiling and survives `Stop` (Ctrl-C), `Ctrl-X`, `Ctrl-N`
and the quit — the machine-wide ghost, with a working trigger that is one
ordinary command. Its output keeps flowing into an unlinked scratch file (E5),
which the 8 MB `CMD_OUTPUT_LIMIT` can no longer stop, because the watcher that
enforces it is gone. `docs/mush.md` §5.6's "kill -9 -pgid to take the whole
group down" is true only while the leader is alive.

**Suggested fix.** Decide, then say it. The group is mush's by construction
(`process_group(0)`), so the honest reading of rule 1 is: when the leader exits,
if the group still has members, kill it (and, if the record is to report
`Exited`, report the group's ending too: "`#c2 done: exit 0 — 2 processes in its
group were stopped`"). The cheaper alternative — document that `&` leaves work
outside mush — is a contract the human will not read and a hole the model can
fall into accidentally; I would take the kill. Either way the `Job` trait's own
words ("Stop it and everything it started") and §5.6's claim become true again.

**Acceptance test.** `a_job_takes_the_whole_group_with_it`: a job running
`sleep 60 & echo done` — after the completion line, the group is empty (no
`/proc` entry with that pgid), the line says the group was ended, and the same
command run as a foreground tool call leaves nothing behind either.

---

## E4 — a panicking actor drops its hold without killing: an unreachable group and a machine lock nobody can clear (minor; mechanism proven, trigger suspected)

**What is wrong.** Two facts about one Drop.

1. `impl Drop for Foreground` (`jobs.rs:503`–`516`) never kills, and for the
   *finished* call that is right: the child was reaped, and a
   `kill -9 -pgid` at a freed id can land on somebody else's group (the doc
   says so). But this Drop is also the one a panicking actor's unwinding runs:
   `forget_foreground` (`jobs.rs:878`) removes the map entry, and the running
   process group is then in *neither* map — out of `kill_owned`'s,
   `kill_all`'s and `Registry::Drop`'s reach. Nothing in the crate catches an
   unwinding panic (`grep -rn catch_unwind crates/` → nothing, and the crate
   forbids `unsafe`), so any panic in tool or prompt code while a command runs
   takes this road; C9 records one such panic in this tree
   (`attempt to add with overflow`, in a *debug* build, at `app/mod.rs:743`).
2. The machine lock is taken before the command is spawned
   (`agent.rs:5107`–`5108`) and released only on the paths that return
   (`agent.rs:5125`, `5168`, and `detach_now`'s error arm at `5213`). A panic
   between the two
   leaves `holder = Some((owner, cmd, None))` (`jobs.rs:1003`–`1060`) with no
   road to clear it: `release_machine` is the holder's alone, `Registry::stop`
   refuses any other caller, `report_cut_off` kills the jobs but never touches
   the holder record, and the UI has no key that releases the machine.

**Evidence.** The mechanism, with the panic's Drop reproduced exactly: hold a
real command (`registry.hold(7, spawn("sleep 30"))`), `drop(held)`, then
`registry.kill_all()` — `PROBE group left after the hold was dropped and kill_all
ran: [(2761487, "sleep")]`. Nothing in the registry can reach it any more, and
the `Child` is dropped with it, so the process is never `wait`ed either (a
zombie in mush's own table until mush exits). The trigger (a real panic in a
live actor) was **not** driven — hence suspected — but its site is ordinary
code, and C9's panic is the precedent in this very tree.

**Blast radius.** A ghost command no surface lists and no key stops (as in E3),
*plus* a machine-wide lock held by an agent that no longer exists: every
sibling's `run_command` queues `LOCK_QUEUE` (30 s) and is refused, with a
sentence naming `#N` — an agent whose row may have been reaped — for the rest of
the session. The human's only exits are `Ctrl-N` (losing the conversation) or a
restart.

**Suggested fix.** Kill on the panic road, safely: `Foreground`'s `Drop` may
kill when its own `poll()` answers `Ok(None)` (still running, therefore *not yet
reaped*, therefore a pid that cannot have been reused — the same fact that makes
the current no-kill rule right for a dead child). And `kill_owned` should clear
the holder when the holder's agent is the one being killed
(`if holder.agent == owner { holder = None }`), so `report_cut_off` — the UI's
road for an actor that vanished — frees the machine as well as the jobs.

**Acceptance test.** `a_panicking_agent_kills_its_command_and_frees_the_machine`:
an actor whose tool panics on purpose (behind `#[cfg(test)]`) — the process group
is gone, `held()` is `None`, and a sibling's `take_machine` succeeds; plus a unit
test that `Foreground::drop` kills a still-running command and signals nothing
for a reaped one.

---

## E5 — the scratch file is removed only by Drop: a mush that does not unwind leaves `/tmp/mush-cmd-*` behind, with an orphan still writing into it (minor, proven)

**What is wrong.** `Shell::spawn` makes two `NamedTempFile`s
(`machine.rs:97`–`98`, `202`–`206`) prefixed `mush-cmd-out-`/`-err-`, and hands
the child a reopened fd for each. The files are unlinked by `Drop` alone
(`tempfile`'s) — there is no reaper at startup, no record of the paths, no
`tmpfiles.d` rule, nothing else in the tree that names them
(`grep -rn "mush-cmd-" crates/` → `machine.rs:203` and the probe). A mush that
dies without unwinding (E1) therefore leaves two files per command it was
running, and a child that is still alive keeps writing into its inheritance with
no watcher, no cap and no reader.

**Evidence.** `/tmp` held **18 `mush-cmd-*` files** before my probe's first run,
all `0600`, two per command, the oldest eleven minutes old and none of them mine
at that point — a `NamedTempFile` is removed by `Drop` and by nothing else, so
every one of them is the residue of a mush-shaped process that is gone. The probe
(the real `Shell`, then `std::process::exit(0)`) listed **20** as it exited, its
own two being `mush-cmd-out-iPNHkk 15B` and `mush-cmd-err-nOiFJS 0B`. Two
minutes after that process died, its child —
`sh -c 'echo tick; while :; do echo tick; sleep 0.2; done # mush-audit-tick'`,
pid/pgid 2757634 — was still running with
`/proc/2757634/fd/1 -> /tmp/mush-cmd-out-iPNHkk`, and the file had grown to
**3 020 bytes** (`CMD_OUTPUT_LIMIT` is the watcher's rule, and the watcher died
with the process). Sizes measured around the kill: 3 020 B before, 3 020 B after
— frozen only because I killed the orphan by hand. Fourteen such files were
still in `/tmp` after I removed my own two, which is the point: nothing ever
reaps them.

**Blast radius.** Disk, unbounded where a runaway writer is involved (a
`yes`-shaped command that E1 or E3 orphans can fill a tmpfs), and litter on the
shared `/tmp` every sibling agent and every human tool also uses. The one thing
that is right here is the mode: `-rw-------`, so the content is not readable by
another user — which is why this is a resource finding and not a leak.

**Suggested fix.** Name them so a stranger can reap them and reap them at
startup: `mush-cmd-<mush-pid>-<n>` in a fixed subdirectory (or under `.mush/`,
which mush already owns and B13's prune road covers), with `acquire`/`main`
sweeping entries whose mush pid is not alive. The `read_tail`/`size` roads don't
care where the file lives.

**Acceptance test.** `a_start_reaps_a_dead_mushs_scratch`: create
`mush-cmd-<dead-pid>-out`/`-err` plus one naming a live pid, start mush, assert
the first two are gone and the live pair is untouched; and, with E1's test, that
a SIGTERM'ed mush's own pair does not survive the next start.

---

## E6 — `Running::kill` fires `kill -9 -<pgid>` unconditionally: the second call aims at a freed group, and the `kill` subprocess's failure is silent (minor, proven)

**What is wrong.** `machine.rs:175`–`188`:

```rust
    fn kill(&mut self) {
        let group = self.child.id();
        let _ = self.child.kill();
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-9", &format!("-{group}")])
                …
                .status();
        }
        let _ = self.child.wait();
    }
```

The first call is safe: the leader is dead but not yet reaped, so its pid — and
therefore the group id — cannot have been reused, and the group kill lands on
the group mush started. The **second** call is not: `wait()` has reaped the
leader, `child.kill()` answers an `InvalidInput` that `let _` swallows, and the
group kill is a syscall by *number* against an id the kernel may have handed to
another process group. The trait's own word for this method is "Idempotent"
(`machine.rs:83`) and the fake keeps that promise
(`machine.rs:434`, `if !self.killed`) — so every scripted test sees an
idempotent kill while the real machine's is not. The same `let _` makes the
group kill's *failure* silent: no `kill` on `PATH`, and only the leader dies.

**Evidence** (a PATH shim logging `kill $* -> $status` around the real `kill`).

```
PROBE shim log:
kill -9 -2761506 -> 0
kill -9 -2761506 -> 1
kill -9 -2761539 -> 0
kill -9 -2761539 -> 1
```

The first pair is the *registry road*: `registry.stop(7, id)` (answered
`stopping job #c1`, asserted) and a second call issued microseconds later — the
watch thread has a whole `POLL` (10 ms) before it notices the first. The second
pair is the mechanism on its own: `job.kill()` twice, 100 ms apart. Each second
call really is issued, and `kill(1)` answers `1` — ESRCH, a group that no longer
exists. The silent-failure
half, with the same machine and the same shim directory:

```
PROBE with_kill=true  group before: [(2763283, "sleep"), (2763284, "sleep")]
PROBE with_kill: true  -> left in the group: []
PROBE with_kill=false group before: [(2763288, "sleep"), (2763289, "sleep")]
PROBE with_kill: false -> left in the group: [(2763289, "sleep")]
```

**Blast radius.** The double call is reachable on ordinary roads: `control stop`
twice (or a Stop and then `Ctrl-Q`/`Ctrl-C` before the watcher's poll), and
`launch`'s handover window, where the *same* `Live` is in the foreground map and
in the job list at once (`jobs.rs:1085`, `drop(held)` after the insert) so a
`kill_all` inside it kills one handle twice. On this box the window is small
(`pid_max` = 4 194 304, measured) but the consequence is a SIGKILL to a process
group mush never started — the harm `Foreground`'s no-kill rule exists to avoid,
committed one call earlier. The silent half costs the whole group on any machine
where `kill` is not on `PATH` (a slim container, a `PATH` mush inherited).

**Suggested fix.** One flag, the fake's own: `struct Running { killed: bool }`
so the second `kill` is a no-op for real; and do the group call in-process
rather than by fork/exec — `rustix` is already a dependency
(`rustix::process::kill_process_group`, `ESRCH` as "already gone") — reporting
any other failure once in the job's window (the `note` the watch thread already
carries) instead of discarding it. `child.wait()` stays after the group call.

**Acceptance test.** `a_second_kill_signals_nothing`: with a PATH shim that logs
invocations, `kill()` twice produces one `kill -9 -<pgid>` (and the second
answers `Ok`); `a_kill_without_the_kill_program_still_ends_the_group`: B's
two-arm probe above, inverted, passes on the fix.

---

## E7 — the session writer's `flush` has no deadline and no liveness check: a dead worker freezes the UI and loses the transcript silently (minor, proven mechanism)

**What is wrong.** `SessionSave::flush` (`session_save.rs:150`–`165`) parks a
`Sender` in `pending.waiting` and blocks on `waited.recv()`. The only thing that
wakes it is `drain` (`198`), which only the worker thread runs (`182`). If that
thread is gone, the waiter stays in the queue forever and the call never
returns — and the caller *is* the UI thread: the human's `Enter`
(`flush_session`), any `/command` that flushes, the `Ctrl-N` road (`new_chat`),
and `App::drop` on the way out. A worker can be gone by panic:
`drain`, `save`, `flush` and `take_error` all `unwrap()` the two mutexes
(`pending`, `failed`), so a panic on either side poisons the other's next lock —
and nothing tells the UI, because `take_error` reads the `failed` cell that only
the dead worker writes. The doc's own reasoning misses it:
`session_save.rs:154`–`159` argues that "blocking *forever* would need the
worker to die **holding the waiter**" — but the waiter's `Sender` is parked in
the queue, so the `recv` never sees a disconnect whether or not the worker died
holding anything; the worker merely dying is enough.

**Evidence.** The probe (a writer with no worker — `Writer::parked`, the shape of
a worker that never ran or died — `save` followed by `flush` on another thread):
`PROBE flush returned within 2s: false`. It never returns. The trigger
(poisoning `pending` inside a live run) was **not** staged; what is proven is the
mechanism, which is the whole of the defect: a writer that is not there hangs the
call that must not hang, with no report.

**Blast radius.** mush freezes inside a flush: `Ctrl-Q`, `Ctrl-C`, `Ctrl-X`,
resize and every keystroke are read by the same thread that is blocked, so the
only way out is a signal — which, on this base, is E1 (the children left
running). The conversation since the last write is lost with it *silently*,
because the one channel that carries a write failure is written only by the
thread that died. The writer thread is also unnamed
(`std::thread::spawn`, `session_save.rs:122`), so the panic message says
`<unnamed>` where every other thread in the process is named (`mush-agent-{id}`,
`mush-job-{id}`) — the "thread name that lies about what panicked" question,
answered here with a name that says nothing.

**Suggested fix.** Three small things in one file: name the worker
(`Builder::new().name("mush-save")`) and handle a spawn failure as a returned
error rather than a panic; keep a `worker_alive` flag cleared by a `Drop` guard
on the thread's stack, so `flush` returns at once when the worker is gone, with
the failure left in `take_error` ("the session writer is gone — the session was
not saved"); and bound `flush`'s `recv` with a deadline, so a wedged
`atomic_write` (an NFS mount, a full disk) cannot freeze the UI for good either.

**Acceptance test.** `a_flush_with_a_dead_writer_returns_with_an_error`: poison
`pending` (or kill a parked writer's thread), call `flush`, assert it returns
within the deadline and that `take_error()` names the writer; plus
`the_writer_thread_is_named` — the panic message, which the job thread's probe
already shows working (`thread 'mush-job-1' … panicked`).

---

## E8 — the lock's one sentence to a human can name a dead pid, and the recorded `lock::tests` flake cannot be a stale lock (minor, proven half)

**What is wrong, half one.** `acquire` takes the flock *first* and writes its
own pid *after* (`lock.rs:61` → `76`–`80`), truncating whatever was there; the
file is never unlinked, so a mush that dies leaves its pid in it. A refusal reads
that text as the holder (`lock.rs:52`–`62`, `holder` at `86`), which the doc
already flags as a hint ("it is never the thing being tested"). Two consequences:
a refusal can name a pid that is not the holder (the microseconds between the
flock and the write, or a stale number from an earlier life), and after any death
the file names a pid that may by now belong to an unrelated live process — while
the sentence the human reads is *"quit it first"*.

**Evidence.** Real binary: mush A holding the workspace → the refusal
`another mush is already running in this workspace (pid 2769932) — quit it first,
or ask it things with `mush agents``; `kill -9 2769932`; `cat .mush/lock` still
answers `2769932` although the workspace is free again (a new start took the
lock and wrote its own 2771423, measured). So the number in the file survives the
process; the only live test is the flock. The stale-but-wrong *live* pid is the
microsecond window, so it stays suspected; the stale content is proven.

**What is wrong, half two — the flake (H28).** The record says two `lock::tests`
were each seen to flake once in a full run, on the assertion "the lock is
takeable again once its holder is dropped", and supposes a stale `/tmp` lock from
a terminated run. My reading and run say that cannot be the mechanism: the
tests' root is
`temp_dir()/mush-lock-{pid}-{label}` and `root()` does `remove_dir_all` first, so
a leftover *file* is gone before the acquire; and a `flock` dies with the fd and
with the process (proved above with a SIGKILLed holder), so a stale lock cannot
outlive anybody. **40 isolated runs of `lock::tests` (four threads, the real
binary's own test harness) were green, and the full suite was green**, which
matches the record's own 40. What *can* make that assert fail without the lock
being wrong is `acquire` erroring before it ever reaches the flock: `open`
failing (`ENOSPC` on a `/tmp` a sibling is filling — this box's `/tmp` is a
7.6 GB tmpfs, 2.5 GB used mid-audit; or `EMFILE`, with a soft limit of 1 024 fds
shared by 725 tests in one process) or `flock` returning something other than
`WouldBlock`. The assertion then blames the lock for the box's state — the same
"loaded suite" class the record names for its other flake.

**Blast radius.** A human told to quit a pid that is not mush: wasted time at
best, a `kill` at an unrelated process at worst. The flake is a test-quality
cost: an assertion that can fail for either of two unrelated reasons, with a
message that names the wrong one.

**Suggested fix.** Write the pid *before* the flock (nothing depends on the
order; the flock is the test), and let the refusal say what it can know: keep the
pid as a hint ("the last holder was pid N — if that process is gone the lock is
free; try again") rather than as a fact. For the tests, assert the *reason*
(`assert!(!error.contains("already running"))`, or print the error) so an
`ENOSPC`/`EMFILE` is read as itself instead of as the lock outliving its holder —
the flock is per open description, so the in-process holder is already a real
test and needs no subprocess.

**Acceptance test.** `a_refusal_does_not_name_a_dead_holder`: a dead pid written
into the lock file, the lock held by another process → the refusal omits the pid
or calls it the last holder; `the_pid_the_refusal_names_is_the_holder` for the
live case (the existing test's positive half); and for the flake,
`the_lock_is_takeable_again_after_its_holder_leaves` asserting the error kind
rather than a bool.

---

## E9 — a handed-over job's age starts at the handover (minor, proven)

**What is wrong.** A `Launch` carries no start time. `Registry::launch` stamps
`started: self.clock.now()` (`jobs.rs:1063`) and hands `let started =
self.clock.now()` to the watch thread (`1088`) at the moment the command *becomes*
a job, so a command that ran for most of `CMD_DETACH_AFTER` as a tool call is
reported as new: `live_for` (`836`) and `status_for` (`1183`) compute the age from
that instant, the completion line's `· 3m12s ·` is measured from it, and so is
the `JOB_MAX_AGE` ceiling. The tool call itself knows the truth (`run_shell`'s
`started`) and nothing passes it on.

**Evidence.** The fake-clock probe: hold a scripted command, advance 59 s (the
tool call's age), hand it over with `Launch::held` —
`PROBE #c1 age after a 59 s tool call: 0ns`.

**Blast radius.** A number three surfaces print (`status`'s headline, the row's
`⚙N <command> 1s`, the completion line) understates how long the machine has been
busy by up to a minute, exactly for the long commands the age exists to describe;
the ceiling is really 4 h + up to 60 s. A human deciding whether to stop a
"1-second" job that has been burning a core for a minute is deciding on a lie.

**Suggested fix.** Stamp the instant where the command is held:
`Registry::hold` already takes `self.clock`, so `Foreground` can keep
`started`; `Launch::held` carries it and `launch` uses it in place of
`self.clock.now()` for both the record and the watch thread. One field, and the
record's own words ("the moment it was handed", `jobs.rs:1085`) become true
without changing any number's meaning for a command that was *started* as a job.

**Acceptance test.** `a_handed_over_job_keeps_the_age_of_its_tool_call`: the
probe above with `age >= 59s` and a completion line whose age is the command's.

---

## E10 — the global panic hook restores the terminal from any thread (minor, proven)

**What is wrong.** `install_panic_hook` (`main.rs:1103`–`1112`) calls
`restore_terminal_modes()` (`main.rs:1073`) for *every* panic in the process —
and mush is a process with a thread per agent (`mush-agent-{id}`), per job
(`mush-job-{id}`) and one for the session writer. A panic in a job's watch thread
or in the writer therefore leaves the alternate screen, disables raw mode and
turns off bracketed paste while the UI thread keeps painting frames: the human's
terminal is smeared into their shell and their keystrokes echo.

**Evidence.** A probe test that installs the hook and panics in a thread named
`mush-job-1`: the test's stdout carried the raw bytes
`[?1049l[?2004l[?1006l[?1015l[?1003l[?1002l[?1000l` — `LeaveAlternateScreen`
and the mode resets — followed by `thread 'mush-job-1' (2761771) panicked`. The
terminal-wide restore ran for a worker's death.

**Blast radius.** The human's screen and their shell's state, from an event
behind their back (a worker died) that the interface never mentions. Recoverable
— quit and start again, or `reset` — but it is the class where mush's own defect
damages something outside mush.

**Suggested fix.** Restore only for the thread that owns the terminal: capture
the UI thread's `ThreadId` where the hook is installed and compare in the hook
(`thread::current().id()`); a worker's panic then gets its own road — the job's
thread kills its group (E4's fix) and the writer marks itself dead (E7's) — and
the human sees neither the escape sequence nor a half-restored screen.

**Acceptance test.** `a_worker_panic_leaves_the_terminal_alone`: with the mode
writes behind a small `Write` seam, a panic on a named worker thread produces no
escape sequences and a panic on the main thread produces all of them.

---

## Verified sound

Each line is what convinced me, not what the prose says.

- **The handover from a tool call to a job has no gap.** `Foreground`'s entry
  lives from `hold` to `drop(held)` in `launch`, and that drop happens *after*
  the job's record is inserted (`jobs.rs:1030`–`1085`), so a `kill_all` in the
  window kills the same `Live` through the foreground map: no moment where a
  running process group is in neither map. Read at the source; the crate's
  `a_handed_over_command_belongs_to_the_agent_that_held_it` pins the ownership
  half.
- **A refused launch kills what it refuses.** Both refusal arms (the lock and
  the budget) call `live.kill()` after the registry lock is dropped
  (`jobs.rs:1071`–`1082`), and `run_command`'s early `has_room` check cannot
  strand a process because admission is re-tested under one lock. Read plus the
  crate's `the_job_budget_is_machine_wide` and
  `a_launch_refused_by_the_lock_is_killed_too`; my probes never saw a refusal
  leak a process.
- **No pipe can deadlock, and a background holder cannot pin a reader.**
  `Shell::spawn` gives the child files (`machine.rs:105`–`106`) and `/dev/null`
  for stdin (`104`); there is no pipe in `machine.rs` at all, and the module's
  reason for files (a pipe is complete only when every holder exits) holds: my
  probes' children kept writing after their reader was gone, with no block.
- **A `Stop` reaches a `run_command` in flight and is reported as a cancel, not
  as the command's own signal.** `kill_owned` kills the held command through the
  same `Live` the watcher polls (`jobs.rs:1152`–`1170`), `wait_bounded`'s poll
  then sees the death, and `ending`'s outside-stop arm turns it into
  `Stopped(Cancelled)` (`agent.rs:5446`–`5458`) — the S4 fix, verified by
  reading the two arms together, and the crate's own
  `a_foreground_command_killed_from_outside_reports_cancelled` (`agent.rs:11927`)
  is the pin.
- **Quitting kills the tree's jobs and a running tool-call command, and the last
  write lands.** `App::drop` (`app/mod.rs:4395`) flushes and then `kill_all`s;
  `main`'s declaration order drops the app before the writer and the lock, so the
  writer's `Drop` (`session_save.rs:169`–`178`) drains the pending snapshot
  *after* the app's flush and joins the worker, and the lock is released last.
  The crate's `quitting_kills_the_jobs_the_agents_started`,
  `quitting_kills_a_running_foreground_command` and
  `quitting_writes_the_messages_the_debounce_had_not` are the pins; the
  drain-on-drop is the one road that makes the "handed over but not yet written"
  snapshot survivable, and it is real (read in `writer`/`drain`: `recv` failing
  still drains, then returns).
- **A save cannot race a mutation.** `SessionSave::save` takes the snapshot by
  value; the UI thread owns the conversation, only the worker serializes it, and
  there is one writer thread, so writes cannot land out of order. Read plus the
  crate's `a_burst_of_handovers_costs_one_write_and_lands_the_newest`.
- **The lock dies with its holder.** Real binary: mush holding the workspace,
  killed with `SIGKILL`, and the very next start acquired the lock (writing its
  own pid over the dead one) — so no stale lock can outlive its process, and the
  refusals I saw always named the live holder. Read: `flock(2)` on the open
  description, `Guard` holds the only fd (`lock.rs:43`).
- **The lock's file is not unlinked, and two workspaces do not see each other.**
  The module's own
  `the_lock_file_is_not_removed_when_the_holder_leaves` and
  `separate_workspaces_have_separate_locks` pass, and my real-binary runs left
  the file in place.
- **The registry answers a poisoned lock the way it claims** (the painter path
  must not panic): the crate's `a_poisoned_registry_still_answers_the_painter`
  passes, and every reader in `jobs.rs` takes a copy out of the lock before
  touching a handle (`jobs()`, `foregrounds()`, `live_for`), which is the lock
  order the watchers rely on. I found no deadlock by reading the four
  acquisition sites.
- **The one rule for "should this command be stopped" is shared.** `stopping()`
  (`jobs.rs:208`–`235`) is called by both watchers, its order is pinned by
  `stopping_names_the_four_ways_a_command_is_killed`, and the runaway writer is
  killed and named by `a_job_that_writes_past_the_limit_is_killed_and_says_so`.
- **The budget is machine-wide and re-tested at the one admission door**, and a
  job never spends an agent's id (`Ids` split; the crate's
  `the_job_budget_is_machine_wide` and
  `a_job_launch_leaves_the_agent_counter_untouched`).
- **`status` is one bounded result and reads the same bytes as the completion
  line.** `STATUS_WINDOW`/`STATUS_COMMAND_COLUMNS`/`JOB_TAIL` are spent through
  one `preview`, pinned by
  `a_status_is_one_bounded_result_however_many_jobs_there_are` and
  `a_long_command_stays_outside_a_status_headline`.
- **The scratch files are 0600.** Measured on the real ones (`-rw-------`),
  which is the one property of E5 that is right; the child can write and nobody
  else can read.
- **In `.mush/` the store is written by rename** (`Session::save` →
  `atomic_write`), so a dead writer cannot leave a torn session — the loss is
  bounded by `SESSION_DEBOUNCE`, as the module says. The socket's guard and a
  stale socket's clearing are B11's verified-sound item; `/tmp`'s litter is E5's.

---

## Blind spots — invariants this area claims with no test behind them

1. **"Idempotent" `kill`** (`machine.rs:83`): the real `Running` has no `killed`
   flag; only the fake does (`machine.rs:434`). Nothing tests a second kill, and
   the fake's own idempotence hides the difference (E6).
2. **`Foreground`'s drop is safe because callers kill first**: no test drops a
   *running* hold, which is the shape a panic runs (E4).
3. **"The process groups are killed on exit"** (the module docs and `docs/mush.md`
   §5.6): every test drives the clean `Drop` through the *fake* machine; no test
   sends mush a signal, and none asserts anything about a real process group
   after a real quit (E1).
4. **The lock file's identity**: nothing tests that the file locked at `acquire`
   is still the file at that path when a second mush arrives (E2); the module's
   tests hold it in one process with nothing touching the name.
5. **The pid in the refusal**: the existing test only asserts that *our own* pid
   (just written by the first acquire) appears — never a stale or foreign one
   (E8).
6. **A job's age is the command's age**: no test hands a command over after time
   has passed, so nothing pins the handover stamp (E9).
7. **The writer's worker cannot die**: nothing poisons `pending`/`failed`, nothing
   asserts the thread's name, and `flush`'s "blocking forever" sentence is
   reasoning with no test (E7).
8. **The scratch file's removal**: no test starts a process that dies without
   unwinding, and no test asserts the file is gone after the ordinary quit either
   (E5).
9. **The panic hook's thread**: no test panics in a worker while the hook is
   installed (E10).
10. **The whole group at the end of a record**: nothing tests a command whose
    leader exits before its children (E3) — the fake's `Script::hangs`/`exits`
    have no group at all.

---

## Not verified

- **A real terminal hangup.** I proved the child survives the parent's
  `SIGTERM`/`SIGHUP` (`kill` to the pid) and that a mush-shaped child outlives
  the process; I did not close a pty master under the real binary, which is the
  road where the kernel's own SIGHUP goes to the session leader. The child is
  outside the foreground process group and has no controlling terminal of its
  own, so the reasoning says it is untouched — I did not measure it.
- **The panic that triggers E4.** No probe drives a panic in a live actor thread
  with a command running; the mechanism (drop without kill, unreachable group,
  lock held) is proven, the trigger is not.
- **The poisoned-`pending` trigger of E7.** I proved a writer with no worker
  hangs the flush; I did not stage a panic inside `drain` to poison the mutex
  that produces it.
- **Forcing `pid` reuse for E6.** `pid_max` on this box is 4 194 304, so I could
  not make the second group kill land on somebody else's group — the probe shows
  the syscall is issued and answered `ESRCH` (`kill ... -> 1`), which is the
  mechanism; the reuse itself is the assumption.
- **A blocked `child.wait()`.** `Running::kill` waits on the leader on whatever
  thread called it — the UI thread on a quit — with no deadline; a child stuck in
  uninterruptible I/O (a dead NFS mount, a stalled device) would hang the quit.
  Staging that needs the filesystem to cooperate.
- **The 4 h ceiling in real time.** `JOB_MAX_AGE` is only driven on the fake
  clock; a real four-hour job was not run.
- **A machine without `kill` on `PATH` as a *real* setup.** I made the shim
  directory; I did not run mush in a slim container where that is the default
  state.
- **Two workspaces that are one store through two mount points** (a bind mount,
  NFS): `flock` is per-inode, and I can only repeat C's answer — the single-path
  case is refused, the aliased case is untested.
- **The clean quit's scratch race.** I did not measure how often the watch
  threads get their last poll in before `main` returns (the exit flush's write
  usually gives them milliseconds); the *certain* case is a death that skips
  `Drop` (E5's probe).
- **Windows/macOS.** `process_group(0)`, `kill -9 -pgid`, `flock` and `/proc`
  readings are unix-only; nothing here was checked on another platform.

---

## Recorded, not changed — checked, and where another audit's fix does not reach

- **H28's flake**, read in full: the record's own account ("the lock is takeable
  again once its holder is dropped", "40 isolated runs green") is right about the
  number and wrong about the presumed cause — a lock file cannot outlive its
  holder (the kernel drops the flock with the fd, proved with a SIGKILLed
  holder), and the tests remove the directory before they start. 40 isolated runs
  and one full suite were green here too. E8's second half names the shapes that
  remain.
- **B3's remedy does not reach E2.** B3 refuses a *shape* (socket/FIFO/device) in
  `write_file`; the lock file is a regular file, so the rename lands and two
  mushes own the workspace. The write road needs a name-level refusal for
  `.mush/lock` (and, for the same class of loss, the session file). This is the
  one place where a fix already proposed in this wave would leave a finding
  standing.
- **B11 and B13 are the socket's and `/tmp`'s other halves**: B11 owns the
  attach socket's bounds (and a stale socket's clearing — which is why E1's
  leftover socket is evidence and not a second finding), B13 owns the paste
  directory. E5 is the third scratch path, `/tmp/mush-cmd-*`, which neither
  audit names.
- **C1's env road on this base, and what stands after its fix.** On this base
  `Shell::spawn` (`machine.rs:96`–`105`) sets no `env_remove`/`env_clear`
  anywhere (`grep -rn "env_remove\|env_clear" crates/` → nothing), so every
  command — and every child of every command — inherits `MUSH_API_KEY`. C1's fix
  (`env_remove("MUSH_API_KEY")` on the command this function builds) is one line
  *inside this same function*, and it changes nothing about any finding here:
  the process group, the scratch files, the kill, the lock and the writer are
  untouched. What it changes is the *orphan's* value: after the fix, the ghost of
  E1/E3/E5 still runs, still holds the group and the ports, and no longer carries
  the key in its environment — one variable less dangerous, no easier to stop.
  Nothing else in this area reads the environment, and the group kill's own
  `PATH` lookup (E6) uses *mush's* environment, not the command's, so a command
  cannot break its own kill by exporting a `PATH`.
- **§8.44–§8.50 touch nothing here.** They are the tool/workspace, wire, meter
  and view rows; the only overlap is S4/H9's "a job dies with its owner", which
  this audit checks and extends (E1–E5).
- **Doc drift** (reported, not edited): `docs/mush.md` §5.6's "its process groups
  are killed on exit" is true only for the clean quit (E1); §5.6's "kill -9 -pgid
  to take the whole group down" is false once the leader has exited (E3);
  `lock.rs:19`–`21`'s "A file that is always there cannot be raced" is false for
  a file that can be *replaced* (E2); `machine.rs:83`'s "Idempotent" (E6);
  `session_save.rs:154`'s "blocking *forever* would need the worker to die
  holding the waiter" (E7 — the worker dying is enough, because the waiter's
  sender is parked in the queue); and `jobs.rs:1085`'s "the moment it was handed"
  is what the age uses, which is E9's point. `App::drop`'s own comment ("Jobs die
  with mush itself … Killing here, on the way out of the process, is the last
  moment it can happen") is exactly true — and it is a `Drop`, which is E1.

## Census of this audit

Ten findings: **0 blocker, 3 major (E1–E3), 7 minor (E4–E10, one suspected)**.
Four are a process that outlives its reason (E1 the signal road, E3 the
backgrounded child, E4 the panic, E5 the scratch's orphan writer); one is the
lock's identity (E2) and one its message (E8); one is a machine-wide signal
fired at a freed id (E6); one is the writer's missing liveness (E7); one is a
number three surfaces print (E9); one is the panic hook's reach (E10). All are
proven by a probe, a `/proc`/`ps` measurement or a run of the real
binary, except E4's trigger, which is marked suspected and whose mechanism is
proven. The probes were: two `#[cfg(test)]` modules holding ten throwaway tests,
a PATH shim directory, the real binary in a pty (`SIGTERM`, `SIGKILL`, the
live-holder refusal, the two-mush lock replacement), a python signal driver, and
40 isolated `lock::tests` runs — all deleted, `git status` clean. The
tree is clean, the suite is green (721 passed · 0 failed), and the only file this
audit changed is this one.
