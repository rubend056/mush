# mush — design doc (v0.1)

> A small, fast, agent-agnostic terminal editor. Open a folder, talk to an
> agent, and watch it edit the files you have open — without either of you
> clobbering the other.

Status: **MVP implemented and working end to end** (M0–M2 of §9). This document
describes what is actually built, then what comes next. Decisions are marked
`[DECIDED]` or `[OPEN]`.

---

## 0. TL;DR

- `mush` is a modal TUI editor in Rust. One binary. **No async runtime.**
- Workspace-first: `mush [DIR]`, or just `mush` in the folder you are in.
- An **agent is built in**: it talks to any OpenAI-compatible endpoint
  (default `http://rubendpc:8078`) and edits the workspace through five tools.
- Agents *drive* the editor: file tools execute on the UI thread, so an agent
  edits the **live buffer**, not a stale copy on disk. The human sees edits land.
- The agent's **system prompt is ~7 lines** and the tool set is five functions.
  Simple prompt is a consequence of a small, honest interface.
- Everything mush writes lives in `<DIR>/.mush/`, which **git-ignores itself**.
- Architecture is a single-owner **event loop**: `Msg` in, `App::update`, `ui::draw`.
- KISS is enforced by a dependency budget of **five direct crates**.

---

## 1. What mush is / is not

### Is

- A **text editor first**: open, navigate, edit, save. Keys stay out of the way.
- A **live collaboration surface** between one human and the built-in agent.
- **Endpoint-neutral**: anything speaking the OpenAI chat-completions API with
  function calling works (llama.cpp, Ollama, vLLM, LM Studio, hosted APIs).
- **Small on purpose.** Roughly 2,700 lines including tests, across two crates.

### Is not

- A full IDE. No debugger, no terminal multiplexer, no project wizard.
- A CRDT / collaborative-OT server. One human, the filesystem is truth.
- Provider-specific, plugin-based, or extensible via a scripting language.
- An agent framework. It ships one small agent loop, not an orchestration layer.

### Why files + a shell is still the interface

The agent's tools are `list_files`, `read_file`, `write_file`, `edit_file`, and
`run_command`. That is the entire surface. Any other agent — a shell script, a
different harness — can collaborate through the same two things: the workspace
files and the shell. A richer attach protocol is planned (§9) but is not
required for mush to be useful today.

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
- **Bounded loops.** At most 24 model turns per request and a hard timeout on
  every shell command; a runaway agent stops.

### Known gap (M2 continuation)

If `run_command` rewrites a file the human has **unsaved** changes in, those
changes are preserved (the buffer is left alone) but the file on disk has moved
on. A filesystem watcher plus a diff3 merge is the planned fix (§9).

---

## 3. The agent contract

### System prompt

`mush-core/src/prompt.rs` generates the entire prompt. It is one paragraph:

```text
You are mush, a coding agent working in the workspace at <ROOT>.

Use the tools to inspect and change files. Rules:
- Read a file before you edit it.
- Prefer edit_file for small, surgical changes; use write_file only for new files or full rewrites.
- Do the work instead of describing it. Keep replies short.
- Never touch paths outside the workspace.
- When the task is done, stop calling tools and reply with a one-sentence summary.
```

### Tools

| Tool | Arguments | Notes |
|---|---|---|
| `list_files` | `path?` | recursive, skips `.git`, `.mush`, `target`, `node_modules`, … |
| `read_file` | `path` | live buffer if open; capped at 16 KB per result |
| `write_file` | `path`, `content` | atomic; creates parent directories |
| `edit_file` | `path`, `old_string`, `new_string` | exact and unique match required |
| `run_command` | `command` | `sh -c` in the root; 120 s timeout; output capped |

Tool calls execute as a normal OpenAI function-calling loop: the assistant
message, then one `role: "tool"` message per call, then the next request. If a
model answers without calling a tool, it is done.

### History budget

Small local models have small contexts (the default endpoint reports 8 K). Before
each request the agent trims the oldest turns until the conversation fits, always
cutting at a **user** message boundary so assistant/tool pairs stay valid.

---

## 4. The editor

`[DECIDED]` Normal + Insert modal editing. No Vim operators, no selections yet;
the smallest thing that is genuinely usable.

| Context | Keys |
|---|---|
| anywhere | `Tab`/`Shift-Tab` cycle panes · `Ctrl-Q` quit · `Ctrl-S` save · `Ctrl-R` reload · `Ctrl-N` new chat · `Ctrl-C` cancel agent · `Ctrl-L` refresh files |
| files | `j`/`k`, arrows, `g`/`G`, `Enter`/`l` to open |
| editor (normal) | `i` `a` `I` `A` `o` `O` insert · `hjkl`/arrows · `0` `$` `g` `G` · `Ctrl-D`/`Ctrl-U` · `x` delete |
| editor (insert) | typing, `Enter`, `Backspace`, `Delete`, arrows, `Esc` to normal |
| chat | typing, `Enter` send, `Backspace`, `↑`/`↓`/`PgUp`/`PgDn` scroll, `Esc` clear · `/new` `/help` `/quit` |

A `Buffer` is `Vec<String>` lines plus a `trailing_newline` flag, so files
round-trip byte-for-byte. Cursor columns are counted in **characters**, so
multi-byte text edits correctly.

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
worktree directly on disk. Human-in-the-loop is the merge story: mush prints
git commands, it never auto-merges.

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
  **only when something changed**.
- `App::update` is the single entry point; `ui::draw` only paints.
- The agent thread is one blocking `while let Ok(cmd) = rx.recv()` loop.

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
    mush-core/   # pure domain: config, messages, prompt, session, workspace. No UI.
      config.rs      endpoint/model/api-key resolution
      message.rs     OpenAI-compatible message + request/response types
      prompt.rs      the system prompt and the five tool schemas
      session.rs     `.mush/` creation and conversation persistence
      workspace.rs   path jail, listings, capped reads, atomic writes
    mush/        # the binary: TUI + agent
      main.rs        CLI, model discovery, terminal guard/panic hook, event loop
      app.rs         state, update, key handling, tool execution, buffers
      agent.rs       model loop, tool dispatch, shell execution
      http.rs        ~150-line blocking HTTP/1.1 client
      ui.rs          layout, panes, transcript rendering, word wrap
  docs/mush.md
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
`connect_timeout`, after which the editor opens and reports no model.

Rules: no full-buffer scan per frame, no redraw without a state change, no
allocation in the input path beyond the edit itself. Release profile uses
`lto = "thin"`, `codegen-units = 1`, `strip = true`.

---

## 9. Roadmap

**Done**

- **M0 — Core.** workspace path jail, atomic writes, session persistence, message
  types, prompt/tool schemas.
- **M1 — Editor.** open/edit/save, modal keys, panes, transcript, wrapping.
- **M2 — Agent.** OpenAI function-calling loop, live-buffer tool execution,
  streaming-free status spinner, cancellation, history trimming, `Resync`.

**Next**

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

- **Unit tests (20).** Buffer semantics (split/join, multi-byte columns, exact
  round-trip), path jail and escaping, capped reads, atomic write, session
  round-trip, `.mush` self-ignore, history trimming, output draining, URL and
  status-line parsing, word wrapping, column slicing.
- **End-to-end (pty).** `scripts/smoke.py` drives the real binary over a
  pseudo-terminal: it sends keystrokes, waits for a local model to call tools,
  and asserts on the resulting files and on `.mush/session.json`.
- **Ignored by default.** `cargo test -- --ignored` runs one live test against
  the configured endpoint, so the suite stays green offline.
- **Not yet.** Property tests for merge/undo (they arrive with M4), and a fuzz
  target for the path jail.

Run it:

```sh
cargo test                 # offline, fast
cargo test -- --ignored    # hits the configured model endpoint
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke
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
- **Six dependencies.** Anything else must earn its place in this table.
- **`.mush/` ignores itself.** Zero setup, zero footprint in the host repo.