# mush

A small, fast terminal surface for coding agents. Open a folder, give the root
agent a task, and watch the tree of agents work — with the repository's branch,
dirty count, and line delta always in view.

mush does not edit files itself. The agents do, and mush is how you steer them
and see what changed.

<!-- generated: frame (blessed by MUSH_BLESS_DOCS=1 cargo test -p mush --bin mush ui::tests::the_readme_frame_matches_the_code) -->
<!-- /generated: frame -->

## Quick start

```sh
cargo build --release
./target/release/mush /path/to/project    # or just: mush
```

`mush --version` (or `-V`) prints the version and exits.

`Tab` moves between the **agents** tree and the **chat**, and `Ctrl-F` hands
the focused one the whole screen — `Tab` switches which that is. Type in the
message box and press `Enter`. The agent works the workspace through ten tools —
`edit_file`, `read_file`, `write_file`, `list_files`, `search`, `run_command`,
`spawn_agent`, `status`, `control`, `wait`: five touch files (`edit_file`
replaces exact text, because an exact-and-unique match is a safety property
`sed -i` does not have; `read_file` reads a line window and works even while
another agent holds the machine), `run_command` is the shell for everything
else (git, tests, builds), and the last four manage the agents and jobs it
starts — see *Subagents* below.

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

Resolution order on startup: **CLI flags > env vars (`MUSH_*`) > saved session
> home config > built-in defaults**. `MUSH_CONTEXT` sets the endpoint's
context window in tokens (the built-in default is 120000 for DeepSeek and 8192
for a custom endpoint); history is trimmed to fit it, so requests never
overflow small local models. The home config file lives at
`$MUSH_CONFIG`, else the platform config directory (`~/.config/mush/config.json`
on Linux) — it is *machine-global*:

```json
{
  "api_key": "sk-...",
  "provider": "deepseek",
  "base_url": "https://api.deepseek.com",
  "model": "deepseek-flash"
}
```

The provider, endpoint, and model you last chose in a workspace are also
remembered in `.mush/session.json`.

## Subagents

The root agent can delegate: `spawn_agent(brief, title, base?)` starts a
subagent that has no memory of your conversation — the brief *is* the context.
`title` (three words) names its row in the tree; `base`, a branch, tag or
commit, is what gives the child a tree of its own (*Isolated agents* below).
`status`, `control` and `wait` are what manage the children and jobs that
follow, and what each call takes and hands back is stated once: in the tool's
own schema, and as the design record in the *agent contract* of
[docs/mush.md](docs/mush.md). Subagents can spawn their own, four levels deep
(`MAX_DEPTH` 3, the root included); `spawn_agent` vanishes from a leaf's
toolset, so a leaf keeps nine, and a live-agent budget (`MAX_AGENTS` 16) caps
total fan-out.

The root is an **orchestrator**: its job is the overview and the person in front
of it — deciding what happens next, briefing the children, and reading what
they hand back. The work itself (the edits, the tests, the chasing) belongs to
subagents, in a few large briefs rather than many small ones, and the system
prompt says so: a change the root makes with its own hands lands in your
checkout with no brief, no branch and no second reader.

The orchestrator may end its turn while children still run: mush shows
`waiting on 1 subagent(s) — the root resumes as they finish`, and the root is
**woken with each child's `#N done: summary`** as they finish — early End is not
a lost result, it's a nap.

Deep chains are tested deterministically (root → child → grandchild, nested
worktrees) but they need a model that actually delegates: small local models
tend to flatten the chain and do the leaf work themselves. Prefer a capable
model for orchestration.

A run ends when the model stops calling tools; a repeated tool batch ends it
early as a *loop*. Nothing counts turns, so a run that keeps making *different*
calls goes until you stop it.

## The agents pane

The pane shows the whole tree: depth by indentation, `·` idle, `◐` running,
`⊘` a cancel in flight or a run that landed stopped, `✓` done (with its final
summary), `✗` failed, `⚠` a run that was cut off, `≡` a conversation being folded,
`⧗` a run parked on somebody else's result (the `wait` tool — the icon says what
the row's words say, `waiting on results 3s`).
A running agent that has children out wears `⏸N`, counting them; `✉` marks a
result its parent has not read (`✉N` the ones from its own children); and a
running job adds `⚙N` to its owner's row. `⚮` after an id marks a row whose
parent the history window has reaped: it is drawn at the top level like a root
child, and the mark is what says it is not one. A row spends its columns on
state, then the branch and line delta (`mush/2 +8−0`), then the activity with its
age (`edit_file src/lex.rs 12s`), then a short title derived from the brief
(`lexer`); the pane title totals the tree (`agents · 1 working · 1 waiting · Σ +324 −40`), and
the selected row's full facts — the brief, its activity, the worktree and the git
command that reads it, its jobs — sit in a footer under the list.

`Enter` on a row shows that agent's transcript in the chat pane; the keyboard
stays in the tree, so `Tab` is what puts it in the message box, where typing
reaches the agent on screen. `←`/`→` put the selection on that agent's parent or
its first child, and `PgUp`/`PgDn` page the rows. `Esc` returns to the root, `c`
cancels the selected agent, `Ctrl-C` stops the **focused** agent, and `Ctrl-X`
stops every running one (an idle agent is left alone — it has nothing to
cancel). A cancel reaches the model call itself: the request is read in short
slices, so Ctrl-C stops a model that has not answered instead of waiting for its
reply.

## Isolated agents

`base` gives a child its own git worktree (`.mush/wt/<id>` on branch
`mush/<id>`), forked from that branch, tag or commit — so parallel agents edit
real files without colliding. Without a `base` the child shares the checkout,
and a shared spawn is refused while another shared child is live in that
checkout — the count is the directory's, across the whole tree, not one
parent's books (the spawner is not counted, so a shared child may still
delegate into the tree its own run is in). A `base` git cannot resolve is a
**failed delegation**, refused before anything is created, never a child that
quietly runs somewhere else. A run's work is **committed** to the child's branch
when the run ends (`mush #3: <brief>`, with the outcome spelled into the subject
when it stopped, was cut off or failed), so the branch really carries it.

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
budget it is sent under — nine tenths of the window once the schemas, the reply
and a margin are reserved — mush asks the model to summarize everything
important and continues from `system + summary` (`MUSH_CONTEXT` sets the
window; the summary appears in the chat). A fold the window cannot hold is not
attempted: it says so, once per state, instead of paying for an endpoint's
refusal. When the trim cannot make enough room, the newest turn's tool results
are dropped in place — each one says the call is not lost and the same output is
one narrower call away, and you get one notice — and a request that still does
not fit is refused before the wire. Nothing goes out over the window.

## Keys

The keys are one table in `crates/mush/src/app/keys.rs` and the commands one in
`crates/mush/src/app/commands.rs`; `mush --help` prints exactly these, and a test
fails while either block is stale:

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

## The screen

The line above the facts is a model of the workspace, not a log. Its **first
line** is the newest event that has no other home — a failure, a stop, a job's
report, a command's answer — or the one derived fact the rows only imply (a root
that ended its turn with children still working and will resume by itself), else
a fading status, else a hint. It never repeats the activity a row and the
transcript already show. Its **second line** (on terminals at least 24 rows tall)
is the stable facts, cut from the right when the terminal is narrow:

```
⌂ ~/p/mush │ master ±3 +12−3 │ deepseek-flash @ deepseek.com · ctx 12k/430.5k (fold 387.4k) ~500k
```

`±3` counts paths with uncommitted changes, `+12−3` the line delta against
`HEAD`, and the meter is the run's own numbers:
`ctx 12k/430.5k (fold 387.4k) ~500k` weighs the conversation against the history
budget the run trims and folds at, where the fold's trigger sits inside it, and
the window itself: the one-column mark names the road the number came by — `~`
assumed from mush's model table, `≈` advertised by the endpoint's model list,
`≤` named by the endpoint in a refusal, and no mark when you stated it yourself
(`--print-config` and `/context` name the road in words). `full` marks the
budget and `over` one byte past it.

Terminals narrower than 80 columns (or shorter than 20 rows) get a
**compact** layout: the agent strip on top, chat below. Below 40×10 mush says
so instead of painting shreds.

Under the conversation is mush's own **foot**: `·` for what happened, `!` for a
failure, `⊘` for a run mush stopped, `⚠` for one that was cut off. It never takes
more than three rows — two for the lines and one for the
count; when there is more, the row that says `+N more lines · /notes` is the
count, and `/notes` reads the whole list. A fatal
run's failure is kept in `.mush/session.json` and is still there next time mush
opens the workspace — until that agent runs again, when the failure belonged to
the run being replaced.

## When the network hiccups

A failure that happened *before the request was handed over* — a dial that
never connected (refused, timed out, a name that did not resolve, a TLS
handshake that failed), or a write that did not hand the whole request to the
endpoint — is the wire, not the endpoint refusing the request, and no whole
request reached it, so mush asks again: **three attempts in total** (the first
try and two retries), with a short backoff between them. Each retry is a line in
the agent's own transcript
(`· Connection refused (os error 111) — retrying (2/3)`) instead of a spinner
that looks stuck, and Ctrl-C abandons the request at once, backoff included.
Everything after the write is final, first time, because the endpoint may
already have read, run and charged for the request: a connection reset, an
unexpected end of stream, a read timeout, a 4xx or 5xx status, a reply past the
body cap, a body that did not parse. An answer is not a hiccup (the one
learned-window retry of a context-length 400 is *Context window*'s backstop,
not this section's). One ask spends one budget of ten minutes (600 s): the whole
deadline is fixed once, and every attempt — and the backoff between them — gets
only what is left of it, so a retry the call cannot afford is not made. Each
phase of an attempt takes the smaller of its own ceiling — 5 s to connect to
one address, 30 s to write one chunk, 10 s to resolve a name — and what is left
of the call, so the worst case is that one ask's ten minutes, not a multiple of
it: about a second and a half for a dial that is refused every time, ten minutes
for an endpoint that accepts the connection and then stalls.

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
   named, and on `/url`, `/provider`, `/models` (a model picker fetches only if
   its list is empty).
3. **The model's documented window** — `deepseek-flash` and `deepseek-v4-pro`
   are 500k, so a hosted API (which answers with ids and nothing else) is not
   silently treated as an 8k local model.
4. **The provider default**: 120k for DeepSeek, 8192 for a custom endpoint.

One reply is capped at an eighth of that window — floored at 1 024 tokens and
capped at 120 000 — so a thinking model has room to answer without the request
overshooting the window it is sent to. The cap is what mush sends as `max_tokens`
(or `max_completion_tokens`, see `/help`), and `mush --print-config` prints the
number it resolved to — beside the tool schemas every request reserves and the
history budget those leave.

The command cap (`CMD_CAP`, scaled to the window by `Config::cmd_cap` to the
fifth a trim leaves, floored at 512 bytes) follows the window, so one command's
output can never fill an 8k transcript — nor land the next request over the
window. A request that does not fit is refused by mush itself, one line before
the wire, naming the roads that make room (downscale a picture, `/compact`, read
less). If a server still rejects a request over its context length, mush reads
the number out of the complaint, tells the UI, and retries once — a backstop,
not the mechanism.

## What it writes

- `./.mush/` — workspace-local state, git-ignored by itself: `session.json`
  (the conversation and the whole agent tree, the provider, endpoint and model,
  a context window you stated, and each agent's last failure),
  `session.json.previous` (the conversation the last new chat kept), and `wt/`
  for isolated agents' worktrees.
- The platform config directory (e.g. `~/.config/mush/config.json`) —
  machine-global defaults **including the API key**. The key never touches the
  workspace.

## Design

See [docs/mush.md](docs/mush.md) for the design: the single-owner event loop,
the agent actor tree, the safety rules, and the roadmap.

The tree has also been read cold, with no comment taken as true: six blind
audits and four duplication passes, whose detail lives in
[docs/findings.md](docs/findings.md) — the six audits in §8.51, the four passes
in §8.70.

## Layout

```
crates/mush-core/   pure domain: workspace, sessions, prompt, messages, config, tools
crates/mush/        the binary: TUI, agent actors, HTTP client
scripts/smoke.py    end-to-end test that drives the real TUI over a pty
scripts/screen.py   prints the painted screen as text at six terminal sizes
scripts/mock_llm.py scripted model server, kept for hand-driven runs (nothing in the repo calls it)
docs/mush.md        the design doc
```

## Tests

```sh
cargo test                    # offline unit tests; the agent-tree scenarios
                              # run in process on a scripted model client
cargo test -- --ignored       # the three live-endpoint checks (the model list,
                              # the shipped reply cap, a TLS handshake) plus the
                              # frame-budget test, which needs an idle box
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke           # needs a model
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke --resize  # needs none
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke --cancel  # needs none
```

The last two drive the real binary over a pty without a model: one resizes it and
verifies it repaints on its own, the other points it at a socket that accepts the
request and never answers, and checks that a single `Ctrl-C` frees the agent to
work again instead of waiting for a reply that never comes.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT — see [LICENSE](LICENSE).
