# mush — design doc (v0.1)

> A small, fast, agent-agnostic terminal editor. Open a folder, talk to an
> agent, and watch it edit the files you have open — without either of you
> clobbering the other.

Status: **MVP implemented and working end to end** (M0–M2.5 of §9). This document
describes what is actually built, then what comes next. Decisions are marked
`[DECIDED]` or `[OPEN]`.

This revision folds in two audits: the agent contract (§3, §5.5) and the screen,
photographed at six terminal sizes (§4.5). Their functional findings are §2's
known gaps; the work they imply is M2.6–M2.7 in §9.

---

## 0. TL;DR

- `mush` is a modal TUI editor in Rust. One binary. **No async runtime.**
- Workspace-first: `mush [DIR]`, or just `mush` in the folder you are in.
- An **agent is built in**: it talks to any OpenAI-compatible endpoint
  (default `http://rubendpc:8078`) and edits the workspace through five file
  tools; it can delegate work through four more.
- Agents *drive* the editor: file tools execute on the UI thread, so an agent
  edits the **live buffer**, not a stale copy on disk. The human sees edits land.
- The agent's **system prompt is ~10 lines** and the tool set is nine functions.
  Simple prompt is a consequence of a small, honest interface.
- Everything mush writes lives in `<DIR>/.mush/`, which **git-ignores itself**.
- Architecture is a single-owner **event loop**: `Msg` in, `App::update`, `ui::draw`.
- KISS is enforced by the dependency budget of §7.

---

## 1. What mush is / is not

### Is

- A **text editor first**: open, navigate, edit, save. Keys stay out of the way.
- A **live collaboration surface** between one human and the built-in agent.
- **Endpoint-neutral**: anything speaking the OpenAI chat-completions API with
  function calling works (llama.cpp, Ollama, vLLM, LM Studio, hosted APIs).
- **Small on purpose.** Roughly 5,000 lines including tests, across two crates.

### Is not

- A full IDE. No debugger, no terminal multiplexer, no project wizard.
- A CRDT / collaborative-OT server. One human, the filesystem is truth.
- Provider-specific, plugin-based, or extensible via a scripting language.
- An agent framework. It ships one small agent loop, not an orchestration layer.

### Why files + a shell is still the interface

The agent's tools are `list_files`, `read_file`, `write_file`, `edit_file`, and
`run_command`. That is the entire surface for touching a workspace — plus four
delegation tools that are only about other agents. Any other agent — a shell
script, a different harness — can collaborate through the same two things: the
workspace files and the shell. A richer attach protocol is planned (§9) but is
not required for mush to be useful today.

---

## 2. The central trick: agents edit live buffers

The naive design has the agent read and write files on disk while the human has
the same file open in memory. Whoever saves last wins, and work is lost.

mush avoids this the simple way: **only the UI thread touches editor state.**

- The agent runs on a background thread and asks the UI thread to run file tools
  (`Msg::Tool`), blocking on a reply channel.
- `read_file` therefore returns the *live buffer* when the file is open, so the
  agent sees unsaved human edits.
- `write_file` / `edit_file` apply to the live buffer and save it, so the human
  sees the agent's edit immediately.
- `run_command` runs on the agent thread (a slow build must not freeze the UI),
  and afterwards the agent asks the UI to re-read clean buffers (`Resync`).

This is why there are no locks around editor data. There is only one owner, and
ownership is expressed as a message.

### Safety rules that follow from it

- **Atomic saves.** Every write is temp-file + `rename`; readers never see a
  half-written file, and a crash cannot corrupt the original.
- **Workspace jail.** Every agent path is resolved against the root and rejected
  if it escapes (`..`, absolute paths). The agent cannot touch `/etc`.
- **Edits are exact.** `edit_file` refuses if `old_string` is missing or appears
  more than once, so an edit can never hit the wrong occurrence.
- **Truncated reads are capped, edits are not.** Reads to the model are capped
  for context; edit operations always work on the complete file.
- **Bounded loops.** At most 24 model turns per request; a shell command runs in
  its own process group with a 120 s timeout, a hard 8 MB output limit, and
  cancellation that reaches it; delegation is bounded in depth and fan-out
  (§5.5). A runaway agent stops. (M2.8 turns that 120 s kill into a 60 s detach —
  §5.6.)

### Known gaps

**1. Unsaved buffers vs `run_command` (M4).** If `run_command` rewrites a file the
human has unsaved changes in, those changes are preserved (the buffer is left
alone) but the file on disk has moved on. A filesystem watcher plus a diff3 merge
is the planned fix (§9).

**2. The subagent pane is display-only.** A child's chat shows its replies and tool
results, but not the parent's brief, the human's nudges to it, or the `#N done`
lines it receives from its own children. The model sees all of them; the pane does
not, so the human's picture of a child is thinner than the child's own context.

---

## 3. The agent contract

### System prompt

`mush-core/src/prompt.rs` generates the entire prompt. It is one paragraph plus
the delegation rules:

```text
You are mush, a coding agent working in the workspace at <ROOT>.

Use the tools to inspect and change files. Rules:
- Read a file before you edit it.
- Prefer edit_file for small, surgical changes; use write_file only for new files or full rewrites.
- Do the work instead of describing it. Keep replies short.
- Never touch paths outside the workspace.
- When the task is done, stop calling tools and reply with a one-sentence summary.

Delegation:
- spawn_agent(brief, isolated?) starts a subagent that has NO memory … the brief must carry every fact …
- Delegate independent, large, or context-heavy subtasks; do single edits and lookups yourself. Prefer a few big delegations over many small ones.
- wait_agents blocks until a child finishes … agent_status lists your children; agent_control stops or messages one.
- Ending your turn while children still run is fine: they keep working and you are woken …
```

A subagent gets its own system prompt: who it is (depth), the workspace it works
in (the shared one, or its isolated worktree), and the same rules — the brief
travels as the first user message, mirroring the root's system+user shape.

### Tools

| Tool | Arguments | Notes |
|---|---|---|
| `list_files` | `path?` | recursive, skips `.git`, `.mush`, `target`, `node_modules`, … |
| `read_file` | `path` | live buffer if open; capped at 16 KB per result |
| `write_file` | `path`, `content` | atomic; creates parent directories |
| `edit_file` | `path`, `old_string`, `new_string` | exact and unique match required |
| `run_command` | `command` | `sh -c` in the root, own process group; 120 s timeout, 8 MB output limit, cancellable |
| `spawn_agent` | `brief`, `isolated?` | a new agent with its own transcript (and worktree) |
| `wait_agents` | `ids?`, `timeout?` | blocks until a child finishes; returns its summary |
| `agent_status` | — | lists the children and their state |
| `agent_control` | `id`, `action`, `text?` | stops or messages one child |

The last four are omitted from a leaf agent's schema (`MAX_DEPTH`), which is what
bounds the tree. `mush_core::tools::TOOL_NAMES` is the single list of names, and
a test asserts the schemas match it. (M2.8's job tools are workspace tools, not
orchestration ones: a leaf may run a build in the background while it edits —
§5.6.)

Tool calls execute as a normal OpenAI function-calling loop: the assistant
message, then one `role: "tool"` message per call, then the next request. Every
call in a batch is answered before anything else happens, because an assistant
message with unanswered tool calls makes most servers reject the whole
conversation. If a model answers without calling a tool, it is done — unless a
child's result or a parked nudge is waiting, in which case the run continues so
the model actually answers it.

Transcripts adopted from the UI are **repaired, not trusted**: the human may have
typed while a tool batch ran (their words land between the calls and the results),
and a quit mid-batch can leave calls with no results. `repair_tool_pairs` moves
results back beside their assistant message and answers whatever is still missing
before the request goes out.

### History budget

Small local models have small contexts (the default endpoint reports 8 K). Every
request reserves room for the tool schemas, the reply, and a margin
(`Config::SCHEMA_TOKENS`; a prompt test keeps the schemas inside it). Before
each request the agent trims the oldest turns until the conversation fits, always
cutting at a **user** message boundary so assistant/tool pairs stay valid.

Trimming drops information, so it is the fallback, not the first move: once the
transcript passes three quarters of the budget the agent asks the model to
summarize everything important and continues from `system + summary`. That is
what lets a long task survive a small context window.

---

## 4. The editor

`[DECIDED]` Normal + Insert modal editing. No Vim operators, no selections yet;
the smallest thing that is genuinely usable.

| Context | Keys |
|---|---|
| anywhere | `Tab`/`Shift-Tab` cycle panes · `Ctrl-Q` quit · `Ctrl-S` save · `Ctrl-R` reload · `Ctrl-N` new chat · `Ctrl-C` cancel running agents (reaches a model that is still thinking) |
| agents | `j`/`k`, arrows, `g`/`G`, `Enter` focus a row, `c` cancel that agent, `Esc` back to the root |
| editor (normal) | `i` `a` `I` `A` `o` `O` insert · `hjkl`/arrows · `0` `$` `g` `G` · `Ctrl-D`/`Ctrl-U` · `x` delete |
| editor (insert) | typing, `Enter`, `Backspace`, `Delete`, arrows, `Esc` to normal |
| chat | typing, `Enter` send, `Backspace`, `↑`/`↓`/`PgUp`/`PgDn` scroll, `Esc` clear · `/new` `/help` `/quit` · `/model` and `/provider` open a picker |

A `Buffer` is `Vec<String>` lines plus a `trailing_newline` flag, so files
round-trip byte-for-byte. Cursor columns are counted in **characters**, so
multi-byte text edits correctly.

---

## 4.5 The screen: reachable is not glanceable

`[DECIDED]` Beyond editing, the UI has one job: answer three questions without
typing anything, at whatever size the terminal is.

| Question | Today | Where the data already is |
|---|---|---|
| What is each agent doing? | Nothing. A row reads `◐ #1 delegate file c…`, and only the *focused* agent's status reaches the status bar. | `AgentNode::last`, rendered only while a node is running |
| Where is its work, and on what branch? | Nowhere: the row is `brief + activity + branch`, truncated in that order, so the branch is always the first field lost. | `AgentNode::branch` |
| How much has changed? | Not implemented — no `git diff`, `--shortstat`, or `status` call exists. | — (M2.6 made the branches real, so a per-agent stat can be computed) |

An audit of real screens (200×50, 120×32, 80×24, 60×17, 40×10, 30×8) found ten
defects. They share one shape: the data exists, the pixels do not.

1. **A dead instruction — fixed in this revision.** The empty editor said "Tab to
   files"; there is no files pane. Copy that cannot come true is worse than none.
2. **Row fields are ranked backwards.** `brief(18) + activity(14) + branch`, then
   truncated to the pane, so identity, activity, and branch never coexist — and the
   branch, which the merge story depends on, is at the tail.
3. **The selected row loses two more columns.** `List` draws `› ` outside the
   item's width budget, so the row being read is the one that gets clipped.
4. **`✓` is a lie for an idle agent.** Any non-running, non-error node renders as
   done, including the root before it has ever run.
5. **The empty-state hints scroll off first.** Only the last `height` transcript
   lines are kept, so at 60×17 the "Ask for a change" line is gone and the key
   hints survive.
6. **The status bar truncates the wrong end.** The model label and key hints
   survive; the branch and stat that M2.7 adds would not.
7. **Layout ignores size.** Fixed percentages, no floor: at 40×10 the message box
   is zero rows tall (typing works, nothing renders); at 200×50 the transcript runs
   198 columns wide.
8. **Tool calls are raw JSON** truncated mid-string
   (`⚙ spawn_agent({"brief": "…`), with no grouping of call and result — while
   `agent::summarize` already produces the right label for the tree.
9. **Red `!` for non-errors.** `/diff`, `/merge`, and `/help` output lands in the
   transcript styled as failure.
10. **No environment facts.** Workspace path, branch, and dirty state are nowhere,
    so `mush` in the wrong directory looks exactly like the right one.

### The plan (M2.7)

Four rules, no new panes, no new dependencies, and `ui.rs` stays dumb — all values
are computed in `App`.

- **R0 — Derive, don't store.** `[DONE]` The three defects above were one bug: the
  screen kept *conclusions* (a status string, a `running` flag) instead of *facts*.
  An agent now has a `Phase` (`Idle · Thinking · Activity(label) · Cancelling ·
  Done · Failed`) and the instant it began; the row glyph, the activity text, and
  the bar are derived from those every frame. `App::status` is a typed line with a
  lifetime: `Info` fades after five seconds, `Error` stays, and work in progress is
  never stored at all — which is what makes `✓` on an idle agent and a lingering
  `thinking…` impossible rather than merely fixed.

- **R1 — Ranked fields, then a footer.** One row per agent, spent left to right in
  priority order (`glyph · id · brief · activity · branch · stat`); the *selected*
  row's full facts get a 1–2 line footer under the list. A narrow pane degrades to
  `◐ #2`; facts move, they do not disappear.
- **R2 — One workspace bar, fixed field order.** `focus │ branch ±dirty +add −del │
  model │ hints`, truncating only at the far right.
- **R3 — Size tiers with a floor.** `w<40 || h<10` → a single centred
  `mush needs at least 40×10`; a compact tier that puts the agent strip above the
  chat and hides an empty editor; the present three panes at 80×20 and up; a
  capped, centred transcript (≈110 cols) on very wide terminals.
- **R4 — Truthful glyphs.** `[DONE for the row]` `·` idle/never ran, `◐` running,
  `⏸` waiting on children, `⊘` a cancel in flight, `✓` finished, `✗` failed, `⑂`
  isolated, with a legend in the pane title (the legend and `⑂` still to come).

```
┌ agents · 2 running · Σ +324 −40 ────────────┐
│   ◐ #0 you        edit src/lib.rs           │
│ ▸ ⏸ #1 lexer      wait_agents               │
│     ◐ #2 tests    write tests/lex.rs        │
│   ✓ #3 docs       wrote README.md           │
├─────────────────────────────────────────────┤
│ #2 tests · 0:41 · ⑂ mush/2 +8 −0            │
│ .mush/wt/2 · git diff HEAD...mush/2         │
└─────────────────────────────────────────────┘
```

The plumbing is the whole cost: a `mush-core/src/git.rs` (~80 lines plus tests,
shell-outs like the worktree code, no new crates) exposing `status(dir)`,
`branch(dir)`, and `shortstat(dir, base)`, plus one cached `App` snapshot refreshed
on agent `Done`/`Error`, on writes and saves, and every ~2 s while busy.

Order: M2.6 first — a branch with no commits makes the per-agent stat meaningless —
then R1+R2+R4, then R3, then the transcript polish (items 8 and 9).

---

## 5. Persistence: everything in `.mush/`

```
<workspace>/
  .mush/
    .gitignore     # contains a single line: *
    session.json   # the conversation, model, provider, and endpoint
    wt/            # isolated agents' git worktrees (when used)
```

The API key is never stored here — it lives in the machine-global home config
(`$MUSH_CONFIG`, else `~/.config/mush/config.json`), set with `/key` or
`MUSH_API_KEY`.

Resolution order on startup: CLI flags > env > saved session > home config >
built-in defaults.

`.mush/.gitignore` containing `*` ignores every file in the directory,
**including itself** — so the directory never shows up in `git status` and never
needs to be added to the project's own `.gitignore`.

`session.json` is rewritten after every message, so quitting (or crashing) loses
nothing. On startup the conversation resumes where it left off. `/new` clears it.

---

## 5.5 Subagents: actors, not a framework

Agents are one thread each, owning one transcript; parents and children talk
directly through mailboxes (`spawn_agent`, `wait_agents`, `agent_status`,
`agent_control`), while the UI observes via id-tagged events. Depth and live
count are hard budgets; the delegation tools are simply omitted from a leaf's
schema. Agents in the main workspace keep the live-buffer rule (file tools
round-trip through the UI thread); `isolated` agents edit their own git
worktree directly on disk. Human-in-the-loop is the merge story: a run's work is
committed to its branch when the run ends, mush prints the git commands, and it
never auto-merges.

An orchestrator that ends its turn while children still run is not finished, it
is napping: the completion is folded into its transcript as a user message and
the run restarts, so a result is never lost just because nobody called
`wait_agents` in time.

Two different messages end an agent's work, and the difference matters:
`Stop` cancels the run in flight (Ctrl-C, `c` on a running row) and leaves the
actor alive to be nudged again; `Shutdown` ends the actor (`/new`, Ctrl-N). An
actor holds a handle to its own mailbox, so it can never infer that everyone
else let go — it has to be told. Every event carries the conversation it
belongs to, so an actor that is still finishing a request when the human starts
a new chat cannot write into it.

A `Stop` has two halves, because one of them cannot wait for a mailbox: the
message reaches the actor, and the flag it sets is *shared with the UI* when the
run starts (`AgentEvent::Running`). The HTTP reader polls that flag between
short socket slices, so Ctrl-C interrupts a model that has not answered yet —
the mailbox alone would be read only after the reply. The row shows `⊘` while
the cancel is in flight, and a fresh run clears it.

---

## 5.6 The machine is shared: jobs, detachment, one lock

A worktree isolates files and nothing else. Two agents on the same box share CPU,
memory, IO, ports, caches, services, `/tmp`, the network, and the human's
patience. The worry that provoked this section — a subagent tuning performance
while a sibling hogs every core — is not a worktree problem, it is a *machine*
problem.

| A worktree isolates | Shared anyway |
|---|---|
| files, HEAD, index, branch | CPU, RAM, IO, GPU |
| history until it is merged | ports (mush's own tests bind 18731…) |
| | build caches, package stores, `~/.cargo`, `/tmp` |
| | databases, containers, dev servers |
| | wall-clock, so every timing measurement |
| | tokens — invisible, and the one with a bill |

`[DECIDED]` Three rules follow.

**1. Long commands detach; they are not killed.** A command that outlives
`CMD_DETACH_AFTER` (60 s) stops being a tool call and becomes a **job**: mush
answers `[still running — detached as #c2; you will be told when it finishes]`
and the process keeps going in its own process group. `detach: true` asks for that
from the start (`npm run dev`). This replaces today's 120 s kill, which was exactly
wrong for a fresh worktree's cold build: the agent lost the build, read a timeout,
and usually started over.

**2. A job is a second-class actor.** It has an id, a command, an owner, a start
time, an exit status, and a bounded window of output. `command_status` lists them,
`command_control {id, action: stop}` ends one, and `CommandDone` lands in its
owner's mailbox exactly like `ChildDone`: it wakes a napping agent, is delivered
once, and folds in as `#c2 done: exit 0 · 3m12s · cargo test — test result: ok. …`.
`wait_commands {ids?, timeout?, all?}` mirrors `wait_agents`, which gains the same
`all` — ask for every result instead of the first one.

Jobs are budgeted (`MAX_JOBS`, beside `MAX_AGENTS`) because each is a thread, a
process group, and disk. They die with their agent (`Shutdown`, `/new`), with mush
itself — its process groups are killed on exit, where today a build an agent
started outlives a clean quit — and with a `Stop` aimed at their owner, because
Ctrl-C means “stop the work in flight”, and a job is work in flight.

**3. One command at a time may own the machine.**
`run_command({command, exclusive: true})` takes a workspace-wide lock.
Timing-sensitive work — benchmarks, profiling, `--test-threads=1`, anything that
binds a fixed port — then runs without a sibling stealing cores or a port, and
everyone else is told `#3 holds the machine; retry when it finishes` instead of
silently interleaving. A detached exclusive job holds the lock for its whole life.
The lock coordinates *agents*; it cannot see the human's own build or an unrelated
process, so it is “agents do not fight each other”, not isolation.

`[OPEN]`, to settle in §11: the shape of the job and token budgets, where the human
sees jobs at all, and whether a job's kept output is a head (what the model reads
first) or a tail (what a crash left behind).

---

## 6. Architecture

Single-owner state. No locks. No async runtime.

```
                       ┌──────────────── UI thread ────────────────┐
  crossterm events ───▶│  Msg::Key ─┐                              │
  tool replies     ───▶│  Msg::Tool ─┼─▶ App::update(&mut self, Msg) │   6. crates
  agent events     ───▶│  Msg::Agent ┘            │                │
                       │                          ▼                │
                       │            terminal.draw(|f| ui::draw(f, app))
                       └───────────────────────────────────────────┘
                                   ▲       │
                                   │       ▼  Msg::Tool (file ops)
                       agent thread: model HTTP loop
                       └ also runs `run_command` directly
```

- One `crossbeam` channel carries every input into the UI thread.
- The loop drains messages, polls the terminal for ~30 ms, ticks, and redraws
  **only when something changed** — including on a resize, which schedules its
  own redraw.
- `App::update` is the single entry point; `ui::draw` only paints.
- An agent thread waits for work (`wait_for_work`), then loops model → tools →
  results for one run, folding anything that arrives meanwhile into the
  transcript.

### Why no `tokio`

The workload is a handful of short, blocking tasks (HTTP, a shell command, a
pty later), not thousands of connections. Synchronous code is linear and
readable; there is no `async fn` coloring and no runtime to debug at 2 a.m.
Threads plus channels give exactly the ownership story we want, at ~0 startup
cost. **Fast includes fast to reason about.**

### Crate layout

```
mush/
  crates/
    mush-core/   # pure domain: config, messages, prompt, session, tools, workspace. No UI.
      config.rs      endpoint/model/api-key resolution (the startup precedence)
      message.rs     OpenAI-compatible message + request/response types
      prompt.rs      the system prompt and the tool schemas
      session.rs     `.mush/` creation and conversation persistence
      tools.rs       tool names, argument helpers, exact-match edit semantics
      userconfig.rs  the machine-global config file (where the API key lives)
      workspace.rs   path jail, listings, capped reads, atomic writes
    mush/        # the binary: TUI + agent
      main.rs        CLI, terminal guard/panic hook, event loop
      app.rs         state, update, key handling, tool execution, buffers
      agent.rs       agent actors, model loop, tool dispatch, shell execution
      http.rs        ~300-line blocking HTTP/1.1 client
      ui.rs          layout, panes, transcript rendering, word wrap
  docs/mush.md
  scripts/          pty smoke test + scripted mock model server
```

Dependency direction is one-way and strict. `mush-core` knows nothing about
terminals or sockets; the TUI knows nothing about HTTP. The one exception is
deliberate: the hard part (path safety, atomic writes, history trimming,
wrapping) is where the tests live.

---

## 7. Dependencies (the whole budget)

| Crate | Why |
|---|---|
| `ratatui` | TUI layout and diff-based rendering |
| `crossterm` (via ratatui) | keyboard events, raw mode, alternate screen |
| `serde`, `serde_json` | messages, session file, tool arguments |
| `crossbeam-channel` | one channel, `.select()`-ready |
| `unicode-width` | correct wrapping and cursor columns for wide glyphs |
| `rustls`, `webpki-roots` | TLS for hosted https endpoints (DeepSeek); the client stays hand-rolled |

Not used, on purpose: `tokio`, `reqwest`/`ureq`, `clap`, `ropey`, `notify`,
`anyhow`, `blake3`, `diffy`. HTTP is hand-rolled because the target is a
plain-HTTP server (or a rustls-wrapped socket), and ~150 lines beats a
dependency tree. CLI parsing is ~40 lines. Each omitted crate is one less thing
to version, audit, and wait for.

---

## 8. Performance

| Metric | Target | Reality |
|---|---|---|
| Cold start | < 20 ms | ~2 ms without model discovery; ratatui enter/leave and one redraw |
| Model discovery | < 50 ms | one `GET /v1/models` (~20 ms cold, ~4 ms warm); skipped with `--model` |
| Keypress → screen | < 5 ms | `update` touches only the buffer; draw only when dirty |
| Idle CPU | ~0% | blocked on a 30 ms poll, no spinner unless an agent is running |
| Memory, no open files | < 15 MB | a `Vec<String>` per open file and a message list |

An unreachable endpoint cannot hang startup: connections are bounded by a 5 s
`connect_timeout`, and the model list by a 10 s read timeout — after which the
editor opens and reports no model. A chat completion, by contrast, may take as
long as the model needs: one 10-minute deadline bounds the whole request, while
the socket itself is read in 200 ms slices so the reader can notice a
cancellation. Ctrl-C therefore stops a model that has not answered instead of
waiting for its reply, and a wedged endpoint still cannot pin a thread forever.

Rules: no full-buffer scan per frame, no redraw without a state change, no
allocation in the input path beyond the edit itself, and no subprocess inside
`draw` — the git snapshot of M2.7 is cached in `App` and refreshed by events.
Release profile uses `lto = "thin"`, `codegen-units = 1`, `strip = true`.

---

## 9. Roadmap

**Done**

- **M0 — Core.** workspace path jail, atomic writes, session persistence, message
  types, prompt/tool schemas, config resolution.
- **M1 — Editor.** open/edit/save, modal keys, panes, transcript, wrapping.
- **M2 — Agent.** OpenAI function-calling loop, live-buffer tool execution,
  streaming-free status spinner, cancellation, history trimming, `Resync`,
  context compaction.
- **M2.5 — Subagents.** actor-per-agent with mailboxes, delegation tools, the
  agent tree, isolated git worktrees, wake-on-completion, bounded depth and
  fan-out.
- **M2.6 — Honest worktrees.** An isolated run ends by committing its worktree
  (`mush #N: <brief>`, synthetic identity, hooks skipped), so the branch carries
  the work and `/diff`, `/merge`, and `/discard --force` do what they say. The
  pty test asserts all three.

**Next**

- **M2.7 — Glance layer (P1/P2).** Ranked agent rows with a selected-row footer;
  one workspace bar (focus · branch ±dirty · ±stat · model · hints); size tiers
  with a 40×10 floor and a width cap; truthful glyphs; tool calls as
  `name(summarized args)` with their results grouped; neutral notices; `/open` as
  a picker. Backed by `mush-core/src/git.rs` and one cached snapshot (§4.5).
- **M2.8 — Concurrent work (jobs + one lock).** Detach long or explicitly
  detached commands into a job registry with ids, status, control, and the same
  completion-wake lifecycle as subagents; `all` waits for agents and commands; a
  workspace-wide `exclusive` lock for timing- and port-sensitive commands; job
  budgets and kill-on-quit; every prompt says what the agent is sharing (§5.6).
  Stage 1 is detach + registry + completion wake, stage 2 the `all` waits, stage 3
  `exclusive` and the budgets. This is the milestone for the machine, the way
  M2.6 is the milestone for the branch.
- **M3 — External agents (attach).** A UNIX socket plus `mush read/edit/focus`
  CLI, so an agent you run yourself can drive mush. Newline-delimited JSON;
  requests carry an `id`; `edit` carries a base revision and returns `conflict`
  rather than guessing.
- **M4 — FS watching + merge.** Watch the workspace and three-way merge external
  changes into dirty buffers (`base`/`ours`/`theirs`), with conflict markers and
  `.mush/backups/` before anything destructive. This closes the M2 gap.
- **M5 — Spawn mode.** `mush` launches a configured agent in a pty pane with
  `MUSH_SOCKET`/`MUSH_ROOT` injected, so "works with any agent" covers binaries
  that know nothing about mush.
- **M6 — Polish.** Search, syntax highlighting (incremental, dirty-lines only),
  undo/redo, word motions, config file, optional MCP bridge as a separate binary.

Each milestone ends with a demoable, tested artifact. No milestone depends on a
later one.

---

## 10. Testing

- **Unit tests.** Buffer semantics (split/join, multi-byte columns, exact
  round-trip), path jail and escaping, capped reads, atomic writes, session and
  user-config round-trips, `.mush` self-ignore, history trimming and its
  termination guard, compaction, tool-execution semantics (list filtering,
  unique-match edits), tool-pair repair, argument validation, shell-command
  timeout, cancellation, output cap and runaway-writer limit, URL/status-line
  parsing, the model-list timeout, cancelling a chat request mid-wait and the
  request deadline (plus the slow-but-alive body the slices must not mistake for
  one), config precedence, schema/prompt invariants,
  word wrapping, column slicing, the actor mailbox (parked nudges, Stop vs
  Shutdown, completion delivery), and the `/new`, Ctrl-C, stale-event, and
  steering-echo state transitions.
- **End-to-end (pty).** `scripts/smoke.py` drives the real binary over a
  pseudo-terminal with the pty as its controlling terminal (so window size and
  SIGWINCH behave as they do in a terminal). Scenarios: agent (needs a model),
  editor (needs a model), resize (needs nothing), cancel (needs nothing — a
  socket that accepts the chat request and never answers must be abandoned by a
  single Ctrl-C, which is only observable from outside the process).
- **Deterministic orchestration.** `cargo test -- --ignored` starts
  `scripts/mock_llm.py` and runs a root → child → grandchild chain, an isolated
  child whose run must commit its worktree (the test then merges it, removes the
  worktree, and deletes the branch — the documented commands, executed), and a
  context overflow that must compact. No network, but it needs `python3` and
  `git`.
- **Live.** Two `#[ignore]`d tests talk to the configured endpoint (one of them
  proves the TLS path), so the default suite stays green offline.
- **The checks.** `cargo fmt --all --check`, `cargo clippy --all-targets --
  -D warnings`, the unit tests, and the pty resize and cancel scenarios are the
  whole gate; they run anywhere rust and python3 do, so any CI can call them.
- **Screen review.** `scripts/screen.py` drives the real binary over a pty and
  prints the painted screen as text at 200×50 down to 30×8, which is how the ten
  defects of §4.5 were found and how the next layer gets reviewed. Pass `--ask`
  with a reachable endpoint to see the agent's own screens (thinking, cancel,
  done); without it the editor's screens need no model at all.
- **Not yet.** Property tests for merge/undo (they arrive with M4), and a fuzz
  target for the path jail.

Run it:

```sh
cargo test                 # offline, fast
cargo test -- --ignored    # scripted mock model + a live-endpoint check
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke --resize
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke --cancel
```

---

## 11. Open questions

1. `[OPEN]` Editor depth: add undo/redo before or after search? Undo is the more
   painful omission for real use.
2. `[OPEN]` Highlighting: none, a tiny regex highlighter, or tree-sitter? Leaning
   "incremental regex at M6", because tree-sitter multiplies the dependency
   budget for the least certain payoff.
3. `[OPEN]` Config file format: `mush.toml` in `.mush/` vs environment only.
4. `[OPEN]` Should `run_command` be denied by default and enabled per session?
5. `[OPEN]` Do we ship the MCP bridge ourselves, or leave it to the community?
6. `[OPEN]` Where do the M2.7 size tiers cut? 40×10 is the floor; is the compact
   tier 80×20, or narrower?
7. `[OPEN]` Notices: keep them inline in the transcript with a neutral colour, or
   give them a one-line log of their own?
8. `[OPEN]` Does the human need to *type into* a subagent's pane (today that path
   is a nudge), or is watching enough once §2 gap 3 is closed?
9. `[OPEN]` Job output: keep the head (what the model reads first) or the tail
   (what a crash left behind) — or both, head for the tool result and tail for
   `command_status`?
10. `[OPEN]` Where do jobs become visible? A badge on the agent row, a footer under
    the tree, or their own pane. The human should not have to ask a model what is
    running on their machine.
11. `[OPEN]` How many jobs may live at once: a per-agent cap, a machine-wide one,
    or both — and does a heavy build count against `MAX_AGENTS` too?

---

## 12. Decision log

- **Filesystem + shell is the universal agent interface.** A socket is an
  upgrade, never a requirement.
- **Agents edit live buffers.** File tools round-trip through the UI thread; this
  replaces locks, merges, and conflict handling in the common case.
- **One owner of state.** `Msg` → `update` → `draw`. No shared mutable editor.
- **No async runtime.** Threads and channels; `run_command` off the UI thread.
- **No OT/CRDT.** Plain files, atomic writes, exact-match edits.
- **The prompt is data, not logic.** It lives in one small function beside the
  tool schemas, so the contract can be read in one screen.
- **Seven direct crates, listed in §7.** Anything else must earn its place.
- **`.mush/` ignores itself.** Zero setup, zero footprint in the host repo.
- **An isolated agent's work is committed when its run ends.** A branch that stays
  at its base commit makes every merge command a lie, however good the diff looks.
- **Every agent gets the workspace rules.** A subagent prompt without them invited
  edits without reading, and "same tools as always" was false for a leaf.
- **The schema reserve is measured, not guessed.** A prompt test fails if the tool
  schemas outgrow `Config::SCHEMA_TOKENS`.
- **Transcripts are repaired, not trusted.** A transcript adopted from the UI is
  normalized (results beside their calls, every call answered) before it goes on
  the wire.
- **The screen degrades, it does not clip.** Ranked fields and a footer mean a
  narrow pane loses detail, not the fact that matters.
- **Commands outlive the turn, not the human.** Long work detaches instead of
  dying at a timeout; a job is an actor, so its result arrives as a message and
  nobody has to poll — or guess how long a build takes.
- **The machine has one lock.** Timing- and port-sensitive commands take it
  explicitly, and agents are told who holds it rather than being left to
  interleave and then trust the numbers.