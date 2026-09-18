# mush — refactor plan: seams and owners

> Status: **Stages 0, 1, 2 and 3.5 complete.** On master: Stage 0 (all four
> moves), `AgentTree` (`app/tree.rs`), `Chat` (`app/chat.rs`), `ConfigCell`
> (`app/settings.rs`), the four seams (`ModelClient`, `Machine`, `Clock`,
> `Events`), the `#[ignore]`d actor tests rewritten in process, and Stage 3.5
> (`Intent` + parsed commands, `app/keys.rs` + `app/commands.rs`). Stage 3.4
> (`ToolHost`) was dropped with the editor: there is one dispatcher per side, not
> three. What is left of Stage 3 is the other half — the `Screen` value and the
> draw sweep that asserts painted text (B17). The delegation-honesty family (N1,
> N3–N6) is closed, and so is the wave this plan's checklist now also tracks
> (`findings.md` U1–U10, B20–B23).
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
the tool dispatch is one function per side (`agent::exec_tool` for orchestration
and shell, `agent::direct_tool` for the file tools).

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
holds the event loop), and `ui/mod.rs + view.rs` (Stage 3's `Screen` value).

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
and there is no live-buffer rule left to own: there is now **one dispatcher per
side** (`agent::exec_tool` for orchestration and shell, `agent::direct_tool` for
the four file tools) and no `ToolHost` trait. M3's socket server, if it lands,
calls those two directly rather than a buffer host.

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
  two dispatchers (`agent::exec_tool` for orchestration + shell,
  `agent::direct_tool` for the four file tools) — and with `ToolHost` dropped
  those two are the whole story, one dispatcher per side.

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
and the discard the UI advertises, and one `write_file` per turn in the
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
clock, and so are `wait_agents`' timeout and `Watch`'s deadline; the background
job, the real output cap and the real `git` stay, each with the reason in
place. Still on the wall clock: `INFO_TTL`/status ageing in `app/mod.rs` (whose
commit is another agent's) and `AgentTree`'s stale-cancel window, which already
has `age` for tests. The default suite still opens local mock sockets in
`http.rs`.

**Stage 3 — `Screen` view and intents.** Stage 3.5 ✅ — keys go through `Intent`
(`app/keys.rs`), and slash commands are parsed values (`app/commands.rs`), with
both help surfaces rendered from the one table (§3.5). The `Screen` half is **not
built**: `ui::draw(frame, &Screen)` where `Screen` is built by `App::screen()`,
so panes become pure functions of a value and the draw sweep can assert painted
text at every size instead of only "does not panic" (B17). *Done when* no render
function takes `&App`.

**Stage 4 — roadmap.** M2.8 = a job registry + `Machine`; M3 = the socket server
over the two dispatchers; M4 = a base revision on `Buffer` + merge in core
`text.rs`; M6 = undo inside `Buffer`. None of them should need to touch `app.rs`'s
routing. (M2.8 landed: `jobs.rs` sits on the `Machine` seam, and M4/M6's `Buffer`
is gone with the editor.)

Sequencing with the concurrent findings pass: Stage 0.1 (`transcript.rs`)
touches only `agent.rs` + `mush-core`, so it can start immediately. The stages
that touch `ui.rs`/`app.rs` should wait until that pass lands.

---

## 6. Checklist: finding → disposition → structural home

Status: ✅ verified closed, ⬜ open or unverified. The findings doc is the queue
of record — `docs/findings.md` is in the tree and every row below that has an
alpha-numeric/short id there is kept in step with it; this column says where the
*fix belongs*, so a row may appear here without a finding (a structural rule) but
not with a status that contradicts `findings.md`.

| ID | What | Status | Structural home |
|---|---|---|---|
| A1 | cancel/deadline only consulted on read timeout | ✅ | http `Watch` (regression test) |
| A2 | `tokens * 3` overflow | ✅ | `config::clamp_context` |
| A3 | `parse_context_hint` misfires / misses | ✅ | `config` (markers + range) |
| A4 | caps as floors; reserve > window | ✅ | `config::{read_cap, cmd_cap, list_limit, history_budget}` |
| A5 | window derived once; runtime switches never re-derive | ✅ | `git::rederive_context` on every runtime switch (`/url`, `/provider`, `set_model`, `set_base_url`), pinned in `config.rs` |
| A6 | CLI provider never selects its endpoint | ✅ | `config::resolve_with` test |
| A7 | no cap on response body | ✅ | `http` `MAX_BODY_BYTES` |
| A8 | localized git output parses as ±0 | ✅ | `git()` sets `LC_ALL=C` |
| A9 | model discovery runs even when a model is known | ✅ | `main.rs`: discovery is `config.model.is_empty().then(…)`, so a named model skips it, and it runs on `/model`, `/models` and `/url` |
| A10 | branch name read as an argv option | ✅ | `git` ref→sha guard |
| A11 | `adopt_context` accepts 1 | ✅ | `config::adopt_context` clamp |
| A12 | `Stat` is `u32`, git counts are 64-bit | ✅ | `git::Stat` is `u64` (`a_huge_diff_keeps_its_count`) |
| A13 | `--` does not stop flag parsing | ✅ | `main::parse_args` |
| A14 | a second positional silently replaces the first | ✅ | `main::set_dir` errors on a second directory (`only one directory may be given`) |
| A15 | unparsable `Content-Length` treated as absent | ✅ | `http` `InvalidData` |
| A16 | bad `MUSH_CONTEXT` silently ignored | ✅ | `config::parse_context_env` |
| A17 | `openai` alias sends the key to the LAN default | ✅ | `Provider::parse` (aliases removed) |
| A18 | git test hardcodes `master` | ✅ | `git` test `init_repo` |
| A19 | DNS resolution unbounded | ⬜ | `Clock` seam + `http::connect` |
| B1 | leftover worktree id collides with a fresh child | ✅ | `AgentTree::reserve_ids` — raised from the leftover scan (`discover_worktrees`), pinned in `tree.rs` |
| B2 | `mask_key` slices on a byte boundary | ✅ | core `text::mask_key` (3.6) |
| B3 | zero-row pane still focusable | ✅ | `App::below_floor` (`app/mod.rs`) refuses every intent but `Quit`, and `ui::draw` paints one notice below `min` — one `is_below_floor` predicate both read |
| B4 | at 40×10 the only transcript row is a blank | ✅ | `Chat::body` trims trailing blank separators before windowing (`trim_trailing_blanks`) |
| B5 | `Status` after `Done` restarts a finished agent | ✅ | `AgentTree::activity` ignores it after `Done`/`Failed`, and `busy` is derived from the phases (`tree.rs`) |
| B6 | failed/idle `Stop` leaves `Cancelling` + `busy` stuck | ✅ | `AgentTree::cancel_requested` (only a run in flight is marked) + the actor's end-of-run event as the ack |
| B7 | learned window never reaches the UI, then is clobbered | ✅ | `app/settings.rs` — `ConfigCell` + `ConfigHandle`; `learn_context` is the one mutator and it writes both sides |
| B8 | context meter ignores the human's own message | ✅ | `Chat::used_tokens_for` derives it on read, per conversation |
| B9 | `fit_row` budgets columns, `truncate` counts chars | ✅ | core `text` (3.6) |
| B10 | a failed nudge rewrites the node's phase | ✅ | `AgentTree::nudge_failed` |
| B11 | reaping a leftover leaves `focused` on a ghost | ✅ | `AgentTree::reap` + `repair_focus`; `discover_worktrees` reaps through it |
| B12 | an `Error` status loses to the activity line | ✅ | one precedence table (3.2) |
| B13 | a child's brief is never shown | ✅ | `Spawned` pushes the opening message |
| B14 | the row's summary is from the first run, forever | ✅ | `AgentTree::{begin, finish}` |
| B15 | `screen.py` mis-reads CSI / `--keys` escapes | ✅ | `scripts/screen.py` — cursor clamped to the grid (a row past the bottom, or a shrink under a low cursor, raised `IndexError` on the next `X`), and `--keys` decodes the escapes it means instead of `unicode_escape`, which turned `é` into `Ã©` and left `\e` literal. `--self-test` pins both. |
| B16 | `smoke.py --cancel` forks after starting a thread | ✅ | `scripts/smoke.py` — the pty is forked before the endpoint's thread exists; verified by running the scenario. |
| B17 | the layout sweep asserts "does not panic", not painted text | ⬜ | `Screen` view + the draw sweep (Stage 3) |
| B18 | `~` elision matches a prefix, not a directory | ✅ | `ui::facts_line` |
| B19 | global notices render into every transcript | ✅ | `Notice.agent` + `Chat::notices_for` — no unscoped read exists |
| N1 | `MAX_TURNS` turns "long" into "failed" | ✅ | `agent/run.rs`: `RUNAWAY_TURNS` + `LOOP_ROUNDS` (a run ends when it stops calling tools; only a *loop* ends it early) |
| N2 | message box is append-only and clips at the right edge | ✅ | `Input` (grapheme cursor + window), `Chat::key` owns the editing keys |
| N3 | a stopped child is reported to its parent as `#N done: cancelled` | ✅ | `agent::Outcome` (one enum, not a `summary == CANCELLED` string sentinel) |
| N4 | Ctrl-C stopped *every* busy agent, and blanked a stopped one to `Idle` | ✅ | `App::interrupt` (focused) + `Ctrl-X` (`interrupt_all`); `Phase::Stopped` |
| N5 | the one-non-isolated-sibling rule fails only *after* the brief is written | ✅ | `spawn_tool` message + the rule stated in `prompt` schemas and the system prompt |
| N6 | an interrupted run commits under the same subject as a finished one | ✅ | `commit_worktree` subject carries the `Outcome` |
| A20 | `needs_compaction` compares byte weights against a *token* budget, and `budget * 3 / 4` can overflow `usize` | ✅ | `mush-core::transcript::compaction_trigger` is saturating and the byte↔token conversion lives once in `Config::history_budget` (same shape as A2) |
| U1 | a working agent drawn as paused (`⏸` from "has children") | ✅ | `ui::phase_glyph` is a function of the node's own `Phase` only, and the children are a separate `⏸N` mark (`ui::agent_line`) |
| U2 | the pane title counts waiting agents as working | ✅ | `AgentTree::roster` derives `working`/`waiting` from the phases (`app/tree.rs`), and `ui::agents_title` names each count |
| U3 | another agent's news snaps a read pane back to the bottom | ✅ | `Chat`'s `reading` is per conversation (`Reading::Holding`), and `painted` marks a held window in the title |
| U4 | a grandchild drawn after everything spawned before it | ✅ | `AgentTree::rows` walks pre-order over the parent links |
| U5 | the newest activity on screen three times | ✅ | `App::tree_line` no longer repeats the activity; the bar says the napping root instead |
| U6 | an agent is a bare number | ✅ | `AgentNode::title` derives a handle from the brief (a path first, else the first non-filler word) |
| U7 | a waiting agent still says `working…` | ✅ | `Phase::waiting` (`app/tree.rs`) tells a model call from `wait_agents`/`wait_commands`, and the row/foot say which |
| U8 | a transient notice never leaves | ✅ | `Chat`'s chatter lifetime (`clear_notes_for`, `dismiss_said`, `SAID_TTL`) + repeat collapse (`Notice.count`) |
| U9 | the shipped DeepSeek window/reply cap is too small | ✅ | `provider::PROVIDERS` fallback 120 000 + `Config::reply_cap` (a quarter of the window, floored at 1 024 and capped at 120 000) |
| U10 | walking back up a deep tree costs a keypress per ancestor | ✅ | `Intent::TreeWalk` on `←`/`→` (`app/keys.rs`) + `PickerMove(±PAGE)` |
| U11 | compaction has no visible state anywhere | ⬜ | `agent.rs` (state on accept), `app/tree.rs` (`Phase::compacting`), `ui.rs` (glyph + row/bar/foot), `App` ("you can keep typing") — fix in flight (`mush/38`) |
| B20 | a child's completion reaches the model but not the screen, and can fold twice | ✅ | `agent::push_line` emits `AgentEvent::Message` with the fold, and `absorb` marks the adopted line delivered |
| B21 | a parent in a tool-calling chain never heard its child finish | ✅ | the fold runs at every message boundary (`fold_completions`), not only on the tool-free turn |
| B22 | a steering message to a subagent is invisible / an idle target not woken | ✅ | `AgentMsg::Steer` → `push_line` (delivered and emitted), and it is work to answer; the reply wording left over is H5's |
| B23 | a transient transport failure ends the run instead of being retried | ✅ | `model.rs::retrying` — `RETRY_ATTEMPTS = 3` over the `Clock` seam, transport failures only, each retry announced in the transcript |

---

## 7. Design-doc sync

Done in the doc-sync wave that closed this plan's checklist. What `docs/mush.md`
was asked for, and what it now says:

- §1: the line count — the design doc claimed "roughly 5,000" /
  "roughly 9,000" lines; it now says ~32,000, the real total of the two crates'
  sources including tests.
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
   smaller. Open — the `Screen` value it belongs to is the unbuilt half of
   Stage 3.
3. Should `AgentTree` own `agent_stats` (git facts) too, or does the git snapshot
   stay a separate value refreshed by events? Today the coupling is one-way
   (`refresh_git` reads the tree), so it probably stays separate.
