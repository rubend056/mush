# The runtime, audited: threads, processes, locks, deadlines, and every road that ends

A blind audit of the layer where a defect costs a machine or a conversation
rather than a line: the actor model's parking and waking, the jobs and the
process groups they own, the workspace lock, the exit roads, the session
writer, the attach boundary, and the caches that grow with a session.

**Base:** `e926526` (`merge: the record enters the two waves, their rows, and
the sentences they made false`), my own worktree. Nothing here was fixed; this
file is the only file created or changed, and no probe was left behind. Every
finding below was read out of the code at this base, and the record was asked
about each one first (`grep` over `docs/findings.md` and `docs/audits/*.md` for
the mechanism's keywords — the "Checked" line on each finding says what was
searched). Rows already in the record are cited and dropped, not repeated; the
open queue (H51–H56, H59–H80, A13–A22, C7, F3, F17) was read before writing and
none of these twelve is one of those.

**How.** Five read-only passes over the same base: the actor model
(`agent.rs`, its parking/waking/ending roads and the books the UI and the parent
read), jobs/machine/lock (`jobs.rs`, `machine.rs`, `lock.rs`,
`whole_disk.rs`), every exit road (`signals.rs`, `main.rs`, `attach.rs`,
`App`'s drop and quit roads), the session writer (`session_save.rs`, the store's
write road), and growth/panics/UI-thread blocking (the `Chat` maps, the
mechanical sweep over every production `unwrap`/`expect`/index/`spawn`, and the
subprocess and file calls on the UI thread). Claims about *timing* are labelled
estimates and say what would have to be true; a claim that could only be
imagined is in "Suspect and held", not in the table.

**The verdict in one line.** The ordinary roads are honest: signals take the
quit road, every kill is a process-group kill, the lock cannot be raced by
unlink, the writer cannot lose a hand-over it was given, and the accept loop
survives a transient error. What bites is what the fixes left at the *edges* —
the pane's record is the one copy nothing bounds in *bytes* (R1), a save that
failed is never retried and the exit flush that exists to bound the loss is
skipped for it (R2), and the exit's last acts run in an order that hands the
terminal and the socket back *before* the flush and the kills, so the natural
second signal dies raw with every process group still running (R3–R5).

## The table, ranked

| # | id | severity | What | Cost |
|---|---|---|---|---|
| 1 | R1 | major | The pane's record is the only copy of a message nothing bounds in bytes: it keeps every image payload (≤ 2 MB each) and every pre-shed tool result for the life of the process, and the meter cannot see either | RSS grows with the human's pastes until the box swaps or the OOM killer takes the session and every job with it |
| 2 | R2 | major | A failed session write is dropped, never retried, and it cleared the dirty mark: the exit flush is skipped, so the store silently lags by everything since the last *successful* write | A lost conversation tail — the one-minute bound the module promises is void after any failed write |
| 3 | R3 | latent | The exit restores the terminal and removes the socket **before** the flush and the kills; the second signal (the natural one at a prompt that looks handed back) then dies raw | E1's blast radius: every job's process group survives a "clean" quit, and the missing socket — E1's own proof — says the opposite |
| 4 | R4 | latent | Two waits on the exit road have no bound: the writer's `join`, and `kill_all`'s `child.wait()` | Mush alive after its terminal is back, holding the lock, refusing the workspace to the next mush; the escape is SIGKILL, which runs no cleanup at all |
| 5 | R5 | latent | The quit kills jobs in one walk and never tells the actors to stop, and a launch asks nothing about the quit | A process group born after the walk outlives a clean quit (S4/H9's ghost), in a window exactly as long as R4's |
| 6 | R6 | minor | The save-failure report is raised from the tick and marks no frame owed, so on an idle app it is never painted — and the next keystroke's line replaces it | The human reads a stale workspace as saved; no pane notice and no `/notes` holds it |
| 7 | R7 | minor | `tree.cut_off` arms no `✉` (unlike `finish`/`fail`/`stopped`), so a child that died by panic falls out of `kept`, is reaped, and its `ChildDone` is erased by the reap's own `ForgetChild` before the parent's fold | The one line F6/§8.73 exists to deliver — "#N cut off, nothing committed" — reaches neither the model nor the screen |
| 8 | R8 | latent | `find -- /` slips the whole-disk guard: `--` is read as the expression's first word, so the path loop finds no operand and the guard's fallback looks at the cwd | Minutes of shared-disk load — the exact incident the guard was built for — for one extra token |
| 9 | R9 | latent | The lock-identity guard has exactly one caller: a mush whose lock was replaced still writes `.mush/session.json.previous` (Ctrl-N's copy) into the store another mush now owns | The other window's only copy of its cleared conversation is overwritten, silently, in the window that owns the store |
| 10 | R10 | minor | Every git *mutation* of the worktree sweep runs on the UI thread — `git::reclaim` per landable id inside `Msg::Git`, and the same pass in `App::new` before the first frame | A keystroke asleep for the length of N × several `git` processes (estimate: ~1–8 s for a 70-id residue), and a start with no TUI on screen while it pays |
| 11 | R11 | latent | Four `std::thread::spawn`s on the UI thread (git read, model discovery, clipboard copy, clipboard image read) panic where every other thread in the tree returns a refusal | A box at its thread limit takes the whole TUI down (or freezes the git facts for the session), where A7/E7's own doctrine is a returned error |
| 12 | R12 | minor | The attach surface's answers have no write bound: a client that stops reading parks a connection thread in `write_all`, and 64 of them hold every slot | `mush read/agents/edit` answers `unavailable` for the rest of the session; `Ctrl-Z` on the shipped CLI is enough to be one of the 64 |

The rest — the roads that are real but do not deserve a row at this base — is
the appendix. Then "Suspect and held", then "Known, not re-reported".

---

## 1. R1 · major · the pane keeps what the meter cannot price

`Chat`'s record is a `Vec<Message>` per agent and it is never trimmed:

```rust
// crates/mush/src/app/chat.rs:1131 (the doc above it: "Append a line to an agent's transcript" — nothing trims)
    pub fn push_message(&mut self, agent: AgentId, message: Message) {
...
        let messages = if agent == AgentId::ROOT { &mut self.root } else { self.agents.entry(agent).or_default() };
        let arrived = messages.len();
        if !note { messages.push(message); }
```

The message a paste sends carries the image bytes all the way in:

```rust
// crates/mush-core/src/message.rs:522
    pub fn user_with_images(text: impl Into<String>, images: Vec<Image>) -> Self { Self { images, ..Self::user(text) } }
// crates/mush-core/src/message.rs:211 — `Image { pub path: String, pub mime: String, pub bytes: Vec<u8> }`
// crates/mush-core/src/workspace.rs:54
pub const IMAGE_FILE_CAP: u64 = 2 * 1024 * 1024;
// crates/mush/src/app/mod.rs:2644 (the send road) — the message is pushed into `Chat` *with* the images
        let message = Message::user_with_images(text, images);
        ...
        self.chat.push_message(AgentId::ROOT, message);
```

The one thing that could price them does not, and says why:

```rust
// crates/mush-core/src/message.rs:248
    pub fn weight(&self) -> usize {
        let payload = match self.pixels {
            Some((width, height)) => crate::config::tokens_for_pixels(u64::from(width) * u64::from(height))
                .saturating_mul(crate::config::BYTES_PER_TOKEN),
            None => self.bytes.len(),
        };
```
(`PIXELS_PER_TOKEN = 750`, `BYTES_PER_TOKEN = 3` — a 100×100 PNG weighing 2 MB
weighs 42 bytes.)

The copy that *is* bounded is the one the `Chat` doc calls out by contrast:

```rust
// crates/mush/src/app/chat.rs:1359 (bounded_transcript — what `used_weight_for` weighs and `App::session_snapshot` stores;
// the doc above it, "the pane's own record is deliberately *not* trimmed", is the contrast this finding is about)
        let mut messages: Vec<Message> = self.system_for(id).into_iter().cloned().collect();
        messages.extend(self.transcript(id).iter().cloned());
        let _ = transcript::trim_history(&mut messages, budget);
```
and the actor sheds only its own copy, in so many words:

```rust
// crates/mush/src/agent.rs:2994 — "each one says in its own text what happened to it — in this actor's copy, while the pane keeps the output"
// crates/mush/src/agent.rs:3624 (the tool result, emitted whole to the UI before any shed)
            messages.push(tool_message.clone());
            actor.ctx.emit(actor.id, AgentEvent::Message(tool_message));
```

**Scenario (ordered).** 1. The human pastes screenshots into a long-running
session (the box allows up to `BOX_IMAGE_BYTES` = 8 × 2 MiB per message,
`app/mod.rs:285`). 2. Each send builds a `Message` with the bytes and pushes it
into `Chat::root` — `app/mod.rs:2644` (`user_with_images`), then `:2666`
(steering a busy root), `:2686` (a fresh root run) or `:2733` (a nudge to a
child). 3. The actor's copy is trimmed by weight and can shed its results; the
store's copy is `bounded_transcript`, and `Session::save` throws the image bytes
away (`shed_images`, `mush-core/src/session.rs:413`). 4. The pane's copy is not
trimmed by anything except `Ctrl-N`, `Chat::forget` for a reaped agent, and
`replace_transcript` — `grep -n 'self\.root\.' crates/mush/src/app/chat.rs` is
`push`, `clear`, `replace`, `len`. 5. The session runs for hours.

**Cost.** UI-process RSS grows with the human's pastes and with every tool
result the actor has already shed — a term the file and the `ctx` meter are both
blind to, because the file omits images and the meter prices them by pixels.
Estimate, with the constants above and saying what is assumed: text alone is
≤ `CMD_CAP` = 16 000 bytes per result (say 1 000 results in a 4 000-message
session → ≤ 16 MB); one 2 MB screenshot per ten messages is +800 MB, and a
paste-heavy session reaches GBs. The failure mode is not a wrong number on
screen: it is a swap-thrash and then an OOM kill that takes the conversation,
the jobs and their process groups with it. A8 decided the pane keeps its record
and priced it in *text* bytes (43 902 B, §8.66); the record is also the only
home of the two things whose weight is not their size, and the ruling never
named them.

**Checked:** `grep -rn "pane's copy" docs/` (A8's row and §8.66; priced in text
bytes), `grep -rn "BOX_IMAGE_BYTES" docs/` (the box only), `grep -rn "shed_newest_results" docs/`
(§8.48's byte-cut removal, nothing about the pane's copy).
**Confidence:** proven by reading; the sizes are estimates as labelled.

---

## 2. R2 · major · a failed save is dropped, never retried, and the exit flush is skipped for it

```rust
// crates/mush/src/app/mod.rs:3946 (the debounced save clears the mark before the disk is known to have taken it)
    fn save_session(&mut self) {
        self.session_dirty_at = None;
        let session = self.session_snapshot();
        self.session_save.save(session);
    }
// crates/mush/src/session_save.rs:382 (the worker: the snapshot is consumed by the failed attempt and only a string survives)
                let attempt = session.save(&inner.root);
                ...
                if let Err(error) = attempt {
                    *inner.failed.lock().unwrap() = Some(error.to_string());
                }
// crates/mush-core/src/session.rs:413 — `pub fn save(mut self, root: &Path) -> std::io::Result<()>` (by value: the bytes cannot be re-used)
// crates/mush/src/app/mod.rs:5224 (App::drop)
    fn drop(&mut self) {
        if self.session_dirty_at.is_some() {
            self.flush_session();
        }
        self.tree.handles().jobs.kill_all();
```

**Scenario (ordered).** 1. A run's last message lands at t=0 and marks the
session dirty. 2. t=60: the tick's debounce sets `session_dirty_at = None`,
builds the snapshot and hands it over. 3. The worker's write fails — `ENOSPC`
because a sibling's build filled the disk for a moment, `EDQUOT`, or a mount
that stopped answering (the named bad days). 4. The next tick's poll shows
`could not save session: …` once (R6: possibly without painting it) and takes
the error; nothing sets the dirty mark again. 5. The human reads for ten
minutes, sends nothing. 6. `Ctrl-Q`: `session_dirty_at` is `None`, so
`App::drop` **skips** `flush_session` — the road whose whole purpose is "whatever
the debounce had not written goes out here". The second shape is the same hole:
a `flush_session` that hits `FLUSH_DEADLINE` (10 s) leaves the mark cleared, the
write then fails on the worker, and the exit flush was already skipped.

**Cost.** A lost write. The exposure after one failure is not one minute, it is
everything since the last *successful* write — the snapshot existed only in the
worker's local and was freed, and no `.bak`, temp file or retry keeps it. The
human's one warning was a bar line that fades.

**Checked:** `grep -n "session_dirty_at" docs/findings.md` (one hit, about
`Ctrl-O`); every `retry` hit in the record is the model transport (B23/B25/B27).
E7 bounded the flush's *wait*; this is what happens after it.
**Confidence:** proven by reading (the mark's lifetime, the by-value save, the
poll's take-once, the conditional exit flush).

---

## 3. R3 · latent · the exit hands back the terminal and the socket before it flushes or kills; the second signal dies raw

```rust
// crates/mush/src/main.rs:1092 and :1097 — `app` is declared, then `_attach` after it,
// so reverse declaration order drops the socket first
    let mut app = App::new(workspace, cell, stored, root, tx.clone(), save);
    let _attach = match attach::serve(&attach_root, tx.clone()) { ... };
// crates/mush/src/main.rs:1150
    let result = event_loop(&mut guard.terminal, &mut app, &rx, &theme);
    drop(guard);                       // ← cooked mode, alternate screen gone
    result                             // ← then `_attach`, then `app` (flush ≤ 10 s, then kill_all)
// crates/mush/src/attach.rs:99
impl Drop for Guard { fn drop(&mut self) { let _ = std::fs::remove_file(&self.path); } }
// crates/mush/src/signals.rs:29 (the module's own rule)
//! **The second signal dies at once.** ... the next one restores the signal's
//! default disposition and re-raises it
```

**Scenario (ordered).** 1. A `cargo build` runs as a detached job; the terminal
window is closed (`SIGHUP`), a session manager stops the unit (`SIGTERM`). 2.
The event loop breaks, `drop(guard)` restores the terminal, `_attach` removes
the socket, and `App::drop` starts its flush (bounded at 10 s, `FLUSH_DEADLINE`)
before `kill_all`. 3. The window is at least the flush; on a big session it is
the snapshot rebuild plus a write. 4. The signal flag is already set, so any
further SIGINT/SIGTERM/SIGHUP runs the conditional default and kills mush on the
spot — and the human is exactly the person most likely to send one, because the
screen says mush is gone. 5. `kill_all` never runs.

**Cost.** Every job's process group survives a "clean" quit — E1's blast radius,
the build holding `target/` and the server holding a port — and the socket file
is *already gone*, which is the record's own proof that a clean exit ran
(`signals.rs:21`). The next mush starts beside the ghosts and says nothing. On
the Ctrl-Q road the same keystroke costs only the flag-set (the first signal
after the quit is swallowed; the second kills), so the raw death is specific to
a signalled quit and to a human who sends a second one.

**Checked:** `grep -rn "removes the socket" docs/` (the record *claims* the
opposite order: flush → kill → join → socket, `signals.rs:16–18` and
`audits/processes-and-jobs.md` §E1's suggested fix), `grep -rn -i "drop order"`
(nothing). E1 is closed and is not re-reported: this is the order its fix left.
**Confidence:** proven by reading (language-level drop order, termios and the
registration order in `signals.rs:99–108`); the window's length is proven from
`FLUSH_DEADLINE`, the *likelihood* of a second signal is an estimate.

---

## 4. R4 · latent · two waits on the exit road have no bound

```rust
// crates/mush/src/session_save.rs:333
impl Drop for Writer {
    fn drop(&mut self) {
        self.wake.take();
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
    }
}
// crates/mush/src/machine.rs:306 (inside `Running::kill`, which every kill road ends in — `kill_all` included)
        let _ = self.child.wait();
// crates/mush/src/jobs.rs:1362 (the walk `App::drop` runs)
    fn kill(&self, owner: Option<u64>) {
        for (holder, live) in self.foregrounds() { ... live.kill(); }
        for record in self.jobs() { if ... { if let State::Running(live) = &record.state { live.kill(); } } }
```

`FLUSH_DEADLINE` bounds only the *waiter's* wait; `join` is the same wait with no
deadline, and `child.wait()` reaps a `SIGKILL`ed leader — instant unless the
leader is in uninterruptible I/O.

**Scenario.** The store is on a mount that stops answering (an NFS server gone,
a FUSE mount, an over-committed device — the day E7 itself names). `Ctrl-Q` or a
signal: the terminal is already back (R3), the flush gives up after 10 s, and
the writer's `join` blocks on the same wedged `write(2)`. Mush is alive with the
human's prompt returned, holding the workspace lock; the next `mush` in that
directory is refused with "another mush is already running in this workspace —
quit the running mush first". The escape is the second signal or `SIGKILL`, and
neither runs a `Drop`: R5's window is then unbounded, and the socket file is not
removed. The second shape is `kill_all`: one D-state leader blocks
`child.wait()`, so the *remaining* jobs are never signalled.

**Cost.** An exit that never ends, a lock that refuses the workspace, and an
escalation that converts a graceful stop into the raw death of R3.
Bounded waits — or a teardown order that does the terminal and the socket last —
is the remedy.

**Checked:** `grep -rn "join" docs/findings.md docs/audits/*.md` (the join is
named twice as a sound road, never as unbounded), `grep -rn "child.wait" docs/`
(nothing). E7's row is `flush`'s liveness, not the drop-time join.
**Confidence:** proven by reading for the waits; the blocking day is an estimate.

---

## 5. R5 · latent · the quit kills once and never stops the actors

```rust
// crates/mush/src/app/mod.rs:5224 — App::drop, the whole of it (quoted in full above)
// crates/mush/src/app/mod.rs:3920 — the only thing that sends Shutdown to the tree
    fn stop_all(&self) { for tx in self.tree.agent_tx.values() { let _ = tx.send(AgentMsg::Shutdown); } }
// ... and its only call site is Ctrl-N (`grep -n 'stop_all()'` → app/mod.rs:3714).
// crates/mush/src/jobs.rs:1176 — `launch` asks about the machine and the budget, never about a quit
    pub fn launch(self: &Arc<Self>, launch: Launch) -> Result<JobId, Refused> {
```

`should_quit` is read only by `main.rs` (`grep -rn 'should_quit'` → the event
loop and this field), so nothing between the walk and the process exit refuses
new work.

**Scenario.** The quit's warning names live work, so an actor is mid-run. The UI
thread: flush → `kill_all`'s single walk → the writer's join (R4: normally
microseconds, unbounded on the bad day) → the app and the registry drop. In that
window the still-running actor receives its HTTP answer and calls a tool that
starts a command: `launch` admits it, `sh -c` runs in a fresh process group, the
process exits, and no walk was ever going to see it.

**Cost.** S4/H9's damage — a group outliving a clean quit — with a window that
is usually tiny (estimate) and exactly as long as R4's unwedged wait when it is
not. A launch refusal once the quit is set, or a `stop_all` before the walk,
closes it.

**Checked:** `grep -rn -i "fence|stop_all" docs/findings.md` (S4/H9 cover the
*walk's reach* and the *registration* of foreground commands, not a window after
it), `grep -rn "should_quit" crates/` (the fence the reader would expect is not
there).
**Confidence:** proven by reading for the mechanism; the overlap is an estimate.

---

## 6. R6 · minor · the save-failure line is raised where no frame is owed

```rust
// crates/mush/src/app/mod.rs:1872 (the last thing `tick` does)
        if let Some(error) = self.session_save.take_error() {
            self.fail(format!("could not save session: {error}"));
        }
// crates/mush/src/app/mod.rs:2225 (the one door a bar line goes through — it sets no frame owed)
    fn set_status(&mut self, kind: StatusKind, text: impl Into<String>) {
        self.status = Some(Status { kind, text: mush_core::text::sanitize(&text), set_at: Instant::now() });
    }
// crates/mush/src/main.rs:1212 — a frame is painted only when owed
        app.tick();
        if app.dirty_screen { terminal.draw(...)?; }
```
Every `dirty_screen = true` in `app/mod.rs` is inside `update`, the spinner's
beat, an expiring line or a warning — `grep -n 'dirty_screen = true'` lists none
in `fail`/`set_status`/the error poll. `fail` also does not use the durable route
`fail_for` → `note_error_for` that every other failure takes, so `/notes` never
holds it.

**Scenario.** The debounced save fires 60 s after the last change — by
construction the quiet moment, and if nothing is running there is no spinner
beat. The write fails; the next tick sets the status line and owes no frame. No
frame is painted, so the bar still shows the old content; the failure becomes
visible only when something else owes a frame — and if that something is a
keystroke whose handler says a line (`/help`, a diff, a job report), the error is
replaced before it is ever seen. At exit the same failure goes to a status line
with no screen under it (`App::drop`'s own doc says so), and `main` prints to
stderr only when `run()` returns `Err`.

**Cost.** A lie of omission that feeds R2: the human believes the workspace is
being saved while it is not.

**Checked:** `grep -n "take_error" docs/`, `grep -n "dirty_screen" docs/` (one
hit, `Ctrl-T`), no row about a status raised outside `update`.
**Confidence:** proven by reading (four call sites of the flag dance, and the
idle case is arithmetic, not luck).

---

## 7. R7 · minor · a cut-off child wears no `✉`, so the reap swallows the news

```rust
// crates/mush/src/app/tree.rs:1144 (`finish`), :1158 (`fail`), :1171 (`stopped`) all do this
            let unread = node.parent.is_some();
            node.result_unread = unread;
// crates/mush/src/app/tree.rs:1194 — the ending that does not (and the reason `stopped`'s comment gives applies verbatim)
    pub fn cut_off(&mut self, id: AgentId) {
        if let Some(node) = self.node_mut(id) {
            node.phase = Phase::CutOff;
            node.since = Instant::now();
        }
        self.agent_cancel.remove(&id);
    }
// crates/mush/src/app/tree.rs:1432 — `kept`: the mark is one of the five ways a node survives
            || self.in_flight_with(node, jobs_live) || node.result_unread
// crates/mush/src/app/mod.rs:3839 — the reap queues the forget *behind* the child's own report
            if let Some(tx) = parent.and_then(|parent| self.tree.agent_tx.get(&parent)) {
                let _ = tx.send(AgentMsg::ForgetChild { id: id.0 });
            }
            self.tree.reap(&[id]);
// crates/mush/src/agent.rs:4282 — and the forget erases the completion the parent has not folded yet
fn forget_child(state: &mut ActorState, id: u64) {
    state.children.remove(&id);
    state.completed.remove(&id);
```

**Scenario (ordered).** 1. A shared child (`branch: None`) dies mid-run; the
tree holds ≥ `CHILD_HISTORY` = 50 droppable children. 2. `file_death` emits
`CutOff` to the UI and then tells its parent `ChildDone { CUT_OFF_RUN, CutOff }`
— and the parent is mid-model-call, so that sits in its mailbox. 3. The UI's
`CutOff` arm runs `note_cut_off` → `tree.cut_off`: no `✉`, no branch, not
focused, not in flight — so `kept` is false. 4. The next tick's `reap_history`
sends `ForgetChild` into the same mailbox (FIFO behind the `ChildDone`), reaps
the node and forgets the transcript. 5. At its next boundary the parent drains
both in one pass: `note_completion` records, `forget_child` erases, and
`fold_completions` finds nothing. `is_forgotten` (`!children.contains_key`,
`agent.rs:1295`) makes it permanent — a report that arrives later is dropped by
design.

**Cost.** The line F6/§8.73 exists to deliver — `#N cut off — the run never
ended; nothing was committed` — reaches neither the parent's model nor the
human's screen. The parent may keep waiting or re-spawn; the row and its
transcript are gone. The record's own comment for `stopped` ("still has to be
folded into its parent's transcript … so 'unread' is the truth about it as
well") is the argument `cut_off` is missing.

**Checked:** `grep -rn "result_unread" docs/` (§8.39/A5's actor-side listing,
§8.73's `✉ #7 ⚠ cut off` probe), `grep -rn "cut_off" docs/` with the reap
keywords — no row. H51 owns the park window, H2/F6 own the death's *delivery*,
not this erasure.
**Confidence:** proven by reading, including the FIFO order and the tombstone.

---

## 8. R8 · latent · `find -- /` walks the disk

```rust
// crates/mush-core/src/whole_disk.rs:911 — the leading options, then the path operands
fn find_refuses(args: &[String], cwd: &Cwd) -> bool {
    let mut at = 0;
    while at < args.len() { match args[at].as_str() { "-H" | "-L" | "-P" => at += 1, ... _ => break } }
    ...
    let mut paths: Vec<&str> = Vec::new();
    while let Some(word) = args.get(at).map(String::as_str) {
        if (word.len() > 1 && word.starts_with('-')) || matches!(word, "!" | "(" | ")") { break; }
        paths.push(word); at += 1;
    }
    if paths.is_empty() { return cwd.is_root(); }   // cwd is the workspace root: not refused
// crates/mush-core/src/whole_disk.rs:831 — the *other* reader in the same file does read the marker
        if !literal && word == "--" { literal = true; at += 1; continue; }
```

**Scenario.** A model (or the human) writes `find -- / -name '*.log'` — `--` is
the ordinary "no option can follow" habit. `refusal` reads `--` as the first word
of the expression, finds no path operand, and falls back to "the walker starts
at its cwd", which is the workspace root. GNU `find` reads `--` as
end-of-options and starts at `/` (measured on this box: `find -- /tmp -maxdepth 0`
printed `/tmp`, exit 0).

**Cost.** The guard's whole harm, for one extra token: a whole-disk walk
thrashes a shared disk for minutes. `CMD_OUTPUT_LIMIT` and `JOB_MAX_AGE` bound
what the *job* costs, not what the disk pays. The asymmetry is the proof that
this is an oversight and not a ruling: the same file refuses `ls -R -- /`.

**Checked:** `grep -rn "find --" docs/` (nothing); §8.92's "what it does not
catch" list names `f""ind /`, `"$ROOT"`, `eval`, `xargs`, `-exec`, aliases, build
scripts and deeper nesting — not `--`.
**Confidence:** proven by reading plus the real `find`.

---

## 9. R9 · latent · the lock-identity guard has one caller

```rust
// crates/mush/src/session_save.rs:379 — the only use of `still_mine` in the tree
            if let Some(Err(why)) = inner.lock.as_ref().map(lock::Identity::still_mine) {
                *inner.failed.lock().unwrap() = Some(why);
// crates/mush-core/src/session.rs:115 — Ctrl-N's copy writes with no such check (the `App` never holds an identity)
pub fn keep_previous(root: &Path, mut session: Session) -> Result<PathBuf, String> {
    let to = previous_session_path(root);
    ...
    crate::workspace::atomic_write(&to, &json, crate::workspace::Fresh::Private)
```
(`grep -rn 'still_mine' crates/` → `lock.rs` itself and that one call site;
`App::keep_cleared_conversation` at `app/mod.rs:3760` is the caller, and the
Ctrl-N warning names the file at `app/mod.rs:580`.)

**Scenario (ordered).** 1. Mush A is running and the lock's *name* is replaced —
the road E2's own module docs name (the human's `mv`, a restore from a backup,
`rm -rf .mush/`). 2. Mush B starts, locks the fresh file and owns the store. 3.
A's saves are refused and said once — that half works. 4. In A's window the
human presses Ctrl-N: `keep_cleared_conversation` → `keep_previous` writes the
whole live conversation over `.mush/session.json.previous`, which is **B's**
reclaim slot. 5. B's own Ctrl-N warning ("kept as `.mush/session.json.previous`")
now points at A's conversation; B's copy is gone.

**Cost.** A conversation lost in a different window, silently, contradicting
E2's recorded claim that a displaced mush "stops writing" — which is true of
`session.json` and false of the store.

**Checked:** `grep -rn "keep_previous|keep_cleared" docs/` (nothing),
`grep -n "still_mine" docs/` (E2's row only, and it scopes itself to a session
save).
**Confidence:** proven by reading (one call site, the write road, the warning).

---

## 10. R10 · minor · the sweep's mutation half runs on the UI thread

```rust
// crates/mush/src/app/mod.rs:1378 — inside `adopt_git`, which `Msg::Git`'s arm calls from `update`
                    match git::reclaim(&root, id.0, &base, fork.as_deref()) {
// crates/mush-core/src/git.rs:786 — and `reclaim` re-asks the read the worker already made, then removes
pub fn reclaim(root: &Path, id: u64, base: &str, fork: Option<&str>) -> Reclaimed {
    match reclaimable(root, id, base, fork) { ... Reclaimable::Landable(landing) => remove(root, id, landing) }
// crates/mush-core/src/git.rs:795 — `remove` is `worktree remove --force`, `worktree prune`, `branch -d`
// crates/mush/src/app/mod.rs:897 (App::new, before `TerminalGuard::enter` at main.rs:1143)
        app.reclaim_isolated();
        app.restore_agents(stored_agents);
        app.discover_worktrees();
        app.refresh_git();
```

**Scenario.** A run of isolated children finishes; the human hand-merges; the
next throttled git read (every 2 s while anything is busy) comes back landable
for N nodes and `update` runs N × (`resolve` + `rev-list --count` + `git status
--porcelain --ignored=matching` + `worktree remove` + `prune` + `branch -d`)
before the event loop reads the next key. Every start pays the same per kept id
through `reclaim_isolated` — and the `Kept` arm deliberately keeps a checkout
whose only change is ignored, "a child that merely compiled into `target/`"
(`git.rs:706–717`, its own words), so the `--ignored=matching` scan is the
expensive one. Measured on this box at this checkout (warm cache, no `target/`):
3.0 ms `status --ignored=matching`, 2.3 ms `rev-list`, 2.0 ms `log`, 1.8 ms
`worktree list`; a second read on a checkout that carried a `target/` measured
11–16 ms for the same two. **Estimate:** ~1–8 s for a 70-id residue, and a
start that looks hung with no TUI on screen while it pays.

**Cost.** A keystroke asleep behind subprocesses — the shape D4 was closed for
on the *endpoint* road — and a start that cannot be interrupted by a keypress
because the terminal is not yet mush's.

**Checked:** `grep -rn "sweep_worktrees" docs/` (which worktree is taken: H10,
F1, F7, §8.33), `grep -rn "reclaim_isolated" docs/` (ordering only).
**Confidence:** proven by reading; the totals are estimates with the measured
per-call times above.

---

## 11. R11 · latent · four bare `thread::spawn`s on the UI thread

```rust
// crates/mush/src/app/mod.rs:1289 (the git read: `git_in_flight = true` is set at :1237)
        std::thread::spawn(move || {
// :3338 (model discovery), :4224 (clipboard copy), :4246 (clipboard image read) — the same shape
```
`std::thread::spawn` panics when the OS refuses a thread; every other thread in
the tree uses the `Builder` and handles the refusal, in so many words:

```rust
// crates/mush/src/attach.rs:124 — "a spawn failure is a returned error and not a raised panic" (A7)
// crates/mush/src/jobs.rs:1278–1292 — the spawn's refusal kills the job, forgets it, frees the machine it claimed and returns `Refused::Thread`
// crates/mush/src/session_save.rs:221 — "A thread the OS refuses is *returned*, not raised" (E7)
// crates/mush/src/main.rs:1108 — a failed discovery thread reports `Msg::Models` with no list
```

**Scenario.** The box is at its thread/pid limit (`ulimit -u`, a container's
`pids.max`, a `run_command` that forked widely — the whole-disk guard stops a
disk walk, not a fork storm). The next `tick` while anything is busy, or the
next `Ctrl-V`/`Ctrl-Y`/`/models`, reaches a bare `spawn`; the panic unwinds the
UI thread, the hook restores the terminal, and mush is gone mid-run.

**Cost.** The TUI dies where every comparable road carries on — or, on the
clipboard pair, the worker dies silently and `Ctrl-Y Enter` copies nothing and
says nothing while the `in_flight` flag for git or models stays set for the
session (the git facts freeze with no line saying why). A `Builder` and a status
line is the fix the tree already believes in.

**Checked:** `grep -rn "std::thread::spawn" docs/` (one hit, the *unnamed
writer* sentence E7 closed; the four sites here are named by no row).
**Confidence:** proven by reading for the panic-on-failure contract and the
flags; the reachable trigger is the named resource limit.

---

## 12. R12 · minor · the attach surface answers with no write bound

```rust
// crates/mush/src/attach.rs:321 (the server's connection thread)
fn serve_connection(stream: UnixStream, ui_tx: &Sender<Msg>, idle: Duration) {
    let from = peer_label(&stream);
    if stream.set_read_timeout(Some(idle)).is_err() { return; }
    ...
        if writer.write_all(out.as_bytes()).is_err() || writer.flush().is_err() { return; }
// crates/mush/src/attach.rs:63 — `MAX_CONNECTIONS = 64`, the slot returned by a `Drop` guard on this thread's stack
```
`set_write_timeout` appears in this tree only in `http.rs` and in the client's
`ask`; the server socket gets `SO_RCVTIMEO` and nothing else, and the reply to a
`read` is the whole transcript as one JSON line.

**Scenario.** A client stops reading with a request in flight — the shipped CLI
has both timeouts, but a human's `Ctrl-Z` on `mush read`, a debugger paused on
the client, or any same-user process (B11's own threat model) parks the
connection thread in `write_all`; the idle *read* timeout does not cover writes,
and the client is not idle, it is not reading. 64 such connections hold every
slot, and the slot is only given back when the thread returns.

**Cost.** Every later `mush read/agents/focus/edit` in that workspace is
answered `unavailable: the attach surface already holds 64 connections — retry
when one is free` for the rest of the session, with no way to ask who holds
them. Mush itself keeps working: the surface is dead, not the app.

**Checked:** `grep -rn -i "set_write_timeout|SO_SNDTIMEO" docs/` (R55 and
§8.78's transport rows, both about the model transport); B11's title is "Every
road **into** the socket is bounded" and its fix bounded the three inbound ones.
**Confidence:** proven by reading that no server-side write bound exists; the
buffer arithmetic is an estimate.

---

## Appendix — the rest, one line each

| id | severity | What |
|---|---|---|
| R13 | latent | The attach reader lets a signal close an idle connection: `read_line_capped`'s non-timeout error arm returns `Err` and `serve_connection`'s `Err(_) => return` treats it as gone, so the `Interrupted` a `SIGWINCH` causes (B25's own measurement, whose sweep reached `http.rs` only) drops a persistent client mid-session — the request never runs and the client is told "mush closed the connection without an answer" (`attach.rs:257–263`, `:345–346`). |
| R14 | latent | The socket's mode is the kernel's default (`UnixListener::bind`, no `set_permissions`, no `SO_PEERCRED`), so a umask of `002`/`000` — a shared-group checkout — lets a group member read the conversation and `edit --send` as the human; the module's "a surface a same-user process can reach" is an assumption, not a check (`attach.rs:146–150`). The fact is recorded twice already and left to this boundary (`audits/secrets-session-config.md:32`, `:104`); B11 bounded the roads *in* and never the permissions, so this is the item neither row took. |
| R15 | latent | The CLI's answer read is unbounded: `read_line` on a socket whose only bound is `SO_RCVTIMEO` (silence, not size), so a process that binds a free `.mush/mush.sock` and answers with an endless stream OOMs `mush read` — the same rule B11 applied to the server's request line, missing on the client's reply (`attach.rs:414–424`). |
| R16 | minor | One reap road forgets the tree node but not the `Chat`: `discover_worktrees` calls `tree.reap(&gone)` with no `chat.forget` (the tick's own reap does both), so every leftover reaped that way keeps an id-keyed entry in every `Chat` map for the session — empty today, a whole transcript the day one of them has one (`app/mod.rs:1560–1570` vs `:3848`). |
| R17 | minor | Every snapshot clones image bytes on the UI thread that `Session::save` then throws away: `bounded_transcript` clones each `Message` (images and all, `chat.rs:1359`) and `shed_images` drops them only on the writer's side (`session.rs:413`) — so a save's UI-thread cost includes every kept image's memcpy, and the parked snapshot keeps a second copy alive for the length of the write. |
| R18 | latent | A `.mush/` recreated mid-run has no ignore line: `ensure_mush_dir` writes `.gitignore` only at start, while `keep_previous`/`Session::save` re-create the directory with `create_dir_all` — so after `rm -rf .mush` (or a `git clean -xfd`), Ctrl-N's kept copy sits untracked and the next `git add -A` stages it (C5's residual, `session.rs:80–84` vs `:120` and `:417`). |
| R19 | minor | `/model`, `/provider` and `/url` change what the session stores and never mark it dirty (`app/mod.rs:3628–3638`, the picker arm; `grep -n 'mark_session_dirty()'` lists no picker/URL/provider site), while the session layer outranks the home config on the next start (`config.rs:1190`, `:1219`, `:1236`) — so the human's pick silently reverts in that workspace. |

## Suspect and held

**Held (checked and sound — with the check).**

- **The signal road itself.** The handlers set one `AtomicBool` and nothing else
  (`signals.rs:52–118`, registered through `signal_hook::flag`); the conditional
  default is registered first so the first signal cannot kill raw, the second is
  idempotent, and a failed install unregisters what it took and says so. A
  signal is turned into the quit on the thread that owns the tree, before the
  drain and before a draw (`main.rs:1167`, `:1304`).
- **The lock.** `flock` on a file deliberately never unlinked, opened read+write
  and never truncated by a reader (`lock.rs:127–166`); the pid is a hedge in the
  sentence and the sentence says so (E8); inode reuse cannot fool
  `Identity::still_mine` while mush's own fd keeps the locked inode allocated;
  the model's write road refuses the store's own names.
- **The writer.** The wake channel's capacity-1 token cannot be lost (`save`
  writes the slot before the poke; the worker drains after a `recv` that failed
  too, `session_save.rs:256–345`); one snapshot in the slot, one in flight, no
  older write can land after a newer one; a dead worker answers a flush at once
  (E7's `Alive` guard); the waiter queue holds at most one live waiter and the
  deadline prunes its own.
- **The store's write.** `atomic_write` is same-directory temp + `rename` with
  the target's own mode and `entry_for_write`'s symlink/socket handling
  (`workspace.rs:2030–2058`) — a failed `write_all`/`persist` unlinks the temp
  and leaves the previous file intact, so `ENOSPC` never leaves a truncated file
  where a restore would trust it. (No `fsync` anywhere in the tree: a *power*
  cut can lose the rename and leave the previous save — the loss stays inside
  the documented debounce, and nothing promises durability, so this is held, not
  a finding.)
- **The kill roads.** `Running::kill` signals the group once (`killed` flag,
  E6), `ESRCH` reads as "already gone", a signal that did not land is a stored
  sentence, and `kill_all`'s walk covers both the registry's jobs and the
  foreground command a tool call is holding (S4's fix, `jobs.rs:1362–1376`).
- **The accept loop.** A transient error cannot kill it (`Err(_) => sleep`,
  `attach.rs:190`), the connection count is taken on the accepting thread and
  returned by a `Drop` on the connection thread's stack, and a refused spawn
  returns only that slot.
- **The actor's own books.** One slot per run, released by a `Drop` guard so a
  panic cannot leak it (`LiveGuard`, `agent.rs:1930–1945`); the one-shared-child
  book deletes its empty keys (`WriterGuard::drop`, `:2066–2080`); `actor_main`
  catches an unwinding run and files it as an ending; a lease for a gone id
  changes nothing and a `Spawned` for a gone parent gets `Shutdown` (A4's fix,
  read complete apart from H51, which is the record's).
- **The id spaces.** `LOST_POOL`'s newest-loss rule and the floor arithmetic are
  the doc's (`ids.rs:74–175`), the poison road is `into_inner` by design, and
  the job counter's floor (`reserve_jobs`) is a `fetch_max`.
- **The render path's arithmetic.** `wrap_text_capped` really is capped (so
  `left -= count` cannot underflow), `text::table`'s `width - 3 * (painted - 1)`
  is non-negative because of the `ceil(width/4)` cap, the markdown row buffers
  are one-per-line by construction, and a hostile multi-byte body cannot panic
  `parse_context_hint` (the lowercase search is byte-length-preserving).

**Suspect (imagined; not evidenced, so not findings).**

- **A poisoned `Writers` or `PublishedFacts` book.** Two production books
  lock with `.expect("… while it is poisoned")` (`agent.rs:2066`, `git.rs:906`),
  and `WriterGuard::drop` can run during an unwind — a poison there would panic
  in a `Drop` while already unwinding (abort, no cleanup). The only reachable
  panic under those locks is an allocation failure, which aborts by itself.
- **A job's watch thread has no panic guard and no liveness check.** The spawn
  itself is handled (`jobs.rs:1279` refuses the job if the thread will not
  start), but nothing catches a panic *inside* `watch` (`jobs.rs:1537`), and
  only it clears a job's record and its machine claim; a thread that died would
  leave a job that never ends and a machine lock nobody can clear. I could not
  name a reachable panic in its body.
- **`Live::kill`/`Foreground::tail` swallow a poisoned job handle**
  (`if let Ok(...) = self.job.lock()`), so a kill could be a silent no-op. Same
  trigger as above; no first cause found.
- **`MAX_AGENTS` is a check-then-act.** `spawn_tool` reads the count and the
  slot is taken later, on the child's thread (`agent.rs:4729` vs `:2186`), so a
  batch of spawns can overshoot by the children whose threads have not started.
  Bounded by the parent's own books and by `worktree add`'s latency; H54 owns the
  cap's arithmetic.
- **`WRITERS` is process-wide while `Ids` is per tree.** After Ctrl-N with an old
  tree's shared child still running, the new tree draws the same child id in the
  same directory and the `(dir, id)` key collapses two writers into one set
  entry, so the guard can miss a live one. Needs a shared *grandchild* to matter;
  not staged.
- **A poisoned `pending`/`failed` in the writer would panic inside `App::drop`
  during an unwind** (double panic → abort). The critical sections are moves and
  `String` assignments; no reachable panic found.
- **`reap_dead_scratch` walks the temp directory on the startup thread** before
  the first frame and before the signal handlers are installed (`main.rs:1013`).
  3 200 entries here is a millisecond or two; a shared `/tmp` with 10⁵ entries is
  the bad day, and I have no measurement of it.

## Known, not re-reported

The record holds these and this audit does not repeat them: E1 (killed mush
leaves process groups), E2 (a replaced lock lets two mushes share a store), E3 (a
backgrounded child escapes the registry), E4 (a dropped hold), E5 (scratch
files), E6 (a second kill at a freed pgid), E7 (the flush's deadline — this
audit reports only what its fix left, R2/R4/R6), E8 (the lock's sentence), E9
(a handed-over job's age), E10 (the panic hook), A13/A14/A15/A18/A21/A22, B11
(the inbound socket bounds — R12/R13/R15 are the outbound and the client sides),
C7, F3, F17, H51–H56, H59–H80, §8.102's flakes and §8.104's M5; S4/H9's walk is
cited in R5, A8's pane ruling in R1, D4 in R10, B25 in R13, C5 in R18, and
§8.73/F6 in R7.
