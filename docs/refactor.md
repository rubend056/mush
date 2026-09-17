# mush — refactor plan: seams and owners

> Status: **in progress.** Wave 0 (**Stage 0**: `transcript.rs`, `text.rs`) has
> landed on master, and the delegation-honesty family below (N1, N3–N6) is closed.
> Stages 1–3 are untouched. The plan still says "not yet started" further down
> where it describes the *findings pass* sequencing — that part is history.
>
> Written 2026-09-17 against `d4f80ae` plus the
> in-flight findings pass (`input.rs`, `config.rs`, `git.rs`, `http.rs`,
> `workspace.rs`, `userconfig.rs`, `ui.rs`, `app.rs` all dirty). References are by
> symbol, not line, because that tree was moving while this was written.
>
> This document is the *structural* companion to `docs/findings.md`: findings
> lists what is broken, this lists where the breakage lives and what shape makes
> it stop recurring. It is not a rewrite proposal. Nothing in the design doc's
> §6 architecture (single owner, no async, two crates) changes.

At the time of writing the working tree has changed shape mid-pass: `Focus` is
`{Agents, Chat}`, and there is no editor pane or `Buffer` in `ui.rs`/`app.rs`.
If that is the intended shape, delete the editor-facing rows below (Buffer,
M6 undo); if it is transient, put them back before Stage 0, because the Buffer
extraction in Stage 0 assumes §4 of the design doc is still the target.

---

## 0. TL;DR

Six extractions and four seams, in four stages, no new crates, no new
dependencies:

| # | Move | Out of | Kills the class containing |
|---|---|---|---|
| 1 | `AgentTree` (ids, focus, phases, per-id maps, derived `busy`) | `app.rs` | B1, B5, B6, B10, B11, B14 |
| 2 | `Chat` (root + per-agent transcripts, notices, input, context meter) | `app.rs` | B4, B8, B12, B19, N2 |
| 3 | `ConfigCell` (one owner of endpoint/model/key/window) | `app.rs` + `agent.rs` | B7, A5 |
| 4 | `ToolHost` (the live-buffer rule in one place) | `app.rs` | three dispatchers → one; M3 reuse |
| 5 | `Intent` keymap + parsed slash commands | `app.rs` | B2; help/status drift |
| 6 | core `transcript.rs`, `text.rs`, `ToolName`, git verbs | `agent.rs`, `ui.rs`, `app.rs` | B9, B13, name/path/cap drift |

Four traits, each with one real and one fake impl: `ModelClient`, `Machine`,
`Clock`, `Events` (+ `ToolHost`).

Acceptance test for the whole program: **the default `cargo test` needs no
socket, no subprocess, and no sleep longer than 50 ms; `--ignored` is only for
live endpoints.**

---

## 1. What makes change expensive today

Five causes, each with the evidence in the tree:

1. **`app.rs` is five subsystems in one `impl`.** Event routing, agent-tree
   state, the tool host, buffers/editor, chat, config/persistence, pickers, slash
   commands, key handling, git facts — one `update` reaches into all of them, and
   `App` carries ~20 fields with no owner between them. This is why every UI
   finding is an `App`-level test with a live actor thread (`test_app`).
2. **`agent.rs` is six.** Mailbox/lifecycle, the turn loop, tool dispatch,
   delegation, shell execution, worktree git — plus the pure transcript algebra
   (repair/trim/sanitize/compaction) that belongs in core.
3. **One fact, several writers.** `cfg` vs `cfg_shared` vs the actor's mutex
   clone (B7); ids allocated by `AgentCtx::ids` *and* parsed from branch names in
   `discover_worktrees` (B1); `busy` cached beside phases (B5/B6); the context
   meter updated by hand at each push site (B8); `dirty_screen` written by both
   `app.rs` and `main.rs`. The B-list is almost entirely this one cause.
4. **IO is inline, so it cannot be faked.** The model call is a free function in
   the middle of the run loop; `sh`, process groups and `kill -9 -pgid` are
   inline in `run_shell`; `Instant::now()` is read everywhere; the actor→UI tool
   round trip is a blocking `recv_timeout`. Consequence: four `#[ignore]`d tests
   need `python3` + fixed ports 18731–18734 + polling readiness, shell tests
   spawn real `yes` and `sleep` and assert wall-clock bounds, and `run_loop`
   itself has no test at all.
5. **Pure domain logic lives in the binary and drifts.** Tool semantics exist in
   three dispatchers, tool names in four places, worktree path/branch strings in
   five, truncation with three different meanings, the token heuristic in three.
   M4 (merge) and M6 (undo) have no clean place to land.

The design doc is also stale on this: §1 claims "roughly 5,000 lines… across two
crates"; the real source (excluding `.mush/wt/`) is ~8,900 lines, with
`agent.rs` ~2,500 and `app.rs` ~2,300. §7's "seven crates" predates the
dependency pass.

---

## 2. Target shape

Two crates, one direction of dependency, no new deps.

```
crates/mush-core/src/            crates/mush/src/
  message.rs                       main.rs · tui.rs        CLI, guard, event loop
  transcript.rs   NEW              app/mod.rs              App: state + Msg routing only
  text.rs         NEW              app/tree.rs      NEW    ids, focus, phases, busy
  tools.rs        +ToolName        app/chat.rs      NEW    transcripts, notices, input
  git.rs          +verbs           app/editor.rs    NEW    Buffer, open/save/reload
  config.rs                        app/host.rs      NEW    tool execution on live buffers
  workspace.rs                     app/settings.rs  NEW    ConfigCell + persistence
  session.rs                       app/keys.rs      NEW    key → Intent (pure)
  userconfig.rs                    app/commands.rs  NEW    /slash parse (pure) + apply
  prompt.rs                        agent/mod.rs            actors, mailboxes, lifecycle
                                   agent/run.rs            the turn loop
                                   agent/delegation.rs     spawn/wait/status/control
                                   agent/shell.rs          Machine/Job + report assembly
                                   model.rs         NEW    ModelClient trait + HTTP impl
                                   http.rs                 transport only
                                   ui/mod.rs + view.rs     draw(frame, &Screen)
```

Rules that keep it from becoming a framework:

- A module per **invariant**, not per topic. If a module has no invariant, it is
  a file split, not an abstraction.
- Traits only where a fake is needed. No `Tool` trait per tool, no plugin
  registry, no `EventBus`.
- `Result<_, String>` stays. Newtypes over new error enums.
- The crate boundary does not move. A third crate would be a bigger commitment
  than any problem here.

---

## 3. The six extractions

### 3.1 `AgentTree` — `app/tree.rs`

Owns `agents`, `agent_cursor`, `focused`, and the per-id maps (`agent_tx`,
`agent_msgs`, `agent_cancel`, `agent_stats`) so that "there is a node with id N"
and "there is a mailbox / transcript / cancel flag for N" cannot disagree.

```rust
pub struct AgentTree { nodes: Vec<AgentNode>, index: HashMap<AgentId, usize>, cursor: usize, focused: AgentId, .. }

impl AgentTree {
    fn reserve_ids(&mut self, floor: u64);                       // leftovers raise the floor  (B1)
    fn insert(&mut self, ev: Spawned) -> &mut AgentNode;         // ids validated, not parsed
    fn begin(&mut self, id: AgentId, cancel: Arc<AtomicBool>);   // Running: clears the summary (B14)
    fn activity(&mut self, id: AgentId, label: impl Into<String>); // Status: ignored after Done/Failed (B5)
    fn finish(&mut self, id: AgentId, summary: String);          // Done: always replaces (B14)
    fn fail(&mut self, id: AgentId, error: String);
    fn cancel_requested(&mut self, id: AgentId) -> bool;         // knows when a cancel cannot land (B6)
    fn nudge_failed(&mut self, id: AgentId, why: &str);          // restores the previous phase (B10)
    fn reap(&mut self, gone: &[AgentId]);                        // resets focused/cursor (B11)
    fn busy(&self) -> bool;                                      // derived; no stored flag (B5/B6)
}
```

Newtypes `AgentId(u64)` and `ConversationId(u64)` — `Msg::Agent { conversation,
id, .. }` currently takes two bare `u64`s in an order nobody can remember, and a
branch name and an id are the same type.

### 3.2 `Chat` — `app/chat.rs`

Owns the root transcript, the per-agent transcripts, notices, the input line and
scroll, and the context meter **derived on read** rather than incremented at
every push site.

- `used_tokens()` computes from `system + transcript`; nothing to forget (B8).
- `visible_lines(height)` trims trailing blank separators before windowing (B4).
- Notices are typed and agent-scoped, so a root failure does not appear in a
  child's pane (B19) and `Error` outranks derived activity in one place (B12).
- `Input` (already extracted as `input.rs`) keeps its grapheme cursor here (N2).

### 3.3 `ConfigCell` — `app/settings.rs`

One owner for provider/model/url/key/window, shared with the actors:

```rust
pub struct ConfigCell { ui: Config, shared: Arc<Mutex<Config>> }
impl ConfigCell {
    fn edit(&mut self, f: impl FnOnce(&mut Config));   // writes both sides
    fn learn_context(&mut self, tokens: usize, source: &str) -> bool; // emits an event, never a silent mutex write
    fn handle(&self) -> ConfigHandle;                  // what an actor is given
}
```

B7 is "the UI and the actor each have a `Config` and only one of them learns".
With a cell, `learn_context` is the only mutator and it goes through one path;
"the UI shows a window the actor does not have" stops being representable.

### 3.4 `ToolHost` — `app/host.rs`

The live-buffer rule (design doc §2) as one function:

```rust
impl ToolHost for App {
    fn call(&mut self, name: ToolName, args: &Value) -> Result<String, String>;
}
```

`list_files` / `read_file` / `write_file` / `edit_file` over buffers + workspace,
plus the `Msg::Tool` reply plumbing. Today this logic is triplicated between
`agent::exec_tool`, `agent::direct_tool` and `app::exec_tool`; after the move
there is one dispatcher per *side* (UI vs worktree) and one tool-name table. The
same trait is what M3's socket server should call, so an external agent and a
built-in one cannot get different semantics.

### 3.5 `Intent` keys and parsed commands — `app/keys.rs`, `app/commands.rs`

- `fn key(focus, mode, picker_open, key) -> Intent` is pure; `App::update`
  applies intents. No more side effects hidden in match arms (the `mask_key`
  class, B2), and key handling is testable without an `App`.
- `fn parse_command(&str) -> Command` is pure and exhaustive; the help text and
  the status hint render from the same table, so `main.rs`'s help cannot drift
  from `run_command`'s arms.

### 3.6 Core modules — `transcript.rs`, `text.rs`, `ToolName`, git verbs

| Move | From | Notes |
|---|---|---|
| `trim_history`, `repair_tool_pairs`, `sanitize_tool_calls`, `COMPACT_INSTRUCTION`, the compaction trigger and the weight/budget arithmetic | `agent.rs` | all pure; tests move with them; core already owns `history_budget` |
| `wrap_text`, `truncate`, `slice_columns`, `fit_row`, `display_column`, `mask_secret` | `ui.rs`, `app.rs` | one meaning of "width" for cursor, wrap, rows and masks (B9) |
| `enum ToolName { ListFiles, ReadFile, WriteFile, EditFile, RunCommand, SpawnAgent, WaitAgents, AgentStatus, AgentControl }` with `as_str`/`parse`; `TOOL_NAMES` derived; schemas keyed by it; dispatch matches on it | `tools.rs`, `prompt.rs`, `agent.rs`, `app.rs` | adding a tool becomes a compile error in every place that must know |
| one `Git` value with `run`/`status`/`branch_stat` + `worktree_add/list/remove/commit` | `git.rs`, `agent.rs` (`create_worktree`, `commit_worktree`, `git_output`), `app.rs` (`discover_worktrees`) | one invocation style instead of three; `.mush/wt/{id}` and `mush/{id}` formatted in one place |

`Buffer` itself goes to `app/editor.rs` rather than core: core is about files and
messages, the binary is about this session's UI. (If the editor does not come
back, skip it.)

---

## 4. The four seams

Each is a trait with exactly two impls — the real one and a small `#[cfg(test)]`
fake. Boxed once at construction (`Arc<dyn …>` where threads are involved);
no generics threading through the actor.

| Seam | Signature (sketch) | Fake | Unlocks |
|---|---|---|---|
| `ModelClient` | `fn chat(&self, req: &ChatRequest, cancel: &AtomicBool) -> Result<ChatResponse, ModelError>` | scripted reply queue, including errors and cancellation | `run_loop`, compaction, the learned-context retry, cancel mid-reply, `MAX_TURNS` wrap-up (N1) — all in-process |
| `Machine` + `Job` | `fn spawn(&self, cmd: &ShellCommand) -> Result<Box<dyn Job>, String>`; `Job::{poll, output, kill}` | scripted end states, output sizes, elapsed time | timeout, cancel, output cap, and M2.8's detach/exclusive lock without `sh`, `yes` or sleeps |
| `Clock` | `fn now(&self) -> Instant; fn sleep(&self, d: Duration)` | advanceable | `wait_tool`'s 50 ms poll, `wait_bounded`'s 10 ms poll, `Watch`'s deadline, `INFO_TTL` ageing |
| `Events` | `fn emit(&self, id: AgentId, event: AgentEvent)` | recording sink | exactly-once completion delivery, fan-out refusal, dispatch, cancel mid-batch — asserted, instead of `mem::forget(ui_rx)` |

`ModelClient` is also where the retry/parse logic around `post_json` moves, so
`http.rs` keeps doing only what it is good at: framing, slices, caps, TLS.

Why these four and not more: every remaining boundary (workspace, config,
messages, prompt) is already pure or already has real-file tests that are cheap
and honest. Adding traits there would buy nothing and cost indirection.

---

## 5. Order of work

Rules: **a commit is either a move or a behavior change plus its test, never
both**; every stage ends green on the existing gate (`cargo fmt --all --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test`, the pty resize and
cancel scenarios).

**Stage 0 — moves only.** `transcript.rs`, then `text.rs`, `ToolName`, git verbs.
~700 lines leave `agent.rs`, ~250 leave `ui.rs`/`app.rs`, no behavior change, tests move
with the code. *Done when* the new modules' tests are the old tests and the gate
is green with no assertion edits.

**Stage 1 — one owner per fact.** `AgentTree`, then `Chat`, then `ConfigCell`,
each landing with the transition rules as unit tests, including a regression per
finding in its row of §6. *Done when* nothing outside `tree.rs` writes
`node.phase`, `node.summary`, `focused` or `agent_cursor`, `busy` is a method,
and the context meter is a method.

**Stage 2 — the seams.** Rewrite the four `#[ignore]`d actor tests in-process and
delete `scripts/mock_llm.py` from the test path (keep it for the pty smoke
scenarios if they still want a scripted model). *Done when* the default suite
needs no socket, no subprocess and no sleep over 50 ms, and `--ignored` contains
only live-endpoint tests.

**Stage 3 — `Screen` view and intents.** `ui::draw(frame, &Screen)` where
`Screen` is built by `App::screen()`; panes become pure functions of a value, so
the draw sweep can assert painted text at every size instead of only "does not
panic" (B17), and keys go through `Intent`. *Done when* no render function takes
`&App`.

**Stage 4 — roadmap.** M2.8 = a job registry + `Machine`; M3 = the socket server
over `ToolHost`; M4 = a base revision on `Buffer` + merge in core `text.rs`; M6 =
undo inside `Buffer`. None of them should need to touch `app.rs`'s routing.

Sequencing with the concurrent findings pass: Stage 0.1 (`transcript.rs`)
touches only `agent.rs` + `mush-core`, so it can start immediately. The stages
that touch `ui.rs`/`app.rs` should wait until that pass lands.

---

## 6. Checklist: finding → disposition → structural home

Status at the time of writing: ✅ verified closed in the working tree, 🔄 the
concurrent pass is in those files, ⬜ open or unverified. The findings doc stays
the queue of record; this column says where the *fix belongs*.

> **Drift:** `docs/findings.md` — the "queue of record" this section defers to —
> is not in the tree (only `docs/mush.md` and this file are). Rows N3–N6 and A20
> below were therefore recorded here, against this table, because there was
> nowhere else for them to go. Either restore `findings.md` or stop pointing at
> it.

| ID | What | Status | Structural home |
|---|---|---|---|
| A1 | cancel/deadline only consulted on read timeout | ✅ | http `Watch` (regression test) |
| A2 | `tokens * 3` overflow | ✅ | `config::clamp_context` |
| A3 | `parse_context_hint` misfires / misses | ✅ | `config` (markers + range) |
| A4 | caps as floors; reserve > window | ✅ | `config::{read_cap, cmd_cap, list_limit, history_budget}` |
| A5 | window derived once; runtime switches never re-derive | 🔄 | `ConfigCell` (3.3) |
| A6 | CLI provider never selects its endpoint | ✅ | `config::resolve_with` test |
| A7 | no cap on response body | ✅ | `http` `MAX_BODY_BYTES` |
| A8 | localized git output parses as ±0 | ✅ | `git()` sets `LC_ALL=C` |
| A9 | model discovery runs even when a model is known | ⬜ | `main.rs` — skip when known, or fetch after first paint |
| A10 | branch name read as an argv option | ✅ | `git` ref→sha guard |
| A11 | `adopt_context` accepts 1 | ✅ | `config::adopt_context` clamp |
| A12 | `Stat` is `u32`, git counts are 64-bit | 🔄 | `git::Stat` widen |
| A13 | `--` does not stop flag parsing | ✅ | `main::parse_args` |
| A14 | a second positional silently replaces the first | ⬜ | `main::parse_args` — error |
| A15 | unparsable `Content-Length` treated as absent | ✅ | `http` `InvalidData` |
| A16 | bad `MUSH_CONTEXT` silently ignored | ✅ | `config::parse_context_env` |
| A17 | `openai` alias sends the key to the LAN default | ✅ | `Provider::parse` (aliases removed) |
| A18 | git test hardcodes `master` | ✅ | `git` test `init_repo` |
| A19 | DNS resolution unbounded | ⬜ | `Clock` seam + `http::connect` |
| B1 | leftover worktree id collides with a fresh child | ⬜ | `AgentTree::reserve_ids` (3.1) |
| B2 | `mask_key` slices on a byte boundary | ✅ | core `text::mask_secret` (3.6) |
| B3 | zero-row pane still focusable | 🔄 | `Screen`/layout tiers (Stage 3) |
| B4 | at 40×10 the only transcript row is a blank | ✅ | `Chat::visible_lines` (3.2) |
| B5 | `Status` after `Done` restarts a finished agent | ⬜ | `AgentTree::activity` (3.1) |
| B6 | failed/idle `Stop` leaves `Cancelling` + `busy` stuck | ⬜ | `AgentTree::cancel_requested` + a `Stop` ack from the actor |
| B7 | learned window never reaches the UI, then is clobbered | 🔄 | `ConfigCell` (3.3) |
| B8 | context meter ignores the human's own message | ⬜ | `Chat::used_tokens` derived (3.2) |
| B9 | `fit_row` budgets columns, `truncate` counts chars | ✅ | core `text` (3.6) |
| B10 | a failed nudge rewrites the node's phase | ✅ | `AgentTree::nudge_failed` |
| B11 | reaping a leftover leaves `focused` on a ghost | ⬜ | `AgentTree::reap` (3.1) |
| B12 | an `Error` status loses to the activity line | ✅ | one precedence table (3.2) |
| B13 | a child's brief is never shown | ✅ | `Spawned` pushes the opening message |
| B14 | the row's summary is from the first run, forever | ✅ | `AgentTree::{begin, finish}` |
| B15 | `screen.py` mis-reads CSI / `--keys` escapes | 🔄 | `scripts/screen.py` |
| B16 | `smoke.py --cancel` forks after starting a thread | 🔄 | `scripts/smoke.py` |
| B17 | the layout sweep never exercises an open file | 🔄 | `Screen` sweep (Stage 3) |
| B18 | `~` elision matches a prefix, not a directory | ✅ | `ui::facts_line` |
| B19 | global notices render into every transcript | ⬜ | agent-scoped notices (3.2) |
| N1 | `MAX_TURNS` turns "long" into "failed" | ✅ | `agent/run.rs`: `RUNAWAY_TURNS` + `LOOP_ROUNDS` (a run ends when it stops calling tools; only a *loop* ends it early) |
| N2 | message box is append-only and clips at the right edge | 🔄 | `app/input.rs` (already extracted) |
| N3 | a stopped child is reported to its parent as `#N done: cancelled` | ✅ | `agent::Outcome` (one enum, not a `summary == CANCELLED` string sentinel) |
| N4 | Ctrl-C stopped *every* busy agent, and blanked a stopped one to `Idle` | ✅ | `App::interrupt` (focused) + `Ctrl-X` (`interrupt_all`); `Phase::Stopped` |
| N5 | the one-non-isolated-sibling rule fails only *after* the brief is written | ✅ | `spawn_tool` message + the rule stated in `prompt` schemas and the system prompt |
| N6 | an interrupted run commits under the same subject as a finished one | ✅ | `commit_worktree` subject carries the `Outcome` |
| A20 | `needs_compaction` compares byte weights against a *token* budget, and `budget * 3 / 4` can overflow `usize` | ⬜ | `mush-core::transcript`; same shape as A2 (`clamp_context`) |

---

## 7. Design-doc sync

When this lands, `docs/mush.md` wants:

- §1: the line count (~8,900, not ~5,000) and the real file sizes.
- §6: the module tree from §2 of this plan.
- §7: the dependency table after the dependency pass, plus the rule that a new
  crate must *replace* hand-rolled code that drifted, not sit beside it.
- §9: **M2.75 — Seams** before M2.8, with the stage list from §5 and the
  acceptance test from §0.
- §10: replace "deterministic orchestration needs `python3` and `git`" with "the
  default suite needs neither a socket, a subprocess, nor a sleep", once Stage 2
  is done.
- §12: two decisions — "one owner per fact" and "a trait is justified only by a
  fake that a test actually uses".

Housekeeping: `.mush/wt/2` was still registered, and `mush/2` pointed at `d4f80ae`
= master with a clean worktree, so `/discard 2` lost nothing. **Done**: wave 0
removed it along with every other leftover worktree and branch, so `find`, grep
and dependency audits no longer count the source twice. The working tree now has
exactly one worktree (the main checkout).

---

## 8. Not doing

- **No third crate.** The pure/binary split is right; the problem was never the
  crate boundary.
- **No `Tool` trait / plugin registry.** Nine tools with a name enum is the
  honest shape.
- **No Elm-style `update -> Vec<Effect>` rewrite.** `AgentTree`/`Chat`/`ConfigCell`
  plus four seams give the same testability at a fraction of the churn.
- **No replacing `http.rs`.** Its slice-and-cancel behaviour is load-bearing
  (`f1d0f2a`), and the request/response boundary is already testable over a real
  loopback socket.
- **No new dependencies.** The seams are traits; nothing here needs a crate.

---

## 9. Open questions

1. Does the editor pane come back? Stage 0's `Buffer`/`text` split and Stage 4's
   undo work assume it does; if `mush` is becoming agents+chat, the plan loses
   that row and §1's diagnosis shrinks by one cluster.
2. Is `dirty_screen` moved into a `Screen`/`Redraw` type (Stage 3), or does
   `update` return a `Changed` bool? The former is more honest; the latter is
   smaller.
3. Should `AgentTree` own `agent_stats` (git facts) too, or does the git snapshot
   stay a separate value refreshed by events? Today the coupling is one-way
   (`refresh_git` reads the tree), so it probably stays separate.
