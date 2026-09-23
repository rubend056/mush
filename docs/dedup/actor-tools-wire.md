# Duplication and extractable primitives: the actor, the tools it runs, and the wire

A blind pass over nine production files — `agent.rs`, `http.rs`, `model.rs`,
`jobs.rs`, `machine.rs`, `signals.rs`, `events.rs`, `clock.rs`, `ids.rs` —
about 10,490 lines of file text and **4,865 non-test code lines** (measured by
counting non-blank, non-comment lines in the production region of each file and
subtracting the `#[cfg(test)]`-gated items inside it; the `fake` modules and
the `mod tests` blocks are excluded). Nothing was built, no test was run, no
code was changed.

The counts below are in that same currency: **non-test code lines**, comments
at the sites excluded (where a candidate's saving is mostly documentation — the
wire markers — the doc-inclusive figure is given too). "Net" = site lines
removed − the primitive's own code lines − the call sites' new lines.

The ranked table orders candidates by lines removed per unit of risk, not by
raw lines: two candidates with big raw counts (the delivery books, the wait
loops) sit below smaller ones because their sites are load-bearing rules
(`docs/findings.md` B24) whose drift the tests can only partly catch.

## Ranked table

| # | candidate | sites (`file:line`) | net production lines | correctness risk if left | effort |
|---|---|---|---|---|---|
| 1 | Three hand-rolled wire-failure classes (`Unsent`, `Framing`, `OverlongLine`) | `http.rs:890-922`, `924-959`, `983-1008` | **~26** (48 → 22 code; 95 → ~45 with docs) | medium — the classes decide what may be retried after the request left mush (A2) and what the endpoint "answered" (B27); a fourth class is five edits in one shape, and `model::chat`'s ask *order* is part of the classification | low |
| 2 | The once-only delivery rule, two books: child (id, run) and job (id) | `agent.rs:1119-1143`, `1900-1935`, `3437-3450`, `3620-3630`, `3665-3695`, `4509-4546` | **~30** (25–35) | medium-high — this *is* the B24 rule; `note_parked`'s re-arm (`NO_RUN`) has no job-side twin, and a merged mark that dropped the run would swallow a parked child's next report (§8.21, §8.39) | high |
| 3 | "Answer every call in the batch, and tell the UI" ×4 | `agent.rs:2717-2727`, `2764-2774`, `2798-2805`, `2885-2889` | **~16** (33 → 17) | medium — a road that pushes the tool message without the emit leaves the human's copy without the line; the actor's copy is replaced by the UI's at the next idle `Run`, so the disagreement is permanent (B20) | low |
| 4 | The stdout+stderr join, two renderings | `agent.rs:5600-5612`, `jobs.rs:1590-1603` | **~13** (25 → 12) | medium — `jobs::preview`'s doc claims "exactly as a foreground result reads", and nothing tests the claim; the heading and the trim are kept in step by hand | low |
| 5 | `kill -9 -pgid`, twice | `machine.rs:216-228`, `230-256` | **~8** (12 → 4 + primitive) | medium — a copy that loses `scrub` hands `MUSH_API_KEY` to the `kill` mush runs (C1); one that loses `Stdio::null()` prints into the TUI | low |
| 6 | Who holds the machine, five sentences | `jobs.rs:709-713`, `723-732`, `758-765`, `780-789`; `agent.rs:4166-4170`, `5164-5174` | **~7** | medium — the model reads the same hold described two ways (H13); three of the five shapes are pinned by tests, so an edit must chase them | low |
| 7 | Small arithmetic with two spellings (five items) | `agent.rs:5585` & `jobs.rs:325`; `jobs.rs:1034-1035`, `1173-1180`, `1417-1422`; `agent.rs:5014-5067`; `http.rs:653`, `675`, `1108`; `agent.rs:4334`, `4456` | **~7** (each item ≤ 4) | medium — the ceiling in *hours* is spelled twice and one edit makes a sentence lie; the running-job count has two spellings for the budget and one for the pane | low |
| 8 | The model-failure sentence, translated twice (a turn and a fold) | `agent.rs:2606-2650`, `3120-3140` | **~7** (24 → 17) | medium — four sentences exist verbatim twice; adding the endpoint's name (or a new class) in one path is a one-line edit with no test | low |
| 9 | The bounded wait loop's scaffolding (deadline, poll slice, `wait_tick` match) | `agent.rs:4261-4263`, `4264-4281`, `4333-4334`; `4409`, `4411-4423`, `4455-4456` | **~3** (21 → 18) | low-medium — the two `Duration::from_millis(50)` slices are the only unnamed poll cadence left, and `model.rs`'s `BACKOFF_SLICE` doc already claims their shape | medium |
| 10 | The attempt's budget vs the transport's timeouts | `model.rs:302-350`; `http.rs:26-44`, `489-535`, `695-741`, `760-797` | **0** (a few lines change) | **high** — the deadline is computed twice (once on the actor's clock, once on the system's) and three syscall timeouts ignore the budget they were handed, so "one ask spends one deadline" is false and a Stop can land late | low |

Total estimated saving: **≈ 117 non-test code lines** (range ~100–130; items 1–4
are ~85 of it). For scale, item 1 alone is 0.5% of the nine files' code and the
whole list is 2.4% of it.

## 1. Three hand-rolled wire-failure classes → one

**Sites.** `http.rs:890-922` (`Unsent`), `924-959` (`Framing`), `983-1008`
(`OverlongLine`). Each is the same quartet: a marker struct, a `Display`, an
`Error` impl, a constructor that boxes it into an `io::Error`, and a predicate
that asks the kind of question `ErrorKind` cannot.

```rust
// http.rs:898-922
#[derive(Debug)]
struct Unsent(String);
impl fmt::Display for Unsent { fn fmt(…) { f.write_str(&self.0) } }
impl std::error::Error for Unsent {}
fn unsent(error: io::Error) -> io::Error {
    io::Error::new(error.kind(), Unsent(error.to_string()))
}
pub fn is_unsent(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.downcast_ref::<Unsent>().is_some())
}

// http.rs:936-959 — the same five pieces, message in the marker
#[derive(Debug)]
struct Framing(String);
fn framing(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, Framing(why.into()))
}
pub fn is_framing(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.downcast_ref::<Framing>().is_some())
}

// http.rs:989-1008 — the same again, with no payload at all
#[derive(Debug)]
struct OverlongLine;
fn overlong_line() -> io::Error { io::Error::new(io::ErrorKind::InvalidData, OverlongLine) }
fn is_overlong(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.downcast_ref::<OverlongLine>().is_some())
}
```

**The shared shape.** A class of wire failure carried in the error's source,
asked back by downcast: the class is *not* the `ErrorKind` (that is why it
exists), and which class a failed call is decides whether `model::retrying` may
ask again (finding A2) and whether the endpoint answered (B27).

**The primitive.**

```rust
/// Which class of wire failure this is: the one thing its `ErrorKind` cannot
/// say, and the thing that decides whether a repeat is honest — a request that
/// never left mush may be asked again, a reply that broke on the way in may
/// not (findings A2, B27).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire { Unsent, Framing, Overlong }

/// One [`Wire`] in an `io::Error`, with the words the failing road wrote.
#[derive(Debug)]
struct Marked(Wire, String);

fn marked(kind: ErrorKind, class: Wire, why: impl Into<String>) -> io::Error;
fn wire(error: &io::Error) -> Option<Wire>;

fn unsent(error: io::Error) -> io::Error;                 // keeps its kind
fn framing(why: impl Into<String>) -> io::Error;          // InvalidData
fn overlong_line() -> io::Error;                          // InvalidData, fixed words
pub fn is_unsent(error: &io::Error) -> bool;
pub fn is_framing(error: &io::Error) -> bool;
fn is_overlong(error: &io::Error) -> bool;
```

The three constructors and the three predicates stay (they are the call sites'
vocabulary, and `is_unsent`/`is_framing` are `pub` for `model.rs`); what goes is
the three `Display`/`Error`/struct bodies and the three copies of the downcast.

**What it removes.** 48 code lines at the sites → 22, **net ~26**; counting the
doc comments that describe each class (95 lines) → ~45, net ~50.

**The divergence path.** `model::chat` (`model.rs:168-200`) classifies an
`io::Error` by asking `is_unsent`, then `is_framing`, then `kind() ==
InvalidData` as a `Refused`, then `transport()`. A fourth marker whose
constructor used a kind `transport()` matches (as `Unsent` deliberately does,
keeping its original kind) and whose predicate is not inserted *before* that
arm becomes a final `Transport`: the one retry the whole class exists to allow
is silently lost. Today the ctor's kind and the predicate are two statements of
one fact, kept in agreement by reading both.

## 2. The once-only delivery rule, two books

**Sites.** `agent.rs:1119-1143` (`record_child`/`record_job`), `3437-3450`
(`note_completion`), `3620-3630` (`note_job`), `1900-1935` (`absorb`'s
adoption), `3665-3695` (`fold_completions`), `4509-4546` (`wait_digest`).

```rust
// agent.rs:1119-1142
fn record_child(&mut self, id: u64, run: u64, outcome: Outcome) -> (String, bool) {
    let line = note_completion(self, id, run, outcome);
    if self.is_forgotten(id) || self.delivered.get(&id) == Some(&run) {
        return (line, false);
    }
    self.delivered.insert(id, run);
    (line, true)
}
fn record_job(&mut self, id: JobId, line: String, news: bool) -> Option<String> {
    let line = note_job(self, id, line, news);
    self.delivered_jobs.insert(id).then_some(line)
}

// agent.rs:3437-3450 — the child's book: the mark is a run …
fn note_completion(state: &mut ActorState, id: u64, run: u64, outcome: Outcome) -> String {
    let line = outcome.line(id);
    if state.is_forgotten(id) { return line; }
    if state.completed.get(&id).map(|c| c.run) != Some(run) {
        state.running.remove(&id);
        state.completed.insert(id, Completion { run, outcome });
    }
    line
}
// agent.rs:3620-3630 — the job's book: the mark is a bare id
fn note_job(state: &mut ActorState, id: JobId, line: String, news: bool) -> String {
    state.running_jobs.remove(&id);
    state.done_jobs.insert(id, JobReport { line: line.clone(), news });
    line
}
```

```rust
// agent.rs:3665-3695 — two snapshots, two folds, one rule
let jobs: Vec<(JobId, String, bool)> = state.done_jobs.iter()
    .filter(|(job, _)| !state.delivered_jobs.contains(job))
    .map(|(job, report)| (*job, report.line.clone(), report.news)).collect();
let mut news = false;
for (job, line, job_news) in jobs {
    if let Some(line) = state.record_job(job, line, job_news) { push_line(actor, messages, line); }
    news |= job_news;
}
let children: Vec<(u64, u64, Outcome)> = state.completed.iter()
    .filter(|(child, completion)| state.delivered.get(*child) != Some(&completion.run))
    .map(|(child, completion)| (*child, completion.run, completion.outcome.clone())).collect();
for (child, run, outcome) in children {
    let (line, fresh) = state.record_child(child, run, outcome);
    if fresh { push_line(actor, messages, line); actor.ctx.emit(actor.id, AgentEvent::ResultRead { child }); }
    news = true;
}
```

`absorb` (`1900-1935`) and `wait_digest` (`4511-4545`) repeat the twins: a
filtered snapshot over `completed` + `delivered`, then the same for `done_jobs`
+ `delivered_jobs`, then the same `record_*` once-only call.

**The shared shape.** Three books — running, recorded, delivered — and one rule
stated twice in prose and twice in code: a result is folded once, a later run
is news, the same run twice is not (`docs/findings.md` B24, H15). The
`ActorState` docs for the job half say outright that it is the child half's
shape ("The same three books as `running`/`completed`/`delivered` above").

**The primitive.**

```rust
/// The books one result travels — what is running, what was recorded, and the
/// run whose line the model has read — because a child's summary and a job's
/// line obey one rule: folded once, never twice, and a later run is news
/// (findings B24, H15). `K` is the id's own type, so a [`JobId`] can never be
/// filed against a child (`crate::ids`, finding B1); a job's run is the one
/// constant run a job has.
struct Delivery<K, R> {
    running: HashSet<K>,
    recorded: HashMap<K, R>,
    /// The run whose line was delivered, not a bare flag: a woken child's
    /// counter starts over, and a mark that cannot say "which run" would
    /// swallow its next report (`note_parked`, §8.21).
    delivered: HashMap<K, u64>,
}

impl<K: Copy + Eq + Hash, R> Delivery<K, R> {
    /// Record one result and answer whether the model has not read it, marking
    /// it read as it says so.
    fn record_once(&mut self, key: K, run: u64, payload: R) -> bool;
    /// Whether a later report of this key would be news by run alone.
    fn unread(&self, key: K) -> bool;
    /// Supersede the recorded run without touching the read mark.
    fn supersede(&mut self, key: K, run: u64);
}
```

The two payload types (`Completion { run, outcome }`, `JobReport { line, news
}`) stay where they are; what merges is the mechanism around them, and the two
readers' snapshot loops become one iterator each per book rather than a
hand-written filter with a copied predicate.

**What it removes.** ~120 code lines carry the rule twice (9 + 5 for
`record_*`, 13 + 6 for `note_*`, 25 + 22 + 32 in the three twin readers, ~8 in
the two drains); a `Delivery` owner lands near 85–90, **net ~30, range 25–35**.
The honest ceiling is set by the snapshot loops, whose payloads and answers
differ enough that only the filter and the mark can move.

**The divergence path.** `note_parked` (`agent.rs:3575-3598`) moves a parked
child's `completed.run` *and* `delivered` to `NO_RUN` so the first run after a
wake takes a number the books have not read — the fix for a result the parent's
model never got (§8.21). A job has no such move (a job cannot be parked). Merge
the books into one bare `delivered: HashSet<K>` and the parked child's next
ending reads as already delivered; the model never sees the result, and the row
clears its `✉` as if it had. Keep the run and a job's mark can be expressed as a
constant run — but the merge must *say* that, and `note_completion`'s "only a
changed run clears `running`" against `note_job`'s "always clear it" is one more
rule the owner would have to state once.

## 3. "Answer every call in the batch, and tell the UI" ×4

**Sites.** `agent.rs:2717-2727` (a reply cut off at the token cap),
`2764-2774` (a reply the endpoint refused), `2798-2805` (the run stopped as a
loop), `2885-2889` (a cancellation between calls).

```rust
// agent.rs:2717-2727                                // agent.rs:2885-2889
for call in &tool_calls {                            for skipped in &tool_calls[index..] {
    let message = Message::tool(                         let message = Message::tool(skipped.id.clone(),
        call.id.clone(),                                     format!("error: {CANCELLED}"));
        format!(                                      messages.push(message.clone());
            "error: the model's reply was cut off…",  actor.ctx.emit(actor.id, AgentEvent::Message(message));
            cfg.reply_cap()                           }
        ),
    );
    messages.push(message.clone());
    actor.ctx.emit(actor.id, AgentEvent::Message(message));
}
```

The refusal road (`2764-2774`) and the loop road (`2798-2805`) are the same
five lines with another sentence; the loop road is the four-line version.

**The shared shape.** The transcript copy and the UI copy are one fact with two
readers — the rule `push_line` (`agent.rs:3640-3644`) already states and owns
for user lines: "A line that reaches `messages` alone is a line the human cannot
see (finding B20)".

**The primitive.**

```rust
/// Answer every call in a batch, in the transcript and onscreen: the calls that
/// never ran still get their tool message, or the conversation keeps a dangling
/// call — and the line reaches the human's copy through the same door, because
/// a line that reaches `messages` alone is one the human cannot see (B20).
fn answer_calls(actor: &Actor, messages: &mut Vec<Message>, calls: &[ToolCall], why: &str) {
    for call in calls {
        let message = Message::tool(call.id.clone(), format!("error: {why}"));
        messages.push(message.clone());
        actor.ctx.emit(actor.id, AgentEvent::Message(message));
    }
}
```

**What it removes.** 33 site code lines → 10, plus the 7-line primitive,
**net ~16**. The multi-line `format!`s stay at their call sites; only the
scaffolding goes.

**The divergence path.** A fifth road of the same kind — a `Stop` that lands
between the model's reply and the batch, say — is written by copying one of the
four; forget the `emit` and the human's copy of the transcript is missing the
error line for calls the model was told about. The next idle `Run` replaces the
actor's transcript with the UI's, so the line is gone for good, not just late.

## 4. The stdout+stderr join, two renderings

**Sites.** `agent.rs:5600-5612` (`command_report`), `jobs.rs:1590-1603`
(`preview`).

```rust
// agent.rs:5600-5612                                // jobs.rs:1590-1603
fn command_report(stdout: &str, stderr: &str) -> String {   fn preview(stdout: &str, stderr: &str, cap: usize) -> String {
    let mut report = String::new();                             let mut out = String::new();
    if !stdout.trim().is_empty() {                              if !stdout.trim().is_empty() {
        report.push_str(stdout.trim_end());                         out.push_str(stdout.trim_end());
        report.push('\n');                                      }
    }                                                           if !stderr.trim().is_empty() {
    if !stderr.trim().is_empty() {                                  if !out.is_empty() { out.push('\n'); }
        report.push_str("--- stderr ---\n");                        out.push_str("--- stderr ---\n");
        report.push_str(stderr.trim_end());                         out.push_str(stderr.trim_end());
        report.push('\n');                                      }
    }                                                           tail_for_model(&out, cap)
    report                                                  }
}
```

**The shared shape.** "stdout, then stderr under a heading" — the heading is
spelled twice, the emptiness tests are spelled twice, and the only difference is
that the tool report closes with a newline (so the `[exit 0]` note lands on its
own line) while the job's window goes to `tail_for_model`.

**The primitive.**

```rust
/// One command's two streams, joined the one way a result reads: the stdout,
/// then the stderr under `--- stderr ---`, so a job's window and a foreground
/// result describe the same bytes however the command ended and whoever asks.
fn streams_window(stdout: &str, stderr: &str) -> String;
```

It belongs beside the `Job` trait in `machine.rs`, which both modules already
import from. `command_report` stays as a two-line wrapper (join + trailing
newline); `preview` becomes `tail_for_model(&streams_window(stdout, stderr),
cap)`.

**What it removes.** 25 site code lines → 12, **net ~13**.

**The divergence path, and a second question under it.** `preview`'s doc asserts
the job window reads "exactly as a foreground result reads", and nothing tests
the assertion; a change to the heading (or to one road's `trim_end`) makes the
same command's output read two ways to the *same* reader — the model, whose tool
result and job completion line are both in its transcript. And the cap under
the join is applied at two levels with one name: `machine::Job::output`/`tail`
truncate *each stream* to `cap` (`machine.rs:78-91`, and the real impl at
`machine.rs:208-214`), while `preview` truncates the *joined* text to `cap`. A command that
writes `cap` bytes to each stream gives the model up to `2 × cap` in a
foreground result and `cap` as a job. Unifying the join owner is the place to
decide that (moving the cap into the join would shrink foreground results — a
behaviour change for the human to call, not a blind edit).

## 5. `kill -9 -pgid`, twice

**Sites.** `machine.rs:216-228` (`Running::kill`), `230-256`
(`Running::end_group`).

```rust
// machine.rs:221-227                                 // machine.rs:245-251
let _ = scrub(&mut Command::new("kill"))              let killed = scrub(&mut Command::new("kill"))
    .args(["-9", &format!("-{group}")])                   .args(["-9", &format!("-{group}")])
    .stdout(Stdio::null())                                .stdout(Stdio::null())
    .stderr(Stdio::null())                                .stderr(Stdio::null())
    .status();                                            .status();
```

**The shared shape.** One signal, one target, one rule that the child is
started through `scrub` (finding C1) and that mush's own streams stay quiet;
the two sites differ only in whether the answer is read.

**The primitive.**

```rust
/// Kill a process group mush made, and only that group: `kill -9 -pgid`, run
/// with mush's secrets out of the environment (finding C1) and nothing of its
/// own on mush's streams. The answer is the caller's: `kill` ignores it, a
/// group's end reports it.
fn kill_group(group: u32) -> io::Result<ExitStatus>;
```

**What it removes.** 12 site code lines → 4, **net ~8** (the response match in
`end_group` stays).

**The divergence path.** The two blocks are the same eleven tokens four times
over. Drop `scrub` from one and the `kill` mush runs — a child of mush, in the
group the credential lives in — inherits `MUSH_API_KEY`, which is exactly the
leak `Shell::spawn` closes; drop `Stdio::null()` and a `kill` complaint lands
in the TUI's own output. Neither is caught by anything but reading the second
copy.

## 6. Who holds the machine, five sentences

**Sites.** `jobs.rs:709-713` (the holder's own second claim), `723-732` (a
sibling that queued), `758-765` (the root), `780-789` (a sibling that never
queued); `agent.rs:4166-4170` (`machine_holding`, used by four wait sentences),
`agent.rs:5164-5174` (`beside_note`, the root running beside a hold).

```rust
// jobs.rs:728-731                                  // agent.rs:4166-4170
"#{} holds the machine with an exclusive command ({}); this call queued \
 and the lock was still held. {}"                    "#{}'s exclusive command ({})"
// jobs.rs:761-764 (root)   jobs.rs:785-788 (unqueued)   // agent.rs:5168 (beside)
"#{} holds the machine with an exclusive command ({}); you are the root —…"
"#{} holds the machine with an exclusive command ({}); this call was refused…"
"{text}\n(ran while #{} held the machine for an exclusive command ({}) —…"
```

All five then cut the command with `truncate(&held.command,
REFUSAL_COMMAND_COLUMNS)` (six sites of that pair, counting the four in
`jobs.rs`).

**The shared shape.** One fact — who holds the machine and what they are
running — with a different continuation per road. `Refused::lock_road` already
owns the shared *road back*; the shared *clause* is the part still copied.

**The primitive.**

```rust
impl Held {
    /// Who holds the machine and what they are running, in one spelling: a
    /// refusal, a wait's note, a root's aside and a beside-note all name the
    /// same hold, and a model must not read two accounts of it (finding H13).
    fn phrase(&self) -> String;   // "#2 holds the machine with an exclusive command (cargo bench)"
    fn named(&self) -> String;    // "#2's exclusive command (cargo bench)"
}
```

**What it removes.** ~20 site lines of clause-and-args → ~8 plus a 6-line
owner, **net ~7**. Small, but it is the sentence the H13 road is made of.

**The divergence path.** The clause is 60-column-cut in six places; a changed
bound or a changed `#` spelling applied to four of five leaves the model with
two descriptions of one hold in the same conversation — and three of the five
shapes are pinned by tests (`agent.rs:11817`, `11851`, `12151`), so the edit
would have to chase them.

## 7. Small arithmetic with two spellings

Five one-liners, none above the 5-line bar alone; together **net ~7**, and two
of them can drift into a false sentence.

1. **The ceiling in hours.** `agent.rs:5585` (`end_note`'s
   `jobs::JOB_MAX_AGE.as_secs() / 3600`) and `jobs.rs:325` (`JobOutcome::line`'s
   `JOB_MAX_AGE.as_secs() / 3600`). One change to `JOB_MAX_AGE` and one of the
   two sentences lies about the ceiling it names. Owner:
   `pub const JOB_MAX_AGE_HOURS: u64 = JOB_MAX_AGE.as_secs() / 3600;` — **net 0
   lines**, drift insurance. (`jobs.rs:325` is in a `format!` argument list, so
   the const is the whole fix.)
2. **How many jobs are alive.** `jobs.rs:1034-1035` (`running()`:
   `self.jobs().iter().filter(|record| record.running()).count()`),
   `1173-1180` (`launch`'s inline
   `inner.jobs.values().filter(|r| r.running()).count() >= MAX_JOBS`), and the
   negated twin at `1417-1422` (`finish`'s `!record.running()` count and
   `find`). The first is the pane's count, the second the budget, the third the
   history window — one question ("alive") in three spellings. Owner:
   `impl Inner { fn running(&self) -> usize; fn ended(&self) -> usize; }` —
   **net ~0 lines**; the drift is that a record state not counted as running in
   one spelling makes the budget and the screen disagree.
3. **The bounded-listing tail.** `agent.rs:5014-5029` (`list_tool`'s
   `join` + `[mush: the first N files …]` + `truncate_for_model(out,
   result_cap(..))`) and `5030-5067` (`search_tool`'s same three steps with a
   note list). Owner: `fn bounded_listing(lines: Vec<String>, notes: Vec<String>,
   cap: usize) -> String` — **net ~4**; drift is a listing that forgets its
   truncation note and reads as complete.
4. **The body cap, three times.** `http.rs:653` (`read_exact`'s
   `len > MAX_BODY_BYTES`), `675` (`read_to_end`'s running check), `1108`
   (`read_chunked`'s `size > MAX_BODY_BYTES - out.len()`). Owner:
   `fn body_room(written: usize, more: usize) -> io::Result<()>` — **net ~4**;
   drift is one road that can be made to allocate past the cap.
5. **The wait's poll slice.** `agent.rs:4334` and `4456`, both
   `clock.sleep(Duration::from_millis(50))` — the only unnamed poll cadence in
   the tree: `jobs::POLL` (10 ms), `LOCK_POLL` (200 ms), `READ_SLICE` (200 ms),
   `RESOLVE_SLICE` (50 ms) and `BACKOFF_SLICE` (50 ms) are all named, and
   `model.rs:255-261`'s `BACKOFF_SLICE` doc already says it is "the same shape
   as the poll in `agent.rs`'s waits". Owner: one `pub(crate) const WAIT_POLL:
   Duration` (clock.rs is the natural home) used by both waits and by
   `BACKOFF_SLICE` — **net ~0 lines**; drift is `wait` and `wait({ on })`
   polling at two cadences.

## 8. The model-failure sentence, translated twice

**Sites.** `agent.rs:2606-2650` (the run's turn) and `3120-3140` (the fold's
summarize call).

```rust
// agent.rs:2620-2621 (turn)                         // agent.rs:3134 (fold)
Err(ModelError::Framing(error)) => {                  Err(ModelError::Framing(error)) =>
    return Err(reply_broke(&cfg.base_url, &error));       return Err(reply_broke(&cfg.base_url, &error)),
// agent.rs:2628-2632                                // agent.rs:3127-3131
Err(ModelError::Unreachable(error))                   Err(ModelError::Unreachable(error))
| Err(ModelError::Unsent(error))                      | Err(ModelError::Unsent(error))
| Err(ModelError::Transport(error)) =>                 | Err(ModelError::Transport(error)) =>
    return Err(format!("cannot reach {}: {error}",        return Err(format!("cannot reach {}: {error}",
        cfg.base_url)),                                       cfg.base_url)),
```

`Refused` ("the endpoint's reply was refused: …") and `Encode` ("could not
encode request: …") are verbatim in both; `Cancelled` maps to `CANCELLED` in
both. What differs stays: the turn *learns* a window from a `Status` and
retries once (A3, A2), and the fold's `Status`/`Malformed` answer is only sent
when the human asked for the fold (U11).

**The shared shape.** One client's failure classes, translated into the run's
own words twice — the fold's comment says why ("compaction is a model call like
any other").

**The primitive.**

```rust
/// The sentence a *transport* failure gets, in the run's own words: a request
/// that never left, one that may already be answered, bytes that never framed
/// themselves, an endpoint mush cannot reach — one failure of one client, so
/// one spelling (findings A2, B27). `None` for the classes whose answer is the
/// caller's: a cancellation, a status, a body that did not parse.
fn transport_line(cfg: &Config, error: &ModelError) -> Option<String>;
```

**What it removes.** 24 duplicated site lines (13 in the turn, 11 in the
fold) → a 12-line owner plus five lines of call-site restructure, **net ~7**.
Below the line-count bar; on the list because four sentences exist twice with
no test tying them together.

**The divergence path.** Add the endpoint's name to the run's unreachable
sentence (or a fifth class to `ModelError`) and edit the fold's copy only if you
notice it; a `/compact` then reports a failure in different words from the run
it belongs to — and the *classification* that A2's fix is made of has two
readers instead of one.

## 9. The bounded wait loop's scaffolding

**Sites.** `agent.rs:4261-4263` + `4264-4281` + `4333-4334` (`wait_tool`) and
`4409` + `4411-4423` + `4455-4456` (`wait_on_tool`).

```rust
// agent.rs:4261-4267 (bare wait)                    // agent.rs:4409-4423 (targeted wait)
let clock = actor.ctx.clock.as_ref();                let clock = actor.ctx.clock.as_ref();
let deadline = clock.now() + Duration::from_secs(WAIT_TIMEOUT_SECS);
loop {                                               loop {
    match wait_tick(actor, state, cancel, |state| {      match wait_tick(actor, state, cancel, |state| {
        …                       // what it waits for         if target.running(state) { … } else { String::new() }
    }) {                                                 }) {
        Tick::Go => {}                                       Tick::Go => {}
        Tick::Cancelled => return Err(CANCELLED.to_string()),
        Tick::Answer(answer) => return Ok(answer),           Tick::Cancelled => return Err(CANCELLED.to_string()),
    }                                                        Tick::Answer(answer) => return Ok(answer),
    …   // decide, answer                                    }
    state.waited = true;                                  …
    clock.sleep(Duration::from_millis(50));               state.waited = true;
}                                                          clock.sleep(Duration::from_millis(50));
                                                       }
```

**The shared shape.** One deadline on the run's clock, one `wait_tick` handling
of the two things that outrank the wait, and one `state.waited` + sleep — the
"it slept, so the loop guard must not read the next identical call as a repeat"
bookkeeping (H13). The decisions between the ticks differ and must stay
different: the bare wait owns the machine road, the targeted one owns the
target's answer.

**The primitive.**

```rust
/// A bounded wait's own arithmetic: one deadline on the run's clock, and the
/// poll slice every wait sleeps in, so `wait` and `wait({ on })` spend time the
/// one way (finding H15) — what they wait *for* stays theirs.
struct Wait<'a> { clock: &'a dyn Clock, deadline: Instant }

impl<'a> Wait<'a> {
    fn new(actor: &Actor) -> Self;
    fn expired(&self) -> bool;
    /// Handle one tick's verdict, or record the sleep it took.
    fn round<T>(&self, verdict: Tick, …) -> Result<Option<String>, String>;
}
```

**What it removes.** 21 shared scaffolding lines → a ~14-line owner and ~4
lines of call sites, **net ~3**. Below the
bar for lines; on the list because the two `Duration::from_millis(50)` literals
(item 7.5) are the only unnamed cadence and the deadline is built twice.

**The divergence path.** Change one sleep to 100 ms and the two waits poll at
two rates; the tree's own doc claims they share a shape, and no test measures
either slice (the fake clock advances by whatever it is handed).

## 10. The attempt's budget vs the transport's timeouts (correctness, net 0)

**Sites.** `model.rs:302-350` (`retrying`: `deadline = clock.now() + timeout`,
`left = deadline.saturating_duration_since(clock.now())`, each attempt handed
`left`); `http.rs:489-535` (`Watch::new` computes a *second* deadline,
`clock.now() + timeout`, on `clock::system()`); `http.rs:26-44`
(`CONNECT_TIMEOUT = 5s`, `WRITE_TIMEOUT = 30s`); `http.rs:695-741` (`connect`:
one `connect_timeout` per address, `set_write_timeout(Some(WRITE_TIMEOUT))`
regardless of the budget); `http.rs:760-797` (`resolve_bounded`:
`RESOLVE_TIMEOUT = 10s` on `clock::system()`, not on the budget it was called
with).

**The shape that is not one.** `ModelClient::chat`'s contract (`model.rs:128-146`)
says "an attempt must bound everything it does by what it was handed", and
`retrying`'s doc says a call "can never spend two" deadlines. The reader honours
it (the watch's deadline is the handed `left`, the read slice is short), but
three bounds do not:

* `write_all` blocks up to `WRITE_TIMEOUT` (30 s) whatever `left` is — and the
  watch is only consulted on `EINTR` or on a read slice, so a `Stop` that lands
  during that write is not read until the write returns;
* `connect_timeout` is 5 s **per address**, so a name with two addresses can
  spend 10 s;
* `resolve_bounded`'s 10 s is on the system clock, and `retrying`'s stale
  comment still says "resolving a host has no timeout".

Concrete path: an endpoint that accepts the TCP connection and then stops
reading. One attempt with 1 s of budget left can hold the actor's thread ~30 s
past its deadline, the `Stop` the human pressed waits with it, and the "one ask
spends one deadline" claim in the docs is false by up to ~45 s.

**The primitive.** Not a struct — a rule with one spelling:

```rust
/// What a syscall's own timeout may be: never more than what is left of the
/// call's deadline, or a transport can outlive the budget it was handed
/// (finding A2's "one call, one deadline", finding A19 for the resolver).
fn bounded(ceiling: Duration, left: Duration) -> Duration { ceiling.min(left) }
```

...threaded into `connect` (`CONNECT_TIMEOUT.min(left)`,
`WRITE_TIMEOUT.min(left)`) and `resolve_bounded` (which should take `left`, not
its own const alone). **Net ~0 lines**; it is on the list because it is the one
place in these nine files where a duplicated arithmetic is *already* diverging.

## Looks duplicated but is not

* **`Watch`'s deadline on `clock::system()` vs `retrying`'s on the actor's
  clock.** Deliberate, and `clock.rs:33-37` names http as one of the two places
  not handed a clock: the socket's own read timeout is the socket's, not a
  policy. A fake clock at the socket would turn a test seam into a transport
  setting.
* **`Scratch::read` (head, `cap + 1`, `truncate_for_model`) vs
  `Scratch::read_tail` (seek from the end, `tail_for_model`)** — `machine.rs:340-363`.
  A foreground result is a head read while the command still runs; a job's kept
  window is a tail (`jobs.rs`'s rule 1: the end is where `test result: FAILED`
  lives). One reader would flip one of the two.
* **The cap at the `Job` seam vs at the registry seam.** `Job::output`/`tail`
  say "per stream"; `jobs::preview` re-caps the joined window. Two contracts
  with the same parameter name — flagged under candidate 4 as a real ambiguity,
  not merged here because the fix changes what the model may read.
* **`JobOutcome::line` (`jobs.rs:294-360`) vs `end_note`
  (`agent.rs:5547-5594`).** The human's completion line and the model's
  bracketed note: different readers and registers. Their *reasons* are already
  shared through `jobs::Stopped` (refactor R15 landed), which is why only the
  hours arithmetic (7.1) is duplicated between them.
* **`Outcome::line`, `digest`, `is_news`, `Committed::from`** — four projections
  of one enum, each for a different surface (a parent's line, a listing's
  digest, a wake decision, a git subject). One table would couple the commit
  subject to the screen's marks.
* **`STATUS_COMMAND_COLUMNS`, `REFUSAL_COMMAND_COLUMNS`, `SUBJECT_COLUMNS`**
  (60/60/60). The code states both reasons: a headline keeps `STATUS_WINDOW`
  bounded, a refusal keeps itself to one line; a git subject is a git subject.
* **`JOB_MAX_AGE` (4 h), `CHAT_DEADLINE` (600 s), `WAIT_TIMEOUT_SECS` (600 s),
  `LOCK_QUEUE` (30 s)** — four questions that happen to share numbers: how long
  a job may hold a slot, how long one call may take, how long a wait may block,
  how long a sibling may queue. One const for two of them would be a bug.
* **`MAX_JOBS` checked in `has_room` (`jobs.rs:1051-1053`) and again in
  `launch` under the lock (`1173-1180`).** Two *moments* of one question, and
  `has_room`'s own doc says why the second cannot be dropped (another agent's
  job can take the slot in between). Only the *counting* is duplicated — item
  7.2 — not the check.
* **`wait_bounded` passing `None` for the ceiling (`agent.rs:5679-5690`) and
  `watch` passing `None` for the limit (`jobs.rs:1542-1551`)** through the same
  `jobs::stopping`: each watcher asks half the question because the other half
  is structurally unreachable for it (a foreground command detaches before it
  can reach a ceiling; a job has no tool-call timeout). Documented at
  `jobs.rs:208-228`.
* **`jobs.rs`'s `group_members(pgid: i32)` (test-only, `jobs.rs:1609-1633`) vs
  `machine::group_members(pgid: u32)` (`machine.rs:273-302`).** Two `/proc` readers, one
  for the watch and one for the tests; the test copy is allowed to be cruder
  (it even parses with `rsplit(')')` rather than `rsplit_once`). Test code, out
  of the count.
* **The per-tool boilerplate itself.** The shape the brief expects to repeat at
  eight call sites is mostly already owned in this tree: the schemas live in
  `mush_core::prompt::tool_schemas` (outside these files), the argument parsing
  in `mush_core::tools::arg_*` (19 call sites in `agent.rs`, each one line), the
  dispatch in `exec_tool` (`agent.rs:3780-3818`, 33 code lines for ten tools),
  the result cap in `result_cap` (`agent.rs:5393-5399`, used by `read_file`,
  `run_shell`, `list_files`, `search`), the result shape in `ToolOutput` and its
  conversions, and the unknown-tool refusal in one arm. What is left per tool is
  its own sentence and its own bound, which is not duplication. The two
  per-tool shapes that *are* still copied are candidate 3 (the batch answer) and
  candidate 4 (the listing join and its cap).

## The first thing I would do

**The wire markers (candidate 1).** It is the largest saving that is purely
mechanical: 48 code lines of three-times sugar become 22, the compiler checks
every call site (`is_unsent`/`is_framing` are `pub`, `is_overlong` is local),
and it retires three hand-written copies of the classification whose
disagreement is a *money* question — whether a request may be sent again. It
needs no decision about behaviour, only about shape (a plain enum and a marker
struct; this tree has no `macro_rules!`, and none is needed). Second, in the
same hour, candidate 3: 33 lines → 17, and it closes a B20-shaped hole rather
than just shortening code. Third, candidate 4 — and I would stop there and take
candidate 2 (the delivery books) as its own commit with the B24 tests as the
arbiter, because it edits a rule six places share and its value (25–35 lines)
is the least certain number in this document.

## Method

Every non-test line of the nine files was read (`agent.rs` 1-5748, `http.rs`
1-1116, `model.rs` 1-796, `jobs.rs` 1-1648, `machine.rs` 1-642, and the whole of
`signals.rs`, `events.rs`, `clock.rs`, `ids.rs`; the `mod tests` blocks and the
`#[cfg(test)]` fakes were skipped except to know where production ends). Sites
were located with `grep -n`, counted with `awk`/`grep -c` over the exact line
ranges, and repeated *shapes* were found by stripping string literals,
identifiers and numbers and taking `sort | uniq -c` over the production ranges —
which is how the four batch-answer loops and the three marker quartets surfaced,
and how the argument-parser call sites were confirmed as one-line uses rather
than duplication. For every candidate that could drift, the divergence path is
named from the code's own arms, not from a build: no test suite, no smoke
script, no code change.

**Not determined by reading alone:** (a) whether the tool *schema* table
duplicates a list, since it lives in `mush-core` and is another pass's area —
what is measured here is only the call sites in `agent.rs`; (b) whether the
cap-at-two-levels divergence (candidate 4's second half) is worth a behaviour
change for the model's result size — that is a human's call; (c) the real-world
frequency of the `has_room()` race and of a stalled write, which need a run to
observe; (d) whether the wire-marker merge is wanted as a *shape* at all — the
counts say the lines are there, and the tree's taste for named single facts says
yes, but a reviewer who wants each class to keep its own type would keep the
sites and lose the 26 lines.
