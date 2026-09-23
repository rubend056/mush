# docs vs code — every claim that is not true

An audit of the four surfaces that read as a contract — `docs/mush.md`, `README.md`,
`docs/refactor.md`, and the doc comments beside the code — against the tree at
`e9265267354c8d4dd2973f3caa5fbf5291c320eb` (the worktree's own base).

Method: read the four documents end to end, then check every claim a machine could
falsify — a number, a constant's value, a key, a file name, a quoted sentence, an
ordering, a *only*/*never*/*exactly*/*always* — against the code with `grep`, `sed`,
`python3` one-liners and `git log -S`. **No `cargo`, no test, no script was run**;
every check is a read of the tree. Where a claim is a test's own subject (the
`<!-- generated: … -->` blocks) the code's comment beside the fixture was read
instead of the block, and the block itself was left alone.

Evidence rule: every finding quotes the offending text **verbatim** with `file:line`
and quotes the code that contradicts it with `file:line`. Severity:
**major** — a reader acting on it does the wrong thing, or it hides a real defect;
**minor** — stale but harmless; **latent** — true today but a trap.

---

## Ranked

| id | severity | finding |
|---|---|---|
| D1 | major | README says every table in either file is generated and test-checked; the manual has five hand-written tables and four generated blocks |
| D2 | minor | §7 bills itself as "the whole budget" and omits `rustix` and `signal-hook`, both direct dependencies |
| D3 | latent | §5's `.mush/` listing omits `lock` — the one file whose replacement lets a second mush write over the conversation — and the `session.json.bak` copies |
| D4 | minor | `docs/refactor.md` §3.5: "the only modal state the keyboard has is the picker" — `keys::key` takes a second mode flag |
| D5 | minor | §8 puts `CHAT_DEADLINE` in `http.rs` (it is `model.rs`'s); README puts "the retry rule's own code" in `http.rs` too |
| D6 | minor | §4.5: the derived title means "two children never read the same" — it means no such thing |
| D7 | latent | §5.6's justification says "eight agents hold eight builds each"; `MAX_AGENTS` is 16 |
| D8 | minor | §10 lists four pty scenarios; `scripts/smoke.py` runs five (`--lock`) |
| D9 | minor | a doc comment names `App::reclaim_worktrees`, which exists in no revision of this tree |
| D10 | minor | §5.6 quotes the job kill line as `4h0m`; `short_age` prints `4h00m` |

---

## The findings

### D1 — "a table cannot drift silently" — five of them can, and one has

**The claim.** `README.md:45-46`:

> `and every table in either file is generated from the code and checked by a test, so a table cannot drift silently.`

**The code.** Only the blocks between `<!-- generated: … -->` markers are compared
with the code, and there are four of them: `docs/mush.md:349` (keys), `:402`
(commands), `:570` (marks), `:590` (frame) — each blessed and checked by the test
named in its own head (`app/keys.rs:1096`, `app/commands.rs:637`, `ui.rs:1175`,
`ui.rs:1209`; the writer is `ui.rs:947-996`, which rewrites that block and nothing
else). `docs/mush.md` also carries five **hand-written** markdown tables:

- `:184` `| Tool | Arguments | What it does |` (§3's ten-tool table)
- `:514` `| Question | What answers it |` (§4.5's three questions)
- `:820` `| A worktree isolates | Shared anyway |` (§5.6)
- `:977` `| Crate | Why |` (§7's dependency budget)
- `:1002` `| Metric | Target | Reality |` (§8's performance figures)

and no test reads any of them.

**What it costs.** The sentence tells the reader to stop checking exactly the
tables that carry numbers — and §7's has drifted already (D2), which is the class
of defect the sentence promises cannot happen. A reader who believes it will quote
the budget or the performance figures as checked facts.

### D2 — §7 is not "the whole budget"

**The claim.** `docs/mush.md:975` — `## 7. Dependencies (the whole budget)` — whose
table (`:979-987`, ten rows) names twelve crates: `ratatui`, `crossterm` (via
ratatui), `serde`, `serde_json`, `crossbeam-channel`, `unicode-width`,
`unicode-segmentation`, `unicode-truncate`, `tempfile`, `dirs`, `rustls`,
`webpki-roots`. §0 leans on it: `docs/mush.md:52` — "KISS is enforced by the
dependency budget of §7."

**The code.** `crates/mush/Cargo.toml` adds two direct dependencies the table does
not name:

```
25:# `extern "C"`; rustix is already in the tree under tempfile and rustls.
26:rustix = { version = "1", features = ["fs", "process"] }
32:signal-hook = "0.3"
```

(`signal-hook` matters enough to be argued for at `crates/mush/Cargo.toml:27-31` and
in `crates/mush/src/signals.rs:22-31`, "**Why `signal-hook`.** The dependency is
already in the tree…")

**What it costs.** A reader checking the budget — the file's own words make it the
one place a new dependency is weighed — sees twelve crates where the two manifests
declare thirteen, and the two missing ones are the two that keep the process
surface honest: `rustix` is the `flock(2)` in `crates/mush/src/lock.rs:47` and the
`kill -9 -pgid` in `crates/mush/src/machine.rs:356-359`, and `signal-hook` is the
handler in `crates/mush/src/signals.rs:48-50`. D1 is what makes this survive.

### D3 — `.mush/`'s inventory is missing the file that must not be touched

**The claim.** `docs/mush.md:694-701`:

> ```
> <workspace>/
>   .mush/
>     .gitignore     # contains a single line: *
>     session.json   # the conversation, model, provider, endpoint, a stated window, and stored failures
>     session.json.previous  # the conversation the last new chat cleared
>     wt/            # isolated agents' git worktrees (when used)
>     paste/         # pictures pasted into the chat (Ctrl-V, or a path from outside)
>     mush.sock      # the attach socket, while mush runs
> ```

**The code.** The store has a sixth direct entry, and the session module calls it
one of "the store's own files":

```
crates/mush-core/src/session.rs:20:  pub const LOCK_FILE: &str = "lock";
crates/mush-core/src/session.rs:34-43:  (LOCK_FILE, "the workspace lock, which one mush holds for as long as it runs —
        replacing it would let a second mush lock the new file and write over this conversation"),
```

and an unreadable session is set aside under a family of names the listing does not
mention either — `crates/mush-core/src/session.rs:355` `pub fn keep_unreadable`,
whose test pins `.mush/session.json.bak` (`:495`) and `.mush/session.json.bak.2`
(`:529`), described in the same file's own comment at `:26`
("the `session.json.bak`, `.bak.2`, … copies `keep_unreadable` sets aside").

**What it costs.** The block is headed "Persistence: everything in `.mush/`", and
what it leaves out is the one file whose *deletion or replacement* is the harm
(the lock's own reason says so). A human or a script taking the inventory as
complete — clearing "mush's own files", or writing a `.mush` cleanup — can drop the
lock while mush holds it, which is precisely the second-mush hazard the module
documents. `README.md:379-383`'s "What it writes" bullet has the same two gaps.

### D4 — the refactor plan says the keyboard has one mode; it has two

**The claim.** `docs/refactor.md:237-239` (§3.5, present tense, marked **Landed**):

> `The sketch's `mode` argument is not there: the only modal state the keyboard has is the picker, held as a bool, and the editor that had insert and normal modes is gone.`

**The code.** `crates/mush/src/app/keys.rs:452`:

```
pub fn key(focus: Focus, picker_open: bool, selecting: bool, key: KeyEvent) -> Intent {
```

with the second mode dispatched at `:490-495` (`if picker_open { return picker(key); }`
/ `if selecting { return select(key); }`) and documented as modal in its own right at
`keys.rs:502-508` ("The mode is modal the way a picker is, so this is the *whole*
keyboard while it is on…"). The manual
states it as a mode (`docs/mush.md:479-481`: "While it is open the mode has the
keyboard — a letter is not typing — and `Tab` leaves it for the pane cycle"), and the
record agrees (`docs/findings.md:4430-4432`: "`keys::key` asks the caller for
`Chat::selecting()` and routes the whole keyboard to `fn select` before either pane").

**What it costs.** A reader planning a key change from §3.5 counts one mode and
routes a binding through `picker_open` alone; the select mode swallows that key.
The sentence is the plan's own acceptance note, so it is what the next agent
trusts.

### D5 — the deadline and the retry rule are `model.rs`'s, not `http.rs`'s

**The claim.** `docs/mush.md:1019-1021`:

> `the phase deadlines and the read slices that let Ctrl-C stop a model that has not answered live in `crates/mush/src/http.rs` (`CONNECT_TIMEOUT`, `WRITE_TIMEOUT`, `RESOLVE_TIMEOUT`, `LIST_READ_TIMEOUT`, `CHAT_DEADLINE`).`

and `README.md:343-344`:

> `The phase ceilings, the read slices that make Ctrl-C work, and the retry rule's own code: `crates/mush/src/http.rs`.`

**The code.** Four of those five names are `http.rs`'s (`:32`, `:38`, `:49`, `:958`);
`CHAT_DEADLINE` is not:

```
crates/mush/src/model.rs:256:  pub const CHAT_DEADLINE: Duration = Duration::from_secs(600);
```

`http.rs` never names it in production code — the deadline arrives as a
`timeout: Duration` argument, and the only mentions in the file are a test import
(`crates/mush/src/http.rs:1363  use crate::model::CHAT_DEADLINE;`) and one test
(`:3101`). The README's "retry rule's own code" is `model.rs` too: `RETRY_ATTEMPTS`
(`crates/mush/src/model.rs:248`), `pub fn retrying` (`:308`).

**What it costs.** These two documents name the home of the numbers a reader is
most likely to go and change (the 600 s deadline, the three attempts). Grepping
`http.rs` for either comes back with the test halves only, and the reader concludes
the constant was renamed rather than found it one file over.

### D6 — "two children never read the same"

**The claim.** `docs/mush.md:532-533` (§4.5, R1):

> `The title is derived from the brief (`deep.txt`, `lexer`), so two children never read the same.`

**The code.** `crates/mush/src/app/tree.rs:502-525` (`AgentNode::title`): the given
`title` if there is one, else the brief's first line through `first_line`, then the
**first path-like word**, else the **first non-filler word** — a pure function of one
brief. Its own doc says only what it is for: "it tells two children apart at a
glance" (`tree.rs:491-493`), and its test pins that two briefs *identical for their
first twenty columns* derive two different titles (`tree.rs:2307-2330`, the test at
`:2310`). Nothing makes the map injective: two briefs whose first path-like word is the same — the
common shape `create a file called deep.txt …` — paint the same title.

**What it costs.** A reader trusts the uniqueness as a screen fact (two rows to
tell apart by title alone) and stops giving a `title` where it matters. The word
doing the damage is *never*, in a document whose other numbers are checked.

### D7 — "eight agents hold eight builds each" — `MAX_AGENTS` is 16

**The claim.** `docs/mush.md:842-843` (§5.6, why the job budget is machine-wide):

> `a job does **not** count against `MAX_AGENTS`, and the budget is one machine-wide cap rather than a per-agent one, since a per-agent cap would let eight agents hold eight builds each.`

**The code.**

```
crates/mush/src/agent.rs:200:  const MAX_AGENTS: u64 = 16;
crates/mush/src/agent.rs:4729:  if ctx.live.load(Ordering::SeqCst) >= MAX_AGENTS {
```

and `crates/mush/src/jobs.rs:71` `pub const MAX_JOBS: usize = 8;`, whose own doc
(`jobs.rs:68`) carries the same sentence: "a per-agent cap would let eight agents
hold eight builds each, which is the situation the cap exists to prevent."

**What it costs.** The arithmetic is the whole argument for the design, and it is
off by two (`16 × 8 = 128` builds, not 64). A reader re-deriving the worst case from
the constant beside it gets a different number than the sentence, which invites the
assumption that `MAX_AGENTS` is 8 — it is not, and the number in the sentence looks
like it was copied from a tree where it was.

### D8 — the pty suite has five scenarios; §10 names four

**The claim.** `docs/mush.md:1094-1096`:

> `Scenarios: agent (needs a model), resize (needs nothing), cancel (needs nothing — a socket that accepts the chat request and never answers must be abandoned by a single Ctrl-C, which is only observable from outside the process), and sigterm (needs nothing).`

and `:1116` — "the unit tests, and the pty resize and cancel scenarios are the
whole gate".

**The code.** `scripts/smoke.py` has five, and the fifth runs by default:

```
scripts/smoke.py:10:   python3 scripts/smoke.py [BINARY] [WORKDIR] [--agent|--resize|--cancel|--sigterm|--lock]
scripts/smoke.py:575:  parser.add_argument("--lock", action="store_true", help="run only the lock scenario")
scripts/smoke.py:596-597:  if both or args.lock:
                               passed &= scenario_lock(binary, base / "lock")
```

**What it costs.** Small but real: the manual is the list of what runs, and a
reader counting the pty coverage (or deciding which scenarios to keep green before
a change to `lock.rs`) misses the one scenario whose subject is the lock — the
scenario most likely to be the only end-to-end check of that file.

### D9 — a doc comment names a function that exists nowhere

**The claim.** `crates/mush-core/src/git.rs:975-977`:

> `The question is the sweep's own for every worktree a tree has published (`WorktreeFacts`): the node's base — its parent's branch, or `HEAD` — and its fork revision, exactly the pair `App::reclaim_worktrees` hands [`reclaimable`].`

**The code.** `App::reclaim_worktrees` is not defined in any file:
`grep -rn "reclaim_worktrees" crates/` answers that one line, and
`git log -S "fn reclaim_worktrees" -- crates` is empty (the string enters the tree
only in this comment, with `efd5213`). The caller that hands `reclaimable` that pair
is `App::refresh_git` (`crates/mush/src/app/mod.rs:1233`, the
`git::reclaimable(&root, id.0, &base, fork.as_deref())` at `:1301`), whose answers
`App::sweep_worktrees` (`:1358`) applies; the other member of that family a reader
will find by grepping is `App::reclaim_isolated` (`:1484`), the pass that takes
landed worktrees on startup and on the human's key.

**What it costs.** A reader who wants the sweep's walk — the next fixer of the
spawn-cap arithmetic the record keeps open — has no such function to open; the
name is also repeated in the record (`docs/findings.md:559`, `:8355`), so a grep
"confirms" it twice and the wrong-function chase starts from two documents.

### D10 — the quoted kill line cannot be painted

**The claim.** `docs/mush.md:848-849` (§5.6):

> `and the kill says so — `#c3 killed: it ran past the 4h ceiling · 4h0m · cargo run``

**The code.** The age in that line comes from one function, which pads minutes to
two digits:

```
crates/mush/src/app/mod.rs:382-388:
pub fn short_age(elapsed: Duration) -> String {
    ...
        _ => format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60),
```

so the line the code prints reads `4h00m` — which is what the code's own test asserts
(`crates/mush/src/jobs.rs:1894`: `"#c3 killed: it ran past the 4h ceiling ·
4h00m · cargo run"`) and what the record quotes (`docs/findings.md:1566`). The
format string is `jobs.rs:326-329`.

**What it costs.** A quoted output line is how a reader greps a log. `4h0m` matches
nothing in a transcript, and the reader concludes the kill was never taken.

---

## Appendix

- **A1** — `docs/refactor.md:398-400` ("`ui.rs` is 298 lines of column arithmetic … the draw sweep asserts the painted text over fifteen sizes × fourteen states") is stale on both numbers: `crates/mush/src/ui.rs` is 1,218 lines (466 before its first `#[cfg(test)]`), and `SWEEP_SIZES` is fifteen (`crates/mush/src/app/mod.rs:15851-15867`, correct) while `sweep_states` pushes a `Sweep` at sixteen sites, one of them inside a loop over four fold variants (`crates/mush/src/app/mod.rs:16117-16148`, the push at `:16139`) — *this is the `ui.rs`/`app/mod.rs` area a child is editing right now, so re-count it before acting.*
- **A2** — `crates/mush/src/app/screen.rs:873`'s own sample line reads `master ±3 +12 −3 │ qwen2.5-coder`, with a space between the two halves of the delta; the painter writes them joined — `crates/mush-core/src/git.rs:33-39` `format!("+{}−{}", self.added, self.removed)`, pinned as `+12−3` by `git.rs:1226-1235` and painted that way in both generated frames (`docs/mush.md:619`, `README.md:39`). *`ui.rs`/`screen.rs` are in flux.*
- **A3** — `docs/refactor.md:235` sketches the keymap entry point as `keys::key(focus, picker_open, key) -> Intent`; the real function takes a fourth argument, `selecting` (`crates/mush/src/app/keys.rs:452`). Same root cause as D4, one line.

---

## Suspect and held

Claims checked that turned out **true** — recorded so nobody re-chases them, with the
check used. All checks are greps/reads at the base commit.

- **The ten tools and the nine-tool leaf** (§0, §3, README): `ToolName::ALL` is ten
  variants in schema order (`crates/mush-core/src/tools.rs:49-60`), `ORCHESTRATION` is
  `spawn_agent` alone (`:66`), `TOOL_NAMES` at `:114`, asserted at `:503-504`.
- **The caps** (§2, §3): `CMD_TIMEOUT_SECS = 120` and `CMD_CAP = 16_000`
  (`crates/mush-core/src/lib.rs:43-45`), `CMD_OUTPUT_LIMIT = 8 * 1024 * 1024`,
  `CMD_DETACH_AFTER = 60 s`, `JOB_MAX_AGE = 4 h`, `MAX_JOBS = 8`
  (`crates/mush/src/jobs.rs:71-148`), `READ_FILE_CAP = 32 MB`, `IMAGE_FILE_CAP = 2 MB`,
  `SEARCH_FILE_CAP = 2 MB` (`crates/mush-core/src/workspace.rs:41-59`).
- **§2's "120 s timeout" and §5.6's detach are not a contradiction**: a foreground
  command auto-detaches at `CMD_DETACH_AFTER` only when the registry has room, and
  the 120 s timeout is the whole story exactly when it does not
  (`crates/mush/src/agent.rs:6309-6325`, and `jobs.rs:64-69`).
- **The two truncation sentences** (§2): verbatim at
  `crates/mush-core/src/workspace.rs:1942` and `:1963`, the read window's at `:1114-1124`.
- **The lock's refusal and the detach line** (§5.6): `Refused::lock_road`
  (`crates/mush/src/jobs.rs:826-829`) matches the manual word for word, and
  `[still running — detached as {id}; you will be told when it finishes]` is
  `crates/mush/src/agent.rs:6398`.
- **The loop guard** (§2, "five rounds over"): `LOOP_ROUNDS = 5`
  (`crates/mush/src/agent.rs:52`) with `repeats >= LOOP_ROUNDS` (`:3494`) — five
  *repeats* of a batch, the sixth identical batch being the one that stops the run
  (`count_round`, `:3073-3090`). Read as "five repeats", the sentence is right.
- **The default endpoint, the windows, the vision row** (§0, §3, README):
  `ProviderSpec::default_base_url = "http://rubendpc:8078"` for Custom
  (`crates/mush-core/src/provider.rs:115`), `500_000` for both DeepSeek models with
  `vision: true` on `deepseek-flash` alone (`provider.rs:88-110`),
  `DEFAULT_CONTEXT_TOKENS = 8192` (`crates/mush-core/src/config.rs:28`), and the
  advertised keys in that order — `["max_model_len", "context_length",
  "context_window", "n_ctx"]`, top level then `meta` (`crates/mush/src/http.rs:196-201`).
- **The vendor-literal guard** (§4.5): the table's own needles were re-run over every
  `crates/*/src/**/*.rs` production half in a `python3` script — 0 hits outside
  `provider.rs` — so "No other file names a vendor" holds today.
- **The screen tiers** (§4.5, §11.9): floor `40×10` (`crates/mush/src/app/mod.rs:396-403`),
  compact at `w<80 || h<20` and the two-line bar at `h≥24`
  (`crates/mush/src/app/screen.rs:158-168`, `:374`), transcript cap 110 and agent pane
  cap 50 (`screen.rs:34`, `:48`), `PAGE = 10` everywhere a page moves
  (`crates/mush/src/app/keys.rs:46`, used at `:530-623`, `:804-816`, `:904-905`).
- **The bar's marks and words** (§4.5): `window_mark` is `""`/`~`/`≈`/`≤` for
  stated/table/advertised/complaint (`crates/mush/src/app/mod.rs:738-745`) and
  `WindowSource::words` spells the same four (`crates/mush-core/src/config.rs:219-226`).
- **The box's own lines** (§4): `cleared the box and 2 images · Ctrl-Z puts it back`
  (`crates/mush/src/app/chat.rs:546`, asserted at `app/mod.rs:10325`),
  `copied 12 lines from #1's reply — 1,284 bytes` (`chat.rs:2027`), the attachment
  rows `▣ path (format · size)` capped at three with the last counting the rest
  (`crates/mush/src/app/screen.rs:116-140`, `MAX_ATTACHMENT_ROWS = 3` at `:68`),
  and the folded block's eight rows (`chat.rs:3282-3286`, `Fold::DEFAULT`).
- **The views are not stored** (§4): `Ctrl-T`/`Ctrl-O`/`Ctrl-F` set `dirty_screen`
  only (`crates/mush/src/app/mod.rs:3771-3809`) and `Chat::clear` (Ctrl-N) leaves
  `reasoning`/`output`/the fold alone (`crates/mush/src/app/chat.rs:1386-1413`,
  `:940-1000`).
- **The four `#[ignore]`d tests** (§10): four attributes in the tree — three live
  endpoint tests in `http.rs` (`:3044`, `:3062`, `:3118`) and the idle-box frame test
  in `app/mod.rs:11508` — as §10 describes them.
- **§10's counts**: fifteen sizes in `SWEEP_SIZES` ✓; "more than twenty scripted
  scenarios" ✓ (34 `#[test]`s call `spawn_scripted`); "a run that goes past two
  hundred turns" ✓ (220 rounds, `agent.rs:18128-18136`); `scripts/screen.py` prints
  six sizes, `200x50 … 30x8` (`screen.py:272`) ✓.
- **A fold of `system + one message` is refused** (§3): `messages.len() <= 2` →
  `nothing_to_compact` (`crates/mush/src/agent.rs:3726-3727`, sentence at `:3661`).
- **`--print-config`'s promise** (§5): the flag returns before `open_workspace`,
  `ensure_mush_dir`, `lock::acquire` and the attach socket
  (`crates/mush/src/main.rs:986-1000`).
- **The store's names that the listing does get right** (§5): `.mush/.gitignore` is one
  line `*` (`crates/mush-core/src/session.rs:61`), `session.json.previous` (`:89`),
  `mush.sock` (`crates/mush/src/attach.rs:36`), the paste name and extensions
  (`crates/mush-core/src/workspace.rs:1876`, `:1785-1788`).
- **The git snapshot's refresh rule** (§8): the two-second tick while busy
  (`crates/mush/src/app/mod.rs:1817-1824`) and the ten-second age label
  (`GIT_STALE`, `crates/mush/src/app/screen.rs:56`, applied at `:986`).
- **The release profile** (§8): `lto = "thin"`, `codegen-units = 1`, `strip = true`
  (`Cargo.toml`, `[profile.release]`).
- **The omitted crates are absent** (§7): none of `tokio`, `reqwest`, `clap`, `ropey`,
  `notify`, `anyhow`, `blake3`, `diffy` appears in `Cargo.lock` (115 packages).

## Known, not re-reported

- The record's open rows **H73–H80** (scratch/sweep legacies, the pty workdir, the
  census's word reading, the emphasis braid, `is_row`, the `ceil(width/4)` column
  cap) and **A19** (three sentences that claim more than the code does) — read and
  skipped; none of the findings above is one of them.
- The documented flakes of §8.102, and the ledger's own row-shaped facts about files
  (`H66` `http.rs`'s "quarter", `H68` `STATUS_WINDOW`/`Live::kill`, `H69` `theme.rs`'s
  count) — checked that the tree and the record now agree; nothing added.
- **Row-shaped claims I verified rather than re-reporting**: `R52`'s two spellings of
  the hours arithmetic do exist as it says (`crates/mush/src/jobs.rs:328` and
  `crates/mush/src/agent.rs:6613`, the latter inside `end_note`, `agent.rs:6575`), and
  the ledger's ✅ rows I sampled name functions that are in the tree.
- **The two areas a child is editing right now** — `app/mod.rs`'s restore path and
  `app/tree.rs`/`ui.rs`'s placement — were read but not pinned to a line: D6 lives in
  `tree.rs::title` and A1/A2 in `ui.rs`/`screen.rs`, and each says so where it stands.
