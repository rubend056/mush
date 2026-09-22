# mush — secrets, the stored session and configuration, audited

A blind audit of the layer where a defect leaks a credential, loses the human's
conversation, or makes two surfaces disagree about one fact. Base `4436db5`,
branch `audit/secrets-session-config` (own worktree). Nothing here was fixed.

**Read end to end.** `crates/mush-core/src/session.rs`, `config.rs`,
`userconfig.rs`, `provider.rs`; `crates/mush/src/session_save.rs`, `main.rs`
(startup, argv, the key's roads, the lock, `--print-config`, the error lines),
`app/mod.rs`'s restore path (`App::new`, `reclaim_isolated`, `restore_agents`,
`seed_children`, `session_snapshot`, `save_session`, `flush_session`,
`session_unreadable`) and, where a claim crossed into one, `http.rs`'s
`write_request`, `machine.rs`'s `Shell`, `agent.rs`'s refusal arms, `git.rs`'s
`landing`, `attach.rs`'s transports. The ledger (`docs/findings.md`) and §8.44–§8.50
were read first; nothing already closed is re-reported.

**Probes** (throwaway, all deleted — `git status` is clean):

- a fake OpenAI endpoint on `127.0.0.1` recording the exact request bytes;
- a pty driver (`/tmp`) that runs the real binary, sets the window size, sends
  keys and captures frames;
- a one-test probe inside `machine.rs` (reverted with `git checkout --`, the
  file is byte-identical to the base) that ran a command through the real
  `Shell`;
- the binary's own `--print-config`, and `git status` in scratch repositories
  under `/tmp`.

## Priority order

1. **C1** — `MUSH_API_KEY` is inherited by every shell the model runs: the
   credential is readable by the model, lands in a stored tool result, and the
   store is readable by any local user through the attach socket.
2. **C2** — a typo in the home config's `provider` is silently ignored: the key
   is then sent to the default *LAN* endpoint, with exit 0 and no word.
3. **C6** — `/provider deepseek` keeps the current key and fires a request at
   the vendor's endpoint with it, then writes that key into the home config as
   the vendor's.
4. **C5** — the store's self-ignore is not enforced: a `.mush/.gitignore` that
   is not mush's makes the conversation visible to `git add -A`.
5. **C4** — `Ctrl-N` writes an empty conversation over the human's, with no
   backup and no confirmation.
6. **C9** — the restore trusts agent ids: a stored `id: 0` replaces the root's
   conversation on screen and the root's mailbox in the tree, and `u64::MAX`
   panics the app.
7. **C3** — a home config mush cannot parse is silently discarded, and the next
   write replaces it.
8. **C7**, **C8**, **C10**, **C11**, **C12** — minor, listed in that order.

---

The sections below run in finding-number order; the list above is the order I
would fix them in.

## C1 — the model's own shell carries `MUSH_API_KEY` (major, proven)

**What is wrong.** `Shell::spawn` runs `sh -c <command>` with the environment it
inherited and never removes the key, so every `run_command` the root or a child
issues can read it. The output of that command is a tool result, and a tool
result is stored in `.mush/session.json` verbatim (`Session::save`,
`crates/mush-core/src/session.rs:328`) — so the key can be *written into the
store* by one `env`-shaped command, and read back by anyone who can reach the
attach socket (`.mush/mush.sock`, `mush read`).

**Code.**

`crates/mush/src/machine.rs:96`:

```rust
    fn spawn(&self, cmd: &ShellCommand) -> Result<Box<dyn Job>, String> {
        let out = Scratch::new("out")?;
        let err = Scratch::new("err")?;
        let mut shell = Command::new("sh");
        shell
            .arg("-c")
            .arg(cmd.command)
            .current_dir(cmd.root)
```

— no `env_clear`, no `env_remove`. The environment is read at
`crates/mush-core/src/config.rs:341` (`api_key: env_nonempty("MUSH_API_KEY")`)
and never scrubbed; `grep -rn "env_remove\|env_clear" crates` answers nothing.

**Evidence.**

- The throwaway test (deleted) ran `printenv MUSH_API_KEY` through the *real*
  `machine::Shell` with `MUSH_API_KEY=sk-probe-inheritance-0123456789` set in the
  process: `PROBE: a command through Shell saw from its environment:
  "sk-probe-inheritance-0123456789\nMUSH_API_KEY=sk-probe-inheritance-0123456789\n"`.
- The whole road is in the tree: `run_command` (`agent.rs`) → `Registry::launch`
  → `Shell::spawn`; a result is a `Message::tool(...)`, `App` marks the session
  dirty on `AgentEvent::Message` (`app/mod.rs:1585`) and the writer stores it.
- The claim it breaks, in as many words: `docs/mush.md:812` — *"The API key is
  never stored here"* — and `crates/mush/src/main.rs:73` — *"a key comes from
  `MUSH_API_KEY` or the home config"*. Nothing says the key is handed to the
  model's own shell.

**Blast radius.** The human's provider credential. The model is the least
trustworthy reader in the system (a repo's README, a brief or a pasted file can
tell it to run `env`), and the two places the key can then sit are both
durable: the transcript in `.mush/session.json`, and any file it writes (which
is where a `git add -A` finishes the leak). `.mush/mush.sock` is created with
the process umask (`srwxr-xr-x` in the probe) and `attach.rs` checks no peer
uid, so on a shared machine a second local user can `mush read` the key out of
the transcript too. (The socket's own permissions are the attach boundary's
item, §6; the *secret's* consequence is this finding's.)

**Fix.** `env_remove("MUSH_API_KEY")` on the command `Shell::spawn` builds —
and on the `git`/clipboard children while there, since none of them needs a
model credential. (If a user's credential helper ever needs an env var, name
that variable explicitly rather than inheriting the provider's.)

**Acceptance test.** In `machine.rs`'s tests: set `MUSH_API_KEY`, run a command
through `Shell` that writes `printenv MUSH_API_KEY` to a file in the job's
root, assert the file is empty and the exit status is the one `printenv` gives
for an unset variable; and a second one asserting the same for
`MUSH_CONFIG`-independent variables the shell *should* keep (`PATH`).

---

## C2 — a typo in the home config's `provider` is silently ignored, and the key goes to the LAN endpoint (major, proven)

**What is wrong.** Three readers validate the provider by name — the command
line, `MUSH_PROVIDER` (finding A17's fix), and the CLI-vs-env arm of
`resolve_with` — and two do not: the home config's value and the session's. A
provider name mush cannot read there is dropped without a word, which leaves the
provider at its default `custom` — whose endpoint is a *LAN host*
(`provider.rs`'s `ProviderSpec { name: "custom", default_base_url:
"http://rubendpc:8078" }`). The key is still in hand, so it is sent there.

**Code.** `crates/mush-core/src/config.rs:823`:

```rust
    if !provider_given {
        if let Some(provider) = Provider::parse(&home.provider) {
            config.provider = provider;
```

and `crates/mush-core/src/config.rs:876` for the session. Nothing else; no
`else` arm, no error. Contrast the same file's own rule, written for
`MUSH_PROVIDER` at `config.rs:367` — *"Ignoring a typo would leave the provider
at its default — `Custom`, whose default endpoint is a LAN host — so a key meant
for somewhere else would be sent there (finding A17)"* — and `docs/mush.md`,
which promises *"A value mush does not know is rejected at startup by name,
never sent and never quietly replaced by a default."*

**Evidence.** A probe home config `{ "api_key": "sk-home-KEY-…", "provider":
"deepsek", "model": "deepseek-flash" }` (the shape the file's own header
invites: every field optional, `base_url` omitted) run through the real binary:

```
$ MUSH_CONFIG=…/config.json mush --print-config ; echo $?
endpoint       http://rubendpc:8078
provider       custom
model          deepseek-flash
api key        sk-h…mnop (masked)
EXIT=0
```

No line on stdout or stderr says the file's `provider` was ignored. The same
typo spelled the two ways that *are* checked:

```
$ MUSH_PROVIDER=deepsek … mush --print-config
mush: MUSH_PROVIDER: unknown provider `deepsek` (try deepseek or custom)   [exit 1]
$ … mush --print-config --provider deepsek
mush: unknown provider `deepsek` (try deepseek or custom)                  [exit 1]
```

and the same file's `reasoning_effort` typo *is* reported (`mush: home config:
unknown reasoning effort `very` (try low, high or max)`), so the layer is not
"unchecked by design" — this one field is.

**Blast radius.** A live credential is transmitted to a host the human never
named (the other end of a mistyped `--provider`, which A17 exists to prevent),
silently and with exit 0. The session's provider has the same arm, so a session
written by a build that named a provider this one does not know also falls back
to the LAN endpoint rather than saying so.

**Fix.** Make the two silent `if let Some(provider) = …` arms report like the
other three: `Err(format!("home config: unknown provider `{value}` (try …)"))`
and the same for the session layer (a *session* typo should be a notice rather
than a hard start failure — the session is not hand-edited input, so name the
provider and keep the endpoint it stored). While there, give the message the
file path (`userconfig::config_path()`), because `MUSH_CONFIG` can point
anywhere (see C3).

**Acceptance test.** `config.rs`'s tests: a `resolve_with` with a home config
whose provider is a typo must be an `Err` naming the value and the list of
providers, and one with `MUSH_PROVIDER` unset must not resolve to
`Provider::Custom`'s LAN endpoint; a session whose provider is unknown must be
reported, not silently replaced.

---

## C6 — `/provider` sends the current key to the vendor's endpoint, then saves it as that vendor's (major, proven by reading)

**What is wrong.** Selecting a provider switches the model, the endpoint and
the window in one write — and leaves `api_key` alone. `apply_provider` then
calls `refresh_models()`, which makes a request with the key *before* the human
can say anything, and `persist_user_config()` writes that same key into the
home config under the new provider.

**Code.** `crates/mush/src/app/mod.rs:2964`:

```rust
        self.switch_provider(provider);
        self.refresh_models();
        self.persist_user_config();
```

`switch_provider` (`app/mod.rs:2995`) touches `provider`, `base_url` and the
model and never `api_key`; `persist_user_config` (`app/mod.rs:2780`) copies
`api_key: self.cfg().api_key.clone()` into the file it saves. `refresh_models`
→ `http::list_models` → `get_json(models_url, cfg.api_key.as_deref(), …)`, and
the header is built at `crates/mush/src/http.rs:445`
(`head.push_str(&format!("Authorization: Bearer {key}\r\n"))`).

**Evidence.** `grep -rn api_key crates/mush/src/app/mod.rs` answers exactly
three lines, all of them the `/key` arm plus `persist_user_config` — nothing on
the provider road. `config.rs`'s own `resolve_with` state-4 comment states the rule
for the startup path ("a stored provider must not leak its hosted-provider knobs
to a URL it does not own") and `Provider::parse`'s doc says the key's
destination is why no `openai` alias exists; the runtime switch is the one road
that changes the *host* without the human naming it. The request the vendor
receives is the same code path as any chat request — I did **not** fire it at a
real vendor to prove it on the wire (and would not put a probe key in front of
the human's LAN host either); the header road itself is pinned by
`http.rs`'s own `assert!(sent.contains("Authorization: Bearer secret"))` at
`http.rs:1304`.

**Blast radius.** A key minted for a LAN box or a proxy is handed to a third
party the moment the human picks "deepseek" in the picker (the model-list fetch
is the first request), and is then persisted as if it were that vendor's key, so
the leak outlives the session. On a metered vendor it is also a request the human
did not intend.

**Fix.** On a switch that changes the host (`provider.spec().switches_endpoint`,
or a `/url` whose host differs), forget the key — set `cfg.api_key = None` — and
say so in the ack: `provider: deepseek · no api key for this endpoint — /key
<secret> sets one (saved to …)`. `/key` then states the destination as it already
does, and no request goes out with a foreign key. (The alternative — keeping the
key and warning — leaves the leak on the very next call.)

**Acceptance test.** In `app/mod.rs`'s switch tests: a cell with a key, a
`switch_provider(Provider::DeepSeek)`, then assert `cell.ui().api_key` and
`cell.handle().config()?.api_key` are `None` and the status line names `/key`;
and the pinned-invariant form of C2: no request is built with an `api_key` whose
endpoint changed under it.

---

## C5 — the store's self-ignore is created only if absent, never enforced (major, proven)

**What is wrong.** The module states the invariant unconditionally — "Everything
mush writes lives in `<root>/.mush/`, which ignores itself via a one-line
`.gitignore`" (`session.rs:1`) and the manual's tree says `.gitignore # contains
a single line: *` (`docs/mush.md`). `ensure_mush_dir` writes that line only when
the file does not exist, so any other content — a hand edit, another tool, or a
repository that ships its own `.mush/.gitignore` — silently makes the
conversation visible to git.

**Code.** `crates/mush-core/src/session.rs:29`:

```rust
pub fn ensure_mush_dir(root: &Path) -> std::io::Result<()> {
    let dir = mushroom_dir(root);
    fs::create_dir_all(&dir)?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        fs::write(&ignore, SELF_IGNORE)?;
    }
    Ok(())
}
```

**Evidence.** Two scratch repositories, each with an unreadable session (so mush
writes a fresh one as it exits), driven through the real binary:

```
# .mush/.gitignore containing "!*"
$ ls -a .mush            → .gitignore  lock  session.json  session.json.bak
$ git status --porcelain → ?? .mush/          # `git add -A` takes the conversation

# the file mush writes on a fresh store ("*")
$ git status --porcelain → (empty)            # the invariant holds where it is enforced
```

**Blast radius.** The whole conversation (and any tool result in it — see C1)
reaches the index and, on the next push, the remote. The trigger is a file mush
believes it owns: `.mush/.gitignore` is documented as mush's one line.

**Fix.** Write mush's line unconditionally (it is one line, idempotent, and the
store is mush's), or read it and rewrite when it does not ignore everything.
Rewriting is the honest reading of "which ignores itself": the file is not the
human's to configure, and a sticky wrong one is a leak.

**Acceptance test.** `session.rs`: a root whose `.mush/.gitignore` says `!*`
(or is empty) after `ensure_mush_dir` must contain exactly `SELF_IGNORE`; and a
`git`-level test in the same module (a `git init`, one session write, `git
status --porcelain` empty) for the property rather than the file.

---

## C4 — `Ctrl-N` writes an empty conversation over the human's own (major, proven)

**What is wrong.** The new-chat key clears the transcripts and then flushes, so
the store is replaced by the empty one — deliberately, and with a test that pins
exactly that. What nothing provides is a copy or a confirmation: the destruction
is immediate, the key is unarmed (unlike `Ctrl-Q`, which arms), and the only
warning in the whole product is a line printed *after* it: `NOTHING_RUNNING`
("nothing running · Ctrl-Q quits · Ctrl-N drops every transcript") and the key's
help row. There is no road back.

**Code.** `crates/mush/src/app/mod.rs:3112`:

```rust
        self.chat.clear();
        self.spin = 0;
        self.discover_worktrees();
        self.refresh_git();
        // The old conversation is gone from this moment: if the write were left
        // to the debounce, a crash would bring it back with the next start.
        self.flush_session();
```

The store's only other protection — the backup beside an *unreadable* file
(`keep_unreadable`, `session.rs:259`) — is not called on this road.

**Evidence.** Real binary, real pty: type a message, `Enter` (the human's turn is
flushed before the run by `flush_session`, `app/mod.rs:3282`), then `Ctrl-N`,
then `Ctrl-Q`:

```
$ cat .mush/session.json
{ "model": "m", "provider": "custom", "base_url": "http://127.0.0.1:9", "messages": [] }
$ grep -rl "a conversation worth keeping" .mush
NOT PRESENT anywhere in .mush
```

**Blast radius.** The human's conversation, from one keystroke, with no undo —
the loss class this area exists to prevent. It is at its worst where the store is
the only copy (children's transcripts, a long run's results).

**Fix.** Two cheap halves, both in the shape the codebase already uses. (i)
Before the clear, keep the conversation the way an unreadable one is kept:
`Session::save` it as `.mush/session.json.previous` (or reuse `keep_unreadable`'s
numbering), so the key costs a reclamation rather than the work. (ii) Arm the
key when the conversation is non-empty, exactly as `Ctrl-Q` arms when something
is running: the first `Ctrl-N` says what would go ("Ctrl-N again clears #N lines
of this conversation — they are kept as session.json.previous"), the second
clears. The line already exists as prose; it just arrives too late.

**Acceptance test.** `app/mod.rs`: a chat with one message, `ctrl(&mut app, 'n')`
→ the store holds the message under the previous name *and* an armed line; a
second `ctrl(&mut app, 'n')` clears the live file and leaves the copy; and an
empty chat is cleared by one press (the key must not become slower for the state
it is for).

---

## C3 — a home config mush cannot read is silently discarded, then overwritten (major, proven)

**What is wrong.** `UserConfig::load_from` reads the file and falls back to
defaults on *any* failure — unreadable, not JSON, or one field of the wrong type
— with nothing said to the human, no line on the bar, and no copy kept. The
layer below it is not the human's file: it is the built-in defaults, whose
endpoint is `custom`'s LAN host. And because `save_to` merges by parsing the
*existing* file as JSON, an unreadable file has nothing to merge — so the first
write from the TUI (`/key`, `/url`, `/model` or `/provider`) replaces it with
mush's four fields and the header, and whatever the human had is gone.

**Code.** `crates/mush-core/src/userconfig.rs:153`:

```rust
    pub fn load_from(path: &Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
```

and `userconfig.rs:180` for the merge:

```rust
        let existing = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        if let (Some(fields), Some(Value::Object(before))) = (merged.as_object_mut(), &existing) {
```

**Evidence.** The real binary, two files (both `--print-config`, exit 0, nothing
on stderr):

```
$ printf '{ "api_key": "sk-secret", ' > broken.json ; MUSH_CONFIG=broken.json mush --print-config
endpoint       http://rubendpc:8078
provider       custom
api key        (none)                       # the human's key is gone, silently

$ printf '{ "api_key": "sk-secret-abc", "provider": "deepseek", "context": "64000" }' > badtype.json
$ MUSH_CONFIG=badtype.json mush --print-config
endpoint       http://rubendpc:8078         # the whole file is dropped by one wrong type
provider       custom
api key        (none)
```

The overwrite half is a read of one call (`persist_user_config` →
`UserConfig::save` → `save_to`): with `existing == None` the merge block is
skipped and `atomic_write` writes this value's fields alone.

**Blast radius.** The human's key, endpoint and settings vanish; the run goes to
the LAN default (or to whatever the defaults are) with no clue, and `--print-config`
cannot tell them either, because it shows the same resolution. (The session file
gets a `.bak` and a red line for exactly this class of accident — finding S3 —
so the *store* is treated as the human's, and their config as nobody's.)

**Fix.** One door for the whole layer: `load_from` should report *why* — a
`Result` or a small `Loaded { config, complaint }` — and `main` should say it the
way the session's failure is said ("could not read <path> — <reason>; using
defaults"), with `--print-config` printing the complaint on the `session`-style
row. A typo must not take the TUI down (the current doc is right about that);
it must also not be invisible. The file also deserves the same treatment before
a write replaces it: keep a `.bak` when the existing file does not parse.

**Acceptance test.** `userconfig.rs`: `load_from` on an invalid file returns the
complaint and the path; `save_to` over an invalid file leaves the original bytes
at a `.bak`; and `main.rs`'s `describe` prints `unreadable — <reason>` in a row
of its own, the way the session layer's `Unusable` already is
(`main.rs:635`).

---

## C7 — a key with a newline injects header lines (minor, proven on the wire)

**What is wrong.** The key is interpolated into the request head with no
validation at any entry point (`MUSH_API_KEY`, the home config's `api_key`,
`/key`), so a key containing CRLF adds headers of its own to the request — or
ends the head early and leaves the body to be read as the next request's
preamble on the pooled connection.

**Code.** `crates/mush/src/http.rs:445`:

```rust
    if let Some(key) = ask.api_key {
        head.push_str(&format!("Authorization: Bearer {key}\r\n"));
    }
```

Entry points: `config.rs:341` (`env_nonempty` filters only empty strings — no
trim, no control-character check), `userconfig.rs:89`, and `/key`
(`app/commands.rs:254`; the *whole* argument is trimmed, so a trailing newline
is handled there — an interior one from the box's Shift-Enter is not).

**Evidence.** The fake endpoint, with `"api_key": "sk-inject-0123456789\r\nX-Injected-By-Key: yes"`
in the home config, received:

```
POST /v1/chat/completions HTTP/1.1
Host: 127.0.0.1:18100
Authorization: Bearer sk-inject-0123456789
X-Injected-By-Key: yes
Content-Type: application/json
Content-Length: 9384
```

`grep -c "^X-Injected-By-Key: yes"` → 1: it is a header the server will act on.

**Blast radius.** Today: a copy-pasted key with a newline in it produces a
malformed request whose symptom (a 400, or a reply read off the wrong response)
points at the endpoint rather than at the key. With `\r\n\r\n` the injected
bytes are a whole second request on a pooled connection, and the pool's own
invariant (a reply may only be read if its framing framed it) is what decides
whether the next caller reads the smuggled request's answer as its own.

**Fix.** Validate where the value enters: trim the key and refuse a value
containing a control character, by name, at startup (`mush: MUSH_API_KEY
contains a control character — check the value`) and in `/key`'s arm ("a key
cannot contain a newline"). At the header, the one-door rule for terminal-bound
strings has a sibling here: `write_request` should build the `Authorization`
value from a checked key rather than a raw one.

**Acceptance test.** `http.rs`'s request tests: a key with `\r\n` is refused (or
replaced by its trimmed, control-free form) and the bytes written contain
exactly one `\r\n` after the bearer token; `main.rs`: `--print-config` names the
problem instead of masking the key.

---

## C8 — the attach printers emit control sequences (minor, proven)

**What is wrong.** `escape_line` makes a transcript line one line (finding A3's
fix) but does not defang it, so a control sequence in the conversation reaches
the human's terminal raw — the one road in the tree that does not go through
`text::sanitize`.

**Code.** `crates/mush/src/main.rs:465`:

```rust
fn escape_line(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}
```

used by `print_lines` (`main.rs:473`) and, without even that, by `print_agents`
(`main.rs:487`) for titles and branches.

**Evidence.** A stored session whose message holds `ESC ] 0 ; PWNED BEL`, read
through the real CLI while mush ran:

```
$ mush read . | cat -v
0	look: ^[]0;PWNED^G done
1	ok
```

The same text in the TUI pane is defanged: the sweep proves it and a pty probe
of a *stored* notice shows the sequence gone and the words kept
(`boom` + `after`, no OSC).

**Blast radius.** The human's terminal (a window/tab rename, a clipboard write,
or worse on a terminal that acts on the payload), reachable from anything in the
conversation — a model reply, a tool result, a file `search` returned, or a
hand-edited session.

**Fix.** Route both printers through `mush_core::text::sanitize` before the
escape (or add the control-character pass to `escape_line`, which is *the*
terminal door for these two commands) — `print_agents`' columns are title,
activity and branch, all endpoint/model text.

**Acceptance test.** `main.rs`: a `read` body and an `agents` body each carrying
`ESC ] 0 ; x BEL`, printed into a `Vec<u8>` (a small writer seam) must contain no
`\x1b`.

---

## C9 — the restore trusts the file's agent ids: `id: 0` replaces the root (major, proven)

**What is wrong.** Every stored agent is registered under its own id, with no
check that the id is a child's. `AgentId::ROOT` is `0`, so a stored row whose
`id` is `0` is registered *as the root*: its actor's mailbox replaces the real
root's in the tree, and its transcript replaces the root conversation in every
pane. A duplicate id does the same to the first row. An id of `u64::MAX` panics
the app in a debug build.

**Code.** `crates/mush/src/app/mod.rs:743` and its neighbourhood:

```rust
            self.tree.reserve_agents(agent.id + 1);
```

then `self.tree.register(Existing { id: AgentId(agent.id), … })`
(`app/mod.rs:845`) and `self.chat.replace_transcript(AgentId(agent.id),
agent.messages)` (`app/mod.rs:865`). `tree.register` is unconditional
(`app/tree.rs:846`):

```rust
        if let Some(tx) = node.tx {
            self.agent_tx.insert(node.id, tx);
        }
        self.agents.push(AgentNode { id: node.id, … });
```

**Evidence.** A session file whose root transcript says `THE REAL ROOT
CONVERSATION` and whose `agents` holds one row `{"id": 0, "parent": 0,
"brief": "impostor", "messages": ["IMPOSTOR LINE"]}`, driven through the real
binary in a pty — the frame, reflowed to its two significant cells:

```
│▶· #0 root                │ you › IMPOSTOR LINE
```

The root's own words are gone from the screen, and `session_snapshot`
(`app/mod.rs:3301`) would now store the impostor's line as the root's transcript
— the probe's file survived only because nothing marked the session dirty before
the quit. `agent_tx[ROOT]` holds the revived impostor's mailbox too, so the
human's next message runs *that* actor (its brief, its workspace) while the real
root actor is unreachable. The second specimen, `"id": 18446744073709551615`:

```
thread 'main' (2551797) panicked at crates/mush/src/app/mod.rs:743:38:
attempt to add with overflow
```

(In a release build the add wraps to `0`, so the reservation that row asked for
is silently skipped — harmless for that one id, since the counter can never
reach `u64::MAX`, but it means the guard is not there.)

**Blast radius.** A session file is *not* untrusted input today: `.mush/` is
ignored by git, but a repository can still commit a `.mush/session.json` (an
ignore rule does not untrack a tracked file) and a clone brings it, and the
human's own store is hand-editable. The cost is the conversation class: the
root's history replaced on screen and (after the next message) in the file, the
root's actor replaced, and a Rust panic in the debug builds developers run.

**Fix.** Validate the file's rows in the restore, in one place, before anything
is registered: an id of `0` (the root), an id already taken by a registered node
or an already-restored agent, or a dangling cycle of parents is a row to
*refuse* and report — the same shape as the S3 notice ("agent #0 in
.mush/session.json has the root's id; the row was skipped").
`agent.id.saturating_add(1)` for the arithmetic.

**Acceptance test.** `app/mod.rs`: a session with a child whose id is `0`
restores with the root's transcript untouched, the child's row refused and a
line naming the reason; a session with two rows of one id keeps the first and
refuses the second; a session whose id is `u64::MAX` restores without panicking;
and `agents_floor()` is asserted to be above every id the file held.

---

## C10 — the fold's refusal echoes the endpoint's whole body into a notice (minor, proven by reading)

**What is wrong.** The run's own refusal truncates what the endpoint said to 600
characters (`agent.rs:2568`); the fold's arm hands the *whole* body into
`AgentEvent::Notice`, and a notice is wrapped and painted. The body is bounded
only by `http.rs`'s `MAX_BODY_BYTES = 80 MiB`.

**Code.** `crates/mush/src/agent.rs:3045`:

```rust
                    ModelError::Status { status, body } => {
                        format!("the endpoint answered {status}: {body}")
                    }
```

reached only when `asked` (a human `/compact`).

**Blast radius.** A hostile or verbose endpoint plus one `/compact` puts up to
80 MiB through the notes list — wrapped into rows on the UI thread — and the
same sentence is what `/notes` re-wraps. (It is an `Info` notice, so it is not
stored and no write is at stake.)

**Fix.** `truncate(&body, 600)` there too — the same call the run's arm uses, so
the two refusals read alike. If the length is news, say it: "…: <600 bytes> …
(the endpoint said 4 096 bytes more)". (This one is `agent.rs`, which a fix
wave is in as this was written — the record is the point; whoever lands it owns
the file.)

**Acceptance test.** `agent.rs`: a scripted model whose refusal body is 1 MiB,
a `/compact`, and an assertion that the emitted notice is bounded and says it
was cut.

---

## C11 — an environment key is silently copied into the home config by an unrelated command (minor, proven by reading)

**What is wrong.** `persist_user_config` writes the *resolved* key, so a key the
human supplied for one run through `MUSH_API_KEY` (the README's own road:
`MUSH_API_KEY=sk-… mush`) is written to disk by any later `/url`, `/model` or
`/provider` — a command whose ack says nothing about a file.

**Code.** `crates/mush/src/app/mod.rs:2780`:

```rust
        let user = UserConfig {
            api_key: self.cfg().api_key.clone(),
```

**Blast radius.** A credential the human deliberately kept out of files is now
in one (mode 0600 — see "verified sound" — but a file that outlives the run,
survives a container image or a copied home, and is rewritten by mush's own
header). The `/key` command does say `saved to <path>`; these three do not.

**Fix.** Either state it in the ack ("endpoint: … · the api key was saved to
<path>") whenever the key written differs from the file's, or write the key only
on the road the human used to state it (`/key`), leaving an env key in the env.

**Acceptance test.** `app/mod.rs`: a cell whose key came from the environment,
a `/url`, and an assertion about what reached `UserConfig` (a fake writer, the
way `session_save::fake::Recorder` does it) plus the line the human reads.

---

## C12 — `--print-config` cannot answer the image gate (minor, gap)

**What is wrong.** The dump is advertised as "what a request will carry"
(`main.rs:649`, `docs/mush.md`) and prints every request knob — but not the one
*capability* a request can be refused for: whether the model may be sent a
picture. The answer lives only in the provider table
(`provider::vision_capable`, `provider.rs`'s `ModelSpec::vision`, `deepseek-flash`
the one row on), and a human pasting a screenshot at a custom endpoint learns it
from the refusal.

**Evidence.** The probe dump, in full, has no vision row; and the gate asks
`vision_capable(&model)` in three places (`app/mod.rs:2210`, `3658`,
`agent.rs:2187`) with no override anywhere in the tree.

**Blast radius.** A wart rather than a wound: the human cannot check the fact
that will decide their paste, and the only way to test it is to spend the
gesture. (It also means "a model that cannot see" is a refusal with no road
around it for a local vision model the table does not name — the table's own doc
records that as deliberate.)

**Fix.** One `("vision", "yes — image parts are sent" | "no — the table does not
document image parts for <model>")` row in `describe`, read from
`vision_capable`, and a mention in the docs' list of what the dump prints.

**Acceptance test.** `main.rs`'s `describe` test: a table model reports yes, a
model the table does not name reports no, and the `--help` line names the row.

---

## Recorded, not changed — and one place I would push back

- **H21's fix does not survive a restart** (recorded in §8.33 as deliberate, and
  I agree with the direction, but the durable half is new): a restored agent has
  no fork revision (`AgentSession` has no such field; `app/mod.rs:1019` passes
  `node.fork.clone()`, which is `None` for every restored node), and
  `git::landing` answers `Merged` when it cannot ask (`git.rs:522`). So a
  read-only child that never committed — H21's specimen — comes back, is swept,
  and has `merged` written into the row *and into the session file as
  `StoredLanded::Merged`*, where it outlives the process that could still have
  measured it. §8.33 argues the label is the safer of two guesses and that the
  removal is git-certified either way (it is: `reclaimable` only removes when the
  base contains the branch). What I would change is not the default but the
  permanence: `landed` was added to the file for exactly this kind of fact, so
  `fork` (an `Option<String>` revision) belongs beside it, absent for old files
  and then answering today's conservative way. That is a small, additive field
  and it makes H21's two questions answerable after a restart, which is the case
  the disclosure in §8.23 was about.
- **H30 stays open** (a restored mid-phase child reads `◐ running` while nothing
  runs) and my read agrees with the record: neither surface is simply wrong, and
  the fix is the same ruling H18/H22 needed. It is *worse* than recorded in one
  respect worth naming: the restored row is what `git status`/`wait` disagree
  with, so the human's two questions — "is it working?" and "is its work
  reachable?" — get opposite answers from one screen.
- **H27** (a nested child's reclamation measured against `HEAD` when its parent's
  branch is gone) is recorded and unchanged; the fork-revision field above would
  also retire it, since the fork *is* the base the work went into.
- **H16 residual / §8.28's bound** is unchanged by this read: the store's size is
  bounded by `CHILD_HISTORY × the fold trigger + the root`, and the root's own
  transcript in the file is the folded one. Nothing I probed contradicts it.

## Verified sound (and the check that convinced me)

- **The key reaches the `Authorization` header and nowhere else in a request.**
  Fake endpoint, real binary, key `sk-probe-KEY-0123456789`: the raw bytes hold
  it exactly once (`grep -c` → 1), on the `Authorization` line, and the request
  body carries none.
- **The key never reaches the store.** Same run: `grep -rl "sk-probe-KEY"
  <workspace>` (including `.mush/`) → *not present*. Structurally, too: `Session`
  has no key field (`session.rs`'s struct), and `Config`/`UserConfig`/`Overrides`
  are never serialized into it (they are not `Serialize`).
- **The key is masked wherever it is shown.** `--print-config` → `sk-h…mnop
  (masked)` (probe + `describe_reports_the_request_not_the_wishes`); `/key`'s
  acks quote `mask_key`; `mask_key`'s own tests pin a short key to `••••` and a
  multi-byte key to four characters, not four bytes.
- **The store is not world-readable on a multi-user machine.** `atomic_write`
  is `NamedTempFile::new_in` + `persist` (rename), so a rewritten file takes the
  temp's mode: the probe's `session.json` is `-rw-------`, and so is the home
  config mush writes.
- **`--print-config` is inert and honest.** No `.mush/`, no lock, no request, a
  directory that does not exist is described, exit 0 (probe); it runs while
  another mush holds the lock (probe); a session that is *there and unusable* is
  a different row from none (`Stored` is read as `Stored`, `main.rs:635`;
  `describe_reports_the_session_layer_it_read` pins the three cases).
- **The precedence lives in one function.** `config::resolve` is the only reader
  of `UserConfig::load()`, `Session::read` and `Overrides::from_env_checked` on
  the startup road (`main.rs:645` and `main.rs:902` are the only two callers),
  and `resolve_with` is pure, so the whole chain is testable and tested
  (`cli_flags_beat_every_stored_layer` and friends).
- **A malformed value is reported by name and exits 1** for `MUSH_PROVIDER`,
  `MUSH_CONTEXT`, `MUSH_REASONING_EFFORT`, `MUSH_THINKING`, `--provider`,
  `--context`, `--temperature` (NaN included), `--reasoning-effort`,
  `--thinking` and the home config's `reasoning_effort` — probed. (C2/C3 are
  the two holes in this rule, not a crack in the rule.)
- **The stored conversation round-trips.** A probe session's bytes are read back
  by the next start; `session_roundtrips` pins model/provider/base_url/context,
  a child's brief/title/branch/status/landed/leftover/summary/unread flag and
  its transcript, and a notice's agent and stamp.
- **A conversation saved mid-run cannot reach a strict endpoint broken.**
  `agent::revive` and the UI's `Adopt`/`Run` doors all pass through `adopted()`,
  which runs `repair_tool_pairs` and re-places the dropped-turns note
  (`agent.rs:1405`, `agent.rs:1741`) — a session written between an assistant's
  tool call and its results is repaired on the way back in.
- **A phase that was live is not flattened.** An in-flight phase is written as
  `running`, and `restore_agents` maps `Running | CutOff` to `Phase::CutOff`
  with its own line (H2); a stored failure is only kept as a failure when the
  transcript still ends where a failure would leave it.
- **The store's ignore works where it is enforced**, the lock keeps one mush per
  workspace (the refusal names the holder's pid, `--print-config` is exempt, the
  file is never unlinked), and the writer's seam behaves: newest snapshot wins,
  a burst is one write, `flush` waits for the file, a failed write is reported
  once (`session_save.rs`'s four tests + the lock probe).
- **A hostile string *stored in the session* is defanged when painted.** A
  pty probe with an OSC sequence in `notices[].text` paints the words with the
  sequence gone (`boom` … `after`), so the sanitize door is at paint and covers
  the file's road as well as the live one. (C8's `mush read` is the road that is
  outside that door.)
- **`keep_unreadable` does what it says**: a broken session becomes
  `session.json.bak`, a second one `.bak.2`, the bytes are byte-identical, and
  the line the human reads names the file, the reason and where the copy went —
  probed end to end (the frame shows it in the pane's foot *and* the bar).
- **The human's own turn is on disk before the run it starts**, and quitting
  flushes: the probe's message survived in `session.json` immediately after
  `Enter`, and `Ctrl-Q` wrote the file (so the 60 s debounce's exposure is
  machine-generated turns only, as its doc says).

## Not verified

- The 60 s crash window's real cost: I demonstrated the two synchronous flushes
  (the human's turn, quitting) but did not kill a long run mid-stream and count
  what a restart brought back. `SESSION_DEBOUNCE`'s doc and `App`'s `Drop` are
  the whole of the argument.
- `atomic_write` has no `fsync`; a power loss can leave a renamed-but-empty
  file. I could not stage a power cut. The failure mode *should* land in
  `Stored::Unusable` (an empty file is not a `Session`: `messages` is required)
  and so on the `.bak` + notice road — but that is reasoning, not a probe.
- Whether the key leaves the machine by any road other than the request header:
  I read every site that names it and grepped the tree, and the only two roads
  out are the header and (C1) a shell's stdout. I did not `strace` the process.
- C6's second half on the wire (the vendor actually receiving the old key): I
  would not point a probe key at a vendor or at the human's LAN host, so the
  claim rests on the config's own plumbing plus `http.rs`'s header test.
- `Session::read` reads the whole file (`fs::read`) with no cap; a 13.9 MB
  session is normal traffic (§8.28) and I did not try a multi-gigabyte one.
- Two mush processes reaching one store through two mount points (bind mounts,
  NFS): `flock` is per-inode and I can only say the single-path case is refused.
- Migration beyond the shapes the tests cover (`{"status":"failed"}`, a file
  with no `context`/`agents`/`notices`, the raw `root`/`updated` keys): no other
  old-version file was built.
- A store written by mush running *inside* a child's worktree: read of the code
  only (`Workspace::new` on the worktree; a separate `.mush/`), not probed.
- `Session`'s deserializer against a *hostile* notice/message field of enormous
  size: not staged.

## Blind spots — invariants with no test

Each of these is a claim in prose (or a road) that nothing in the suite pins:

1. **The key is not in the store.** No test greps a written session for the
   key; only the struct's shape prevents it today.
2. **A shell cannot see `MUSH_API_KEY`.** No test at all (C1's probe *is* the
   test that would pin it).
3. **A key with a control character is refused** (C7): no test on any entry
   point, and `http.rs`'s request tests use a clean key.
4. **An unreadable home config is reported** (C3): `userconfig.rs` tests a
   *missing* file and two valid ones; nothing tests invalid JSON, a wrong type,
   or the write that follows.
5. **A typo'd provider in the home config or the session is refused** (C2): only
   the CLI and `MUSH_PROVIDER` roads are pinned.
6. **The store's `.gitignore` survives an existing wrong file** (C5):
   `ensure_creates_self_ignoring_dir` starts from nothing.
7. **A provider switch does not carry a foreign key** (C6): the switch test pins
   the endpoint, model and window and says nothing about the key.
8. **`Ctrl-N` leaves a copy** (C4): the existing test asserts the opposite — the
   stored conversation is cleared — which is precisely why the gap reads as a
   decision rather than a defect.
9. **The restore refuses an id that is the root's, a duplicate, or out of range**
   (C9): nothing in the suite feeds `restore_agents` a hand-written file.
10. **The attach printers cannot emit a control sequence** (C8): A3's test checks
    newlines and tabs only.
11. **The fold's refusal is bounded** (C10): the run's arm's 600-character cut is
    pinned; the fold's is not.
12. **`--print-config` answers the image gate** (C12): there is no row to pin.
13. **A restored agent's `landed` is not invented** — H30 is the live case, and
    no test drives a restore of a phase that was mid-flight into a row whose
    word matches what git can prove.
