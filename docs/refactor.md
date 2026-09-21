# mush — refactor plan: seams and owners

> Status: **Stages 0, 1, 2, 3 and 3.5 complete.** On master: Stage 0 (all four
> moves), `AgentTree` (`app/tree.rs`), `Chat` (`app/chat.rs`), `ConfigCell`
> (`app/settings.rs`), the four seams (`ModelClient`, `Machine`, `Clock`,
> `Events`), the `#[ignore]`d actor tests rewritten in process, Stage 3.5
> (`Intent` + parsed commands, `app/keys.rs` + `app/commands.rs`), and Stage 3's
> other half — the `Screen` value and the draw sweep that asserts painted text
> (`app/screen.rs` + `ui.rs`, B17, `7e123e1`). Stage 3.4 (`ToolHost`) was
> dropped with the editor: there is one dispatcher per side, not three. The
> delegation-honesty family (N1, N3–N6) is closed, and so is the findings wave
> whose rows §6 carries (`findings.md` U1–U11, B20–B23).
>
> Written 2026-09-17 against `d4f80ae` plus the
> in-flight findings pass (`input.rs`, `config.rs`, `git.rs`, `http.rs`,
> `workspace.rs`, `userconfig.rs`, `ui.rs`, `app.rs` all dirty). References are by
> symbol, not line, because that tree was moving while this was written. The
> "Landed" notes under §§3–5 record where the tree ended up differing from the
> sketches, so a reader can trust the code over the plan.
>
> This document is the *structural* companion to `docs/findings.md`: findings
> lists what is broken, this lists where the breakage lives and what shape makes
> it stop recurring. It is not a rewrite proposal. Nothing in the design doc's
> §6 architecture (single owner, no async, two crates) changes.

At the time of writing the working tree had changed shape mid-pass: `Focus` is
`{Agents, Chat}`, and there is no editor pane or `Buffer` in `ui.rs`/`app.rs`.
That was the intended shape, and the editor-facing rows were dropped with it:
there is no `Buffer` extraction, no `app/editor.rs`, and no `ToolHost` — a
mush that does not open files has nothing for the live-buffer rule to own, and
the tool dispatch is one function (`agent::exec_tool`, which owns every tool —
the twelve-to-six cut of §3.4 left no file-tool side for a second dispatcher to
hold).

---

## 0. TL;DR

Six extractions and four seams, in four stages, no new crates, no new
dependencies:

| # | Move | Out of | Kills the class containing |
|---|---|---|---|
| 1 | `AgentTree` (ids, focus, phases, per-id maps, derived `busy`) | `app.rs` | B1, B5, B6, B10, B11, B14 |
| 2 | `Chat` (root + per-agent transcripts, notices, input, context meter) | `app.rs` | B4, B8, B12, B19, N2 |
| 3 | `ConfigCell` (one owner of endpoint/model/key/window) | `app.rs` + `agent.rs` | B7, A5 |
| 4 | ~~`ToolHost`~~ (dropped with the editor) | `app.rs` | — |
| 5 | `Intent` keymap + parsed slash commands | `app.rs` | B2; help/status drift |
| 6 | core `transcript.rs`, `text.rs`, `ToolName`, git verbs | `agent.rs`, `ui.rs`, `app.rs` | B9, B13, name/path/cap drift |

Four traits, each with one real and one fake impl: `ModelClient`, `Machine`,
`Clock`, `Events`. (`ToolHost` would have been a fifth, but the editor it existed
for is gone.)

Acceptance test for the whole program: **the default `cargo test` needs no
socket, no subprocess, and no sleep longer than 50 ms; `--ignored` is only for
live endpoints.**

---

## 1. What made change expensive (the diagnosis, then)

Five causes, each with the evidence in the tree as it stood at `d4f80ae`. They
are kept in the present tense because they are what motivated the moves; §0 and
the "Landed" notes below say which of them the extractions actually removed.

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

The design doc was also stale on this: §1 claimed "roughly 5,000 lines… across
two crates"; the real source at the time (excluding `.mush/wt/`) was ~8,900
lines, with `agent.rs` ~2,500 and `app.rs` ~2,300. (The tree is ~32,000 lines
now, and `app.rs` is `app/mod.rs` plus its modules.) §7's "seven crates"
predates the dependency pass.

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

What is actually there, and what is not. All of `mush-core`'s list landed, plus
`transcript.rs` and `text.rs`; on the binary side `app/{mod,tree,chat,settings,
keys,commands}.rs`, `model.rs`, `machine.rs`, `clock.rs`, `events.rs`,
`session_save.rs` and `jobs.rs` all exist. What did **not** land, and is now
ruled out rather than pending: `app/editor.rs` and `app/host.rs` (the editor was
dropped, so there is no `Buffer` and no `ToolHost` — dispatch is one function per
side), the `agent/` split (`agent.rs` is still one file), `tui.rs` (`main.rs`
holds the event loop), and the `ui/mod.rs + view.rs` split (`Screen` landed in
`app/screen.rs` instead, `7e123e1`).

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

**Landed (Stage 1.3).** `ConfigCell` holds the UI copy and an `Arc<Mutex<Config>>`
shared with every actor in the tree, so the two sides cannot disagree; the file
is `crates/mush/src/app/settings.rs` and one `ConfigHandle` travels to the
agents. One owner for provider/model/url/key/window, shared with the actors:

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

### 3.4 `ToolHost` — `app/host.rs` — **dropped**

The live-buffer rule (design doc §2) as one function:

```rust
impl ToolHost for App {
    fn call(&mut self, name: ToolName, args: &Value) -> Result<String, String>;
}
```

`list_files` / `read_file` / `write_file` / `edit_file` over buffers + workspace,
plus the `Msg::Tool` reply plumbing. This was written when the logic was
triplicated between `agent::exec_tool`, `agent::direct_tool` and
`app::exec_tool`. The editor was then dropped, so `app::exec_tool` went with it
and there is no live-buffer rule left to own: there is now **one dispatcher**
(`agent::exec_tool`, which owns every tool) and no `ToolHost` trait. M3's socket
server, if it lands, calls it directly rather than a buffer host. The twelve-to-six tool cut went further than the editor's departure:
`list_files`, `read_file` and `write_file` are gone, because the shell lists,
reads and writes better than a bespoke tool could — so `edit_file` is the only
file tool left and the signature above is history twice over. That cut was
reversed in turn (`docs/findings.md` §8.36): a machine lock refuses *every*
`run_command`, reads included, and a shell cannot carry an image, so `read_file`,
`write_file`, `list_files` and `search` came back beside `edit_file` — still one
dispatcher, still no `ToolHost`.

### 3.5 `Intent` keys and parsed commands — `app/keys.rs`, `app/commands.rs`

**Landed.** `keys::key(focus, picker_open, key) -> Intent` is pure and calls no
side effect of its own; `App::apply_intent` is the only thing that carries an
intent out, and nothing below `App::on_key` reads a `KeyCode`, so a binding is
testable without an `App` (the `mask_key` class, B2). The sketch's `mode`
argument is not there: the only modal state the keyboard has is the picker, held
as a bool, and the editor that had insert and normal modes is gone.
`commands::parse_command(&str) -> Result<Command, CommandError>` is pure, and
both help surfaces — `mush --help`'s KEYS/COMMANDS blocks and the in-app
`/help` notice — render from `keys::KEYS` and `commands::COMMANDS`, so neither
can drift from what the parser accepts.

### 3.6 Core modules — `transcript.rs`, `text.rs`, `ToolName`, git verbs

| Move | From | Notes |
|---|---|---|
| `trim_history`, `repair_tool_pairs`, `sanitize_tool_calls`, `COMPACT_INSTRUCTION`, the compaction trigger and the weight/budget arithmetic | `agent.rs` | all pure; tests move with them; core already owns `history_budget` |
| `wrap_text`, `truncate`, `slice_columns`, `fit_row`, `display_column`, `mask_key` | `ui.rs`, `app.rs` | one meaning of "width" for cursor, wrap, rows and masks (B9) |
| `enum ToolName { ListFiles, ReadFile, WriteFile, EditFile, RunCommand, SpawnAgent, WaitAgents, AgentStatus, AgentControl }` with `as_str`/`parse`; `TOOL_NAMES` derived; schemas keyed by it; dispatch matches on it | `tools.rs`, `prompt.rs`, `agent.rs`, `app.rs` | adding a tool becomes a compile error in every place that must know |
| one `Git` value with `run`/`status`/`branch_stat` + `worktree_add/list/remove/commit` | `git.rs`, `agent.rs` (`create_worktree`, `commit_worktree`, `git_output`), `app.rs` (`discover_worktrees`) | one invocation style instead of three; `.mush/wt/{id}` and `mush/{id}` formatted in one place |

`Buffer` itself would have gone to `app/editor.rs` rather than core: core is about
files and messages, the binary is about this session's UI. The editor did not
come back, so there is no `Buffer` and no `app/editor.rs` — skip it.

**Landed (Stage 0).** Three notes where the tree ended up differing from the
table above, so Stage 1 starts from what is really there:

- `ToolName` is real: `tools::ToolName::{as_str, parse, ALL, ORCHESTRATION}`,
  with `TOOL_NAMES` derived from `ALL` and `prompt::tool_schemas` keyed by the
  variant. `TOOL_NAMES` was *not* made a compile-time error to extend — the
  schema table's own test is what keeps a new tool from being forgotten.
- The git verbs are module-level functions in `git.rs` (`WORKTREE_DIR`,
  `worktree_path`, `branch_name`, `worktree_id`, `Worktree` + `worktrees`/
  `parse_worktrees`, `has_commits`, `worktree_add`, `commit_all`), not a `Git`
  newtype: every caller already names a directory first, so a struct would hold
  one field and add ceremony, not an invariant. `git::run` is the one mutating
  invocation style and `git` (private) the one read-only one; `agent.rs`'s
  `git_output` copy is gone. The subject of an isolated agent's commit is still
  built in `agent.rs` (`commit_subject`), because that is where the id, brief and
  outcome are.
- `app.rs` has no `exec_tool` and no editor pane, so 3.4's "triplicated" is now
  one dispatcher (`agent::exec_tool`, which owns every tool) — and with
  `ToolHost` dropped that one is the whole story. The twelve-to-six tool cut
  folded the file-tool dispatcher into it too, by removing three of the four
  file tools.

---

## 4. The four seams

Each is a trait with exactly two impls — the real one and a small `#[cfg(test)]`
fake. Boxed once at construction (`Arc<dyn …>` where threads are involved);
no generics threading through the actor.

| Seam | Signature (sketch) | Fake | Unlocks |
|---|---|---|---|
| `ModelClient` | `fn chat(&self, req: &ChatRequest, cancel: &AtomicBool) -> Result<ChatResponse, ModelError>` | scripted reply queue, including errors and cancellation | `run_loop`, compaction, the learned-context retry, cancel mid-reply, `MAX_TURNS` wrap-up (N1) — all in-process |
| `Machine` + `Job` | `fn spawn(&self, cmd: &ShellCommand) -> Result<Box<dyn Job>, String>`; `Job::{poll, written, output, kill}` | scripted end states, output sizes, kills | timeout, cancel, output cap, and M2.8's detach/exclusive lock without `sh`, `yes` or sleeps — landed in 2.3, with the timeout still decided by the watcher in `agent.rs` |
| `Clock` | `fn now(&self) -> Instant; fn sleep(&self, d: Duration)` | advanceable by hand, `sleep` returns at once | `wait_tool`'s 50 ms poll, `wait_bounded`'s 10 ms poll, `Watch`'s deadline — landed in 2.3; `INFO_TTL` ageing still reads the wall clock in `app/mod.rs` |
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

**Stage 0 — moves only.** ✅ 
`transcript.rs`, then `text.rs`, `ToolName`, git verbs. ~700 lines left `agent.rs`,
~250 left `ui.rs`/`app.rs`, no behavior change, tests moved with the code. *Done
when* the new modules' tests are the old tests and the gate is green with no
assertion edits — both held (the two later commits added tests; no existing
assertion was edited).

**Stage 1 — one owner per fact.** ✅ 1.1 `AgentTree`, 1.2 `Chat`, 1.3
`ConfigCell` (`app/settings.rs`). *Done when* nothing outside `tree.rs` writes
`node.phase`, `node.summary`, `focused` or `agent_cursor`, `busy` is a method,
and the context meter is a method — all three hold, and §3.3's cell landed with
them (its finding was B7; A5 turned out to be closed already: `rederive_context`
runs on every runtime switch).

**Stage 2 — the seams.** ✅ Rewrite the `#[ignore]`d actor tests in-process and
delete `scripts/mock_llm.py` from the test path (keep it for the pty smoke
scenarios if they still want a scripted model). *Done when* the default suite
needs no socket, no subprocess and no sleep over 50 ms, and `--ignored` contains
only live-endpoint tests — held: the actor scenarios it used to hold (five, not
the four this line first claimed; more have grown beside them since) run in the
default suite, and `--ignored` is now exactly the three live-endpoint tests in
`http.rs` (the model list, the reply cap, and the TLS handshake).

**Stage 2.1 — `ModelClient`.** ✅ `crates/mush/src/model.rs` holds one trait
(`chat(&ChatRequest, &AtomicBool) -> Result<ChatResponse, ModelError>`), the
real `HttpModel` over `http::post_json` (body encoding, and the classification
of a cancellation, a refusal, a transport failure, a status the endpoint chose
and an unreadable body), and a `#[cfg(test)]` `fake::Scripted` — a reply queue
that also scripts a refusal and a cancellation, and records the requests it was
given. `AgentCtx::model` carries it, children inherit it, so one client serves a
whole tree (and one scripted client can too). `run_loop` and `compact_history`
go through it with every branch and every error string unchanged; `http.rs` is
untouched. Three in-process tests: a scripted run that runs its tool call and
ends with the answer, the learned-context retry, and a cancellation mid-reply.

**Stage 2.2 — the `#[ignore]`d actor tests.** ✅ All five now run in process on
`fake::Scripted`. A scripted reply may say which request it answers (the depth
in the system prompt picks the asker, what the transcript holds picks the turn),
because a tree's actors ask concurrently, and may be held open until the test
releases it — which is what makes "the nudge arrived while the reply was in
flight" and "the parent's turn ended before its child finished" facts rather
than races. The work stayed real: git worktrees, files, the commit, the merge
and the discard the UI advertises, and one file write per turn in the
turn-limit scenario. The tests now wait on the `Done`/`Compact`/`Message` events
they assert on instead of polling for a file, and each one asserts what it
pinned before — the child's work on `mush/1` in `.mush/wt/1`, `mush/2` based on
`mush/1`, the folded summary as the next request's only user message, the nudge
in the second request, a wrap-up summary rather than a bare failure. Gone with
them: `start_mock*`, `stop_mock`, the four port constants (18731–18735), the
`python3` readiness probe, and every sleep over 20 ms in these tests.
`scripts/mock_llm.py` stays in the tree for the pty smoke scenarios; no test
refers to it. The only surface the production code grew is `#[cfg(test)]`:
`agent::spawn_scripted`, which starts the same root actor over a caller-supplied
client.

**Stage 2.3 — `Machine` + `Job`, `Clock`, `Events`.** ✅ The last three seams of
§4, plus B6. `machine.rs` holds `Machine::spawn(&ShellCommand) -> Box<dyn Job>`
and `Job::{poll, written, output, kill}`; the real impl is the shell as it
always was (own process group, scratch files, `kill -9 -pgid`), and `Scratch`
and `kill_command` moved into it. `clock.rs` holds `Clock::{now, sleep}`, the
system impl, and an advanceable fake; `wait_tool`'s 50 ms poll, `wait_bounded`'s
10 ms poll and `Watch`'s deadline all read it through `AgentCtx`. `events.rs`
holds `Events::emit(id, event)`: the real sink is the UI channel with the
conversation stamped on, and the fake records — which is what retires
`mem::forget(ui_rx)` from the actor tests. B6 is closed on both ends: only a run
in flight is marked `⊘ cancelling…` (an idle, done or failed agent keeps its
phase and its `busy` flag stays down), and a `Stop` that arrives behind the
`Run` it was aimed at is no longer folded away by the wait that follows the
`Run`. Four of the five shell tests are now driven by the fake machine and
clock, and so are `wait`'s timeout and `Watch`'s deadline; the background
job, the real output cap and the real `git` stay, each with the reason in
place. Still on the wall clock: `INFO_TTL`/status ageing in `app/mod.rs` (whose
commit is another agent's) and `AgentTree`'s stale-cancel window, which already
has `age` for tests. The default suite still opens local mock sockets in
`http.rs`.

**Stage 3 — `Screen` view and intents.** ✅ Stage 3.5 — keys go through `Intent`
(`app/keys.rs`), and slash commands are parsed values (`app/commands.rs`), with
both help surfaces rendered from the one table (§3.5). Stage 3 — `App::screen(&self,
area) -> Screen` (`app/screen.rs`) now derives every painted value (tiers, pane
rects, rows, words, ranks, the picker's window) and `ui::draw(frame, &Screen)`
paints it, so `ui.rs` is 298 lines of column arithmetic and **no render function
takes `&App`**; the draw sweep asserts the painted text over fifteen sizes ×
fourteen states (B17, `7e123e1`).

**Stage 4 — roadmap.** M2.8 = a job registry + `Machine`; M3 = the socket server
over the two dispatchers; M4 = a base revision on `Buffer` + merge in core
`text.rs`; M6 = undo inside `Buffer`. None of them should need to touch `app.rs`'s
routing. (M2.8 landed: `jobs.rs` sits on the `Machine` seam, and M4/M6's `Buffer`
is gone with the editor.)

Sequencing with the concurrent findings pass: Stage 0.1 (`transcript.rs`)
touches only `agent.rs` + `mush-core`, so it can start immediately. The stages
that touch `ui.rs`/`app.rs` should wait until that pass lands.

---

## 6. Checklist: finding → structural home

Every row below is closed, so the Status column this table used to carry is
gone. **This table is their status home**: `docs/findings.md` does not re-list
these `A`/`B`/`N` rows — it carries its own `U`/`B`/`H`/`A`/`S`/`V` series and
defers to this plan for the structural queue. What this table keeps is the
structural half: where the fix belongs, so a later change to that same
invariant knows its home. The `A1`–`A8` here are the starting audit's, not
`findings.md` §6's attach rows of the same letters.

| ID | What | Structural home |
|---|---|---|
| A1 | cancel/deadline only consulted on read timeout | http `Watch` (regression test) |
| A2 | `tokens * 3` overflow | `config::clamp_context` |
| A3 | `parse_context_hint` misfires / misses | `config` (markers + range) |
| A4 | caps as floors; reserve > window | `config::{cmd_cap, history_budget}` (the read and listing caps went with the file tools) |
| A5 | window derived once; runtime switches never re-derive | `git::rederive_context` on every runtime switch (`/url`, `/provider`, `set_model`, `set_base_url`), pinned in `config.rs` |
| A6 | CLI provider never selects its endpoint | `config::resolve_with` test |
| A7 | no cap on response body | `http` `MAX_BODY_BYTES` |
| A8 | localized git output parses as ±0 | `git()` sets `LC_ALL=C` |
| A9 | model discovery runs even when a model is known | `main.rs`: discovery is `config.model.is_empty().then(…)`, so a named model skips it, and it runs on `/model`, `/models` and `/url` |
| A10 | branch name read as an argv option | `git` ref→sha guard |
| A11 | `adopt_context` accepts 1 | `config::adopt_context` clamp |
| A12 | `Stat` is `u32`, git counts are 64-bit | `git::Stat` is `u64` (`a_huge_diff_keeps_its_count`) |
| A13 | `--` does not stop flag parsing | `main::parse_args` |
| A14 | a second positional silently replaces the first | `main::set_dir` errors on a second directory (`only one directory may be given`) |
| A15 | unparsable `Content-Length` treated as absent | `http` `InvalidData` |
| A16 | bad `MUSH_CONTEXT` silently ignored | `config::parse_context_env` |
| A17 | `openai` alias sends the key to the LAN default | `Provider::parse` (aliases removed) |
| A18 | git test hardcodes `master` | `git` test `init_repo` |
| A19 | DNS resolution unbounded | `http::resolve_bounded` runs the lookup on its own thread and bounds the wait at 10 s on the `Clock`, so a hung resolver is a `TimedOut` naming the host |
| B1 | leftover worktree id collides with a fresh child | `AgentTree::reserve_ids` — raised from the leftover scan (`discover_worktrees`), pinned in `tree.rs` |
| B2 | `mask_key` slices on a byte boundary | core `text::mask_key` (3.6) |
| B3 | zero-row pane still focusable | `App::below_floor` (`app/mod.rs`) refuses every intent but `Quit`, and `ui::draw` paints one notice below `min` — one `is_below_floor` predicate both read |
| B4 | at 40×10 the only transcript row is a blank | `Chat::body` trims trailing blank separators before windowing (`trim_trailing_blanks`) |
| B5 | `Status` after `Done` restarts a finished agent | `AgentTree::activity` ignores it after `Done`/`Failed`, and `busy` is derived from the phases (`tree.rs`) |
| B6 | failed/idle `Stop` leaves `Cancelling` + `busy` stuck | `AgentTree::cancel_requested` (only a run in flight is marked) + the actor's end-of-run event as the ack |
| B7 | learned window never reaches the UI, then is clobbered | `app/settings.rs` — `ConfigCell` + `ConfigHandle`; `learn_context` is the one mutator and it writes both sides |
| B8 | context meter ignores the human's own message | `Chat::used_tokens_for` derives it on read, per conversation |
| B9 | `fit_row` budgets columns, `truncate` counts chars | core `text` (3.6) |
| B10 | a failed nudge rewrites the node's phase | `AgentTree::nudge_failed` |
| B11 | reaping a leftover leaves `focused` on a ghost | `AgentTree::reap` + `repair_focus`; `discover_worktrees` reaps through it |
| B12 | an `Error` status loses to the activity line | one precedence table (3.2) |
| B13 | a child's brief is never shown | `Spawned` pushes the opening message |
| B14 | the row's summary is from the first run, forever | `AgentTree::{begin, finish}` |
| B15 | `screen.py` mis-reads CSI / `--keys` escapes | `scripts/screen.py` — cursor clamped to the grid (a row past the bottom, or a shrink under a low cursor, raised `IndexError` on the next `X`), and `--keys` decodes the escapes it means instead of `unicode_escape`, which turned `é` into `Ã©` and left `\e` literal. `--self-test` pins both. |
| B16 | `smoke.py --cancel` forks after starting a thread | `scripts/smoke.py` — the pty is forked before the endpoint's thread exists; verified by running the scenario. |
| B17 | the layout sweep asserts "does not panic", not painted text | `app/screen.rs` + `ui::draw(frame, &Screen)`; `the_draw_sweep_asserts_painted_text_not_that_it_did_not_panic` over 15 sizes × 14 states, plus seven focused `the_sweep_*` tests (`7e123e1`) |
| B18 | `~` elision matches a prefix, not a directory | `app/screen.rs::facts_line` (moved from `ui.rs` by B17) |
| B19 | global notices render into every transcript | `Notice.agent` + `Chat::notices_for` — no unscoped read exists |
| N1 | `MAX_TURNS` turns "long" into "failed" | `agent/run.rs`: `RUNAWAY_TURNS` + `LOOP_ROUNDS` (a run ends when it stops calling tools; only a *loop* ends it early) |
| N2 | message box is append-only and clips at the right edge | `Input` (grapheme cursor + window), `Chat::key` owns the editing keys |
| N3 | a stopped child is reported to its parent as `#N done: cancelled` | `agent::Outcome` (one enum, not a `summary == CANCELLED` string sentinel) |
| N4 | Ctrl-C stopped *every* busy agent, and blanked a stopped one to `Idle` | `App::interrupt` (focused) + `Ctrl-X` (`interrupt_all`); `Phase::Stopped` |
| N5 | the one-non-isolated-sibling rule fails only *after* the brief is written | `spawn_tool` message + the rule stated in `prompt` schemas and the system prompt |
| N6 | an interrupted run commits under the same subject as a finished one | `commit_worktree` subject carries the `Outcome` |
| A20 | `needs_compaction` compares byte weights against a *token* budget, and `budget * 3 / 4` can overflow `usize` | `mush-core::transcript::compaction_trigger` is saturating and the byte↔token conversion lives once in `Config::history_budget` (same shape as A2) |
| U1 | a working agent drawn as paused (`⏸` from "has children") | `screen::phase_glyph` is a function of the node's own `Phase` only, and the children are a separate `⏸N` mark (`ui::agent_line`) |
| U2 | the pane title counts waiting agents as working | `AgentTree::roster` derives `working`/`waiting` from the phases (`app/tree.rs`), and `ui::agents_title` names each count |
| U3 | another agent's news snaps a read pane back to the bottom | `Chat`'s `reading` is per conversation (`Reading::Holding`), and `painted` marks a held window in the title |
| U4 | a grandchild drawn after everything spawned before it | `AgentTree::rows` walks pre-order over the parent links |
| U5 | the newest activity on screen three times | `App::tree_line` no longer repeats the activity; the bar says the napping root instead |
| U6 | an agent is a bare number | `AgentNode::title` derives a handle from the brief (a path first, else the first non-filler word) |
| U7 | a waiting agent still says `working…` | `Phase::waiting` (`app/tree.rs`) tells a model call from `wait`, and the row/foot say which |
| U8 | a transient notice never leaves | `Chat`'s chatter lifetime (`clear_notes_for`, `dismiss_said`, `SAID_TTL`) + repeat collapse (`Notice.count`) |
| U9 | the shipped DeepSeek window/reply cap is too small | `provider::PROVIDERS` fallback 120 000 + `Config::reply_cap` (a quarter of the window, floored at 1 024 and capped at 120 000) |
| U10 | walking back up a deep tree costs a keypress per ancestor | `Intent::TreeWalk` on `←`/`→` (`app/keys.rs`) + `PickerMove(±PAGE)` |
| U11 | compaction has no visible state anywhere | `Phase::Compacting(Parked\|Requested\|NearlyFull)` (`app/tree.rs`), `≡` + the fold's words (`app/screen.rs`), the bar's "keep typing" sentence (`App::tree_line`), and `compact_now` owning the fold's cancel flag (`mush/38`) |
| B20 | a child's completion reaches the model but not the screen, and can fold twice | `agent::push_line` emits `AgentEvent::Message` with the fold, and `absorb` marks the adopted line delivered |
| B21 | a parent in a tool-calling chain never heard its child finish | the fold runs at every message boundary (`fold_completions`), not only on the tool-free turn |
| B22 | a steering message to a subagent is invisible / an idle target not woken | `AgentMsg::Steer` → `push_line` (delivered and emitted), and it is work to answer; the reply wording left over is H5's |
| B23 | a transient transport failure ends the run instead of being retried | `model.rs::retrying` — `RETRY_ATTEMPTS = 3` over the `Clock` seam, transport failures only, each retry announced in the transcript |

---

## 7. Design-doc sync

Done in the doc-sync wave that closed this plan's checklist. What `docs/mush.md`
was asked for, and what it now says:

- §1: the line count — the design doc claimed "roughly 5,000" /
  "roughly 9,000" lines; it now points at `scripts/census.py` for the split
  (blank, comments, tests, production) instead of asserting a total that would
  drift.
- §6: the module tree now names the modules that actually landed (`app/{mod,tree,
  chat,settings,keys,commands}.rs`, `jobs.rs`, `model.rs`, `machine.rs`,
  `clock.rs`, `events.rs`, `session_save.rs`, core `transcript.rs`/`text.rs`),
  with a note on what was dropped (`editor.rs`, `host.rs`, the `agent/` split).
- §7: the dependency table is unchanged by the wave — "no new dependencies" held
  — plus the rule that a new crate must *replace* hand-rolled code that drifted,
  not sit beside it.
- §9: **M2.7**, **M2.75** and **M2.8** read as done, with what each actually does;
  M3/M5/M6 say plainly that they are not started.
- §10: **done** — "deterministic orchestration needs `python3` and `git`" became
  "the model is scripted; the work is real". The default suite still runs a real
  `git` (the worktree scenarios are about git), three real commands whose point
  *is* the process group or the scratch file, and the local mock sockets in
  `http.rs` — with their 200–600 ms read slices — left after Stage 2.3.
- §0's acceptance test: **held** for the default suite, whose only remaining
  `#[ignore]`s are the three live-endpoint tests in `http.rs`.
- §12: the decisions the wave made — one owner per fact; a trait is justified
  only by a fake a test actually uses; a delivered result has one owner; notices
  have kinds and lifetimes; a transport hiccup is retried and an answer is not; a
  window a human states beats a default.

Housekeeping: `.mush/wt/2` was still registered, and `mush/2` pointed at `d4f80ae`
= master with a clean worktree, so `/discard 2` lost nothing. **Done**: wave 0
removed it along with every other leftover worktree and branch, so `find`, grep
and dependency audits no longer count the source twice. The working tree now has
exactly one worktree (the main checkout).

---

## 8. Not doing

- **No third crate.** The pure/binary split is right; the problem was never the
  crate boundary.
- **No `Tool` trait / plugin registry.** Twelve tools with a name enum is the
  honest shape.
- **No Elm-style `update -> Vec<Effect>` rewrite.** `AgentTree`/`Chat`/`ConfigCell`
  plus four seams give the same testability at a fraction of the churn.
- **No replacing `http.rs`.** Its slice-and-cancel behaviour is load-bearing
  (`f1d0f2a`), and the request/response boundary is already testable over a real
  loopback socket.
- **No new dependencies.** The seams are traits; nothing here needs a crate.

---

## 9. Open questions

1. Does the editor pane come back? **No** — the v0.2 decision dropped it, so
   Stage 0's `Buffer`/`text` split has no `Buffer` half and Stage 4's undo work
   has no home; §1's diagnosis loses that cluster.
2. Is `dirty_screen` moved into a `Screen`/`Redraw` type (Stage 3), or does
   `update` return a `Changed` bool? The former is more honest; the latter is
   smaller. Open — the `Screen` value itself landed (`7e123e1`), but
   `dirty_screen` is still a bare `App` field, so the question is whether the
   redraw signal joins the value or leaves as a return.
3. Should `AgentTree` own `agent_stats` (git facts) too, or does the git snapshot
   stay a separate value refreshed by events? Today the coupling is one-way
   (`refresh_git` reads the tree), so it probably stays separate.

---

## 10. The wave loop (how work lands, and what it owes the tree)

The rule the waves have settled into, so it is not re-derived each time:

1. **One branch per item**, one commit per item, committed as soon as the gate is
   green (`cargo fmt --all --check`; `cargo clippy --all-targets -- -D warnings`;
   `cargo test`; both `scripts/smoke.py` pty scenarios). A branch that dies mid-way
   leaves its work in a commit whose subject is the brief — that commit is rebuilt
   with a repo-style subject, and the rebuild is proved lossless by
   `git diff <old-tip> <new-tip>` being empty.
2. **An integrator merges it onto `master`** (`--no-ff`), resolving for the two
   intentions rather than for the diff, and records the decisions in the merge
   body. No merge commit carries a change of its own beyond reconciliation.
3. **Every integration onto `master` is followed by a review agent whose subject
   is duplication and missing seams** — the same fact, rule or shape written more
   than once, and the abstraction that would remove the repetition without moving
   derivation across a seam (§2's rules). It reports a ranked list with `file:line`
   evidence, a net line delta, the risk, and the test that would protect the
   change; the list becomes the next wave's queue. This is how the tree pays for
   itself as it grows: each wave is allowed to add code, and the review decides
   what the next wave takes back out.
4. **A UX/UI review wave closes the loop**: it drives the real binary at §4.5's
   sizes and reports what the screen says against what is true. Its findings get
   the same treatment as any other row: a home is named, a fixer is sent, and the
   row's status is moved in `docs/findings.md`.

---

## 11. The duplication queue

The review that follows every integration onto `master` (§10.3) reports the same
fact, rule or shape written more than once, ranked by (net lines × confidence) ÷
risk. This is the one ledger of what those reviews have found: a row per item,
what it costs, and — while it is open — the risk the fix removes and the test
that would protect it. The reviews themselves are the subsections below, kept for
their evidence: what each measured, which bugs it injected, and which semantic
changes it proved byte-identical.

How to read a row. `Net` is the fix's size — the measured delta for a landed row,
the review's estimate for an open one. `Status` names the commit that landed a
row, or `⬜` while it is open; `R6` is judged and deliberately left, and its row
says why, so it is not mistaken for forgotten. References are by **symbol** — a
function, type or test name — never by line, because the tree moves under a
ledger that outlives it; the review subsection each row came from is where the
evidence's commit is named, and a price re-set by a later review says so (the
sixth review, after the `Screen` rewrite, re-priced `D9` and `R9`).

The census the reviews read, and the tree this ledger anchors to, measured at
`b8d8baa`: 42 394 lines (prod 11 543, tests 17 943, comments 10 153) — from the
`960e073` baseline it was seeded with, 41 416 (prod 11 800, tests 17 299, comments
9 635; `findings.md` §8.19), so prod −257 / tests +644 / comments +518.
`scripts/census.py` is the method; `findings.md` §8.5 has the per-wave deltas,
from 9 185 lines at `143325a15` to 41 093 at `f70374f` (prod ×2.5, tests ×6.7,
comments ×7.5).

| # | What is duplicated | Net | Risk | Protecting test | Status |
|---|---|---|---|---|---|
| D1 | The `App` test fixture is hand-rolled twelve times (`app/mod.rs`: `app_at`, `app_and_rx`, `app_writing`, `app_recording`, `reopened`, `test_app` + six inline). One `app_root(root, stored, save) -> (App, Receiver<Msg>)`. | ≈ −70 | — | — | ✅ `58a309c` (−103) |
| D2 | Two enums answer "what is a wait waiting on": `tree::Waiting` and `jobs::Waited`, with the same `noun()` and the same tool names spelled again in `Waited::tool()`. Keep `jobs::Waited`; `Phase::waiting() -> Option<jobs::Waited>`. | ≈ −20 | — | — | ✅ `58a309c` (−14) |
| D3 | `retrying(...)` + its announce closure are copied verbatim (`agent.rs` run loop and `compact_history`). One `fn ask(actor, request, cancel)`; the error arms stay per-caller. | ≈ −9 | — | — | ✅ `58a309c` (+1) |
| D4 | `App::say`/`App::fail` differ only in `kind`, and the "name the agent if it is not focused" branch is written twice. One `set_status(kind, text)` + `say_for(id, text)`. | ≈ −12 | — | — | ✅ `58a309c` (±0) |
| D5 | `describe()` derives "stated / the provider's default" twice (`main.rs`), and `AgentTree::has` re-spells `node(id).is_some()`. | ≈ −6 | — | — | ✅ `58a309c` (−4) |
| D6 | `paint_diff` re-implements `git::commit`'s `rev-parse --verify ^{commit}` probe (`git.rs` keeps the one home). | ≈ −5 | — | — | ✅ `58a309c` (−3) |
| D7 | `App::busy` and `App::working_agents` are two derivations of "own run or a job". One `in_flight(node)`; `tree.busy()` stays agent-only on purpose. | ≈ −3 | — | — | ✅ `58a309c` (+4) |
| D8 | The `ChatRequest` shape and the thinking/reasoning knobs are built twice (`agent.rs` run loop and `compact_history`) — **and this copy is where the bug lived**: the fold sent `max_tokens` even on an endpoint configured for `max_completion_tokens`, and swallowed a `Status`/`Malformed` as `Ok(())`, so compaction silently never happened there. | ≈ −10 | — | — | ✅ `58a309c` (+151, tests included) |
| D9 | Two "drop cells from the right until they fit" loops, now cross-module: `ui::agents_title` joins clauses with ` · `, `screen::facts_line` with ` │ `; their floors differ (the title falls back to `" agents "`, the facts line never drops its first cell). The cheaper shape is to pre-elide in `screen::agents_pane`, so `title_cells` becomes a `String` and the painter's loop goes. | ≈ −5 (was −8) | A shared helper would move pane layout into the painter; the sixth review proved the build/elide split byte-identical, so what is left is the floors — a clause cut mid-number, or a facts line that gives up its workspace cell. | `the_title_elides_clauses_instead_of_cutting_numbers` reads the painted title at 200×50 and 80×24; the draw sweep paints it at every size. | ✅ `452fe44` |
| D10 | The `bar_rows` rule (`area.height >= 24`) is written in the prover and again in the test helper `selected_rows`; one pure `screen::bar_rows(height)`. | ≈ −2 | The tier edge moves in the prover and not in the helper, so `selected_rows` reads rows the bar paints over. | `the_facts_line_survives_at_80x24` pins the 24 edge; `selected_rows` is what the tree's highlight tests read. | ✅ `35c9ab1` |
| R1 | `absorb`'s `Run` arm carried the same "adoption may only *add* marks, never move one backwards" paragraph twice, one per author. It is one paragraph above the single `delivered.entry(id).or_insert(run)` loop. | ≈ −2 | — | — | ✅ `45116ad` |
| R2 | `AgentTree::compacted` and `compacting_ended` are the same body twice, differing only in the at-rest phase (`Phase::Done` vs `Phase::Idle`). One private `fold_over(id, in_run, at_rest)`. | ≈ −5 | A third fold surface picks one of the two and the at-rest phase drifts. | `a_landed_fold_takes_its_state_off_the_screen` and `a_fold_that_ends_without_landing_leaves_a_quiet_row` assert the phase each end leaves. | ✅ `3148eb3` |
| R3 | The "deliver a completion once" rule was open-coded seven times (both `absorb` arms, `drain_mailbox`, `fold_completions`' two loops, both wait tools). `ActorState::record_child(id, run, outcome) -> (String, bool)` and `record_job(...)` are the one home of "mark it read and say whether it was fresh"; each caller keeps only its own decision. | ≈ −15 | — | — | ✅ `45116ad` (with `R7`) |
| R4 | The "at rest with children working" predicate was derived twice and the two disagreed. `AgentTree::napping(id)` is read by `roster` and by `tree_line`. | ≈ −4 + fix | — | — | ✅ `642fda8` |
| R5 | A fold that came to nothing was ended by each caller for the same `Ok(false)`; `compact_history` now emits its own `CompactingEnded { in_run }` on every `Ok(false)` return. | ≈ −5 | — | — | ✅ `45116ad` |
| R6 | Run identity is four homes plus the adoption scan. Folding `completed`+`delivered` into one `Completion { run, outcome, read }` (and `done_jobs`+`delivered_jobs` likewise) is **not** clearly better: `completed` says what the child last reported, `delivered` what the model has read, and the merged shape would clobber the newer record on an out-of-order arrival. The delivery road's one home is `R3`'s `record_child`/`record_job` instead. | ≈ −8 | **Judged and left on purpose**: the merge would trade one duplication for a wrong answer on an out-of-order arrival. Not an oversight. | — | ⬜ judged |
| R7 | Overlapped `R3` (the two `absorb` arms are one shape); the fix picked one home — `record_child`/`record_job` — not both. | — | — | — | ✅ `45116ad` |
| R8 | The fold's *verb* is spelled again in the bar and in `/compact`'s acknowledgement while `Compacting::words()` owns the row's words — so the bar says `compacting` for a fold its own row calls `folding at the next step`. Derive the verb from `Compacting`. | ≈ 0 | The bar and the row disagree about the same fold. | `a_fold_in_flight_is_painted_on_every_surface` reads the row's and the bar's words together, for each kind of fold. | ✅ `25246dd` |
| R9 | `screen::chat_pane` looks `self.tree.node(self.tree.focused)` up twice for two projections, `busy` and `compacting`, where one binding has both. | ≈ −3 | None within a frame — both reads see the same tree, and the sixth review proved the split byte-identical. One lookup fewer to keep in step. | the chat-pane assertions in `a_fold_in_flight_is_painted_on_every_surface`. | ✅ `ba03a5f` |
| R10 | Two stale comments the integration left: `app/tree.rs` offered `summarizing…` as an `Activity` example (no `AgentEvent::Status` sends it), and `app/screen.rs` said the title's `M waiting` are "the `⏸` rows" — false since U1, because `AgentRow.waiting` is `busy_children` (a *working* parent wears `⏸N` too) while `roster.waiting` counts only the at-rest. (The third comment, the bar's claim that `say`/`fail` own every line, and the `roomy` doc landed in `f70374f`.) | ≈ −4 | The two docs in the tree contradicted each other about the same count, so either reader was misled about what `waiting` means. | `the_title_counts_working_and_waiting_agents_separately` pins the title's meaning; nothing pins a comment. | ✅ `35e8acb` |
| R11 | "The brief's first line, whitespace collapsed" is written four times: `first_line`, `subject_brief`, `job_title`'s body and `AgentNode::title`. One home in `mush_core::text`, where the string arithmetic already lives (B9). | ≈ −8 | A fifth spelling is what the next surface adds; the four already disagree at the edges (`subject_brief` trims, `job_title` keeps only the last `&&` clause). | `a_subject_is_the_briefs_first_line_cut_on_a_word_boundary`, `a_title_is_derived_from_the_brief`, `a_job_is_named_by_its_command_on_one_line`. | ✅ `fde3314` |
| R12 | `Landed`'s past word was spelled where it is rendered in two modules: `worktree_gone` and `screen::agent_detail`. `Landed::past()` is the one home of the word, read by `worktree_gone`'s refusal; `agent_detail`'s row composed a *sentence* ("merged into HEAD"), not the bare word, so that second site was **judged prose, not a duplicate**, and left as the painter's. (The `worktree_command` site this row also named went with the command cut, `ad5b791`.) **Reversed by `findings.md` §8.33**: the row's extra words turned out to be a second *fact* rather than a second spelling — “into HEAD” was false for a nested child whose branch landed in its parent's — so `mush/122` made the row paint `Landed::past()` too, and the word has one home after all. | ≈ −6 | The refusal and the row tell a landed agent's story in different words, and a third surface adds a third. | `a_landed_agent_does_not_offer_commands_that_cannot_work` reads the row's words; `worktree_gone`'s refusal is read in `app/mod.rs`. | ✅ `fd0a477` |
| R13 | The UI's `worktree_gone` and the actor's are deliberately two-sided (the UI keeps the words in the box and out of the transcript; the actor is the backstop for a sender the UI never sees) — keep both — but their predicates disagreed after a restore: `restore_agents` passed the stored `branch` unfiltered while `revive` filtered it on the worktree's existence. `agent::live_branch(root, id, branch)` is the one decision, shared by both. | +2 | — | — | ✅ `642fda8` |
| R14 | The registry's one reach was walked three times: `kill_owned`, `kill_all` and `Registry::drop` each walked `foregrounds`+`jobs` — except `Drop`, which walked only `jobs` while its own doc claimed the backstop. One `fn kill(&self, owner: Option<u64>)`; `drop` is `kill(None)`. | ≈ −8 | — | — | ✅ `45116ad` |
| R15 | `Ended`'s three "mush stopped it" variants were `jobs::Stopped` copied: `wait_bounded` translated 1:1 and `watch` translated the same three into `JobOutcome`. `Ended::Stopped(jobs::Stopped)` is that reason, built by `jobs::stopping`, and the report's sentence table is one pure `end_note`. `Ended::Detached` is not an end `run_shell`'s report reaches — the handover returns first — and its sentence now says what it is (the command went to the job registry), not the timeout it never was. | ≈ −8 | A fourth stop reason is added to `Stopped` and one translator misses it — the model reads the wrong sentence. | `the_three_ways_a_foreground_command_ends_are_not_confusable` reads every arm of the table, the `Detached` one included. | ✅ `29700b3` |
| R16 | The `/compact` refusal — the `asked` guard, the `emit` and the `Ok(false)` — is written twice in `compact_history`; the second copy was added by the `mush/47` merge. One `fn nothing_to_compact(actor)`. | ≈ −4 | A third "nothing to fold" arm tells the human something the other two do not, or stays silent where the row says `compacting…`. | the `NOTHING_TO_COMPACT` tests assert both arms' words. | ✅ `6a68a0e` |
| R17 | `main::shown_under` re-states `Workspace::rel`: the same strip-prefix/unwrap-or dance, except `rel` also folds `\` to `/`. Pass the `&Workspace` the caller already has and delete `shown_under`. | ≈ −6 | A second elision rule drifts from `rel`'s (a path outside the root, a Windows separator), and the session's own name is shown by whichever one is used. | `a_path_outside_the_workspace_is_shown_whole` now lives in `workspace.rs` beside `rel`. | ✅ `421be43` |
| R18 | `keep_unreadable`'s two `Err` branches repeated the name formatting and the sentence shape — one `cannot_keep(from, why)`. This was also the only untested path in the session code. | ≈ −4 | A third failure gets a sentence that does not name the file the human must go find. | `a_session_that_cannot_be_kept_still_names_the_file` walks both `Err` branches. | ✅ `d3517f6` |
| R19 | `App::session_unreadable` was the second spelling of "a failure takes the notice, the bar and the dirty mark" against the `AgentEvent::Error` arm; `App::fail_for` is the one door, with the caller keeping only whether the bar is the place for the line and what it names. | ≈ −4 | The durable notice and the bar's line stop agreeing about one failure. | the session-unreadable tests, which assert the notice, the bar and the stored line. | ✅ `7185e90` |
| R20 | `Launch::held` stated the owner `Registry::hold` already recorded — one owner per fact, so `Foreground` now carries it; and the same edit builds `Live` by hand twice → one `Live::new(job)`. | ≈ −2 | The owner on the record and the owner in the slot disagree, and `kill_owned` kills the wrong set. | `a_handed_over_command_belongs_to_the_agent_that_held_it` reads the job list back through the holder. | ✅ `d5ba536` |
| R21 | `attach_agents` was a second derivation of the row the painter derives, re-computing `focused`, `children_working`, `title`, `branch` and the phase words by hand. It now serializes `App::agent_row` plus the wire-only extras (`worktree`, `summary`, `leftover`, `revision`) in `app/mod.rs`. | ≈ −20 | — | — | ✅ `3b6602d` |
| R22 | "What a phase is called" was spelled three ways in two modules: `Phase::label`/`doing` and the painter's `phase_glyph`/`phase_detail`. `doing()` overlapped `label()` on thinking/compacting/cancelling and collapsed `Stopped`/`Done`/`Failed` to `idle`. Owner: `screen.rs` owns painted prose, `Phase` owns the machine name; `doing()` derives from `label()`, `phase_detail` composes from them instead of re-spelling the stems — and one doc comment pointed at `ui.rs` (`Phase::doing`'s, in `app/tree.rs`), which no longer derives a phase word. | ≈ −15 | Every new `Phase` variant breaks the exhaustive matches that spell it (the compiler doing its job); the drift was the quit line reading `#0 idle + 1 job` for a stopped agent that owns a live job. | `glyphs_are_truthful` and `details_age_with_the_phase`; the quit line is read by `a_live_run_arms_the_quit_and_names_what_dies`, and the stopped-over-live-job case by `a_stopped_agent_over_a_live_job_is_named_as_stopped`. | ✅ `cc7aae3` |
| R23 | The attach response body was an untyped `Value` whose keys were spelled in producer and consumer, and the consumer silently defaulted a missing key (`print_agents`' `field` closure returned `""`), so a rename was a silent blank rather than an error. `attach::Roster`/`attach::Transcript` are the two bodies as types, each with one `read`; a missing or wrongly-typed key is the client's error naming the key. | ±0 | A wire key renamed on one side painted an empty column forever, and nothing failed. | `the_cli_shapes_read_the_roster_the_producer_writes` and `the_cli_shapes_read_the_transcript_the_producer_writes` read a real `handle_attach` body. | ✅ `7d03edc` |
| R24 | `attach_focus` asked "is #N in the tree?" twice: `tree.has(id)` then `point_cursor_at`, whose `bool` was ignored. They agreed only because `rows()` paints every node; it now uses the one return value. | ≈ −3 | A tree that hides a node (a future filter) accepts a focus that lands on no row. | `attach_focus_moves_the_focus_like_enter`. | ✅ `487ab57` |
| R25 | The tree pane's window geometry was derived twice — `inner`/`footer_rows`/`list_area` in `screen.rs` (to derive `▲`/`▼`) and again in `ui.rs` (to place the `List`) — so the counts were a model of the scroll. One `AgentsPane::list_area: Rect`, set where the pane is laid out in `app/screen.rs` and read by the painter in `ui.rs`. | ≈ −7 | — | — | ✅ `f70374f` |
| R26 | `phase_detail(cursor_node)` was derived twice per frame — once per row and again for the footer, each reading `node.since.elapsed()` separately, so a boundary crossing could paint two ages in one frame. The footer now reads the row it already built. | ≈ −2 | One frame paints two ages for one phase. | `details_age_with_the_phase` pins each spelling; a frame test that the row's and the footer's activity agree. | ✅ `ef8587b` |
| R27 | The worktree path string was assembled in the UI although `worktree_path`'s doc claims core owns it: `format!("{}/{}", git::WORKTREE_DIR, node.id)` in `app/screen.rs`, again in `crates/mush-core/src/git.rs`, and in a test at `app/screen.rs`. One `git::worktree_rel(id)`, used by `worktree_path` and the row. | ≈ −1 | Core changes `.mush/wt/{id}` and the row names a directory that is not there. | `an_unmerged_agent_names_its_worktree_and_the_git_command_to_read_it`, re-aimed at `worktree_path`. | ✅ `38578a9` |
| R28 | Test-only: `selected_rows` re-implemented `shot` — the same `set_term_size` + `screen()` + `Terminal::new` + `draw` + buffer read. One `fn painted(app, w, h) -> (Screen, Buffer)`, which `shot` and `selected_rows` both read. | ≈ −8 (tests) | The two harnesses drift — already: `selected_rows` carries its own copy of `bar_rows` (`D10`). | the tree's highlight tests that read `selected_rows` (`page_keys_move_the_tree_cursor_a_page_and_clamp_at_both_ends`, in `app/mod.rs`). | ✅ `35c9ab1` |
| R29 | `busy_children(id)` was an O(n) scan called per node in `roster()` (through `napping`), per row in `agent_row` and once in `tree_line` — the same fact walked ~2n+1 times per frame — and `App::live_jobs`, the documented one door, was bypassed by `in_flight`. One per-frame `HashMap<AgentId, usize>` (`AgentTree::busy_counts`). | ≈ −3 | A per-frame O(n²) walk on a deep tree, and three walks over the same fact that can disagree. | `the_bar_and_the_title_agree_on_who_the_root_waits_for` and `the_title_counts_working_and_waiting_agents_separately`. | ✅ `fc5982a` |

### The first review (after the `mush/39` integration, `0bd7e8a`)

Measured at `0bd7e8a`: 32 150 lines total, ~19 250 of them tests (`app/mod.rs`
6 224 / 3 966 test, `agent.rs` 6 872 / 4 104, `app/chat.rs` 2 524, `jobs.rs` 1 358,
`ui.rs` 1 024). It found `D1`–`D8`, the `ChatRequest` copy (`D8`) carrying the
compaction bug with it.

**Genuinely not duplicated (checked, do not re-litigate):** `ConfigCell`'s two
faces over one `believable()`; `keys::KEYS`/`commands::COMMANDS` each rendered
from one row list; `ToolName`; the worktree verbs' one-row-earlier shape;
`Outcome::line` writing and `chat::report` parsing (a persistence boundary: the
reader must not trust the writer); `git::run`'s two contracts; `Phase` ↔
`StoredStatus` as a tested inverse pair; `Roster`/`busy_children`; `agent.rs`'s
layered test fixtures; the sanitize-at-the-door arrangement.

### The second review (after the `mush/38`+`mush/41` integration, `fb7265d`)

Measured there: 33 504 lines in `crates` (`agent.rs` 7 681 with 4 689 test,
`app/mod.rs` 6 428 / 4 126). It found `R1`–`R10`.

**Confirmed not duplicated by the second review:** the fold's state (all five
surfaces read the one node phase — no surface re-derives it); the run identity's
homes (each knows something the others cannot); `busy_children` (used, never
re-inlined); the sanitize doors and `truncate`/`fit_row` (no new copy);
`Outcome::line` vs `status_tool`'s listing (two readers, two formats);
`Compacting`'s three variants (constructed only where the *why* is known);
`Phase::Compacting` → `StoredStatus::Idle` on save (a persistence decision).

### The third review (after the `mush/46` integration, `c4aa2e3`)

It found `R11`–`R13`. Confirmed: `diff_rows`' preamble/hunk knowledge has exactly
one home, and `git.rs` is not it (presentation, not a parser);
`subject_brief`/`parse_commit_subject` are a tested writer/reader pair across a
persistence boundary (keep); `Focus` has three writers, each a different move,
and two readers — no drift.

### The fourth review (after the `mush/47` integration, `e747bc3`)

It found `R14`–`R20`. Confirmed: the doubled `absorb` paragraph (`R1`) was still
there then, adjacent above the one loop; `Session::load_from` delegates to
`read_from` (one parse, one absent/unreadable decision) and the `.bak` name has
one home (`first_backup`, whose path is returned rather than re-derived); no
notice string is spelled twice (`NOTHING_TO_COMPACT` is one const at both sites,
the isolation `reason` is one `Option<String>` rendered for two readers, the
session line is built once in `main`); the three notices take two doors on
purpose (`AgentEvent::Notice` → `chat.note_for`, Info; `App::session_unreadable`,
Alert and durable) and the kind/rank is the constructor's, never re-derived;
`Launch::started`/`held` is a seam rather than a second lifecycle (one `Live`, one
admission/lock/mailbox path, one process group), and `JobOutcome`'s
TimedOut/Cancelled → `Stopped` is a job-reader's choice, not a re-derivation.

**Three things it found that are not duplication:**

1. `Launch`'s doc promised the group "is in the registry's reach every moment of
   its life" (in `e747bc3`'s lines, `jobs.rs:459–463`), but `Source::into_live`
   freed the foreground slot (`:528` → `:649`) before `launch` took the registry
   lock, so a `kill_all` in that gap missed the command. `R14`'s edit —
   hold the slot until the record exists — closed it. No test can pin the
   window; the prose was what was wrong.
2. `c450e78`'s message says the unreadable session is "copied byte-identically"
   while `keep_unreadable` renames it: the original path is gone, not duplicated.
   One word.
3. The `S3`/`S4`/`S6`/`S7` rows of `docs/findings.md` had lost their `ID` and
   `What` cells — a `✅` edit left two-cell rows in a four-column table — so the
   queue of record no longer said what any of the four was. Repaired in the same
   commit as this review.

### The fifth review (after the M3 attach wave, `f29b352`)

It found `R21`–`R24`. Confirmed sound, do not re-litigate: one wire type
(`Request`/`Op` have one `encode` and one parser, and the CLI builds the same
type, so a client cannot spell a field the parser reads differently); `App` stays
the only effector (the socket thread only sends `Msg::Attach` with a one-shot
`bounded(1)`); a bad line is answered from the socket thread and the connection
survives; `id` is echoed on every reply and is `null` only when the line could
not be parsed; `advance` (`(prior+1).max(lines)`) is the right shape for a stored
counter; a stale socket file is cleared and rebound while a live one is refused
and left in place; `Guard::drop` removes the file; `focus` reuses
`focus_cursor_row`, the same path Enter takes; an `edit` conflicts rather than
guessing for a stale base *within* a conversation; `edit send` restores
`tree.focused`; quitting with a client connected exits 0 and removes the socket;
`Msg::Attach` is drained inside one 30 ms tick. The attach review's finding rows
are `findings.md` §6 `A1`–`A8` (a different A-series from the §6 checklist
above; closed by `9b02a3b`), and its structural rows are `R21`–`R24`.

A new `Phase::CutOff` broke every exhaustive match on `Phase` — `label`, `doing`,
`phase_glyph`, `phase_detail`, `is_busy`, `waiting`, `compacting` and the
`Phase ↔ StoredStatus` pair — when the H2/H4 wave added it (`eab825e`). That is
the compiler doing its job, and it is why the name should have one home (`R22`)
before the next phase arrives.

### The sixth review (after the `Screen` wave, `7e123e1`)

Read against the replaced `ui.rs` function by function, plus a string-literal and
a constant census, plus eight injected bugs. It found `D9`, `D10`, `R9`, `R10`
and `R25`–`R29`. The two deliberate semantic changes —
the build/elide split in `agents_title`, and `bar_line` → `screen::bar_word`
returning `(Rank, String)` with colour left to `ui::rank_style` — were verified
byte-identical in output.

**Confirmed not duplicated (do not re-litigate):** `Screen` caches nothing and
`App` holds no frame; `picker_width`/`picker_text_width`, `agents_columns`,
`floor_notice`/`is_below_floor`, `phase_glyph`/`phase_detail`,
`agent_detail`/`agent_footer`/`compact_footer`, `facts_line`/`git_cell`, `HINT`,
`border(focused)` each have one home; `trim_trailing_blanks` and `FOOT_ROWS` have
one owner (`Chat::painted`/`Chat::foot`); `job_lines` *is* `live_jobs().map`, not
a second derivation; `git_cell` and the row's `place` answer different questions;
`mush_core::text::{sanitize, truncate, fit_row}` are the only sanitary doors, and
the sweep reads painted cells rather than source text.

The class the next blind audit should hunt — one fact with several spellings, and
a poll with a side effect — is named in `findings.md` §8, with the recipe that
found this wave's rows; the census it asks for is `scripts/census.py`
(`findings.md` §8.5).
