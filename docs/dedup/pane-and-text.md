# Duplication in the transcript pane and the text it wraps

A blind pass over four files — `crates/mush/src/app/chat.rs`,
`crates/mush/src/ui.rs`, `crates/mush-core/src/text.rs`,
`crates/mush/src/theme.rs` — read end to end, production code only: 2,644
non-blank, non-comment lines before each file's `mod tests`, the same currency
as the brief's 2,640.

Five candidates clear the bar; six more were measured and are rounding errors
on their own lines. The two that can **drift** are 1 and 3: in both, the same
arithmetic is written twice and the only thing keeping the copies together is a
test (3) or a `debug_assert` that compiles out of release builds (1).

## Ranked

| # | Candidate | Sites (`file:line`) | Net code lines removed | Correctness risk if left | Effort |
|---|---|---|---|---|---|
| 1 | The row map is counted a second time: `mark_rows` re-wraps what `marked` wrapped, and `View` restates `marked`'s own reply decision | chat.rs 3011-3051, 2991-2995, 2838-2849; text.rs 134-196, 317-348 | −55 | **High** — silent in release; the fence rule lives in two files | medium |
| 2 | The mark's columns (indent + mark on the first row, their blank under it, body wrapped inside) spelled three times, with `MIN_BODY`'s drop-the-mark rule on one road only | chat.rs 2637-2672, 2549-2570, 2958-2966, 2499-2536 | −28 | Latent — at a 4-column body the two unguarded roads paint 5 columns | small |
| 3 | `wrap_runs` is `wrap_capped`'s arithmetic a second time, tab stop and break loop included | text.rs 650-708 vs 134-196 | −20 | Moderate — pinned by a parity test over ten texts × widths 1..=24 | medium |
| 4 | Four walks to a transcript's own edge, in two readings (`lines_of` vs `Stops::of`) | chat.rs 1557-1563, 1566-1572, 1683-1690, 1703-1706 | −8 | Low | small |
| 5 | "Mush's one way of putting a colour behind text", spelled three times, and its inverse once | ui.rs 112, 248, 251, 384, 414 | 0 | Low (pixel tests read three of the five) — the single owner is the point | small |

Total: ≈ **120 code lines** of the 2,644 (4.5%) if all five land.

---

## 1. The row map is counted a second time — net −55

**Sites.** chat.rs 3011-3051 (`mark_rows`, 37 code lines), 2982-2995 (`View`,
5 code lines and a 9-line doc), 2838-2849 (the stop walk inside
`capped_result`, 10 code lines); text.rs 134-196 (`wrap_capped`), 317-348
(`markdown_rows`, and the `fence_line` rule at 346-348).

```rust
// chat.rs:2838-2848 — capped_result counts the wrapped rows again, per source line
let mut stops: Vec<Stop> = Vec::with_capacity(wrapped.len());
let mut left = SHOWN + 1;
for (line, raw) in message.text().split('\n').enumerate() {
    if left == 0 { break; }
    let count = wrap_text_capped(raw, wrap, left).len();
    stops.extend(std::iter::repeat(Stop::Line(line)).take(count));
    left -= count;
}
debug_assert_eq!(stops.len(), wrapped.len(), "one source per wrapped row");
```

````rust
// chat.rs:3030-3045 — mark_rows counts them once more, and restates the fence rule
for (line, raw) in text.split('\n').enumerate() {
    let count = match view {
        View::Plain => wrap_text(raw, wrap).len(),
        View::Markdown => {
            let source = sanitize(raw);
            if source.trim_start().starts_with("```") {   // text.rs:346, again
                fence = !fence;
                0
            } else if fence {
                wrap_text(&source, wrap).len()
            } else {
                markdown_rows(&source, wrap).len()
            }
        }
    };
    rows.extend(std::iter::repeat(Some(Stop::Line(line))).take(count));
}
````

```rust
// text.rs:346-348 — the rule mark_rows copies
fn fence_line(line: &str) -> bool {
    line.trim_start().starts_with("```")
}
```

```rust
// chat.rs:2627 + 2920 — the reply is decided twice: once by the mark, once by the caller
let reply = mark == REPLY_MARK;                 // marked, chat.rs:2627
mark_rows(out, &mut rows, "mush › ", …, View::Markdown);   // render_message, chat.rs:2917-2925
```

**The shared shape.** Every painted row is attributed to one source line, and
the attribution is built by wrapping the text a second time in the caller.
`marked` already wraps each source line on its own (text.rs 137 splits on `\n`
first, and the parser is line-local); `mark_rows` then repeats that walk purely
to learn which source line each of those rows was. `capped_result` does the
same for the cap's ninth row. Two shape-decisions ride on the copies:
`View::Markdown` vs `marked`'s own `mark == REPLY_MARK`, and the fence literal
vs `fence_line`.

**The primitive.** In `mush_core::text`:

```rust
/// Wrapped rows, each tagged with the index of the source line it is the reading of: the painter
/// and the cursor's map read one walk, so a row cannot be attributed to a line the pane never painted.
pub fn wrap_tagged(text: &str, width: usize, max_lines: Option<usize>) -> Vec<(usize, String)>;

/// [`markdown_rows`], with the source line beside every row: the view's one cross-line rule — a fence
/// line paints no row — stays inside the parser that owns it.
pub fn markdown_tagged_rows(text: &str, width: usize) -> Vec<(usize, Vec<Run>)>;
```

`wrap_text`/`wrap_text_capped`/`markdown_rows` become three-line maps over them,
and `marked` returns the tag of every row it pushed (`fn marked(…) -> Vec<usize>`),
which deletes `mark_rows` and `View` outright: the arms become

```rust
rows.extend(marked(out, mark, style, message.text(), width)
    .into_iter()
    .map(|line| Some(Stop::Line(line))));
```

**The divergence, if it is left.** Add `~~~` to `fence_line` (a natural
extension of the parser) and forget chat.rs 3035: the view paints no row for a
`~~~` line while the map counts one row for it, so every stop after the first
fence is attributed one row too early — the cursor band and the highlighted
range land on the wrong rows, and only in a debug build does the
`debug_assert_eq!` (3049) fire. The same edit made to the *reply's mark*
(`REPLY_MARK` at 2626 vs the literal `"mush › "` at 2920) drops the markdown
view from one road and keeps it in the other. Release builds have neither
assert, and the copy tests read `Stops`/`lines_of`, not this map, so nothing
else sees it.

**What it removes.** `mark_rows` 37, `View` 5 (and its 9 doc lines), the two
9-line call sites become 3 lines each (−12), the walk in `capped_result` −10,
`Stops::of`'s recomputation of `visible` −6 — against ~8 new lines in text.rs and
~4 in `marked`: **net ≈ −55**.

---

## 2. The mark's columns, spelled three times — net −28

**Sites.** chat.rs 2637-2672 (`marked`, 31 code lines in two near-identical
loops), 2549-2570 (`reasoning_rows`, 13), 2958-2966 (the `"tool"` arm, 9),
2499/2523/2632 (`MIN_BODY` and its one guard).

```rust
// chat.rs:2662-2672 — the plain path, inside marked
for (index, line) in wrap_text(text, width.saturating_sub(lead)).into_iter().enumerate() {
    let head = if index == 0 {
        Span::styled(mark.to_string(), style)
    } else {
        Span::raw(" ".repeat(lead))
    };
    out.push(Line::from(vec![head, Span::raw(line)]));
}
```

```rust
// chat.rs:2559-2570 — reasoning_rows, the same shape with an indent in front
let lead = INDENT + MARK.width();
for (index, line) in wrap_text(reasoning, width.saturating_sub(lead)).into_iter().enumerate() {
    let head = if index == 0 {
        format!("{}{MARK}", " ".repeat(INDENT))
    } else {
        " ".repeat(lead)
    };
    out.push(Line::from(Span::styled(format!("{head}{line}"), style)));
}
```

```rust
// chat.rs:2958-2966 — the "tool" arm, the same shape a third time
for (index, (line, stop)) in capped_result(message, width).into_iter().enumerate() {
    let head = if index == 0 {
        format!("{}{mark}", " ".repeat(TOOL_INDENT))
    } else {
        " ".repeat(tool_lead(message))
    };
    out.push(Line::from(Span::styled(format!("{head}{line}"), style)));
    rows.push(Some(stop));
}
```

(`marked` holds this shape twice: the reply loop, 2637-2661, differs from the
plain loop only in how a row's body spans are built.)

**The shared shape.** One block of rows that leads with a mark: the first row
carries the indent and the mark, every later row carries their width of blank,
and each row's words are wrapped inside the columns those leave. The `marked`
version also has the drop rule — `let (mark, lead) = if width >= lead + MIN_BODY
{ (mark, lead) } else { ("", 0) }` (2632) — because a mark the pane clips "is a
row that says who spoke and nothing about what was said".

**The primitive.**

```rust
/// One row of a marked block: `indent` and `mark` lead the first row, their width of blank leads
/// every row after it, and a pane too narrow for the mark is left the words — the one place a
/// mark's columns are spent.
fn marked_row(
    index: usize,
    indent: usize,
    mark: &str,
    style: Style,
    width: usize,
    body: Vec<Span<'static>>,
) -> Line<'static>;
```

`marked` builds each row's body first (runs via `reply_style`, or one raw span),
then calls it; `reasoning_rows` and the tool arm call it with their own indent
and mark. `tool_lead` becomes `fn tool_lead(message: &Message, width: usize) ->
usize` so the cap's wrap boundary drops the mark exactly when the painter does —
the same one-arithmetic-two-readers move `capped_result` already makes.

**The divergence, if it is left.** `MIN_BODY = 4` and its doc ("the app's
`MIN_BODY` is 4", text.rs 1170-1172) say a body narrower than the mark plus four
columns is a case the tree decided on — and chat.rs's own test
`a_pane_narrower_than_the_voice_still_shows_the_words` (3985) paints at width 5
and asserts the mark is gone. The reasoning block's lead is 4 (`INDENT` 2 +
`"⋯ "` 2) and a failed result's is 4 (`TOOL_INDENT` 2 + `"! "` 2), and neither
road has the guard: at a 4-column body both wrap at `width.max(1) = 1` and
prefix a 4-column head, painting **5 columns into a 4-column pane** — the last
character clipped, which is the bug `marked`'s own doc records. Today the frame
floor keeps this latent (the chat pane is `Constraint::Min(20)` in the two-pane
layout and full-width at the 40-column floor, so a body is ≥ 18 columns); it is
a bound enforced on one road of three, waiting for the fourth mark road or a
modal given a width of its own.

**What it removes.** `marked`'s two loops become one body-and-head pass (−17),
`reasoning_rows` −8, the tool arm −4, `MIN_BODY`'s guard moves into the
primitive, against ~8 lines of primitive and signature changes. Net −28.

---

## 3. `wrap_runs` is `wrap_capped` again — net −20

**Sites.** text.rs 134-196 (`wrap_capped`, 47 code lines), 650-708 (`wrap_runs`,
49), whose own doc says so: "The arithmetic is `wrap_capped`'s, tab expansion
included, and a test pins the two against each other — one rule, two spellings,
and no drift between the view and the text beside it."

```rust
// text.rs:150-157 — the tab stop and the width of one character
let (rendered, char_width) = if ch == '\t' {
    ("    ".to_string(), 4)
} else {
    (ch.to_string(), UnicodeWidthChar::width(ch).unwrap_or(1).max(1))
};
```

```rust
// text.rs:663-668 — the same two facts, in another vocabulary
let (char_width, tab) = if ch == '\t' {
    (4, true)
} else {
    (UnicodeWidthChar::width(ch).unwrap_or(1).max(1), false)
};
```

```rust
// text.rs:167-177 — the break loop
loop {
    if current_width + char_width <= width || current.is_empty() { break; }
    if let Some(space) = last_space {
        let rest = current.split_off(space);
        out.push(std::mem::take(&mut current));
        current = rest.trim_start().to_string();
    } else {
        out.push(std::mem::take(&mut current));
    }
```

```rust
// text.rs:674-687 — the same loop, string trimming replaced by item trimming
loop {
    if current_width + char_width <= width || current.is_empty() { break; }
    if let Some(space) = last_space {
        let rest = current.split_off(space);
        out.push(std::mem::take(&mut current));
        current = rest;
        while matches!(current.first(), Some((' ', _))) { current.remove(0); }
    } else {
        out.push(std::mem::take(&mut current));
    }
```

`out.push(std::mem::take(&mut current))` appears four times across the two
functions and nowhere else in text.rs.

**The shared shape.** One line breaker: items in, rows out, with the tab stop
(four columns, not a break point), the space-break loop, and the
tail-that-is-still-too-full retry — the loop that was added to *both* copies for
`wrap_text(" bcd日", 4)`, and the mid-line cap check added to both. The only
difference between the copies is what an item *is*.

**The primitive.**

```rust
/// The one line breaker: items in, rows out, with the tab stop, the space-break loop and the
/// tail-that-is-still-too-full retry in one place; the caller says how many columns an item takes.
struct Breaker<T: Copy> { /* width, row: Vec<(T, usize)>, row_width, last_space, rows */ }

impl<T: Copy> Breaker<T> {
    /// Push one item (a tab is `columns = 4` of the space item); a row that no longer fits is closed first.
    fn push(&mut self, item: T, columns: usize, is_space: bool);
}
```

`wrap_capped` maps rows of `char` back to `String`s; `wrap_runs` feeds
`(char, RunStyle)` items and reuses `runs_of` (710-719) to gather them. Both
callers also need the source-line tags of candidate 1, which the same breaker
serves.

**The divergence, if it is left.** The parity test
`a_plain_message_wraps_exactly_like_wrap_text` (text.rs 1514) pins today's two
copies over ten texts at widths 1..=24, and
`a_wrapped_row_never_outgrows_its_width` (1174) covers widths 4..=12 for the
plain one only. That is real protection, and it is also the whole protection: a
tab stop, a break rule or a width the test's list does not enumerate (a `\r`
inside a line, a wide glyph at width ≥ 25) can be fixed in one wrapper and not
the other, and the symptom is the reply's view wrapping differently from the
plain text beside it — the copy and the painted row disagree about where a line
ended, which is the bug class the comment above `wrap_runs` was written about.

**What it removes.** 96 code lines of two drivers against ~40 of breaker plus
~14 and ~20 of the two drivers. Net −20 (15-25 depending on how the caller
supplies "is this item a space").

---

## 4. Four walks to a transcript's own edge — net −8

**Sites.** chat.rs 1557-1563 (`last_line`), 1566-1572 (`first_line`),
1683-1690 and 1703-1706 (the two `find_map`s inside `adjacent`).

```rust
// chat.rs:1557-1563
fn last_line(&self, on: AgentId) -> Option<(usize, Stop)> {
    let transcript = self.transcript(on);
    (0..transcript.len()).rev().find_map(|index| {
        lines_of(&transcript[index]).map(|lines| (index, Stop::Line(lines.len() - 1)))
    })
}

// chat.rs:1566-1572
fn first_line(&self, on: AgentId) -> Option<(usize, Stop)> {
    let transcript = self.transcript(on);
    (0..transcript.len())
        .find_map(|index| lines_of(&transcript[index]).map(|_| (index, Stop::Line(0))))
}
```

```rust
// chat.rs:1687-1690 and 1703-1706 — the same walk with the measure in hand
((cursor.0 + 1)..transcript.len()).find_map(|index| {
    Stops::of(&transcript[index], measure).map(|_| (index, Stop::Line(0)))
})
…
(0..cursor.0).rev().find_map(|index| {
    Stops::of(&transcript[index], measure).map(|stops| (index, stops.last()))
})
```

**The shared shape.** "The first, or last, *stop* of a transcript: the first
message in the walk that has rows of its own, at its first or last line."
`start_select`/`Home`/`End` answer it once each, and `adjacent` answers it once
per direction — and the four disagree about the reading: the two edge functions
use `lines_of` (measure-blind), `adjacent` uses `Stops::of` (measure-aware).

**The primitive.**

```rust
/// The first or last stop of a transcript at this measure: the pane's own edge, walked the one way
/// by the keys that jump to it and the keys that step toward it.
fn edge_line(&self, on: AgentId, newest: bool, measure: Option<usize>) -> Option<(usize, Stop)>;
```

`last_line`/`first_line` become `self.edge_line(on, true/false, self.measure())`
and `adjacent`'s two closures become the same call.

**The divergence, if it is left.** The two readings agree today only because
`clamped_cursor` re-clamps whatever they return — the measure-blind answer for a
capped result is a line the pane painted no row for, and the clamp turns it into
`Stop::Tail` afterwards. A second cap (a reply cap, a picture cap) or a change
to `Stops::of` makes `Home`/`End` land on a stop the pane does not paint: the
cursor is then placed by `cursor_row`'s fallback onto "the last row at or before
the stop", which is the wrong row on screen.

**What it removes.** 16 code lines → one ~12-line function and four one-line
call sites. Net −8.

---

## 5. "One way to put the hue behind text", spelled five times — net 0

**Sites.** ui.rs 112 (`draw_agents`), 248 and 251 (`select_painted`), 384
(`draw_picker`), 414 (`draw_status`).

```rust
// ui.rs:202 says it — "mush has exactly one way of putting a colour behind
// text, and this is it — the bar's badge and the agents pane's selected row
// paint the same pair"; the code says it five times:
ui.rs:112   List::new(items).highlight_style(Style::default().fg(Color::Black).bg(theme.accent()));
ui.rs:251   Style::default().fg(Color::Black).bg(theme.accent())
ui.rs:384   .highlight_style(Style::default().fg(Color::Black).bg(theme.accent()))
ui.rs:414   Style::default().fg(Color::Black).bg(theme.accent()),
// and the inverse, once:
ui.rs:248   Style::default().fg(theme.accent()).bg(Color::Black)
```

**The shared shape.** A `Style` pair whose meaning is a property of the theme
(the palette is chosen in the L* 65-84 band "precisely so it works *under* black
text", theme.rs 52-54) and whose spelling is a terminal-level fact every painter
currently owns.

**The primitive.** In ui.rs, beside `dim` and `border`:

```rust
/// The one way mush puts a colour behind text: `Color::Black` on the hue, the pair the palette's
/// L* band exists for — a band on this screen wears this or it is not a band.
fn on_accent(theme: &Theme) -> Style;

/// The band's inverse, worn where the keyboard is: the hue's characters on `Black`.
fn accent_on_black(theme: &Theme) -> Style;
```

`draw_picker`'s border also becomes `border(true, theme)`, which is exactly the
style it spells at 370.

**Why it is worth doing anyway.** It is the only candidate whose saving is a
rounding error (0 lines, and honestly so), and it is the one where the *comment*
already claims the owner: five cells that must wear the same pair, three of them
read by pixel tests (`the_default_theme_paints_the_fixed_palette`,
`a_themed_frame_paints_the_hue`,
`the_select_mode_paints_its_cursor_and_its_selection_on_their_own_cells`) — so a
drift shows up as a test failure rather than a mystery, but only for the three
sites the tests happen to read. `rank_style`'s and `border`'s
`Style::default().fg(theme.accent())` (52, 39, 314) are a different fact — ink,
not band — and stay as they are.

---

## Measured and below the bar

Each of these is under five lines net; they are listed so nobody re-finds them
and calls them savings.

| Shape | Sites | The one-line primitive | Net |
|---|---|---|---|
| The tail every `render_message` arm ends with: `image_rows` + `rows.resize(out.len() - start, None)` + a blank + `rows.push(None)` | chat.rs 2899-2902, 2913/2934-2937, 2967-2970 | `fn close_message(out, rows, start, message)` | −4 |
| `lines_of` (593-600) is the arms' own predicate, restated in three arms ("The predicate is the `render_message` arms' own") | chat.rs 2881-2903, 2904-2926, 2939-2958 | the arms take `lines_of(message)` as their guard | −3 |
| The fixed palette written twice (`Default::default` and the `MUSH_THEME=off` arm), differing only in `origin` | theme.rs 203-207 vs 240-244 | `fn fixed(origin: Origin) -> Self` | −2 |
| "One news line per agent, the newest" — the same `retain` in `note_error_for` and `note_cut_off_for` | chat.rs 1295-1305, 1318-1320 | `fn drop_news_for(&mut self, agent: AgentId)` | −2 |
| Three clamps of a cursor position into a rect | ui.rs 327-338 | `fn inside(rect: Rect, x: usize, y: usize) -> Position` | 0 |
| The `· ` mush signs its own lines with, in two tables | chat.rs 142, 188 | one `const MUSH_MARK: &str` | 0 |

---

## Looks duplicated but is not

These are pairs a lexer or a shape-scan will keep offering, and merging any of
them is a bug.

1. **`fit_row`'s `MIN_FIELD = 7` (text.rs 814) vs `marked`'s `MIN_BODY = 4`
   (chat.rs 2499).** A tree row spends *fields* and drops one whole rather than
   cut a brief to `cre…`; a message row keeps its words and drops the *mark*
   instead. One constant would either cut a brief that is not a word, or paint a
   message with no speaker.
2. **`select_painted`'s band vs the two `highlight_style`s.** Section 5 says the
   *style pair* should have one owner; the *shapes* must not merge. A list's
   highlight paints the row's whole cells — it is a place in the tree, and the
   picker's `› ` mark lives inside it — while the transcript's band is patched
   onto spans only, ends where the text ends, and skips a blank row entirely
   (ui.rs 183-224 is the reasoning, and the two tests at 531 and 644 read the
   difference cell by cell).
3. **The agents list and the picker list** (ui.rs 100-133 vs 374-390): the
   picker wants `Clear`, a `highlight_symbol("› ")` and a hint row; the tree row
   must not have a symbol (it already carries `▶`, and a symbol drawn outside the
   row's width was the bug the comment at 103-110 records), takes its rect from
   `pane.list_area` because the title's hidden-row counts were derived over it,
   and has a footer separator instead of a hint.
4. **`NoticeKind::mark` (chat.rs 140-147) vs `Voice::mark` (183-190).** The same
   signature over two vocabularies: what kind of line this is, and who said it.
   The `· ` they happen to share is mush signing its own words in both tables —
   that literal is worth one const, but a shared table would let a *speaker's*
   colour choose a *notice kind*'s mark, and `Voice::Mush`'s is dim precisely
   because it is not a speaker.
5. **`rank_style` (ui.rs 49-56) vs `footnote_lines`/`NoticeKind::mark`.** The bar
   colours a line by *rank* (Alert red, Activity the accent, Said gray); the foot
   colours it by *kind* (Info dim, stopped/cut-off yellow, error red). Same words,
   two palettes on purpose: the bar's activity line is chrome and wears the hue,
   the foot's is content. `Notice::rank` (234-241) is the one place the two
   vocabularies meet, which is what keeps the precedence table single.
6. **`FOOT_ROWS`/`FOOT_NOTE_ROWS` (chat.rs 82, 85) vs `capped_result`'s
   `SHOWN = 8` vs `MAX_TRANSCRIPT`.** Three caps over three quantities — rows the
   foot may take from a pane, rows one result may paint of its message, columns a
   pane is measured at — with different owners (`painted` derives the foot's room
   from the pane's height, `foot` spends it, `capped_result` owns the result's).
   Folding them into one table would tie the foot's size to the result cap.
7. **`lines_of` vs `Stops::of`.** Different questions — "does this message have
   words of its own" and "where does the pane's cap fall in it" — and already one
   road: `Stops::of` calls `lines_of`. Candidate 1 puts the *map* on that road
   too; these two stay.
8. **ui.rs's cursor clamps (327-338) vs `content_rows`/`Input::view` in
   screen.rs and input.rs.** The box's row arithmetic is derived once, at the
   edge: `content_rows` decides how many rows the text gets and `Input::view`
   windows them around the cursor, so the painter's
   `min(saturating_sub(1))`s are backstops for a view that outlived its rect, not
   a second derivation. (Read to judge the duplicate; the sites are another
   pass's.)

---

## The first thing I would do

**Candidate 1, in this order:** add `wrap_tagged` and `markdown_tagged_rows` to
`mush-core/src/text.rs` (the bodies of `wrap_capped` and `markdown_rows` with the
source-line index carried out), make `marked` return the tag of every row it
pushed, delete `mark_rows` and `View`, and let `capped_result` read its rows from
the same tags.

Why this one first: it is the largest saving (−55), it is the only pair of
*decisions* the tree keeps in step by hand (the fence rule in two files, the
reply's view chosen by the mark in one place and by the caller in another), and
its failure mode is the one a human notices as broken rather than as slow — the
select cursor highlighting one row while `Enter` copies another, with the two
`debug_assert`s that would have caught it compiled out of release. It also
requires no decision the tree has not already made: `marked` already decides the
view from the mark, so returning the map is only letting that decision reach the
key road.

Candidate 3 is the better *shape* fix (one breaker, and the tab-stop arithmetic
that has already been patched twice) and candidate 2 is the cheapest; but neither
turns a duplicated rule into a shared one the way candidate 1 does, and both are
easier to do after it — the breaker serves the tags, and `marked`'s single loop
is where candidate 2's `marked_row` lands.

## Method

- Read all four files end to end, production part (before each `mod tests`):
  2,903 + 404 + 855 + 368 non-blank lines, of which 1,607 + 274 + 533 + 230 are
  code (non-blank, non-`//`) = 2,644. Every saving and site figure below is in
  that currency, measured with `awk` over the exact line spans quoted.
- Shape scan: each file's code lines normalised (string literals → `"S"`,
  numbers → `N`, trimmed) and put through `sort | uniq -c | sort -rn`. That is
  what surfaced the four `out.push(std::mem::take(&mut current))` (candidate 3),
  the four `rows.resize(out.len() - start, None)`, the three
  `Style::default().fg(Color::Black).bg(theme.accent())`, the two
  `accent: Color::Cyan`, and the two `self.notices.retain`.
- Every candidate's sites were then compared line by line; for each
  "one arithmetic, two spellings" the divergence is stated as the *edit* that
  would make the copies disagree, and the code path that would then misbehave.
- Two facts were read where they are derived rather than counted as sites: the
  box's row arithmetic (`content_rows`, `Input::view`) and the pane-width floor
  (`MIN_WIDTH`, `Constraint::Min(20)`) — the second is what makes candidate 2's
  risk latent rather than live, and saying so is part of the finding.
- No builds, no test runs, no code changed. Only in-file doc comments and the
  files' own test names were read as evidence of intent; no `docs/` file, no
  session audit, no `README` was opened.
