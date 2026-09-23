# The second mechanism: six blind readers, one class of code

Six independent readers, each in its own worktree at `master@eddaa0f`, each given
one area and the same brief. They had no history of this project, were told not
to read `docs/`, commit messages or each other's worktrees, and worked
read-only and statically: no edits, no `cargo`, no running mush. Each was told
to hunt **one class** — code that buys little and costs attention forever:

1. two mechanisms answering one question (two layers, two units);
2. heuristics that buy a bound at the price of edge cases and drift;
3. state with two owners, or a field that restates something derivable;
4. code that exists only to serve other code that could itself go;
5. unreachable or redundant logic;
6. duplicated derivations that must be changed together.

and to answer, for every candidate, *who else already answers this* and *what
actually breaks if it is deleted* — with misses cheap and false positives
expensive.

| # | area | files |
|---|---|---|
| 30 | store & transcripts | `session.rs`, `transcript.rs`, `message.rs`, `text.rs`, `session_save.rs` |
| 31 | model loop & tools | `agent.rs`, `model.rs`, `prompt.rs`, `provider.rs`, `tools.rs` |
| 32 | tree & app state | `app/mod.rs`, `app/tree.rs`, `app/commands.rs` |
| 33 | jobs, ids, machine | `jobs.rs`, `ids.rs`, `machine.rs`, `clock.rs`, `events.rs`, `input.rs` |
| 34 | panes, keys, paint | `app/chat.rs`, `app/screen.rs`, `app/keys.rs`, `app/settings.rs`, `ui.rs` |
| 35 | git, config, transport | `git.rs`, `workspace.rs`, `config.rs`, `userconfig.rs`, `http.rs`, `attach.rs`, `main.rs` |

Nothing here is a decision and nothing has been changed: this is the input to
one. `✓` marks a claim re-checked by hand after the readers reported (the grep
or read is named); everything else is reader evidence only. Line numbers are at
`eddaa0f`.

---

## Tier 1 — deletions with no behaviour change

Each of these is a whole mechanism, field, field-copy or arm whose readers are
either tests or nothing at all.

1. **`Session.root` and `Session.updated` are written on every save and read
   nowhere.** `session.rs:191,205`; writers `app/mod.rs:2646,2656`. ✓ `rg` finds
   only the two writers, the struct, and JSON fixtures — `config::resolve` reads
   `model`, `base_url`, `provider`, `context` only. A stored path that can
   disagree with the directory the file was found in is a lie, not a fact, and
   `updated` says nothing the mtime does not. [#30]
2. **`ProviderSpec.needs_api_key` and `Provider::needs_api_key()`.** ✓
   `provider.rs:43,80,106,168` — five lines, zero readers in the workspace; a
   missing key is discovered by the endpoint's 401. [#31]
3. **`TOOL_NAMES`, `ORCHESTRATION_TOOLS`, `const fn names`.** ✓
   `tools.rs:81-98`; readers are four test lines in `prompt.rs`/`tools.rs`, one
   of which (`assert_eq!(all, TOOL_NAMES.to_vec())`, `:271`) asserts the
   derivation it was built from. [#31]
4. **`impl Display for ToolName`** (`tools.rs:74-79`) — one-line forwarding to
   `as_str`, read only by an assertion that restates the body. [#31]
5. **`Asked.tools`** (`model.rs:304`, set at `:579`) — ✓ one reader,
   `agent.rs:7988`'s test assertion, while `tool_schemas` beside it carries the
   same `request.tools` whole. [#31]
6. **`impl Display for CommandError`** (`commands.rs:80-88`) — ✓ the human site
   (`mod.rs:1603-1618`) matches the variants and spells the three texts itself;
   the impl is a second spelling with no caller. [#32]
7. **`-y` / `--yes`, `AUTO_APPROVE`, `auto_approve()`, the help paragraph, the
   `--print-config` row** (`main.rs:44-56,76,137,482-484,548,596-599,635`). ✓
   Its only reader prints back that it was given; the file's own doc says so
   ("Nothing asks yet … so the flag is only *recorded*"). Deleting it turns
   `mush -y` into an unknown option, which is a promise to settle, not a
   behaviour change. [#35]
8. **`Refused::Machine`'s "you hold the machine" arm** (`jobs.rs:472-476`). ✓
   Every `Held` is built by a *not-you* test (`:753` `holder != agent`, `:818`
   `*holder != owner`), so `held.agent == asker` is false at every construction
   site. [#33]
9. **60 vs 40 for the same held command** — `REFUSAL_COMMAND_COLUMNS = 60`
   (`jobs.rs:129`, used at `:488`) against a bare `40` in `agent.rs:3433`, the
   same field in the same kind of sentence. One number, two homes. [#33]
10. **`finish`'s `Option` and `watch`'s fallback** (`jobs.rs:1044-1048`,
    `1198-1200`) — one caller, which holds the job's own `Live`; the `None`
    cannot happen, and the fallback would hide a stuck record behind a line
    claiming age `0s`. [#33]
11. **`label(id)`** (`jobs.rs:135`) is `id.to_string()` with four call sites
    that already hold a `JobId`; beside it two hand-spelled `#{}` sites
    (`:484`, `:903`) on `u64` fields the `AgentId` newtype exists to prevent.
    [#33]
12. **The 10 ms poll cadence as a constant here and a literal there** —
    `jobs.rs:103` `POLL` (whose doc sentence is about the *other* loop) vs
    `agent.rs:3857`'s bare `Duration::from_millis(10)`. [#33]
13. **`next_agent`'s pool guard** (`ids.rs:111-121`) — `lose_agent` rejects
    `id >= counter` at push and the counter only grows, so the `while` always
    returns on its first iteration; the number the doc says is dropped is
    dropped by `reserve_agents`'s `retain` (`:148-153`), the opposite
    comparison, under a different lock. [#33]
14. **`input.rs`'s two self-excluded guards** — the `0..8` fixed-point bound
    (`:171`) and the `(lines, 0, 0)` fallback after `lines.get_mut` (`:118-125`),
    both excluded by the arithmetic in the same function. [#33]
15. **`serde_json` fallbacks that cannot fire** — `Session::save`'s
    `unwrap_or_else(|_| b"{}".to_vec())` (`session.rs:372`) reports success
    after replacing the conversation with `{}` (there is a working error channel
    two files away), and `stored_bytes`'s `unwrap_or(0)`
    (`transcript.rs:317`) makes an unmeasurable message free. [#30]
16. **`Session::read_from` / `load_from`** (`session.rs:324-331,349-355`) — a
    path-taking seam with one caller each, driven by no test (unlike
    `UserConfig::load_from`, which tests do exercise with other paths). [#30]
17. **`stored_bytes` re-implemented in the tests as `cost`**
    (`session.rs:687-694`) — the unit the cap is *defined* in, derived twice;
    widen `stored_bytes` to `pub(crate)` and delete `cost`. [#30]
18. **`parse_shortstat`'s `Option`** (`git.rs:612-643`) — every arm returns
    `Some`; the `None` comes from `git(...)` one line up in `diff_stat`. [#35]
19. **The `"git worktree failed"` string compare** (`git.rs:88-93,323-331`) —
    the file's own `GIT_UNAVAILABLE` documents why comparing a copy of a message
    is wrong; a reworded `run` silently retires this rewrite. [#35]
20. **The TLS arm's read timeout set twice around the handshake**
    (`http.rs:960-966`) — the handshake's slicing and the reads that follow
    both want `READ_SLICE`, so the second `set_read_timeout` sets the value the
    first one already did; it marks the phase boundary, not a change. [#35]
21. **`!matches!(kind, Interrupted)` in `dead_kept`** (`http.rs:282-288`) — the
    `!watch.cancelled()` on the line above already excludes the only producer.
    [#35]
22. **`elide`'s `kept == 0` arm and `min_kept`** (`screen.rs:720-737`) — the arm
    returns exactly what the trailing fallback returns; passing `1` at `:429`
    makes `min_kept` `1` at every call site, so the parameter goes too. [#34]
23. **`PickerPane.show_hint` and the empty-popup early return**
    (`screen.rs:273,604-616,651`; `ui.rs:225`) — `room` and `inner` are the same
    all-borders arithmetic, so the painter's own test already answers it. [#34]
24. **`AgentEvent::JobDone`'s `job` field** (`mod.rs:1270-1274`, built at
    `jobs.rs:1203`) — its only handler is `let _ = job;` and its only reader is
    a test matching `..`. [#32]
25. **`JOB_TITLE_COLUMNS`'s "the same bound as an agent's title"** — ✓ 30
    (`mod.rs:333`) against 24 (`tree.rs:205`), arrived in the same commit, with
    a test that reads the constant it asserts. Either one published bound or the
    sentence goes. [#32]
26. **`env.temperature` / `env.max_completion_tokens`** (`config.rs:213-214`,
    `605-608`, `623-624`) — no producer can build them; `.or(None)` and
    `|| false` are the identity. [#35]
27. **`Workspace::exists`** (`workspace.rs:63-65`) — ✓ its only caller is a
    test; `resolve` is the same thing with the reason attached. [#35]
28. **Four keys pinned by two tests each, and a test that copies `KEYS`'s help
    text into itself** (`keys.rs:770-812` vs `531-560`, `:855-866` vs `85-121`;
    `help("Ctrl-C") == "stop the focused agent"`). The only unique claim left is
    `assert_ne!` between two help lines. [#34]

## Tier 2 — one fact, two mechanisms: collapse

Bigger than a keyword, smaller than a redesign. Each is a decision.

1. **"This agent has work in flight" is written four times** — `kept`
   (`tree.rs:1196,1200`), `may_park` (`:1309,1317`), `App::in_flight`
   (`mod.rs:1337-1339`), `cancel_cursor_row` (`mod.rs:3096-3097`). `in_flight`
   already is the whole predicate (`phase.is_busy()` or a live job); the
   `jobs_live` pre-checks are a lock-avoidance, not a second rule. Move it to
   `AgentTree` and let all four call it. [#32]
2. **`busy_children` and `busy_counts` are the same count derived twice** —
   `tree.rs:1492-1497` vs `:1438-1446`, with `the_busy_map_agrees_with_the_per_id_scan`
   (`:2026`) existing only to check they agree; both real callers
   (`screen.rs:442`, `mod.rs:1544`) already build the map. ✓ [#32]
3. **`attach_edit` pre-answers `deliver`'s refusals and has drifted on the
   third** — `mod.rs:2014-2019` re-tests model and worktree, `deliver`
   re-tests both (`:1666`, `:1731`) and answers the dead root (`:1719`) *after*
   `push_message(ROOT, …)` at `:1680`. ✓ So a client's refused message is in
   the root transcript. Fix by making `deliver` fail without side effects and
   moving the box refill to its one human caller (`:1603-1607`). [#32]
4. **The environment is resolved twice** — `resolve` calls
   `Overrides::from_env_checked()`, which re-reads `MUSH_CONTEXT`,
   `MUSH_REASONING_EFFORT`, `MUSH_THINKING` on top of `Overrides::from_env`,
   while `Config::from_env` already applied five other `MUSH_*` vars
   (`config.rs:205-240,307-326,562-568,595-626`). Today's split means
   `Config::from_env`'s own doc is false for two of the seven variables and a
   malformed value is parsed twice under two policies. [#35]
5. **The `mush/` branch prefix is spelled three times** (`git.rs:185`, `:203`,
   `:521`) while the same file has `WORKTREE_DIR` because the *path* used to be
   three `format!`s and "a divergence between them is a worktree nobody can
   reclaim". [#35]
6. **`Record` is a two-state machine in three fields** — `live`/`line`/`tail`
   (`jobs.rs:275-291`) kept in step by `finish` alone, with two readers
   defending the fourth combination that cannot exist (`:914`, `:1007`). [#33]
7. **`status`'s headline is uncut, and the test pads the bound with a magic
   `256` bytes a job** — `record.command` rides on top of `STATUS_WINDOW`
   (`jobs.rs:1003`), while `job_title` (`mod.rs:346`) and
   `REFUSAL_COMMAND_COLUMNS` (`jobs.rs:129`) are the two cuts that already
   exist; `furniture = (MAX_JOBS + JOB_HISTORY) * 256` (`:1605`) is the test
   asserting a rule the code does not keep. ✓ [#33]
8. **`notes_report` re-derives the note mark and measures its lead in bytes** —
   `chat.rs:829-842` copies `NoticeKind::mark`'s table (`:126-133`) and spends
   `lead.len()` where `marked` (`:1355-1356`) measures columns; one home for the
   glyph, one unit for the width. [#34]
9. **`Reading::Holding`'s validity rule is spelled three times** —
   `chat.rs:926` (`scroll_by`), `:996-1001` (the title), `:1014-1016` (the
   body). They agree today; the third would slice a shorter transcript if it
   ever stopped agreeing. [#34]
10. **`Rank`'s unused `PartialOrd, Ord`** (`chat.rs:182`) beside the foot's
    parallel `worth` integers (`:1108-1144`) — the doc claims "the foot ranks
    through it" and the foot only ever asks `== Rank::Alert`. [#34]
11. **Three fields answer "which pane has the keyboard"** — `AgentsPane.focused`,
    `ChatPane.focused`, `BarPane.focus` (`screen.rs:175,227,253`), all written
    from one `self.focus`. [#34]
12. **The foreground slot is a second identity for a command, and the owner is
    stored twice** (`jobs.rs:385-397`, `611-614`, `685-706`) — the map's key is
    already the identity, one owner holds at most one foreground command, and
    `slot`/`next_slot` exist to police a stale release that needs two
    overlapping commands from one actor. [#33]
13. **`StoredStatus::Running`** (`session.rs:104-113`) — a fifth ending whose
    only reader (`app/mod.rs:597`) collapses it into `CutOff`, and whose writer
    (`:2625`) already collapses every in-flight phase into one wildcard. [#30]
14. **`Message::ensure_tool_call_ids`** (`message.rs:192-199`, called at
    `transcript.rs:80,143`) — the deserializer already assigns ids to every
    `Message` from the wire and from `session.json`. [#30]
15. **`Stopped`'s `Heard`/`Gone` arms** (`tree.rs:303-316`) — the only
    production reader asks `== Stopped::CutOff` (`mod.rs:2930-2933`); the phase
    mutation inside already distinguishes the other two. [#32]
16. **`Args` is `Overrides` plus three fields, copied by hand** (`main.rs:65-77`,
    `85-98`, `106-203`) — a new flag is added in three places today. [#35]
17. **The attach `id` is written, echoed and read by nobody** (`attach.rs:226`,
    `329`, `363`, `386`; `main.rs:351`; `mod.rs:1870`) — one response per request
    in order makes correlation invisible. Out-of-tree clients are the question,
    not the code. [#35]
18. **The caps' numbers are stated three times** — the constant, the
    compile-time assert `SESSION_ROOT_BYTES >= 64 * SESSION_AGENT_BYTES`
    (`session.rs:49`) and the restating test (`:959-966`), plus `bound_stored`
    (`:377-379`), a one-line forwarder with one caller. [#30]
19. **`Message::weight` vs `stored_bytes`, and the literal `3`** in
    `history_budget`/`bytes / 3` (`config.rs:405`, `app/chat.rs:567`,
    `prompt.rs:289`) — two units for transcript size; the readers keep both (two
    consumers, two real bounds) and flag only the triplicated literal, whose fix
    is a constant. [#30]

## Tier 3 — wrong, not redundant (found on the way)

1. **The `/model` bullet reads the cursor row, not the row being painted** —
   `screen.rs:621-628`: `current` is computed from `picker.items.get(picker.cursor)`
   inside a loop over `item`, so it is constant for the whole window: ✓ with the
   cursor on the current model *every* visible row wears `• `, and after one `j`
   none does. The `Provider` arm one line below compares `item`. The suite only
   asserts `contains("• test-model")` and never paints after `move_picker`.
   [#34]
2. **A refused attach message lands in the root transcript** — see Tier 2 §3;
   the drift is already real ([#32]).
3. **`worktree_add` refuses a workspace that is a subdirectory of a repository**
   — `git.rs:298` tests `dir.join(".git").exists()`, which is the only place in
   the crate that insists the workspace root *is* the repository root; every
   other call uses `git -C` and works. So the facts cell paints the branch and
   diff while every isolated spawn says "not a git repository". [#35]
4. **`--print-config` prints the model id raw** (`main.rs:561-565`) where
   `Config::label` (`config.rs:535-551`) exists precisely because an
   endpoint-chosen id that reaches a terminal can rename the window; the id can
   also be *adopted* from `/v1/models` and *stored* in the session, and
   `--print-config` reads the session. [#35]
5. **`status`'s schema promises "each child's state and title or branch"**
   (`prompt.rs:190`) while `child_listing` (`agent.rs:3208-3244`) prints neither
   a title (it lives only in the UI tree) nor, for a running or shared child, a
   branch. [#31]
6. **The truncation instruction enters the actor's transcript but not the UI's**
   — `agent.rs:1979-1982` pushes `TRUNCATION_INSTRUCTION` directly instead of
   through `push_line` (`:2681`), whose doc says a line that reaches `messages`
   alone is a line the human cannot see; the actor's transcript is then
   replaced at the next idle `Run` (`absorb`, `:1386`). [#31]
7. **The model picker's item string is a data format with three readers** —
   `mod.rs:2218` builds `"{id} · {tokens}"`, `screen.rs:626` parses it to decide
   a bullet and `mod.rs:2343` parses it again to decide the model to send. A
   model id that ever contains `" · "` makes both wrong together; the item
   should carry the id. [#34]

## The cap cluster (the thing this started from)

The store's own byte cut is the one place the sweep's verdict and mine differ,
and the difference is worth recording rather than resolving by rhetoric.

- Reader #30, hunting exactly this class, filed the cap's *restatement* (three
  copies of the numbers, the `cost` duplicate in tests) but **dismissed the cap
  itself as a second mechanism**: "`cap_transcript` vs `trim_history`: same
  object, different layer and unit; both keep system+task+newest and deleting
  either moves the problem, it does not remove a rule." It also kept
  `Session::truncated`/`Chat::root_dropped` ("two holders of one count … stale,
  not wrong") and kept `Message::weight` vs `stored_bytes` ("different
  consumers … both bounds are real").
- The mechanism the tree already has is a **count**: `CHILD_HISTORY = 50`
  (`tree.rs:519`) applied by `reap_history`, which is what makes the file
  smaller. With the cap gone the store's bound is that count, not bytes — and
  any tightening is one number.
- The reader's caveat about the count is filed honestly in its own report: with
  unlanded isolated children exempt from `kept`, a tree can exceed 50 rows
  (`MAX_WORKTREES = 70` bounds it), and the residual cost — the per-second deep
  copy of every kept transcript on the UI thread — is untouched by either
  mechanism.

## Considered and dismissed (the union)

Kept, so the next reader does not re-open them:

`Provider::ALL` vs `PROVIDERS` (the only variant enumeration) · `subagent_prompt`'s
`delegates` (mush-core cannot see `MAX_DEPTH`) · three `worktree_gone` predicates
(three entry points) · `wait_digest`'s `fresh_only` (a doc/behaviour seam, no
smaller code) · `end_note`'s unreachable arms (exhaustive-match table) ·
`RunUsage.total_missing`+`total` (a representation) · `Outcome::line`/`digest`
and `Work::status_line`/`digest` (two readers, two lengths) ·
`commit_subject`/`parse_commit_subject` (two sides of one grammar, round-trip
test) · `CUT_OFF_RUN = u64::MAX` and `NO_RUN = 0` (deliberately opposite
sentinels) · `AgentMsg` matched in three contexts · `MAX_DEPTH` in three readers ·
`compact_now`'s fallback endpoint (its comment is the lie, not the code) ·
`ActorState.shared` · `push_line`'s two deliberate bypasses · status expiry in
both `tick` and `status_line` (a sub-30 ms read window, each half tested) ·
`App::busy`'s short-circuit · `attach_worktree` vs `worktree_gone` (hand-edited
files only) · reclamation's propose/act recheck (a subprocess-cost filter over a
TOCTOU recheck) · `Phase::doing`/`tell_parent_running` (single-caller names) ·
`Landed::Discarded` (reads old session files) · `has_room` then `launch` (the
admission protocol) · `stopping`'s per-site `None` (the shared half is the
point) · `JobOutcome` (a reachable subset) · `Foreground::tail` (a trait method)
· `Waited` (a marker type at one call site) · `LOST_POOL = 8` · the two poison
policies · `ConfigCell.ui` beside `ConfigHandle.shared` · `App::term_width`
vs the frame's `Rect` · the pane's `▲N`/`▼N` vs ratatui's `List` ·
`Picker::hint()` (a third key list, but text, not a mechanism) ·
`picker_text_width`'s `- 6` · `Foot.counted` · `Chat::note`/`note_error` ·
`unread_children` walked twice (carrying the ids would be added state) ·
`draw_chat`'s cursor clamps · `MAX_WORKTREES = 70` vs `16 + 50` (a load-bearing
relation stated in prose, but deriving it is a new dependency) · the leading-CRLF
skip in `exchange` (defends a hypothetical server, fixes the same symptom) ·
`Reclaimable` deciding twice (deliberate) · `Config::new` (a test seam) ·
`thinking_stated`/`reasoning_effort_stated`/`uses_max_completion_tokens` ·
`Session::load` vs `read` · `Cli::detect` vs `parse_from` (two grammars, they do
diverge) · userconfig's silent defaults vs the session's `Unusable` · the
`_comment` header · `RepoStatus.dirty` vs `Stat::files` · `serve_with`'s injected
start · `scripts/session_blame.py`'s copied budget number.

## Undecidable without running anything

1. Whether one agent can hold two foreground commands (Tier 2 §12's premise).
2. Whether the `reserve_agents`/`next_agent` window is ever exercised from two
   threads.
3. Whether a real endpoint or proxy emits a leading CRLF before a status line.
4. Whether opening mush in a subdirectory of a repository is intended.
5. Whether any out-of-tree attach client reads the `id`.
6. Whether `MAX_WORKTREES = 70` was chosen against `MAX_AGENTS + CHILD_HISTORY`
   or just above the window.
7. Whether `--print-config`'s raw model line has ever fired.
8. Whether the `/model` all-bullets frame was ever seen (the size sweep passes
   with it).
9. Whether a poisoned config lock is reachable in a normal run.
10. Whether the missing `TRUNCATION_INSTRUCTION` emit changes any observable run.

## How I would sequence it (the reviewer's own list, after the readers)

1. **The two bugs first**, because they are small and live: the `/model` bullet
   ([#34] Tier 3 §1) and the attach refusal that lands a client's message in the
   root transcript ([#32] Tier 2 §3). Neither is a simplification, and both are
   the kind of thing this sweep exists to catch.
2. **The `deliver`/`attach_edit` collapse and the four-spelling in-flight
   predicate** ([#32] Tier 2 §1, §3): these *are* the class — N spellings of one
   fact replaced by one that has a name.
3. **The Tier 1 batch in one pass**: every item there is a grep-backed deletion,
   and the batch is what makes the census move rather than the diff.
4. **The two behaviour questions** — the subdirectory `.git` probe ([#35] Tier 3
   §3) and `--print-config`'s raw model id ([#35] Tier 3 §4) — are fixes, not
   subtractions, and each wants a decision about what mush promises before code.
5. **The cap is deliberately not in this list.** It is the open question from
   before this review, and reader #30's contrary judgement (recorded above) is
   the counter-argument to my recommendation, not a resolution of it.
