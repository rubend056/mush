# The TUI, audited: the state machine and the painting, read blind

The ten files that are the human's screen and the state behind it — `app/mod.rs`,
`app/chat.rs`, `app/keys.rs`, `app/commands.rs`, `app/screen.rs`,
`app/settings.rs`, `app/tree.rs`, `ui.rs`, `theme.rs`, `input.rs` — read end to
end, test modules included, with four child blinds on the outer files and the
hub (`mod.rs`, `keys.rs`) read by this one. Every run quoted below is a real run
on this machine (Linux, `cargo test -p mush --bin mush <filter>` in the debug
profile; the package has **no lib target** — `cargo test -p mush --lib` answers
"no library targets found", so every probe ran against the bin). All probes were
appended as a throwaway `#[cfg(test)] mod audit_probe`, run, and deleted; the
tree is back at `5889590` with an empty `git status`.

**Blind** means no doc comment is taken as true: each is a claim attacked, and
the code is what it does. Nothing here re-reports a closed row: B1–B16 and
C1–C12 are read and cross-referenced ("as in C6"), and `docs/findings.md`
§8.44–§8.50 with ledger H41–H48 are read — the three findings that sit *inside*
those mechanisms (D12, D13, D14) are named as outside the closed rows, in the
words of the row they leave alone.

**The two blockers.** A fold can panic the frame while the select mode is up
(D1), and two rows of one id panic the pane on a keypress (D2). Both were
re-proven in this worktree, not taken on a child's word; their runs are quoted.

---

## Priority order

Ordered by what the human loses, not by line count.

1. **D1 — a fold under the select mode panics the frame.** The automatic fold
   fires at nine tenths of the budget, so this is reachable in a long ordinary
   session with one `Ctrl-Y` pressed at the wrong minute; the panic is on the UI
   thread and takes every actor, worktree job and the draft with it.
2. **D2 — two stored rows of one id lose a painted row, and any `G` then panics
   the pane.** A repository can commit `.mush/session.json`; the store is
   hand-editable; the first symptom (a hidden agent) is silent.
3. **D3 — `agent.id + 1` panics at startup** from a session file naming
   `u64::MAX` and from an unmerged `mush/18446744073709551615` branch. In debug
   builds (the suite, `cargo run`) mush cannot start in that workspace at all.
4. **D4 — `/url`, `/model`, `/models`, `/provider` and `Ctrl-P` block the UI
   thread on the endpoint: measured 10.036 s.** The screen freezes and the
   keyboard stops answering while an agent may be running — the one moment the
   human needs the frame and the stop key.
5. **D5 — the message box on a short terminal hides the line being typed and
   puts the cursor on a picture.** 40×12 with three attachments: three `▣` rows
   painted, the draft invisible, the cursor inside `shots/shot2.png`.
6. **D6 — a stored session's `base_url` re-points the endpoint and the home
   key follows it.** The human's provider key is on the wire to a host a file
   chose, seconds after `mush` starts in a cloned repository. (C6's rule on a
   road C6 did not name; the site is `mush-core/src/config.rs`, so it belongs to
   whichever wave owns that ruling.)
7. **D7–D10 — the tree's small lies**: the cursor slides onto another agent on a
   reap; a reclaimed worktree keeps its `+3−1` on the row and in the title's Σ;
   an orphan is indented by its stored depth; a status can erase `⊘ cancelling…`.
8. **D11–D13 — the paint at the edges**: the chat pane's title is cut without an
   elision rule (and the select mode's count and `/notes` go with it); a resize
   clips an open `/notes`/`/help`; zen's Chat arm moves and grows the box, and
   the `at_every_size` pin checks two sizes.
9. **D14–D18 — the select mode and the reading**: a fence-only reply is an
   invisible turn; a held reading resurrects after a fold; an agent whose prompt
   was published but has no transcript weighs 0; the News doc row is false for a
   stop; a focus change under the mode makes `Enter` copy nothing, silently.
10. **D19–D26 — the small ones**: a control character in a URL reaches the
    request line; a paste that merges graphemes swallows one Backspace; the
    config cell's "copies cannot differ" is false on the handle road; an empty
    key is "set", "(none)" and an empty Bearer; `/help` at 40 columns; `Ctrl-Y`
    under zen; the "several agents running" line's `Enter`; `Msg::Git`'s missing
    conversation stamp.

---

## Census

Everything below was read end to end; "test" counts from the first
`#[cfg(test)] mod tests` to EOF, so production + doc comments are what is left.

| file | lines | of which test |
|---|---:|---:|
| `crates/mush/src/app/mod.rs` | 15,831 | 11,417 |
| `crates/mush/src/app/chat.rs` | 4,974 | 2,220 |
| `crates/mush/src/app/tree.rs` | 2,969 | 1,219 |
| `crates/mush/src/app/screen.rs` | 1,712 | 474 |
| `crates/mush/src/app/keys.rs` | 1,117 | 496 |
| `crates/mush/src/theme.rs` | 809 | 413 |
| `crates/mush/src/ui.rs` | 720 | 295 |
| `crates/mush/src/app/commands.rs` | 472 | 196 |
| `crates/mush/src/input.rs` | 377 | 150 |
| `crates/mush/src/app/settings.rs` | 333 | 131 |
| **total** | **29,314** | **17,011** |

Twenty-six findings: **2 blocker, 4 major, 20 minor** (two suspected: D10 and
D26; every other finding carries a real run or a code path spelled out).

---

## D1 — a fold under the select mode panics the frame

**Severity: blocker. Proven (re-proven here).** `crates/mush/src/app/chat.rs:1768`,
with `select_body` at `chat.rs:1695` and `clamped_cursor` at `chat.rs:1442`.

The key road clamps the select cursor; the paint road does not. `select_apply`
(the one place a mode key runs) begins with `clamped_cursor`, so a key after a
transcript shrank is safe. `select_body`, reached from `painted` on every frame,
uses `select.cursor` raw; when the cursor no longer names a row, `top_at_bottom`
(`chat.rs:1800`) asks `chunk` for the message at that index, and `chunk` does
`let message = &self.transcript(on)[index];` (`chat.rs:1768`). A fold replaces
the transcript — the `AgentEvent::Compact` arm in `mod.rs` calls
`self.chat.replace_transcript(id, vec![carried])` — and nothing drops the mode.

My own probe (a real run, probe deleted): eight replies, `Ctrl-Y`, then the
fold exactly as the event arm makes it, then one frame:

```
thread 'app::tests::audit_probe_a_fold_under_the_select_mode' panicked at crates/mush/src/app/chat.rs:1768:24:
index out of bounds: the len is 1 but the index is 7
```

The child blind's probe read the asymmetry on the same state: a key first
(`select_apply`) leaves `selecting=true` — the mode survives, clamped — while
the frame without a key panics. The trigger needs no key: the automatic fold at
nine tenths of the history budget (`transcript::compaction_trigger`) is enough,
and `/compact` cannot be typed while the mode holds the keyboard, so the
automatic arm is the road that matters.

**Blast radius.** The whole process: the panic is in the draw path on the UI
thread, so it unwinds through `main`, taking every actor thread, every child's
worktree and every detached job with it — and the message box's draft, which
was never sent. The transcript survives only to the last `flush_session`.

**Fix.** Clamp on the paint road the way the key road does: at the top of
`select_body`, take `self.clamped_cursor(on)`; when there is no line left, paint
the ordinary body and no mode. Better still, give `Selecting` the transcript's
revision and drop the mode in `replace_transcript` — the one road that replaces
a transcript — because a cursor into a transcript that no longer exists is not a
cursor.

**Acceptance test.** `a_fold_while_selecting_does_not_panic_the_frame`: root
with ≥2 messages, `Ctrl-Y`, `replace_transcript(ROOT, [compacted summary])` (the
`_AgentEvent::Compact` shape), then a frame — no panic, and either the cursor
lands on a line that exists or the pane paints no cursor and no `Enter copies`
clause.

---

## D2 — two rows of one id lose a painted row, and a real key then panics the pane

**Severity: blocker. Proven (re-proven here).** `crates/mush/src/app/screen.rs:472`,
with `AgentTree::rows` at `tree.rs:1521-1545`, `cursor_bottom` at `tree.rs:1710`,
`cursor` at `tree.rs:1716`.

Two lengths decide one question. `rows()` de-duplicates by **id** (the `grow`
walk's `rows.iter().any(|row| row.id == node.id)` and the "whatever the walk
missed" fallback both skip a second node with an id already painted), so with a
duplicate `rows().len() == agents.len() - 1`. The cursor is bounded by the
**storage** length — `cursor_bottom` sets `agents.len() - 1`, `cursor()` clamps
the same way — and the pane then indexes the painted rows with it:
`agent_footer(self, nodes[cursor], &rows[cursor], …)` (`screen.rs:472`).

My own probe: a stored session with two rows of id 2, restored through
`App::new`, then `G` (`Intent::TreeLast`) and a frame:

```
AUDIT PROBE: agents=3 rows=2
thread 'app::tests::audit_probe_a_duplicate_id_panics_the_pane' panicked at crates/mush/src/app/screen.rs:472:41:
index out of bounds: the len is 2 but the index is 2
```

The ids in the file are trusted: `mod.rs:743` reserves `agent.id + 1` and
`tree.register` pushes unconditionally, so a duplicate is registered twice.
That is **C9** (`docs/audits/secrets-session-config.md`), still open; this is a
new consequence C9 did not name, and unlike D3 it is a plain index panic — it
happens in release builds too. It is also wrong before any key: the second node
is never painted at all, which is exactly what `rows()`'s own doc promises
cannot happen ("every node is therefore painted exactly once").

**Blast radius.** A repository can commit `.mush/session.json` (an ignore rule
does not untrack a tracked file) and the store is hand-editable. On screen: an
agent the human cannot see, then a keypress — two `j`s or one `G` — kills the
running TUI mid-run, with agents' worktrees on disk.

**Fix.** One owner for "the row count": clamp the cursor to `rows().len()` (or
index with `.get()`), and have the restore refuse a duplicate id (C9's fix) so
the state cannot be built in the first place.

**Acceptance test.** `the_pane_never_indexes_past_the_rows_it_painted` (a
session with two rows of one id paints at 80×24 after `G`, and the cursor names
the last *painted* row) and `rows_paints_every_node_even_when_two_share_an_id`.

---

## D3 — `agent.id + 1` overflows: a startup panic from a file and from a git ref

**Severity: major. Proven (two real runs).** `crates/mush/src/app/mod.rs:743`
(restore), `:1204`/`:1209` (the reclaim pass), `:1272` (`discover_worktrees`).

The agent counter is "one past the largest id the repository has named"
(`Ids::reserve_agents`), and every road computes that as `id + 1` without a
checked add. `AgentSession.id` is a `u64` read straight from the session file,
and `git::worktree_id` parses any `mush/<digits>` branch with `parse::<u64>()`.
Two probes, both panicking in this worktree:

```
thread '…audit_probe_a_session_naming_the_largest_agent_id' panicked at crates/mush/src/app/mod.rs:743:38:
attempt to add with overflow
thread '…audit_probe_a_branch_naming_the_largest_agent_id' panicked at crates/mush/src/app/mod.rs:1209:46:
attempt to add with overflow
```

The second needs an **unmerged** commit on the branch (a bare branch is
reclaimed first and never reaches the reservation); the first needs only one
number in `.mush/session.json`. In a release build the add wraps to 0, so
`reserve_agents(0)` silently leaves the floor unset rather than crashing — the
debug build (the test suite, `cargo run`, most development) dies at startup.

**Blast radius.** A corrupted or hand-edited session file, or a repository that
once held a hand-made branch with that name, makes mush unstartable in that
workspace — and the human's only signal is a Rust panic before the first frame.
The root cause is C9's (the file's ids are trusted); this is its arithmetic
consequence.

**Fix.** `reserve_agents(id.saturating_add(1))` at all three sites, and validate
the restored id range at the door (C9): refuse an id above a ceiling the id
space can grow to (say `1 << 32`), with a sentence naming the file and the
value.

**Acceptance test.** `no_agent_id_can_overflow_the_floor`: a session naming
`u64::MAX` and a repository with an unmerged `mush/18446744073709551615` branch
both start, refuse the value by name, and leave the id floor usable.

---

## D4 — a command road blocks the UI thread on the endpoint: measured 10.036 s

**Severity: major. Proven (measured).** `crates/mush/src/app/mod.rs:2807-2810`
(`refresh_models` → `http::list_models` → `get_json`), called from
`mod.rs:2733` (`/url`), `:2765` (`/models`), `:2862` (`Ctrl-P` with no list
yet), `:2981` (`/provider`).

The startup fetch runs on its own thread behind `Msg::Models` precisely so
"nothing about an endpoint delays the first frame" (finding A9). The *command*
roads call the same function synchronously, inside `App::update`, under
`LIST_READ_TIMEOUT = 10 s` plus a 5 s connect. My probe put a listener that
accepts and never answers in front of an `App`, then called `refresh_models`
and timed it:

```
AUDIT PROBE: refresh_models took 10.036121491s on the UI thread
thread '…audit_probe_refresh_models_waits_on_the_ui_thread' panicked at crates/mush/src/app/mod.rs:15929:9:
the UI thread waited 10.036121491s inside update
```

The panic line is the probe's own assert (all probes deleted); the two numbers
are the measurement.

**Blast radius.** The human points mush at an endpoint that is down or wedged —
a typo, a LAN box off, a proxy hanging — and the whole TUI stops: no repaint, no
key handling, an agent that may be mid-run and cannot be seen or stopped. The
keys pressed meanwhile are not lost (raw mode queues them) but they land
afterwards, as a burst. `Ctrl-P` repeats the wait whenever the list is empty.
`docs/mush.md:1144` sells the opposite ("Keypress → screen | < 5 ms | `update`
touches only UI state"), and the manual's own next paragraph only promises
*startup* is bounded.

**Fix.** Use the road that already exists: command `refresh_models` spawns the
fetch thread and hands back a `Msg::Models` (the endpoint guard and the
empty-list fallback are already written for it), painting a `fetching…` line
first. If a synchronous wait is kept anywhere, it must not be on the thread that
paints.

**Acceptance test.** `a_command_road_never_waits_on_the_endpoint`: with a
listener that accepts and never answers, `/url`, `/models`, `/provider` and
`Ctrl-P` each return to the loop in well under 100 ms, and the answer arrives as
a `Msg::Models`; the bar says `no models` only when it does.

---

## D5 — at a short terminal the box paints its attachments with the rows the line being typed needed

**Severity: major. Proven (real frame capture).** `crates/mush/src/app/screen.rs:609`
(`input_rows`), `:693` (`text_rows`), `screen.rs:111` (`attachment_rows`, capped
by `MAX_ATTACHMENT_ROWS = 3`, `screen.rs:64`), `crates/mush/src/ui.rs:309-333`
(attachments painted first, cursor clamped into the box).

One question — how many rows does the box's content have? — has three answers.
`input_rows` asks for `draft + attachments + 2`; the layout may grant fewer
(`[Min(3), Length(input_rows)]` inside a column that is shorter); the painter
then computes `text_rows = field.height - attachments.len()`, while the
attachment rows take a constant cap and no room argument. When
`field.height <= attachments.len()`, `input.view` still returns its one
sentence, but the paragraph has one inner row and paints the first attachment;
the cursor is placed at `attachments.len() + cursor_row`, clamped to the box's
last row — an attachment row. The child blind's probe at 3 attachments:

```
PROBE1 40x10: asked=6 box=…height: 3 inner=…height: 1 cursor=Some((13, 7)) text_painted=false
PROBE1 40x10 row  7: "│▣ shots/shot0.png (png · 0 B)         │"     ← the cursor is here
PROBE1 40x12: …height: 5 inner=…height: 3 cursor=Some((13, 9)) text_painted=false
PROBE1 80x24: …height: 6 inner=…height: 4 cursor=Some((43, 20)) text_painted=true
```

**Blast radius.** The primary interaction on a small terminal: the human types
and nothing appears; the cursor blinks inside a file name. Nothing is lost (the
message still sends) and nothing explains it. `input_rows`' own doc says "a row
the box does not have is a row the message being typed is pushed out of" — that
is exactly what happens. No test can catch it: `assert_shape` checks line
widths, and the one test that paints a box with attachments does it at 120×32.

**Fix.** Give the text its row before the attachments: cap the attachment rows
to the room the box really has, at least one row short of it, then
`text_rows = field.height - attachments.len()`, and clamp the cursor into the
*text* area, never onto an attachment row.

**Acceptance test.** `the_box_paints_the_line_the_cursor_is_on`: over the sweep
sizes, with 1..=8 attachments and a multi-line draft, every painted attachment
row is inside the box, the cursor's recorded position is the row its line was
painted on, and the draft's text is on screen whenever `input_rows()` fits the
rows the box was granted.

---

## D6 — a stored session's `base_url` re-points the endpoint and the home key follows it

**Severity: major. Proven (real request captured, as in C6).** Site:
`crates/mush-core/src/config.rs:869-875` (the session layer's URL applied after
the home layer's) and `:820-821` (the home key copied in), reached at startup by
`main.rs:899` `config::resolve`. Outside this blind's file list — reported for
routing.

The key's home (the home config's `api_key`) and the destination's home (the
session's `base_url`) are different layers of one precedence chain, and a
session file inside the workspace decides the host while the machine-global key
travels to it. A child blind's probe (home config
`api_key: "sk-probe-home-KEY"`, `base_url: "http://127.0.0.1:1"`, session
`base_url` pointing at a local listener) captured:

```
PROBE resolved endpoint=http://127.0.0.1:46505 key=Some("sk-p…-KEY")
GET /v1/models HTTP/1.1
Authorization: Bearer sk-probe-home-KEY
```

**Blast radius.** Clone a repository, run `mush` in it, and the human's provider
key is on the wire to a host the *file* chose, seconds later, with exit 0 and no
line. This is C6's mechanism (a host change that keeps the key) on the one road
C6 did not name; C6 is not fixed, so the runtime switch leaks too.

**Fix.** Apply C6's rule to every stored layer: when a layer that is not the
human's own home config changes the host, drop the key and say so on the startup
line, or refuse the session's URL by name.

**Acceptance test.** `a_stored_session_cannot_take_the_home_key_to_its_own_host`:
`resolve_with` with a home key/URL and a session naming another host resolves to
`api_key == None` (or is refused naming the file); a session whose endpoint is
the home one keeps the key.

---

## D7 — a reap keeps the cursor's index, so the selected agent changes with no keystroke

**Severity: minor. Proven.** `crates/mush/src/app/tree.rs:1488-1500` (`reap`),
`:1504-1509` (`repair_focus`).

`repair_focus` only clamps the index: `self.agent_cursor =
self.agent_cursor.min(self.agents.len().saturating_sub(1))`. The cursor is an
index into `rows()`, and `past_history` drops the **oldest** rows — rows *above*
the cursor — so the same index now names a different agent. The child blind's
probe (51 finished children, 10 `j`s, then a reap):

```
cursor index=10 before=Some(AgentId(10)) after=Some(AgentId(11))
```

**Blast radius.** Past `CHILD_HISTORY = 50` finished children — an orchestrator's
session, the shape this file documents — every reaping tick slides the selection
one agent newer. The highlight and the transcript pane then disagree, and the
keys aimed at the selected row act on another agent: `c` stops a different
agent than the one being read; `Enter` opens another transcript.

**Fix.** Record `cursor_id()` before the retain and point the cursor back at it
after (`point_cursor_at`), clamping only when that id is gone too.

**Acceptance test.** `reaping_keeps_the_cursor_on_the_agent_it_named`.

---

## D8 — a reclaimed worktree keeps its branch stat on the row and in the title

**Severity: minor. Proven.** `crates/mush/src/app/tree.rs:902-908`
(`mark_reclaimed` clears `branch` and `kept`), `screen.rs:550-557` (the row
appends `agent_stats.get(&id)` whenever the map holds one), title sums in
`screen.rs:946`.

The fresh stat map is installed before the sweep decides, so a worktree the same
`adopt_git` reclaims leaves its `+3−1` behind. The child blind's probe:

```
before=[ "│   ✓ #1 port  mush/1 +3−1  did it     │" ]
after =[ "│   ✓ #1 port  +3−1  did it            │" ]
```

**Blast radius.** The row says `merged +3−1` and the title's `Σ +3 −1` counts a
branch that no longer exists, until some later git read happens to run — and the
tick reads git only while something is busy. A human reading the totals as
"unlanded work in this tree" is told about work already in the base.

**Fix.** `mark_reclaimed` removes the id's stat with its branch (or the row gates
the stat on `branch.is_some() && landed.is_none()`).

**Acceptance test.** `a_reclaimed_worktree_leaves_no_branch_stat`.

---

## D9 — an orphan row is indented by its stored depth

**Severity: minor. Proven.** `crates/mush/src/ui.rs:153`
(`let indent = "  ".repeat(row.depth)`), with `mod.rs:1309` (a leftover is
registered `depth: 1, parent: None`) and `mod.rs:848` (a restore keeps
`agent.depth.max(1)`).

The *order* is derived — `rows()` treats a missing parent as top-level — but the
*nesting* is stored. A depth-2 child whose parent was reaped paints five
indented columns with no `#1` row anywhere:

```
rows=[0, 2]
"│▶· #0 root                            │…"
"│     ✓ #2 2  done                     │…"
```

**Blast radius.** Cosmetic, but it is the U4 class one level down: the painted
indent and the derived order are two spellings of the nesting, and every
leftover row is one of them. `←` and the `▲N` counts read the derived one; the
human reads the indent.

**Fix.** Paint the indent from the painted parent chain (the `rows()` walk
already knows the row's depth), or register a parentless leftover at depth 0.

**Acceptance test.** `a_row_without_a_painted_parent_is_painted_at_the_top_level`.

---

## D10 — a status can erase `⊘ cancelling…`

**Severity: minor (suspected: the tree half is proven, the race is not driven).**
`crates/mush/src/app/tree.rs:975` (`activity`) and `:1003` (`thinking`) guard on
`is_busy` and on "not a fold" — and `Phase::Cancelling` passes both — while
`:1222` (`expire_cancels`) only retires a phase that is still `Cancelling`.

The child blind's probe drove the tree half: after the human's Ctrl-C sets
`Cancelling`, one status becomes `Activity("edit_file src/a.rs")`, one thinking
becomes `Thinking`, and `expire_cancels()` then returns false. The window in
`agent.rs` is a check-then-emit (`drain_signals`, the cancel check, then the
status emit), so a Ctrl-C landing between them leaves a live run whose row shows
a tool label for the rest of the call with no sign a stop is in flight.

**Blast radius.** One keystroke's feedback lost on the row the human is watching
while they try to stop a run; no data loss. The same family: a just-resumed
parked child is optimistically `Thinking` while its mailbox is still dead, so a
Ctrl-C in that one-tick window takes the dead-mailbox branch and writes a
`⚠ cut off … nothing committed` into the parent's transcript for a run the
revived actor then starts.

**Fix.** Refuse to replace `Phase::Cancelling` in `activity`/`thinking`, exactly
as they refuse to replace a fold.

**Acceptance test.** `a_status_never_erases_the_cancelling_mark`.

---

## D11 — the chat pane's title is the one title with no elision rule

**Severity: minor. Proven.** `crates/mush/src/ui.rs:284` (`block.title(painted.title.clone())`),
against the agents pane's `elide` (`screen.rs:844`) and zen's `zen_title`
(`screen.rs:880`), with the chat's own title built at `chat.rs:1659`.

`Block` clips a title at the border. With `Ctrl-Y` open and a child focused the
child blind's frame read:

```
PROBE6 40x10: title=" agent #12 · Enter copies · Esc leaves · +6 more lines · /notes " width=64 room=38
              painted_border="┌ agent #12 · Enter copies · Esc leaves┐"
```

The count of hidden lines and `/notes` — the pane's own way of saying what it
hides — are clipped; one column narrower and `Esc leaves` goes too. Outside the
mode the title fits at the 15 sizes probed.

**Blast radius.** Cosmetic and narrow (a 40-column terminal with the select mode
open), but it is the §8.46 class: a count the human is owed, cut mid-word by a
painter that does no arithmetic.

**Fix.** Route the chat title through `elide` too.

**Acceptance test.** `no_pane_title_paints_past_its_pane`.

---

## D12 — a resize with `/notes` or `/help` open clips every row of the report

**Severity: minor. Proven. Outside the closed rows.** `mod.rs:2910` /
`mod.rs:2938` (rows wrapped to `picker_text_width(self.term_width)` at open),
`screen.rs:736` (the popup's rect from the frame's `area.width`).

One fact, two sources: `set_term_size` records the new size, but an open picker
is never re-wrapped, so the popup clips. The child blind's probe (open at 200,
paint at 60):

```
PROBE3 after resize to 60x17: "│         │›   0s · alpha bravo charlie delta ech│…"   tail painted: false
PROBE3 reopened at 60x17:     … "         romeo sierra tango           "            tail painted: true
```

The sweep's own comment (`mod.rs:13106`) claims a resize "opens them again for
each size, which is what a human resizing the terminal with the popup up would
get" — it is not; a resize does not reopen the popup.

**Blast radius.** Forty columns of every row unreachable until the popup is
closed and reopened. Cosmetic, recoverable.

**Fix.** Re-wrap an open picker on `set_term_size`, or derive the popup's width
from the same `term_width` the rows were wrapped to.

**Acceptance test.** `a_resized_popup_is_rewrapped_not_clipped`.

---

## D13 — zen's Chat arm re-derives the message box, and the `at_every_size` pin checks two sizes

**Severity: minor. Proven. Outside the closed rows (§8.49/H47).**
`crates/mush/src/app/screen.rs:389-397` (the zen Chat arm calls `chat_pane`
again) against `:400-407` (the zen Agents arm reuses the two-pane split's rect,
with its own comment: "its own split, not a re-derivation").

At 40×12 the box jumps from 5 rows to 6 and up one row when the tree gives up
its rows; the child blind's probe:

```
PROBE4 40x12: two.box=…height: 5   zen_chat.box={ y: 5, height: 6 }   zen_agents.box=…height: 5
PROBE4 40x12: two.box_kept_in_zen_chat=false  two.box_kept_in_zen_agents=true
```

The pin named `zen_gives_the_focused_pane_the_two_panes_width_at_every_size`
(`mod.rs:10839`) loops over exactly two sizes, 80×24 and 200×40 — both tall
enough that the arm's re-derivation happens to agree. The visible effect is
benign (the box gets what it asked for), but the record's promise — the view
"moves the frame and not the conversation" — is not what the code does.

**Fix.** Compute the two-pane chat split once and let the zen Chat arm reuse
`rows[1]` for the box, widened to the frame, as the Agents arm already does.

**Acceptance test.** `zen_keeps_the_boxes_rows_at_every_size`.

---

## D14 — a reply of only fence lines is an invisible turn, and the select cursor has no row

**Severity: minor. Proven. Inside §8.50's mechanism, outside its record.**
`crates/mush/src/app/chat.rs:543` (`lines_of` decides selectability from the
role alone), `chat.rs:622` (`select_rows`).

`markdown_rows` removes fence lines ("the fence lines are not painted"), so a
reply that is only a fence has a source line whose message paints no row. The
child blind's probe:

```
P2 rows=[] selecting=true painted.select=false title=" mush · Enter copies · Esc leaves " rows=[]
P2 copied text="```" line="copied 1 line from #0's reply — 3 bytes"
P11 "```\n\n```" rows=["mush › "] title=" mush "      <- a bare mark with no words
P11 "```\n```"   rows=[]                            <- the turn is invisible
```

**Blast radius.** The human reads a pane where the model said something it
cannot see; with the mode on, the pane advertises `Enter copies` and paints no
cursor anywhere, while `↑`/`↓`/`Home`/`End` move a cursor that is never painted
and `Enter` copies a line that is not on screen. No data loss (the copy is
right). This also contradicts §8.50's "every word is kept": a fence-only reply's
bytes are kept but nothing is painted.

**Fix.** Give a fence line a row when its block has no body (paint it as text),
and/or make `painted` fall back to the nearest paintable row instead of
returning `None`; the frame must not confuse "no mode" with "mode with no
visible row".

**Acceptance test.** `a_reply_of_only_a_fence_is_not_an_invisible_turn` and
`the_select_cursor_has_a_row_on_every_line_lines_of_names`.

---

## D15 — a held reading comes back from the dead after a fold

**Severity: minor. Proven.** `crates/mush/src/app/chat.rs:363` (`Reading::held`
validates the hold against a length), `:1569` (`scroll_by` stores `up_to =
messages`).

A fold replaces the transcript; `held` returns `None` for one frame, and then
growth back past the *old* `up_to` resurrects the hold — a window belonging to a
conversation that no longer exists. The doc states the opposite ("a position the
transcript no longer has is not a position — the pane is at the bottom again"):

```
P5 held=" mush · scrolled ↑3 rows · PgDn " after_fold=" mush " after_growth=" mush · scrolled ↑3 rows · PgDn "
```

**Blast radius.** A lie about the reading position plus a pane silently parked
mid-transcript after a fold; the human presses `PgDn` to find the newest line.
Cosmetic-to-annoying; no loss.

**Fix.** Drop `reading` for the agent inside `replace_transcript` (a fold *is* a
new transcript), or carry a revision in `Reading` and compare it in `held`.

**Acceptance test.** `a_fold_puts_every_pane_back_at_the_bottom`.

---

## D16 — an agent whose actor published its prompt but has no transcript weighs 0

**Severity: minor. Proven.** `crates/mush/src/app/chat.rs:960-966`
(`used_weight_for` returns 0 before the prompt is weighed).

`learn_system` (fed by `AgentEvent::SystemPrompt`) and the agent's first
`Message` are two events; between them the meter and the attach gate read 0:

```
P4 used_weight_for(#1)=0 prompt weight=3006
P4 after one line: used_weight_for(#1)=3012
```

The doc's invariant — "the actor's list is never the heavier of the two, so the
room the attach gate computes is never larger than the room the next request
has" — is false here by a whole prompt (~3 KB ≈ 1 k tokens). H44's class, one
step further: it *under*-reports.

**Blast radius.** The attach gate's window bound (`mod.rs:3683`, `:3893`)
accepts a picture on a room short by the prompt; the next request is then
refused before the wire by H43's `over_window_line` — a turn and the human's
money for a message the gate had accepted. The window is one frame of event
draining; the arithmetic is wrong.

**Fix.** Weigh the prompt first and return it alone when there is no transcript,
or make `learn_system` create the empty transcript entry it knows exists.

**Acceptance test.** `an_agent_whose_actor_said_its_prompt_weighs_it_even_with_nothing_said`.

---

## D17 — the module doc's "News" row is false for a stopped run

**Severity: minor. Proven.** `crates/mush/src/app/chat.rs:26-28` ("a failure, a
run mush stopped … is written to the session so a restart still says what
broke") against `:1180` (`stored_notices` keeps `NoticeKind::Error` only).

Probe: `P3 kind=Some(Stopped) stored_notices=0` after the loop-stop notice. The
code is deliberate and defensible — a stop must not come back as a red `!`, and
the stored *status* carries it in the row — so the defect is the doc, not the
store.

**Fix.** Either store the stop/cut-off kind and restore it as such, or repair
the sentence.

**Acceptance test.** `a_stopped_run_is_either_stored_as_a_stop_or_not_claimed_to_be`.

---

## D18 — a focus change under the select mode makes `Enter` copy nothing, silently

**Severity: minor. Proven.** `crates/mush/src/app/chat.rs:1375` (`select_apply`'s
first arm clears the mode and returns `None` when `select.agent != on`),
`mod.rs:3479` (`App::select_key`'s `Copy` arm does nothing with `None`), reached
by `attach::Op::Focus` → `attach_focus` (`mod.rs:2639`), which changes the
focused agent with no `cancel_select`.

```
P10 copied=None selecting=false (the selection is gone)
```

**Blast radius.** A modal keyboard with no cursor, a dead opener (`Ctrl-Y` is a
no-op while selecting) and an `Enter` that eats the selection with no line. A
human steering mush through the attach socket can strand a human at the
keyboard.

**Fix.** On a focus change, either follow the mode (it names an agent) or drop
it and say so; make `Enter` refuse loudly rather than silently.

**Acceptance test.** `a_focus_change_under_the_select_mode_either_follows_it_or_says_it_left`.

---

## D19 — a control character in a URL reaches the request line

**Severity: minor. Proven on the wire. As in C7, on a second field.**
`crates/mush/src/app/commands.rs:132-135`/`:261` (the `/url` row accepts any
non-empty text), `mush-core/src/config.rs:393-395` (`normalize_url` trims the
ends only), `crates/mush/src/http.rs:440-447` (the head is written raw).

The child blind's probe: `parse_command("/url http://…/\r\nX-Injected-By-Url:
yes\r\nAccept-Language: zz")` → `Command::Url`, and the listener received a
`X-Injected-By-Url` header and a broken request line. The human's own road
reaches it: `Msg::Paste` normalises CRLF to LF and Shift-Enter inserts LF, so a
pasted block whose first word is `/url` is enough; the file roads carry CRLF
intact.

**Blast radius.** A malformed request (a 400 naming the endpoint) or a smuggled
header; with D6's session door, a hostile file can do it with the key attached.

**Fix.** One checked door for URLs (`set_base_url`/`normalize_url`): refuse any
value containing a control character, by name, at every entry point, as C7
proposed for the key.

**Acceptance test.** `a_url_with_a_control_character_is_refused_by_every_door`.

---

## D20 — a paste that merges graphemes leaves the cursor past the end, and one Backspace is swallowed

**Severity: minor. Proven.** `crates/mush/src/input.rs:55-59`
(`self.cursor += text.graphemes(true).count()` — the paste's count, not the
result's), `:61-69` (`backspace` computes both byte positions past the end).

```
PROBE skin tone: cursor=2 graphemes=1 text="x🏽"
PROBE after one Backspace: text="x🏽" cursor=1   (the first Backspace deleted nothing)
```

A skin-tone modifier or a regional indicator joins the previous cluster, so the
cursor lands one past the end. `Delete` no-ops too; `insert` still appends
correctly and `view` does not panic.

**Blast radius.** One keystroke that does nothing, on IME input or a pasted
emoji sequence, in a box whose contract says edits land on grapheme boundaries.

**Fix.** Clamp after the insert (`self.cursor = self.cursor.min(self.graphemes())`).

**Acceptance test.** `a_paste_that_merges_with_the_grapheme_before_it_leaves_the_cursor_in_the_box`.

---

## D21 — the config cell's doc claims the copies cannot differ; a handle learn strands the UI

**Severity: minor. Proven (mechanism); reachability is a landmine, not a live
bug today.** `crates/mush/src/app/settings.rs:15-21` (the module doc: a poisoned
lock is "the one way the copies can still differ"), `:54-61`, `:108-115` (the
cell weighs against `ui`), `:172-179` (the handle weighs against `shared`).

```
PROBE after the actor learnt 16000: ui=128000 actors=16000
PROBE after the 4000 complaint: ui learned=false ui=128000 actors=4000
```

Today's only handle caller (`AgentCtx::learn_context`) always emits the event,
so the app converges — but a second caller, or an event dropped because the
conversation tag changed, leaves the bar/tool caps on one window and every
request on another.

**Fix.** Compare one shared `in_use` on both roads, or make
`ConfigHandle::learn_context` private to the announce-always wrapper.

**Acceptance test.** `the_two_copies_agree_or_the_number_is_refused`.

---

## D22 — an empty key is "set", "(none)" and an empty Bearer

**Severity: minor. Proven.** `mush-core/src/config.rs:820-821` (copied with no
emptiness filter), `mod.rs:2741` (`/key`'s ack), `main.rs:694` (the dump
filters `!key.is_empty()`), `http.rs:444-445`.

```
PROBE /key would say "api key set (••••…)"; --print-config says "(none)"
PROBE request: Authorization: Bearer 
```

**Blast radius.** Three answers to one fact on three surfaces read in that
order, while every request carries an empty bearer. Needs the empty string in
the file (`MUSH_API_KEY=""` is filtered), so a hand edit or a script accident.

**Fix.** Filter the empty string where the layer is read
(`home.api_key.clone().filter(|k| !k.is_empty())`), which fixes all three at
once.

**Acceptance test.** `an_empty_config_key_is_no_key`.

---

## D23 — the `/help` table's description column collapses at the floor

**Severity: minor. Measured.** `crates/mush/src/app/commands.rs:200-213` (the
usage column is never wrapped; `room = width - description_column`, floored at
1), reached by `open_help_picker` → `picker_text_width`.

```
PROBE terminal 40: picker text width 34, 364 lines, longest 34
    /provider [deepseek|custom]  s
                                 w
```

At the 40-column floor the descriptions wrap to one character per line; at 80
they fragment into nine-column pieces. `mush --help` is unaffected (unbounded
width).

**Fix.** When `room` falls under a floor, hang the description under the usage
(or wrap the usage column), as the key table already does.

**Acceptance test.** `the_help_picker_keeps_a_readable_description_column`.

---

## D24 — `Ctrl-Y` under zen with the tree full-screen opens a mode the frame cannot paint

**Severity: minor. Proven (real run).** `mod.rs:3479` (`select_key` asks the
chat for a cursor over `tree.focused`), `chat.rs:1339` (`start_select` refuses
only when the transcript has no *words*), against the zen-agents layout where
`ChatPane::transcript` is `None` (§8.49).

My own probe: a reply, `Tab` to the tree, `Ctrl-F`, then `Ctrl-Y`:

```
rows = screen(&mut app, 80, 24)   → no row holds the reply (the chat pane is hidden)
ctrl(&mut app, 'y')               → app.chat.selecting() == true
text_of(&app)                     → no line says a mode took the keyboard
```

The mode owns the keyboard, paints no cursor (the chat's rect is zero), and the
app-wide `Ctrl-` block still answers; a letter is swallowed with nothing on
screen saying why.

**Blast radius.** `Ctrl-Y` is app-wide, so a human reading the tree can lock
their own typing with no visible cause; `Esc` or `Tab` recovers, but nothing
says so. This is H47's mechanism on a case its record does not contain ("a pane
with no source line says so in the bar" — here the pane has no **rows**).

**Fix.** Refuse with a line when the chat pane has no rows (the same shape as
the empty-pane case), or make `Ctrl-Y` leave zen/focus the chat pane first.

**Acceptance test.** `ctrl_y_with_the_chat_hidden_says_so`.

---

## D25 — the "several agents running" line names `Enter`, which never stops anything

**Severity: minor. Proven by the key table.** `mod.rs:4124`:
`"{} agents running · Enter picks one to stop · Ctrl-X stops them all"`.

`Ctrl-C` reaches that line when ≥2 agents are busy and the focused one is not.
`Enter` in the chat pane is `Intent::Send`; in the tree it is `Intent::TreeFocus`
— it makes the row's transcript visible and starts or stops nothing (the stop
key is `Ctrl-C`, or `c` on the tree's cursor row). Wording, not logic.

**Blast radius.** A human follows the line, presses `Enter` in the chat pane
with a draft in the box, and sends it instead of stopping anything.

**Fix.** Name the road that stops: `… · Tab to the agents pane, c stops the
selected row · Ctrl-X stops them all`, or drop the `Enter` clause.

**Acceptance test.** `the_many_agents_line_names_a_key_that_stops`.

---

## D26 — `Msg::Git` carries no conversation stamp

**Severity: minor (suspected: mechanism read, not raced).**
`crates/mush/src/app/mod.rs:1041` (the worker sends `Msg::Git { stats, status,
sweep }`), `:1332` (`update` adopts it with no conversation check), against
`Msg::Clipboard`, `Msg::Copied` and `Msg::Agent`, which all carry one.

A read in flight across `Ctrl-N` lands in the new tree. `adopt_git` filters the
stats by `tree.has(id)` but that is an id test, not an identity test, and
`sweep_worktrees` acts on a snapshot taken from the old tree (re-deciding from
git facts, which limits the harm), while `git_in_flight` is cleared from under
whatever read the new tree started. The stale value `self.git = status` is the
same repository, so the visible harm is a frame or two of another tree's numbers
and a spurious `git::reclaim` pass.

**Fix.** Stamp `Msg::Git` with the conversation, as every other off-thread
message is, and drop it in `update`.

**Acceptance test.** `a_git_read_from_the_old_chat_does_not_touch_the_new_tree`.

---

## Verified sound

Each line is what was checked and against what, not what a doc says.

- **The keymap is one pure function and both help surfaces read it** (`keys.rs`):
  `KEYS` is the one table, `help_table`/`help_table_at` render it for `mush
  --help` and the `/help` popup, the precedence (app-wide `Ctrl-` keys and
  `Tab` above the picker, the picker above the select mode, the mode above the
  panes) is pinned key by key from both focuses and over a picker, releases are
  ignored, and the deliberate modifier leaks (`Alt-C`, `Ctrl-J`) are pinned as
  decisions.
- **No panic anywhere in the frame, at any size** (child blind on
  `screen.rs`/`ui.rs`): 39 sizes (0×0, 1×1, 5×4, 39×9, 255×60, 400×200 …) × 6
  states (rich tree, markdown/CJK transcripts, attachments, zen both focuses, a
  picker, the select mode at both ends) through `App::screen` + `ui::draw` — all
  survived; the compact tiers' three `Length`s add up exactly; `▲N`/`▼N` match
  ratatui's real window; every frame starts from a blank buffer.
- **The painter can emit a terminal command**: `Buffer::set_stringn` does
  **not** filter control graphemes — the *Unsolved* experiment in
  `paint-and-measure.md` was run: paint `Line::from(Span::raw("\u{1b}[2J"))`
  into a `TestBackend` and read the cell, and the escape is in the cell, because
  crossterm's backend writes `cell.symbol()` verbatim — so the unsanitized
  `⌂ {root}` cell and `Config::label()`'s endpoint are real escapes, not
  cosmetic gaps, which is why the box and the bar defang what they paint
  (`a3274b3`; B15's *marks* never reach a cell).
- **`rows()` order and completeness for unique ids, and cursor arithmetic**
  (child blind on `tree.rs`): pre-order over parent links, orphans top-level,
  `grow`'s visited check makes a cycle a non-loop, `cursor_id` is the one
  index→id door, jumps take single steps through it. (D2 is the duplicate-id
  hole; D7 the reap hole.)
- **Counts and phases have one owner**: `live_job_count` and a row's `⚙N` both
  filter `record.running()`; `Roster`/`busy_counts`/`napping` put each agent in
  one bucket; `is_busy`/`waiting`/`compacting`/`words`/`label`/`doing` and their
  readers agree; `phase_glyph`/`phase_detail` are exhaustive; `title()`'s bound
  is display columns through `unicode_truncate`, not bytes.
- **The select copy road is line-exact across the cap** (child blind on
  `chat.rs`): a 30-line tool result copied one source line at a time for every
  `n`; the pane's map carries a row for all 30 (the 8 painted plus the `…`
  tagged with its line). And `mark_rows`' restated row count vs `markdown_rows`
  survived a 4,000-text × 13-width × 3-role fuzz with no `debug_assert` trip and
  no over-wide row (its release-build guard is a blind spot below).
- **The `+N more lines` arithmetic** (the count is notes only, the last row is
  protected, the count row is spent only when shown < room) matches its three
  pins; the `pending`/voice road puts an image-only send in the human's voice;
  the box and the transcript both use `app::image_label`, so one picture's row
  cannot be spelled two ways.
- **`used_weight_for`'s sum is one sum** (published prompt 3,006 + a 6-byte line
  = 3,012 in the probe) — D16 is the window before the transcript exists.
- **`theme.rs`'s palette claims hold today**: FNV-1a-64 matches the standard's
  vectors, `nearest_256` is the true nearest, the closest hue pair measured
  ΔE2000 11.78 against the doc's "≈ 11.8", the L* band measures 65.93–83.15
  against the doc's 65–84 — the *tests* are weaker (see blind spots).
- **`input.rs`'s window arithmetic cannot panic**: a 427-triple sweep (CJK,
  combining, ZWJ/flags, tabs, blank, multi-line, widths 0..=6) found no panic,
  no cursor row outside the returned lines, no line wider than the field.
- **`commands.rs` is one table**: the parser, `table`/`table_at` and therefore
  `mush --help`'s COMMANDS block and `/help` all read it; every row parses; a
  client cannot execute a command (`attach_edit`'s send road delivers text as a
  message).
- **`settings.rs` holds for the app as wired**: `own`/`edit`/`adopt_handle`
  write both copies and the one live handle caller always emits, so the UI
  converges (D21 is the doc and the landmine).
- **The hub's own invariants** (mine): every off-thread message but `Msg::Git`
  is conversation-stamped and stale ones are dropped, with a stale `Spawned`
  child still told `Shutdown`; the quit arm is derived from the status line, so
  it cannot outlive its warning, and an attach op restores it unless the op
  itself failed (H9); `carry_images` uses the receiving workspace's own
  reader (stat before open), so a FIFO in a picture's path cannot park the UI
  thread; the attach revision guard refuses a stale edit instead of guessing;
  `Ctrl-N` stops every actor, kills the registry's jobs, respawns the root with
  a fresh cell and flushes (C4's data loss stands, but the mechanics are tested);
  `Drop` flushes a dirty session and kills the jobs.
- **The two blockers' neighbouring pins still hold**: the existing
  `a_fold…`/zen/select tests pass, and the two panics need states no fixture
  builds — which is exactly why they survived.

## Blind spots — invariants this area claims with no test behind them

- **`rows().len() == agents.len()`** — the basis of the cursor clamps, the
  compact pane's height and `screen.rs:472`'s indexing. Pinned only for unique
  ids. (D2)
- **"A reap leaves the cursor on the agent it named"** — only the clamp is
  tested. (D7)
- **"A reclaimed worktree leaves no stale measurement"** — `mark_reclaimed` has
  no test. (D8)
- **"Only a mark on a run in flight can be in `expire_cancels`"** — true of the
  setter, not of what the phase can become after it. (D10)
- **"the actor's list is never the heavier of the two"** (`chat.rs:952-958`) —
  D16 falsifies it before an actor's first message. (D16)
- **"a position the transcript no longer has is not a position"**
  (`Reading::held`) — D15's growth-after-fold case is the counterexample.
- **`Painted::select == None` means "the ordinary reading"** — the second state
  (mode on, no paintable row) is unpinned. (D14)
- **A prompt with no transcript weighs nothing** — no test weighs one. (D16)
- **The box's painted content fits the box** — `input_rows` is asserted only at
  80×24 and 200×40, and the *granted* height is checked nowhere. (D5, D13)
- **`mark_rows`' fence restatement vs `markdown_rows`** — guarded only by a
  `debug_assert`, compiled out in release; `REPLY_MARK` and the assistant arm's
  literal `"mush › "` are two spellings of the same mark, so a drift would put
  the cursor and the copy on the wrong source line in release.
- **Theme's own two claims** — the test checks squared RGB distance ≥ 400 where
  the doc promises ΔE2000 ≈ 11.8 (two colours 0.66 apart in ΔE2000 pass), and
  an L* band of 60–85 where the doc says 65–84.
- **`/key`'s ack and `--print-config`'s key row** are two readings of one value
  with no cross-surface test. (D22)
- **A URL is stored without a control character** — `normalize_url`'s doc says
  only "no trailing slash". (D19)
- **`Msg::Git`'s conversation identity** — no test crosses `Ctrl-N` with a read
  in flight. (D26)
- **`picker.items.len() as u16 + 3`, `agents.len() as u16`** — truncating casts,
  unreachable today, unasserted.

## Not verified — what needs a real terminal, a live actor, a slow disk, or a platform

- **The panics' user-visible shape.** Both blockers were driven through
  `TestBackend` frames, `App::on_key` and the real `update` path — not the
  binary's loop. Whether the terminal is left usable after the unwind, and what
  `main` prints, needs a pty.
- **D4's freeze with a live agent.** The 10.036 s was measured on an idle `App`;
  the mechanism is the same with a run in flight, but I did not drive one
  through the freeze.
- **D10's race and the parked-child Ctrl-C window** — one instruction wide and
  one tick wide; the tree half is proven, the race is read.
- **D3's release behaviour** — the wrap is read (debug assertions off), not run
  in release.
- **A crafted chain of parents** (a session with ~10⁵ chained rows) could
  overflow the stack in `rows()`' recursive `grow` before D2's panic; the file
  was not built.
- **A real resize with `/notes` open** (D12) and the select band's live cursor
  cells — driven as two frame states and a `TestBackend` buffer, not by a pty.
- **The clipboard and the attach socket**: F6's reachability via
  `mush focus` is read from `attach_focus`'s own test, not run against a live
  socket; the copy itself stops at `Copied`.
- **A live endpoint's** model list and its interaction with the picker's width;
  the manual's 10-minute chat deadline; a wedged DNS.
- **A 16-colour/`TERM=linux` terminal** (the theme ladder has truecolor and the
  256 nearest only), macOS/Windows, and a 32-bit target.
- **`MUSH_THEME`/`MUSH_*` with non-UTF-8 bytes** (`std::env::var().ok()`
  silently reads them as unset, contradicting "a name mush does not know is a
  startup error") — house-wide, unstaged.
- **D5's real-terminal cursor**: the position was read from the backend's
  recorded cursor, not a blinking terminal.

## Drift these findings make

- **`docs/mush.md:1144`** — the performance table's "Keypress → screen | < 5 ms
  | `update` touches only UI state" is false on `/url`, `/models`, `/model`,
  `/provider` and `Ctrl-P`: they block inside `update` for up to the 10 s read
  timeout (D4). The same row's "Model discovery | < 50 ms | … or on `/model`,
  `/models`, `/url`" describes the happy path only; the manual's own next
  paragraph bounds *startup*, not the command roads.
- **Two test names over-promise**: `zen_gives_the_focused_pane_the_two_panes_width_at_every_size`
  loops over two sizes (D13); the sweep's `reopen` comment (`mod.rs:13106`)
  claims a resize re-opens the popup, which nothing does (D12).
- **`screen.rs`'s `input_rows` doc** ("a row the box does not have is a row the
  message being typed is pushed out of") is exactly the state D5 paints.
- **`docs/mush.md:465`'s anywhere row** for `Ctrl-Y` and the manual's select
  section say nothing about zen: D24 is the hidden case.
- Nothing else I checked makes `docs/findings.md`, `docs/mush.md`,
  `README.md` or `docs/refactor.md` false; the ledger's H30/H21/H27/H16
  residuals are untouched by this read.

## Recorded, not changed — checked

- **C6 is unfixed and D6 is its second road** — the fix should be one rule at
  both doors (a host change drops or refuses the key).
- **C9 is unfixed and D2/D3 are its arithmetic.** Whatever the ruling on
  restoring the file's ids, D2's index panic and D3's overflow are worth fixing
  independently; both are one line each.
- **H23 (the attach draft half)** stands: `attach_edit`'s doc says "the message
  box's draft for `agent`" while `Chat` has one box; the earlier audit recorded
  it and I did not re-report it — D18 is the neighbouring fact (the *selection*
  is scoped to an agent, the box is not).
- **C4 (Ctrl-N overwrites the stored conversation)** stands and is not
  re-reported; the key's own paths are otherwise tested.
- **H28's flake** did not appear in any run here.
