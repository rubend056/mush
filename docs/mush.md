# mush — design doc (v0.2)

> A small, fast terminal surface for coding agents. Open a folder, give the
> root agent a task, and watch the tree of agents work — with the repository's
> branch, dirty count, and line delta always in view.

Status: **implemented and working end to end** (M0–M3 of §9). This document
describes what is actually built, then what comes next. Decisions are marked
`[DECIDED]` or `[OPEN]`.

This revision folds in two audits — the agent contract (§3, §5.5) and the
screen, photographed at six terminal sizes (§4.5) — and the v0.2 decision to
drop the built-in editor: mush manages agents and shows git state; the agents
edit the files.

---

## 0. TL;DR

- `mush` is a TUI in Rust. One binary. **No async runtime.**
- Workspace-first: `mush [DIR]`, or just `mush` in the folder you are in.
- An **agent is built in**: it talks to any OpenAI-compatible endpoint
  (default `http://rubendpc:8078`) and works the workspace through ten tools:
  `read_file`, `write_file`, `list_files` and `search` touch files, `edit_file`
  replaces exact text, `run_command` is the shell, `spawn_agent` delegates, and
  `status`, `control` and `wait` manage what it started — a long command becomes
  a **job** the agent can check on later, and `read_file` is the one *tool* road
  an image can travel by (the human has two of their own: a pasted path and
  `Ctrl-V`, §3).
- mush holds **no file state**: agents read and write files directly, and the
  UI shows their tree, their transcripts, and the git facts.
- The agent's **system prompt is three short blocks** — the rules, the
  delegation policy, and what the machine is like — and the root's tool set is
  ten functions. A leaf keeps nine of them: `spawn_agent` is omitted, which is
  what bounds the tree. Small prompt, small interface, one consequence of the
  other.
- Everything mush writes lives in `<DIR>/.mush/`, which **git-ignores itself**.
- Architecture is a single-owner **event loop**: `Msg` in, `App::update`, `ui::draw`.
- KISS is enforced by the dependency budget of §7.

---

## 1. What mush is / is not

### Is

- A **control surface for the built-in agent**: ask, watch, steer, cancel.
- A **glance at the repository**: branch, dirty count, and per-branch line
  delta, updated while agents work.
- **Endpoint-neutral**: anything speaking the OpenAI chat-completions API with
  function calling works (llama.cpp, Ollama, vLLM, LM Studio, hosted APIs).
- **Small on purpose.** Two crates; `scripts/census.py` prints the split
  (production, tests, comments) and the total, so the size is checked rather
  than remembered.

### Is not

- A text editor. The agents own file editing; mush never opens a file, so
  there is no second copy of anything to reconcile.
- A full IDE. No debugger, no terminal multiplexer, no project wizard.
- A CRDT / collaborative-OT server. One human, the filesystem is truth.
- Provider-specific, plugin-based, or extensible via a scripting language.
- An agent framework. It ships one small agent loop, not an orchestration layer.

### Why there are file tools beside a shell

`run_command` stays the road for what a shell is for — git, tests, builds — and
the agent's own tools cover the files: `read_file` (a bounded window that says
what it left), `write_file`, `list_files`, `search`, and `edit_file`, whose
exact-and-unique replacement is a safety property `sed -i` does not have. An
earlier revision cut this down to `edit_file` alone, on the grounds that a shell
lists, reads and writes better than a bespoke tool could (`rg`, `sed -n '1,200p'
file`, `ls -la`, `mkdir -p dir && cat > file <<'EOF'`). Two facts broke that
premise (H31):

- **A lock refuses reads too.** A sibling's `exclusive` command refuses *every*
  `run_command` (§5.6), which is right for a build and wrong for a read: an
  agent that launched a long benchmark and then wanted to look at a file had no
  way to, and the wait that frees the machine is minutes long. The file tools
  take no lock and work beside one.
- **A shell cannot carry an image.** A screenshot, a chart or a rendered diagram
  on disk is bytes no shell command hands back to a model that can see; an image
  arrives in a tool result as an image, which only a typed tool can build. So
  `read_file` is the tool that carries one (§3).

The rest is unchanged: `edit_file` keeps its exactness, and any other
agent — a shell script, a different harness — can still collaborate through the
same two things: the workspace files and the shell. The same two things are also
how *you* drive mush: the attach protocol of §9 (M3) lets a script you run
yourself read a transcript and hand a message to an agent over a UNIX socket,
without needing to know anything about mush's internals.

---

## 2. The UI owns no file state

An editor-shaped design has a hard problem: the agent reads and writes files on
disk while the human has the same file open in memory. Whoever saves last wins.

mush avoids it by not being an editor. Agents do their own file I/O on their own
threads (the file tools, `run_command`), and the UI holds only what the
human needs to steer them: the agent tree, the focused transcript, the message
box, and the git snapshot. There is no live buffer, so there is no stale copy,
no lock, and no save race — a consequence of a smaller product.

What the UI does own is one channel. Every input — a keystroke, an agent
event — becomes a `Msg`, and one thread applies it to `App`. Agents never paint
and never share state with the painter.

### Safety rules that stay

- **Atomic saves.** Every file write — `write_file`, `edit_file` — is
  temp-file + `rename`; readers never see a half-written file, and a crash cannot
  corrupt the original.
- **Workspace confinement is a convention, not a fence.** Every file tool
  resolves its `path` against the root and rejects an escape (`..`, absolute
  paths; `write_file` refuses the root itself), but `run_command` is a real shell
  and nothing confines it. So the rules name the
  workspace, tell the agent that paths are workspace-relative and that commands
  run with their cwd at its root, and say never to touch paths outside it — and
  the prompt says so in one place (`RULES`). There is no enforced *path jail*:
  the agent is trusted to stay, not stopped from leaving.
- **Edits are exact.** `edit_file` refuses if an `old_string` is missing or
  appears more than once, so an edit can never hit the wrong occurrence.
- **Every big-text result is capped, edits are not.** One cap bounds them all —
  a command's output, a file read, a listing, a search (`CMD_CAP = 16 000`
  bytes, scaled down by `Config::cmd_cap()` to a quarter of the history budget,
  floored at 512) — and a job's report is the same kind of window, a **tail**
  (§5.6). A capped result says so and says the way past it: a result whose head
  is kept ends with
  `[mush: output truncated at {cap} bytes — rerun it narrower (rg, head, a smaller path) to see the rest]`,
  and a result whose *end* matters keeps its tail, preceded by
  `[mush: output truncated at {cap} bytes (the end is shown) — rerun it narrower to see the rest]`.
  A read has its own window and says so too (`[mush: lines 1–200 of 900 — read on
  with offset=201]`, or `— end of file`), which is a smaller question than the
  cap and gets its own sentence. Two files are read differently from the rest: a
  `read_file` of a file past 32 MB is refused (a window cannot get past it — the
  file is opened whole first), and a `search` past 2 MB *in one file* skips it
  and counts it, so a "no match" that skipped a file says how many and names
  `run_command` (`sed -n`, `rg`) as the road. An image has a cap of its own and
  no window: past 2 MB it is refused with a downscale as the road, and an image
  the run's model is not documented to see is refused *before* it is sent, so a
  request that cannot be read never costs a turn. Edit operations always work on
  the complete file.
- **Bounded loops.** A run ends when the model stops calling tools; a *loop* —
  the same tool batch five rounds over with nothing changed in between — ends it
  early, and a 200-turn runaway guard withdraws the tools and asks for a
  summary. A shell command runs in its own process group with a 120 s timeout
  and a hard 8 MB output limit, and delegation is bounded in depth and fan-out
  (§5.5). A runaway agent stops.

---

## 3. The agent contract

### System prompt

`mush-core/src/prompt.rs` generates the entire prompt, and is the only place it
exists: this section describes its shape rather than quoting it, because a quote
here is a second copy, and a second copy goes stale on its own. One line names
the workspace, then the rules, the delegation policy, and what the machine is
like (§5.6) — `RULES`, `DELEGATION` and `MACHINE`. The blocks are shared with the
subagent prompt, so a rule has one home rather than two copies that drift, and
the prompt is kept small on purpose: every word of it is paid for on every
request of every turn.

The DELEGATION block is only in a prompt whose tools include delegation: a leaf
at `MAX_DEPTH` has no `spawn_agent`, so it is not told how to use it. A
subagent's prompt is otherwise the same shape — who it is (depth), the workspace
it works in (the shared one, or its own worktree with a `base`) — and the brief
travels as the first user message, not in the system prompt, mirroring the
root's system+user shape. The UI shows that same brief as the child's first
message, so the human's picture of a child starts where the child's does.

### Tools

Ten tools, in schema order — five for the workspace's files, the shell, the
delegation tool, and three that manage what an agent started (§1). Each schema
says *the call* and nothing else: its arguments, their defaults, what comes
back. How to work is the prompt's, so a description that repeats a rule is a
second copy of it.

| Tool | Arguments | Notes |
|---|---|---|
| `edit_file` | `path`, `edits` | exact-and-unique replacement, one shape: `edits` is always a list (a lone edit is a list of one), `replace_all` opts into an ambiguous match, and the batch lands all-or-nothing in one call |
| `read_file` | `path`, `offset?`, `limit?` | a file as a window of lines, with no line numbers (a numbered line is a string that cannot match `edit_file`'s `old_string`) and one trailing sentence saying what the window left; a png, jpeg, gif or webp — sniffed from the file's own bytes, never its name — comes back as the image itself, if the model is documented to see; works beside a held lock |
| `write_file` | `path`, `content` | create or replace a whole file, parent directories included; the answer is one line naming what it replaced; the workspace root itself is refused |
| `list_files` | `path?` | the files under a path, sorted, one per line; build and VCS directories are skipped; capped at 400 names with the way past it |
| `search` | `pattern`, `path?`, `ignore_case?` | a literal string (no regex — a regex engine is a dependency, and `rg` is the shell's), one `path:line: text` per match; binary and huge files skipped |
| `run_command` | `command`, `detach?`, `exclusive?` | a shell in the workspace root, own process group; 120 s timeout, output capped to fit the window, cancellable; `detach` starts a job at once, `exclusive` takes the machine lock (§5.6) |
| `spawn_agent` | `brief`, `title`, `base?` | a new agent with its own transcript; `title` names its row, and `base` forks a worktree on `mush/<id>` for it (§5.5) |
| `status` | — | your children and your jobs in one listing: each child's state and branch, each job's state, age and command; `✉` marks a result you have not read; a listing, not a delivery — `wait` hands results over |
| `control` | `id`, `action`, `text?` | stop or message one, naming it as `status` prints it (`2` for a child, `c2` for a job); a job can only be stopped |
| `wait` | `on?` | blocks until every child and every job you own has finished, then one digest; returns at once when there is nothing to wait for; a subagent also waits out another agent's machine lock, gives up after 10 minutes, and a message to it ends the wait early; `on` narrows it to one thing, named as `status` prints it — the rest keeps running, but any result you have not read ends the wait too, and the machine lock is not waited for |

The `spawn_agent` row is omitted from a leaf agent's schema (`MAX_DEPTH`), which
is what bounds the tree, so a root has ten tools and a leaf nine; the `status`,
`control` and `wait` rows are not omitted, because they manage the *jobs* a leaf
may run in the background while it edits (§5.6). `mush_core::tools::TOOL_NAMES`
is the single list of names, and a test asserts the schemas match it.

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

### Images

`read_file` is the one *tool* road an image travels by, and it carries it
**inside the tool result**: a message with text and one `image_url` content part
per image, each a `data:` URL of the bytes (`Message::content_parts`). Nothing
else in the request path knows images exist — `ChatRequest` takes `&[Message]`,
and a message with no images serializes byte for byte as it always did — because
a second message type for the vision case is a second thing to keep in step
with the first.

The **human has two roads of their own**, and both end in the same message:

- **A bracketed paste that is nothing but an image's path** attaches the image
  to the next message instead of inserting the path as text. That is what
  drag-and-drop and a file manager's "copy file" put in a paste, and a paste
  whose *only* word is the path is the human saying "this picture".
  `Workspace::pasted_image` reads the shapes a terminal produces — a bare
  workspace-relative name, an absolute path (inside the root or outside it; a
  human may name what the model's tools may not, because they already have the
  file), a quoted name, a `file://` URL with its `%20`s, and the `\ ` a terminal
  uses to escape a dragged file's own space — and `Ok(None)` for anything else:
  prose, a paragraph, a directory, a file that is not a picture. It is `Err`
  only when the paste names an image that cannot ride, and then the path is
  inserted as text anyway — a paste is never swallowed, and the words are the
  road to a downscale.
- **`Ctrl-V` in the chat pane** attaches the image on the system clipboard: a
  screenshot with no file behind it yet. `clipboard.rs` reads it through the
  programs a human would use (`wl-paste`, `xclip`, `pngpaste`), on a thread of
  its own with a deadline, and `Workspace::save_pasted_image` writes the bytes
  to `.mush/paste/pasted-<unix millis>.<png|jpg|gif|webp>` so the model can read
  the picture again after a trim or a restart. `.mush/` git-ignores itself, so a
  pasted screenshot cannot dirty the tree.

Both roads end in `Message::user_with_images`: the human's own message, with
the images riding in it exactly as they ride in a tool result. Two facts can
refuse an attachment before it is sent — no model at all, and a model the
provider table does not document as accepting image parts (`Ctrl-P` is the road
named) — because an endpoint that may reject image parts must not cost a turn
to discover it. The second is asked twice, at the box that takes the attachment
and again at the wire that sends it: `Ctrl-P` can change the model in between,
and what must never reach a model that cannot see is the *message*. These are
the same facts that decide whether a `read_file` image travels; the human's gate
is the one that keeps the path as text instead of dropping the gesture, and the
clipboard's is the one that has no path to keep.

A third fact is said but does not refuse: an image bigger than the whole
`Config::history_budget()` attaches, with the line that says `trim_history`
sheds image payloads *before* it drops a turn, so those bytes would never reach
the model. The human decides what to send; what the model will actually see is
not a thing to leave unsaid.

Three facts decide whether an image travels:

- **The format is sniffed, not named.** Png, jpeg, gif and webp are recognised
  by their own first bytes; an extension is a claim by whoever wrote the file,
  and a `data:` URL's mime is read by an endpoint that never sees a name.
- **The model must be documented to see it.** Vision is a per-model fact in the
  provider table (`ModelSpec::vision`; `deepseek-flash` is the one row that
  states it), and everything else — including every model mush has never heard
  of — is off. Being wrong in that direction costs an image the model could have
  read; being wrong the other way costs the whole turn, because an endpoint that
  never documented image parts may reject the request.
- **An image is capped at 2 MB and cannot be windowed.** `offset`/`limit` are
  lines and an image has none, so the refusal names the one road that makes a
  big picture readable: downscale it with `run_command` and read that.

An image's bytes leave the transcript the same way they would leave the context:
`Message::drop_images` replaces them, in place, with one line naming the path and
format (`[image: shots/a.png (png) — bytes dropped to save room; read the file
again if you need them]`). Trimming calls it **before** it drops any turn — an
image is what an over-budget transcript is usually made of and the cheapest thing
to lose, since the placeholder still says where it is — and the session writer
calls it before it serializes, so a multi-megabyte screenshot never lands in
`.mush/session.json`. The drop is idempotent, which is what keeps a session
saved, loaded and saved again from stacking placeholder on placeholder.

### History budget

Small local models have small contexts (the default endpoint reports 8 K). Every
request reserves room for the tool schemas, the reply, and a margin —
`Config::SCHEMA_TOKENS` (a prompt test keeps the schemas inside it),
`REPLY_SHARE_WORDS` of the window (floored at 1 024 and capped at 120 000), and
5 000 tokens for whatever a turn's tool result adds before the next request. The
three numbers live in `crates/mush-core/src/config.rs`, and
`mush --print-config` prints what they resolve to for the window in front of
you. Before each request the agent trims the oldest turns until the conversation
fits, always cutting at a **user** message boundary so assistant/tool pairs stay
valid.

Trimming drops information, so it is the fallback, not the first move: once the
transcript passes nine tenths of the budget the agent asks the model to
summarize everything important and continues from `system + summary`. That is
what lets a long task survive a small context window.

`/compact` is the same fold, asked for by hand instead of triggered by the
window — one routine, so the two cannot disagree about the summary message or
about what a fold costs. It goes to the focused agent's mailbox. An idle agent
folds at once and nothing else happens: no run is started, because there is
nothing to answer — the summary *is* the result, and the `Compact` event the UI
already mirrors keeps the pane, the session file and the meter in step. Mid-run
the request parks like a nudge and is honoured at the next message boundary,
never between an assistant's tool calls and their results.

A transcript that is already `system + one message` is refused, with "nothing to
compact" on the transcript and nothing on the wire: folding it would cost a
request and can only re-summarize the summary. The refusal is said out loud
because a human typed a command — silence there is indistinguishable from a
fold that quietly failed. Anything longer is folded exactly as asked.

The fold is one ordinary request: the conversation's own — same system prompt,
same tools, same `tool_choice`, same thinking knobs — with
`COMPACT_INSTRUCTION` appended as one more **user message**. Nothing about it is
special-cased, and that is the point: the tool schemas are the head of the
rendered prompt, and an endpoint caches *prefixes*, so a summarize call that
dropped them (or switched `tool_choice` from `auto` to `none`) would share no
prefix with the run it belongs to and re-prefill the entire history — at exactly
the moment that history is at its largest, which is the cost the fold exists to
avoid. What keeps the model from calling a tool is the instruction, persisted in
the message where the model can act on it: *reply with the summary, as plain
text, and end your turn: call no tool*. The one thing that is not the run's is
the summary's own reply cap (`COMPACT_REPLY_TOKENS`), and that is safe — sampling
and length parameters are not prompt text, so they cost no cache miss. A model
that answers with a tool call instead of a summary is not a fold: mush says it
could not compact and leaves the transcript alone.

---

## 4. The message box

`[DECIDED]` No editor. The one text field is the message box, and it behaves
like every other terminal input: a cursor, arrows, `Home`/`End`,
`Backspace`/`Delete`. Edits land on **grapheme cluster** boundaries
(`unicode-segmentation`), so a combining mark or a ZWJ emoji is one keystroke,
and the view is measured in display columns (`unicode-width`, through
`unicode-truncate`), so a CJK glyph takes two. Past the pane width the box
scrolls horizontally instead of clipping its tail: `…` marks whichever edge is
elided, and the cursor is always on screen.

| Context | Keys |
|---|---|
| anywhere | `Tab`/`Shift-Tab` cycle panes · `Ctrl-Q` quit (a second press confirms while work is running) · `Ctrl-N` new chat (stops every agent and restarts the root) · `Ctrl-C` stops the focused agent (reaches a model that is still thinking) · `Ctrl-X` stops every running agent · `Ctrl-P` model picker · `Ctrl-T` show or hide the model's reasoning |
| picker | `j`/`k`, arrows, `g`/`G`, `Home`/`End`, `PgUp`/`PgDn` move the list, `Enter` take the row, `Esc` close |
| agents | `j`/`k`, arrows, `g`/`G`, `Home`/`End` move the rows, `PgUp`/`PgDn` page them, `←` the row's parent, `→` its first child, `Enter` show its transcript, `c` cancel that agent, `Esc` back to the root |
| chat | typing, `Enter` send, `Shift`/`Alt-Enter` a new line, `Ctrl-V` attach the image on the clipboard, `←`/`→`/`Home`/`End` the box cursor, `Backspace`/`Delete` (Backspace on an empty box pops the newest attachment), `↑`/`↓`/`PgUp`/`PgDn` scroll, `Esc` clear the box and its attachments · a `/`-line is a command: `/provider` `/model` `/url` `/key` `/models` `/compact` `/notes` `/help` `/quit` |

A paste that is nothing but an image's path attaches the image; anything else
is text and lands in the box as it always did (§3). The attachments are painted
as dim `▣ path (format · size)` rows above the text — one per image, at most
three, the third counting the rest when there are more, with the title saying
how many — and they travel with the send: `Enter` on an empty box with an image
attached is still a send, because the picture *is* the message. A send that
does not land puts the words and the images back in the box, and `Esc` clears
both.

`Enter` in the agents pane moves the *view*, not the keyboard: the row's
transcript replaces the chat pane while the keys stay in the tree, and `Tab` is
what puts them in the box, where typing reaches the agent on screen. A page is
always ten rows, in every pane and every list.

The transcript is not only the human's words, and it says so. `you › ` marks a
line the human typed — and only a line the human typed: a subagent's brief opens
its pane as `brief › `, a parent's `control` message arrives as `parent › `,
and a folded result (`#1 done: …`) is mush's own report, marked `· ` like the
other lines mush writes. And the text itself is untrusted: a model reply, a tool
result and a tool call's arguments are defanged before they are painted, so an
`ESC ]0; …` in them cannot rename the terminal window and a `CSI 2J` cannot
repaint the frame they are drawn on.

A thinking endpoint's own reasoning is painted above the turn it decided, dim
and marked `⋯ `, one block per assistant turn. It is the endpoint's
`reasoning_content` for that turn and nothing else: it is already stored with
the turn in `.mush/session.json` and replayed with it on the next request — a
thinking endpoint refuses a replayed turn without it — so the block adds a
*view* of what the request already carries. `Ctrl-T` shows or hides it in every
pane; the toggle writes nothing, sends nothing and is not stored, and a new chat
keeps whatever the human chose. A reasoning that trims to nothing paints no row
at all, so a reply that did no thinking costs no line.

---

## 4.5 The screen: reachable is not glanceable

`[DECIDED]` Beyond editing, the UI has one job: answer three questions without
typing anything, at whatever size the terminal is.

| Question | What the frame derives it from |
|---|---|
| What is each agent doing? | its `Phase` and the instant it began — the glyph and the activity text are the same fact twice read |
| Where is its work, and on what branch? | `AgentNode::branch`, kept ahead of the activity and the title when the columns run out |
| How much has changed? | one cached `git diff --shortstat` per branch, measured against the parent's branch |

An audit of real screens (200×50, 120×32, 80×24, 60×17, 40×10, 30×8) found ten
defects. They share one shape: the data exists, the pixels do not.

1. **A dead instruction — fixed in this revision.** The old empty editor said
   "Tab to files"; there is no files pane. Copy that cannot come true is worse
   than none. (The editor itself is gone as of v0.2.)
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

### The plan (M2.7) — all four rules landed

Four rules, no new panes, no new dependencies, and `ui.rs` stays dumb — all values
are computed in `App`.

- **R0 — Derive, don't store.** `[DONE]` The three defects above were one bug: the
  screen kept *conclusions* (a status string, a `running` flag) instead of *facts*.
  An agent now has a `Phase` (`Idle · Thinking · Activity(label) ·
  Compacting(kind) · Cancelling · Stopped · CutOff · Done · Failed`) and the
  instant it began; the row glyph, the activity text, and the bar are derived from
  those every frame. `App::status` is a typed line with a
  lifetime: `Info` fades after five seconds, `Error` stays, and work in progress is
  never stored at all — which is what makes `✓` on an idle agent and a lingering
  `thinking…` impossible rather than merely fixed.
- **R1 — Ranked fields, then a footer.** `[DONE]` A row is spent in this order:
  state (`glyph · id`), then `branch +add −del`, then the activity with its age,
  then the title — facts that exist nowhere else survive longest, and the title
  yields first because the footer and the transcript carry the brief in full. The
  title is *derived* from the brief (`deep.txt`, `lexer`), so two children never
  read the same. The selected row's full facts get a footer under the list, up to
  three lines when the pane is tall and one when it is compact, isolated agents
  included (`.mush/wt/2 · git diff HEAD...mush/2` — git's own spellings, no mush
  wrapper to learn). `fit_row` is a pure function with tests, and the pane's
  title carries `agents · 2 working · 1 waiting · Σ +324 −40` — each count named,
  each agent in exactly one.
- **R2 — One bar, two lines.** `[DONE]` Line one is one word chosen by rank: an
  alert (a failure, the quit prompt) beats the tree's own line (a napping root
  that will resume by itself), which beats a said line (a stop, a job's report, a
  command's answer), which beats the idle hint. The ranking is the whole point:
  the bar is forbidden from showing what a row or the transcript already carries.
  Line two (on terminals at least **24** rows tall)
  is the stable facts, elided from the right: `⌂ path │ branch ±dirty +add −del │
  model @ endpoint · ctx 12k/~500k`, with the read's own age appended once it is
  past ten seconds, so a cached fact cannot read as a live one. The meter's `~`
  says the window was assumed rather than stated, and it says `full` at the
  window and `over` past it. The window's size is never a mystery again, and the
  repository survives the narrowest of them.
- **R3 — Size tiers with a floor.** `[DONE]` `w<40 || h<10` → a single notice,
  centred on both axes, that falls back to a shorter spelling and always names
  40×10; below it every key is refused except `Ctrl-Q`, because a screen that is
  only a notice cannot show the effect of `Ctrl-N`. `w<80 || h<20` → compact: the
  agent strip on top, chat below; `h≥24` → the two-line bar; the transcript is
  capped at 110 columns however wide the terminal is.
- **R4 — Truthful glyphs.** `[DONE]` `·` idle/never ran, `◐` running, `✓`
  finished, `✗` failed, `⚠` a run that was cut off (the process went away with it
  and nothing was committed), `≡` a conversation being folded. A running agent
  with children out wears `⏸N` — the count, beside its own phase and never
  instead of it — and `⊘` marks both a cancel in flight and a run that landed
  stopped, so a guard-stop is not dressed as a failure. `✉` marks a result its
  parent has not read, `✉N` the ones from an agent's own children. Tool calls are
  `⚙ name summarized-args` (never raw JSON, the tools that steer a run included:
  `⚙ control #4 message "…"`), and notices are neutral `·` unless something
  actually failed (`!`).

```
┌ agents · 2 working · 1 waiting · Σ +324 −40 ─────────────────────┐
│▶· #0 ⏸1 root                                                     │
│   ◐ #1 ⏸1 lexer   waiting on results 3s                          │
│     ◐ #2 tests  mush/2 +324−40  edit_file tests/lex.rs 3s        │
│   ✓ #3 docs       wrote README.md                                │
├──────────────────────────────────────────────────────────────────┤
│ #2 write tests for the lexer                                     │
│ edit_file tests/lex.rs 3s · .mush/wt/2 · git diff HEAD...mush/2  │
└──────────────────────────────────────────────────────────────────┘
```

The cursor row is the one wearing the pane's selection colour — which is now the
workspace's own hue, below; there is no separate marker glyph, because the row's
own `▶` already says which agent the chat pane is showing and two arrows beside
each other said two things at once.

The plumbing is a `mush-core/src/git.rs` (shell-outs like the worktree code, no
new crates) exposing `status(dir)`, `branch(dir)`, `branch_stat(dir, base)` and a
`--shortstat` parser, plus one `App` snapshot (`git`, `agent_stats`) refreshed on
agent `Done`/`Error`/`Stop`, on a focus change, after a command, on a durable
session flush, and every two seconds while anything is running. A nested agent is
measured against its *parent's* branch, which is what makes the Σ in the title
exact.

The context window is resolved the same way: a window the human stated
(`--context` / `MUSH_CONTEXT`, the home config's `context`, or this workspace's
stored choice) › what the endpoint advertises (`max_model_len`, `context_length`,
`context_window`, `n_ctx`, at the top level or under `meta`) › the model's
documented window (`deepseek-flash` and `deepseek-v4-pro`: 500k) › the provider
default. Derived windows are never persisted — they are re-read, so a stale guess
cannot outlive its cause — and the caps a tool result may use follow the window,
so one command's output can never fill an 8k transcript. A server that complains
about the context length teaches mush the number it names, and the run retries
once.

Everything mush knows about a named vendor — the name a human types, its
default endpoint, the models it documents, their windows, whether the thinking
field is sent and with what effort, and what the status bar calls its host — is
one row of `provider::PROVIDERS` in `mush-core/src/provider.rs`. No other file
names a vendor: the help text, the `/provider` picker, the error messages and
the home config's own header are all spelled from that table, and a unit test
fails on a vendor literal found anywhere else in the two crates' production
code. Adding a provider is a row there, not a hunt.

### Notices are typed, and they age `[DECIDED]`

The lines mush writes *about* a conversation are now a value of their own, and
that value carries the three things the questions below needed: the agent it
concerns, when it happened, and which of two kinds it is. `Chat` owns them, and
every way a line reaches the pane is a method on it.

**Chatter** is a line about one moment, said at one moment — a hint, a command's
answer, a diff, a usage line, anything a run did not fail at. It ends when the
moment does: the agent's next run clears it (`clear_notes_for`), the human's next
send clears it (`dismiss_said`), and `SAID_TTL` — two minutes — clears it if
neither happens, so a `/help` read once cannot become furniture on a screen left
alone. Identical consecutive lines collapse into one with a `×N` count, and the
count is the difference between "the reply was empty" and "the reply was empty
every turn until the run gave up".

**News** is a failure, or a run mush itself stopped. It belongs to its run, not
to a moment: a new one replaces the agent's old one (two failures for one agent
would disagree about which is current), it is written to `.mush/session.json` so
a restart still says what broke, and only the agent's next run replaces it. A
stop is not a failure — the loop guard ends a model that kept repeating one call,
and nothing the model did broke — so it is painted `⊘` in yellow, the same
reading the row gives that agent, and it is one line with the guard's own notice.

The foot paints the notes under the transcript, oldest first, headed by a failure
and capped: two rows for the notes and one for the count, so a busy agent cannot
spend the conversation's own rows on mush's own lines. What did not fit is one
row saying `+N more lines · /notes`, and a pane too short for even that carries
the count in its title. `/notes` reads the whole list, opening on the head of the
newest note (the row that says *when* it happened) and labelling itself `line
n/m`, so a long note that wrapped is read from its start rather than its middle.

What this replaced — one plain list, every notice painted under the newest
message with one lifetime for all of them, `/forget` keeping a forgotten agent's
lines invisibly, and a failure from twenty runs ago reading as the newest thing
said — is gone.

### One hue per workspace `[DECIDED]`

Two mush windows on two workspaces were indistinguishable: same borders, same
focus badge, same selected row, same eight colours. The screen knew *what*
everything on it was and nothing about *whose* it was.

- **The path is the identity.** `FNV-1a 64` of the workspace's canonical path —
  written out in `theme.rs` rather than taken from `DefaultHasher`, whose output
  is documented as unstable across Rust releases, because a colour that changed
  when mush was rebuilt is the very confusion the hue prevents — indexes a table
  of thirty hues. Canonicalizing first makes `cd work` and `cd work/.` one window
  in one colour; a path that does not resolve (a `--print-config` on a workspace
  that does not exist yet) is hashed as it was typed.
- **Chrome wears the hue, content does not.** Seven sites take the accent: the
  focused borders, the picker's frame and its selected row, the message prompt,
  the bar's badge, the selected agent row, and an activity line. The alert red,
  the notice yellow, the dim gray and the body gray stay fixed — *what happened*
  reads the same in every window, and only *whose window this is* changes.
- **The thirty are separated perceptually, not by name.** Sampled around the hue
  circle with alternating lightness, all of them in the L\* 60–85 band where the
  `Color::Black` text mush paints on the badge and the selected rows still reads,
  and no two closer than ΔE2000 ≈ 11.8 — so two windows are told apart at a
  glance rather than by comparing swatches. The names are lowercase, one word and
  typeable (`MUSH_THEME=olive`), and a test forbids `auto`, `256` and `off` from
  being names.
- **The form follows the terminal.** `COLORTERM=truecolor`/`24bit` (or a `TERM`
  that names a direct-colour mode) paints the hue's own bytes; anything else
  paints the nearest entry of the terminal's 256 — searched over the 6×6×6 colour
  cube and the gray ramp, since the first sixteen entries are the terminal's own
  palette and are meant to be chosen by name. The `Color` variant *is* the form,
  so a window described as truecolor cannot be holding an index.
- **`MUSH_THEME` overrules all of it.** A hue's name, `256` to demand the indexed
  form on a truecolor terminal, `off` for the fixed palette of every version
  before this one, `auto` for unset. An unknown value is a startup error listing
  every spelling that works — a typo costs a message, not a window in a colour
  nobody asked for.
- **`--print-config` says what the window would look like**: `theme  olive
  (truecolor, from the workspace path)` — the hue, the form it will be painted
  in, and the fact that chose it, so the dump a human compares two windows with
  is the same fact the windows have.

Left as it was: the colours of the *conversation* — the cyan `you ›` and
`brief ›` voices, the magenta `parent ›`, and the footer's cyan agent id. Those
are speech rather than chrome, and a voice whose colour changed per window would
be one more thing to learn. The hue is not a theme system: there is no setting
for the alert red, because a failure has to look the same wherever it is read.

---

## 5. Persistence: everything in `.mush/`

```
<workspace>/
  .mush/
    .gitignore     # contains a single line: *
    session.json   # the conversation, model, provider, endpoint, and stored failures
    wt/            # isolated agents' git worktrees (when used)
```

The API key is never stored here — it lives in the machine-global home config
(`$MUSH_CONFIG`, else `~/.config/mush/config.json`), set with `/key` or
`MUSH_API_KEY`.

That file is meant to be hand-edited, and it documents itself. Every field is
optional — `api_key`, `provider`, `base_url`, `model`, `context` (a stated
window; the built-in default is 120 000 for DeepSeek and 8 192 for a custom
endpoint, spelled from the provider table), `temperature`, `max_completion_tokens`
(`true` sends the reply cap — an eighth of the window, floored at 1 024 and
capped at 120 000, and `--print-config` prints the number this window affords —
as `max_completion_tokens`), `reasoning_effort` (`"low"`,
`"high"` or `"max"` — exactly the values DeepSeek's OpenAI format documents), and
`thinking` (`true` asks for the provider's thinking mode, `false` sends no
`thinking` field and leaves the model's own default) — and the file mush writes
opens with a `_comment` header naming the precedence and each field, as plain
JSON rather than a JSONC dialect. Unknown keys are ignored *and kept* when mush
rewrites the file, and so is any field a particular writer leaves unstated: `/key`
saves the connection without erasing what a human typed by hand. There is no
`param_style`, because the only parameter-name switch mush has is the reply cap,
and no `history_budget_multiplier`, because the window is the knob that budget
derives from.

The two thinking knobs are the same kind of setting: unstated, the provider's
own default applies (DeepSeek asks for `{"type":"enabled"}` and
`reasoning_effort: high`; every other endpoint gets neither field), and stated,
the human's value is sent wherever they pointed mush — that is what makes a
local thinking model configurable at all. `--thinking off` is the honest off: it
sends *no* `thinking` field rather than a `{"type":"disabled"}` mush has no
documentation for, so the model's own default stands. A value mush does not know
is rejected at startup by name, never sent and never quietly replaced by a
default.

`mush --print-config` prints what those layers resolved to — endpoint, provider,
what the stored session was (`none`, how much of a conversation it read, or that
the file is there and *unreadable*), model, window and whether a human stated
it, temperature, reasoning effort and thinking mode (each with whether a human
stated it), the reply cap's size and the name it travels under, the key masked,
`-y`, and the hue the window would wear — and exits 0 without opening the
terminal, creating `.mush/` or taking the workspace lock.
It is the honest view of the precedence, and what makes a hand-edited file
debuggable. The other flags a human would type are `--temperature F`,
`--reasoning-effort LEVEL` (`low`, `high` or `max`; also
`MUSH_REASONING_EFFORT`), `--thinking MODE` (`on` or `off`; also
`MUSH_THINKING`), `--max-completion-tokens`, and `-y`/`--yes`, which *records*
that this session's human pre-approved the work: mush has no approval prompt yet
(the single-owner rule above), so the flag is a record for the features that
will ask, and today it changes nothing.

Resolution order on startup: CLI flags > env > saved session > home config >
built-in defaults.

`.mush/.gitignore` containing `*` ignores every file in the directory,
**including itself** — so the directory never shows up in `git status` and never
needs to be added to the project's own `.gitignore`.

`session.json` is written on its own thread. A streamed message only marks the
conversation dirty, and the file is rewritten at most once a second — so a tool
result costs the screen nothing — while a sent message, a command that changed
what is stored, a compaction and a new chat (Ctrl-N) are written before they
return, and quitting writes whatever is still only in memory. Quitting therefore
loses nothing, and a crash can cost at most the last second of a streamed reply.
On startup the conversation resumes where it left off. Ctrl-N clears it.

It carries every subagent's transcript too, so a relaunch brings the tree back
with its briefs and its context. It also carries each agent's last **failure**
(and only a failure: a command's answer and a hint answered a moment that a
restart has none of), so a broken run is still on screen next time the workspace
opens. A restored agent comes back **at rest**: its row shows how its last run
ended, its mailbox is live, and the human's next message
is what starts it. Opening mush is not a request — an agent that was mid-run when
the process ended resumes from the transcript it had.

---

## 5.5 Subagents: actors, not a framework

Agents are one thread each, owning one transcript; parents and children talk
directly through mailboxes (`spawn_agent`, `status`, `control`, `wait`), while
the UI observes via id-tagged events. Depth and live count are hard budgets; the
delegation tool is simply omitted from a leaf's schema. Every agent does its own
file I/O on its own thread. A child given a
`base` gets its own git worktree (`.mush/wt/<id>`, branch `mush/<id>`) forked
from that ref; without one it shares the checkout, and only one shared child may
run at a time. A `base` git cannot resolve is a *failed delegation* —
`cannot start from <ref>` — refused before anything is created, never a child
that quietly runs somewhere else.

Human-in-the-loop is the merge story: a run's work is committed to its branch
when the run ends (`mush #<id>: <brief>`, with the outcome spelled into the
subject when the run stopped, was cut off, or failed), so the branch really
carries it. The selected row's footer names where it is (`.mush/wt/<id>`) and the
git command that reads it (`git diff HEAD...mush/<id>`, git's own spelling), and
landing or dropping it is git's own work too — `git merge mush/<id>`, `git
worktree remove .mush/wt/<id>`, `git branch -D mush/<id>`. mush never
auto-merges. Leftover worktrees are rediscovered on startup from `git worktree
list`, so those commands keep working across a restart.

An orchestrator that ends its turn while children still run is not finished, it
is napping: the completion is folded into its transcript as a user message and
the run restarts, so a result is never lost just because nobody called
`wait` in time.

Two different messages end an agent's work, and the difference matters:
`Stop` cancels the run in flight (Ctrl-C, `c` on a running row) and leaves the
actor alive to be nudged again; `Shutdown` ends the actor (Ctrl-N, the new
chat). An actor holds a handle to its own mailbox, so it can never infer that
everyone else let go — it has to be told. Every event carries the conversation
it belongs to, so an actor that is still finishing a request when the human
starts a new chat cannot write into it.

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
| history until it is merged | ports (a dev server, a benchmark's fixed port) |
| | build caches, package stores, `~/.cargo`, `/tmp` |
| | databases, containers, dev servers |
| | wall-clock, so every timing measurement |
| | tokens — invisible, and the one with a bill |

`[DECIDED]` Three rules follow.

**1. Long commands detach; they are not killed.** A command that outlives
`CMD_DETACH_AFTER` (60 s) stops being a tool call and becomes a **job**: mush
answers `[still running — detached as #c2; you will be told when it finishes]`
and the process keeps going in its own process group (a dev server asks for the
same thing from the start — `run_command`'s `detach`, §3). This replaces the
plain 120 s kill, which was exactly wrong for a fresh worktree's cold build: the
agent lost the build, read a timeout, and usually started over. (When the
machine-wide budget is already full there is no room to hand a job to, and the
foreground timeout is the whole story then.)

**2. A job is a second-class actor.** It has an id, a command, an owner, a start
time, an exit status, and a bounded window of output. `CommandDone` lands in its
owner's mailbox exactly like `ChildDone`: it wakes a napping agent, is delivered
once, and folds in as `#c2 done: exit 0 · 3m12s · cargo test — test result: ok. …`.
Which call lists a job, stops one, or waits on one is §3's table, one home for
the contract — this section is about what the machine does, not what the schema
says.

Jobs are budgeted (`MAX_JOBS = 8`, machine-wide and beside `MAX_AGENTS`) because
each is a thread, a process group, and disk. They die with their agent
(`Shutdown`, Ctrl-N), with mush itself — its process groups are killed on exit,
where a build an agent started used to outlive a clean quit — and with a `Stop`
aimed at their owner, because Ctrl-C means “stop the work in flight”, and a job
is work in flight.

**3. One command at a time may own the machine.**
`run_command({command, exclusive: true})` takes a workspace-wide lock.
Timing-sensitive work — benchmarks, profiling, `--test-threads=1`, anything that
binds a fixed port — then runs without a sibling stealing cores or a port, and
everyone else is queued behind it and then refused, by name, rather than
silently interleaving: `#3 holds the machine with an exclusive command (…); this
call queued and the lock was still held. wait blocks until the machine is free —
a result the wait hands over first says the lock is still held, so wait again —
and then make this call once more; do not retry it in a loop`. The refusal has a
road back that is not a retry: a subagent's `wait` blocks while another agent
holds the machine (the root's does not — see below), so the refused call can run
once the machine is free. The road is the *bare* `wait`: a targeted one
(`wait({on})`, H34) is not the lock's business — it waits for its target, and a
target that holds the lock releases it when it ends. A wait that finds a result nobody has read hands that
over first, so the road back can be two waits: the refusal says so, the `wait`
schema owns the order, and the hold that outlives it is named beside the result.
A detached exclusive job holds the lock for its whole life.
The lock coordinates *agents*; it cannot see the human's own build or an
unrelated process, so it is “agents do not fight each other”, not isolation. It
is also a lock on `run_command` and nothing else: the file tools take no lock, so
`read_file`, `list_files`, `search`, `write_file` and `edit_file` all work beside
a held one (H31) — a sibling's benchmark is exactly when an agent wants to
re-read a file, and refusing that read was never the point. The
root is exempt from a lock it did not take — it commands beside a held one and
is *told* it did (`beside_note`) — because being blind for the duration of a
child's benchmark cost the orchestrator its only lever (H13); its own
*exclusive* claim is still refused, because two claims to own the machine is the
one thing the lock prevents, and its `wait` does not block on the lock for the
same reason.

`[DECIDED]` The job budget is one machine-wide cap (`MAX_JOBS`), not a per-agent
one — a per-agent cap would let eight agents hold eight builds each, which is the
situation the cap exists to prevent — and a job does **not** count against
`MAX_AGENTS`: the two are separate budgets for separate resources. **A job also
has an age ceiling: `JOB_MAX_AGE` = 4 h of wall time, hardcoded.** `MAX_JOBS`
bounds how many jobs may exist and says nothing about how long one may hold its
slot, its process group and its scratch files; without the ceiling a hung
command a model started with `detach` held all three until mush quit, because a
job has no tool call to time it out — a foreground command does, and past
`CMD_DETACH_AFTER` it is a job instead. Four hours is past any honest build,
test or benchmark and still finite, and the kill says so: `#c3 killed: it ran
past the 4h ceiling · 4h0m · cargo run`. There is no knob, on purpose — a
ceiling a config can raise is not a ceiling on the disk every agent shares. A
job's kept output is a **tail**, consistently, in the completion line and in
`status` alike: a job is read when it *ends*, and what ended it (`test result:
FAILED`, `error: could not compile`, the panic) is at the bottom, not the top.

`[OPEN]`, in §11: where the human sees jobs at all, beyond the `⚙N` their owner's
row already wears.

---

## 6. Architecture

Single-owner state. No locks. No async runtime.

```
                       ┌──────────────── UI thread ────────────────┐
  crossterm events ───▶│  Msg::Key ─┐                              │
  agent events     ───▶│  Msg::Agent ┴─▶ App::update(&mut self, Msg)│
                       │                          │                │
                       │                          ▼                │
                       │   App::screen(area) ─▶ Screen ─▶ ui::draw(f, &Screen)
                       └───────────────────────────────────────────┘
                                   ▲
                                   │  AgentEvent (id-tagged)
                       agent threads: model HTTP loop
                       └ the shell runs on the agent's own thread
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
      config.rs      endpoint/model/api-key/window resolution (the startup precedence)
      git.rs         branch, dirty count and per-branch diffstat, from `git` shell-outs
      message.rs     OpenAI-compatible message + request/response types
      prompt.rs      the system prompt and the tool schemas
      provider.rs    the provider table: a vendor's endpoint, models and defaults
      session.rs     `.mush/` creation and conversation persistence
      text.rs        display-column arithmetic: wrap, truncate, fit_row, mask, sanitize
      tools.rs       tool names, argument helpers, exact-match edit semantics
      transcript.rs  pairing, repair, trimming and the compaction trigger
      userconfig.rs  the machine-global config file (where the API key lives)
      workspace.rs   path resolution, whole reads, atomic writes
    mush/        # the binary: TUI + agent
      main.rs        CLI, terminal guard/panic hook, event loop
      app/mod.rs     state, `update`, intent and command dispatch
      app/tree.rs    agents, ids, phases, focus, git facts — the tree's one owner
      app/chat.rs    transcripts, notices, the message box and the scrollback
      app/settings.rs  the `ConfigCell`: one owner for endpoint/model/key/window
      app/keys.rs    key → `Intent`, as a pure table
      app/commands.rs  the slash commands: one parse, one table
      agent.rs       agent actors, model loop, tool dispatch, shell execution
      clipboard.rs   the system clipboard's image: wl-paste / xclip / pngpaste
      jobs.rs        the job registry: detached commands, the machine lock
      model.rs       the `ModelClient` seam, the HTTP client, the transport retry
      machine.rs     the shell seam: spawn, poll, kill a command
      clock.rs       the clock seam: now and sleep, faked in tests
      events.rs      the event seam: how an actor reports to the UI
      session_save.rs  the writer thread behind `.mush/session.json`
      input.rs       the message box's grapheme cursor and horizontal window
      http.rs        a few hundred lines of blocking HTTP/1.1 client
      ui.rs          the painter: reads a `Screen` a value at a time and paints it
      app/screen.rs  every painted value, derived by `App` (layout, rows, words)
      attach.rs      the M3 socket: `.mush/mush.sock`, one JSON request per line
  docs/mush.md
  scripts/          pty smoke test + screen printer + scripted mock model server
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
| `unicode-width` | correct wrapping and columns for wide glyphs |
| `unicode-segmentation` | grapheme-correct cursor edits (already compiled via ratatui) |
| `unicode-truncate` | display-width truncation and slicing (already compiled via ratatui) |
| `tempfile` | secure scratch files for command output, atomic replace |
| `dirs` | platform-correct config directory |
| `rustls`, `webpki-roots` | TLS for hosted https endpoints (DeepSeek); the client stays hand-rolled |

Not used, on purpose: `tokio`, `reqwest`, `clap`, `ropey`, `notify`, `anyhow`,
`blake3`, `diffy`. HTTP is hand-rolled because the target is a plain-HTTP server
(or a rustls-wrapped socket), and a few hundred lines beats a dependency tree.
`ureq` is the tempting swap, but its receive timeout is a total budget rather
than a per-read one, and the cancel flag is polled *between* socket reads — that
is the feature Ctrl-C depends on. CLI parsing is a hand-written `match` over the
arguments, and it stays that way. Each omitted crate is one less thing to
version, audit, and wait for.

---

## 8. Performance

| Metric | Target | Reality |
|---|---|---|
| Cold start | < 20 ms | ~2 ms; model discovery is only fetched when no model is named |
| Model discovery | < 50 ms | one `GET /v1/models` (~20 ms cold, ~4 ms warm), or on `/model`, `/models`, `/url` |
| Keypress → screen | < 5 ms | `update` touches only UI state; draw only when dirty |
| Idle CPU | ~0% | blocked on a 30 ms poll, no spinner unless an agent is running |
| Memory | < 15 MB | the agent tree, the transcripts, and one message box |

An unreachable endpoint cannot hang startup: connections are bounded by a 5 s
`connect_timeout`, and the model list by a 10 s read timeout — after which the
window opens and reports no model. A chat completion, by contrast, may take as
long as the model needs: one 10-minute deadline bounds the whole request, while
the socket itself is read in 200 ms slices so the reader can notice a
cancellation — and the deadline and cancel flag are checked after every
successful read too, not only on a timeout, so a server dribbling one byte per
slice cannot outlive them. Ctrl-C therefore stops a model that has not answered
instead of waiting for its reply, and a wedged endpoint still cannot pin a
thread forever. Name resolution is the one step std cannot bound itself, so it
runs on its own thread behind a 10 s deadline the caller waits on: a lookup
that outlives it is abandoned with a `TimedOut` naming the host, never a hang.

Rules: no full-buffer scan per frame, no redraw without a state change, and no
subprocess inside `draw` — the git snapshot is cached in `App`, read on its own
thread, and refreshed on the transitions a human drives (a focus change, a
command, a durable session flush), on agent `Done`/`Error`/`Stop`, and by a
two-second tick while anything is running. A read older
than ten seconds is labelled with its age, because a cached fact must not read as
a live one. Release profile uses `lto = "thin"`, `codegen-units = 1`,
`strip = true`.

---

## 9. Roadmap

**Done**

- **M0 — Core.** workspace path resolution, atomic writes, session persistence,
  message types, prompt/tool schemas, config resolution.
- **M1 — Editor.** `[REMOVED v0.2]` open/edit/save, modal keys, panes. mush is not
  an editor; the message box of §4 is what remains of it.
- **M2 — Agent.** OpenAI function-calling loop, a file-editing tool and the shell
  on the agent thread, streaming-free status spinner, cancellation, history
  trimming, context compaction.
- **M2.5 — Subagents.** actor-per-agent with mailboxes, delegation tools, the
  agent tree, isolated git worktrees, wake-on-completion, bounded depth and
  fan-out.
- **M2.6 — Honest worktrees.** `[DONE]` An isolated run ends by committing its
  worktree (`mush #<id>: <brief>`, synthetic identity, hooks skipped), so the
  branch carries the work and `git diff HEAD...mush/<id>`, `git merge mush/<id>`
  and `git branch -D` do what they say.
- **M2.7 — Glance layer.** `[DONE]` Ranked agent rows with a selected-row footer;
  the workspace bar (one line of what just happened, then the stable facts); size
  tiers with a 40×10 floor and a width cap; truthful glyphs; tool calls as
  `name(summarized args)`; typed notices with kinds and lifetimes;
  `mush-core/src/git.rs` and one cached, aged snapshot (§4.5). The context window
  is discovered (endpoint → model table → provider default), shown, stated with
  `--context` / `MUSH_CONTEXT` (and remembered in the session), and the tool caps
  scale with it.
- **M2.75 — Seams.** `[DONE]` [docs/refactor.md](refactor.md): the extractions so
  every fact has one owner, and the four test seams (`ModelClient`, `Machine`,
  `Clock`, `Events`) so the gate needs no model server, no free port, and no
  wall-clock wait. Stage
  3.5's `Intent` keymap and parsed commands landed with it, and so did Stage 3's
  other half — the `Screen` value and the draw sweep that asserts painted text
  (`app/screen.rs` + `ui.rs`, `7e123e1`).
- **M2.8 — Concurrent work (jobs + one lock).** `[DONE]` A command that outlives
  `CMD_DETACH_AFTER` (60 s), or that asked with `detach: true`, becomes a **job**:
  ids drawn from the tree's one counter (`#c2`), a machine-wide registry
  (`MAX_JOBS = 8`), the job surface of `status`/`control`/`wait`, a kept window
  that is the **tail**, jobs that die with their owner and with mush, and
  `run_command({exclusive: true})` taking a workspace-wide lock that names its
  holder (§5.6). This is the milestone for the machine, the way
  M2.6 is the milestone for the branch.

- **M3 — External agents (attach).** `[DONE]` A UNIX socket at
  `.mush/mush.sock` plus `mush read/agents/focus/edit`, so an agent you run
  yourself can drive mush. Newline-delimited JSON; requests carry an `id`;
  `edit` carries a base revision and returns `conflict` rather than guessing.
  The attach thread never touches `App`: it sends a `Msg` and waits on the
  reply, so the event loop stays the only effector. `read` answers an agent's
  transcript lines with a monotone revision, `agents` answers the roster the
  tree pane paints (this is the half of finding H1 that no longer needs
  `.mush/session.json`), `focus` selects an agent and shows its transcript
exactly as `Enter` on its row does, and `edit` replaces the shared message box —
or, with `send`, delivers the human's message — only when the base revision still
matches.

**Next**

M5 and M6 are not started; M4 is dead.
- **M4 — FS watching.** `[OBSOLETE v0.2]` There are no buffers to merge into;
  the periodic git snapshot already tells the human what moved.
- **M5 — Spawn mode.** `mush` launches a configured agent in a pty pane with
  `MUSH_SOCKET`/`MUSH_ROOT` injected, so "works with any agent" covers binaries
  that know nothing about mush.
- **M6 — Polish.** Transcript search, a config file, optional MCP bridge as a
  separate binary, and per-agent token accounting (invisible, and the one with a
  bill).

Each milestone ends with a demoable, tested artifact. No milestone depends on a
later one.

---

## 10. Testing

- **Unit tests.** Message-box semantics (grapheme edits, the cursor window, wide
glyphs), path resolution and escaping, capped reads and the command cap, atomic
writes, session and user-config round-trips, `.mush` self-ignore, history
trimming and its termination guard, compaction, tool-execution semantics
(exact-and-unique edits, a batch that lands all-or-nothing), tool-pair repair,
argument validation, shell-command timeout, cancellation, output cap and
runaway-writer limit, URL/status-line parsing (IPv6 literals included), the
model-list timeout, cancelling a chat
request mid-wait and the request deadline (plus the slow-but-alive body the
slices must not mistake for one, and a dribbling body the deadline must still
stop), an oversized or malformed response body, the git snapshot (branch, dirty
count, per-branch diffstat, ref names that look like flags), the context-window
precedence and the caps that follow it, row field priority and column-aware
truncation, the `~` elision boundary, a draw sweep over fifteen terminal sizes ×
a sweep of states that asserts the *painted* text (not "does not panic"), the
attach protocol's ops and one real socket exchange,
the job registry (detach, the machine lock, the tail window),
the transport retry and what it must *not* retry, the notices' kinds and
lifetimes, per-conversation scrollback, the floor refusing every key but `Ctrl-Q`,
config precedence, schema/prompt invariants, word wrapping, the actor mailbox
(parked nudges, Stop vs Shutdown, completion delivery), and the new-chat, Ctrl-C,
stale-event, steering-echo, stale-status, id-floor, and phase-restore state
transitions.
- **End-to-end (pty).** `scripts/smoke.py` drives the real binary over a
  pseudo-terminal with the pty as its controlling terminal (so window size and
  SIGWINCH behave as they do in a terminal). Scenarios: agent (needs a model),
  resize (needs nothing), cancel (needs nothing — a socket that accepts the chat
  request and never answers must be abandoned by a single Ctrl-C, which is only
  observable from outside the process).
- **Deterministic orchestration.** More than twenty `cargo test` scenarios
  drive the real actor loop in process, on a scripted `ModelClient` rather than a
  server; six of them spawn a real subagent actor. Between them: a root → child →
  grandchild chain, an isolated child whose run must commit its worktree (the test
  then runs git's own merge, worktree remove and branch delete), a context
  overflow that must compact (and the corners where a fold is refused or parked),
  a nudge that arrives mid-reply and must be answered, a root that ends its turn
  while a child still runs and is woken by its result, a stop acknowledged as a
  stop, and a run that hits its runaway guard and must end with a summary. The
  model is scripted; the work — git worktrees, files, the commit, the merge — is
  real, so they need no socket and no `python3`, though they do need `git`, and one
  scenario waits on a real shell sleep. `scripts/mock_llm.py` is kept for
  hand-driven runs; no test and no script refers to it.
- **Live.** Three `#[ignore]`d tests keep the default suite green offline: two
talk to the configured endpoint (the model list and the shipped reply cap), and
one makes a TLS handshake against `https://api.deepseek.com`.
- **The checks.** `cargo fmt --all --check`, `cargo clippy --all-targets --
  -D warnings`, the unit tests, and the pty resize and cancel scenarios are the
  whole gate; they run anywhere rust and python3 do, so any CI can call them.
- **Screen review.** `scripts/screen.py` drives the real binary over a pty and
  prints the painted screen as text at 200×50 down to 30×8, which is how the ten
  defects of §4.5 were found and how the next layer gets reviewed. Pass `--ask`
  with a reachable endpoint to see the agent's own screens (thinking, cancel,
  done); without it the empty screens need no model at all.
- **Not yet.** A fuzz target for path resolution. Property tests for the merge
  verbs are moot now the merge commands are gone — git is what runs them.

Run it:

```sh
cargo test                 # offline, fast
cargo test -- --ignored    # the three live-endpoint checks
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke --resize
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke --cancel
```

---

## 11. Open questions

1. `[OPEN]` Transcript search: `/find` over the focused transcript, or is the
   scrollback enough?
2. `[OPEN]` Token accounting per agent: a rough meter per row would make a
   `MAX_AGENTS` fan-out legible, but the character heuristic is wrong by design.
3. `[DECIDED]` Config file format: a machine-global JSON file (`$MUSH_CONFIG`,
   else the platform config directory), hand-editable and self-documenting. Not
   `mush.toml` in `.mush/`, which would make the endpoint a workspace fact, and
   not environment-only, which would make a hand-edited setting impossible.
4. `[OPEN]` Should `run_command` be denied by default and enabled per session?
5. `[OPEN]` Do we ship the MCP bridge ourselves, or leave it to the community?
6. `[DECIDED]` The M2.7 tiers cut at 80×20: narrower or shorter stacks the agent strip
   above the chat, and 40×10 is the floor, below which mush says so instead of
   painting shreds. Very wide terminals cap the agent pane at 50 columns and the
   transcript at 110.
7. `[DECIDED]` Notices have an owner agent, a kind and a lifetime (§4.5): neutral
   `·` for chatter, yellow `⊘` for a run mush stopped, `!` in red only when
   something actually failed. A command's answer ends with its moment (the
   agent's next run, the human's next send, or `SAID_TTL`); a failure belongs to
   its run, is written to the session, and survives a restart. The bar's line one
   carries the newest event with no other home, never a repeat of the activity
   strip, and the row's `✗` is derived from the phase, so it cannot go stale.
8. `[OPEN]` Does the human need to *type into* a subagent's pane (today that path
   is a nudge), or is watching enough now that the row and its footer carry the
   brief?
9. `[DECIDED]` Job output is a **tail**, consistently, in the completion line and
   in `status` alike: a job is read when it ends, and what ended it is at
   the bottom of the log. The head is what a *foreground* command keeps, because
   the model reads it while the command still runs; that is a different window
   for a different reader, not a second copy of this one.
10. `[OPEN]` Where do jobs become visible? A running job now adds `⚙N` to its
    owner's row and the selected row's footer names each one, but whether that is
    enough, or they want their own pane, is open. The human should not have to ask
    a model what is running on their machine.
11. `[DECIDED]` One machine-wide cap, `MAX_JOBS = 8`, not a per-agent one (a
    per-agent cap would let eight agents hold eight builds each). A job does not
    count against `MAX_AGENTS`: the two are separate budgets for separate
    resources.

---

## 12. Decision log

- **Filesystem + shell is the universal agent interface.** A socket is an
  upgrade, never a requirement.
- **The UI owns no file state.** Agents do their own file I/O on their own
  threads; there is no live buffer, so there is no save race to design around.
- **mush is not an editor** `[v0.2]`. It manages agents and shows git state at a
  glance. The message box is the only editable text.
- **One owner of state.** `Msg` → `update` → `draw`. No shared mutable state
  between the painter and the work.
- **A wrap-up turn, not a bare error, at the turn limit** `[v0.2]`. A long task
  ends with a summary of what was done and what is left; the bound stays.
- **No async runtime.** Threads and channels; `run_command` off the UI thread.
- **No OT/CRDT.** Plain files, atomic writes, exact-match edits.
- **The prompt is data, not logic.** It lives in one small function beside the
  tool schemas, so the contract can be read in one screen.
- **The direct crates in §7 are the budget.** Anything else must earn its place.
- **`.mush/` ignores itself.** Zero setup, zero footprint in the host repo.
- **An isolated agent's work is committed when its run ends.** A branch that stays
  at its base commit makes the merge git is asked to do a lie, however good the
  diff looks.
- **Every agent gets the workspace rules.** A subagent prompt without them invited
  edits without reading, and "same tools as always" was false for a leaf.
- **The schema reserve is measured, not guessed.** A prompt test fails if the tool
  schemas outgrow `Config::SCHEMA_TOKENS`.
- **A file tool is not a second shell.** `read_file`, `write_file`, `list_files`
  and `search` exist for the two reads a shell cannot serve — one beside a held
  machine lock, and one that is an image — and `run_command` stays for the rest.
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
- **One owner per fact.** A count, a window, a key or a transcript lives in one
  place and is derived on read; nothing is stored twice with two writers.
- **A trait is justified only by a fake a test actually uses.** `ModelClient`,
  `Machine`, `Clock` and `Events` earn their indirection; nothing else does.
- **A delivered result has one owner.** A child's or a job's report is written to
  the model's transcript and emitted to the UI in the same act, so the pane is a
  copy of it and cannot disagree with what the model read.
- **Notices have kinds and lifetimes.** A line about a moment ends with the
  moment; a failure belongs to its run, is written to the session, and outlives a
  restart.
- **A transport hiccup is retried; an answer is not.** A reset or a refused
  connection is asked again three times, each retry announced in the transcript; a
  status the endpoint chose, a body past the cap, or a cancellation is returned as
  it is, first time.
- **A window a human states always beats a default.** `--context` / `MUSH_CONTEXT`
  / the home config's `context` win over what an endpoint advertises, a model
table and the provider's own fallback, and are remembered in the session.
- **A window says whose it is.** Each workspace hashes its canonical path to one
  of thirty hues and the chrome wears it (§4.5); the *content* colours do not
  move, because a failure has to read the same in every window. The hash is
  written out — Rust's standard hasher is documented as unstable across releases
  — so a workspace's colour survives a rebuild, and the path is canonicalized
  first so it survives a change of spelling. `MUSH_THEME` overrules it, and
  `--print-config` names the fact that produced the colour.