# mush

A small, fast terminal surface for coding agents. Open a folder, give the root
agent a task, and watch the tree of agents work — with the repository's branch,
dirty count, and line delta always in view.

mush does not edit files itself. The agents do, and mush is how you steer them
and see what changed.

<!-- generated: frame (blessed by MUSH_BLESS_DOCS=1 cargo test -p mush --bin mush ui::tests::the_readme_frame_matches_the_code) -->
```
┌ agents · 2 working · Σ +12 −3──┐┌ mush ──────────────────────────────────────────────────────────┐
│▶◐ #0 root  thinking 4s         ││you › rename the lexer module                                   │
│   ◐ #1 lexer  mush/1 +12−3     ││                                                                │
│   ✓ #2 docs  wrote README.md   ││mush › Starting with the rename.                                │
│                                ││                                                                │
│                                ││± src/lex.rs                               → 3 hunks            │
│                                │││ edited src/lex.rs — 3 edits                                   │
│                                ││                                                                │
│                                ││· #2 done: wrote README.md                                      │
│                                ││                                                                │
│                                ││mush › The tests are next.                                      │
│                                ││· waiting on #1                                                 │
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
│────────────────────────────────│└────────────────────────────────────────────────────────────────┘
│ #0 rename the lexer module     │┌ message ───────────────────────────────────────────────────────┐
│ thinking 4s · .mush/wt/1 · gi… ││›                                                               │
└────────────────────────────────┘└────────────────────────────────────────────────────────────────┘
 chat  Tab cycles panes · /help lists commands · Ctrl-P picks a model
 ⌂ ~/p/demo │ master ±3 +12−3 │ deepseek-flash @ deepseek.com · ctx 12k/430.5k ~500k
```
<!-- /generated: frame -->

The rest of this file is the user-facing contract. The manual
([docs/mush.md](docs/mush.md)) adds the map into the code — the spec itself is
the doc comments beside it. Every block between `<!-- generated: … -->` markers
is generated from the code and checked by a test named in its own head, so such
a block cannot drift silently; the tables written by hand outside those blocks
are prose, and a number in one is a claim to check rather than a fact a test
keeps true.

## Quick start

```sh
cargo build --release
./target/release/mush /path/to/project    # or just: mush
```

`mush --version` (or `-V`) prints the version and exits; `mush --help` prints
the keymap and the commands.

`Tab` moves between the **agents** tree and the **chat**, and `Ctrl-F` hands the
focused one the whole screen — `Tab` switches which that is. Type in the message
box and press `Enter`. The agent works the workspace through twelve tools: seven
touch the files (`edit_file` replaces exact text, because an exact-and-unique
match is a safety property `sed -i` does not have; `read_file` reads a line
window and works even while another agent holds the machine; `outline` sketches
a file's declarations without spending the window on its text; `search` finds a
literal string; `usages` answers who mentions a symbol as a word, grouped by
file), `run_command` is the shell for everything else (git, tests, builds),
`spawn_agent` delegates, and `status`, `control` and `wait` manage the agents and
jobs it starts. The schemas are `crates/mush-core/src/prompt.rs`; the manual's §3
lists them.

It talks to any OpenAI-compatible endpoint with function calling:

```sh
mush --url http://localhost:11434 --model qwen2.5-coder
mush --provider deepseek           # hosted DeepSeek API (deepseek-flash, deepseek-v4-pro)
MUSH_URL=http://host:8080 MUSH_MODEL=my-model mush
MUSH_PROVIDER=deepseek MUSH_MODEL=deepseek-flash MUSH_API_KEY=sk-… mush
```

The default endpoint is `http://rubendpc:8078`; if no model is given, mush
reads `/v1/models` and picks the first.

## Providers, endpoints, API keys

Everything can be changed at runtime from the chat — no restart:

- `/provider` — pick **deepseek** (hosted API, https) or **custom** (any
  OpenAI-compatible endpoint) from a menu.
- `/model` — pick a model (or press `Ctrl-P`). The list comes from the
  endpoint's `/v1/models`; when that's unreachable, the known models for the
  provider are offered (`deepseek-flash`, `deepseek-v4-pro`).
- `/url http://host:port` — point at any endpoint. `https://` works too (TLS
  via rustls).
- `/key <secret>` — set the API key. Shown masked, and saved to the home
  config file (never to the workspace). A key read from `MUSH_API_KEY` stays in
  the environment — mush never copies it into that file.
- `/models` — refresh the model list for the current endpoint.
- `/context` — say the window and the road it came by; `/context N` states one
  for this workspace (remembered in `.mush/session.json`), and `/context auto`
  drops the statement so the window derives again.

**Resolution order on startup: CLI flags > env vars (`MUSH_*`) > saved session
> home config > built-in defaults.** The home config lives at `$MUSH_CONFIG`,
else the platform config directory (`~/.config/mush/config.json` on Linux); with
`HOME` unset and no `MUSH_CONFIG` there is none, and a save refuses by name — it
is *machine-global*:

```json
{
  "api_key": "sk-...",
  "provider": "deepseek",
  "base_url": "https://api.deepseek.com",
  "model": "deepseek-flash"
}
```

Every knob, its default, and what `--print-config` prints: the manual's §5.

## Subagents

The root agent can delegate: `spawn_agent(brief, title?, base?)` starts a
subagent that has no memory of your conversation — the brief *is* the context.
`title` names its row in the tree and is optional; `base`, a branch, tag or
commit, is what gives the child a tree of its own (*Isolated agents* below).
`status`, `control` and `wait` manage the children and jobs that follow, and what
each call takes and hands back is stated once: in the tool's own schema, and as
the contract in the manual's §3. Subagents can spawn their own, to a bounded
depth and with a live-agent budget on total fan-out (`MAX_DEPTH`, `MAX_AGENTS`,
`crates/mush/src/agent.rs`); `spawn_agent` vanishes from a leaf's toolset, so a
leaf keeps nine.

The root is an **orchestrator**: its job is the overview and the person in front
of it — deciding what happens next, briefing the children, and reading what they
hand back. The work itself (the edits, the tests, the chasing) belongs to
subagents, in a few large briefs rather than many small ones, and the system
prompt says so: a change the root makes with its own hands lands in your
checkout with no brief, no branch and no second reader.

The orchestrator may end its turn while children still run: mush shows
`waiting on 1 subagent(s) — the root resumes as they finish`, and the root is
**woken with each child's `#N done: summary`** as they finish — early End is not
a lost result, it's a nap. Deep chains are tested deterministically (root →
child → grandchild, nested worktrees) but they need a model that actually
delegates: small local models tend to flatten the chain and do the leaf work
themselves. Prefer a capable model for orchestration.

A run ends when the model stops calling tools; a repeated tool batch ends it
early as a *loop*. Nothing counts turns, so a run that keeps making *different*
calls goes until you stop it.

## The agents pane

The pane shows the whole tree, depth by indentation, and every mark a row can
wear — painted by the code's own row painter:

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

`✉`/`✉N` are unread results (the parent's, and an agent's own children's), `⚮`
is a row whose parent the history window has reaped (it is drawn under its
nearest surviving ancestor — the root when none of its own survive — at that
ancestor's depth plus one, dim, and the mark is what says its own parent is not
the row it sits under), and `⚙N` counts the jobs on their owner's row. A running
agent with children out wears **no** count of them: the children's own rows say
they run, and the pane title's `N waiting` counts the agents at rest with work
out. A row spends its columns on
state, then the branch and line delta (`mush/2 +8−0`), then the activity with its
age (`edit_file src/lex.rs 12s`), then a short title derived from the brief
(`lexer`); the pane title totals the tree (`agents · 1 working · 1 waiting · Σ
+324 −40`), and the selected row's full facts — the brief, its activity, the
worktree and the git command that reads it, its jobs — sit in a footer under the
list.

`Enter` on a row shows that agent's transcript in the chat pane; the keyboard
stays in the tree, so `Tab` is what puts it in the message box. `←`/`→` put the
selection on that agent's parent or its first child, and `PgUp`/`PgDn` page the
rows. `Esc` returns to the root, `c` cancels the selected agent, `Ctrl-C` stops
the **focused** agent, and `Ctrl-X` stops every running one (an idle agent is
left alone). A cancel reaches the model call itself: the request is read in
short slices, so Ctrl-C stops a model that has not answered instead of waiting
for its reply.

## Isolated agents

`base` gives a child its own git worktree (`.mush/wt/<id>` on branch
`mush/<id>`), forked from that branch, tag or commit — so parallel agents edit
real files without colliding. Without a `base` the child shares the checkout,
and a shared spawn is refused while another shared child is live in that
checkout — the count is the directory's, across the whole tree, not one parent's
books. A `base` git cannot resolve is a **failed delegation**, refused before
anything is created, never a child that quietly runs somewhere else. A run's
work is **committed** to the child's branch when the run ends (`mush #3:
<brief>`, with the outcome spelled into the subject when it stopped, was cut off
or failed), so the branch really carries it.

**mush never auto-merges** — the selected row's footer names the worktree and
`git diff HEAD...mush/3`, and git itself lands or drops the work:

```sh
git merge mush/3                 # land it on the branch you are on
git worktree remove .mush/wt/3   # reclaim the checkout
git branch -D mush/3             # drop the branch
```

Leftover worktrees (a `mush/*` branch with a checkout still on disk) are
rediscovered on startup and shown in the tree, so those commands keep working
after a restart.

Long running conversations are **auto-compacted**: when the history nears the
budget, mush asks the model to summarize everything important and continues from
`system + summary` (the summary appears in the chat). A fold the window cannot
hold is not attempted: it says so instead of paying for an endpoint's refusal.
When the trim cannot make enough room, the newest turn's tool results are
dropped in place — each one says the call is not lost and the same output is one
narrower call away — and a request that still does not fit is refused before the
wire. Nothing goes out over the window. The budget, the fold and the numbers
behind `--print-config`: the manual's §3.

## Keys and commands

`mush --help` prints exactly these, and a test fails while either block is
stale:

<!-- generated: keys (blessed by MUSH_BLESS_DOCS=1 cargo test -p mush --bin mush app::keys::tests::the_keys_block_matches_the_code) -->
```
  anywhere:
    Ctrl-Q               quit (a second press confirms while work is running)
    Ctrl-C               stop the focused agent
    Ctrl-X               stop every running agent
    Ctrl-N               start a new chat (a second press stops every agent, drops every transcript)
    Ctrl-P               model picker
    Ctrl-T               show or hide the model's reasoning
    Ctrl-O               the compact log: one line per tool call (a failure always shows)
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
    Shift-↑ / Shift-↓    move the box cursor a row
    Alt-↑ / Alt-↓        the same move, where the terminal reports Alt
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
    /glyphs [ascii|symbols]      show the mark each tool wears, or paint them in ascii
    /help                        list the keys and these commands
    /quit                        leave mush
```
<!-- /generated: commands -->

## The screen

The line above the facts is a model of the workspace, not a log. Its **first
line** is the newest event that has no other home — a failure, a stop, a job's
report, a command's answer — or the one derived fact the rows only imply (a root
that ended its turn with children still working and will resume by itself), else
a fading status, else a hint. It never repeats the activity a row and the
transcript already show. The **second line** (on terminals at least 24 rows
tall) is the stable facts: the workspace, the branch and its delta, the model,
and the context meter.

```
⌂ ~/p/mush │ master ±3 +12−3 │ deepseek-flash @ deepseek.com · ctx 12k/430.5k (fold 387.4k) ~500k
```

Terminals narrower than 80 columns (or shorter than 20 rows) get a **compact**
layout: the agent strip on top, chat below. Below 40×10 mush says so instead of
painting shreds. Under the conversation is mush's own **foot** (`·` for what
happened, `!` for a failure, `⊘` for a run mush stopped, `⚠` for one that was
cut off): never more than three rows, with `+N more lines · /notes` when there
is more, and `/notes` reads the whole list. A failure is kept in
`.mush/session.json` and is still there next time mush opens the workspace —
until that agent runs again. The meter's marks and the size tiers: the manual's
§4.5.

## When the network hiccups

A failure that happened *before the request was handed over* — a dial that
never connected (refused, timed out, a name that did not resolve, a TLS
handshake that failed), or a write that did not hand the whole request to the
endpoint — is the wire, not the endpoint refusing the request, and no whole
request reached it, so mush asks again: **three attempts in total**, with a
short backoff between them. Each retry is a line in the agent's own transcript
(`· Connection refused (os error 111) — retrying (2/3)`) instead of a spinner
that looks stuck, and Ctrl-C abandons the request at once, backoff included.
Everything after the write is final, first time, because the endpoint may
already have read, run and charged for the request: a connection reset, an
unexpected end of stream, a read timeout, a 4xx or 5xx status, a reply past the
body cap, a body that did not parse. One ask spends one deadline (ten minutes),
and every attempt — and the backoff between them — gets only what is left of it.
The phase ceilings and the read slices that make Ctrl-C work:
`crates/mush/src/http.rs`; the call's whole deadline and the retry rule's own
code: `crates/mush/src/model.rs`.

## Context window

Every request fits inside the endpoint's window, and the window comes from the
first of these that knows:

1. **You**: `--context N`, `MUSH_CONTEXT=N`, a `context` in the home config, or
   `/context N` in the chat. A number you state is remembered in
   `.mush/session.json` and never overruled; `/context auto` drops the statement
   and lets the window derive again.
2. **The endpoint**, when it advertises one and mush fetched its model list: the
   first of `max_model_len`, `context_length`, `context_window`, `n_ctx` it
   reports, at the top level or under `meta`. Discovery runs when no model was
   named, and on `/url`, `/provider`, `/models`.
3. **The model's documented window** — `deepseek-flash` and `deepseek-v4-pro`
   are 500k, so a hosted API (which answers with ids and nothing else) is not
   silently treated as an 8k local model.
4. **The provider default**: 120k for DeepSeek, 8192 for a custom endpoint.

A reply is capped at a share of that window, and `mush --print-config` prints
the number it resolved to — beside the tool schemas every request reserves and
the history budget those leave. The cap is what mush sends as `max_tokens` (or
`max_completion_tokens`). The command cap follows the window, so one command's
output can never fill an 8k transcript; a request that does not fit is refused
by mush itself, one line before the wire, naming the roads that make room
(downscale a picture, `/compact`, read less). If a server still rejects a
request over its context length, mush reads the number out of the complaint,
tells the UI, and retries once — a backstop, not the mechanism.

## What it writes

- `./.mush/` — workspace-local state, git-ignored by itself: `lock` (the
  workspace lock, held while mush runs; replacing it lets a second mush write
  over this conversation), `session.json` (the conversation and the whole agent
  tree, the provider, endpoint and model, a context window you stated, and
  each agent's last failure), an unreadable session set aside as
  `session.json.bak` (then `.bak.2`, …), `session.json.previous` (the
  conversation the last new chat kept), `wt/` for isolated agents' worktrees,
  and `paste/` for pictures pasted into the chat.
- The platform config directory (e.g. `~/.config/mush/config.json`) —
  machine-global defaults **including the API key**. The key never touches the
  workspace.

## Design and layout

[docs/mush.md](docs/mush.md) is the manual: the user-facing contract and a map
into the code. The tree has also been read cold, with no comment taken as true:
six blind audits and four duplication passes, whose detail lives in
[docs/findings.md](docs/findings.md) — the six audits in §8.51, the four passes
in §8.70.

```
crates/mush-core/   pure domain: workspace, sessions, prompt, messages, config, tools
crates/mush/        the binary: TUI, agent actors, HTTP client
scripts/smoke.py    end-to-end test that drives the real TUI over a pty
scripts/screen.py   prints the painted screen as text at six terminal sizes
scripts/loc_history.py draws the census's four LOC series by day, out of git history
scripts/mock_llm.py scripted model server, kept for hand-driven runs (nothing in the repo calls it)
docs/mush.md        the manual
```

## Tests

```sh
cargo test                    # offline unit tests; the agent-tree scenarios
                              # run in process on a scripted model client
cargo test -- --ignored       # the live-endpoint checks (the model list, the
                              # shipped reply cap, a TLS handshake) plus the
                              # frame-budget test, which needs an idle box
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke           # needs a model
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke --resize  # needs none
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke --cancel  # needs none
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke --sigterm # needs none
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke --lock    # needs none
```

The endpoint-free scenarios drive the real binary over a pty: `--resize` checks
that it repaints on its own, `--cancel` points it at a socket that accepts the
request and never answers and checks that a single `Ctrl-C` frees the agent to
work again, `--sigterm` checks the clean-quit road a signal takes, and `--lock`
checks that a second mush on one workspace is refused while the first works. The
manual's §10 lists what the suite covers.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT — see [LICENSE](LICENSE).
