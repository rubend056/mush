# mush

A small, fast, agent-agnostic terminal editor. Open a folder, talk to an agent,
and watch it edit the files you have open — without either of you clobbering the
other.

```
┌ agents ─────────┬ src/main.rs ─────────────────────┐
│ ▶ #0 ✓ you      │  1 fn main() {                  │
│   #1 ✓ lexer    │  2     println!("hi");          │
│   #2 ◐ tests    │  3 }                             │
├─────────────────┴──────────────────────────────────┤
│ #1 › (focused agent's chat)                        │
│ mush ›  ⚙ edit_file({"path":"src/main.rs",...})   │
│ message (#1) › _                                    │
└────────────────────────────────────────────────────┘
 agents · chat     deepseek-flash @ deepseek.com
```

## Quick start

```sh
cargo build --release
./target/release/mush /path/to/project    # or just: mush
./target/release/mush src/main.rs         # opens a file
```

`Tab` moves between the **agents** (tree), **editor**, and **chat** panes. Type
in the message box and press `Enter`. The agent reads and edits this workspace
through five file tools plus four delegation tools — see *Subagents* below.

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
> home config > built-in defaults**. The home config file lives at
`$MUSH_CONFIG`, else `$XDG_CONFIG_HOME/mush/config.json`, else
`~/.config/mush/config.json` — it is *machine-global*:

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

The **agents** pane shows the whole tree: depth by indentation, `◐` running,
`⏸` waiting on children, `✓` done (with its final summary), `✗` failed,
branch suffix for isolated agents. Enter on a row focuses that agent — the
chat below switches to its transcript and typing nudges it. `Esc` returns to
the root, `c` cancels the selected agent, `Ctrl-C` cancels everything.

`isolated: true` gives a child its own git worktree
(`.mush/wt/<id>` on branch `mush/<id>`), so parallel agents edit real files
without colliding. **mush never auto-merges** — the tree shows the branch and
these print the exact commands:

```
/diff <id>      git diff HEAD...mush/3
/merge <id>     git merge mush/3
/discard <id>   git worktree remove .mush/wt/3 && git branch -D mush/3
```

Leftover worktrees (`mush/*` branches) are rediscovered on startup and shown
in the tree, so those commands keep working after a restart; `/worktrees`
re-scans.

## Keys

| Key | Action |
|---|---|
| `Tab` / `Shift-Tab` | cycle panes (agents, editor, chat) |
| `Enter` | send message (chat) · focus agent (agents) |
| `i` / `Esc` | enter / leave insert mode (editor) |
| `Ctrl-P` | model picker |
| `Ctrl-S` / `Ctrl-R` | save / reload the open file |
| `Ctrl-N` | new chat |
| `Ctrl-C` | cancel all agents |
| `Ctrl-Q` | quit (twice if there are unsaved changes) |

Chat commands: `/provider`, `/model`, `/url`, `/key`, `/models`, `/open`,
`/diff`, `/merge`, `/discard`, `/new`, `/help`, `/quit`.

## What it writes

- `./.mush/` — workspace-local state, git-ignored by itself:
  `session.json` (the root conversation, provider, endpoint, model).
- `~/.config/mush/config.json` — machine-global defaults **including the API
  key**. The key never touches the workspace.

## Design

See [docs/mush.md](docs/mush.md) for the full design: the single-owner event
loop, why agents edit live buffers instead of stale files, the safety rules,
and the roadmap.

## Layout

```
crates/mush-core/   pure domain: workspace, session, prompt, messages, user config
crates/mush/        the binary: TUI, agent actors, HTTP client
scripts/smoke.py    end-to-end test that drives the real TUI over a pty
```

## Tests

```sh
cargo test                    # offline unit tests
cargo test -- --ignored       # live endpoint tests, plus isolated_subagent
                              # (deterministic orchestration via scripts/mock_llm.py)
python3 scripts/smoke.py target/debug/mush /tmp/mush-smoke
```