# The actor and the wire, audited — base `4436db5`

The area where a defect costs money, a lost run, or a wrong request:
`crates/mush/src/agent.rs` (the run loop, `AgentEvent`, the tool batch,
`spawn_agent`/`wait`/`control`, the mailbox and `drain_mailbox`, cancellation,
phases, token accounting), `http.rs`, `model.rs`, `events.rs`, `clock.rs`,
`ids.rs`, `jobs.rs`/`machine.rs`, and the UI-side readers of those facts
(`app/tree.rs`'s phases and marks, `app/chat.rs`'s meter and transcript,
`app/mod.rs`'s event dispatch).

**Method.** Every doc comment was read as a claim to check. Every finding is
either **proven** — a probe was run against this base and its numbers are quoted,
or a live run staged it — or **suspected**, with what could not be staged said
plainly. The ledger (`docs/findings.md`, the open queue and §8.44–§8.50) was read
first; nothing already fixed is re-reported, and where a recorded item turned out
**worse than recorded** that is said in the finding. Probes were throwaway tests
run with `cargo test -p mush --bin mush <name>` (the workspace is one binary
crate; `-p mush --lib` does not exist) plus pty runs of the real binary; all of
them were deleted and the tree is clean at this commit.

**Baseline.** `cargo test --workspace` at this base: the `mush` suite (216 tests)
and `mush-core`'s, all green, 2.47 s of tests.

**Two fixes in flight, not at this base.** (i) the select mode's paint in
`ui.rs`; (ii) a "thinking" phase signal in `agent.rs`/`app/tree.rs`/
`app/mod.rs`, from the human's report that a finished tool's label sticks
through the next model call. A20 is that second one, audited as it stands and
marked "at this base". Nothing else here lies in those spots.

**Ledger deltas** (recorded items this audit found worse, or measured):
H35/§8.39's "`✉` re-arm, recorded rather than fixed" is worse than recorded —
the stale mark also pins the child's thread and exempts its node from the
history window (A5). H16's residual ("the file is bounded by the history window
… only ever writes live tree nodes") is false on a window whose fold cannot fit
(A8). H12 (per-agent token accounting) has a concrete instance that is worse than
"not visible while spent": the numbers are not reported at the end either, on
four of the roads a run can end by (A6). H34's parenthetical "`done_jobs` is
never pruned" is now measured (A16). B27's class survives one road over: a
hex-garbage chunk size is still diagnosed as the endpoint's refusal (A10).

---

## Priority order

1. **A1** — an endpoint that never sends a newline can make mush allocate
   without bound: the process is OOM-killed and every agent's run, plus up to a
   minute of the session file, goes with it. The one finding here whose loss is
   the human's *data*.
2. **A2** — a request the endpoint already received is sent again, and the
   whole-request deadline is treated as a retryable hiccup: up to six wire sends
   and ~30 minutes for one logical call, on the human's bill.
3. **A3** — a finished job's unread report is parked behind a sibling's machine
   hold for the full 600 s. Ten minutes of wall clock, and the model blind to a
   result it asked for — the H13 blindness, for jobs.
4. **A4** — an event that arrives for an id the UI has already reaped re-creates
   that agent's transcript out of nothing, and a false failure line can survive
   every restart. Unbounded, un-reapable growth plus a lie in the record.
5. **A5** — the `✉` re-arm (recorded in §8.39) also stops `park_history` and the
   history window from ever clearing that agent: a pinned thread and a node the
   window can no longer forget, per race hit.
6. **A6** — the endpoint's own token counts are reported only on a clean,
   tool-free end: a fold, a stop, a failure and a loop-stop report nothing, so
   the one real cost number is missing exactly where money was spent.
7. **A7** — `exclusive`, `detach` and `base` are silently defaulted when the
   model sends the wrong JSON type: a claimed lock that was never taken (two
   benchmarks interleave), and a child that runs in the *parent's* checkout when
   a `base` was asked for.
8. **A8** — the pane's copy is never trimmed and the fold that would reset it
   cannot fit on a window below ≈5.5 k tokens: `.mush/session.json` and the
   `ctx` meter grow without bound while every request fits.
9. The minors (A9–A23), in the order below. None is cosmetic-only, but none of
   them costs money or a run the way the eight above do; A9 (a debug-build actor
   panicking on an endpoint's numbers), A10 (a lost run on a one-line wire break)
   and A14 (an idle fold swallowing the command that arrived behind it) are the
   three worth taking with the batch above while the file is open.

Severity counts: **0 blocker · 8 major · 15 minor**.

---

## Findings

### A1 — A header line with no newline is read without any bound; the process can be OOM-killed by its own endpoint

**Severity: major.** `http.rs:43-45`:

```rust
/// A response body larger than this is refused while it is being read, so a
/// server cannot make mush allocate without bound (docs §8). Generous on
/// purpose: a big diff or a long model reply is normal work.
const MAX_BODY_BYTES: usize = 80 * 1024 * 1024;
```

The cap is on the **body**. `read_line` (`http.rs:591`) — the only reader of the
status line, every header line, every chunk-size line and the trailer
(`exchange`, `http.rs:370-390`; `read_chunked`, `http.rs:952-1005`) — grows a
`Vec<u8>` until it sees `\n` or EOF, with no limit:

```rust
fn read_line<R: BufRead>(reader: &mut R, watch: &Watch) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let Some(chunk) = fill(reader, watch)? else { ... };
        ...
        line.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        if line.ends_with(b"\n") { break; }
    }
```

The only bound is time: the `Watch` deadline (`CHAT_READ_TIMEOUT = 600 s` for a
chat call, 10 s for the model list) and the socket's throughput.

**Evidence (two probes, both run at this base and deleted).**

- Direct: `read_line` over a `Cursor` holding 64 MiB of `a` with no newline
  returned **a 64 MiB line** (`PROBE read_line: returned a 64-MiB line with no
  newline in it`). Nothing in the function can stop it.
- End to end: a loopback server answered `HTTP/1.1 200 OK\r\nX-Big: ` followed by
  85 MiB of `a` (past `MAX_BODY_BYTES`), then a valid `Content-Length: 2` body.
  `get_json` returned **`Ok(200, "{}")`** and peak RSS went **13,560 kB →
  188,672 kB (+175 MB ≈ 2 × the line**: the `Vec` plus
  `String::from_utf8_lossy(...).into_owned()`). A second run wrote 35 MiB in
  1.5 s at loopback speed, so a 600 s chat deadline is room for tens of GB.

**Blast radius.** An endpoint that stalls mid-header — a captive portal, a broken
proxy, a hostile or merely buggy server — makes mush allocate until the OOM
killer takes the process: every agent's transcript, up to `SESSION_DEBOUNCE`
(60 s) of the session file, and every run in flight.

**Fix.** A `HEADER_LINE_CAP` (64 KiB is generous for any real status or header
line) enforced inside `read_line`, raising the `framing(...)` marker — never
`InvalidData`, which is classified as the endpoint's refusal and is not retried.
**Acceptance test:** a server writing `HEADER_LINE_CAP + 1` bytes with no newline
returns an error with `is_framing(&error) == true`, and a `read_line`-only probe
over 64 MiB keeps memory flat (the shape of `an_oversized_body_is_refused`).

### A2 — A request the endpoint already received can be sent again, and the 600 s deadline is a retryable class

**Severity: major (money).** `http.rs:279-291`:

```rust
            // A kept connection the server had already closed. Nothing was
            // heard from it — not one byte of an answer — so the request was
            // never answered and sending it again cannot duplicate anything.
            ...
            let dead_kept =
                reused && !heard && !watch.cancelled() && error.kind() != io::ErrorKind::TimedOut;
```

"Nothing heard" is not "nothing received". The POST head and body are written and
flushed *before* any read (`write_request`, `http.rs:432-470`), and `heard` only
becomes true once a byte of the status line has arrived (`exchange`,
`http.rs:341`). A reset/EOF after the server read the request but before it
answered is `heard == false` → one replacement send *inside* `request`, and then
`model.rs::retrying` retries `Transport | Framing` up to `RETRY_ATTEMPTS`:

```rust
pub const RETRY_ATTEMPTS: usize = 3;   // model.rs:211
...
ModelError::Transport(message) | ModelError::Framing(message) => message.clone(),
```

so one logical call can reach the wire **six times** (2 × 3). Each retry is
announced as `… — retrying (2/3)`, which reads as a pure network problem with no
cost.

Worse, `transport()` (`model.rs:195-209`) lists `TimedOut`, and the
*whole-request* deadline raises exactly that (`Watch::check`, `http.rs:461-464`):
a slow-but-working model that needs more than `CHAT_READ_TIMEOUT` (600 s) is
asked again, with the first generation already billed, for the worst case
`retrying`'s own doc states: *"roughly half an hour for an endpoint that stalls
three times and loses every time"*.

**Evidence.** A loopback server that read request #1 **in full** and closed
without answering. One `retrying` call returned the reply on the retry, and the
server logged **two identical POST bodies**. The classification is read, not
inferred: `TimedOut` is in the retry set and is what the deadline produces.

**Blast radius.** The human's bill: a long prompt carried three times is three
times the input cost for one reply, and the first generation is paid for though
its answer is thrown away. Plus ~30 minutes of a run that looks stuck before it
fails.

**Fix (a ruling, not a patch).** Keep the retry for a reset with `heard ==
false`; treat the 600 s deadline as terminal (raise `Unreachable`, not
`TimedOut`, at the `Watch` deadline) or make the deadline configurable; and say
what is true — the endpoint may have received and charged the request.
**Acceptance test:** with a clock advanced past the read deadline, exactly one
POST reaches the wire (not three); a second test pins `dead_kept` as the *only*
two-send road, with a comment naming the cost.

### A3 — A finished job's unread report is parked behind a sibling's machine hold for the full 600 s

**Severity: major (a stuck run).** `agent.rs:4071-4077` and `agent.rs:4126-4145`:

```rust
/// Whether a `wait` has a result nobody has read to hand over: a child whose
/// body the model has not been given yet. A job's line is not here — it was
/// folded into the transcript when the job ended (`note_job`), so handing it
/// over again is a recap, not news.
fn unread_result(state: &ActorState) -> bool {
    state.children.keys().any(|id| state.unread(*id))
}
```

```rust
            // A result nobody has read comes first, whatever the machine is
            // doing: waiting is what a model does when it wants a result, and
            // parking one behind a sibling's benchmark is the blindness H13 is
            // about. Everything else a digest would carry — an already-read
            // body, a job's line — is a recap of something the transcript
            // already holds, so it is not a reason to refuse to wait.
            if unread_result(state) { ... }
            match machine_wait(actor) {
                Some(held) => {
                    if clock.now() >= deadline { ... }
                    holding = Some(held);
                }
```

The first premise is false: `note_job` (`agent.rs:3513`) only *records* the line
in `done_jobs`. The fold into the transcript happens at a message boundary
(`drain_mailbox`, `agent.rs:3261`), while the mid-call poll `drain_signals`
(`agent.rs:3159`) records without folding. So a job that ends while a `wait` is
in the same batch — exactly `[run_command{detach:true}, wait]` — leaves the line
unread in `done_jobs`, and the machine branch is gated on a predicate that only
looks at children: the wait sleeps to its deadline before handing the job's own
result over.

**Evidence.** A state with one finished job whose line nobody has read, plus
`take_machine(2, "cargo bench")`; `exec_tool(Wait, {})` spent **600 s** of the
fake clock and only then answered the line plus
`wait timed out — #2's exclusive command …`. The identical **child** shape
answers in **0 s**, pinned by
`a_finished_result_is_handed_over_before_the_machine_is_waited_out`
(`agent.rs:11439`, asserting `clock.elapsed() == ZERO`). The job twin does not
exist.

**Blast radius.** Up to ten minutes of the human's wall clock, and the model
handed the result only once it can no longer use it — H13's blindness, for jobs.
The fix is one predicate.

**Fix.** Use the guard the entry already has (`agent.rs:4094-4096`):
`!state.running_jobs.is_empty() || state.done_jobs.keys().any(|job| !state.delivered_jobs.contains(job))`
in place of `unread_result` at the machine gate.
**Acceptance test:** `a_finished_job_is_handed_over_before_the_machine_is_waited_out`,
the job twin, asserting `clock.elapsed() == ZERO` and the line present in the
answer.

### A4 — An event for an id the UI has reaped re-creates that agent's transcript, and a false failure can survive every restart

**Severity: major.** `app/mod.rs:1410-1421` gates only the *conversation*:

```rust
            Msg::Agent { conversation, id, event } => {
                if conversation == self.tree.conversation() {
                    self.on_agent(id, event);
                } else if let AgentEvent::Spawned { cmd, .. } = &event { ... }
```

and no arm of `on_agent` (`app/mod.rs:1514`) checks membership:
`Message` → `chat.push_message` (`:1589`), `Notice` → `note_for`, `Error` →
`fail_for`, `SystemPrompt` → `learn_system`, `Spawned` → `tree.insert`.
`Chat::push_message` writes unconditionally (`chat.rs:867-889`,
`self.agents.entry(agent).or_default().push(message)`), while `Chat::forget` is
called only from `reap_history` (`app/mod.rs:3192`), which walks ids
`tree.past_history()` returns — a map over `tree.agents`. A ghost id is therefore
unreachable by every reaping path, forever. The neighbouring doors *do* check:
the attach door guards membership (`app/mod.rs:2525`), and `tree.has` exists
(`tree.rs:1685`).

**Evidence** (probes at this base, then a real pty run): 51 finished children +
`app.tick()` to force a real reap, then a late event for a reaped id —

- `probe_a_late_message_for_a_reaped_child_recreates_a_transcript` fails:
  `transcript(id)` is `["a reply the reap was too late for"]`;
- `probe_a_late_failure_for_a_reaped_child_is_written_to_the_session` fails, and
  `probe_a_ghost_failure_comes_back_on_the_next_launch` fails
  (`next.chat.notices_for(AgentId(1)).count() == 1`, +54 JSON bytes in the
  stored session — an `Error`-kind notice *is* stored, `chat.rs:1176-1185`);
- `probe_a_reaped_childs_live_actor_still_writes_a_ghost_transcript` fails.

Reachability was staged, not assumed: `drain_actors` is non-blocking in the
event loop (`main.rs:990/1022`) and `app.tick()` (the reap/park tick) runs at
`main.rs:1028`, so anything an actor emits after the last drain and before the
tick is applied *after* the reap — a window of one frame (≈30 ms), every frame.
The same window can also `Shutdown` a run that just started (`park_history`'s
`Shutdown` on a node whose actor had begun a run the UI has not heard about
yet) — **suspected**, the §8.21 window the nudge road already documents; not
staged.

**Blast radius.** A transcript (and its weight, revision, voices and reading
position) for a pane nothing can open, that no reaping path can ever remove,
plus a `✗` line about work that never happened, restored on every launch. Both
grow with the session.

**Fix.** `if self.tree.has(id) { self.on_agent(id, event) } else if let
AgentEvent::Spawned { cmd, .. } = &event { let _ = cmd.send(AgentMsg::Shutdown); }`
— one guard on the door the neighbouring doors already have.
**Acceptance test:** `a_late_event_for_a_reaped_agent_changes_nothing` — 51
children + a tick, then `Message`/`Notice`/`Error`/`SystemPrompt`/`Done`/
`Spawned` for the gone id; assert `transcript(id).is_empty()`,
`notices_for(id).count() == 0`, `used_weight_for(id) == 0`, nothing in the stored
session and no new row.

### A5 — The `✉` re-arm is worse than recorded: it also pins the child's thread and exempts its node from the history window

**Severity: major.** The ledger (§8.39, "The `✉` re-arm, recorded rather than
fixed") records the wrong *mark*. The consequence is not just a mark. The child
sends its parent `ChildDone` **before** it emits the run's ending to the UI:

```rust
        actor.tell_parent(AgentMsg::ChildDone {          // agent.rs:1595
            id: actor.id,
            run: state.runs,
            outcome: outcome.clone(),
        });
        match outcome {
            Outcome::Failed(error) => actor.ctx.emit(actor.id, AgentEvent::Error(error)),
            Outcome::Stopped(_) => actor.ctx.emit(actor.id, AgentEvent::Stopped),
            Outcome::Finished(_) => actor.ctx.emit(actor.id, AgentEvent::Done),   // :1603
```

If the parent is scheduled in that gap, it folds the result and emits
`ResultRead` (which clears `result_unread`), and *then* the child's `Done` lands
and re-arms the mark (`tree.rs:1061-1067`):

```rust
    pub fn finish(&mut self, id: AgentId, summary: Option<String>) {
        if let Some(node) = self.node_mut(id) {
            let unread = node.parent.is_some();
            ...
            node.result_unread = unread;
```

Nothing can clear it afterwards: every `ResultRead` emitter is gated by
`record_child`'s `fresh`, and `delivered` already names that run
(`agent.rs:1070-1078`). The stuck mark then feeds two other rules:
`may_park`'s `let result_read = !node.result_unread || past_window`
(`tree.rs:1432`) keeps the thread, and `kept`'s `|| node.result_unread`
(`tree.rs:1325`) keeps the node out of `past_history` — so §8.21's two caps (the
newest 8 warm threads, the 50-node window) stop bounding that agent.

**Evidence** (real pty runs of the shipped binary; stored sessions inspected at
rest): n=51 → one child left `unread`, its `mush-agent-13` thread alive for the
whole 78 s watch (10 agent threads at rest); n=60 → 7/60 unread, 13 threads alive
at +25 s, **57 nodes stored where the window should hold ≤50**; three n=20 runs →
unread sets `{2,19}`, `{1,6,8,14}`, `{2,5,11,19}`, one thread each. Rate under
load: 1/51, 2/20, 4/20, 4/20, 7/60. The state-machine half is deterministic:
`ResultRead` then `Done` ⇒ unread and unparkable; `Done` then `ResultRead` ⇒
read and parkable. No read child outside the warm window kept a thread — the
leak is exactly this race.

**Blast radius.** Per race hit, a pinned thread, a lying `✉`, and a node the
history window can no longer forget. Accumulating over a session; after a
restore, `app/mod.rs:942`'s `read: !node.result_unread` hands the parent
`read: false`, so a duplicate `#N done:` delivery is **suspected** (code read
only).

**Fix.** Emit the run's ending event *before* `tell_parent(ChildDone)` (move
`agent.rs:1599-1607` above `:1595`): both events go to the same UI channel, so
the parent's `ResultRead` can then never be ordered before the `Done` that
re-arms the mark. (The ledger's alternative — the run number on both events — is
a more general fix; the reorder is the smallest one.)
**Acceptance test:** the state-machine probe above, plus an end-to-end run of 20
children to `Done` + idle asserting the stored session holds zero
`result_unread: true` and at most the warm number of agent threads.

### A6 — The endpoint's own token counts are reported only on a clean, tool-free end, and a fold's usage is dropped

**Severity: major (the money number).** `agent.rs:95-102`:

```rust
/// Report the endpoint's own numbers once, when the run ends. Cheap and rare
/// (one line per run), and the only place a real count can come from: the
/// UI's meter is bytes/3, which is all a server without `usage` offers.
fn report_usage(actor: &Actor, usage: Option<RunUsage>) {
```

`report_usage` has exactly one call site, `agent.rs:2747`, inside the
`tool_calls.is_empty()` branch — the *clean* end. Every other ending (a Stop, a
failure, the loop guard, an over-window refusal) returns from `run_loop`
directly, and `compact_history`'s own `ask` (`agent.rs:3020`) never reads
`reply.usage`, though the fold re-sends the whole history (usually the largest
request of the run, and the one that most needs pricing).

**Evidence.** A fold the endpoint counted at 9,000 prompt + 100 completion with
a final reply carrying no `usage`: the run's notices were **empty**. A run whose
first reply reported 4,040 tokens and was then cancelled (`Err("cancelled")`):
**no notice**. An independent probe pinned the same with a fold of 1,111/222/1,333
and a final reply of 7/5/12: the one line read *"the endpoint counted 7 prompt +
5 completion tokens this run (12 total)"* — the fold absent.

**Blast radius.** The only number in mush that is not an estimate disappears
exactly where money went: a stopped long run, a cancelled run and a fold
(hundreds of thousands of tokens on a big window) each report nothing, and the
human's cost picture falls back to a bytes/3 estimate. This is the concrete
instance of the open H12, and worse than recorded: not just invisible while
spent, but unreported at the end.

**Fix.** Move the report to `actor_main` (`agent.rs:1624`), where the outcome is
decided, so every ending reports what accumulated; add the fold's `reply.usage`
to the accumulator (return it from `compact_history`, or give `ActorState` a
`usage: Option<RunUsage>` both callers feed).
**Acceptance test:** a run with a fold and a final reply reports the sum of both
calls; a cancelled run after one counted call still emits its line.

### A7 — `exclusive`, `detach` and `base` are silently defaulted when the JSON type is wrong

**Severity: major.** `agent.rs:5012-5016` and `agent.rs:3761`:

```rust
    let detach = args.get("detach").and_then(Value::as_bool).unwrap_or(false);
    let exclusive = args
        .get("exclusive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
```

```rust
    let named = args.get("base").and_then(Value::as_str);
```

The house rule is the opposite, stated in `mush-core/src/tools.rs:125-158`:
*"A value that is present but not a number is refused rather than defaulted:
silently reading line 1 when the model asked for a window is how a read answers
a question nobody asked"* — and `arg_bool`/`arg_usize`/`arg_path` all obey it.
`spawn_tool`'s own comment says a base *"is resolved before anything is created
and never silently dropped (finding H7)"*; `Value::as_str` dropping a non-string
base is exactly a silent drop.

**Evidence.** Three direct `exec_tool` probes, with the machine held by #2:

- `{command:"cargo bench", exclusive:"true"}` **ran** with a `beside_note`
  ("ran while #2 held the machine for an exclusive command"), where the pinned
  test shows `exclusive: true` is refused: **two benchmarks interleaved**, the
  one thing the lock exists to prevent.
- `{command:"serve", detach:"yes"}` became a foreground call — a 60 s block and,
  for anything longer, the 120 s kill `detach` promises not to apply.
- `{brief:"…", base:7}` (also `null`, `["main"]`) spawned a **shared** child in
  the parent's checkout: the child's edits land in the parent's tree and its
  branch does not exist — the M2.6 "merge git is asked to do is a lie" shape H32
  is about.

**Blast radius.** A model that emits `"true"`/`"yes"`/a number quietly loses the
lock (a wrong measurement), the detach (a killed server), or the isolation (work
in the wrong tree). None is refused, so nothing tells the model.

**Fix.** `tools::arg_bool(args, "detach", false)?` and
`tools::arg_bool(args, "exclusive", false)?`; for `base`, a `Value` match that
reads `null` as absent and refuses any other non-string.
**Acceptance test:** the three wrong-typed calls are refused with the sentence
`arg_bool`/`arg_path` already own, shaped like the existing `exclusive: true`
refusal test.

### A8 — The pane's copy is never trimmed, so on a window whose fold cannot fit the session file and the `ctx` meter grow without bound

**Severity: major.** `app/chat.rs:1020-1026`:

```rust
    /// already bounded without it: a conversation is folded at nine tenths of
    /// its history budget, a *finished* child's is never folded again (it sits
    /// frozen at whatever it reached), and the file's bound is `CHILD_HISTORY ×
    /// that fold trigger + the root`. So dropping the row drops one child's
    /// frozen transcript from the next save.
```

The bound assumes the fold always fires. The one road into the UI's copy of a
conversation is an append (`app/mod.rs:1589`,
`self.chat.push_message(id, message)`), the only writer that removes turns is
`AgentEvent::Compact` → `replace_transcript` (`app/mod.rs:1756-1761`), and the
run's `trim_history`/shed touch the **actor's** list only. But a fold is refused
whenever `SCHEMA_TOKENS + prompt_tokens + 1024 > context_tokens`
(`fold_request_fits`, `agent.rs:2232`) — i.e. **every window below ≈5.5 k
tokens** at the trigger — and `/context` accepts down to 1,024. There,
`trim_history` keeps cutting the actor's list while the pane's copy accumulates
turn after turn, and `used_weight_for` (`app/chat.rs:960`) weighs the pane's
copy.

**Evidence.** At a 4,000-token window (a 6,000-byte budget), 24 runs through
`run_loop` with a scripted model left the actor's list at 6,315 B while the UI's
copy grew **24,417 B → 48,621 B across 12 → 24 runs**; a sweep of windows
1,024→20,000 showed the fold refused for all of 1,024…5,376 and fitting from
5,504. At the app level, 24 `Message` events gave `used_weight_for(ROOT) =
51,671` against `history_budget() = 12,288`, the meter saying
`ctx 17.2k/4.1k over (fold 3.7k) ~8.2k`, and `session_snapshot()` serializing
**48,999 B** — which `Session::save` writes whole, every 60 s.

**Blast radius.** On a small window (a local model, `/context 2000`) the one
surface the human uses to judge context reads `over` forever though every request
fits, and `.mush/session.json` grows linearly with the conversation, rewritten
whole on each save. H16's residual claim ("the file … is bounded by the history
window … only ever writes live tree nodes") is false in this band. On the 8 K
default the fold fits, so the drift is one fold cycle.

**Fix.** Make the stored row and `used_weight_for` read a bounded view rather
than the pane's record (trim a copy in `session_snapshot`/`used_weight_for`, or
have the actor publish its trimmed transcript — the dropped-note road already
exists); keep the pane's full record in memory only if the human's record is what
it is for.
**Acceptance test:** on a 4 K window after 24 runs, the stored row and `used`
stay within one turn of the budget (≤ ~8 KB, not 49 KB), and the meter never
shows `full`/`over` while the actor's own list fits.

### A9 — `RunUsage` adds endpoint-supplied `u64`s with `+=`

**Severity: minor.** `agent.rs:68-93`:

```rust
    fn add(&mut self, usage: &mush_core::Usage) {
        self.prompt += usage.prompt_tokens;
        self.completion += usage.completion_tokens;
        ...
    fn line(&self) -> String {
        let total = if self.total_missing {
            self.prompt + self.completion
```

`Usage`'s fields are plain `u64`s parsed from the wire, so `u64::MAX` is a value
an endpoint can send.

**Evidence.** A scripted run whose replies carried `u64::MAX` in all three fields
panicked **`attempt to add with overflow` at agent.rs:69** on the second request
— in a debug build the actor thread dies mid-run (the UI files it as a cut-off
run). In release (`[profile.release]` sets no `overflow-checks`) the same
arithmetic wraps and the human reads a nonsense cost, silently.

**Blast radius.** A debug-build actor dies for a reason nothing connects to the
endpoint's numbers; a release mush prints a wrong money number.

**Fix.** `saturating_add` in `add` and `line` — the same choice `request_weight`
(`agent.rs:2157`) and `Image::weight` already make, for the same reason.
**Acceptance test:** a reply carrying `u64::MAX` produces a saturated line and no
panic (run in both profiles).

### A10 — A hex-garbage chunk size is classified as the endpoint's refusal, so a wire break that would be retried kills the run instead

**Severity: minor.** `read_chunked` parses a size line with
`usize::from_str_radix(size_field, 16)` (`http.rs:966-974`) and reads it through
`read_chunk_bytes` → `read_exact` (`http.rs:919-927`):

```rust
fn read_chunk_bytes<R: BufRead>(reader: &mut R, len: usize, watch: &Watch) -> io::Result<Vec<u8>> {
    read_exact(reader, len, watch).map_err(|error| match error.kind() {
        io::ErrorKind::UnexpectedEof => body_cut_off(),
        _ => error,
    })
}
```

A size line that is garbled but still parses as hex (`FFFFFFFF`) passes the cap
check and becomes `body_too_large()` → `InvalidData` → `ModelError::Refused`
(`model.rs:141-145`) — which is *before* the framing check and is never retried.

**Evidence.** A loopback server answering
`HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFF\r\nhello\r\n0\r\n\r\n`
produced `kind=InvalidData is_framing=false
msg=the response body is larger than 83886080 bytes` — the sentence a healthy
endpoint is blamed with, where B27's fix made the equivalent break
(`malformed chunk size: ""`) a `Framing` error retried once on a fresh
connection.

**Blast radius.** A flaky proxy garbling one size line loses the whole run and
blames the endpoint (the B27 experience, observed live once). One retry would
have saved it.

**Fix.** Have `read_chunk_bytes` distinguish "the wire claimed more than the cap"
from "a body that really is that big": a chunk-size line is framing, so raise the
`framing(...)` marker there, keeping `Content-Length`/EOF-framed bodies a
`Refused` refusal as their own doc requires.
**Acceptance test:** the probe above reports `is_framing(&error) == true`, and a
`Content-Length` past the cap stays `InvalidData`/`Refused` (the existing
`an_oversized_body_is_refused` pins that half).

### A11 — `status` is a big-text road that ignores `result_cap` and `turn_room`

**Severity: minor.** `result_cap`'s doc (`agent.rs:5185-5190`) says *"Every
big-text road uses it — a command's output, a file read, a listing, a search"*,
and `run_loop` spends the shared `turn_room` with each result's weight
(`agent.rs:2771-2821`). `status_tool` (`agent.rs:4398`) never calls it; its
bound is the registry's own (`STATUS_WINDOW = 6,000` plus each job's headline).

**Evidence.** A `status` with 4 ended jobs at an 8 K window
(`budget=12288`, `cmd_cap=2458`, `result_cap=2458`) returned **8,197 bytes** —
3.3× the turn's whole room, and the pinned jobs test allows
`STATUS_WINDOW + 16 × headline = 14,384`, past the 12,288-byte budget.

**Blast radius.** The next request crosses the window and H43's
`shed_newest_results` replaces the *newest* results — the listing the model just
asked for and any fresh result beside it — with `SHED_RESULT_NOTE`, and the human
reads "dropped N tool result(s)". A cap that does not bound what the turn can
carry is what `turn_room` was added to prevent.

**Fix.** `truncate_for_model(status, result_cap(actor, state))`, as
`list_tool`/`search_tool` do, or derive `STATUS_WINDOW` from the window the way
`cmd_cap` is.
**Acceptance test:** the jobs test asserts `≤ result_cap`, not
`≤ STATUS_WINDOW + furniture`.

### A12 — `write_file` reads the whole file it is about to replace, to answer its line count

**Severity: minor.** `agent.rs:4800-4812`:

```rust
    let existed = actor.ws.exists(&path);
    let before = actor
        .ws
        .read_file(&path)
        .ok()
        .map(|text| text.lines().count());
```

`read_file` is the uncapped whole-file read; the answer
(`wrote path — N → M lines`) needs only a count.

**Evidence.** One `write_tool` over a 128 MiB text file: peak RSS **140,548 kB →
402,980 kB (+262,432 kB = 2.0 × the file** — the `fs::read` Vec plus
`from_utf8_lossy(...).into_owned()`'s copy), and the whole file is read before
the replacement is written.

**Blast radius.** A model that overwrites a multi-GB log OOM-kills the process —
every agent's run, up to a minute of session file. `read_file`'s own road is
capped at 32 MiB; this road is not.

**Fix.** Count lines while streaming with a bounded buffer, or answer
`(replaced)` past a size threshold — the count is decoration, the write is the
work.
**Acceptance test:** a write over a 256 MiB file keeps peak RSS near the new
content's size.

### A13 — The pane re-parses every visible tool call's whole argument JSON on every frame

**Severity: minor.** `app/chat.rs`'s `tool_label` calls `summarize_args`
(`agent.rs:5468`), a full `serde_json::from_str` of
`call.function.arguments`, per frame from `render_message`. The file bounds the
*result* side for exactly this reason (*"Wrapping the whole result was most of a
frame's cost on a long session"*), and `mush-core/src/text.rs:759` states the
invariant: *"The cost of a row is therefore the row's width, not the length of
the text behind it."* H45/H46 removed the caps on `content`/`new_string`, so the
argument is unbounded.

**Evidence.** `{"path":"a.txt","content":"…"}`: 20 KB → **171 µs**, 200 KB →
**1.17 ms**, 2 MB → **9.68 ms per call, per frame** (debug build; the label
printed is the 4-byte path).

**Blast radius.** A stuttery pane and lagging keystrokes while a big call is on
screen; the frame budget is the human's perception of the whole app.

**Fix.** Cut the raw string to a bounded prefix before parsing (the
`sanitize_upto`-style `4 × budget + 64` shape), or cache the label per call id.
**Acceptance test:** a label path receiving ≤ N bytes for a 4 MB call, or a
frame-budget test with a big call on screen.

### A14 — The idle fold drains the mailbox like an in-run one: a command meaning "start a run" is swallowed

**Severity: minor (narrow window, total loss when it lands).**
`wait_for_work` (`agent.rs:1648-1654`) honours a pending fold at rest by calling
`compact_now`, whose folder is documented *"Fold pending mailbox commands into
the **current run**"* (`agent.rs:3232`, call at `agent.rs:2953`). With no run to
fold into, a `Nudge`/`Run` drained there is pushed into a transcript the fold
then replaces (`let system = messages[0].clone(); *messages = vec![system,
Message::user(...)]`); `drain_mailbox` returns nothing, so the `Fold::Run` the
command meant is lost and the idle loop's next act is a blocking `recv`.

**Evidence** (a probe passing at this base): with `Nudge("carry on")` already in
`actor.rx` and `compact_requested` set, `compact_now` made exactly 1 call — the
words were inside the summarize request — the transcript came back as
`[system, compaction_message]` with the line gone, and **no `Running` was ever
emitted**. The same shape with a `ChildDone` in the window leaves the completion
recorded and unread with nothing to wake the parent (the row keeps `✉`); a
`CommandDone` is worse: `record_job` inserts into `delivered_jobs`
(`agent.rs:1088`) and the line is then erased — **marked read, never a line of
the conversation**. Window: the command must sit behind the `Compact` in the same
channel batch while the actor is at rest (microseconds between the `recv` and the
drain; up to the run's tail work — an isolated agent's `commit_worktree`, tens of
ms — when the flag was set at the end of a run).

**Fix.** Do not drain for `in_run == false`, or make `compact_history`/
`drain_mailbox` report a `Fold` and let the idle loop start the run the command
asked for.
**Acceptance test:** the probe above (the mailbox is left, or a `Running`
follows the fold), plus an end-to-end where `Compact` + `Nudge` are queued
together.

### A15 — Parking a child kills the jobs it started, and `park_history`'s doc says parking ends "the thread and nothing else"

**Severity: minor (doc vs behaviour).** `app/mod.rs:3204-3210`:

```rust
    /// ... Parking ends the thread and nothing else: the node, the id and the
    /// transcript stay exactly where they fall, and the next message to that
    /// child rebuilds its actor from the transcript on screen
```

`park_history` sends `AgentMsg::Shutdown` (`app/mod.rs:3222`), whose arm is
`registry.kill_owned(actor.id)` (`agent.rs:3171`, `agent.rs:3261`, and the idle
`absorb`, `agent.rs:1783`). §5.6 and `reap_history`'s own doc say jobs die with
their agent, so the *behaviour* is documented — this sentence is the one that is
not.

**Evidence.** A fake machine counted the kill after the park's `Shutdown`
(`after Shutdown: kills=1 … cancel=true`). The trigger is unrelated: any later
child finishing past the warm window recomputes `parkable`.

**Blast radius.** A server, watch or benchmark a child detached — `jobs.rs` calls
a job "the one thing here that is meant to outlive the run that started it" — is
SIGKILLed later with no line in the transcript (the actor is gone), and the
registry prunes the record after 8 more ended jobs, so a revived child cannot
learn what happened. Either the behaviour or the sentence may be the human's
choice; what cannot stand is both.

**Fix.** Say it in `park_history`'s doc (cheapest: `may_park`'s `in_flight_with`
already refuses to park a node with a live job), or park with a message that ends
the thread without `kill_owned`.
**Acceptance test:** if the behaviour stays, the pinned sentence; if not, a test
that a parked child's job still runs.

### A16 — Three per-child books grow for the life of an actor

**Severity: minor.** `ActorState::done_jobs`/`delivered_jobs` (`agent.rs:926-927`)
and `forgotten` (`agent.rs:946`) are insert-only; no `.remove` exists on any of
the three, and the source they mirror prunes itself (`Registry::finish`,
`jobs.rs:1263`, keeps `MAX_JOBS + JOB_HISTORY = 16` records).

**Evidence.** 1,000 jobs left `done_jobs = 1,000` and `delivered_jobs = 1,000`
holding **61,893 B** of line text; 1,000 `forget_child` calls left
`forgotten = 1,000`. The root actor is never parked or reaped (`parkable` skips
the parentless root), so its books live for the session, and each `done_jobs`
entry is a second copy of a line already in the transcript.

**Blast radius.** Memory only — a session with a thousand jobs holds tens of KB
nothing can reach. (The ledger already notes "`done_jobs` is never pruned" inside
H34's row; this is the measurement.)

**Fix.** Cap `done_jobs` at the registry's own history, oldest-first, and drop a
tombstone once no in-flight report can name it.
**Acceptance test:** after 100 jobs the books hold ≤ 16 entries while `status`
and `wait` still answer for live ones.

### A17 — `LOST_POOL`'s doc says the oldest lost number is forgotten; the code drops the newest

**Severity: minor.** `ids.rs:68-71` vs `ids.rs:154-158`:

```rust
/// Past this, the oldest lost number is forgotten and simply stays a gap, which
/// costs nothing: gaps are what the counter is for.
const LOST_POOL: usize = 8;
...
        if id.0 >= agents.counter || agents.lost.len() >= LOST_POOL {
            return;
        }
```

The incoming id is dropped, so the *newest* loss becomes the gap and the oldest
eight are reused.

**Evidence.** Draw #1..#9 and lose them all: the next draw is **#8** (the oldest
eight kept, #9 the gap), where the doc predicts #9.

**Fix.** One line of prose, or push oldest-first. **Acceptance test:** a pool
test that names which number is the gap when the pool is full.

### A18 — The dropped-turns note is put back in its place only for the root

**Severity: minor.** `place_dropped_note`'s only caller is `adopted`
(`agent.rs:1751`), reached from `Run`/`Adopt`/`Compact` — and `AgentMsg::Run` is
only ever sent to the root (`app/mod.rs:2274`). A child is revived with a
`Steer`/`Nudge`, so its copy is never adopted.

**Evidence.** After trims, an actor's note sat at index 2 of 9 while the pane's
copy held it at **index 4 of 26**, with 21 later messages after it: a child's
pane tells its reader the dropped-turns line *is* the front of the conversation
when it is four lines in. H44's record claims the placement fix generally.

**Fix.** Place the note on the append for every agent (one helper both roads
call), or emit the placement with the note.
**Acceptance test:** a child's pane holds the note at index 2 after a trim.

### A19 — Three sentences that claim more than the code does (a repeated notice, a shed note, two `null`s)

**Severity: minor.**

- `for_the_model` (`agent.rs:2194-2202`) emits the "dropped N image part(s)"
  `Notice` on **every** request assembly (`run_loop:2489`,
  `compact_history:2989`), so a 20-turn run with one picture repeats it 20 times.
  §8.45's own precedent made the fold refusal "once per state … the same
  unchanging transcript retried every turn is not news". **Evidence:** a
  2-request run with one picture produced 2 identical notices. **Fix:** a
  `dropped_images_said` flag beside `fold_refused`, reset on `/model`.
- `SHED_RESULT_NOTE`'s doc (`agent.rs:2254-2256`) says *"The rewrite lands in the
  actor's copy, so every later request in this conversation says what happened to
  the result."* True within a run, false across the next idle hand-over:
  `absorb`'s `Run` arm does `*transcript = adopted(messages)` (`agent.rs:1796`)
  from the UI's untrimmed copy, restoring the bytes and re-emitting the
  "dropped N tool result(s)" line. No wire hole (the shed is re-applied before
  the request), but the doc is wrong and the work repeats. **Fix:** reword to
  "this run", or re-apply the shed in `adopted`.
- `wait({"on": null})` is refused (`on` must be one target … got null) while
  every other optional argument reads `null` as absent (`tools.rs:130/141/155`)
  — one wasted turn for a model that fills optionals with `null`. And
  `arg_string` says "missing `path`" for `{"path": 7}`, where the field is
  present with the wrong type (the H20 class, unrecorded). **Fix:**
  `Some(Value::Null) => Ok(None)` in `wait_target`, and one sentence for a wrong
  type.

### A20 — A finished tool's label sticks through the next model call (**at this base**)

**Severity: minor.** `app/mod.rs:1573` routes `AgentEvent::Status` to
`tree.activity` (`app/tree.rs:965`), which sets `Phase::Activity(label)`. The
only events between a batch's tool results and the next request are
`AgentEvent::Message(tool)` (`app/mod.rs:1585` → `chat.push_message`, no phase
change) and nothing else, so after `edit_file src/a.rs` returns the row keeps
saying `edit_file src/a.rs` while the model generates the next reply.
`Phase::Thinking` is set only by `begin` (on `Running`), by a fold's endings and
by `compacting`. The human reported it; a fix is in flight in
`agent.rs`/`app/tree.rs`/`app/mod.rs` and is **not at this base**.

**Fix (the in-flight one, for the record):** emit a phase signal when a tool
result is stored (or before the next request), so the row wears `thinking…`
between the last label and the next one.
**Acceptance test:** after a scripted tool batch, the agent's phase is `Thinking`
between the last tool result and the next request.

### A21 — Two `expect("workspace root must exist")` sit on the UI thread

**Severity: minor, suspected.** `agent.rs:1253` (`root_actor`) and
`agent.rs:1364` (`revive`) both do
`Workspace::new(&…).expect("workspace root must exist")`; `Workspace::new` is
`fs::canonicalize(root)` (`mush-core/src/workspace.rs:307-311`), which fails for
a path that is gone. `root_actor` is called from `App::new_chat` (Ctrl-N) and
`revive` from `App::deliver_to_actor` (the human's message to a parked child),
both on the UI thread — a panic there takes mush down with the terminal
unrestored.

**What could not be staged:** the realistic road. `revive` reads
`live_branch`/`path.exists()` first, so the window is the race between that check
and `canonicalize` (a sibling's `git worktree remove`, which mush's own refusal
sentence instructs a model to run); `root_actor`'s path is the cwd, deleted
mid-session by an agent's own `rm -rf`. The unit fact — that `Workspace::new` can
fail there and the code panics — is certain.

**Fix.** Handle the error: emit `AgentEvent::Error` and keep the actor on the
root path (or refuse the revival with a sentence) rather than unwrapping.
**Acceptance test:** `revive` with a worktree path removed between the check and
the call returns an error, not a panic.

### A22 — After a restore the job space restarts at `#c1` while the restored transcript still names old `#cN` lines

**Severity: minor, suspected.** `Session`/`AgentSession` carry no job counter
(`mush-core/src/session.rs`), `Ids::default()` starts jobs at 1 (`ids.rs:118`),
and the restore raises only the *agent* floor (`app/mod.rs:743`) while
`session_snapshot` restores the `#cN done: …` lines into the transcript. A
`control stop #c1` after a restart can therefore address a different command than
the line the model is reading. No book is corrupted (the books are fresh).

**What could not be staged:** a live restore with jobs outstanding. **Fix:**
store the highest job id the transcript names and raise the job floor at restore.

### A23 — Cleanup is process-group-only; the doc claims "everything the command started"

**Severity: minor.** `machine.rs:108`, the comment the module is written
around:

```rust
        // build and cleanup can target everything the command started.
```

and `machine.rs:112` `shell.process_group(0)`, killed by
`machine.rs:175-181` `.args(["-9", &format!("-{group}")])`. A command that calls
`setsid` (or a daemon that re-parents) leaves the group.

**Evidence.** A tool call `setsid sleep 297` (mush pid 2671947) left `sleep 297`
(pid 2672068, `ppid=1`, its own `sid`/`pgid`) alive after a clean quit (exit 0,
0.01 s), and no `Stop` could reach it — the tool call had already returned.

**Blast radius.** One leaked process per re-sessioning command; the cleanup
guarantee is the sentence, not the mechanism.

**Fix.** The sentence, or a killed session/cgroup. **Acceptance test:** spawn
`setsid sleep N`, assert no survivor (currently fails).

---

## Verified sound (and the check that convinced us)

The previous audits' most valuable section, kept.

- **Tool-call/result pairing, every exit.** Every arm of the batch — normal,
  truncated, refused, loop-stop, cancelled, unknown tool — pushes exactly one
  `Message::tool` per call, in call order, with ids from the *sanitized*
  assistant message that went into history. Pins:
  `a_reply_whose_calls_have_no_ids_still_gets_answered`,
  `a_refused_reply_answers_the_calls_it_carried`, the truncation tests,
  `only_stop_a_tool_batch_and_the_cap_are_normal_ends`.
- **Adopted/restored transcripts are repaired.** `adopted` →
  `repair_tool_pairs` (results beside their batch, missing calls answered, legacy
  empty ids re-pointed) → `drop_orphan_results` → `place_dropped_note`; one door
  for `Run`, `Adopt` and `Compact`. 31 transcript tests green at this base.
- **The window invariant on every assembled request.** `for_the_model`'s output
  is weighed before `ask`; over budget, the newest turn's results are shed
  largest-first (never a picture, never the human's words) and then the request
  is refused with one line. Pins:
  `no_request_the_trim_cannot_cut_goes_over_the_window`,
  `the_window_takes_back_the_newest_turns_results_and_says_so`,
  `a_picture_the_window_cannot_hold_is_refused_before_the_wire`,
  `a_transcript_that_cannot_fit_is_refused_with_one_line`,
  `one_turns_results_share_the_room_under_the_ceiling`.
- **The blind-model gate is asked at assembly, for run and fold both**, with the
  bytes kept in the actor's transcript. Pins:
  `a_blind_model_is_never_sent_an_image_part`,
  `the_folds_request_is_stripped_too_for_a_blind_model`. (Its *notice* repeats —
  A19 — but the gate itself is right.)
- **Retry classification is one class wide.** Only `Transport | Framing` is
  retried; `Cancelled`, a status (4xx *and* 5xx), a refusal past the body cap and
  an unparseable body return first time; the backoff polls the cancel flag and
  the deadline in 50 ms slices; attempts are bounded and announced. Pins:
  `only_a_failure_of_the_wire_is_a_transport_failure`,
  `an_answer_from_the_endpoint_is_never_retried`,
  `a_cancellation_during_the_backoff_abandons_the_retry`,
  `a_chunked_body_cut_off_on_a_real_wire_is_retried_on_a_fresh_connection`.
  (A2 is what a retry *costs*, not a class leaking.)
- **Pool safety.** A connection is kept only after a self-framed HTTP/1.1 body
  with no `Connection: close`; every failure (cancel, deadline, cut body, broken
  frame) returns `Err` and drops it; a reused dead connection is replaced once;
  `heard` blocks the replacement send once the answer has started. 29 `http`
  tests + 9 `model` tests green.
- **Cancellation reaches a model call that has not answered.** The flag travels
  with `AgentEvent::Running`; `http.rs` polls it after every successful read
  *and* on every slice timeout; a Stop outranks the deadline and the retry; a
  cancelled body is never kept in the pool. Pins:
  `a_cancelled_chat_request_stops_at_once`,
  `a_cancelled_dribbling_response_stops`,
  `an_already_cancelled_request_is_not_sent`,
  `a_cancelled_body_is_not_kept_for_the_next_request`.
- **Ctrl-C reaches every process *inside* the group, fast** (pty timings):
  foreground `sleep 300` dead in 0.020 s, a child-owned command in 0.022 s, a
  detached job `#c1` in 0.006 s, `sh -c 'sleep 299 & wait'` (both) in 0.013 s —
  all ≪ `CMD_TIMEOUT_SECS`; quitting with a foreground command and with a job
  left no survivors (pgrep by session). (`setsid` is the one escape — A23.)
- **A finished tool batch is never lost to a mailbox race.** Mid-batch
  `drain_signals` parks nudges/steers and records completions; the boundary
  `drain_mailbox` folds the parked queue *first*; `Shutdown` answers the rest of
  the batch with `error: cancelled` so no call dangles; and a Stop that arrives
  behind the work it aimed at is caught by `wait_for_work`'s queued drain and
  born into `run_cancel`. Pins:
  `a_stop_behind_the_run_it_was_aimed_at_is_not_swallowed`,
  `a_stop_in_the_mailbox_interrupts_a_running_command`,
  `stop_cancels_but_shutdown_ends`,
  `drain_signals_parks_nudges_for_the_next_boundary`.
- **Delivery is once, from any road** (fold, wait, status digest, adoption).
  `record_child`/`record_job` are the only two gates and both answer "fresh or
  not"; adoption only *adds* marks, never removes or moves one backwards; a
  forgotten child cannot re-open a book (tombstones checked in `note_completion`,
  `note_mailbox`, `note_running`, `note_work`); `note_parked` moves a book to
  `NO_RUN` so a woken child's run 1 is news. Pins:
  `every_delivery_road_hands_a_result_over_once`,
  `a_child_completion_is_delivered_once_across_an_idle_run`,
  `a_job_report_is_delivered_once_across_an_idle_run`,
  `forgetting_a_child_drops_its_books_and_swallows_a_late_report`,
  `a_row_handed_over_after_a_forget_reopens_the_child`.
- **A result the parent has read wakes nobody**: `Outcome::is_news` is false
  only for `Stop::Reclaimed` (a park), while a stop by a hand *is* news and names
  the hand. Pins: `a_stop_wakes_a_napping_parent_but_a_park_does_not`,
  `a_cut_off_child_wakes_a_napping_parent`,
  `status_distinguishes_stopped_from_done_and_failed`.
- **Parking/reaping cannot leak a job.** `may_park` asks the registry
  (`in_flight_with` → `live_jobs`), so a node with a live job is not parked; the
  park/reap `Shutdown` kills its jobs through `kill_owned`, which walks both jobs
  and foregrounds; `report_cut_off` is the UI's kill for an actor that vanished
  with jobs (`Registry::stop` refuses any caller but the owner); `Drop for
  Registry` is the backstop. Pins:
  `a_held_command_is_killed_by_its_owners_stop_and_by_kill_all`,
  `a_stop_ends_the_jobs_of_the_agent_it_was_aimed_at`,
  `the_job_budget_is_machine_wide`, plus `park_history`'s own app-level tests
  (a park must not Shutdown the run a nudge just started).
- **No process leaks on a foreground command's error paths.** The process group
  is held by the registry for the length of the tool call (`run_shell`'s `hold`),
  every error path calls `job.kill()`, the watcher's four ends all kill, and a
  job that outlives `CMD_DETACH_AFTER` is handed over with its output already
  read. Pins: `a_command_that_runs_forever_is_killed_on_time`,
  `a_running_command_can_be_cancelled`,
  `a_launch_refused_by_the_lock_is_killed_too`,
  `a_command_that_outlives_the_detach_deadline_becomes_a_job`.
- **`park_history` reclaims exactly the read, finished children** past the warm
  window: 12 agent threads → 9 right after `Done`, stable over 78 s; n=20 gives
  root + warm; n=60 reaps 53-eligible. (A5 is the race where one is *not*
  reclaimable.)
- **The in-run `/compact` is sound**: with a held reply and `Compact` + `Nudge`
  queued, the fold happened at the run's message boundary *with the human's
  words inside the summarize request*, one fold for one `/compact`, and the run
  continued and answered (3 asks: run, fold, run). (The *idle* fold is A14.)
- **Hostile model arguments do not panic.** ~110 shapes over all ten tools
  (`Null`, `{}`, wrong types, `u64::MAX`, `-1`, `1.5`, 300-deep nesting, 200 KB
  strings, NUL/ESC paths, `../../etc/passwd`, `#c2`/`C2`/`-1`/`2^64` targets)
  produced an `Err` or a sane answer every time; no `unwrap`/index/`as` on model
  data in `exec_tool` … `summarize` or `jobs.rs` (`preview_tail`'s slice is
  guarded by `chars.len() <= 400`, `short_revision` uses `get`). Note A7: the
  wrong *types* did not panic — they were silently defaulted, which is the
  finding.
- **Id spaces.** Draw and retire share one mutex; the agent floor is raised for
  every restored id (`app/mod.rs:743`) and every id git still names; lost numbers
  below a floor are retired; `AgentId`/`JobId` are distinct types and no book
  mixes them. Pins: `a_lost_number_is_the_next_one_handed_out`,
  `a_floor_retires_lost_numbers_below_it`,
  `isolated_ids_names_branches_with_and_without_a_checkout`.
- **The event seam cannot lose or block an actor.** `Ui` stamps a
  `ConversationId`; the UI drops a stale conversation's events and shuts a stale
  `Spawned` child down; the channel is unbounded so `emit` cannot block on a busy
  UI, and a closed channel is harmless (three tests in `events.rs`). The cost of
  that choice is unbounded queueing behind a stuck UI thread — a trade-off, not a
  defect, and the alternative (blocking an actor) is worse.
- **`MAX_AGENTS`, the worktree guard and the id rules are checked before
  anything is created**: before the id draw and before `worktree_add`;
  `git::unlandable` counts only what no sweep would take; ids are reusable only
  before git created anything. (But see the blind spots: `MAX_AGENTS` has no
  test.)
- **Two spellings of one arithmetic are one spelling where it matters.**
  `status_tool` calls `child_listing`; `child_listing` and `wait_digest` both
  read `ActorState::unread`; the tree's `✉` moves only on `ResultRead`, whose
  emit sites are exactly the fresh-delivery sites; `Outcome::digest` is the one
  sentence a listing and a wait share; `end_note` is the one translation table
  for how a command ended; `request` is the one request builder for the run and
  the fold (no second `max_tokens` spelling); `compaction_reply_cap` and
  `fold_request_fits` subtract the same three terms.

## Not verified (and what would settle it)

- **A real endpoint's billing** (A2's money half): the double *send* is proven on
  the wire; whether a vendor charges a request it received but never answered
  needs the billing page of a live account.
- **A20 and the select-mode paint in a live terminal**: the phase path is
  read and the pty was used for A4/A5/A23, but the *paint* of a stuck label was
  not driven frame by frame here.
- **TLS/rustls, redirects and the shipped reply cap against a live vendor**:
  `live_https_tls_handshake`, `live_models_endpoint`,
  `live_endpoint_accepts_the_shipped_reply_cap` are `#[ignore]`d; not run.
- **A22**: a real session restore with jobs outstanding.
- **A21**: a deliberate deletion race (a sibling's `git worktree remove` between
  `live_branch`'s check and `Workspace::new`) with a pty.
- **A8 through a real `App`**: the App has no model seam, so the growth was
  measured on the agent road and composed with the two `App` lines
  (`on_agent(Message) → push_message`, `session_snapshot → chat.transcript`).
- **The `spawn → hold` window in `run_shell`** (`suspected`, unstaged): a
  `kill_all` landing between `Machine::spawn` and `Registry::hold` misses the
  process group, and `Foreground::drop` deliberately does not kill.
- **A5's restart consequence** (a duplicate `#N done:` delivery after a restore)
  — code read only.
- **The full-suite flake class** (H28): the suite was run green once at this
  base; the load-sensitive tests were not stressed.
- **A13's frame cost** under a real `ratatui` loop (measured as parse cost, not
  as a frame).

## Blind spots in the tests

Invariants this area claims with **no** test pinning them — the list a later wave
should write:

- the length bound of any header/status/trailer/chunk-size line (A1);
- an overflowing `usage` sum (A9);
- a fold reply's `usage` reaching the run's line, and `report_usage` on a
  stopped/failed/loop-stopped run (A6);
- a non-fitting fold's effect on the *pane* and the store — the meter's `over`
  band (A8);
- a finished job's unread report against a held machine (only the child twin
  exists) (A3);
- wrong-typed `detach`/`exclusive`/`base` (A7);
- an event for an already-reaped id, and a `ResultRead`-then-`Done` ordering
  (A4, A5 — the latter is pinned as a state machine but not end to end);
- the idle fold with a queued `Nudge`/`ChildDone`/`CommandDone` (A14);
- `MAX_AGENTS` and `MAX_DEPTH` — no test at all (`MAX_AGENTS` appears twice
  outside comments);
- which number is the gap when `LOST_POOL` is full (A17);
- a `status` result fitting `result_cap` (A11); the per-frame cost of a big tool
  call's label (A13);
- a job's line reaching a parked owner's mail (`watch`'s `let _ =
  mailbox.send(…)`, `jobs.rs:1413`, drops it; the record survives in `status`
  while the registry keeps it, so nothing asserts either way);
- a revived agent's job-id space (A22).
