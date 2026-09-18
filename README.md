# mush

A small, fast terminal surface for coding agents. Open a folder, give the root
agent a task, and watch the tree of agents work — with the repository's branch,
dirty count, and line delta always in view.

mush does not edit files itself. The agents do, and mush is how you steer them
and see what changed.

```
┌ agents · 2 working · Σ +12 −3 ───┬ mush ───────────────────────────────┐
│▶◐ #0 ⏸1 root  thinking 4s        │you › rename the lexer module        │
│   ◐ #1 lexer  mush/1 +12−3       │mush › Starting with the rename.     │
│   ✓ #2 docs  wrote README        │      ⚙ edit_file src/lex.rs         │
│                                  ├─────────────────────────────────────┤
│                                  │ › _                                 │
└──────────────────────────────────┴─────────────────────────────────────┘
 chat  Tab cycles panes · /help lists commands · Ctrl-P picks a model
 ⌂ ~/p/demo │ master ±3 +12−3 │ deepseek-flash @ deepseek.com · ctx 12k/~500k
```

## Quick start

```sh
cargo build --release
./target/release/mush /path/to/project    # or just: mush
```

`mush --version` (or `-V`) prints the version and exits.

`Tab` moves between the **agents** tree and the **chat**. Type in the message
box and press `Enter`. The agent works the workspace through six tools —
`edit_file`, `run_command`, `spawn_agent`, `status`, `control`, `wait`: the
shell does the listing, reading and writing (`rg`, `sed -n '1,200p' file`,
`ls -la`, `mkdir -p dir && cat > file <<'EOF'`), `edit_file` replaces exact text
because an exact-and-unique match is a safety property `sed -i` does not have,
and the last four manage the agents and jobs it starts — see *Subagents* below.

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
  config file (never to the workspace).
- `/models` — refresh the model list for the current endpoint.

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
toolset, so a leaf keeps five, and a live-agent budget (`MAX_AGENTS` 16) caps
total fan-out.

The orchestrator may end its turn while children still run: mush shows
`waiting on 1 subagent(s) — the root resumes as they finish`, and the root is
**woken with each child's `#N done: summary`** as they finish — early End is not
a lost result, it's a nap.

Deep chains are tested deterministically (root → child → grandchild, nested
worktrees) but they need a model that actually delegates: small local models
tend to flatten the chain and do the leaf work themselves. Prefer a capable
model for orchestration.

A run ends when the model stops calling tools; a repeated tool batch ends it
early as a *loop*, and the runaway guard ends with a **wrap-up turn** instead of
an error: tools are withdrawn, the model summarizes what was done and what is
left, and that summary is the run's result.

## The agents pane

The pane shows the whole tree: depth by indentation, `·` idle, `◐` running,
`⊘` a cancel in flight or a run that landed stopped, `✓` done (with its final
summary), `✗` failed, `⚠` a run that was cut off, `≡` a conversation being folded.
A running agent that has children out wears `⏸N`, counting them; `✉` marks a
result its parent has not read (`✉N` the ones from its own children); and a
running job adds `⚙N` to its owner's row. A row spends its columns on
state, then the branch and line delta (`mush/2 +8−0`), then the activity with its
age (`edit_file src/lex.rs 12s`), then a short title derived from the brief
(`lexer`); the pane title totals the tree (`agents · 2 working · 1 waiting · Σ +324 −40`), and
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
and only one shared child may run at a time. A `base` git cannot resolve is a
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
endpoint's context window, mush asks the model to summarize everything
important and continues from `system + summary` (`MUSH_CONTEXT` sets the
window; the summary appears in the chat). Nothing is silently dropped —
trimming only cuts in when the model itself cannot produce a summary.

## Keys

| Context | Keys |
|---|---|
| anywhere | `Tab`/`Shift-Tab` cycle panes (agents, chat) · `Ctrl-Q` quit (a second press confirms while work is running) · `Ctrl-N` new chat (stops every agent, restarts the root) · `Ctrl-C` stop the focused agent — an idle one is left alone, and a cancel reaches a model that is still thinking · `Ctrl-X` stop every running agent · `Ctrl-P` model picker |
| agents | `j`/`k`, arrows, `g`/`G`, `Home`/`End` move the rows, `PgUp`/`PgDn` page them · `←`/`→` the row's parent / its first child · `Enter` show its transcript, keys staying in the tree · `c` cancel it · `Esc` back to the root |
| chat | typing · `Enter` send · `Shift`/`Alt-Enter` a new line · `←`/`→`, `Home`/`End` move the box cursor · `Backspace`/`Delete` · `↑`/`↓`, `PgUp`/`PgDn` scroll the transcript · `Esc` clear the box |
| picker | `j`/`k`, arrows, `g`/`G`, `Home`/`End` move, `PgUp`/`PgDn` page the list · `Enter` take the row · `Esc` close |

Chat commands: `/provider`, `/model`, `/url`, `/key`, `/models`, `/compact`,
`/notes`, `/help`, `/quit` (`mush --help` prints this table and the keys).

## The screen

The line above the facts is a model of the workspace, not a log. Its **first
line** is the newest event that has no other home — a failure, a stop, a job's
report, a command's answer — or the one derived fact the rows only imply (a root
that ended its turn with children still working and will resume by itself), else
a fading status, else a hint. It never repeats the activity a row and the
transcript already show. Its **second line** (on terminals at least 24 rows tall)
is the stable facts, cut from the right when the terminal is narrow:

```
⌂ ~/p/mush │ master ±3 +12−3 │ deepseek-flash @ deepseek.com · ctx 12k/~500k
```

`±3` counts paths with uncommitted changes, `+12−3` the line delta against
`HEAD`, and `ctx 12k/~500k` how much of the window this conversation has taken —
the `~` says the window was assumed rather than stated.

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

A failure of the transport — a connection reset or refused, an unexpected end
of stream, a connect or read timeout — is the wire, not the endpoint refusing
the request, so mush asks again: **three attempts in total**, with a short
backoff between them. Each retry is a line in the agent's own transcript
(`· Connection reset by peer (os error 104) — retrying (2/3)`) instead of a
spinner that looks stuck, and Ctrl-C abandons the request at once, backoff
included. What the endpoint *answered* — a 4xx or 5xx status, a reply past the
body cap, a body that did not parse — is returned as it is, first time: an
answer is not a hiccup. Every attempt is bounded by the client's own budget
(5 s to connect, 30 s to write, a 10-minute read deadline), so three attempts
plus the backoff is the worst case: seconds for the hiccup this is for, about
half an hour for an endpoint that stalls and loses every time.

## Context window

Every request fits inside the endpoint's window, and the window comes from the
first of these that knows:

1. **You**: `--context N`, `MUSH_CONTEXT=N`, or a `context` in the home config. A
   number you state is remembered in `.mush/session.json` and never overruled.
2. **The endpoint**, when it advertises one and mush fetched its model list: the
   first of `max_model_len`, `context_length`, `context_window`, `n_ctx` it
   reports, at the top level or under `meta`. Discovery runs when no model was
   named, and on `/url`, `/provider`, `/models` (a model picker fetches only if
   its list is empty).
3. **The model's documented window** — `deepseek-flash` and `deepseek-v4-pro`
   are 500k, so a hosted API (which answers with ids and nothing else) is not
   silently treated as an 8k local model.
4. **The provider default**: 120k for DeepSeek, 8192 for a custom endpoint.

One reply is capped at a quarter of that window — floored at 1 024 tokens and
capped at 120 000 — so a thinking model has room to answer without the request
overshooting the window it is sent to. The cap is what mush sends as `max_tokens`
(or `max_completion_tokens`, see `/help`), and `mush --print-config` prints the
number it resolved to.

The command cap (`CMD_CAP`, scaled to the window by `Config::cmd_cap`) follows
the window, so one command's output can never fill an 8k transcript. If a server
rejects a request over its context length, mush reads the number out of the
complaint, tells the UI, and retries once.

## What it writes

- `./.mush/` — workspace-local state, git-ignored by itself: `session.json`
  (the conversation and the whole agent tree, the provider, endpoint and model,
  a context window you stated, and each agent's last failure), and `wt/` for
  isolated agents' worktrees.
- The platform config directory (e.g. `~/.config/mush/config.json`) —
  machine-global defaults **including the API key**. The key never touches the
  workspace.

## Design

See [docs/mush.md](docs/mush.md) for the design: the single-owner event loop,
the agent actor tree, the safety rules, and the roadmap.

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
