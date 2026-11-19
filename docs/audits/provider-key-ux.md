# provider & key — which endpoint mush talks to, and which secret it sends there

An audit of the one surface a human uses to answer *where is this request going,
and who am I there*: `/provider`, the provider picker, `/url`, `/model` and its
picker, `/key`, `/context`, the three layers behind them, and the four surfaces
that read them back. Base `0d59909`, branch `audit/provider-key-ux` (own
worktree). Nothing here was fixed; no file but this one was touched.

The question is not "is `/url` correct". Each command is correct about the field
it owns. The question is whether a human can *predict*, from what mush paints,
which of three layers holds which fact — and the answer is no, because the
commands quietly write more than one layer, the layers do not agree about what a
provider is, and the two sentences that would explain a loss live for five
seconds.

**Read end to end.** `crates/mush-core/src/provider.rs`, `config.rs` (the
provider table, `label`, `set_base_url`, `forget_key_if_host_changed`,
`host_of`, `resolve_with`), `userconfig.rs` (`comment`, `save_to`, `KeyWrite`),
`session.rs`; `crates/mush/src/app/mod.rs` (the five command arms, the two
pickers, `switch_endpoint`, `switch_provider`, `persist_user_config`,
`session_snapshot`, `say`/`fail`/`INFO_TTL`), `app/settings.rs` (`ConfigCell`),
`app/screen.rs` (the facts line, the picker), `main.rs` (`--print-config`,
startup notices); `docs/mush.md` §4.5, §5, §11 for what a human is promised.

**Probes** (throwaway, deleted; `git status` after the audit commit is this file
and nothing else):

- the tree's own binary (`cargo build -p mush`, `0d59909`), run with `HOME` and
  `MUSH_CONFIG` pointed at a scratch home and a hand-written
  `.mush/session.json`, four states deep — `--print-config` only, never the TUI;
- every ack quoted below is read from the code and, where one exists, from the
  test that pins it (named); no ack was captured off a screen, because driving
  the TUI is out of bounds for this audit.

The ledger (`docs/findings.md`) was read for what is already closed: A1, A2, A5,
A9, A17, C2, C3, C6, C7, C11, C12, D6, D19, D21, D22, IN12, IN14 and R19 are
assumed fixed and are not re-reported. Two defects still wrong at `HEAD` — the
root `"api_key": null` a save writes, and `KeyWrite::Stated` being chosen when no
key is in force — are **PK2**; a fix for them is landing on another branch in
parallel, so PK2 states them and does not re-derive them.

---

## The map (read this first)

### The three layers

| layer | where | holds | written by | read by |
|---|---|---|---|---|
| **this run** | argv, `MUSH_*` | provider, url, model, key, window, effort, thinking | the shell | `config::resolve` once, at startup |
| **this workspace** | `.mush/session.json` | `provider`, `base_url`, `model`, a *stated* window (`session.rs:288`, `:292`, `:295`, `:303`) | every save: `/provider`, `/url`, a `/model` pick, `/context`, and any sent message (`session_snapshot`, `app/mod.rs:5198`) | `config::resolve` — it **outranks the home file** (`config.rs:1233`) |
| **this machine** | `$MUSH_CONFIG` else the platform config dir | `api_keys` (host → key), the flat `api_key`, `provider`, `base_url`, `model`, `context`, `temperature`, `max_completion_tokens`, `reasoning_effort`, `thinking` (`userconfig.rs:116-167`) | `/key` (the key) and `/url`, `/provider`, `/model` (the other fields) | `config::resolve` **once, at startup** |
| the running config | the `ConfigCell` (`app/settings.rs:72`) | the truth for this process | `ConfigCell::edit` (`settings.rs:113`) | the bar, the requests, every actor's `ConfigHandle` |

The header the file writes says the order itself: *"Resolution: CLI flags >
MUSH_\* environment > this workspace's session > this file > built-in
defaults."* (`userconfig.rs:49`). True, and it is precisely why the rest of this
audit exists: two of the five layers are files, mush writes both from the same
commands, and only one of them is ever read back.

### Which command moves which fact

| command | provider | base_url | model | window | key | session | home file | ack |
|---|---|---|---|---|---|---|---|---|
| `/provider <name>` / picker | **moves** | **moves** if the row owns its endpoint (`switches_endpoint`, `provider.rs:73`) | **moves** if the current id is not in the row's list (`app/mod.rs:4570`) | re-derived (`app/mod.rs:4575`) | **dropped** on a host move | dirty (`:4485`) | written (`:4490`) | `provider: deepseek · ctx ~500k` (`:4491`) |
| `/url <url>` | never | **moves** | kept | re-derived from the *model id* | **dropped** on a host move | dirty (`:4029`) | written (`:4049`) | `endpoint: <url> · ctx ~500k` (`:4032`) |
| `/model` (picker) | never | never | **moves** | re-derived from the new id (`config.rs:896`) | — | dirty (`:4604`) | written (`:4609`) | `model: <id> @ <endpoint> · ctx …` (`:4610`) |
| `/key [secret]` | never | never | never | never | **states it** | not marked | written under the current host (`:4076`) | `api key set (sk-a…6789) — saved to <path>` |
| `/context [N\|auto]` | never | never | never | **moves** | — | **only** the session (`:4104`, `flush_session`) | never | `ctx 32768 (set) — this workspace will remember it` |
| `/models` | — | — | — | — | — | no | no | `12 models from <url>/models` |

Three of the five commands write the machine-global file, and two of those three
say nothing about a file in their ack. That is the whole of **PK4**.

### What the human reads back

- **The facts line** (`app/screen.rs:1040`): `⌂ ws │ main 3s │ deepseek-flash @
  deepseek.com · ctx ~500k`. The endpoint cell is `Config::label`
  (`config.rs:1005`): the provider's short name (`display_endpoint`) *only* when
  the endpoint in force is the provider's own (`on_the_providers_own_endpoint`,
  `config.rs:984`), otherwise the raw `base_url`. **The provider's name never
  appears on the bar**, and neither does whether a key is in force.
- **The acks** above, in the status bar as `StatusKind::Info` — hidden after
  `INFO_TTL = 5s` (`app/mod.rs:649`, `:3129`).
- **`--print-config`** (`main.rs:876-898`): one row per fact, masked key
  (`sk-a…6789 (masked)`, `main.rs:791`), the provider on the row *below* the
  endpoint. The only surface where provider and endpoint are visible together.
- **The file**: a `_comment` header (`userconfig.rs:48-87`) explaining every
  field, then two key fields.

### The states you can be in, and what each surface says

| state | how you get there | what a request carries | what the screen says |
|---|---|---|---|
| `deepseek` at `api.deepseek.com` | default pick, `/provider deepseek`, or a file saying so | thinking on, `reasoning_effort: high`, 500k window for `deepseek-flash` | `deepseek-flash @ deepseek.com` — right, and the one state the bar names honestly |
| `deepseek` at a LAN URL | `/url http://rubendpc:8078` under a `deepseek` config | **still** thinking on, effort high, 500k window, vision yes (probe) | `deepseek-flash @ http://rubendpc:8078` — reads exactly like `custom` (PK3) |
| `custom` at a vendor URL | `/provider custom` from a `deepseek` config (the row keeps the endpoint) | thinking off, no effort, 500k window anyway (*vision and window follow the model id*, not the row) | `deepseek-flash @ https://api.deepseek.com` |
| a key per host, endpoint moved in this run | `/key` on host A, then `/url` host B | **no** `Authorization` | `no api key for this endpoint` — true |
| the same, moved back to A | `/url` host A again, same run | **still** no `Authorization`, though the file holds A's key | `no api key for this endpoint` — **false** (PK1) |
| a fresh start where the session names host B | last run saved B; the file's `base_url` is A | **no** key, even if the file has `api_keys["B"]` (probe) | the notice below, then nothing (PK1, PK8) |

---

## Ranked

| id | severity | finding |
|---|---|---|
| PK1 | major | the file's key for the endpoint **in force** is never adopted: the map is read before the session moves the endpoint, and no runtime command re-reads it — so `/url` back to a host with a stored key says `no api key for this endpoint`, which is false |
| PK2 | major | a save with no key in force states "none", so it writes a root `"api_key": null` **and** destroys a legacy flat key instead of migrating it — and the commands that do it are `/url`, `/model` and `/provider`, whose acks never mention the key (known; a fix is landing elsewhere) |
| PK3 | major | `deepseek` + a URL it does not own is an ordinary, silent state: `/url` never touches the provider, so the vendor's `thinking`, `reasoning_effort`, window and vision keep applying to the new host — while `--url` *does* clear the provider. Nothing on screen names the provider |
| PK4 | major | three commands write the machine-global file whose fields the workspace's session then outranks; a hand-edited `base_url` is silently dead in every workspace that has saved, and a `/url` typed in one workspace rewrites the default for every workspace that has not |
| PK5 | major | `/provider` changes three facts (endpoint, model, window) and its ack names one and a half; the picker's rows are bare names with no hint that one of them repoints the URL and the other keeps it |
| PK6 | minor | the acks speak three words for one endpoint, `saved to <path>` sits inside a sentence about a key the command did not write, and `/key`'s report cannot name the host it is about |
| PK7 | minor | two key shapes in one file (`api_key` flat, `api_keys` map) that the header explains but no surface reads back: which hosts have keys is answerable only by opening the file, and the resolve path's `road` is used for error text alone |
| PK8 | minor | every line that says the key was dropped is a five-second bar line that never reaches the transcript or `/notes`, and nothing durable marks "no key in force" — so a loss the human did not catch at the moment it happened is invisible forever |
| PK9 | minor | `/url`'s `ctx` is re-derived from the **model id**, not the endpoint, and printed on a line that starts with the new endpoint; the endpoint's own number arrives later, silently, and only if the server advertises one |

The sections below run in finding-number order; the table above is the order I
would fix them in (PK1 and PK2 are the two that cost a working credential).

---

## PK1 — the key the file holds for the endpoint in force is never adopted (major, proven)

**What is wrong.** The home config's map is looked up **once**, for the endpoint
that won the *CLI/env/home* arms — and then the session arm replaces that
endpoint and drops the key with it. Nothing looks the map up again: not the rest
of `resolve_with`, not any runtime command, because the app never re-reads the
home config at all — it keeps only the *path* (`app/mod.rs:846`,
`home_config: Option<PathBuf>`; the only reader of `userconfig::load` in the
whole binary is `main.rs:1012`). So:

- at **startup**, a workspace whose session names host B starts with no key even
  when the file holds `api_keys["B"]`;
- at **runtime**, `/url` back to a host whose key is in the file reports `no api
  key for this endpoint`, in as many words, while that endpoint *has* one.

The file's own header promises the opposite: *"switching the endpoint keeps
every other host's entry, and switching back re-adopts it"*
(`userconfig.rs:55`). It re-adopts it **on the next start of a workspace whose
home `base_url` matches** — a much narrower promise than the sentence.

**Code.** `crates/mush-core/src/config.rs:1160-1175`, whose comment states the
rule the code then breaks:

```rust
    // The file's key for the endpoint now settled: the map's entry for the
    // endpoint's host, or ... This waits for the arms above so the host looked
    // up is the endpoint that won, not one a later arm replaces.
    if !key_given && config.api_key.is_none() {
        let host = host_of(&config.base_url);
        let entry = home.api_keys.iter().find(|(stored, _)| stored.eq_ignore_ascii_case(host));
```

The session — the arm that *does* replace it — comes 60 lines later
(`config.rs:1229-1253`):

```rust
        if !url_given && !session.base_url.is_empty() {
            let was = config.base_url.clone();
            match checked_url(&session.base_url, "session's base_url") {
                Ok(url) => config.base_url = url,
                ...
            }
            if config.forget_key_if_host_changed(&was) {
                notices.push(format!("session: endpoint {} is another host — no api key for this endpoint; ..."));
```

The runtime half is `ConfigCell::edit` (`app/settings.rs:113-124`), whose one key
rule is `forget_key_if_host_changed` (`config.rs:963-968`) — it can only *lose* a
key:

```rust
        let forgotten = self.ui.api_key == key_was && self.ui.forget_key_if_host_changed(&was);
```

and the acks that follow it say what was lost, never what the file still holds
(`no_key_hint`, `app/mod.rs:4507`).

**Proof.** Two `--print-config` runs of this tree's own binary. Home config
`{"provider":"custom","base_url":"http://lan-a:8078","api_keys":{"lan-a:8078":
"sk-a-…","lan-b:9000":"sk-b-…"}}`; the workspace's session
`{"provider":"custom","base_url":"http://lan-b:9000","model":"m-b"}`:

```
  endpoint       http://lan-b:9000
  provider       custom
  session        read (0 messages)
  api key        (none)
  notice         session: endpoint http://lan-b:9000 is another host — no api key
                 for this endpoint; the key was not sent — /key <secret> sets one
                 (saved to …/config.json)
```

The file names a key for `lan-b:9000`, the endpoint in force is `lan-b:9000`, and
the line says there is none. The same shape with a **legacy flat** key (`"api_key":
"sk-legacy-…"`, no map) loses that key too: a flat key is host-less, so it is
read for the file's *base_url* and then dropped by the session's move — the one
key a hand-written file has, silently out of the run.

The runtime half is pinned, deliberately, by a test:
`a_key_on_each_of_two_hosts_survives_the_switch_between_them`
(`app/mod.rs:18742`) asserts both entries survive in the file and that the ack
says `no api key for this endpoint` — with the comment *"the run has no key for
the new host until the next start adopts its own, so the ack names the road back
rather than claiming a key it does not hold."* The test is honest about the
behaviour; the behaviour is the defect.

**Blast radius.** The human's credential, on the exact road the map was built
for. A workspace whose session pins an endpoint — which is every workspace after
its first save, since `session_snapshot` always writes `provider` and `base_url`
(`app/mod.rs:5199-5201`) — is one where the file's per-host keys are dead until
the file's own `base_url` happens to name the same host.

**Fix.** One lookup, at the endpoint that won: move the key arm in
`resolve_with` below the session arm, and give the app the same reader
(`UserConfig::key_for_host`, over the file it already knows the path of) called
from `ConfigCell::edit` on an endpoint move, instead of
`forget_key_if_host_changed` alone. `forget_key_if_host_changed` stays as the
*no-entry* case: an endpoint whose host the file does not name still drops the
key, which is finding C6's rule and must not be weakened.

**Acceptance test.** In `config.rs`: a session naming host B plus a home map with
a `B` entry resolves to that key and pushes **no** notice. In `app`'s suite: after
`/key` on A, `/url` to B, `/key` on B, `/url` back to A, the cell holds A's key
again and the ack says so.

---

## PK2 — the save that states "no key" writes `api_key: null` and destroys a flat key (major, known defect)

**What is wrong.** Two halves, both still at `HEAD`, both being fixed on another
branch; stated here because they are part of the UX a human meets.

1. `UserConfig::save_to` writes a root `"api_key": null` on **any** save of a
   file that has no flat key — in the `Keep` arm as well as the `Stated` one
   (`userconfig.rs:438`, `:470`, both
   `... .unwrap_or(Value::Null)`). A file whose keys all live in the map gains a
   `null` at the root on the first `/url`, `/model` or `/provider` — the exact
   thing the human reported ("a null key is written into the json at the root").
   The header (`userconfig.rs:50-52`) then explains a field the human never set
   and cannot use.
2. `App::persist_user_config` chooses `KeyWrite::Stated` whenever no key is in
   force (`app/mod.rs:4204`, `if self.key_stated || self.cfg().api_key.is_none()`).
   `Stated` with no key clears the flat field, so a **legacy flat key is
   destroyed rather than migrated** under its host — by a command whose ack says
   nothing about a key at all.

**Code.** `crates/mush/src/app/mod.rs:4204-4208`:

```rust
        let key = if self.key_stated || self.cfg().api_key.is_none() {
            KeyWrite::Stated
        } else {
            KeyWrite::Keep
        };
```

and `crates/mush-core/src/userconfig.rs:462-471` (the `Stated`, no-key path):

```rust
                        None => None,
                    };
                    if !map.is_empty() { fields.insert("api_keys".to_string(), Value::Object(map)); }
                    fields.insert("api_key".to_string(), flat.map(Value::String).unwrap_or(Value::Null));
```

**Not re-derived.** The brief names the fix as landing: `Stated` is only for a
save that states a key, and the flat field is written only when it holds one. No
probe here — this audit did not drive the TUI, and `save_to` has no non-TUI
caller.

**What the human sees.** Nothing. `/url`'s ack is `endpoint: … · ctx …` and
`/provider`'s is `provider: … · ctx …`; neither mentions the file, so the
sentence that would have to change is in the wrong command's mouth (PK6).

---

## PK3 — `deepseek` pointed at a LAN URL: reachable in one command, visible nowhere (major, proven)

**What is wrong.** A provider row is a *bundle*: a default endpoint, a model
list, a fallback window, and two request knobs (`thinking_by_default`,
`reasoning_effort_by_default`, `provider.rs:62`, `:68`). The row's
`switches_endpoint` (`provider.rs:73`) decides whether *selecting* it moves the
endpoint; nothing decides what happens when the endpoint moves *under* it. So
`/url` leaves the provider exactly where it was, and the knobs stay on:

- `/url http://rubendpc:8078` under a `deepseek` config → requests to the LAN box
  still carry `thinking: {"type":"enabled"}` and `reasoning_effort: "high"`, and
  the window is 500 000 tokens for `deepseek-flash`, and image parts are sent —
  while the endpoint is somebody's llama.cpp;
- `--url`/`MUSH_URL` on the same tree does the opposite: `resolve_with` step 4
  sets `config.provider = Provider::Custom` unless a provider was named
  alongside (`config.rs:1300-1306`). One act, two commands, two providers.

The state is invisible: the facts line prints `model @ endpoint` and the
provider name is painted nowhere but inside the picker, where it is the cursor
(`app/screen.rs:952`; `grep -n provider screen.rs ui.rs` finds only the picker).
The only surface that shows provider *and* endpoint together is
`--print-config`.

**Code.** `app/mod.rs:4541-4552` — `/url`'s whole write:

```rust
    fn switch_endpoint(&mut self, url: &str) -> bool {
        let moved = self.cell.edit(|cfg| {
            cfg.set_base_url(url);
            cfg.rederive_context();
        });
        self.models.clear();
        moved
    }
```

No provider. Contrast `config.rs:1300-1306`:

```rust
    if (cli.url.is_some() || env.url.is_some()) && !provider_given {
        config.provider = Provider::Custom;
    }
```

**Proof.** A hand-written session `{"provider":"deepseek","base_url":
"http://rubendpc:8078","model":"deepseek-flash"}` through `--print-config`:

```
  endpoint       http://rubendpc:8078
  provider       deepseek
  model          deepseek-flash
  vision         yes — image parts are sent
  window         500000 tokens (assumed from mush's model table)
  reasoning      high (the provider's default)
  thinking       on (the provider's default)
  reply cap      62500 tokens as max_tokens
```

Everything a vendor gets, aimed at a LAN host — and the screen a human actually
looks at says `deepseek-flash @ http://rubendpc:8078`, which is character for
character what `custom` at that URL would say.

**The mirror image.** `/provider custom` *keeps* the endpoint
(`switch_provider`, `app/mod.rs:4565-4567`), so from a `deepseek` default it
leaves the vendor's URL under the `custom` name: thinking off, no effort field —
but the window is still 500 000 and vision still yes, because those are looked
up by **model id** (`config.rs:858`, `provider.rs:259`). So the bundle is split
across two keys: two facts follow the provider name, two follow the model id, and
the human has no way to know which is which.

**Fix.** Make one rule out of `resolve_with` step 4 and have both roads take it:
a URL that is not the current provider's own endpoint means `custom` — i.e.
`Config::set_base_url` clears the provider when the endpoint leaves the row's
`default_base_url`, and `/url <the row's own endpoint>` is the only case that
keeps it. That deletes PK3 by construction and keeps `/provider` as the one road
to a vendor host. The alternative is the removal in the proposed model below:
stop reading `thinking_by_default`/`reasoning_effort_by_default` on the request
path at all.

---

## PK4 — which layer holds which fact: the session and the file both hold all three, and only one is read back (major, proven)

**What is wrong.** `provider`, `base_url` and `model` are each stored **twice**:
in the workspace's session (`session.rs:288-296`) and in the machine's file
(`userconfig.rs:135-139`); the same three commands write both (`/provider` →
`app/mod.rs:4485` + `:4490`; `/url` → `:4029` + `:4049`; the `/model` pick →
`:4604` + `:4609`); and at startup the session wins (`config.rs:1233`, and
`the_session_beats_the_home_config`, `config.rs:1660`). The file is never read
again after startup. Three consequences, none of them sayable from any surface:

1. **A hand-edited `base_url` is dead** in every workspace that has ever saved,
   because its session already names an endpoint. Probe: a file saying
   `http://lan-a:8078` and a session saying `http://lan-b:9000` resolves to
   `lan-b`, with a `session  read (0 messages)` row that names neither field.
   Nothing says *why* the file's value was ignored.
2. **A `/url` typed in one workspace rewrites the machine default** for every
   workspace that has no session — the ack says nothing about a file, and the
   test `an_unrelated_command_does_not_move_an_environment_key_into_the_home_config`
   (`app/mod.rs:18708`) asserts the field landed: `assert_eq!(saved.base_url,
   endpoint, "the command's own field landed")`. The complement is worse: two
   workspaces pointing at two hosts fight over one file, and each run of the
   first rewrites the other's default.
3. **No surface says which layer supplied a value.** `--print-config` has one
   row for the session and it says only `read (0 messages)` (`main.rs:880`);
   `describe_reports_the_notices_a_stored_layer_owed` (`main.rs:2537`) pins the
   notices, not the source.

This also contradicts the manual's own decision: `docs/mush.md:1566-1569`
records `[DECIDED]` against a config file in `.mush/` *because it "would make the
endpoint a workspace fact"*. The endpoint **is** a workspace fact — via
`session.json`, which `docs/mush.md:1053` lists honestly ("the conversation,
model, provider, endpoint, a stated window"). Only the file that says so lives in
the home directory, and it is the one being overwritten.

**Fix.** Pick one owner per fact and delete the other write. The model below
says which; the smallest version is that `persist_user_config` stops writing
`provider`, `base_url` and `model` (`app/mod.rs:4175-4208`) — the session is
already the workspace's record — and `save_to` takes the host for a key
explicitly (`KeyWrite` becomes `Host { host, key } | Keep`) rather than deriving
it from a `base_url` it no longer needs to write.

---

## PK5 — `/provider` changes three facts and reports one and a half; the picker's rows are bare names (major)

**What is wrong.** `apply_provider` sets the provider, moves the endpoint to the
row's own when `switches_endpoint` says so, may replace the model, and re-derives
the window — then says:

`app/mod.rs:4491` → `provider: deepseek · ctx ~500k`

The endpoint it just moved to and the model it just changed are both missing, so
the human learns that a name changed but not where their requests now go or which
model answers them. The endpoint is the fact they most need and the one `/url`'s
ack always carries — each ack names the half the other omits.

The picker is the same omission one step earlier (`app/mod.rs:4452-4466`):

```rust
            .map(|provider| PickerItem {
                id: Some(provider.name().to_string()),
                label: provider.name().to_string(),
            })
```

Two rows, `deepseek` and `custom`, with no hint that the first repoints the URL
to `api.deepseek.com` and swaps the model to `deepseek-flash`, while the second
keeps whatever URL is in force. `provider.rs:84` says the table's order *is* the
picker's order — so the table already knows the words (`default_base_url`,
`models`, `switches_endpoint`) and the popup has room for them (`ui.rs:530` is a
generic modal list that already carries `id · tokens` labels for models,
`app/mod.rs:4346`).

**What the test pins.** `a_runtime_switch_rederives_the_window`
(`app/mod.rs:17918`) is the behaviour, exactly: `switch_provider(Provider::DeepSeek)`
→ `base_url == "https://api.deepseek.com"`, `model == "deepseek-flash"`,
`context_tokens == 500_000`. Three facts move; the ack names the provider and the
number.

**Fix (cosmetic, one function).** One ack formatter for `/url`, `/provider` and
`/model`, printing the whole state that changed: `provider: deepseek →
https://api.deepseek.com · model deepseek-flash · ctx ~500k` for `/provider`,
`endpoint: http://… · provider custom · ctx ~500k` for `/url`. Same for the
picker rows: `deepseek — api.deepseek.com, deepseek-flash`, `custom — keeps your
endpoint`.

---

## PK6 — the acks' vocabulary (minor)

**What is wrong.** One endpoint is spelled four ways, and one of the acks'
tails promises a file on behalf of a command that did not write that field.

| surface | words (`file:line`) |
|---|---|
| `/url` ack, `--print-config` row, file field | `endpoint: http://lan-a:8078` (`app/mod.rs:4032`), `endpoint` + `base_url` (`main.rs:876`, `userconfig.rs:65`) |
| the facts line, when the provider owns it | `@ deepseek.com` (`provider.rs:110`, `config.rs:1011`) |
| `/models` and every fetch line | `12 models from http://lan-a:8078/v1/models` (`app/mod.rs:4287`) |
| `/provider` ack | no endpoint at all (`app/mod.rs:4491`) |

And the tails:

- `no api key for this endpoint — /key <secret> sets one (saved to <path>)`
  (`no_key_hint`, `app/mod.rs:4507`). Two things at once: an assertion about the
  run's key (false in PK1's case) and a promise about a file. The file named is
  the home config, *not* the endpoint — a human reading "saved to
  ~/.config/mush/config.json" learns where a key goes, not where their requests
  go.
- `saved to <path>` (`saved_to`, `app/mod.rs:4521`) appears in three acks. In
  `/key`'s it is a promise that the key is on disk. In `/url`'s and
  `/provider`'s it is bolted onto the key hint, and the save it belongs to writes
  `provider`/`base_url`/`model` — which the ack never mentions, while (PK2) the
  same save may be clearing the flat key.
- `/key` with no argument: `no api key — /key <secret> sets one (saved to <path>)`
  (`app/mod.rs:4058-4060`, pinned by `the_key_report_names_where_a_key_would_be_saved`,
  `app/mod.rs:18655`). It can only speak about "the key in force"; it cannot say
  *which host it is about*, and it says "no api key" on a machine that may hold
  three.

**Fix (cosmetic).** One sentence per fact, from one place: `endpoint` means the
base URL everywhere; the key hint says `no key for lan-b:9000 — /key …` (host,
not "this endpoint"); `saved to` occurs only in `/key`'s ack, and `/url`'s tail
says nothing about a file because it states nothing about one (PK4's fix).

---

## PK7 — two key shapes in one file, and no surface reads the map back (minor)

**What is wrong.** The home config holds the key twice over: a flat `api_key`
("the provider's secret", `userconfig.rs:50-52`) and a host → key map
(`api_keys`, `userconfig.rs:53-57`). `resolve` prefers the map entry for the
endpoint's host and falls back to the flat field
(`config.rs:1168-1175`, `the_flat_key_falls_back_for_a_host_the_map_does_not_name`,
`config.rs:1979`). The header explains both and then promises the re-adoption
PK1 shows is not there.

No surface reads the map back:

- `--print-config` prints the key **in force**, masked, with no host:
  `api key  sk-a…6789 (masked)` (`main.rs:791`, `:898`). The `road` variable that
  knows whether it came from the map, the flat field or `MUSH_API_KEY` is used
  **only** to build a refusal (`config.rs:1173` — `("home config's api_keys",
  …)` reaches `checked_key(key, road)` and nothing else);
- `/key` reports set-or-not, never which hosts;
- the facts line has no key cell at all (`screen.rs:1040`).

So "does this host have a key?" — the one question the map exists to answer — is
answerable only by opening the file. PK1 makes that question load-bearing.

**Fix (cosmetic + the PK1 change).** One row in the dump: `keys for  lan-a:8078,
lan-b:9000 (masked)`; and add the road to the `api key` row (`map`, `flat`, `env`)
since `road` already carries it. The flat field's removal is the proposed model's
item 5.

---

## PK8 — the loss is permanent and the notice lives five seconds (minor)

**What is wrong.** The whole vocabulary of loss is `StatusKind::Info`: `/url`'s
and `/provider`'s acks go through `say` (`app/mod.rs:2977`) and the startup
notices too (`main.rs:1169-1170`, `for notice in notices { app.say(notice); }`).
`status_line` hides an `Info` line after `INFO_TTL = 5s` (`app/mod.rs:649`,
`:3129`). A *stored* failure takes a different road — `stored_unreadable`
(`:3072`) puts it in the pane's foot, `/notes`, and the session itself — so the
code already has the right shape for "this outlives the moment", and the
key-dropped notice does not use it.

Consequence: a human launches a workspace whose session moved the endpoint, is
reading the screen (not the bar) for the first five seconds, and from then on
every request goes out with **no** `Authorization` header while every surface
says nothing about a key. The first evidence is the endpoint's 401, whose text
mush shows, and which says nothing about `/key`.

**Fix (cosmetic).** Send the key notices down the failure road (`note_error` +
the bar), and put the state on the bar permanently: `Config::label` gains
`· no key` / `· key` (`config.rs:1005`), which is one string and also answers
"is my key in force" at any moment of the run.

---

## PK9 — `/url` prints a window that belongs to the model, not the endpoint (minor)

**What is wrong.** `/url` re-derives the window and prints it in the same line
that announces the new endpoint (`app/mod.rs:4030-4039`):

```rust
                self.refresh_models();
                let mut line = format!("endpoint: {} · {}", self.cfg().base_url, self.context_label());
```

`rederive_context` (`config.rs:896`) goes to `fallback_context` (`config.rs:858`),
which is `known_context(model).unwrap_or(provider.fallback_context_tokens)` —
a fact about the **model id**, unchanged by `/url`. Probes: `deepseek-flash`
behind a LAN URL prints `ctx ~500k`; an unknown `m-b` prints `ctx ~32768`, the
`custom` row's fallback. So the number on that line is the *old* model's
number — the endpoint's own window arrives later, from the fetch `/url` starts,
via `adopt_models` → `adopt_advertised_context` (`app/mod.rs:4309-4311`), and
does so **silently** (`learn_context` paints nothing), and only if the server
advertises one at all (`http.rs:252`: `max_model_len`, `context_length`,
`context_window`, `n_ctx`).

`/model`'s arm is the honest twin: it re-derives from the *new* id and prints
`model: <id> @ <endpoint> · ctx ~…`, and the mark tells the truth — `~` table,
`≈` advertised, `≤` complaint, and nothing for a stated window (`(set)`,
`window_mark`, `app/mod.rs:972`). A stated window surviving both commands is
correct and documented (`a_runtime_switch_rederives_the_window`'s tail,
`app/mod.rs:17963`).

**Fix (cosmetic).** Drop `ctx` from `/url`'s line (it is not a fact about the
endpoint) and say it when the fetch answers — `12 models from … · ctx ≈32k` —
or keep it and mark it `(for deepseek-flash)`.

---

## What is already right

- **One spelling of "host"** for both the write and the lookup
  (`host_of`, `config.rs:582-588`), with case folded on both sides
  (`a_bad_key_in_the_map_is_refused_naming_the_file`, `config.rs:2018`; the map's
  key is `host[:port]`, and two ports on one box are two hosts).
- **The rule that a key never follows an endpoint to another host** exists, is
  one function, is reached by every runtime write (`ConfigCell::edit` is the only
  door) and is pinned in both copies (`a_host_change_forgets_the_key_in_both_copies`,
  `settings.rs:329`). PK1 is that it can only ever lose one.
- **A save never deletes another host's entry** (`userconfig.rs:447-472`), which
  is why `/key` on two hosts survives a switch in the file even though the run
  cannot use the second (PK1 again).
- **Every door checks the value**: `checked_url`/`checked_key` at argv, env, the
  file, the session and `/url`/`/key`, with the refusal naming the road and never
  echoing a secret (`config.rs:510-570`).
- **A new home file is `0600`** (`save_to`'s `atomic_write(… Fresh::Private)`,
  `userconfig.rs:483`).
- **The window's mark** (`~ ≈ ≤ (set)`) is the best idea on this surface: a
  stated value is visibly the human's. Nothing else here has such a mark.
- **`/context`'s ack** names the value, the road and the scope (*"this workspace
  will remember it"*) — the one ack on this surface that lies about nothing.

---

## The proposed model

**A provider is not a place; it is a name for a preset you apply once.** The
endpoint is a *workspace* fact, because a repository is what a human points
somewhere, and it belongs in `.mush/session.json` beside the model and the
window, written by `/url` and by nothing else at runtime. A key is a fact about
an endpoint's *host*, machine-global, in the home config's `api_keys` map,
written by `/key` and by nothing else — and, the half that is missing today,
**looked up by whatever changes the endpoint**, whether that is `/url`,
`/provider`, a start, or a workspace's stored session. A request is built from
four things and no fifth: the endpoint in force, the key for that endpoint's
host, the model in force, and what the human *stated*. The provider name never
reaches a request; it reaches `--print-config` and the picker, and only to say
what a command did.

The smallest set of changes that makes the code match it, ordered, each with
what it would remove:

1. **[behaviour] One key lookup, at the endpoint that won.** Move the key arm of
   `resolve_with` (`config.rs:1160-1200`) below the session arm (`:1229-1253`), and
   give the app the same reader — `UserConfig::key_for_host` — called from
   `ConfigCell::edit` on an endpoint move. *Removes*: the "no key until the next
   start" rule, PK1's false sentence, and the premise of
   `a_key_on_each_of_two_hosts_survives_the_switch_between_them`. Files:
   `crates/mush-core/src/config.rs`, `crates/mush-core/src/userconfig.rs`,
   `crates/mush/src/app/settings.rs`, `crates/mush/src/app/mod.rs`.
2. **[cosmetic] One ack for the whole state.** `provider: deepseek →
   https://api.deepseek.com · model deepseek-flash · ctx ~500k`;
   `endpoint: http://… · provider custom · ctx ~500k`; and the provider picker's
   rows describe what they do. *Removes*: the three `format!` sites and PK5.
   File: `crates/mush/src/app/mod.rs` only.
3. **[behaviour] `persist_user_config` stops writing `provider`, `base_url` and
   `model`; `save_to` takes the host for a key explicitly** (`KeyWrite::Host {
   host, key } | Keep`). *Removes*: three fields from a write, the machine file
   as a mirror of the last command typed anywhere, and the need for a `base_url`
   in the value at all. Files: `crates/mush/src/app/mod.rs`,
   `crates/mush-core/src/userconfig.rs`. *Policy*: whether the file keeps those
   three fields as hand-set defaults (my preference — they are what a fresh
   workspace starts from) or loses them.
4. **[behaviour] `Config::set_base_url` clears the provider** when the endpoint
   leaves the row's `default_base_url`, i.e. `resolve_with` step 4's rule applied
   at runtime. *Removes*: PK3, and the `deepseek` + LAN state entirely. Files:
   `crates/mush-core/src/config.rs`, `crates/mush/src/app/mod.rs`.
5. **[behaviour, policy] Delete the legacy flat `api_key`** after one migration
   that writes each key under its host. *Removes*: a field from `UserConfig`, a
   paragraph from the header, a fallback branch from `resolve` and the `Keep`
   arm's null. Files: `crates/mush-core/src/userconfig.rs`,
   `crates/mush-core/src/config.rs`. One-way; the human decides whether an old
   file is worth a backup copy or just a read.
6. **[cosmetic] `--print-config` names the layers**: which fields the session
   supplied, and `keys for lan-a:8078, lan-b:9000 (masked)`. *Removes*: PK4's
   mystery and PK7's "open the file". File: `crates/mush/src/main.rs`.
7. **[cosmetic] The bar carries the key state** — `Config::label` gains `· no
   key` / `· key` — and the key notices take the failure road instead of a
   five-second `Info` line. *Removes*: PK8. Files: `crates/mush-core/src/config.rs`,
   `crates/mush/src/app/mod.rs`, `crates/mush/src/main.rs`.
8. **[policy — the human decides] Whether the two request knobs stay on the
   provider row.** Removing `thinking_by_default` and `reasoning_effort_by_default`
   from the request path (`provider.rs:62`, `:68`; `Config::thinking_enabled`,
   `reasoning_effort`) makes "a provider is a preset you applied" true by
   construction: a request then carries a `thinking`/`reasoning_effort` field
   only because the human stated one. The cost is that a `deepseek` user gets no
   `reasoning_effort: high` by default. Keeping them means every screen has to
   *name the provider* (PK3's other fix), because a bundle that no surface names
   is a bundle nobody can predict.

---

## What not to build

- **More provider rows.** The table is a default bundle, not a catalogue; a row
  adds a word to a picker that cannot yet explain the two words it has.
- **A `/keys` command.** A second surface that lists secrets is a second place
  to get masking wrong; the answer is one more row in `--print-config` (item 6).
- **Per-model keys.** The key is minted for a host; a model does not authenticate.
- **A config file in `.mush/` that holds the key.** The store is workspace-local
  and one `git add -A` from a remote; the whole C-series exists because of that.
- **`/provider deepseek@http://…`.** That is `/url` followed by `/provider`; a
  compound argument is an order of operations the human has to learn.
- **Encrypting the home config.** A key stored beside the code that reads it is
  not a secret. `0600` on a new file is the honest boundary and it is already
  there.
- **A provider picker that fetches each row's endpoint.** A network call on a
  keystroke, and the answer would be about endpoints nobody is pointed at.
- **Auto-detecting the provider from the URL.** A guess about somebody's host —
  the reason `custom` exists and the reason `openai`/`openai-compatible` are not
  aliases (`provider.rs:160-168`).
- **A "profile" concept.** A profile is a provider row plus a key; that is the
  file, already.

---

## Not verified, and the blind spots

- Every ack above is read from the code, not captured from a screen: driving the
  TUI is out of bounds for this audit. Where a test pins the ack it is named; the
  rest are `format!` strings one line from the `say`.
- PK2's file shape was not reproduced: `save_to` has no non-TUI caller, and the
  human's own report plus the code (`userconfig.rs:438`, `:470`) are the claim; I
  did not drive a save.
- The `deepseek` + LAN probes use hand-written `session.json` files, so they
  prove the *resolution*, not the keystrokes that produce the state. The
  keystrokes are one runtime write (`switch_endpoint` + `rederive_context`,
  `app/mod.rs:4541-4552`) and the read of it is `switch_endpoint`'s: no provider
  is touched there, which is the whole of PK3;
  `switching_to_a_provider_forgets_the_key` (`app/mod.rs:17960`) is what pins the
  key half of that arm.
- Whether an endpoint's advertised window really arrives after a `/url` on the
  human's LAN host depends on that server (`http.rs:252`'s four field names); I
  tested resolution, not the wire.
- **Blind spot — no test drives the layers against each other.** No test has a
  session whose `provider`/`base_url` differ from the home config's *and* a map
  entry for the session's host; that is precisely PK1's hole, and it is why the
  behaviour looks intended (`a_key_on_each_of_two_hosts_survives_the_switch_between_them`
  asserts the ack is right about the run, not about the file).
- **Blind spot — the picker's labels are unpinned.**
  `the_provider_picker_picks_the_name_its_row_stands_for` (`app/mod.rs:18120`)
  pins the row the cursor opens on and what `Enter` does to the cell and the ack
  — not what a row *says*. PK5's diagnosis of two bare words is a reading, not a
  regression.
