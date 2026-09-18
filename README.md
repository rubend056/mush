# mush

A small, fast terminal surface for coding agents. Open a folder, give the root
agent a task, and watch the tree of agents work — with the repository's branch,
dirty count, and line delta always in view.

mush does not edit files itself. The agents do, and mush is how you steer them
and see what changed.

```
┌ agents · 1 running · Σ +12 −3 ───┬ mush ──────────────────────────────┐
│ ▶ · #0   you (root agent)        │ you › rename the lexer module       │
│   ◐ #1   lexer    edit lex.rs 4s │ mush › Starting with the rename.    │
│   ✓ #2   docs     wrote README   │       ⚙ edit_file src/lex.rs        │
│                                  ├─────────────────────────────────────┤
│                                  │ › _                                 │
└──────────────────────────────────┴─────────────────────────────────────┘
 chat  #1 edit lex.rs 4s
 ⌂ ~/p/demo │ master ±3 +12−3 │ deepseek-flash · ctx ~500k
```

## Quick start

```sh
cargo build --release
./target/release/mush /path/to/project    # or just: mush
```

`Tab` moves between the **agents** tree and the **chat**. Type in the message
box and press `Enter`. The agent reads and edits the workspace through five file
tools, four delegation tools, and three job tools (`command_status`,
`command_control`, `wait_commands`) — see *Subagents* below.

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

The root agent can delegate: `spawn_agent(brief)` starts a subagent that has
no memory of your conversation — the brief *is* the context. `wait_agents`
blocks until a child finishes, `agent_status` lists them, `agent_control`
stops or messages (nudges) a running child. Subagents can spawn their own, up
to `MAX_DEPTH` (3); the delegation tools vanish from a leaf's toolset, and a
live-agent budget (16) caps total fan-out.

The orchestrator may end its turn while children still run: mush shows
`waiting on N subagents`, and the root is **woken with each child's
`#N done: summary`** as they finish — early End is not a lost result, it's a
nap.

Deep chains are tested deterministically (root → child → grandchild, nested
worktrees) but they need a model that actually delegates: small local models
tend to flatten the chain and do the leaf work themselves. Prefer a capable
model for orchestration.

A run ends when the model stops calling tools; a *loop* — the same tool batch
five rounds over with nothing changed in between — ends it early, and a 200-turn
runaway guard gets a **wrap-up turn** instead of an error: tools are withdrawn,
the model summarizes what was done and what is left, and that summary is the
run's result.

## The agents pane

The pane shows the whole tree: depth by indentation, `·` idle, `◐` running,
`⊘` a cancel in flight or a run that landed stopped, `✓` done (with its final
summary), `✗` failed. A running agent that has children out wears `⏸N`, counting
them, and a running job adds `⚙N` to its owner's row. A row spends its columns on
state, then the branch and line delta (`mush/2 +8−0`), then the activity with its
age (`edit_file src/lex.rs 12s`), then a short title derived from the brief
(`lexer`); the pane title totals the tree (`agents · 2 running · Σ +324 −40`), and
the selected row's full facts — the brief, the worktree, the merge commands, its
jobs — sit in a footer under the list.

`Enter` on a row focuses that agent — the chat switches to its transcript and
typing nudges it. `←`/`→` put the selection on that agent's parent or its first
child, and `PgUp`/`PgDn` page the rows. `Esc` returns to the root, `c` cancels the
selected agent, `Ctrl-C` stops the **focused** agent, and `Ctrl-X` stops every
running one (an idle agent is left alone — it has nothing to cancel). A cancel
reaches the model call itself: the request is read in short slices, so Ctrl-C
stops a model that has not answered instead of waiting for its reply.

## Isolated agents

`isolated: true` gives a child its own git worktree
(`.mush/wt/<id>` on branch `mush/<id>`), so parallel agents edit real files
without colliding. A run's work is **committed** to that branch when the run
ends (`mush #3: <brief>`), so the branch really carries it. **mush never
auto-merges** — the tree shows the branch, and these run the git that reads and
lands it:

```
/diff <id>      runs `git diff HEAD...mush/3`: a stat line, then the hunks (capped)
/merge <id>     runs `git merge mush/3`, then reclaims the worktree and the branch
/discard <id>   runs `git worktree remove --force .mush/wt/3 && git branch -D mush/3`
```

Leftover worktrees (`mush/*` branches) are rediscovered on startup and shown
in the tree, so those commands keep working after a restart; `/worktrees`
re-scans.

Long running conversations are **auto-compacted**: when the history nears the
endpoint's context window, mush asks the model to summarize everything
important and continues from `system + summary` (`MUSH_CONTEXT` sets the
window; the summary appears in the chat). Nothing is silently dropped —
trimming only cuts in when the model itself cannot produce a summary.

## Keys

| Key | Action |
|---|---|
| `Tab` / `Shift-Tab` | cycle panes (agents, chat) |
| `Enter` | send message (chat) · focus agent (agents) |
| `j` `k` · `↑` `↓` · `g` `G` `Home` `End` | move down/up the rows (agents) or the transcript (chat) |
| `PgUp` `PgDn` | page the rows (agents), the transcript (chat), or a picker's list |
| `←` `→` | the selected agent's parent / first child (agents) |
| `Enter` · `c` · `Esc` | focus · cancel · back to the root (agents) |
| `←` `→` `Home` `End` · `Backspace` `Delete` | edit the message box (chat) |
| `Ctrl-P` | model picker |
| `Ctrl-N` | new chat (stops every agent, restarts the root) |
| `Ctrl-C` | stop the focused agent — an idle one is left alone, and a cancel reaches a model that is still thinking |
| `Ctrl-X` | stop every running agent |
| `Ctrl-Q` | quit |

Chat commands: `/provider`, `/model`, `/context`, `/url`, `/key`, `/models`,
`/worktrees`, `/diff`, `/merge`, `/discard`, `/notes`, `/new`, `/help`, `/quit`.

## The screen

The line above the facts is a model of the workspace, not a log. Its **first
line** is the newest event that has no other home — a failure, a stop, a job's
report, a command's answer — or the one derived fact the rows only imply (a root
that ended its turn with children still working and will resume by itself), else
a fading status, else a hint. It never repeats the activity a row and the
transcript already show. Its **second line** (on terminals at least 24 rows tall)
is the stable facts, cut from the right when the terminal is narrow:

```
⌂ ~/p/mush │ master ±3 +12−3 │ deepseek-flash · ctx ~500k
```

`±3` counts paths with uncommitted changes, `+12−3` the line delta against
`HEAD`. Terminals narrower than 80 columns (or shorter than 20 rows) get a
**compact** layout: the agent strip on top, chat below. Below 40×10 mush says
so instead of painting shreds.

Under the conversation is mush's own **foot**: `·` for what happened, `!` for a
failure. It never takes more than three rows — two for the lines and one for the
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

1. **You**: `--context N`, `MUSH_CONTEXT=N`, or `/context N`. A number you state
   is remembered in `.mush/session.json` and never overruled.
2. **The endpoint**, when it advertises one and mush fetched its model list:
   llama.cpp's `meta.n_ctx`, vLLM's `max_model_len`, OpenRouter's
   `context_length`. Discovery only runs when no model was named, or on
   `/model`, `/models`, and `/url`.
3. **The model's documented window** — `deepseek-flash` and `deepseek-v4-pro`
   are 500k, so a hosted API (which answers with ids and nothing else) is not
   silently treated as an 8k local model.
4. **The provider default**: 120k for DeepSeek, 8192 for a custom endpoint.

One reply is capped at a quarter of that window — floored at 1 024 tokens and
capped at 120 000 — so a thinking model has room to answer without the request
overshooting the window it is sent to. The cap is what mush sends as `max_tokens`
(or `max_completion_tokens`, see `/help`), and `mush --print-config` prints the
number it resolved to.

The tool caps (a read, command output, a listing) scale with the window, so one
`read_file` can never fill an 8k transcript. If a server rejects a request over
its context length, mush reads the number out of the complaint, tells the UI,
and retries once.

## What it writes

- `./.mush/` — workspace-local state, git-ignored by itself:
  `session.json` (the root conversation, provider, endpoint, model).
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
scripts/mock_llm.py scripted model server, kept for manual pty smoke (no test refers to it)
docs/mush.md        the design doc
```

## Tests

```sh
cargo test                    # offline unit tests; the agent-tree scenarios
                              # run in process on a scripted model client
cargo test -- --ignored       # the three live-endpoint checks (the model list,
                              # the shipped reply cap, a TLS handshake)
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
