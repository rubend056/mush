# mush — manual (v0.3)

> A small, fast terminal surface for coding agents. Open a folder, give the
> root agent a task, and watch the tree of agents work — with the repository's
> branch, dirty count, and line delta always in view.

Status: **implemented and working end to end** (M0–M3 of §9).

This document is the user-facing contract and a map into the code. It is not the
spec: the spec is the doc comments beside the code, written as the reason, and
§12 says where the record this file used to carry now lives.

**How this manual works**

- What a human *does* and *sees* is written here: keys, commands, panes and
  regions, the marks a row wears, the wording of a refusal, the config knobs and
  their defaults, the recipes, and the one-line facts that let a reader predict
  the screen (the trimmer runs before the request is sent).
- A *mechanism* is a pointer, not a paragraph. The sentence names the module and
  the items that own it — e.g. why the trimmer stops where it stops:
  `crates/mush-core/src/transcript.rs` (`trim_history`, `trim_target`,
  `compaction_trigger`) — and the code's own comment is the reason written as
  the reason.
- Anything the code can print is **generated** into the blocks marked
  `<!-- generated: … -->` and checked by `cargo test`, so a stale block is a
  failing test. Each block's own head carries the one command that regenerates
  it (`MUSH_BLESS_DOCS=1 cargo test -p mush --bin mush <the check>`), so a
  reader of the raw file sees what is generated and how to rebuild it; never
  edit one by hand.

---

## 0. TL;DR

- `mush` is a TUI in Rust. One binary. **No async runtime.**
- Workspace-first: `mush [DIR]`, or just `mush` in the folder you are in.
- An **agent is built in**: it talks to any OpenAI-compatible endpoint (the
  built-in default is `http://rubendpc:8078`) and works the workspace through ten
  tools: `read_file`, `write_file`, `list_files` and `search` touch files,
  `edit_file` replaces exact text, `run_command` is the shell, `spawn_agent`
  delegates, and `status`, `control` and `wait` manage what it started — a long
  command becomes a **job** the agent can check on later, and `read_file` is the
  one *tool* road an image can travel by (§3).
- mush holds **no file state**: agents read and write files directly, and the UI
  shows their tree, their transcripts, and the git facts.
- The agent's system prompt and the ten schemas are
  `crates/mush-core/src/prompt.rs` (`RULES`, `DELEGATION`, `MACHINE`,
  `ROOT_ROLE`) and `crates/mush-core/src/tools.rs` (`TOOL_NAMES`).
- Everything mush writes lives in `<DIR>/.mush/`, which **git-ignores itself**.
- Architecture is a single-owner **event loop**: `Msg` in, `App::update`,
  `ui::draw` (§6).
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
  (production, tests, comments) and the total, so the size is checked rather than
  remembered.

### Is not

- A text editor. The agents own file editing; mush never opens a file.
- A full IDE. No debugger, no terminal multiplexer, no project wizard.
- A CRDT / collaborative-OT server. One human, the filesystem is truth.
- Provider-specific, plugin-based, or extensible via a scripting language.
- An agent framework. It ships one small agent loop, not an orchestration layer.

### Why there are file tools beside a shell

`run_command` stays the road for what a shell is for — git, tests, builds — and
the file tools cover the two reads a shell cannot serve:

- **A lock refuses reads too.** A sibling's `exclusive` command refuses every
  `run_command` (§5.6), which is right for a build and wrong for a read: the file
  tools take no lock and work beside one.
- **A shell cannot carry an image.** A picture on disk is bytes no shell command
  hands back to a model that can see; `read_file` is the tool that carries one
  (§3).

Everything else is unchanged: `edit_file`'s exact-and-unique replacement is a
safety property `sed -i` does not have, and any other agent — a shell script, a
different harness — can collaborate through the same two things: the workspace
files and the shell. The tools' own reasoning:
`crates/mush-core/src/tools.rs`, `crates/mush-core/src/workspace.rs`.

---

## 2. The UI owns no file state

Agents do their own file I/O on their own threads; the UI holds only what the
human needs to steer them: the agent tree, the focused transcript, the message
box, and the git snapshot. There is no live buffer, so there is no stale copy,
no lock, and no save race. The UI owns one channel: every input — a keystroke,
an agent event — becomes a `Msg`, and one thread applies it to `App` (§6).

### Safety rules that stay

- **Atomic saves.** Every file write — `write_file`, `edit_file` — is
  temp-file + `rename`; readers never see a half-written file, and a crash cannot
  corrupt the original.
- **Workspace confinement is a convention, not a fence.** Every file tool
  resolves its `path` against the root and rejects an escape (`..`, absolute
  paths, and a component that is not a name at all — a Windows prefix — is
  `invalid path`), and `write_file` refuses the workspace root itself; but
  `run_command` is a real shell. One command shape *is* stopped rather than
  trusted: a walk of the whole filesystem rooted at `/` (`find /`,
  `cd / && find`, `du /`, `ls -R /`, `grep -r … /`) is refused at the spawn with
  a sentence naming the road, because it thrashes the disk every agent shares.
  The gate reads the command text — it sees through `cd`, wrappers and one level
  of `sh -c` — so it guards an honest mistake, not a jail. A *data* road checks a
  name for a different reason: a name it cannot hand back as itself — bytes that
  are not UTF-8, a line break, ends the tools' own trim would move — is left out
  of a listing or a search and counted instead, because a path the model cannot
  pass back to `read_file` is a dead end. The prompt says all of it in one place
  (`RULES`); the guard is `mush_core::whole_disk::refusal`, asked by
  `Shell::spawn` in `crates/mush/src/machine.rs`.
- **Edits are exact.** `edit_file` refuses if an `old_string` is missing or
  appears more than once, so an edit can never hit the wrong occurrence.
- **Every big-text result is capped; inputs are not.** One cap bounds every
  result — a command's output, a file read, a listing, a search — and each
  capped result says so and says the way past it: a result whose head is kept
  ends with `[mush: output truncated at {cap} bytes — rerun it narrower (rg,
  head, a smaller path) to see the rest]`, and a result whose *end* matters keeps
  its tail, preceded by `[mush: output truncated at {cap} bytes (the end is
  shown) — rerun it narrower to see the rest]`. A read has its own window and
  says so too (`[mush: lines 1–200 of 900 — read on with offset=201]`, or
  `— end of file`). A `read_file` of a file past 32 MB is refused (a window
  cannot get past it — the file is opened whole first), and a `search` past 2 MB
  *in one file* skips it and counts it, so a "no match" that skipped a file says
  how many and names `run_command` as the road. An image past its cap is refused
  with a downscale as the road, and an image the run's model is not documented to
  see is refused *before* it is sent, so a request that cannot be read never
  costs a turn. Edits always work on the complete file — a file whose bytes are
  not valid UTF-8 is refused rather than read lossily — and no *input* is capped:
  `write_file`'s `content` and `edit_file`'s replacement are bytes the model
  already sent, so a cap there would save the conversation nothing and cost a
  turn and the work. A write large enough to push the request past the window
  ends that turn at the request's own refusal (`cannot send this request: …`),
  with the bytes already on disk. The caps' sizes follow the window:
  `Config::cmd_cap()` (`crates/mush-core/src/config.rs`), the `CMD_CAP`
  (`crates/mush-core/src/lib.rs`) and the `READ_FILE_CAP` / `SEARCH_FILE_CAP` /
  `IMAGE_FILE_CAP` constants (`crates/mush-core/src/workspace.rs`).
- **Bounded loops.** A run ends when the model stops calling tools; a *loop* —
  the same tool batch five rounds over with nothing changed in between — ends it
  early. Nothing counts turns, so a model that keeps making *different* calls
  runs until the human stops it. A shell command runs in its own process group
  with a 120 s timeout (`CMD_TIMEOUT_SECS`) and a hard 8 MB output limit
  (`CMD_OUTPUT_LIMIT`), and delegation is bounded in depth and fan-out (§5.5).

---

## 3. The agent contract

### System prompt

The entire prompt is generated in `crates/mush-core/src/prompt.rs` and is the
only place it exists — this manual describes its shape rather than quoting it,
because a quote is a second copy that goes stale on its own. The root opens with
the workspace it works in and the job it holds — `ROOT_ROLE`: keep the overview,
decide what happens next, and talk to the human, because the work belongs to
subagents — then `RULES`, the `DELEGATION` policy, and `MACHINE` (what the
machine is like, §5.6). A subagent gets the same blocks without `ROOT_ROLE` (a
child is handed a brief rather than a role, and it does not talk to the human),
and its brief travels as the first user message, mirroring the root's
system + user shape; the UI shows that brief as the child's first message. A leaf
at the depth bound has no `spawn_agent`, so it is not told how to use it. The
prompt is kept small on purpose: every word of it is paid for on every request of
every turn.

### Tools

Ten tools, in schema order — five for the workspace's files, the shell, the
delegation tool, and three that manage what an agent started:

| Tool | Arguments | What it does |
|---|---|---|
| `edit_file` | `path`, `edits` | exact-and-unique replacement; `edits` is always a list (a lone edit is a list of one), `replace_all` opts into an ambiguous match, and the batch lands all-or-nothing in one call |
| `read_file` | `path`, `offset?`, `limit?` | a file as a window of lines, with no line numbers, and one trailing sentence saying what the window left; a png, jpeg, gif or webp — sniffed from the file's own bytes, never its name — comes back as the image itself, if the model is documented to see; works beside a held lock |
| `write_file` | `path`, `content` | create or replace a whole file, parent directories included; the answer is one line naming what it replaced; the workspace root itself is refused |
| `list_files` | `path?` | the files under a path, sorted, one per line; build and VCS directories are skipped; capped at `LIST_LIMIT` names with the way past it |
| `search` | `pattern`, `path?`, `ignore_case?` | a literal string (no regex — a regex engine is a dependency, and `rg` is the shell's), one `path:line: text` per match; binary and huge files skipped |
| `run_command` | `command`, `detach?`, `exclusive?` | a shell in the workspace root, own process group; 120 s timeout, output capped to fit the window, cancellable; a command that writes past the output limit is killed and its result says so; `detach` starts a job at once, `exclusive` takes the machine lock (§5.6) |
| `spawn_agent` | `brief`, `title?`, `base?` | a new agent with its own transcript; `title` names its row, and `base` forks a worktree on `mush/<id>` for it (§5.5) |
| `status` | — | your children and your jobs in one listing: each child's state and branch, each job's state, age and command; `✉` marks a result you have not read; a listing, not a delivery — `wait` hands results over |
| `control` | `id`, `action`, `text?` | stop or message one, naming it as `status` prints it (`2` for a child, `c2` for a job); a job can only be stopped |
| `wait` | `on?` | blocks until every child and every job you own has finished, then one digest; returns at once when there is nothing to wait for; a subagent also waits out another agent's machine lock, gives up after 10 minutes, and a message to it ends the wait early; `on` narrows it to one thing, named as `status` prints it — the rest keeps running, but any result you have not read ends the wait too |

The `spawn_agent` row is omitted from a leaf agent's schema, which is what bounds
the tree, so a root has ten tools and a leaf nine; the `status`, `control` and
`wait` rows are not omitted, because they manage the *jobs* a leaf may run in the
background while it edits. `mush_core::tools::TOOL_NAMES` is the single list of
names, and a test asserts the schemas match it; the depth and fan-out bounds are
`MAX_DEPTH` and `MAX_AGENTS` in `crates/mush/src/agent.rs`.

Tool calls execute as a normal OpenAI function-calling loop: the assistant
message, then one `role: "tool"` message per call, then the next request. Every
call in a batch is answered before anything else happens, because an assistant
message with unanswered tool calls makes most servers reject the whole
conversation. If a model answers without calling a tool, it is done — unless a
child's result or a parked nudge is waiting, in which case the run continues so
the model actually answers it. Transcripts adopted from the UI are **repaired,
not trusted**: the human may have typed while a tool batch ran, and a quit
mid-batch can leave calls with no results; `repair_tool_pairs`
(`crates/mush-core/src/transcript.rs`) moves results back beside their assistant
message and answers whatever is still missing before the request goes out.

### Images

`read_file` is the one *tool* road an image travels by, and it carries it
**inside the tool result**: a message with text and one `image_url` content part
per image, each a `data:` URL of the bytes. The **human has two roads of their
own**, and both end in the same message:

- **A bracketed paste of image paths** attaches the pictures to the next message
  instead of inserting the paths as text — the shape drag-and-drop and a file
  manager's "copy" produce. A paste whose *every* word is an image's path — one
  or several, split on whitespace or newlines — is the human saying "these
  pictures". One bad word — prose, a directory, a missing path, a file that is
  not a picture — makes the whole paste the words it is, so a gesture is never
  half-taken and a paragraph is never hijacked. A paste is never swallowed: when
  a named image cannot ride, the words are inserted as text anyway and the paths
  are the road to a downscale.
- **`Ctrl-V` in the chat pane** attaches the image on the system clipboard: a
  screenshot with no file behind it yet. `clipboard.rs` reads it through
  `wl-paste`, `xclip` or `pngpaste`, on a thread of its own with a deadline, and
  the bytes are saved to `.mush/paste/pasted-<unix millis>.<png|jpg|gif|webp>` —
  the same directory the outside-path copy uses. A reader still running at the
  deadline is killed, and the line names the program, the wait and the road that
  always works (save the picture to a file and paste its path); a picture past
  the cap gets the cap's own refusal. `.mush/` git-ignores itself, so a pasted
  screenshot cannot dirty the tree.

A picture whose file is **outside the workspace** is copied into the paste
directory of the workspace that will read it — the receiving agent's own root, so
an isolated agent gets a copy under its worktree — and that copy is the path the
message carries. The carry is asked again at the send, so a focus that moved
between the attach and `Enter` is copied for the agent that will actually receive
it. The bytes are read, never moved: the original file is the human's, and what
mush promises to keep is the copy. How the paste shapes are read and the bytes
saved: `crates/mush-core/src/workspace.rs` (`pasted_images`,
`save_pasted_image`).

Four facts can refuse an attachment before it is sent — no model at all; a model
the provider table does not document as accepting image parts (`Ctrl-P` is the
road named); a picture the window cannot carry; and the box's own bound on
picture bytes — because an endpoint that may reject image parts must not cost a
turn to discover it. The seeing fact is asked at the box, at the wire, and when
the request is assembled. The box and the wire refuse the message; assembly
cannot, because the words are already in the transcript — it drops the image
parts from the request alone, leaves each message's placeholder text standing
where its images were, and says one line naming `/model`.

A room shortage is said and not refused: an image that does not fit the room the
conversation has left — the history budget minus the system prompt, the
**focused agent's** transcript and the images already waiting in the box —
attaches, and the line says what attaching it costs: the trimmer drops the
**oldest turns** to make room for it, so the words are what is at stake and never
the picture. `/compact` is the road that folds those turns into a summary
instead, and a downscale is the cheaper picture. A picture that outweighs the
whole history budget has nothing to offer but the downscale; the box refuses it,
and a request still over the window where it is assembled is refused with one
line before the wire.

Three facts decide whether an image travels:

- **The format is sniffed, not named.** Png, jpeg, gif and webp are recognised
  by their own first bytes; an extension is a claim by whoever wrote the file,
  and a `data:` URL's mime is read by an endpoint that never sees a name.
- **The model must be documented to see it.** Vision is a per-model fact in the
  provider table (`ModelSpec::vision`; `deepseek-flash` is the one row that
  states it), and everything else — including every model mush has never heard
  of — is off. Being wrong in that direction costs an image the model could have
  read; being wrong the other way costs the whole turn.
- **An image is capped (2 MB) and cannot be windowed.** `offset`/`limit` are
  lines and an image has none, so the refusal names the one road that makes a big
  picture readable: downscale it with `run_command` and read that.

How an image is **weighed** (pixels, not bytes), where its dimensions come from,
and how a stored transcript sheds the bytes in place:
`crates/mush-core/src/message.rs` (`Message::content_parts`, `Message::weight`,
`Message::drop_images`) and `crates/mush-core/src/workspace.rs`
(`image_dimensions`).

### History budget

Small local models have small contexts (a custom provider's default window is
8 K, `provider::PROVIDERS`). Every request reserves room for the tool schemas,
the reply and a margin, and the rest is the history budget the run trims and
folds at. Before each request the agent
folds or trims, in that order, and always cuts at a **user** message boundary so
assistant/tool pairs stay valid.

Trimming drops information, so it is the fallback, not the first move: once the
transcript is near the budget the agent asks the model to summarize everything
important and continues from `system + summary`, which is what lets a long task
survive a small context window. A fold the window cannot hold is not attempted —
it says so, once per state, instead of paying for an endpoint's refusal. When the
trim cannot make enough room, the newest turn's tool results are dropped in
place, each one saying the call is not lost and the same output is one narrower
call away, and a request that still does not fit is refused before the wire.
Nothing goes out over the window.

`/compact` is the same fold asked for by hand instead of triggered by the window
— one routine, so the two cannot disagree about the summary message or about what
a fold costs. An idle agent folds at once (no run is started; the summary *is*
the result); mid-run the request parks like a nudge and is honoured at the next
message boundary, never between an assistant's tool calls and their results. A
transcript that is already `system + one message` is refused with "nothing to
compact" on the transcript and nothing on the wire, said out loud because a
human typed a command. One line survives a cut in every copy: the sentence
`DROPPED_TURNS_NOTE` gives the model — the oldest turns were dropped, so this
transcript is not the whole conversation — and a copy that appends it is moved
back to the place the dropped turns were.

The three reserved numbers live in `crates/mush-core/src/config.rs`
(`SCHEMA_TOKENS`, `reply_cap`, `history_budget`), and `mush --print-config`
prints what they resolve to for the window in front of you: the reply cap's size
and the name it travels under, the tool schemas every request reserves
(`schemas`), and the history budget they leave (`history budget`). The trim
watermark, the fold's trigger and the dropped-turns note:
`crates/mush-core/src/transcript.rs` (`trim_target`, `compaction_trigger`,
`trim_history`, `place_dropped_note`); a fold's own fit test is
`fold_request_fits` in `crates/mush/src/agent.rs`.

---

## 4. The message box

No editor. The one text field is the message box, and it behaves like every other
terminal input: a cursor, arrows, `Home`/`End`, `Backspace`/`Delete`. Edits land
on **grapheme cluster** boundaries (`unicode-segmentation`), so a combining mark
or a ZWJ emoji is one keystroke, and the view is measured in display columns
(`unicode-width`, through `unicode-truncate`), so a CJK glyph takes two. Past the
pane width the box scrolls horizontally instead of clipping its tail: `…` marks
whichever edge is elided, and the cursor is always on screen.

**Keys and commands.** `mush --help` prints exactly these two tables, and so do
the blocks below (a test fails while either is stale):

<!-- generated: keys (blessed by MUSH_BLESS_DOCS=1 cargo test -p mush --bin mush app::keys::tests::the_keys_block_matches_the_code) -->
```
  anywhere:
    Ctrl-Q               quit (a second press confirms while work is running)
    Ctrl-C               stop the focused agent
    Ctrl-X               stop every running agent
    Ctrl-N               start a new chat (a second press stops every agent, drops every transcript)
    Ctrl-P               model picker
    Ctrl-T               show or hide the model's reasoning
    Ctrl-O               show or hide tool output, reports and briefs (a failure always shows)
    Ctrl-F               the focused pane takes the whole screen, and back
    Ctrl-Y               select the transcript: Enter copies, Esc leaves
    Tab / Shift-Tab      cycle panes (agents, chat)

  in a picker:
    Enter                take the selected row
    Esc                  close the picker
    j / k, ↑ / ↓         move down / up the list
    g / G, Home / End    first / last row
    PgUp / PgDn          page the list

  selecting (Ctrl-Y):
    ↑ / ↓                the cursor one line older / newer
    Shift-↑ / Shift-↓    the same move, keeping the selection
    PgUp / PgDn          ten lines at a time (with Shift, keeping the selection)
    Home / End           the oldest / newest line
    Enter                copy the selection, or the cursor's own line
    Esc                  leave without copying

  agents pane:
    ←                    the selected agent's parent
    →                    the selected agent's first child
    Enter                show the selected agent's transcript
    j / k, ↑ / ↓         move down / up a row
    g / G, Home / End    first / last row
    PgUp / PgDn          page up / down the rows
    c                    cancel the selected agent
    Esc                  back to the root agent

  chat pane:
    Enter                send the message
    Ctrl-V               attach the image on the clipboard
    Shift / Alt-Enter    new line in the message
    letters and symbols  type into the message box
    ← / →, Home / End    move the box cursor
    Backspace / Delete   delete in the box; at the start of the box, Backspace pops the newest attachment
    Ctrl-U               clear the words in the box, keeping the images
    Ctrl-Z               put back the words and images the box last lost
    ↑ / ↓, PgUp / PgDn   scroll the transcript
    Esc                  clear the box and its attachments
```
<!-- /generated: keys -->

<!-- generated: commands (blessed by MUSH_BLESS_DOCS=1 cargo test -p mush --bin mush app::commands::tests::the_commands_block_matches_the_code) -->
```
    /provider [deepseek|custom]  switch provider, or pick one from a list
    /model                       pick a model from the endpoint's list
    /url <url>                   point at another OpenAI-compatible endpoint
    /key [SECRET]                show the API key in use, or set one (saved to the home config)
    /models                      refresh the model list from the endpoint
    /context [N|auto]            say the window's size and road, state one, or auto for the table
    /compact                     fold the focused agent's conversation into a summary
    /notes                       read every note about the focused agent, in full
    /help                        list the keys and these commands
    /quit                        leave mush
```
<!-- /generated: commands -->

A paste whose every word is an image's path attaches them all (§3), and anything
else is text and lands in the box as it always did. The attachments are painted
as dim `▣ path (format · size)` rows above the text — one per image, at most
three, fewer when the box has less room, the last of them counting the rest when
there are more — and they travel with the send: `Enter` on an empty box with an
image attached is still a send, because the picture *is* the message. A send that
does not land puts the words and the images back in the box, and `Esc` clears
both.

The box's losses have roads back. `Backspace` at the very start of the box —
index zero, not the start of the wrapped line the cursor happens to be on — pops
the newest attachment, so a picture can be taken back while the words that go
with it stay. `Ctrl-U` clears the words and keeps the images. `Ctrl-Z` puts back
what the box last lost, wherever it went: a pop restores the image, `Ctrl-U` the
words, and `Esc` the words and every image — one slot, not a history, and a send
spends it, so a message that has been sent cannot come back on a keystroke.
Nothing that arrived after a loss is taken away by the key that restores it.
`Esc` says what it cleared and names the road back (`cleared the box and 2
images · Ctrl-Z puts it back`).

`Enter` in the agents pane moves the *view*, not the keyboard: the row's
transcript replaces the chat pane while the keys stay in the tree, and `Tab` is
what puts them in the box, where typing reaches the agent on screen. A page is
always ten rows, in every pane and every list (`PAGE`,
`crates/mush/src/app/keys.rs`). `Ctrl-F` is the frame's version of the same
question — the focused pane takes the whole screen — and because `Tab` already
cycles the focus it is what switches which pane that is. A hidden pane keeps its
facts on screen: with the agents pane a zero rect, its `N working` / `N jobs` /
`N waiting` counts move into the conversation pane's title; the hidden-row counts
(`▲N`/`▼N`, `+N more lines`) stay behind.

The transcript is not only the human's words, and it says so. `you › ` marks a
line the human typed — and only a line the human typed: a subagent's brief opens
its pane as `brief › `, a parent's `control` message arrives as `parent › `, and
a folded result (`#1 done: …`) is mush's own report, marked `· ` like the other
lines mush writes. And the text itself is untrusted: a model reply, a tool result
and a tool call's arguments are defanged before they are painted, so an
`ESC ]0; …` in them cannot rename the terminal window and a `CSI 2J` cannot
repaint the frame they are drawn on. A model's *reply* is read one more way: each
source line is parsed as a small, additive markdown view — line-local except for
a table, whose columns are a fact about the whole block and which the walk reads
and paints as a block — `**strong**`, `*emphasis*`/`_emphasis_` (a span's
content is read again, so emphasis nests), `` `code` ``, `~~strike~~`, one to
three `#` headings, list markers kept with a wrapped row hung under the item's
own text, `> ` painted as a `│ ` bar, `[ ]`/`[x]` as `☐`/`☑`, three or more
`-`/`*`/`_` as a rule across the pane, fenced code, links as `text (url)`, and
tables — the delimiter row names each column's alignment and the cells share the
pane's width exactly, wrapping inside their columns — and painted in the reply's
styles, with only the scaffolding a view does not read (a heading's `#`s, a
quote's `>`, a checkbox's brackets, a table's pipes and delimiter row, a fence's
two lines) left unpainted. It is deliberately not a document renderer — no
paragraph reflow, no nested lists, no HTML — and it changes no bytes: tool
results and `run_command` output, the human's own lines, briefs, notices and the
model's reasoning rows are painted raw, so a `#` there is a comment and an `*` a
glob. What the copy road hands another program is the *source* lines of a reply,
never the painted screen. Both rules live in `crates/mush-core/src/text.rs`
(`sanitize`, `markdown_rows`).

`Ctrl-Y` is that copy road. mush never captures the mouse, so the terminal owns
selection and a drag is a rectangle of screen cells; the mode is a cursor over
the transcript's **source** lines instead. `↑`/`↓` move it one line, `Shift`
holds the selection while it moves, `PgUp`/`PgDn` ten, `Home`/`End` jump to the
oldest or newest, `Enter` copies and leaves, `Esc` leaves without copying. While
it is open the mode has the keyboard — a letter is not typing — and `Tab` leaves
it for the pane cycle. What lands on the system clipboard is `Message::text()`,
exactly: the selected source lines joined with the newlines they have, so a soft
wrap never becomes one, a tab is a tab, a folded block — a tool result, mush's
own report about a child or a job, and the brief a child's pane opens with — is
copied whole even past the eight rows the pane paints of it, and a picture a
saved transcript shed copies as its placeholder. A copy that cannot reach the
clipboard says so in the bar instead: no writer on `PATH` names what to install,
and a writer that never takes the text is killed at the deadline and reported,
not waited on. The bar says `copied 12 lines from #1's reply — 1,284 bytes` once
the clipboard has taken the text.

A thinking endpoint's own reasoning is painted above the turn it decided, dim and
marked `⋯ `, one block per assistant turn. It is the endpoint's
`reasoning_content` for that turn and nothing else: it is already stored with the
turn in `.mush/session.json` and replayed with it on the next request — a
thinking endpoint refuses a replayed turn without it — so the block adds a *view*
of what the request already carries. `Ctrl-T` shows or hides it in every pane;
the toggle writes nothing, sends nothing and is not stored, and a new chat keeps
whatever the human chose. A reasoning that trims to nothing paints no row at all.

`Ctrl-O` is the same kind of view over the output: a tool's result, mush's own
report about a child or a job, and the brief a child's pane opens with. Like
`Ctrl-T` the key writes nothing and is not stored. Two rows stay in both states,
because a hidden failure would be a lie about what happened: a failed result's
own `! error: …` row and a `#1 failed: …` report.

---

## 4.5 The screen

Beyond editing, the UI has one job: answer three questions without typing
anything, at whatever size the terminal is.

| Question | What answers it |
|---|---|
| What is each agent doing? | its `Phase` and the instant it began — the glyph and the activity text are the same fact twice read |
| Where is its work, and on what branch? | the row's branch and line delta, kept ahead of the activity and the title when the columns run out |
| How much has changed? | one cached `git diff --shortstat` per branch, measured against the parent's branch |

**R0 — derive, don't store.** An agent has a `Phase`
(`Idle · Thinking · Activity(label) · Compacting(kind) · Cancelling · Stopped ·
CutOff · Done · Failed`) and the instant it began; the row glyph, the activity
text and the bar are derived from those every frame. `App::status` is a typed
line with a lifetime: an `Info` line fades, an `Error` stays, and work in
progress is never stored at all — which is what makes `✓` on an idle agent and a
lingering `thinking…` impossible rather than merely fixed.

**R1 — ranked fields, then a footer.** A row spends its columns in this order:
state (`glyph · id`), then the marks, then `branch +add −del`, then the activity
with its age, then the title — facts that exist nowhere else survive longest, and
the title yields first because the footer and the transcript carry the brief in
full. The title is derived from the brief (`deep.txt`, `lexer`), so two children
never read the same. The selected row's full facts get a footer under the list,
up to three lines when the pane is tall and one when it is compact, isolated
agents included (`.mush/wt/2 · git diff HEAD...mush/2` — git's own spellings).
The pane's title totals the tree (`agents · 2 working · 1 waiting · Σ +324 −40`
— each count named, each agent in exactly one). `fit_row` is
`crates/mush-core/src/text.rs`; the derivation is `App::rows` in
`crates/mush/src/app/screen.rs`.

**R2 — one bar, two lines.** Line one is one word chosen by rank: an alert (a
failure, the quit prompt) beats the tree's own line (a napping root that will
resume by itself), which beats a said line (a stop, a job's report, a command's
answer), which beats the idle hint. It is forbidden from showing what a row or
the transcript already carries. Line two (on terminals at least **24** rows tall)
is the stable facts, elided from the right:

```
⌂ ~/p/mush │ master ±3 +12−3 │ deepseek-flash @ deepseek.com · ctx 12k/430.5k (fold 387.4k) ~500k
```

`±3` counts paths with uncommitted changes, `+12−3` the line delta against
`HEAD`, and the meter weighs the conversation against the history budget the run
trims and folds at, where the fold's trigger sits inside it, and the window
itself: the one-column mark names the road the number came by — `~` assumed from
mush's model table, `≈` advertised by the endpoint's model list, `≤` named by the
endpoint in a refusal, and no mark when you stated it yourself (`--print-config`
and `/context` name the road in words). `full` marks the budget and `over` one
byte past it. The ranking is `chat::Rank`; the line is `screen::facts_line`.

**R3 — size tiers with a floor.** `w<40 || h<10` → a single notice, centred on
both axes, that always names 40×10, and below it every key is refused except
`Ctrl-Q`. `w<80 || h<20` → **compact**: the agent strip on top, chat below.
`h≥24` → the two-line bar; the transcript is capped at 110 columns however wide
the terminal is, and the agent pane at 50. The tiers are `App::screen`.

**R4 — truthful glyphs.** Every mark a row can wear, rendered by the code's own
row painter, and a whole frame at 100×28 with one row per mark:

<!-- generated: marks (blessed by MUSH_BLESS_DOCS=1 cargo test -p mush --bin mush ui::tests::the_marks_block_matches_the_code) -->
```
 · #0 idle
 ◐ #0 thinking  thinking 3s
 ◐ #0 working  edit_file src/lib.rs 12s
 ≡ #0 compacting  compacting 2s
 ⧗ #0 waiting  waiting on results 3s
 ⊘ #0 cancelling  cancelling 0s
 ⊘ #0 stopped  stopped · re-send to resume
 ⚠ #0 cut off  cut off · nothing committed
 ✓ #0 done  wrote README.md
 ✗ #0 failed  no route to host
▶◐ #0 the focused row  thinking 3s
 ◐ #0 lexer  mush/1 +12−3 ⚙1  edit_file src/lex.rs 3s
 ✓ #0 ✉ result unread  wrote README.md
 · #0 ✉2 two reads owed
 ✓ #0 ⚮ parent gone  wrote src/lex.rs
```
<!-- /generated: marks -->

<!-- generated: frame (blessed by MUSH_BLESS_DOCS=1 cargo test -p mush --bin mush ui::tests::the_manual_frame_matches_the_code) -->
```
┌ agents · 3 working · 1 waiting─┐┌ mush ──────────────────────────────────────────────────────────┐
│▶◐ #0 ✉2 root  thinking 4s      ││you › make the tree show every state                            │
│   ⧗ #1  waiting on results 3s  ││mush › Spawning the children.                                   │
│     ◐ #2 tests                 ││      ⚙ spawn_agent tests probe                                 │
│   ✗ #3 probe  no route to host ││      · spawned #2 (tests)                                      │
│   ⊘ #4 run                     ││      ⚙ wait                                                    │
│   ⚠ #5 build                   ││      · #2 done: 3 tests pass                                   │
│   ≡ #6 fold  compacting 2s     ││      ✗ #3 failed: no route to host                             │
│   ✓ #7 ✉ docs  wrote README.md ││      ⚠ #5 cut off · nothing committed                          │
│   ✓ #8 ⚮  wrote src/lex.rs     ││mush › Every mark is on a row above.                            │
│                                ││                                                                │
│                                ││                                                                │
│                                ││                                                                │
│                                ││                                                                │
│                                ││                                                                │
│                                ││                                                                │
│                                ││                                                                │
│                                ││                                                                │
│                                ││                                                                │
│                                ││                                                                │
│                                ││                                                                │
│                                ││                                                                │
│                                │└────────────────────────────────────────────────────────────────┘
│                                │┌ message ───────────────────────────────────────────────────────┐
│                                ││›                                                               │
└────────────────────────────────┘└────────────────────────────────────────────────────────────────┘
 chat  spawned #8 (orphan) — its parent was reaped
 ⌂ ~/p/demo │ master ±3 +324−40 │ deepseek-flash @ deepseek.com · ctx 12k/430.5k ~500k
```
<!-- /generated: frame -->

In one line each: `·` idle/never ran, `◐` running, `✓` finished, `✗` failed,
`⚠` a run that was cut off (the process went away with it, or the actor's thread
did, and nothing was committed), `≡` a conversation being folded, `⧗` a run
parked on somebody else's result (`wait` — the icon a glance reads says the same
thing the row's words do, `waiting on results 3s`), `⊘` a cancel in flight or a
run that landed stopped, so a guard-stop is not dressed as a failure, `✉` a
result a parent has not read, `✉N` the ones from an agent's own children, `⚮` a
row whose parent the history window has reaped (it is drawn at the top level like
a root child, and the mark is what says it is not one; how many children a parent
keeps is `CHILD_HISTORY` in `crates/mush/src/app/tree.rs`), `▶` the focused
agent, and `⚙N` jobs on their owner's row. A running agent with children out wears *no*
count of them: the children's own rows say they run, and the title's
`N waiting` counts the agents at rest with work out. Tool calls are
`⚙ name summarized-args` (never raw JSON, the tools that steer a run included:
`⚙ control #4 message "…"`), and notices are neutral `·` unless something
actually failed (`!`).

The cursor row is the one wearing the pane's selection colour (the workspace's
own hue, below); there is no separate marker glyph, because the row's own `▶`
already says which agent the chat pane is showing.

**The hue.** Two mush windows on two workspaces are told apart by one hue per
workspace: the path decides it, and the *chrome* wears it — the focused borders,
the picker's frame and its selected row, the message prompt, the bar's badge, the
selected agent row, an activity line, and the select mode's cursor band and its
selection. The *content* colours — the alert red, the notice yellow, the dim gray
and the body gray — stay fixed, because *what happened* reads the same in every
window and only *whose window this is* changes. `MUSH_THEME` overrules the hash:
a hue's name, `256` to demand the indexed form on a truecolor terminal, `off` for
the fixed palette of every version before this one, `auto` for unset; an unknown
value is a startup error listing every spelling that works. `--print-config` says
what the window would look like (`theme  olive (truecolor, from the workspace
path)`) — the hue, the form it will be painted in, and the fact that chose it.
The hash, the hue table and the 256-colour form:
`crates/mush/src/theme.rs` (`Theme::resolve`, `hue`, `HUES`).

**Notices.** The lines mush writes *about* a conversation are typed, and they
age. **Chatter** is a line about one moment — a hint, a command's answer, a usage
line: the agent's next run clears it, the human's next send clears it, and it
expires on its own if neither happens; identical consecutive lines collapse into
one with a `×N` count. **News** is a failure, or a run mush itself stopped: it
belongs to its run (a new one replaces the agent's old one), only the agent's
next run replaces it, and only the *failure* half is written to
`.mush/session.json`, so a restart still says what broke. A stop is not a failure
— it is painted `⊘` in yellow, the same reading the row gives that agent. The
foot paints the notes under the transcript, oldest first, headed by a failure and
capped: two rows for the notes and one for the count, and what did not fit is one
row saying `+N more lines · /notes`; `/notes` reads the whole list, opening on
the head of the newest note and labelling itself `line n/m`. While a run is in
flight the foot's lowest row is the pane's own activity line, in the same words
the row above uses (`run_command cargo test.`, `thinking.`, `waiting on
results.`, the fold's own sentence, `cancelling.`); the dots are the whole
animation, and nothing is painted at rest. The kinds and their lifetimes:
`crates/mush/src/app/chat.rs` (`Rank`, `SAID_TTL`, `Chat::foot`).

**The vendor table.** Everything mush knows about a named vendor — the name a
human types, its default endpoint, the models it documents, their windows,
whether the thinking field is sent and with what effort, and what the status bar
calls its host — is one row of `provider::PROVIDERS` in
`crates/mush-core/src/provider.rs`. No other file names a vendor, and a unit test
fails on a vendor literal found anywhere else in the two crates' production code.
Adding a provider is a row there, not a hunt.

What the audit that shaped this section found, and the defects each rule fixed:
`docs/findings.md`.

---

## 5. Persistence: everything in `.mush/`

```
<workspace>/
  .mush/
    .gitignore     # contains a single line: *
    session.json   # the conversation, model, provider, endpoint, a stated window, and stored failures
    session.json.previous  # the conversation the last new chat cleared
    wt/            # isolated agents' git worktrees (when used)
    paste/         # pictures pasted into the chat (Ctrl-V, or a path from outside)
    mush.sock      # the attach socket, while mush runs
```

The API key is never stored here — it lives in the machine-global home config
(`$MUSH_CONFIG`, else `~/.config/mush/config.json`). `/key` is the one road that
writes it there; `MUSH_API_KEY` supplies one from the environment for the run,
and no other save copies it into the file. `.mush/.gitignore` containing `*`
ignores every file in the directory, **including itself**, so the directory never
shows up in `git status` and never needs to be added to the project's own
`.gitignore`.

That file is meant to be hand-edited, and it documents itself. Every field is
optional: `api_key`, `provider`, `base_url`, `model`, `context` (a stated
window), `temperature`, `max_completion_tokens` (`true` sends the reply cap as
`max_completion_tokens`, and `--print-config` prints the number this window
affords), `reasoning_effort` (`"low"`, `"high"` or `"max"`), and `thinking`
(`true` asks for the provider's thinking mode, `false` sends no `thinking` field
and leaves the model's own default). Unknown keys are ignored *and kept* when
mush rewrites the file, and so is any field a particular writer leaves unstated:
`/key` saves the connection without erasing what a human typed by hand. Unstated,
the provider's own default applies, and a value mush does not know is rejected at
startup by name, never sent and never quietly replaced. The fields and the writer
are `crates/mush-core/src/userconfig.rs`.

`mush --print-config` prints what those layers resolved to — endpoint, provider,
what the stored session was (`none`, how much of a conversation it read, or that
the file is there and *unreadable*), model, whether that model is documented to
see an image (`vision`), window and the road it came by, temperature, reasoning
effort and thinking mode (each with whether a human stated it), the reply cap's
size and the name it travels under, the tool schemas every request reserves and
the history budget those leave, the key — always stated, masked, `(none)` when
there is none — `-y`, and the hue the window would wear — and exits 0 without
opening the terminal, creating `.mush/` or taking the workspace lock. The other
flags a human would type are `--temperature F`, `--reasoning-effort LEVEL` (also
`MUSH_REASONING_EFFORT`), `--thinking MODE` (`on` or `off`; also
`MUSH_THINKING`), `--max-completion-tokens`, and `-y`/`--yes`, which *records*
that this session's human pre-approved the work (mush has no approval prompt yet,
so today it changes nothing). **Resolution order on startup: CLI flags > env >
saved session > home config > built-in defaults.**

`session.json` is written on its own thread. A streamed message only marks the
conversation dirty, and the file is rewritten at most once a debounce — so a tool
result costs the screen nothing — while a sent message, a command that changed
what is stored, a compaction and a new chat are written before they return, and
quitting writes whatever is still only in memory. Quitting therefore loses
nothing, and a crash can cost at most the last debounce of a streamed reply; the
window is `SESSION_DEBOUNCE` in `crates/mush/src/app/mod.rs`. On startup the
conversation resumes where it left off. Over a conversation with something in it,
`Ctrl-N` clears in two steps: the first press says what would go and where it is
kept (`.mush/session.json.previous`), and the second writes that copy and clears;
an empty conversation clears on one press, and a copy that cannot be written
refuses the key. The session carries every subagent's transcript too, so a
relaunch brings the tree back with its briefs and its context; it also carries
each agent's last **failure** (and only a failure), so a broken run is still on
screen next time the workspace opens. A restored agent comes back **at rest**:
its row shows how its last run ended, its mailbox is live, and the human's next
message is what starts it. Opening mush is not a request.

---

## 5.5 Subagents: actors, not a framework

Agents are one thread each, owning one transcript; parents and children talk
directly through mailboxes (`spawn_agent`, `status`, `control`, `wait`), while
the UI observes via id-tagged events. Every agent does its own file I/O on its
own thread. A child given a `base` gets its own git worktree (`.mush/wt/<id>`,
branch `mush/<id>`) forked from that ref, resolved in the spawning agent's own
workspace — so `HEAD` in a base is that agent's `HEAD`, not the application
root's; without one it shares the spawning agent's workspace, and only one shared
child may run there at a time. That count is the directory's live writers, asked
across the whole tree — not one parent's books — and the spawner is never counted
against itself, so a shared child may still delegate into the tree its own run is
in. A `base` git cannot resolve is a *failed delegation* — `cannot start from
<ref>` — refused before anything is created, never a child that quietly runs
somewhere else. Depth and live count are hard budgets (`MAX_DEPTH`, `MAX_AGENTS`
in `crates/mush/src/agent.rs`), and the delegation tool is simply omitted from a
leaf's schema.

Human-in-the-loop is the merge story: a run's work is **committed** to its branch
when the run ends (`mush #<id>: <brief>`, with the outcome spelled into the
subject when the run stopped, was cut off, or failed), so the branch really
carries it. The selected row's footer names where it is (`.mush/wt/<id>`) and the
git command that reads it (`git diff HEAD...mush/<id>`, git's own spelling), and
landing or dropping it is git's own work too:

```sh
git merge mush/3                 # land it on the branch you are on
git worktree remove .mush/wt/3   # reclaim the checkout
git branch -D mush/3             # drop the branch
```

**mush never auto-merges.** Leftover worktrees (a `mush/*` branch with a checkout
still on disk) are rediscovered on startup from `git worktree list`, so those
commands keep working after a restart. The worktree and commit code:
`crates/mush-core/src/git.rs`, `agent::commit_subject`.

An orchestrator that ends its turn while children still run is not finished, it
is napping: the completion is folded into its transcript as a user message and
the run restarts, so a result is never lost just because nobody called `wait` in
time. Two different messages end an agent's work, and the difference matters:
`Stop` cancels the run in flight (Ctrl-C, `c` on a running row) and leaves the
actor alive to be nudged again; `Shutdown` ends the actor (Ctrl-N, once the new
chat is confirmed). An actor holds a handle to its own mailbox, so it can never
infer that everyone else let go — it has to be told. Every event carries the
conversation it belongs to, so an actor that is still finishing a request when
the human starts a new chat cannot write into it. A `Stop` has two halves,
because one of them cannot wait for a mailbox: the message reaches the actor, and
the flag it sets is *shared with the UI* when the run starts, so the HTTP reader
polls it between short socket slices and Ctrl-C interrupts a model that has not
answered yet. The row shows `⊘` while the cancel is in flight.

---

## 5.6 The machine is shared: jobs, detachment, one lock

A worktree isolates files and nothing else. Two agents on the same box share CPU,
memory, IO, ports, caches, services, `/tmp`, the network, and the human's
patience.

| A worktree isolates | Shared anyway |
|---|---|
| files, HEAD, index, branch | CPU, RAM, IO, GPU |
| history until it is merged | ports (a dev server, a benchmark's fixed port), build caches, package stores, `~/.cargo`, `/tmp`, databases, containers, dev servers, wall-clock (so every timing measurement), and tokens — invisible, and the one with a bill |

**1. Long commands detach; they are not killed.** A command that outlives
`CMD_DETACH_AFTER` stops being a tool call and becomes a **job**: mush answers
`[still running — detached as #c2; you will be told when it finishes]` and the
process keeps going in its own process group (a dev server asks for the same
thing from the start with `run_command`'s `detach`). When the machine-wide budget
is already full there is no room to hand a job to, and the foreground timeout is
the whole story then.

**2. A job is a second-class actor.** It has an id, a command, an owner, a start
time, an exit status, and a bounded window of output. `CommandDone` lands in its
owner's mailbox exactly like `ChildDone`: it wakes a napping agent, is delivered
once, and folds in as `#c2 done: exit 0 · 3m12s · cargo test — test result: ok. …`.
A job's kept output is a **tail**, consistently, in the completion line and in
`status` alike: a job is read when it *ends*, and what ended it is at the bottom,
not the top. Jobs are budgeted (`MAX_JOBS`, machine-wide and beside `MAX_AGENTS`)
because each is a thread, a process group, and disk; a job does **not** count
against `MAX_AGENTS`, and the budget is one machine-wide cap rather than a
per-agent one, since a per-agent cap would let eight agents hold eight builds
each. They die with their agent (`Shutdown`, Ctrl-N), with mush itself — its
process groups are killed on exit — and with a `Stop` aimed at their owner,
because Ctrl-C means “stop the work in flight”, and a job is work in flight. A
job also has an age ceiling, `JOB_MAX_AGE` (4 h of wall time, hardcoded): without
it a hung `detach` held its slot, its process group and its scratch files until
mush quit, and the kill says so — `#c3 killed: it ran past the 4h ceiling · 4h0m
· cargo run`. There is no knob, on purpose: a ceiling a config can raise is not a
ceiling on the disk every agent shares.

**3. One command at a time may own the machine.**
`run_command({command, exclusive: true})` takes a workspace-wide lock.
Timing-sensitive work — benchmarks, profiling, `--test-threads=1`, anything that
binds a fixed port — then runs without a sibling stealing cores or a port, and
everyone else is queued behind it and then refused, by name, rather than silently
interleaving:

> `#3 holds the machine with an exclusive command (…); this call queued and the
> lock was still held. wait blocks until the machine is free — a result the wait
> hands over first says the lock is still held, so wait again — and then make
> this call once more; do not retry it in a loop`

The refusal has a road back that is not a retry: a subagent's *bare* `wait`
blocks while another agent holds the machine, so the refused call can run once
the machine is free (the root's `wait` does not block on the lock). A targeted
`wait({on})` is not the lock's business — it waits for its target, and a target
that holds the lock releases it when it ends — and a wait that finds a result
nobody has read hands that over first, so the road back can be two waits. A
detached exclusive job holds the lock for its whole life. The lock coordinates
*agents*; it cannot see the human's own build or an unrelated process, so it is
“agents do not fight each other”, not isolation. It is a lock on `run_command`
and nothing else: the file tools take no lock, so `read_file`, `list_files`,
`search`, `write_file` and `edit_file` all work beside a held one. The root is
exempt from a lock it did not take — it commands beside a held one and is *told*
it did (`beside_note`) — because being blind for the duration of a child's
benchmark cost the orchestrator its only lever; its own *exclusive* claim is
still refused, because two claims to own the machine is the one thing the lock
prevents. The lock itself: `crates/mush/src/jobs.rs` and `lock.rs`. Where the
human sees jobs beyond the `⚙N` their owner's row already wears is `[OPEN]`
(§11).

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

The workload is a handful of short, blocking tasks (HTTP, a shell command, a
pty later), not thousands of connections: synchronous code is linear and
readable, with no `async fn` coloring and no runtime to debug at 2 a.m. Threads
plus channels give the ownership story we want at ~0 startup cost. The refactor
that extracted the seams is `docs/refactor.md`.

### Crate layout

```
mush/
  crates/
    mush-core/   # pure domain: config, messages, prompt, session, tools, workspace. No UI.
      config.rs      endpoint/model/api-key/window resolution, the caps
      git.rs         branch, dirty count and per-branch diffstat, from `git` shell-outs
      lib.rs         the crate root: the module list, the re-exports, and the command caps
      message.rs     OpenAI-compatible message + request/response types
      prompt.rs      the system prompt and the tool schemas
      provider.rs    the provider table: a vendor's endpoint, models and defaults
      secrets.rs     the secrets mush holds, and why a process it starts never inherits one
      session.rs     `.mush/` creation and conversation persistence
      text.rs        display-column arithmetic and the markdown view (line-local, except a table, read as a block)
      tools.rs       tool names, argument helpers, exact-match edit semantics
      transcript.rs  pairing, repair, trimming and the compaction trigger
      userconfig.rs  the machine-global config file (where the API key lives)
      whole_disk.rs  the one command shape refused at the spawn
      workspace.rs   path resolution, whole reads, atomic writes
    mush/        # the binary: TUI + agent
      main.rs        CLI, terminal guard/panic hook, event loop
      app/mod.rs     state, `update`, intent and command dispatch
      app/tree.rs    agents, ids, phases, focus, git facts — the tree's one owner
      app/chat.rs    transcripts, notices, the message box and the scrollback
      app/settings.rs  the `ConfigCell`: one owner for endpoint/model/key/window
      app/keys.rs    key → `Intent`, as a pure table
      app/commands.rs  the slash commands: one parse, one table
      app/screen.rs  every painted value, derived by `App` (layout, rows, words)
      agent.rs       agent actors, model loop, tool dispatch, shell execution
      clipboard.rs   the system clipboard: wl-paste / xclip / pngpaste read an image,
                     and wl-copy / xclip / pbcopy write text
      jobs.rs        the job registry: detached commands, the machine lock
      model.rs       the `ModelClient` seam, the HTTP client, the transport retry
      machine.rs     the shell seam: spawn, poll, kill a command
      clock.rs       the clock seam: now and sleep, faked in tests
      events.rs      the event seam: how an actor reports to the UI
      session_save.rs  the writer thread behind `.mush/session.json`
      input.rs       the message box's grapheme cursor and horizontal window
      http.rs        a few hundred lines of blocking HTTP/1.1 client
      ids.rs         the two id spaces (`#1` agents, `#c2` jobs), one drawn place
      lock.rs        one mush per workspace: the `flock` that keeps two off one store
      signals.rs     the signals that mean end mush, all taking the clean-quit road
      theme.rs       the per-workspace hue and the form the terminal can paint it in
      ui.rs          the painter: reads a `Screen` a value at a time and paints it
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
Why `ureq` is not the swap (its receive timeout is a total budget rather than a
per-read one, and Ctrl-C depends on polling *between* socket reads):
`crates/mush/src/http.rs`. Each omitted crate is one less thing to version,
audit, and wait for.

---

## 8. Performance

| Metric | Target | Reality |
|---|---|---|
| Cold start | < 20 ms | ~2 ms; model discovery is only fetched when no model is named |
| Model discovery | < 50 ms | one `GET /v1/models` (~20 ms cold, ~4 ms warm), or on `/model`, `/models`, `/url` |
| Keypress → screen | < 5 ms | `update` touches only UI state; draw only when dirty |
| Idle CPU | ~0% | blocked on a 30 ms poll, no repaint unless an agent is running |
| Memory | < 15 MB | the agent tree, the transcripts, and one message box |

Rules: no full-buffer scan per frame, no redraw without a state change, and no
subprocess inside `draw` — the git snapshot is cached in `App`, read on its own
thread, and refreshed on the transitions a human drives (a focus change, a
command, a durable session flush), on agent `Done`/`Error`/`Stop`, and by a
two-second tick while anything is running. A read older than ten seconds is
labelled with its age, because a cached fact must not read as a live one. Release
profile uses `lto = "thin"`, `codegen-units = 1`, `strip = true`.

What bounds a request, and why an unreachable endpoint cannot hang startup: the
phase deadlines and the read slices that let Ctrl-C stop a model that has not
answered live in `crates/mush/src/http.rs` (`CONNECT_TIMEOUT`, `WRITE_TIMEOUT`,
`RESOLVE_TIMEOUT`, `LIST_READ_TIMEOUT`, `CHAT_DEADLINE`). If a server rejects a
request over its context length, mush reads the number out of the complaint,
tells the UI, and retries once — a backstop, not the mechanism.

---

## 9. Roadmap — the plan stops here

**Done.** M0 core (workspace, sessions, messages, prompt, config); M1 editor
`[REMOVED v0.2]`, whose message box is what §4 keeps; M2 the agent loop
(function calling, the shell, cancellation, history trimming, compaction); M2.5
subagents (actor-per-agent with mailboxes, the tree, isolated worktrees,
wake-on-completion, bounded depth and fan-out); M2.6 honest worktrees (an
isolated run commits its worktree); M2.7 the glance layer (ranked rows and
footer, the bar, size tiers, truthful glyphs, typed notices, the cached git
snapshot); M2.75 the seams (`docs/refactor.md`: the four test seams, the `Intent`
keymap, the `Screen` value and the draw sweep); M2.8 concurrent work (jobs and
the one lock, §5.6); M3 external agents (the attach socket, `mush
read/agents/focus/edit`).

**Parked, not planned.** M5 spawn mode and M6 polish are not work that was owed;
they are what a roadmap has left over once the product is what it should be, and
mush stops here deliberately. M5 — `mush` launching a configured agent in a pty
pane with the attach socket's path and the workspace root in its environment, so
"works with any agent" would cover binaries that know nothing about mush — has a
real use case and a different shape: it is for running someone else's agent CLI
(`claude`, `codex`, `aider`) inside mush's panes, and it is the one phase that
would change what mush *is* (§1: a control surface for the built-in agent, not a
terminal multiplexer). What a guest costs is also what it loses: a pty, a
terminal emulator, a second keyboard mode, and a box of bytes with no phases, no
tool calls, no token count and no useful cancel. M6's three items — transcript
search, an optional MCP bridge as a separate binary, per-agent token accounting
— stay the open questions they are in §11.

M4 (FS watching) is `[OBSOLETE v0.2]`: there are no buffers to merge into, and
the periodic git snapshot already tells the human what moved. Each milestone
ends with a demoable, tested artifact and no milestone depends on a later one,
which is why stopping between them costs nothing: what is above is either built
or nothing at all.

**What comes next is not a roadmap.** From here mush changes when the human
asks — a bug they hit, a thing they want, a sentence in this manual that is no
longer true — and never because a plan says so. §11's open questions are
questions the human may answer one day, not a queue; a reader who finds one of
them should ask rather than build.

---

## 10. Testing

- **Unit tests.** Message-box semantics (grapheme edits, the cursor window, wide
  glyphs), path resolution and escaping, capped reads and the command cap, atomic
  writes, session and user-config round-trips, `.mush` self-ignore, history
  trimming and its termination guard, compaction, tool-execution semantics
  (exact-and-unique edits, a batch that lands all-or-nothing), tool-pair repair,
  argument validation, shell-command timeout, cancellation, output cap and
  runaway-writer limit, URL/status-line parsing (IPv6 literals included), the
  model-list timeout, cancelling a chat request mid-wait and the request deadline
  (plus the slow-but-alive body and the dribbling body), an oversized or
  malformed response body, the git snapshot (branch, dirty count, per-branch
  diffstat, ref names that look like flags), the context-window precedence and
  the caps that follow it, row field priority and column-aware truncation, the
  `~` elision boundary, a draw sweep over fifteen terminal sizes × a sweep of
  states that asserts the *painted* text, the attach protocol's ops and one real
  socket exchange, the job registry (detach, the machine lock, the tail window),
  the transport retry and what it must *not* retry, the notices' kinds and
  lifetimes, per-conversation scrollback, the floor refusing every key but
  `Ctrl-Q`, config precedence, schema/prompt invariants, word wrapping, the actor
  mailbox (parked nudges, Stop vs Shutdown, completion delivery), and the
  new-chat, Ctrl-C, stale-event, steering-echo and phase-restore state
  transitions.
- **End-to-end (pty).** `scripts/smoke.py` drives the real binary over a
  pseudo-terminal with the pty as its controlling terminal, so window size and
  SIGWINCH behave as they do in a terminal. Scenarios: agent (needs a model),
  resize (needs nothing), cancel (needs nothing — a socket that accepts the chat
  request and never answers must be abandoned by a single Ctrl-C, which is only
  observable from outside the process), and sigterm (needs nothing).
- **Deterministic orchestration.** More than twenty `cargo test` scenarios drive
  the real actor loop in process, on a scripted `ModelClient` rather than a
  server; six of them spawn a real subagent actor. Between them: a root → child →
  grandchild chain, an isolated child whose run must commit its worktree (the
  test then runs git's own merge, worktree remove and branch delete), a context
  overflow that must compact (and the corners where a fold is refused or parked),
  a nudge that arrives mid-reply and must be answered, a root that ends its turn
  while a child still runs and is woken by its result, a stop acknowledged as a
  stop, and a run that goes past two hundred turns and ends only because the
  model stopped calling tools. The model is scripted; the work — git worktrees,
  files, the commit, the merge — is real, so they need no socket and no
  `python3`, though they do need `git`, and one scenario waits on a real shell
  sleep.
- **Live.** Four `#[ignore]`d tests keep the default suite green offline: two
  talk to the configured endpoint (the model list and the shipped reply cap), one
  makes a TLS handshake against `https://api.deepseek.com` (no key, so a 401 is
  the pass), and one measures a frame against the 16 ms budget on an idle box.
- **The checks.** `cargo fmt --all --check`, `cargo clippy --all-targets --
  -D warnings`, the unit tests, and the pty resize and cancel scenarios are the
  whole gate; they run anywhere rust and python3 do, so any CI can call them.
  `scripts/census.py` prints the production/test/comment split.
- **Screen review.** `scripts/screen.py` drives the real binary over a pty and
  prints the painted screen as text at 200×50 down to 30×8, which is how the ten
  defects of §4.5 were found and how the next layer gets reviewed. Pass `--ask`
  with a reachable endpoint to see the agent's own screens (thinking, cancel,
  done); without it the empty screens need no model at all.
- **Blind reads.** The tree has been read end to end by six blind audits and four
  blind duplication passes — the reader knowing the code and not the findings —
  and their findings are written down in `docs/findings.md` §8.51 and §8.70.

Run it:

```sh
cargo test                 # offline, fast
cargo test -- --ignored    # the live-endpoint checks, plus the frame-budget test
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke --resize
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke --cancel
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke --sigterm
```

---

## 11. Open questions

1. `[OPEN]` Transcript search: `/find` over the focused transcript, or is the
   scrollback enough?
2. `[OPEN]` Token accounting per agent: a rough meter per row would make a
   `MAX_AGENTS` fan-out legible, but the character heuristic is wrong by design.
3. `[OPEN]` Should `run_command` be denied by default and enabled per session?
4. `[OPEN]` Do we ship the MCP bridge ourselves, or leave it to the community?
5. `[OPEN]` Does the human need to *type into* a subagent's pane (today that path
   is a nudge), or is watching enough now that the row and its footer carry the
   brief?
6. `[OPEN]` Where do jobs become visible? A running job now adds `⚙N` to its
   owner's row and the selected row's footer names each one, but whether that is
   enough, or they want their own pane, is open. The human should not have to ask
   a model what is running on their machine.
7. `[OPEN]` A fuzz target for path resolution.
8. `[DECIDED]` Config file format: a machine-global JSON file (`$MUSH_CONFIG`,
   else the platform config directory), hand-editable and self-documenting. Not
   `mush.toml` in `.mush/`, which would make the endpoint a workspace fact, and
   not environment-only, which would make a hand-edited setting impossible.
9. `[DECIDED]` The size tiers: compact at 80×20, the floor at 40×10, the agent
   pane capped at 50 columns and the transcript at 110.
10. `[DECIDED]` Job output is a **tail**, consistently; a *foreground* command
    keeps its head, because the model reads it while the command still runs.
11. `[DECIDED]` One machine-wide `MAX_JOBS`, not a per-agent one; a job does not
    count against `MAX_AGENTS`.

---

## 12. Where the rest of the record went

This file used to carry the design record: a decision log, the audit that shaped
the screen, the arithmetic behind the trimmer's watermark and the job ceiling,
the shape of the prompt, the milestone plan. Those reasons now live where the
code does — in the doc comment beside the item, written as the reason — which is
what the rule at the top of this file moved. The milestones' history is
`git log`; the audits and duplication passes are `docs/findings.md` (§8.51,
§8.70); the seam refactor is `docs/refactor.md`.
