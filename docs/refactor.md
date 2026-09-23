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
live endpoints and the idle-box frame budget.**

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
reversed in turn (`docs/findings.md` H31, §8.36): a machine lock refuses *every*
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
| `ModelClient` | `fn chat(&self, req: &ChatRequest, cancel: &AtomicBool) -> Result<ChatResponse, ModelError>` | scripted reply queue, including errors and cancellation | `run_loop`, compaction, the learned-context retry, cancel mid-reply, a run kept past the old 200-turn ceiling (H45) — all in-process |
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
`http.rs` (the model list, the reply cap, and the TLS handshake) plus
`app/mod.rs`'s idle-box test of the 16 ms frame budget
(`a_frame_fits_in_a_60fps_budget_on_a_long_transcript`).

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
long-run scenario. The tests now wait on the `Done`/`Compact`/`Message` events
they assert on instead of polling for a file, and each one asserts what it
pinned before — the child's work on `mush/1` in `.mush/wt/1`, `mush/2` based on
`mush/1`, the folded summary as the next request's only user message, the nudge
in the second request, a run kept past the old 200-turn ceiling that ends on the
model's own stop (H45). Gone with
them: `start_mock*`, `stop_mock`, the four port constants (18731–18735), the
`python3` readiness probe, and every sleep over 20 ms in these tests.
`scripts/mock_llm.py` stays in the tree for the pty smoke scenarios; no test
refers to it. The only surface the production code grew is `#[cfg(test)]`:
`agent::spawn_scripted`, which starts the same root actor over a caller-supplied
client.

**Stage 2.3 — `Machine` + `Job`, `Clock`, `Events`.** ✅ The last three seams of
§4, plus B6. `machine.rs` holds `Machine::spawn(&ShellCommand) -> Box<dyn Job>`
and `Job::{poll, written, output, kill}`; the real impl is the shell as it
always was (own process group, output to scratch files), and `Scratch` moved
into it — the `kill -9 -pgid` child that moved with it is gone: `3bb1a5b`
signals the group in process through `kill_group` and `rustix`. `clock.rs`
holds `Clock::{now, sleep}`, the
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
| N1 | `MAX_TURNS` turns "long" into "failed" | `agent/run.rs`: `LOOP_ROUNDS` (a run ends when it stops calling tools; only a *loop* ends it early) — the 200-turn ceiling and its wrap-up turn were removed later (H45, §8.47) |
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
| B23 | a transient transport failure ends the run instead of being retried | `model.rs::retrying` — `RETRY_ATTEMPTS = 3` over the `Clock` seam, and only an `Unsent` failure is retried: a dial that never connected, or a write that did not hand the whole request over. Everything after the write is final — an answer, a refusal, a cancellation, a broken frame — and each retry is announced in the transcript (`b6a59c3`, `190886c`) |

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
  `#[ignore]`s are the three live-endpoint tests in `http.rs` and `app/mod.rs`'s
  idle-box frame-budget test.
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
risk. This is the one ledger of the duplication queue's decisions: a row per item,
what it costs, and — while it is open — the risk the fix removes and the test
that would protect it. The queue's evidence is the record's, not this ledger's:
`findings.md` §8.51 carries the six blind audits and their findings, §8.70 the
four duplication passes that read the tree blind, each pointing back at the rows
here. The reviews' own subsections below are kept for their evidence: what each
measured, which bugs it injected, and which semantic changes it proved
byte-identical.

How to read a row. `Net` is the fix's size — the measured delta for a landed row,
the review's estimate for an open one. `Status` names the commit that landed a
row, or `⬜` while it is open; `R6` is judged and deliberately left, and its row
says why, so it is not mistaken for forgotten. References are by **symbol** — a
function, type or test name — never by line, because the tree moves under a
ledger that outlives it; the review subsection each row came from is where the
evidence's commit is named, and a price re-set by a later review says so (the
sixth review, after the `Screen` rewrite, re-priced `D9` and `R9`).

The census the reviews read — the tree this ledger then anchored to — measured at
`b8d8baa`: 42 394 lines (prod 11 543, tests 17 943, comments 10 153) — from the
`960e073` baseline it was seeded with, 41 416 (prod 11 800, tests 17 299, comments
9 635; `findings.md` §8.19), so prod −257 / tests +644 / comments +518.
`scripts/census.py` is the method; `findings.md` §8.5 has the per-wave deltas,
from 9 185 lines at `143325a15` to 41 093 at `f70374f` (prod ×2.5, tests ×6.7,
comments ×7.5).

The four blind passes below (the seventh through tenth reviews) read production
code only, each in its own currency: 2 644 code lines in the pane and the text it
wraps, ~4 600 production lines in the store's fifteen files, 4 865 non-test code
lines in the actor's nine, 4 800 code lines in the app and its panes. The tree
those rows were checked against at entry, measured at `38d0438`: 79 208 lines —
prod 18 232, tests 34 663, comments 21 708, blank 4 605 (`scripts/census.py`).
That sentence is kept as it stood: it is the census the four passes' rows were
checked against at entry, not a number to be overwritten. This ledger's anchor is
now `7338d81`: 86 623 lines — prod 19 092, tests 38 164, comments 24 395, blank
4 972 (`scripts/census.py`) — prod +860 / tests +3 501 / comments +2 687 / blank
+367 over `38d0438`. The `b8d8baa` sentence above is left as it stands: it is
the census the reviews before these read, not a number to be overwritten, and the
rows below say where a later wave re-priced one of them (`1e07c2e`).

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
| R30 | The row map is counted a second time: `mark_rows` paints through `marked` and then re-wraps each source line to learn which painted row is the reading of which source line; its Markdown arm restates `text.rs`'s fence rule (the `starts_with("```")` literal against `fence_line`), and `View` restates `marked`'s reply decision (`mark == REPLY_MARK`). One tagged walk — `wrap_tagged`/`markdown_tagged_rows`, with `marked` returning each row's source line — deletes `mark_rows` and `View`. **The report read a tree before `f47bfdb`:** `capped_result`, `tool_lead` and `SHOWN` are gone (the elided-tail and fold waves), and the fold's `folded_rows` already returns tagged rows on one walk shared with `stops_at`, so the walk halves of the estimate are already closed; the fence rule and the reply decision are still two homes, and the `debug_assert`s that keep them in step compile out of release. | ≈ −55 (report, pre-`f47bfdb`) | **High** — silent in release; the fence rule lives in two files: add `~~~` to `fence_line` and not to `mark_rows`, and every stop after the first fence is attributed one row early, so the cursor band and the highlighted range land on the wrong row. | — | ⬜ |
| R31 | The mark's columns, spelled more than once: `marked` holds the first-row-mark/later-rows-blank shape in its reply loop and again in its plain loop, with the drop rule (`width >= lead + MIN_BODY`), and `folded_marked` lays out the same head/blank shape for folded blocks with the guard re-spelled in `folded_rows`. One `marked_row(index, indent, mark, style, width, body)`. **The report read a tree before `f47bfdb`:** its third road (`reasoning_rows`' own loop, the `"tool"` arm, `INDENT`/`TOOL_INDENT`/`tool_lead`) has no copy at `38d0438` — `reasoning_rows` and the tool arm go through `folded_marked`, and `folded_rows` drops the mark below `MIN_BODY`, so the 5-columns-into-4 case the report feared is unreachable. | ≈ −28 (report, pre-`f47bfdb`; what remains is the shape twice and the guard twice) | **Latent then, drift now** — at a 4-column body the two unguarded roads painted 5 columns; today the guard is stated in `marked` and again in `folded_rows`, and no test paints a folded block below width 8. | `a_pane_narrower_than_the_voice_still_shows_the_words` | ⬜ |
| R32 | `wrap_runs` is `wrap_capped`'s arithmetic a second time — the tab stop, the space-break loop and the tail-that-is-still-too-full retry — with `out.push(std::mem::take(&mut current))` four times in `text.rs` and nowhere else. One `Breaker<T>`; `wrap_capped` maps rows of `char` back to `String`s, `wrap_runs` feeds `(char, RunStyle)` items. | ≈ −20 | **Moderate** — pinned only by a parity test over ten texts at widths 1..=24: a tab stop or break rule fixed in one wrapper and not the other makes the reply's view wrap differently from the plain text beside it. | `a_plain_message_wraps_exactly_like_wrap_text`; `a_wrapped_row_never_outgrows_its_width` (plain only) | ⬜ |
| R33 | Four walks to a transcript's own edge, in two readings: `last_line`/`first_line` walk with measure-blind `lines_of`, while `adjacent`'s two `find_map`s walk with `Stops::of` (through `stops_at`). One `edge_line(on, newest, measure)`, read by the select mode's `First`/`Last` keys and by `adjacent` (the report says `Home`/`End`; those are the input box's keys). | ≈ −8 | **Low** — the readings agree only because `clamped_cursor` re-clamps; a second cap or a change to `Stops::of` makes `First`/`Last` land on a stop the pane does not paint, and `cursor_row`'s fallback puts the cursor on the wrong row. | — | ⬜ |
| R34 | "Mush's one way of putting a colour behind text", spelled five times (`draw_agents`' `highlight_style`, `select_painted`'s band and its inverse, `draw_picker`'s `highlight_style`, `draw_status`'s badge), with the comment at the first already claiming the owner. One `on_accent(theme)` and one `accent_on_black(theme)`; `rank_style`'s and `border`'s ink styles are a different fact. | 0 | **Low** (pixel tests read three of the five sites) — the single owner is the point. | `the_default_theme_paints_the_fixed_palette`, `a_themed_frame_paints_the_hue`, `the_select_mode_paints_its_cursor_and_its_selection_on_their_own_cells` | ⬜ |
| R35 | The attach wire's request and response are written by hand: `Request::encode` and `Response::encode` rebuild the exact keys `parse_request` and the answer parser read (`"op"`, `"agent"`, `"since"`, `"base"`, `"text"`, `"send"`, `"id"`, `"ok"`, `"error"`, `"kind"`, `"message"`, `"revision"`), beside a parser that must stay hand-written for its per-field sentences. One `Serialize` derive for `Op` (`#[serde(tag = "op", rename_all = "lowercase")]`) and one `Wire` struct with two optional keys for the answer. | ≈ 50 | **Low** — the wire keys are written twice and a rename on one side alone is a client painting a blank column; the report's "only the round trip keeps them in step" is overstated — `the_attach_subcommands_build_their_request` and `a_response_is_one_line_and_round_trips` assert exact lines. | `every_op_round_trips_through_its_line` | ⬜ |
| R36 | `clipboard::run` and `clipboard::deliver` are one bounded child twice: spawn through `scrub`, take the pipe end, an `mpsc` thread, a `try_wait` poll loop under one deadline (`deadline.saturating_duration_since(Instant::now())`), kill and reap on every exit, `sleep(POLL)`. One `wait_bounded(child, answers, deadline) -> Waited<T>`; each site keeps which end it wires and what its thread does with it. (`agent.rs` already has a `wait_bounded` for the job poll, so the name is taken.) | ≈ 28 | **Medium** — the deadline discipline is copied by hand: a reader copy that forgets the `wait` after `kill` leaks a zombie; a writer copy that spent the whole deadline in the receive would double the human's wait. | — | ⬜ |
| R37 | `session::keep_unreadable` (with `first_backup` and `BACKUP_TRIES = 100`) and `userconfig::keep_unparsable` (with its own `TRIES = 100`) spell the same next-free-name-beside-the-file loop character for character, both bound at 100. **Landed `0348028`:** one `workspace::backup_name(path)` and one `workspace::BACKUP_TRIES`, each caller keeping its own rename and its own reason (`a_backup_name_is_the_first_free_one_beside_the_file`; `a_second_unparsable_config_does_not_overwrite_the_first`). | ≈ 20 | **Medium** — the bound and the "never overwrite a copy already beside the file" rule had two homes, and a divergence loses a backup. | — | ✅ `0348028` |
| R38 | Three cap refusals, one shape: `over_read_cap`, `image_too_big` and `clipboard_image_too_big` each compute `cap / (1024 * 1024)`, `mime.strip_prefix("image/")`, a road and a `Some`/`None` pair of sentences whose `None` arm exists so a buffer's length is never named as the file's — and the three say that one truth three ways (“its size is not known” / “so its size is not known” / “its true size is not known”). One `past_cap(what, cap, size, road)`. | ≈ 14 | **Medium** — the clause is written three times, so the next reader who learns it, or forgets it, must find all three. | — | ⬜ |
| R39 | A window a human stated, read two ways: `parse_context_env` trimmed and the `--context` arm did not, so `MUSH_CONTEXT=" 8192"` was accepted while `--context " 8192"` was refused — a live contradiction, not a future risk — and the flag wrote its “needs a value”/“needs a token count” sentences by hand beside `number(value, flag)`. **Landed `1fd1933`:** one `config::parse_context(value, road)`; the environment door and the flag arm each pass their own road name, and `the_context_flag_and_the_variable_read_one_number_one_way` reads both. | ≈ −6 | **Already diverged** — the same number stated on the two roads a human has was accepted on one and refused on the other; nothing pinned the flag's behaviour to the env's. | — | ✅ `1fd1933` |
| R40 | Four attach subcommands, written out in six places: a `Cli` variant, a row in `ATTACH_FLAGS`, an arm of `Cli::detect`'s flag loop and of its construction match, `Cli::dir`, `Cli::request`, `Cli::run`, a printer and `help_text`, plus the two hand parsers (`parse_from` and `Cli::detect`) with separate value reading and separate `--` handling. One `Sub` row (name, flags, positionals, `build`) and a `Words` struct. **Verified:** `ATTACH_FLAGS`/`require_flag`/`require_once` already share the per-subcommand flag table, so the six places carry less plumbing than the count says. | ≈ 40 (high effort) | **Medium** — a subcommand advertised without a parser, or parsed without an advertisement; the refusals (A16, H26, A5) are pinned findings the table must keep. | — | ⬜ |
| R41 | `search` read without the bound the other two readers keep: after the `SEARCH_FILE_CAP` stat check it called `fs::read`, while `whole_read` and `image_at` read through `file.take(cap + 1)`. **Landed `0b42d69`:** one `read_bounded(path, cap)`, with `a_bounded_read_stops_at_the_cap`. | 0 (a fix, not a merge) | **Medium** — a file that grew past the cap between the stat and the read was read whole, the one thing the other two roads' bound exists to prevent (a race, not a live bug). | — | ✅ `0b42d69` |
| R42 | `thinking_default_hint` and `effort_default_hint` are the same paragraph twice: collect what the rows state, ask whether any row states nothing, return `no \`X\` field anywhere` if none does, else the join plus its “elsewhere” clause. One `default_hint(field, stated, elsewhere)`; a third `context_default_hint` sits between them with no elsewhere arm, so it is not part of the shape. | ≈ 5 | **Low** — a hint that drifts from the table lies in the home config's own header. | — | ⬜ |
| R43 | A named thread whose refused spawn is data, not a panic: `session_save`'s writer, `attach`'s accept loop, a connection thread that gives its slot back and `main`'s model discovery. One `named_thread(name, work) -> Result<JoinHandle<()>, String>`. **Verified:** the shape has more callers than the four — `agent.rs`, `jobs.rs` and `http.rs` also spawn named threads and treat a refused start as data. | ≈ 12 | **Low** — the thread-naming scheme and the never-raise rule get one home. | — | ⬜ |
| R44 | `.mush/paste/` was built one way and displayed another: the directory join and `Image.path` said `.mush/paste/<name>` while both write-failure messages said `cannot write .mush/<name>` — a live contradiction naming a file that was not there. **Landed `ce315a2`:** one `workspace::PASTE_REL` beside `paste_dir`/`paste_rel`, read by the join, by `Image.path` and by every message about a paste, with `a_paste_that_cannot_be_written_names_the_paste_directory`. | ≈ 4 | **Low for lines, live for trust** — two messages pointed at a path that did not exist. | — | ✅ `ce315a2` |
| R45 | `tools::arg_*` — `arg_string`, `arg_string_opt`, `arg_usize`, `arg_bool`, `arg_path` — five readers a shape scan keeps offering as one: each is a `match args.get(key)` with its own absent arm (`missing`, `Ok(None)`, a default), two quote the wrong value and three do not, and each sentence is one a model acts on. A generic reader costs ~15 lines to remove ~15 and risks rewording five pinned messages and defaulting paths the callers refuse. | 0 | **Judged and left on purpose** — one shape would trade five pinned sentences for one; the report's “the tests at the bottom pin it per function” is too kind, since `arg_usize` and `arg_path` have no test in `tools.rs`. | — | ⬜ judged |
| R46 | Three hand-rolled wire-failure classes — `Unsent`, `Framing`, `OverlongLine` — each a marker struct, a `Display`, an `Error` impl, a constructor boxing it into an `io::Error` and a downcast predicate, beside a classification order (`is_unsent`, then `is_framing`, then `InvalidData` as a refusal, then `transport`) that a fourth class makes five edits in. One `Wire` enum plus one `Marked(Wire, String)` marker and one `wire(error)` reader; the constructors and predicates stay as the call sites' vocabulary. | ≈ 26 (48 → 22 code lines) | **Medium** — the classes decide what may be retried after the request left mush (A2) and whether the endpoint “answered” (B27); a fourth marker whose arm lands after the `InvalidData` arm becomes a final `Transport` and the one retry it exists for is silently lost. | — | ⬜ |
| R47 | The once-only delivery rule, two books: `ActorState`'s `running`/`completed`/`delivered` and `running_jobs`/`done_jobs`/`delivered_jobs` with `record_child`/`record_job`, `note_completion`/`note_job`, `note_parked`'s `NO_RUN` re-arm and `fold_completions`' two folds. One `Delivery<K, R>` owner whose mark is a run, not a bare id. **Verified:** `absorb`'s adoption is no longer the quoted filtered snapshot — it scans `announced`/`announced_jobs` and only `delivered.entry(id).or_insert(run)` — and `wait_digest` filters through `ActorState::unread`; the rule, not every reader's shape, is what is copied. | ≈ 30 (25–35) | **Medium-high** — this is the B24 rule: `note_parked` has no job twin, and a merged mark that dropped the run would swallow a parked child's next report, while `note_completion` clears `running` only on a changed run and `note_job` always clears it. | — | ⬜ |
| R48 | “Answer every call in the batch, and tell the UI” four times — a reply cut at the token cap, a reply the endpoint refused, a run stopped as a loop and a cancellation between calls each push a `Message::tool` per call into `messages` and emit `AgentEvent::Message`, with `push_line`'s rule (“a line that reaches `messages` alone is one the human cannot see”, B20) rewritten at each site. One `answer_calls(actor, messages, calls, why)`. | ≈ 16 | **Medium** — a fifth road copied from one of the four that forgets the `emit` leaves the human's copy without the line, and the next idle `Run` replaces the actor's transcript with the UI's, so the loss is permanent. | — | ⬜ |
| R49 | The stdout+stderr join, two renderings: `command_report` and `jobs::preview` each test emptiness twice, spell the `--- stderr ---` heading and trim the ends, differing in the trailing newline against `tail_for_model`. One `streams_window(stdout, stderr)` beside `machine.rs`. Under it, the report's judged second half: `Job::output`/`tail` cap each stream while `preview` caps the joined text, so one command can give the model up to twice the cap as a foreground result and the cap as a job. | ≈ 13 | **Medium** — `preview`'s doc claims “exactly as a foreground result reads” and nothing tests the claim; the heading and the trim are kept in step by hand. | — | ⬜ |
| R50 | `kill -9 -pgid`, twice: `Running::kill` and `Running::end_group` ran the same eleven tokens through `scrub`, with mush's own streams nulled, differing only in whether the answer was read. **Landed `3bb1a5b`:** one in-process `machine::kill_group(group) -> Result<(), rustix::io::Errno>` (`rustix::process::kill_process_group`) is what both read — `Running::kill` through `Running.killed`, which makes a second call a no-op, and `Running::end_group` only where `group_members` found someone — with `ESRCH` read as “already gone” and anything else owed once through `Job::kill_failure` (`a_second_kill_signals_nothing`, `a_kill_without_the_kill_program_still_ends_the_group`, `a_failed_kill_leaves_one_sentence_in_the_window`). | ≈ 8 | **Medium** — a copy that lost `scrub` handed `MUSH_API_KEY` to the `kill` mush ran (C1), one that lost `Stdio::null()` printed into the TUI, and the fork/exec did nothing at all where `kill` is not on `PATH` (E6); nothing but reading the second copy caught the first two. | — | ✅ `3bb1a5b` |
| R51 | Who holds the machine, five sentences: the holder's own second claim, a queued sibling, the root and an unqueued sibling in `jobs.rs`, and `agent.rs`'s `machine_holding` and `beside_note`, all cutting the command with `truncate(&held.command, REFUSAL_COMMAND_COLUMNS)` in six places. One `Held::phrase()`/`Held::named()`. **Verified:** the report's “three of the five shapes are pinned” is stale — all five are read now. | ≈ 7 | **Medium** — the model reads the same hold described two ways (H13); a changed bound or a changed `#` spelling applied to four of five leaves two accounts of one hold in one conversation. | — | ⬜ |
| R52 | Five one-liners of arithmetic with two spellings: the ceiling in hours (`JOB_MAX_AGE.as_secs() / 3600` in `JobOutcome::line` and `end_note`; owner `JOB_MAX_AGE_HOURS`), the count of live jobs (`Registry::running`, `launch`'s inline count against `MAX_JOBS`, `finish`'s negated count; owner `Inner::running`/`ended`), the bounded-listing tail (`list_tool`/`search_tool`; owner `bounded_listing`, which the `unnamed` note left more parallel, not less), the body cap (`MAX_BODY_BYTES` in three readers; owner `body_room`) and the wait's poll slice (two `Duration::from_millis(50)`; owner `WAIT_POLL`). | ≈ 7 (each item ≤ 4) | **Medium** — the ceiling in hours is spelled twice and one edit makes a sentence lie; the running-job count has two spellings for the budget and one for the pane. | — | ⬜ |
| R53 | The model-failure sentence, translated twice — `run_loop`'s turn and `compact_history`'s `ask`: `CANCELLED`, “the endpoint's reply was refused”, `reply_broke`, “cannot reach” and “could not encode request” are verbatim in both (**five**, not the report's four), while the classes whose answer is the caller's (`Status`, `Malformed`, a cancellation) stay per-caller. One `transport_line(cfg, error) -> Option<String>`. | ≈ 7 | **Medium** — adding the endpoint's name (or a new class) in one path is a one-line edit with no test tying the two together, and `/compact` then reports a failure in different words from the run it belongs to. | — | ⬜ |
| R54 | The bounded wait loop's scaffolding: `wait_tool` and `wait_on_tool` each build `clock.now() + WAIT_TIMEOUT_SECS`, run the same `wait_tick` match on the two things that outrank the wait, set `state.waited` and `clock.sleep(Duration::from_millis(50))` — the only unnamed poll cadence left, which `BACKOFF_SLICE`'s doc already claims. One `Wait` owner for the deadline and the slice; what each waits for stays its own. | ≈ 3 | **Low-medium** — change one sleep and the two waits poll at two rates, and the fake clock advances by whatever it is handed, so no test measures either slice. | — | ⬜ |
| R55 | The attempt's budget against the transport's timeouts: `retrying` computed one deadline on the actor's clock and handed each attempt what was left, but `connect` used `CONNECT_TIMEOUT` per address, `write_all` could block for `WRITE_TIMEOUT` and `resolve_bounded` waited `RESOLVE_TIMEOUT` on the system clock — none min'd with what was left. (The report's “the deadline is computed twice” is half overstatement — `Watch::new` anchors on the handed `left`, the system clock there being its own deliberate choice — but the three syscall bounds were the live divergence.) **Landed `1dbea62`:** every per-phase bound is the smaller of its own ceiling and what is left of the call (`Watch::left`) — `CONNECT_TIMEOUT` per address, the write re-setting the socket's own timeout per chunk (`set_write_timeout`, `write_bounded`) with the watch checked between chunks, and `resolve_bounded` waiting `min(RESOLVE_TIMEOUT, left)` — and a phase that spends the budget answers as the deadline (`Watch::spend`), so the layer above classifies it as the deadline it is — a final `Transport`, not an `Unsent` failure to ask again with nothing left (`no_phase_outlives_the_calls_deadline`, `a_stop_lands_while_a_write_stalls`, `a_healthy_call_is_not_cut_by_the_ceilings`). | 0 | **High, and the reason the fix was needed** — an endpoint that accepts the connection and stops reading held one attempt ~30 s past its deadline (~45 s with connect and resolve), so “one ask spends one deadline” was false and a `Stop` the human pressed waited with it. | — | ✅ `1dbea62` |
| R56 | The two-step warning — armed, armed-again, kept true, disarmed — written twice: `arm_quit`/`arm_new_chat`, `disarm_quit`/`disarm_new_chat` and `refresh_quit_warning`/`refresh_new_chat_warning` each re-derive the line and re-spell “keep the clock of an arming already standing”, while the attach road hand-writes the two kinds. One `armed`/`arm_warning`/`refresh_warning`/`disarm` and `StatusKind::waits_for_a_second_press`. | +29 | **Drift** — the warning kinds are hand-written at the attach road while two functions each know one, and the keep-the-clock rule exists twice (H9, C4). | — | ⬜ |
| R57 | The image box weighed by two doors: `attach_images` (which calls the single door for a batch of one) and `attach_image` spell the same sums — `budget.saturating_sub(used_weight_for)`, the weight and byte folds over `self.chat.attachments()`, the running `over_budget`/`first_over` loop against the budget and `at_stake`/`first_at_stake` against the room — and `deliver` asks the same gate again at the send. One `Held::of`/`Held::plus` and `at_stake(images, from, bound)`, with `NO_MODEL_YET` for the literal written three times. | +18 | **Drift** — the three bounds are spelled twice, and the verdicts already differ in words: one picture over the window gets “even with every older turn dropped”, a paste of four gets “the pictures already in the box weigh {pending}”. | — | ⬜ |
| R58 | Three `AgentNode` constructors, one field list: `with_root`, `insert` and `register` write the same **fourteen** fields in three orders (the report says fifteen). One `blank(id, parent, depth, brief)`; the root is one line over it, the spawn three fields, the restore eight. | +12 | **Low** — the literals are exhaustive, so a new field fails to compile; the hazard is field order, not drift. | — | ⬜ |
| R59 | Four picker openers, one struct literal: `open_model_picker`, `open_notes_picker`, `open_help_picker` and `open_provider_picker` each build `Picker { kind, items, cursor }`, and the “open on the current value, else the top” rule is derived twice — by `model.id` once and by `item.id.as_deref()` the other. One `open_picker` and `PickerItem::choice`/`reading`. | +12 | **Low** — compiler-guarded; the cursor's default row is derived twice. | — | ⬜ |
| R60 | The help page's two columns, once in `keys::help_table_at` and once in `commands::table_at`: measure the left column, reserve `4 + w + 2`, wrap the description into what is left and hang every continuation under the description column, both over `wrap_text`. One `columns(rows, width)` beside `wrap_text`. **Landed `1afa368`:** one `mush_core::text::columns(left, left_width, description, width)` with one `MIN_DESCRIPTION_COLUMNS` (16) is what `commands::table_at` and `keys::help_table_at` loop over; below the floor the description hangs under its own left cell instead of wrapping to one column (`the_help_picker_keeps_a_readable_description_column`). | +10 | **Drift** — `4 + w + 2` and the continuation indent had two spellings, and only a wrapped description showed `/help` disagreeing with `mush --help`. | — | ✅ `1afa368` |
| R61 | “The run may not be labelled”: `AgentTree::activity` and `thinking` are the same eight lines — busy, then not folding, then the phase and its clock — and `nudge` asks the fold half of the guard. One `folding(id)`. **Verified:** `nudge` keeps only the fold guard and no busy check, so the report's three versions are two full copies and one guard. | +9 | **Low-medium** — a new setter that forgets “only a run in flight” or “a fold is never replaced” is a lie on a row. | — | ⬜ |
| R62 | The three dim footer lines: `agent_footer` cuts `detail`, `unread` and `jobs` to widths whose reserves (`2`, `2`, `14`, and `6` on the id line) are hand-counts of the label painted beside the text. One `footer_line(label, text, width)`. | +9 | **Minor drift** — `-2`, `-2`, `-14`, `-6` are hand-counts of the label beside them. | — | ⬜ |
| R63 | The refusal road of `deliver`: six roads say and return by hand (`self.fail(line)` then `Err(line)`). One `refuse(line) -> Result<(), String>`. | +8 | **Low** — a road that forgets `fail` refuses silently. | — | ⬜ |
| R64 | The bar's row budget derived three times: `CURSOR_LINE_COLUMNS = 72`, `UNKNOWN_NAME_COLUMNS = 36` and `QUIT_LINE_COLUMNS = 72` each re-spell the 80×24 row less the ` chat ` badge, and `JOB_TITLE_COLUMNS = 30` claims to be “the same bound as an agent's title” while `tree::TITLE_COLUMNS` is 24 — a false claim already in prose. One `BAR_ROW_COLUMNS = 72` with the others derived from it. | +6 | **Drift, already present in prose** — 30 against 24. | — | ⬜ |
| R65 | A host change's ack, twice: the `/url` and `/provider` arms each build `head · context_label` and append the ` · ` and `no_key_hint()` when the new host took the key, and the model ack `model: label · context_label` is the same string in `adopt_models` and `pick`. One `host_line(head, forgotten)`. | +6 | **Drift** — the `" · {context}"` tail and the optional no-key clause exist twice, and the model ack string twice. | — | ⬜ |
| R66 | A phase and its clock, written as a pair in eleven places: each setter writes the phase and `node.since = Instant::now()`. **A live lie:** `nudge` returned `Option<Phase>` and `nudge_failed` wrote back only `node.phase = was` — against its own doc, “Put the row back exactly as it was” — so a row that said `waiting on results 4m` said `0s` after a failed nudge. **Landed `9f0a12c`:** `nudge` returns the displaced phase *and its clock* (`Replaced`), and `nudge_failed` restores both; the broader `AgentNode::enter(phase)` refactor of the eleven setters is deliberately not done — this closed the defect only, and `a_nudge_that_cannot_be_delivered_restores_the_previous_phase_and_its_clock` reads `since` too. | +5 | **Live drift** — the restored age was the failed nudge's, not the phase's, and the doc and the code disagreed. | — | ✅ `9f0a12c` |
| R67 | Three endings of a run, one body: `finish`, `fail` and `stopped` each set `result_unread = node.parent.is_some()`, the phase, `since` and `agent_cancel.remove(&id)` (`finish` also `summary`). One `AgentNode::end(phase)`. | +4 | **Low-medium** — an ending that forgets `result_unread` silently drops the `✉` a parent's `wait` or fold reads. | — | ⬜ |
| R68 | The wire's “no agent #N”, spelled three times in `attach_read`, `attach_focus` and `attach_edit` (`format!("no agent {id}")`). One `no_agent(id)`; the membership check keeps its three lines, so the helper costs five. | −5 | **Judged and left on purpose** — it costs lines rather than saves them; the refusal is a contract three arms write out, and the three spellings agree today. | — | ⬜ judged |
| R69 | Below the line, five spans: `ConfigCell::learn_context`/`ConfigHandle::learn_context` (`believable` then `adopt_context`), the picker's two cursor clamps (`move_picker` against `set_picker_cursor`), `tree_walk` against `move_tree_cursor`, the two `take_error()` reads, and `Picker::title`'s `line {}/{}` in two arms. One `adopt_believable`, `Picker::set_cursor`, `walk_rows`, `take_save_error`, `Picker::position` — three of them zero-net consistency moves, worth doing only when the file is touched anyway. | +5 | **Low** — consistency moves, not savings; the only real bound is the picker's two clamps against two ends. | — | ⬜ |
| R70 | The message box: the ask and the paint are two sums — `input_rows` adds the capped draft lines, the capped attachment rows and the two borders, while `content_rows` reads the same sum from the other end with the one-row-for-the-text kept only there; both docs say “the two agree whenever the ask is granted”, which is the hand-kept invariant. One `box_rows(draft_lines, attached, granted)`. | −7 | **Judged and left on purpose** — it costs lines: change the `+ 2` and a box granted one row fewer than it paints loses a draft line and an image while the title still counts the image. Kept for the invariant, not the line count. | — | ⬜ judged |
| R71 | The chat column's split, twice: `screen`'s zen path and `chat_pane` both laid out `[Constraint::Min(3), Constraint::Length(self.input_rows())]`, and the zen comment claimed “its own split, not a re-derivation”. **Landed `bc581ba`:** `App::screen` computes the split once and all three arms read it — the two-pane chat is `chat_pane_with(rows[0], rows[1])`, the zen Chat arm widens `rows[1]` to the frame and the zen Agents arm reuses those rows — and `chat_pane` is gone; `zen_keeps_the_boxes_rows_at_every_size` reads the box's rows at 40×12, 40×10, 60×17 and 79×24. | −5 | **Judged, then landed** — the zen arm handed the box rows the two-pane layout did not, moving the box when the tree took the screen. | — | ✅ `bc581ba` |
| R72 | A share of the terminal, two integer types: `picker_width` computed `terminal_width * 60` in `u16`, `agents_columns` computed the same share in `u32`. **A real, narrow defect, not a style:** at 1093 columns the `u16` multiply overflowed — a debug build panicked on the multiply, a release build wrapped and the clamp quietly yanked the popup to its floor. **Landed `d56a10c`:** one `share(whole, percent, min, max)` does the multiply in `u32`, read by `picker_width` and `agents_columns`; `no_share_of_a_terminal_overflows_its_integer` sweeps 40..=2000 against the natural arithmetic. | 0 | **Real, narrow** — `terminal_width * 60` overflowed `u16` above 1092 columns; no test swept past 240. | — | ✅ `d56a10c` |
| R73 | The reap window and the park window count different populations: `past_history` ranks `eligible` (what the tree may drop, with an unread result outside the count) while `parkable` ranks every parented child, under a comment that says “the warm window and the reap window are one arithmetic” — so with a kept child the parker's window is larger, a child in the band is asked `past_window`, and a report the parent's model has not read stops protecting its thread. One `past(len, cap)` fed the same population by both. | −4 | **Judged and left on purpose** — costs lines; with fifty-one children and one unread result the reaper's `over` is 0 while the parker's `window` is 1. The wake path recovers the message, so the cost is a promise broken, not a line lost. | — | ⬜ judged |

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
one home, `workspace::backup_name` (since `0348028`), which answers the next free
name beside the file and leaves each caller its own rename and its own reason; no
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

### The seventh review (the transcript pane and the text it wraps, `b1f1642`)

Read blind — the area and nothing else — and read-only: the four production files
of the pane and the text it wraps (`app/chat.rs`, `ui.rs`, `mush-core/src/text.rs`,
`theme.rs`) end to end, 2 644 non-blank non-comment lines before each `mod tests`,
`awk` over the quoted spans for every count, a normalised `sort | uniq -c` over the
stripped lines for the shapes; no build, no test, no code changed. Landed directly
on master at `b1f1642`; its rows are `R30`–`R34`, **≈ 120 code lines of the
2 644 (4.5 %)** if all five land, with six more shapes measured below the bar
(−4, −3, −2, −2, 0, 0) so they are not re-found as savings. Nothing it proposed
changes output: the tagged walk, the shared breaker and the mark primitive are one
rule read by the same readers, and the only semantics riding on the copies are the
two shape-decisions the report named — the view chosen from the mark, and the fence
rule.

The two that can **drift** are the row map (`R30`: the fence rule and the reply's
view are two homes, and the `debug_assert`s that keep them in step compile out of
release) and the wrap itself (`R32`: the tab stop and break loop are two spellings,
pinned by one parity test over ten texts at widths 1..=24). The mark's drop rule
was on one road of three (`R31`). The report read a tree before the elided-tail and
fold waves: two of its named sites (`capped_result`, `tool_lead`, `SHOWN`, the
`reasoning_rows` and `"tool"` roads' own leads) are gone from the tree at
`38d0438`, and the fold's `folded_rows`/`folded_marked` already share one tagged
walk with `stops_at` and guard the mark on the folded road — so `R30`'s and `R31`'s
reachable saving is smaller than the report's estimate, while the fence restatement
and the reply's second decision are unchanged. Its own first move — add
`wrap_tagged`/`markdown_tagged_rows` to `mush-core/src/text.rs` and let `marked`
return the source line of every row it pushed — never landed.

**Looks duplicated but is not (do not re-litigate):** `fit_row`'s `MIN_FIELD = 7`
against `marked`'s `MIN_BODY = 4` (a tree row spends *fields* and drops one whole;
a message row keeps its words and drops the *mark*); `select_painted`'s band
against the two `highlight_style`s (a list's highlight paints a row's whole cells —
a place in the tree, with the picker's `› ` inside it — while the transcript's band
patches spans only, ends where the text ends and skips a blank row); the agents
list against the picker list (`Clear`, `highlight_symbol("› ")`, a hint row and a
rect from `pane.list_area`, against a row that must not have a symbol);
`NoticeKind::mark` against `Voice::mark` (a notice kind and a speaker; the `· `
they share is mush signing its own words, and `Voice::Mush` is dim precisely because
it is not a speaker); `rank_style` against `footnote_lines`/`NoticeKind::mark` (the
bar colours by *rank*, the foot by *kind*; `Notice::rank` is where the two
vocabularies meet); `FOOT_ROWS`/`FOOT_NOTE_ROWS` against the result cap
(`Fold::DEFAULT`/`Fold::shown`) and `MAX_TRANSCRIPT` (three caps over three
quantities with three owners); `lines_of` against `Stops::of` (different questions,
and `Stops::of` already calls `lines_of`); and `ui.rs`'s cursor clamps against
`content_rows`/`Input::view` (the box's arithmetic is derived once at the edge; the
painter's `min(saturating_sub(1))`s are backstops).

### The eighth review (the store, the workspace and the CLI, `50e4d9e`)

Read blind and read-only: every production line of fifteen files — `workspace.rs`,
`config.rs`, `git.rs`, `attach.rs`, `session.rs`, `tools.rs`, `main.rs`,
`clipboard.rs`, `transcript.rs`, `userconfig.rs`, `prompt.rs`, `provider.rs`,
`message.rs`, `lock.rs`, `session_save.rs`, ~4 600 production lines — test modules
read only as evidence of what a message must keep saying; nothing built, run or
changed. Ranked by value per unit of effort, not raw lines; landed at `50e4d9e`,
its rows `R35`–`R45`, **≈ 180 production lines, ~90 of them low-risk** (the `wire`
derive, `backup_name`, `past_cap` and `parse_context`).

It found **live contradictions, not future risks**: `parse_context_env` trimmed
while the `--context` arm did not, so `MUSH_CONTEXT=" 8192"` was accepted and
`--context " 8192"` refused (`R39`); the paste directory was built as
`.mush/paste/` while both write-failure messages named `.mush/<name>`, a file that
was not there (`R44`); and the same window was parsed in two trimming spellings.
Four of its rows have since **closed on master** — `mush/84` (merge `1e07c2e`)
landed `config::parse_context(value, road)` (`1fd1933`),
`workspace::{PASTE_REL, paste_dir, paste_rel}` (`ce315a2`),
`workspace::backup_name` and one `BACKUP_TRIES` (`0348028`), and
`read_bounded(path, cap)` (`0b42d69`), each with a test — and the rows carry the
landed commits in place of the proposed shapes. Its net-zero findings were the two
it singled out: `search`'s read was to be bounded like the other two readers
(`R41`, now landed) and `tools::arg_*` stays five readers with five sentences
(`R45`, judged). The report read before the workspace wave: the three `past_cap`
refusals moved and `search`'s match lines were rewritten, but every finding held.

**Looks duplicated but is not (do not re-litigate):** `IMAGE_FILE_CAP` and
`SEARCH_FILE_CAP` (both `2 * 1024 * 1024`, two threats — what goes on the wire,
what a walk opens in memory — and merging them makes raising one raise the other);
`read_file` (strict) and `read_window` (lossy) — already one road (`whole_read`),
the decoding passed in, and what is written back must not be lossily decoded (B6);
`truncate_for_model` and `tail_for_model` (opposite ends, and the two sentences are
the whole behaviour); `git()` and `run_named()` (a question with no answer against
a mutation the human must read; a shared `git_command` nets zero);
`parse_context_env`'s `> 0` against `parse_context_hint`'s `MIN..=MAX` (a human's
statement against an endpoint's guess); `session::Stored` against
`userconfig::Loaded` (the session's must not be flattened — S3; the home config's
may — C3); `now_secs` and `now_millis` (shareable as `epoch_millis()`, ~2 lines,
not a finding); `session_save::FLUSH_DEADLINE` against `attach::ASK_TIMEOUT`/
`IDLE_TIMEOUT` (three timeouts, three reasons); and `tools::arg_*`, which is the
judged row `R45`, not a merge.

### The ninth review (the actor, the tools and the wire, `b654717`)

Read blind and read-only: every non-test line of nine files — `agent.rs`, `http.rs`,
`model.rs`, `jobs.rs`, `machine.rs` and the whole of `signals.rs`, `events.rs`,
`clock.rs`, `ids.rs` — 4 865 non-test code lines; sites by `grep -n`, counted by
`awk`, shapes by a stripped `sort | uniq -c`; no build, no test, no change. Landed
at `b654717`; its rows are `R46`–`R55`, **≈ 117 non-test code lines** (range
~100–130; the wire classes, the delivery books, the batch answers and the stream
join are ~85 of it), ordered by lines removed per unit of risk — which is why the
delivery books and the wait loops sit below smaller candidates.

It turned up one **already-diverging arithmetic** (`R55`): `retrying` computes the
attempt's deadline on the actor's clock and hands each attempt what is left, but
`connect` bounds each address by `CONNECT_TIMEOUT`, `write_all` may block for
`WRITE_TIMEOUT` and `resolve_bounded` waits `RESOLVE_TIMEOUT` on the system clock
— none min'd with what is left — so "one ask spends one deadline" is false by up
to ~45 s and a `Stop` can land late; the row keeps only the live half (the `Watch`
anchor on the handed `left` was the deliberate choice the report's own not-list
names). The report's first move is the largest mechanical saving: the three
hand-rolled wire-failure markers become one `Wire` class and the classification
`model::chat` reads gets one home (`R46`). It read before the fold wave, which
touched `app/chat.rs` only: the fold still repeats the run turn's failure
sentences verbatim (**five**, not the report's four), so `R53` holds, while
`absorb`'s adoption has since been reshaped (`announced`/`announced_jobs`,
`delivered.entry(id).or_insert(run)`), which `R47`'s row says. The report judged
one behaviour change and left one: `bounded(ceiling, left)` should tighten the
three syscall bounds (`R55`), while the cap-at-two-levels ambiguity in `R49` is a
behaviour change for the human to call, not a blind edit.

**Looks duplicated but is not (do not re-litigate):** `Watch`'s deadline on
`clock::system()` against `retrying`'s on the actor's clock (deliberate — `clock.rs`
names http as one of the two places not handed a clock); `Scratch::read` against
`Scratch::read_tail` (a foreground result is a head read while the command still
runs; a job's kept window is a tail, where `test result: FAILED` lives); the cap at
the `Job` seam against the cap at the registry seam (two contracts with one
parameter name — the ambiguity is `R49`'s second half, not a merge);
`JobOutcome::line` against `end_note` (the human's completion line and the model's
bracketed note; the reasons are already shared through `jobs::Stopped`, R15);
`Outcome::line`, `digest`, `is_news` and `Committed::from` (four projections for
four surfaces; one table would couple the commit subject to the screen's marks);
`STATUS_COMMAND_COLUMNS`, `REFUSAL_COMMAND_COLUMNS` and `SUBJECT_COLUMNS` (three
`60`s with two stated reasons); `JOB_MAX_AGE`, `CHAT_DEADLINE`,
`WAIT_TIMEOUT_SECS` and `LOCK_QUEUE` (four questions that share numbers);
`MAX_JOBS` in `has_room` and again in `launch` (two *moments* of one question —
only the counting is `R52`'s); `wait_bounded` and `watch` passing `None` (each
watcher asks half the question because the other half is structurally unreachable);
`jobs::group_members` against `machine::group_members` (test-only, out of the
count); and the per-tool boilerplate itself (schemas, `arg_*`, `exec_tool`,
`result_cap` and `ToolOutput` already own it, and what is left per tool is its
sentence and its bound).

### The tenth review (the app and its panes, `38d0438`)

Read blind and read-only, in two delegated readers for `tree.rs` and
`screen.rs`/`settings.rs`, every span re-read at the line before quoting: the app's
state machine and its panes — `app/{mod,tree,screen,keys,commands,settings}.rs` and
`input.rs`, 9 228 production source lines of which 4 800 are code — at `f47bfdb`;
no docs, no audit, no build, no change. Landed at `38d0438`; its rows are
`R56`–`R73`. The pass's net is **≈ +112 code lines saved** (its positive nets +133
against four findings that spend −21), but it is entered for its bugs as much as
its savings: `nudge_failed` puts a phase back without its clock, so a row that said
`waiting on results 4m` says `0s` after a failed nudge, against its own doc (`R66`,
a live lie, and the row the report would fix first); `picker_width` multiplies a
`u16` and overflows above 1092 columns where `agents_columns` computes the same
share in `u32` (`R72`); and the parker and the reaper count different populations
under a comment that says "one arithmetic" (`R73`). Four findings cost lines rather
than save them (`R68`, `R70`, `R71`, `R73`) and are entered `⬜ judged`, each with
the contract or invariant it buys stated in the row. Nothing of the seven files
changed between `f47bfdb` and `38d0438`, so every span the report quotes still
stands as written; its count of `AgentNode`'s fields is fourteen, not fifteen
(`R58`), `nudge` carries only the fold guard and no busy check (`R61`), the three
"no agent #N" spellings agree today — the helper is a contract, not a drift
(`R68`) — and `stopped` does set `result_unread` (`R67`).

**Looks duplicated but is not (do not re-litigate):** `keys::picker`/`tree`/`chat`'s
movement arms (the same `j/k`, `g/G`, `PgUp/PgDn` rows into three `Intent` families;
no key is read twice, and that is easiest to see with one arm per pane);
`picker_width`/`picker_text_width` (the same derivation nested on purpose — the −6
is the border, the `› ` and the indent); `phase_glyph`/`phase_detail` (one phase
partition, two deliberate vocabularies, both matches exhaustive);
`Msg::Clipboard`/`Msg::Copied`/`Msg::Agent`'s conversation guard (three boundaries,
and `Msg::Models` guards an endpoint instead); the re-asks of a stale snapshot —
`adopt_git`'s `tree.has`, `sweep_worktrees`'s second `has` and
`reclaim_isolated`'s `worktree_in_use` (one decision asked at one moment, because a
remove must be one decision); `fork_base` read in `refresh_git` and in
`sweep_worktrees` (two moments, both through `App::fork_base` over
`agent::fork_base`); `help_table_at`/`table_at`'s rows (two different lists; only
the column arithmetic is `R60`); `Input::cursor_line().1` against
`Input::cursor_column()` (columns in the cursor's own line against columns from the
start of the box, with `window_line` the one spelling of what is visible); and
`StatusKind::Quit`/`NewChat` themselves (two variants are right — their *rules* are
one shape, `R56`).
