# Paint and measure — the columns, the cells and the terminal's own state, audited

A blind audit of the layer where a defect is a wrong *column*: the width
arithmetic the panes wrap and truncate by, the geometry of every tier from
200×50 down to the 40×10 floor, the exactness of the stop/fold machinery the
select mode steps through, the terminal's own state on every exit road, the
cost and the safety of one frame, and the marks and the palette that reach the
glass. "Blind" here means no doc comment is taken as true: each one is a claim,
and what follows is what the code does at the base.

**Base:** `e926526` (`merge: the record enters the two waves, their rows, and
the sentences they made false`), my own worktree. The audit file is the only
file I created or changed; nothing was run — no cargo, no test, no clippy, no
pty, no real terminal. Every column count quoted below was taken with
`python3`, every line number read at this base.

**How.** `crates/mush-core/src/text.rs` (sanitize, `wrap_capped`, `wrap_runs`,
`runs_width`, the table walk, `cut`/`truncate`/`fit_row`), `crates/mush/src/`
`app/chat.rs` (Stop/Stops/Selecting/step/copy/`cursor_row`/`folded_*`/
`mark_rows`/`Fold`), `app/screen.rs` (layout, panes, `elide`, `facts_line`,
`attachment_rows`, the floor), `ui.rs` (every paint), `app/mod.rs` (the sweep,
`assert_shape`, `assert_no_command`, `image_label`, paste and attach), `main.rs`
(the loop, the guard, the panic hook), `signals.rs`, `input.rs`, `theme.rs`,
`Cargo.lock` were read where this lane reaches them; `docs/findings.md` was read
for the ledger.

**A note on the ground moving.** Two sibling writers are editing
`crates/mush/src/app/mod.rs` (parentless-row restore) and
`crates/mush/src/app/tree.rs`/`ui.rs` (placement/dim ink) at this base. The
findings below that name those files are the roads as they stand at `e926526`
and may have moved by the time this is read.

**Not re-reported.** H73–H80, A19, §8.102's flakes and §8.104 M5 (the lane
brief's list); the reply's double parse (`docs/dedup/pane-and-text.md` §1,
`docs/refactor.md` §11); §8.105's fixed race; the defang roads `9039e82`
closed; D1/D5/D13/D20/D26, R1/R28/R72, R3 and P11–P12 where they still stand.

## Findings, ranked

| # | Severity | Finding |
|---|---|---|
| PM1 | major | The message box is the one painted surface `sanitize` never sees: an attach client's — or a paste's — escape sequence in the draft reaches the human's terminal. |
| PM2 | minor | Two fixed sentences are painted into rows narrower than they are at the 40-column floor: the picker's hint (44 into 38) and the bar's idle hint (71 into 40). |
| PM3 | minor | A known-zero-width character is measured as one column in every spelling of the width arithmetic (`.max(1)`): rows break early on NFD text and ZWJ emoji. |
| PM4 | minor | `▣ name (mime · size)` rows are painted whole and never truncated: past the box the file name is cut mid-name and the size is lost. |
| PM5 | minor | A resize and a key in the same drain step the cursor at the last painted measure while the frame paints at the new one: one `↓` can leap a whole fold tail, and `↑` does not return. |
| PM6 | minor | A second SIGTERM/SIGHUP/SIGINT — or any SIGKILL — ends mush with the terminal still raw and in the alternate screen; deliberate, but its cost is recorded nowhere. |
| PM7 | latent | The two wrappers recompute a broken tail on different rules (`UnicodeWidthStr` sequence widths vs a per-char sum): ZWJ text wraps to different row counts in the plain and styled views. |
| PM8 | latent | `TerminalGuard::enter` can leave raw mode on with no guard built when the alternate-screen write or `Terminal::new` fails: `main` exits 1 into a raw shell. |
| PM9 | latent | A panic on a non-owner thread prints its words to a stderr the alternate screen is painting over; ratatui's diff never repairs those cells. |

## PM1 — the message box is the one painted surface `sanitize` never sees (major)

`mush_core::text::sanitize`'s doc ends with the invariant this finding is about
(`crates/mush-core/src/text.rs:46-48`): "this is the same width-and-text
arithmetic as the rest of the module, and it is what the wrappers apply before a
row is built, **so no caller can forget it**." The message box is that caller.
The draft is stored raw (`crates/mush/src/input.rs:62-66`):

```rust
    pub fn insert(&mut self, text: &str) {
        let at = self.byte_at(self.cursor);
        self.text.insert_str(at, text);
        self.cursor = (self.cursor + text.graphemes(true).count()).min(self.graphemes());
    }
```

and two untrusted roads carry text into it:

- **an attach client's edit.** `attach::Op::Edit` matches into `App::attach_edit`
  (`crates/mush/src/app/mod.rs:2918-2929`), whose draft arm is
  `self.chat.set_draft(id, text);` (`:3141`), and `Chat::set_draft` is
  `self.input.clear(); self.input.insert(text);`
  (`crates/mush/src/app/chat.rs:2742-2747`). The text is the client's positional
  read straight from argv (`crates/mush/src/main.rs:389-396`; the flag table at
  `:243-250` names only `--agent/--base/--send`), so
  `mush edit --agent 0 --base <rev> $'\e]0;PWNED\a'` stores the escape as the
  draft. No validation stands between the client's bytes and `insert`.
- **a bracketed paste.** `Msg::Paste` normalises line endings and nothing else
  (`crates/mush/src/app/mod.rs:1683-1695`), and a paste that turns out to be
  image paths whose attachment is refused is *deliberately* put in the box as
  text (`:1698-1716`) — the payload the terminal handed over is the payload the
  box holds.

Both end in the same paint: `Input::view`/`window_line`
(`crates/mush/src/input.rs:128-150` and `:185-231`) copy a substring of the
stored text into the painted line verbatim, and `ui::draw_chat` paints every
draft line as a raw span (`crates/mush/src/ui.rs:356-361`):

```rust
        rendered.push(Line::from(vec![
            Span::styled(lead, style),
            Span::raw(line.clone()),
        ]));
```

**Why this is a leak and not a cosmetic gap.** This is the same ratatui
`Paragraph`/`Span`/`Buffer` road as the reply `9039e82` had to defang, at the
same crate: `Cargo.lock:507-509` pins `ratatui 0.29.0`, and so does the lock at
`9039e82` (checked with `git show 9039e82:Cargo.lock` and `9039e82^:Cargo.lock`).
That commit's own record of what the *painter* did before it was told to
sanitize is the load-bearing evidence: "a reply of `…and then\rREPLACED…`
returned the cursor to column 1 and erased the pane's border, `ESC ]0;PWNED BEL`
set the window title, and a tool result carrying `ESC [2J ESC [H` wiped the
frame." And the pre-fix reply row is the exact shape the box paints today —
`9039e82^:crates/mush/src/app/chat.rs:928`:

```rust
        out.push(Line::from(vec![head, Span::raw(line)]));
```

The one-line painters tell the same story: `text::truncate`'s own doc
(`crates/mush-core/src/text.rs:1520-1532`) records a reply "returned the cursor
over the pane's own border" and an error body's `ESC ]0;PWNED BEL` that
"renamed the window" before `truncate` sanitized. So the painter does write
control characters through to the terminal; the sanitize at the *row builder*
is the only thing that stopped the reply, and the box builds its rows in a
different place. `docs/audits/tui.md:898` records the
contrary claim — that `Buffer::set_stringn` filters control graphemes, which
would make this cosmetic. The repository's own observed symptom at the same
crate version is why this is filed rather than held; the experiment that would
settle it in one line is in *Unsolved* below.

**Cost.** A draft painted by the attach road, a frame wiped (`ESC[2J ESC[H`), a
window title set (`ESC]0;…BEL`), a border arrowed into (`\r`) — the human's
terminal obeying text they never typed, while mush keeps painting a frame it
believes is intact. It is the same class as the historical C8/§8.90 leaks, on
the one surface the defang did not reach.

**What the tests do not say.** `assert_no_command`
(`crates/mush/src/app/mod.rs:7127-7144`) reads the *painted frame* for control
characters and is run over a hostile reply, a run's failure, a tool result, a
`/notes` popup and the wide-glyph sweep (`:16448`, `:16549`, `:16883`) — never
over the message box, and no paste test uses a hostile payload
(`:8923-9288`). The sweep's hostile `Msg::Paste` cases are image paths and plain
prose.

## PM2 — two fixed sentences are painted into rows narrower than they are (minor)

The floor is 40 columns (`MIN_WIDTH`/`MIN_HEIGHT` and `is_below_floor`,
`crates/mush/src/app/mod.rs:396-404`), and 40×10 is one of the sweep's sizes
(`SWEEP_SIZES`, `crates/mush/src/app/mod.rs:15851-15866`). Two sentences longer
than the rows they are painted on have no elision:

- **the picker's hint** is 44 columns (`Picker::hint`,
  `crates/mush/src/app/mod.rs:214-219`: `" j/k or PgUp/PgDn · Enter pick · Esc cancel "`),
  painted whole into the popup's inner rect (`crates/mush/src/ui.rs:427-435`).
  At a 40-column terminal that rect is 38 columns: `picker_width(40)` is
  `share(40, 60, PICKER_MIN_WIDTH, PICKER_MAX_WIDTH)` = 40
  (`crates/mush/src/app/screen.rs:74-88`) and the border takes two, while the
  `PickerPane` handed to the painter carries the hint as a `&'static str` with
  no width in it (`screen.rs:830-835`). The painted row is exactly
  ` j/k or PgUp/PgDn · Enter pick · Esc c` — `"ancel "` is clipped.
- **the bar's idle hint** is 62 columns (`ui::HINT`, `crates/mush/src/ui.rs:36`:
  `"Tab cycles panes · /help lists commands · Ctrl-P picks a model"`), painted
  after a ` agents ` badge and a space into the bar's own line
  (`ui.rs:445-458`), so at 40 columns the visible row is
  ` agents Tab cycles panes · /help lis` — the tail `ts commands · Ctrl-P picks
  a model` is gone, cut mid-word.

**Cost.** The 40-column human never reads the verb that belongs to `Esc` in the
picker's own hint, and never reads half of the bar's. Nothing pins it: the popup
test asserts the head only (`text.contains("j/k or PgUp/PgDn")`,
`crates/mush/src/app/mod.rs:16862-16865`) and the bar assertion asks only for a
badge and *something* past it (`:6972-6982`).

## PM3 — a known-zero-width character is measured as one column (minor)

`UnicodeWidthChar::width` returns `None` for the characters the tables know to
take no column — a combining mark (U+0301), ZWJ (U+200D), ZWNJ, ZWSP (U+200B),
BOM (U+FEFF), SHY (U+00AD) — and `.max(1)` turns every one of them into a
column. Four sites use that spelling:

- `wrap_capped`, `crates/mush-core/src/text.rs:262-271`
  (`UnicodeWidthChar::width(ch).unwrap_or(1).max(1)`);
- `wrap_runs`, `:1458-1463`;
- `runs_width`, `:1196-1205`, whose doc says "a glyph is its own width,
  and a character the width is unknown for is one column" (`:1186-1190`) — false
  for exactly these, where the width is not unknown but known-zero;
- the table rule's widest-cell walk, `:2075-2086`.

**Input.** Any decomposed text: a tool result echoing a macOS file name
(`Screenshot\u{301}.png`), a text file with combining accents, or a ZWJ
sequence. `\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}` is five code points and
measures 8 columns by this arithmetic; a terminal paints 2.

**Cost.** Over-measure, never overflow: the row breaks early by one column per
mark, so a line of NFD text leaves that many columns of the pane unused, and a
family emoji costs six columns of it. The fold pays the same over-measure: a
block of NFD text costs rows it would not cost at the terminal's own widths, so
the 8-row fold shows fewer source lines before its `…` than the same text
without the marks. Both wrappers agree on it, so this is a disagreement with the
*terminal*, not between mush's own roads.

## PM4 — an image row is painted whole and never truncated (minor)

`image_label` sanitizes the path but does not shorten it
(`crates/mush/src/app/mod.rs:255-261`: `"{} ({format} · {})"` over
`sanitize(&image.path)` and `size_label(image.bytes.len())`), and both painters
hand the whole label to the renderer:

- the transcript row: `format!("  ▣ {}", image_label(image))`
  (`crates/mush/src/app/chat.rs:2965-2971`), painted by the transcript
  `Paragraph` into the pane's inner rect (`crates/mush/src/ui.rs:326-327`);
- the box's attachment row: `format!("▣ {}", image_label(image))`
  (`crates/mush/src/app/screen.rs:126-140`, capped at `MAX_ATTACHMENT_ROWS`),
  painted whole (`crates/mush/src/ui.rs:346-362`).

**Geometry.** At 80×24 the agents pane is 30 columns (`agents_columns`,
`crates/mush/src/app/screen.rs:104-107`, from the layout at `:407-413`), so the
chat pane and both of its inner rects are 48 columns. A row is `▣ ` + the whole
path + ` (` mime ` · ` size `)`:

```
  ▣ Screenshot from 2024-01-01 12-00-00.png (png · 2.0 MB)   ← 66 columns
  ▣ Screenshot from 2024-01-01 12-00-00.png (png             ← the 48 painted
```

The size, the format and the closing paren are gone, and two screenshots taken
one second apart — the difference is the last digit before `.png` — paint the
same 48 columns: the human cannot tell them apart by the row that is supposed to
name them. The box's own row has the same fate (`▣ ` + 44 columns of path at a
40-column terminal's 38-column box).

**Why no test catches it.** `assert_shape` asserts every *transcript* line and
every *draft* line fits the pane it is painted in
(`crates/mush/src/app/mod.rs:7005-7029`) but never asserts an attachment row.
The D5 test that does read attachment rows (`:19044-19052`) asserts containment
of the row's whole text in a painted line — an assertion that *would* catch a
clipped path — but its fixture's paths are `shots/shotN.png`, 30 columns at most
at 40×12, so it passes while a real name does not.

## PM5 — a resize and a key in the same drain step the cursor at the old measure (minor)

`main.rs`'s loop drains *every* pending event — a `Resize` and then keys — and
paints once afterwards (`crates/mush/src/main.rs:1183-1207`; the one paint at
`:1213-1222`). The key road reads the width the pane *last* painted at:
`Selecting::measure` is a `Cell` the frame publishes as it paints
(`select.measure.set(Some(width))`, `crates/mush/src/app/chat.rs:2126`, whose
doc at `:470-475` says the frame publishes it "and the keys read it"), and
`Chat::measure()` hands that published value to `step_stop`
(`chat.rs:1871-1873` and `:1949-1959`), which steps `adjacent` through
`stops_at` at that measure. The paint one line later uses the *new* `width`
(`:2126-2135`). One frame, two measures.

**Input.** A 30-line tool result whose lines wrap to four rows each at a
60-column pane: the 8-row fold (`Fold::DEFAULT`, `chat.rs:3284-3287`) paints
lines 1–2 and hides 3–30 behind the `…`. Select mode is on with the cursor on
line 2; then *drag the window to 120 columns and press `↓` in the same frame*
(the resize and the key arrive in one drain, and the loop paints once). At 120
the same lines are one row each, so the fold paints lines 1–8 and the `…` stands
after the 8th.

**Cost.** The key road steps at measure 60: line 2 → `Stop::Tail`. The frame
paints at 120, where `Stops::clamp` keeps the Tail a Tail (`:715-722`), so the
cursor is now the `…` after eight painted rows — one press leaps seven rows —
and `↑` does not return: backward from `Tail` is
`Stop::Line(stops.visible - 1)` (`:1938`), line 8 at 120, not line 2. The copy
stays with the stop the cursor names, so this is *not* §8.105's copy/paint race;
what breaks is the position the human built one press at a time. No test resizes
while selecting (`app/chat.rs`'s select tests never call `set_term_size`).

## PM6 — the second signal dies raw in the alternate screen (minor)

`signals.rs` chooses this out loud (`crates/mush/src/signals.rs:30-42`): "**The
second signal dies at once.** … the next one restores the signal's default
disposition and re-raises it, so mush dies immediately instead of waiting out a
flush or a writer's join. … an insisting human must never meet a process that
cannot be killed because its cleanup is stuck." The registration is
`flag::register_conditional_default` plus `flag::register` for SIGTERM, SIGHUP
and SIGINT (`:96-104`).

The cost of that choice is not written down anywhere: the *first* signal's road
restores the terminal only when the quit road finishes (the loop breaks, `App`'s
`Drop` flushes and kills, `TerminalGuard::drop` writes the mode resets,
`crates/mush/src/main.rs:1289-1292`). A second SIGTERM/SIGHUP/SIGINT arriving
during that flush — or any SIGKILL, which no handler can see — ends the process
with raw mode set and the alternate screen selected. The human's shell is then
in the alternate screen with `ISIG` cleared: typing does not echo, `Ctrl-C` does
not signal, and the fix is `reset`/`stty sane`.

**Cost, and why not major.** The window is the flush, and the input is a
deliberate second kill; the alternative the module chose (an unkillable process)
is worse, and SIGKILL is uncoverable by any design. Filed so the trade is
written down: the first signal always restores, the second knowingly does not.

## PM7 — the two wrappers recompute a broken tail on different rules (latent)

`wrap_capped` recomputes a row's width after a space break with the *string*
API — `current_width = UnicodeWidthStr::width(current.as_str())`
(`crates/mush-core/src/text.rs:306`, inside the break loop at `:290-310`) — while
`wrap_runs` recomputes a per-char sum —
`.map(|(ch, _)| UnicodeWidthChar::width(*ch).unwrap_or(1).max(1)).sum()`
(`:1483-1486`). unicode-width 0.2's string width knows ZWJ sequences (a family
is 2 columns); the per-char sum spells the same family 8. The module's claim is
"one rule, two spellings, and no drift between the view and the text beside it"
(`:1443-1445`), and the test that pins it is
`a_plain_message_wraps_exactly_like_wrap_text` (`:3005`): its fixed texts cannot
reach the difference, and its fuzz alphabet is
`['a', 'b', ' ', '\u{a0}', '\u{3000}', '\u{2028}', '\t']` (`:3027`) — no
combining mark, no ZWJ, no zero-width character in it.

**Input.** `a \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} b` at a narrow width,
in the plain wrapper (a tool result, `wrap_text_capped`) and the styled one (a
reply, `wrap_runs`).

**Cost.** The same words can wrap into a different number of rows in the two
views — the row a space break leaves as its tail is charged the sequence's own
width by one wrapper and the sum of its parts by the other — so a reply and a
result holding the same text do not line up. Latent: it needs the named input,
and the pinning test's alphabet is exactly what cannot produce it.

## PM8 — a failed `enter` leaves raw mode on with no guard (latent)

`TerminalGuard::enter` enters the modes and then builds the terminal
(`crates/mush/src/main.rs:1281-1287`):

```rust
    fn enter() -> io::Result<Self> {
        enter_terminal_modes()?;
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        Ok(Self { terminal })
    }
```

`enter_terminal_modes` does `enable_raw_mode()?` and *then*
`execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)`
(`main.rs:1242-1254`), and `Terminal::new` writes its own setup. If any of the
writes after `enable_raw_mode()` fails, no `TerminalGuard` exists, so no `Drop`
runs: `run()` returns `Err` and `main` prints the line and
`std::process::exit(1)` (`main.rs:62-66`) with raw mode still on.

**Input.** An I/O failure on the alternate-screen write or on ratatui's setup
write (a closed/full stdout), on a terminal whose raw mode is already set.

**Cost.** mush refuses to start and leaves the human's shell raw — the mirror
image of the road E10 fixed, latent because it needs the I/O failure, not a key.

## PM9 — a non-owner panic prints into the alternate screen (latent)

`install_panic_hook_for` restores the terminal only on the thread that owns it
and then always calls `previous(info)` (`crates/mush/src/main.rs:1337-1352`), so
a panic on any *other* thread writes the default hook's text to stderr while the
UI still holds the alternate screen and raw mode. On raw mode the newlines do
not return the cursor to column one, and ratatui's per-frame diff only rewrites
cells that changed between *its* buffers (`main.rs:1213-1222`), so the stray
characters are never repaired — they persist until their cells happen to change
or a resize repaints everything, and the alternate screen discards them at exit.
`actor_main` does catch a worker's panic (`crates/mush/src/agent.rs:1959-1973`),
but the panic *hook* has already run by then, and `panic_words`' own doc admits
the road (`agent.rs:2124-2131`): "the process-wide hook restores the terminal and
prints to a stderr the screen is painting over."

**Input.** A panic anywhere in an actor body (or the writer, or any worker
thread) while the UI runs — the shape the tree exercises deliberately at
`crates/mush/src/agent.rs:13671`. I found no *reachable* panic in the actor body
on this read; the finding is about the scribe, not the trigger.

**Cost.** Glyph garbage smeared into a frame mush believes is intact, until the
cells change or the human resizes. The *reason* still arrives — `file_death`
paints the panic's words as a `CutOff` row — so nothing is lost but the frame's
integrity.

## Appendix — smaller notes, not findings

- **There is no "settings popup".** The lane brief names one; `settings.rs` is
  the configuration *cell* (`/model`, `/url`, `/key` edit it through
  `cell.edit`), and its words are painted by the bar and the facts line. Nothing
  to audit there beyond `Config::label()`, whose model field is sanitized
  (`crates/mush-core/src/config.rs:985-990`).
- **The `/notes` and `/help` hint** (`" j/k or PgUp/PgDn scrolls · Esc closes "`,
  39 columns) exceeds the popup's 38-column inner row at the floor by its
  trailing space only; invisible, no cost.
- **The unsanitized `⌂ {root}` cell** (`crates/mush/src/app/screen.rs:891`,
  `format!(" ⌂ {shown}")`) is PM1's class with a rarer input: a workspace path
  whose *directory name* holds an escape (`mkdir $'\e[2J'` is legal). The URL
  and key roads refuse control characters at every door (D19/`4f7793c`, C7), so
  the rest of the cell is bounded; held here rather than filed, because the
  human authored that path.
- **East Asian Ambiguous glyphs.** `▶ ◐ ≡ · … ─ │ ┼ ▣ • → ↑ ↓` are all
  Ambiguous-width: unicode-width counts them 1, a terminal configured to resolve
  ambiguous characters as wide paints them 2, so on such a terminal every border
  and every row mark is one column wider than mush measured. Not a mush defect —
  the terminal's own policy — and not settleable from here.
- **Absurd sizes are all floor sizes.** `is_below_floor` sends 39×9, 30×8,
  20×5 and 1×1 to `Screen::Floor` (one notice, no panes), so no pane arithmetic
  runs there; the notice itself picks the longest spelling that fits. Held,
  sound.
- **The two documented width exceptions are unreachable in the app.** A glyph
  wider than the whole width (`wrap_text("日", 1)`'s empty-row arm) needs a pane
  narrower than a glyph, and chat's inner width is ≥ 38 at the 40-column floor;
  `fit_row`'s early return when `width <= head_width + 2` needs a head that
  nearly fills the pane, and a row's head is a mark plus an id at depth ≤ 3
  (`MAX_DEPTH`, indent ≤ 6). Held.
- **The lane brief's `⏸` mentions are stale** — the pause mark was removed in
  §8.93; `every_row_mark_is_one_column` pins the glyphs that remain
  (`crates/mush/src/app/screen.rs:1599`).

## Suspect and held

- **The select/stop machinery is consistent.** I traced `Stops::of`/`clamp`/
  `last`/`span` (`chat.rs:633-760`), `first_row`/`last_row`, `clamped_cursor`
  (`:1900-1914`), `adjacent` (`:1919-1947`), `step_stop`, `copy`, `select_body`,
  `window_from`, `cursor_row` and `top_at_cursor`/`top_at_bottom` end to end:
  the clamp is total, `visible ≥ 1` always (so `visible - 1` cannot underflow),
  the row map and the paint come from the same walk (`folded_rows`), `mark_rows`
  reconstructs the mark's lead from the always-present (possibly empty) head
  span, and `counts[line]` has one entry per `split('\n')` line. The
  `debug_assert_eq!`s on the map (`chat.rs:2405`, `:3502`, `:3565`, `:3684`,
  `:3733`, `:3757`) have no release-only failure I could construct. Held: the
  trace is not a proof, but nothing in the arithmetic is left unexplained.
- **One frame per iteration.** `update` sets `dirty_screen` for every `Msg` and
  `tick` paints once (`main.rs:1213-1222`); there is no other `terminal.draw`;
  a burst is capped at 4096 events (`:1165`); `Chunk::take_into` zips so
  `lines`/`rows` stay parallel; `trim_trailing_blanks` cannot underflow; the
  `summaries`/`spoken` caches are cleared or shifted on replace/forget/
  shift_indices. H77 (a crafted reply line can stall a frame) stays open and is
  not re-reported.
- **The sweep is wide.** Fifteen sizes including 1×1 and both floor boundaries,
  borders, badge, agent rows, transcript lines and draft lines asserted at each
  (`SWEEP_SIZES` and `assert_shape`). Its known gaps: the top border row (which
  carries the title) is skipped, and `input.attachments` is not asserted — the
  titles were read by hand at every sweep size (agents, chat and input titles,
  the picker titles and the 38-column notes title) and all fit; the attachment
  gap is PM4's.
- **The terminal roads that do restore.** A clean quit, an `Err` from the
  event loop, a UI-thread panic (the hook restores *before* `previous(info)`
  prints, so the message is readable) and `Drop` all leave raw mode and the
  alternate screen behind themselves; `panic = "abort"` is not set in
  `Cargo.toml`, so unwinding and `Drop` survive a panic. The cursor is clamped
  to the inner box and no mush code writes `?25h`/`?25l` by hand
  (`TerminalGuard::drop`'s `show_cursor` is the only unhide).
- **The box's attachment budget** (`attachment_rows`'s cap guard,
  `content_rows`, `input_rows`' agreement), `picker_pane`'s windowing
  (`(items+3).min(24).min(height-2)`, `visible = inner.height - 1`), the
  elided facts line and `ui.rs:144`'s
  `inner.y + inner.height - pane.footer.len()` (footer ≤ 3, only built when the
  room has the rows — no underflow) were read and hold.

## Known, not re-reported

H73–H80 (including H77's frame stall, which this lane's cost reading reaches and
leaves open), A19, §8.102's flakes, §8.104 M5; `mark_rows`' double parse of a
reply (`docs/dedup/pane-and-text.md` §1, `docs/refactor.md` §11);
`Fold::DEFAULT`'s reasoning slot at `usize::MAX` and the per-frame re-render it
permits (§8.68's residual); §8.105's fixed copy/paint race (PM5 is a *measure*
race, and its copy still matches the stop — not the same defect); D1/D5/D13/D20/
D26, R1/R28/R72, R3 and P11–P12; the defang roads `9039e82` closed (the reply,
the tool result, the `/notes` rows, the tool-call label, the model list, the
model in `Config::label()`) — PM1 is the surface that fix did not reach.

## Unsolved at this base

- **Whether ratatui 0.29's `Buffer::set_stringn` drops control graphemes.** The
  crate source is outside this workspace and could not be read. The one
  experiment: paint `Line::from(Span::raw("\u{1b}[2J"))` into a `TestBackend`
  and read the cell — an escape in the cell means crossterm writes it to the
  terminal (its backend writes `cell.symbol()` verbatim), which makes PM1
  certain; an empty cell refutes it and makes the box cosmetic. The repository's
  own record at `9039e82` is the evidence I filed on.
- **`unicode-truncate 1.1`'s tables vs `unicode-width 0.2`.** `Cargo.lock` names
  both (`unicode-width 0.2.0` as a direct dependency and `0.1.14` through
  `unicode-truncate`), and `text::truncate`/`cut` (`text.rs:1533-1546`,
  `:1566-1578`) and `input::window_line` (`input.rs:223-224`) mix them:
  `total`/`led`/the budget
  come from `UnicodeWidthStr::width` (0.2) while the slicing is
  `unicode_truncate` (0.1, grapheme tables). A concrete candidate is
  `truncate("❤️", 1)` (U+2764 U+FE0F) returning two columns for a one-column
  budget. Not provable without the crate's source; no caller in this lane
  reaches a one-column width (the floor is 40), so it is named here rather than
  filed.
- **A terminal's real behaviour** for the Ambiguous-width glyphs, the 16-colour
  approximation of the hue band, and whether bracketed paste passes control
  characters through — all three need a terminal and a human's eye, and were not
  available to this audit.
