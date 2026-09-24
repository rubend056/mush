//! The one grid a tool call is painted on, in both views.
//!
//! **Why a grid, and why the pane is its unit.** A log's turns are frequently
//! one call each — the model thinks, calls, thinks, calls — so a column that
//! lines up only within one message lines up with nothing. What the human reads
//! down a pane is a column of `→`s with the outcome beside each one, and that
//! column has to be a function of the pane's width *alone*: the pane caches
//! each message's rows on its own ([`super::chat`]'s `Chat::chunk`), so nothing
//! here may look at a neighbouring message, and a result message must never
//! need its producing call's width.
//!
//! **The grid.** One reading of a pane's width, `width`:
//!
//! ```text
//!   ❯ cargo test -p mush                       exit 101      41L 1.2KB
//!   │ 3 failed · 12 passed
//!   │←mark→│←  ask_w  →│←gap→│←  outcome_w    →│
//!   0      2            arrow_x-2    arrow_x   width
//! ```
//!
//! The second row is the call's own block: the pipe stands in the mark's two
//! columns ([`details`]), so a result's payload is painted under the header of
//! the call that made it without either row knowing the other's text.
//!
//! - `outcome_w = (width / 3).clamp(12, 28)` — the whole right-hand column,
//!   arrow included. A third of the pane is the news' share; 12 columns is the
//!   least an outcome sentence reads in (the arrow takes two, leaving ten), and
//!   28 stops the outcome from swallowing a wide terminal's pane.
//! - The outcome's own box is `outcome_w - 2`: `→ ` costs two columns.
//! - `arrow_x = width - outcome_w`, and `ask_w = arrow_x - 2` — the two columns
//!   before the arrow are the gap that keeps the ask from touching it.
//! - The ask starts after the call's own `mark` — the glyph and the space after
//!   it the tool's row wears ([`crate::app::symbols`]) — so its text budget is
//!   the ask column less **the mark's measured width**, never an assumed one.
//!   Both the ask and the outcome are cut with `…` from the right: the first
//!   clause of either is the one that matters, and the digest already orders an
//!   outcome's clauses exit-status first.
//!
//! **The outcome column is two sub-columns.** The `→` leads it and the news
//! sits behind it, left-aligned and in the tone's colour — `exit 101`, `4 hits`,
//! `#185 done` — while the payload the call produced is weighed at the pane's
//! **own right edge**, dim: `41L 1.2KB`. The verdict is the result's *news*
//! ([`CallFacts::outcome`]) and the measure is what its payload counts and
//! weighs ([`CallFacts::measure`]); a row whose result produced no payload — a
//! status, a control, a spawn, a wait — leaves the measure column empty, which
//! is uniform by column and honest by content. The two columns are functions of
//! `width` alone: the verdict starts at `arrow_x + 2` and the measure ends at
//! `width`, so no row has to know what a neighbour says. The arrow is the
//! column's own and not the news': a row whose result carried a payload but no
//! news keeps it (`▤ text.rs 1408→1530    →    123L/9000L 4KB`), so a call's row
//! wears the same shape at every width the split reaches — the one-clause
//! grammar paints that same row `→ 123L/9000L`.
//!
//! **The rung, and why a verdict is never crushed.** A row paints both clauses
//! only where the pane's box holds them both **whole**:
//!
//! ```text
//!   verdict_w + GAP + measure_w <= outcome_columns()
//! ```
//!
//! so the measure can never starve the verdict — the verdict is not cut down to
//! one column, a pair that does not fit falls back to the one-clause grammar the
//! pane had before the split (`→ exit 3`, `→ 4 hits`), and the verdict keeps the
//! whole box. Under [`SPLIT_FLOOR`] columns of box (`VERDICT_FLOOR` ten, the two
//! columns of air, and the narrowest measure a payload produces, `MEASURE_FLOOR`
//! six — `3L 13B`) *no* row splits, whatever it holds: a pane that narrow paints
//! one clause per call rather than two columns of crumbs. The box reaches
//! `SPLIT_FLOOR` at 60 columns — `(60 / 3) - 2` is 18 — which is the rung the
//! width sweep in `chat`'s matrix test pins from both sides.
//!
//! A one-clause row says the verdict where there is one and the measure's own
//! count where there is not (`→ 4 hits`, `→ 41L`): the count is the smallest
//! true thing the payload can say, and an arrow with nothing behind it is the
//! claim the grid refuses for a call whose result has not landed.
//!
//! **The two-row block.** The outcome is never dropped. When `ask_w` falls under
//! [`ASK_FLOOR`] — the ask's own column would be a name and three letters — the
//! ask keeps the pane's whole width and the outcome moves to its own row,
//! indented so its `→` sits in the same outcome column as the row above. That
//! two-row shape is a property of the layout rather than a special case: a pane
//! that is narrow for *any* reason gets the same block, and a call that later
//! wants a second row under its header ("`edit_file` occupies X to Y") has the
//! precedent here. A stacked pane is always under the rung (`ASK_FLOOR` bites
//! below 28 columns), so the stacked outcome is always the one-clause shape.
//!
//! **The ask is spans by role, and the shell's own line.** The mark is the
//! row's one bright thing and the call's named target keeps the ask's own
//! colour — a path, a pattern, a command's program *per stage* — while what
//! *qualifies* the target goes dim: a read's window, a search's `in crates ·
//! ignore_case`, a command's arguments and its `· background`, a control's
//! quoted words. A command is read at its own operators: the program of every
//! `|`, `;`, `&&` and `||` stage is a target and the rest of the stage is a
//! qualifier, and a `;`/`&&`/`||` operator is a [`Role::Seam`] — dim, and the
//! one place a row may end. A `|` is not a seam: a pipeline is one thing, and
//! it keeps its row. [`ask_pieces`] is the one place that decides which is
//! which; the painter only lays the spans out.
//!
//! **Two views, one grid.** The compact log gives every call exactly **one
//! row** — the dense view the whole design rests on — and that row is the one
//! the pane always painted: the ask cut from the right with `…`, its size
//! marker reserved before the cut. In the **unfolded** view the same ask is
//! painted across the rows it needs instead: a `;`/`&&`/`||` stage starts a
//! row, the operator stays at the end of its own row so no seam is invented, a
//! `|` keeps its stage where it is, and a word wider than a whole row is split
//! rather than cut — the unfolded view drops no byte of the ask. An ask the
//! ask's own column *does* hold is one row in both views. Continuation rows
//! hang under the mark. Neither view moves the other's columns: the arrow's
//! column, the verdict's place and the measure's right edge are functions of
//! the pane's width alone, so an ask that grows a row moves nothing but itself.
//!
//! **The compact row's cut ends at a seam.** Where the ask does not fit one
//! row and its line has stages, the row paints the stages it holds whole, the
//! next stage as far as the row goes, the `…` that says the line went on, and
//! the count of the stages that `…` left behind: `❯ wc -l tools.rs ; grep … ;
//! +2 stages`. The count is the shell's own split ([`stages`]) and never an
//! estimate; a command with no separator keeps the plain cut it has always had,
//! a cut never leaves an operator with nothing behind it, and where the count's
//! own words do not fit the row — or would leave no ask beside them
//! ([`TAIL_FLOOR`]) — the pane says nothing it cannot say whole.
//!
//! **The cwd chip.** A command's leading `cd <dir> &&` is where the line ran —
//! `crate::agent`'s digest reads it off the command and carries it in
//! [`crate::agent::CallFacts::cwd`], and it is not part of the ask. Where the
//! directory is *not* the workspace root the row wears it, dim and bracketed,
//! at its head: `[.mush/wt/198] ❯ git log …`. In the root it is dropped (every
//! command already runs there) and a subdirectory is shown workspace-relative;
//! it is recovered from the command the model wrote, never guessed.
//!
//! **The script a call carries.** A `run_command` whose first line opens a
//! heredoc — `python3 - <<'PY'`, `git commit -F - <<'EOF'` — holds the actual
//! work in its arguments' *later* lines, which no row of the ask could show
//! (the ask is the first line, and the compact log is one row). The unfolded
//! view paints the body under the ask, at the call's own gutter, dim: one row
//! that says what it is — `script 40L · input, not output`; it is the
//! command's **input** and not the payload under it, which is its output — and
//! then the body folded by the payload's own rule (the first rows, an `… N
//! lines …` row, the last rows), so a forty-line script costs the rows a
//! payload costs. The compact log paints none of it: its one row is the whole
//! design ([`details`]).
//!
//! **The text a writer's ask carries.** An `edit_file` and a `write_file` are
//! the two calls whose ask **is** text — a replacement, a whole file — and whose
//! results are one sentence each, so their rows named the file and never showed
//! a byte of what the model sent. The unfolded view paints it, at the same
//! gutter and in the same dim rows a script is painted in: an edit's replaced
//! and new lines, edit by edit, behind a row that counts them (`diff 2 edits ·
//! +4−3`), and a write's content behind a row that says what it is (`write 41L ·
//! content, not output`).
//!
//! It is the **ask**, and not a diff of the file. The arguments carry no line
//! numbers and no surrounding text, so the block invents neither: no `@@`
//! header, no context line, and no claim about whether the replacement matched —
//! that is the result's news alone (`3 hunks`, or the `! error: …` row a refusal
//! earns). The `−`/`+` marks tell the arguments' own two strings apart, and a
//! write's lines wear no mark at all: a write has no old text to have been
//! diffed against, so a `+` on its lines would be a diff this painter never
//! took. Both blocks are painted whether the call landed or failed, because the
//! arguments are what the model sent and a refused edit's exact strings are what
//! its failure row is about; the measure at the pane's edge is the half that *is*
//! gated, because a call that failed wrote nothing
//! ([`crate::agent::Measure`]).
//!
//! Both are folded by the payload's own rule
//! ([`crate::app::chat::folded_head_tail`]) — a five-hundred-line edit costs the
//! eight rows a payload costs, with the counts in its lead row read from the
//! whole strings — and every row is cut to the pane's columns. The compact log
//! paints none of it: its one row is the whole design ([`details`]).
//!
//! **The arguments' size marker.** A call whose arguments as sent are
//! [`ARGUMENT_MARKER`] bytes or more wears their size after the ask, dim and
//! parenthesised: `❯ python3 - <<'PY' (3KB)`. The parentheses are the whole
//! signal — the number is not an argument and must not read as one — and the
//! marker is **reserved before the ask is cut**, so a long command's own size is
//! never eaten by the very cut it explains. It is measured here from the call
//! the painter holds: it is a fact about the arguments' size, not about what
//! they asked, so the digest does not carry it.
//!
//! **No row is ever wider than the pane.** Every row is cut to its own columns,
//! and at a width below the mark's own four columns the rows degenerate to what
//! fits rather than overflow — the sweep in `chat`'s tests pins it from 20 to
//! 200, and a unit test below pins the refuse-to-overflow below that.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_truncate::UnicodeTruncateStr;
use unicode_width::UnicodeWidthStr;

use mush_core::text::truncate;
use mush_core::tools::ToolName;
use mush_core::ToolCall;

use crate::agent::{CallFacts, Measure, Tone};
use crate::app::size_label;
use crate::ui::dim;

/// The gap between the ask's column and the arrow: two columns, so the `…` of a
/// cut ask never touches the `→`.
const GAP: usize = 2;

/// The pipe a call's block stands at, where this module pads a row by hand
/// instead of through [`crate::app::symbols::Symbols::GUTTER_MARK`]: the same
/// character, read from the glyph table where the gutter's own spelling is
/// authored, so the two cannot drift apart.
const PIPE: char = crate::app::symbols::Symbols::GUTTER_PIPE;

/// The sign the diff block paints before a line an `edit_file`'s `old_string`
/// holds, and the one before a line its `new_string` holds ([`writer_rows`]):
/// the glyph table's own two ([`crate::app::symbols::Symbols::REMOVED`]), read
/// from it so the block, the table and the tree's `+12−3` cannot drift apart.
const REMOVED: char = crate::app::symbols::Symbols::REMOVED;
const ADDED: char = crate::app::symbols::Symbols::ADDED;

/// The words that say what a heredoc's body is where it is painted
/// ([`script_rows`]): the command's **input**, and not the payload below it.
/// The distinction is the whole point of the row — a script stands exactly
/// where a result's dump stands ([`details`]) — so it is spelled once, here,
/// and pinned by a test.
const SCRIPT: &str = "· input, not output";

/// The mark a cut ask ends with, spelled where the cut writes it by hand
/// ([`cut_stages`]): the same one [`truncate`] appends to a piece it shortens,
/// so a row's cut looks the same whichever of the two made it.
const ELLIPSIS: &str = "…";

/// The columns [`ELLIPSIS`] takes: one, and named here so the arithmetic that
/// reserves them ([`cut_stages`]) reads as what it is.
const ELLIPSIS_W: usize = 1;

/// The outcome column's bounds. See the module doc for why these two.
const OUTCOME_MIN: usize = 12;
const OUTCOME_MAX: usize = 28;

/// The narrowest ask column that still shares its row with the outcome: the
/// mark plus ten columns of ask. Under it the outcome takes its own row.
const ASK_FLOOR: usize = 14;

/// The verdict's own columns: the room the news keeps before the measure is
/// dropped from the row. `cancelled` and `exit 101` fit whole; anything shorter
/// has room to spare, and a verdict longer than this is a *sentence* (`no
/// children and no jobs`) whose row has no measure to share the box with.
const VERDICT_FLOOR: usize = 10;

/// The narrowest measure a payload produces: a one-line read, `3L 13B`. A box
/// that cannot hold this beside a verdict's floor cannot hold the split at all.
const MEASURE_FLOOR: usize = 6;

/// The rung: the outcome box's own columns at which the two-clause grammar
/// begins. Under it every call paints one clause ([`Grid::splits`]).
const SPLIT_FLOOR: usize = VERDICT_FLOOR + GAP + MEASURE_FLOOR;

/// How many bytes of a call's arguments earn the size marker. The number the
/// human named: past it a command is a program the model wrote rather than one
/// it named, and the row says so — `(3KB)`.
const ARGUMENT_MARKER: usize = 300;

/// The columns of ask a row must have behind its head before the unfolded view
/// wraps it: the head *and* four columns. A row that would hold one letter is
/// not a row, so a pane that narrow clips the ask the way it always did (the
/// compact row's own cut) rather than growing a column a line.
const WRAP_FLOOR: usize = 4;

/// The columns of ask the cwd chip must leave behind the mark before a row
/// wears it: the chip is a fact *about* the line and never the line, so where
/// the ask's column cannot hold the chip, the mark and this many columns of the
/// command behind them, the chip is dropped and the row reads as the command it
/// is. Ten is the smallest ask that still says what ran — `cargo test` — and the
/// floor is what keeps a 28-column pane's mark on its own row (a chip fifteen
/// columns wide would otherwise cut the mark off it).
const CHIP_FLOOR: usize = 10;

/// The columns of ask the compact row keeps for itself before it says what its
/// cut left: a row that holds the count and no line at all — the head, a gap,
/// ` ; +2 stages` — is a count of nothing, and the plain cut shows more of the
/// command than the count would. So the tail is painted only where it leaves
/// this many columns of the ask's own text behind it.
const TAIL_FLOOR: usize = 8;

/// The header's geometry at one pane width. Built once per call row, and a pure
/// function of the width — the property the pane's per-message cache rests on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Grid {
    width: usize,
    outcome_w: usize,
    arrow_x: usize,
    ask_w: usize,
}

impl Grid {
    /// The grid a `width`-column pane paints a call on.
    pub(crate) fn of(width: usize) -> Self {
        let outcome_w = (width / 3).clamp(OUTCOME_MIN, OUTCOME_MAX);
        let arrow_x = width.saturating_sub(outcome_w);
        let ask_w = arrow_x.saturating_sub(GAP);
        Grid {
            width,
            outcome_w,
            arrow_x,
            ask_w,
        }
    }

    /// The column the `→` of every call sits at, outcome or not.
    pub(crate) fn arrow_x(self) -> usize {
        self.arrow_x
    }

    /// Whether the outcome has a row of its own: the ask's column cannot hold
    /// both. See [`ASK_FLOOR`].
    pub(crate) fn stacked(self) -> bool {
        self.ask_w < ASK_FLOOR
    }

    /// Whether this pane's box can hold the two-clause grammar at all — the
    /// verdict's floor, the air between, and the narrowest measure ([`SPLIT_FLOOR`]).
    /// A row still splits only where the two clauses it really holds fit whole;
    /// this is the pane's own half of the rule.
    pub(crate) fn splits(self) -> bool {
        self.outcome_columns() >= SPLIT_FLOOR
    }

    /// The columns row one has for the mark and the ask: the ask's own column
    /// when the outcome shares the row, the whole pane when it does not.
    pub(crate) fn ask_columns(self) -> usize {
        if self.stacked() {
            self.width
        } else {
            self.ask_w
        }
    }

    /// The columns the outcome's own box has, the arrow's two already spent —
    /// and never more than the pane has left of the arrow, so the degenerate
    /// widths below [`OUTCOME_MIN`] clip instead of overflow.
    pub(crate) fn outcome_columns(self) -> usize {
        let after_arrow = self.width.saturating_sub(self.arrow_x.saturating_add(2));
        self.outcome_w.saturating_sub(2).min(after_arrow)
    }
}

/// One call's header: the mark — which *is* the tool's name — the ask in its
/// roles, and, where the result has landed, its two right-hand clauses: the
/// verdict at the arrow and the measure at the pane's own right edge — or, on a
/// pane too narrow for both, the one clause it can hold.
///
/// `mark` is the call's own — the tool's glyph and the space after it
/// ([`crate::app::symbols`]) — and it is **measured**, not assumed: the ask
/// stands behind it in the ask's column whatever the glyph's width is. A call
/// with no result yet paints the ask alone: a `→` with nothing after it would
/// be a claim about a phase the transcript cannot see, and the ask keeps
/// exactly the columns it will have when the result lands, so a row does not
/// jump under the human's eyes.
///
/// `ask_rows` is the view's own ([`AskRows`]): the compact log's one row, or
/// the unfolded view's as many rows as the ask needs. Whatever it is, the
/// clauses sit on the ask's **last** row — the arrow at its column and the
/// measure at the pane's edge — so the views share one grid and an ask that
/// grew a row moved nothing but itself.
pub(crate) fn header(
    call: &ToolCall,
    facts: &CallFacts,
    width: usize,
    mark: &str,
    ask_rows: AskRows,
) -> Vec<Line<'static>> {
    let grid = Grid::of(width);
    let ask = ask_pieces(call, facts, mark);
    let rows: Vec<Vec<Span<'static>>> = match ask_rows {
        AskRows::One => vec![ask_spans(ask.cut(grid.ask_columns()))],
        AskRows::Many => ask
            .rows(grid.ask_columns())
            .into_iter()
            .map(ask_spans)
            .collect(),
    };
    // The two right-hand clauses, measured before either is painted: the
    // arithmetic that decides the row's shape is the same arithmetic that lays
    // it out.
    let measure = facts
        .measure
        .as_ref()
        .map(Measure::text)
        .filter(|text| !text.is_empty());
    let measure_w = measure.as_deref().map_or(0, UnicodeWidthStr::width);
    let verdict_w = facts
        .outcome
        .as_ref()
        .map_or(0, |outcome| UnicodeWidthStr::width(outcome.text.as_str()));
    if measure.is_none() && facts.outcome.is_none() {
        // No result yet, or a result with nothing at all to say: the ask alone.
        return rows.into_iter().map(Line::from).collect();
    }
    // The split: both clauses whole, in the pane's box, above the rung.
    let split = measure.is_some()
        && !grid.stacked()
        && grid.splits()
        && verdict_w + GAP + measure_w <= grid.outcome_columns();
    if !split {
        return one_clause(grid, rows, facts);
    }
    let mut rows = rows;
    let mut spans = rows.pop().expect("a header is never no rows");
    let ask_w = painted_width(&spans);
    if grid.arrow_x() > ask_w {
        spans.push(Span::styled(" ".repeat(grid.arrow_x() - ask_w), dim()));
    }
    // The arrow belongs to the column and not to the news: a row with a
    // right-hand clause at all wears it, and the news follows where the result
    // carried one. A read's row (`▤ text.rs 1408→1530    →    123L/9000L 4KB`)
    // keeps the `→` its one-clause twin paints, so widening a pane never takes
    // a mark off a call.
    spans.push(Span::styled("→ ", dim()));
    if let Some(outcome) = &facts.outcome {
        spans.push(Span::styled(outcome.text.clone(), tone_style(outcome.tone)));
    }
    let measure = measure.expect("a split needs a measure");
    // The measure is right-aligned at the pane's own edge: the fill is what is
    // left between the verdict and it, and the columns there are all a pane
    // too narrow for the pair never reaches ([`split`]).
    let used = grid.arrow_x().saturating_add(GAP).saturating_add(verdict_w);
    let fill = grid.width.saturating_sub(used).saturating_sub(measure_w);
    if fill > 0 {
        spans.push(Span::styled(" ".repeat(fill), dim()));
    }
    spans.push(Span::styled(
        truncate(&measure, grid.width.saturating_sub(used)),
        dim(),
    ));
    rows.push(spans);
    rows.into_iter().map(Line::from).collect()
}

/// The one-clause shape: the news at the arrow, or — where the result's only
/// clause is its payload's count — the count, because a `→` with nothing behind
/// it is the claim the grid refuses. A pane whose ask cannot share the row (the
/// [`ASK_FLOOR`] block) gives the outcome a row of its own.
///
/// The clause rides the ask's **last** row: the rows an unfolded ask grew stay
/// the ask's, and the arrow it already had keeps its column.
fn one_clause(
    grid: Grid,
    mut rows: Vec<Vec<Span<'static>>>,
    facts: &CallFacts,
) -> Vec<Line<'static>> {
    let clause = match &facts.outcome {
        Some(outcome) => truncate(&outcome.text, grid.outcome_columns()),
        None => match &facts.measure {
            // The count and not the size: the narrow grammar keeps the smallest
            // true clause, and a size alone (a picture's `4KB`) is the count
            // when the payload has no countable rows.
            Some(measure) => truncate(&measure.brief(), grid.outcome_columns()),
            None => String::new(),
        },
    };
    let tone = match &facts.outcome {
        Some(outcome) => outcome.tone,
        None => Tone::None,
    };
    if grid.stacked() {
        let mut lines: Vec<Line<'static>> = rows.into_iter().map(Line::from).collect();
        lines.push(clause_row(grid, &clause, tone));
        return lines;
    }
    let mut spans = rows.pop().expect("a header is never no rows");
    let ask_w = painted_width(&spans);
    if grid.arrow_x() > ask_w {
        spans.push(Span::styled(" ".repeat(grid.arrow_x() - ask_w), dim()));
    }
    spans.push(Span::styled("→ ", dim()));
    spans.push(Span::styled(clause, tone_style(tone)));
    rows.push(spans);
    rows.into_iter().map(Line::from).collect()
}

/// The outcome's row when it stands alone: the arrow at the grid's own column,
/// under the row above's.
fn clause_row(grid: Grid, clause: &str, tone: Tone) -> Line<'static> {
    let pad = grid.arrow_x().min(grid.width);
    // The row is assembled before it is padded, so a pane too narrow even for
    // `→ ` clips the sentence instead of overflowing its columns.
    let content = truncate(&format!("→ {clause}"), grid.width.saturating_sub(pad));
    let mut spans = vec![Span::styled(" ".repeat(pad), dim())];
    match content.strip_prefix("→ ") {
        Some(rest) => {
            spans.push(Span::styled("→ ", dim()));
            spans.push(Span::styled(rest.to_string(), tone_style(tone)));
        }
        // A cut that landed inside the arrow itself: what is left is the arrow's
        // own dim text, and there is no outcome left to colour.
        None => spans.push(Span::styled(content, dim())),
    }
    Line::from(spans)
}

/// One piece of an ask, and the role it is painted in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// The call's named target — a path, a pattern, a program: what the call is
    /// *about*, painted in the ask's own colour.
    Named,
    /// What qualifies it — a window, `in crates`, a command's arguments, the
    /// flags it ran under: dim.
    Qualifier,
    /// The shell operator that ends a stage — `;`, `&&`, `||`: dim like a
    /// qualifier, and the one place a row may end ([`AskRows::Many`]). A `|`
    /// is not one: a pipeline is one thing, and it keeps its row.
    Seam,
    /// The `(3KB)` size marker: dim, and reserved before the ask is cut
    /// ([`Ask::cut`]).
    Marker,
}

/// One span of the ask before it is styled: its text and its role.
type Piece = (String, Role);

/// Which of the pane's two views is painting an ask, and so how many rows the
/// ask may take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AskRows {
    /// The compact log: **one row**, the dense view the whole design rests on.
    /// The ask is cut to it, at a seam where its line has one ([`Ask::cut`]).
    One,
    /// The unfolded view: the rows the ask needs, broken at the shell's own
    /// seams and hung under the mark ([`Ask::rows`]).
    Many,
}

/// A call's ask as the pane paints it: the row's **head** — the cwd chip where
/// the call's own `cd` named one, and the mark — and the ask's own **body**.
///
/// The head is outside the body on purpose: the body *is* the ask, so it joins
/// back into it exactly (the property the roles rest on), while the chip is a
/// fact the command's own text no longer carries
/// ([`crate::agent::CallFacts::cwd`]).
struct Ask {
    /// The chip and the mark, in that order: what the row wears before its
    /// first column of ask.
    head: Vec<Piece>,
    /// The ask's own pieces, in their roles, seams marked.
    body: Vec<Piece>,
}

impl Ask {
    /// The row's head at one budget: the chip and the mark, or — where the ask
    /// cannot hold the chip, the mark and [`CHIP_FLOOR`] columns of ask behind
    /// them — the mark alone. The chip is the first thing a narrow pane gives
    /// up, because a call's identity is its mark and the line it ran, and a
    /// chip that ate either would be the row's whole content.
    fn head(&self, budget: usize) -> Vec<Piece> {
        let mark: Vec<Piece> = self.head.last().cloned().into_iter().collect::<Vec<_>>();
        if self.head.len() == 1 || painted_pieces(&self.head) + CHIP_FLOOR <= budget {
            self.head.clone()
        } else {
            mark
        }
    }

    /// The columns a continuation row hangs under: the mark's own width. The
    /// chip is the first row's head and no other row's, so a wrapped ask hangs
    /// under the mark it wears rather than under the chip that names its cwd.
    fn hang(&self) -> usize {
        self.head
            .last()
            .map_or(0, |(text, _)| UnicodeWidthStr::width(text.as_str()))
    }

    /// The ask on one row, cut to `budget` columns — the compact log's row.
    ///
    /// The head is reserved first and the size marker second, because the cut
    /// is what pays for them: the mark is the call's identity on its row (and a
    /// chip is wide enough that a narrow pane would otherwise cut the mark off
    /// the row the chip names), and the marker is the size *of* the ask, so the
    /// cut it explains must not eat it. What is left is the body, cut by
    /// [`cut_seams`]: the stages the row holds whole, the next stage as far as
    /// the row goes, and the `…` and count of the stages behind it — or, where
    /// the line has no stage to end at, the plain cut from the right.
    fn cut(&self, budget: usize) -> Vec<Piece> {
        let head = self.head(budget);
        let mut row = cut_pieces(&head, budget);
        let room = budget.saturating_sub(painted_pieces(&row));
        let marker = match self.body.last() {
            Some((text, Role::Marker)) => Some((text.clone(), Role::Marker)),
            _ => None,
        };
        let body = match marker {
            Some(_) => &self.body[..self.body.len() - 1],
            None => &self.body[..],
        };
        let marker_w = marker
            .as_ref()
            .map_or(0, |(text, _)| UnicodeWidthStr::width(text.as_str()));
        row.extend(cut_seams(body, room.saturating_sub(marker_w)));
        if let Some((text, role)) = marker {
            let room = budget.saturating_sub(painted_pieces(&row));
            if room > 0 {
                row.push((truncate(&text, room), role));
            }
        }
        row
    }

    /// The ask across the rows it needs: a `;`/`&&`/`||` stage starts a row and
    /// its operator ends the row that stage's text is in ([`Role::Seam`]), a
    /// `|` and the words around it keep their row, a word wider than a whole
    /// row is split rather than cut — the unfolded view gives the ask the rows
    /// it needs and drops no byte of it — and the rows after the first hang
    /// under the mark (their own blank head, the columns the mark took).
    ///
    /// An ask the pane holds is **one row**, whatever view paints it: wrapping
    /// is what a call does when it has to, and a chain the ask column holds is
    /// the line the shell ran. The compact row is the floor below that: a pane
    /// whose ask column cannot hold the head and [`WRAP_FLOOR`] columns clips
    /// the ask as it always did ([`Ask::cut`]) rather than wrapping into a
    /// column of letters.
    fn rows(&self, budget: usize) -> Vec<Vec<Piece>> {
        let head = self.head(budget);
        let lead = painted_pieces(&head);
        if budget < lead + WRAP_FLOOR {
            return vec![self.cut(budget)];
        }
        if lead + painted_pieces(&self.body) <= budget {
            let mut row = head;
            row.extend(self.body.iter().cloned());
            return vec![row];
        }
        let hang = self.hang();
        let atoms = atoms(&self.body);
        let mut rows: Vec<Vec<Piece>> = vec![head];
        // The columns the row in hand has taken, and the columns it started
        // with: a fresh row starts at `hang` (the first one at `lead`), and a
        // fresh row drops an atom's leading whitespace instead of painting it
        // at the margin.
        let mut used = lead;
        let mut start = lead;
        for (at, atom) in atoms.iter().enumerate() {
            let mut rest = atom.word.clone();
            loop {
                let room = budget.saturating_sub(used).saturating_sub(atom.reserve);
                let text = if used == start {
                    rest.clone()
                } else {
                    format!("{}{rest}", atom.lead)
                };
                let width = UnicodeWidthStr::width(text.as_str());
                if width <= room {
                    if !text.is_empty() {
                        rows.last_mut()
                            .expect("a row is open")
                            .push((text, atom.role));
                        used += width;
                    }
                    break;
                }
                if used == start {
                    // Wider than the whole row: the unfolded view splits it.
                    // A fresh row always has room ([`WRAP_FLOOR`]), and `max`
                    // keeps that a fact rather than a hope. [`UnicodeTruncateStr`]
                    // hands back the prefix and its own width — the rest of the
                    // word is the text that prefix did not take, which is the
                    // half of the answer this needs.
                    let (head, _) = text.unicode_truncate(room.max(1));
                    if !head.is_empty() {
                        rows.last_mut()
                            .expect("a row is open")
                            .push((head.to_string(), atom.role));
                    }
                    rest = text[head.len()..].to_string();
                    rows.push(vec![(" ".repeat(hang), Role::Qualifier)]);
                    used = hang;
                    start = hang;
                    continue;
                }
                // The word does not fit what is left of this row: the row ends
                // here and the next one takes it whole.
                rows.push(vec![(" ".repeat(hang), Role::Qualifier)]);
                used = hang;
                start = hang;
            }
            // **A stage starts a row**: the operator that ended the stage just
            // placed is the last thing on its row, and the stage behind it
            // begins its own — which is what makes a wrapped chain read as the
            // stages the shell ran rather than as one long line.
            if atom.role == Role::Seam && at + 1 < atoms.len() {
                rows.push(vec![(" ".repeat(hang), Role::Qualifier)]);
                used = hang;
                start = hang;
            }
        }
        rows
    }
}

/// The ask in the roles the pane paints it in — **the one place** that decides
/// which part of a call's ask is its target and which part qualifies it — as
/// the head the row leads with and the body it hangs behind it.
///
/// The mark is the head's last piece and always [`Role::Named`]; the cwd chip
/// leads it where the call's own command named a directory ([`CallFacts::cwd`])
/// and is a qualifier, because where a line ran qualifies what it ran. A tool
/// the table knows joins its ask to the mark directly (the mark carries its own
/// trailing space); a name no tool answers to keeps the generic mark *and* its
/// own name — `⚙ frobnicate x` — because `⚙` alone would say nothing about a
/// call mush has never heard of.
fn ask_pieces(call: &ToolCall, facts: &CallFacts, mark: &str) -> Ask {
    let mut head: Vec<Piece> = Vec::new();
    if let Some(cwd) = &facts.cwd {
        head.push((format!("[{cwd}] "), Role::Qualifier));
    }
    head.push((mark.to_string(), Role::Named));
    let mut body: Vec<Piece> = match ToolName::parse(&call.function.name) {
        Some(tool) => ask_roles(tool, &facts.ask),
        None => {
            let mut pieces = vec![(call.function.name.clone(), Role::Named)];
            if !facts.ask.is_empty() {
                pieces.push((format!(" {}", facts.ask), Role::Qualifier));
            }
            pieces
        }
    };
    if let Some(marker) = argument_marker(call) {
        body.push((format!(" {marker}"), Role::Marker));
    }
    Ask { head, body }
}

/// The per-tool reading of an ask's roles. The tools that take no arguments
/// (`status`, and a bare `wait`) have an empty ask and no piece after the mark.
///
/// A target is what the call is *about*: a path, a pattern, a program, the id a
/// spawn was given. Everything the call reads the target *through* — a window,
/// where a search looked, the case it ran under, a command's arguments, the
/// flags it ran under, the words a control carries — is a qualifier and dim.
fn ask_roles(tool: ToolName, ask: &str) -> Vec<Piece> {
    if ask.is_empty() {
        return Vec::new();
    }
    match tool {
        // A command is read per stage: each `|`/`;`/`&&`/`||` stage's program
        // is a target it ran, that program's arguments qualify it, and the
        // operator that ends a stage is the [`Role::Seam`] a row may end at.
        ToolName::RunCommand => stages(ask),
        // A search's pattern is what was looked for; where and how it looked
        // (`in crates · ignore_case`) are the qualifiers.
        ToolName::Search => match ask.find("\" in ") {
            Some(at) => vec![
                (ask[..at + 1].to_string(), Role::Named),
                (ask[at + 1..].to_string(), Role::Qualifier),
            ],
            None => vec![(ask.to_string(), Role::Named)],
        },
        // A read's window is the one thing that qualifies the file it names.
        ToolName::ReadFile => match ask.rsplit_once(' ') {
            Some((head, tail)) if is_window(tail) => vec![
                (head.to_string(), Role::Named),
                (format!(" {tail}"), Role::Qualifier),
            ],
            _ => vec![(ask.to_string(), Role::Named)],
        },
        // `#4 message "the words"`: the id and the action are the target, the
        // quoted words are what it carries.
        ToolName::Control => match ask.find(" \"") {
            Some(at) => vec![
                (ask[..at].to_string(), Role::Named),
                (ask[at..].to_string(), Role::Qualifier),
            ],
            None => vec![(ask.to_string(), Role::Named)],
        },
        // A path, a title, a wait's target, a symbol's own spelling: nothing
        // qualifies them here.
        ToolName::Outline
        | ToolName::WriteFile
        | ToolName::EditFile
        | ToolName::ListFiles
        | ToolName::Usages
        | ToolName::SpawnAgent
        | ToolName::Wait => vec![(ask.to_string(), Role::Named)],
        ToolName::Status => Vec::new(),
    }
}

/// Whether an ask's last word is a read's window: `5→7`, `1408→`, spelled
/// exactly as `agent::read_ask` writes one — digits, the arrow, and
/// digits or nothing.
fn is_window(word: &str) -> bool {
    let Some((start, end)) = word.split_once('→') else {
        return false;
    };
    !start.is_empty()
        && start.bytes().all(|byte| byte.is_ascii_digit())
        && end.bytes().all(|byte| byte.is_ascii_digit())
}

/// A command's ask, one piece per run of its own shell line: each stage's
/// program — its first word — is a target, everything around it a qualifier,
/// and the operator that ends a stage its own [`Role::Seam`] piece. The pieces
/// join back into the ask exactly, so the roles never change what the row says.
///
/// The stages are the shell's own ([`operators`]), which is what keeps a `;`
/// inside a quoted pattern a character of the pattern: `grep -n "a;b" f` has
/// one stage and no seam, and a row never breaks inside the quotes.
fn stages(ask: &str) -> Vec<Piece> {
    let mut pieces: Vec<Piece> = Vec::new();
    let mut from = 0;
    for op in operators(ask) {
        // A heredoc's `<<` is a redirection and not a stage: its delimiter
        // stays part of the stage it opens, in the qualifier that carries it.
        if op.text == "<<" {
            continue;
        }
        push_stage(&mut pieces, &ask[from..op.at]);
        pieces.push((
            op.text.to_string(),
            if op.seam { Role::Seam } else { Role::Qualifier },
        ));
        from = op.at + op.text.len();
    }
    push_stage(&mut pieces, &ask[from..]);
    pieces
}

/// One run of text between two of a command's operators, read into its roles:
/// the whitespace around it and everything after its first word are qualifiers,
/// and the first word — the program the stage ran — is the target. An empty run
/// (two operators touching) contributes nothing at all.
fn push_stage(pieces: &mut Vec<Piece>, stage: &str) {
    let lead = stage.len() - stage.trim_start().len();
    if lead > 0 {
        pieces.push((stage[..lead].to_string(), Role::Qualifier));
    }
    let rest = &stage[lead..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    if end > 0 {
        pieces.push((rest[..end].to_string(), Role::Named));
    }
    if end < rest.len() {
        pieces.push((rest[end..].to_string(), Role::Qualifier));
    }
}

/// One operator a shell line carries: where it starts, its own spelling, and
/// whether it *ends a stage*. The two that matter to a row are the seam (`;`,
/// `&&`, `||` — a stage starts a row at it) and the heredoc's `<<`, whose body
/// the unfold paints under the ask ([`scripts`]). A `|` is neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Operator {
    at: usize,
    text: &'static str,
    seam: bool,
}

/// The shell's own operators in one line, in order, read outside quotes: a `;`
/// or a `|` inside `"…"` is a character of a pattern and neither a seam nor a
/// heredoc, a `\` escapes the byte after it outside single quotes, and a `#`
/// where a word starts runs to the end of the line.
///
/// One reader for the two questions these rows ask of a command — where its
/// stages end ([`stages`]) and where its heredocs open ([`scripts`]) — because
/// the two must agree about what a quote hides.
fn operators(line: &str) -> Vec<Operator> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    let mut quote: Option<u8> = None;
    // Whether the byte at the cursor starts a word: the one thing a `#` needs
    // to be a comment rather than a parameter (`$#`).
    let mut word = true;
    while at < bytes.len() {
        let ch = bytes[at];
        match quote {
            Some(b'\'') => {
                if ch == b'\'' {
                    quote = None;
                }
                at += 1;
            }
            // A double quote: the backslash escapes inside it too.
            Some(_) => {
                if ch == b'\\' {
                    at += 2;
                    continue;
                }
                if ch == b'"' {
                    quote = None;
                }
                at += 1;
            }
            None => match ch {
                b'\\' => {
                    at += 2;
                    word = false;
                }
                b'\'' | b'"' => {
                    quote = Some(ch);
                    at += 1;
                    word = false;
                }
                b'#' if word => break,
                b';' => {
                    out.push(Operator {
                        at,
                        text: ";",
                        seam: true,
                    });
                    at += 1;
                    word = true;
                }
                b'&' if bytes.get(at + 1) == Some(&b'&') => {
                    out.push(Operator {
                        at,
                        text: "&&",
                        seam: true,
                    });
                    at += 2;
                    word = true;
                }
                b'|' if bytes.get(at + 1) == Some(&b'|') => {
                    out.push(Operator {
                        at,
                        text: "||",
                        seam: true,
                    });
                    at += 2;
                    word = true;
                }
                b'|' => {
                    out.push(Operator {
                        at,
                        text: "|",
                        seam: false,
                    });
                    at += 1;
                    word = true;
                }
                // `<<` is a heredoc; `<<<` is a herestring and opens none, so
                // it is stepped over whole.
                b'<' if bytes.get(at + 1) == Some(&b'<') => {
                    if bytes.get(at + 2) == Some(&b'<') {
                        at += 3;
                    } else {
                        out.push(Operator {
                            at,
                            text: "<<",
                            seam: false,
                        });
                        at += 2;
                    }
                    word = false;
                }
                byte if byte.is_ascii_whitespace() => {
                    at += 1;
                    word = true;
                }
                _ => {
                    at += 1;
                    word = false;
                }
            },
        }
    }
    out
}

/// A heredoc a command opens with its own first line, with the lines a shell
/// would read at its delimiter.
struct Script {
    /// The body, one entry per line, the delimiter's own line excluded.
    body: Vec<String>,
}

/// Every heredoc a command opens **on its first line**, in order, each with the
/// lines up to its delimiter: the quoted or bare delimiter spellings a shell
/// honours (`<<'PY'`, `<<"PY"`, `<<\PY`, `<<PY`, and `<<-PY`'s tab-stripped
/// terminator).
///
/// Only the first line opens one: the ask is the first line (`crate::agent`'s
/// `command_ask`), so a body whose `<<` is not on the row the human reads has
/// nothing to hang under. A delimiter that never closes takes the lines that
/// are there, as a shell reading to the end of its input would.
fn scripts(command: &str) -> Vec<Script> {
    let mut lines = command.split('\n');
    let Some(head) = lines.next() else {
        return Vec::new();
    };
    let opens: Vec<(String, bool)> = operators(head)
        .into_iter()
        .filter(|op| op.text == "<<")
        .filter_map(|op| heredoc_word(&head[op.at + 2..]))
        .collect();
    let mut rest: Vec<&str> = lines.collect();
    let mut scripts = Vec::new();
    for (delimiter, tabs) in opens {
        let mut body = Vec::new();
        let mut at = 0;
        while at < rest.len() {
            let line = rest[at];
            at += 1;
            // `<<-` strips leading tabs from the line the shell looks for; a
            // body line keeps its own bytes either way.
            let ends = if tabs {
                line.trim_start_matches('\t') == delimiter
            } else {
                line == delimiter
            };
            if ends {
                break;
            }
            body.push(mush_core::text::sanitize(line));
        }
        rest = rest[at..].to_vec();
        scripts.push(Script { body });
    }
    scripts
}

/// One heredoc's delimiter as the shell reads it: the word after the `<<`, with
/// its quotes and escapes taken out, and whether `<<-` strips the tabs of the
/// line that closes it. `None` for a `<<` with no word after it.
fn heredoc_word(rest: &str) -> Option<(String, bool)> {
    let (tabs, rest) = match rest.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, rest),
    };
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut chars = rest.trim_start().chars();
    while let Some(ch) = chars.next() {
        match quote {
            Some(end) => {
                if ch == end {
                    quote = None;
                } else {
                    word.push(ch);
                }
            }
            None => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => {
                    if let Some(next) = chars.next() {
                        word.push(next);
                    }
                }
                ch if ch.is_whitespace()
                    || matches!(ch, ';' | '|' | '&' | '<' | '>' | '(' | ')') =>
                {
                    break;
                }
                ch => word.push(ch),
            },
        }
    }
    (!word.is_empty()).then_some((word, tabs))
}

/// One atom of an ask's body: a word with the whitespace that leads it, in its
/// role — or an operator, which carries the whitespace before it and an empty
/// word — and the columns the row it is painted on must keep free.
#[derive(Debug)]
struct Atom {
    lead: String,
    word: String,
    role: Role,
    /// The columns the operator that ends this atom's stage takes: the last
    /// word of a stage keeps them free, so the operator lands at the end of the
    /// row that word is on — a seam is never the first thing on a row, and a
    /// stage that wraps still ends where the shell ended it. Zero for every
    /// other atom: only the row the stage's text *ends* on pays for the
    /// operator, and a stage long enough to wrap spends its earlier rows on its
    /// own words.
    reserve: usize,
}

/// The ask's body cut into the atoms a row is packed from: words with their
/// leading whitespace, operators on their own, in their roles. An atom's lead is
/// every column of whitespace before it, wherever in the body it stood, so the
/// space a piece ends with leads the operator or the word that follows it.
///
/// One walk, two readers: the wrapping ([`Ask::rows`]) packs these, and the
/// whitespace a word leads with is the only thing a row start drops.
fn atoms(body: &[Piece]) -> Vec<Atom> {
    let mut atoms: Vec<Atom> = Vec::new();
    let mut pending = String::new();
    for (text, role) in body {
        if *role == Role::Seam {
            atoms.push(Atom {
                lead: std::mem::take(&mut pending),
                word: text.clone(),
                role: *role,
                reserve: 0,
            });
            continue;
        }
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let lead = rest.len() - rest.trim_start().len();
            pending.push_str(&rest[..lead]);
            rest = rest.trim_start();
            // Whitespace with nothing behind it in this piece *leads* the next
            // atom — the operator in the next piece, or the next word — rather
            // than standing as an atom of its own: a piece may end in the space
            // before the operator that ends its stage, and an atom that could
            // break a row there would take the operator's reserved columns with
            // it, leaving the operator to open the row it was kept off.
            if rest.is_empty() {
                break;
            }
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            atoms.push(Atom {
                lead: std::mem::take(&mut pending),
                word: rest[..end].to_string(),
                role: *role,
                reserve: 0,
            });
            rest = &rest[end..];
        }
    }
    // What is left in `pending` is the ask's own trailing whitespace: nothing
    // follows it to lead, and a row never paints a space the line does not end
    // on, so it is dropped rather than atomized.
    // The reserve, read from the right: the atom just before a seam keeps that
    // seam's own columns free (see the field's own doc). Every atom before it
    // does not: a stage's earlier rows are the stage's, and paying the
    // operator's columns on each of them would spend a row on a seam the row
    // does not carry.
    let mut reserve = 0;
    for atom in atoms.iter_mut().rev() {
        atom.reserve = reserve;
        reserve = if atom.role == Role::Seam {
            UnicodeWidthStr::width(atom.lead.as_str()) + UnicodeWidthStr::width(atom.word.as_str())
        } else {
            0
        };
    }
    atoms
}

/// The size marker a long call's row wears right after its ask: `(3KB)` where
/// the arguments as sent are [`ARGUMENT_MARKER`] bytes or more.
///
/// Measured from the call the painter already holds — a fact about the
/// arguments' size and not about what they asked, so the digest does not carry
/// it — and it is [`crate::app::size_label`]'s own spelling, the same one the
/// measure beside it uses.
fn argument_marker(call: &ToolCall) -> Option<String> {
    let bytes = call.function.arguments.len();
    (bytes >= ARGUMENT_MARKER).then(|| format!("({})", size_label(bytes)))
}

/// The ask's pieces cut to `budget` columns — the mark included, the way the
/// one-string ask was cut before the roles split it — with the size marker's
/// own columns **reserved first**.
///
/// The marker is the size *of* the ask, so the cut it explains must not eat it:
/// the ask's text gets what is left, and a marker wider than the whole budget is
/// itself cut rather than dropped. The pieces go left to right and the first
/// that does not fit ends the row with `…` ([`cut_pieces`]) — or, where the line
/// has stages, ends at the seam the cut owes the reader ([`cut_seams`]).
/// The body cut to one row, ending at a stage where the shell line has stages
/// to end at.
///
/// Without a seam this is exactly the cut the pane has always had
/// ([`cut_pieces`]): the pieces left to right, the first that does not fit cut
/// with `…`. With seams it paints the stages it holds whole, paints the next
/// stage as far as the row goes, and ends with the `…` that says the line went
/// on and the count of the stages behind it (` ; +2 stages`) — a count and not
/// an estimate, read from the shell's own split. A cut that would leave an
/// operator with nothing behind it drops that operator: the `…` takes its
/// place, because a row ending in `;` would say the line stopped there. And
/// where the count's own words cannot fit the row whole — or would leave no ask
/// at all to count ([`TAIL_FLOOR`]) — the plain cut is what is left, because a
/// count the pane cuts is not a count.
fn cut_seams(body: &[Piece], budget: usize) -> Vec<Piece> {
    let total = body.iter().filter(|(_, role)| *role == Role::Seam).count() + 1;
    if total == 1 || painted_pieces(body) <= budget {
        return cut_pieces(body, budget);
    }
    // The count and the cut are two readings of one row: the wider count eats
    // into the room the cut has, and the earlier the cut lands the more stages
    // it leaves behind it. So the count a cut reads can only narrow, round on
    // round — the loop opens with the widest count the line could have left and
    // settles in at most one round per stage, and the row returned is the
    // reading the count it wears was read from.
    let mut tail = stage_tail(total - 1);
    loop {
        let tail_w = UnicodeWidthStr::width(tail.as_str());
        if tail_w + TAIL_FLOOR > budget {
            return cut_pieces(body, budget);
        }
        let (mut row, whole, cut) = cut_stages(body, budget - tail_w);
        let next = stage_tail(total - whole - usize::from(cut));
        if next == tail {
            if !tail.is_empty() {
                row.push((tail, Role::Qualifier));
            }
            return row;
        }
        tail = next;
    }
}

/// The body painted from the left into `room` columns, and what the row it
/// paints shows of the shell's line: the stages it holds **whole** (their text
/// and their operator both), and whether the stage the cut lands in has text on
/// the row at all.
///
/// The row *is* cut — the caller only reaches here with a body the row cannot
/// hold — so its tail is kept for the [`ELLIPSIS`] and the space that sets it
/// off from the text it cuts: `grep …`, not `grep…`, because a word cut short
/// wears its `…` glued ([`truncate`]) while a row cut at a piece says where the
/// *line* went on. A cut that would leave an operator with nothing behind it
/// drops that operator instead: the `…` takes its place, and the stage whose
/// operator went is counted as cut into rather than whole, because a row ending
/// in `;` would say the line stopped there.
fn cut_stages(body: &[Piece], room: usize) -> (Vec<Piece>, usize, bool) {
    let room = room.saturating_sub(ELLIPSIS_W + 1);
    let mut row: Vec<Piece> = Vec::new();
    let mut used = 0;
    let mut whole: usize = 0;
    let mut in_stage = false;
    for (text, role) in body {
        let width = UnicodeWidthStr::width(text.as_str());
        if used + width > room {
            if matches!(row.last(), Some((_, Role::Seam))) {
                row.pop();
                whole = whole.saturating_sub(1usize);
            }
            in_stage = !row.is_empty();
            if in_stage && !ends_in_space(&row) {
                row.push((" ".to_string(), Role::Qualifier));
            }
            row.push((ELLIPSIS.to_string(), Role::Qualifier));
            return (row, whole, in_stage);
        }
        row.push((text.clone(), *role));
        used += width;
        if *role == Role::Seam {
            whole += 1;
            in_stage = false;
        } else {
            in_stage = true;
        }
    }
    (row, whole, in_stage)
}

/// The body cut to `budget` columns the way the one-row ask has always been
/// cut: the pieces left to right, the first that does not fit cut with `…` and
/// the rest dropped. [`cut_seams`]'s plain arm, and the whole of the cut for a
/// command with no separator ([`Role::Seam`]).
///
/// A row never ends in a bare operator: where the cut lands exactly on a seam,
/// the operator gives way to the `…`, because a row ending in `;` would say the
/// line stopped there. The operator's own columns pay for the mark.
fn cut_pieces(body: &[Piece], budget: usize) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    let mut used = 0;
    for (text, role) in body {
        if used >= budget {
            break;
        }
        let room = budget - used;
        let width = UnicodeWidthStr::width(text.as_str());
        if width <= room {
            used += width;
            out.push((text.clone(), *role));
            continue;
        }
        out.push((truncate(text, room), *role));
        break;
    }
    if matches!(out.last(), Some((_, Role::Seam))) {
        out.pop();
        let room = budget.saturating_sub(painted_pieces(&out));
        let set_off = !ends_in_space(&out);
        if room > ELLIPSIS_W + usize::from(set_off) {
            out.push((" …".to_string(), Role::Qualifier));
        } else if room > 0 {
            out.push((ELLIPSIS.to_string(), Role::Qualifier));
        }
    }
    out
}

/// Whether a row's pieces already end in whitespace: the `…` that ends a cut
/// row needs its own leading space only where the text does not bring one — a
/// stage's trailing space is a piece's tail, not a piece.
fn ends_in_space(row: &[Piece]) -> bool {
    row.last()
        .is_some_and(|(text, _)| text.ends_with(char::is_whitespace))
}

/// The compact row's own count of what its cut left: ` ; +2 stages`.
///
/// The separator is the one the missing stage would have worn, and one stage is
/// spelled without the `s` — the pane's own habit for a count of one. Zero
/// stages is nothing at all: a row with nothing behind its cut says so with its
/// own `…`.
fn stage_tail(left: usize) -> String {
    match left {
        0 => String::new(),
        1 => " ; +1 stage".to_string(),
        left => format!(" ; +{left} stages"),
    }
}

/// The columns a set of pieces paints.
fn painted_pieces(pieces: &[Piece]) -> usize {
    pieces
        .iter()
        .map(|(text, _)| UnicodeWidthStr::width(text.as_str()))
        .sum()
}

/// The ask's pieces as painted spans: the named parts in the ask's colour,
/// everything else dim — the seams and the size marker included.
fn ask_spans(pieces: Vec<Piece>) -> Vec<Span<'static>> {
    pieces
        .into_iter()
        .map(|(text, role)| match role {
            Role::Named => Span::styled(text, ask_style()),
            Role::Qualifier | Role::Seam | Role::Marker => Span::styled(text, dim()),
        })
        .collect()
}

/// The columns a set of spans paints.
fn painted_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// The dim rows the unfolded view paints under a call's header: the text the
/// call's own arguments carry, where they carry it — the **script** of a
/// heredoc ([`script_rows`]) and the replacement or content of the two file
/// writers ([`writer_rows`]) — and then the result's own fact rows — each at the
/// block's own **gutter** ([`crate::app::symbols`]'s pipe) and cut to what the
/// pane has left of it. The compact log paints none of them: its one row per
/// call is the header, and these are what the human reads when the call is open.
///
/// The gutter is the same one the result's payload wears ([`crate::app::chat`]
/// paints it through the same constant), so the header, its facts and the dump
/// under them read as one block — which is what the human's `Ctrl-Y` walk and
/// the pane's own columns both measure against. The ask's own text stands first
/// because it belongs to the ask: it is what the call *asked for*, and the facts
/// behind it are what came back.
pub(crate) fn details(
    call: &ToolCall,
    facts: &CallFacts,
    width: usize,
    mark: &str,
) -> Vec<Line<'static>> {
    let gutter = UnicodeWidthStr::width(mark);
    // The block's own gutter: the pipe every row under a call's header stands
    // at, padded to the width the header's mark takes — two columns for every
    // mark the table hands out ([`crate::app::symbols::Symbols::GUTTER_MARK`]),
    // and the mark's own columns for a caller that hands this module a wider
    // one (the mark-width test above). A pipe of a narrower gutter would let
    // the row out of the block it belongs to.
    let pad = match gutter.min(width) {
        0 => String::new(),
        take => format!("{}{}", PIPE, " ".repeat(take - 1)),
    };
    let budget = width.saturating_sub(gutter);
    let mut rows = script_rows(call, &pad, budget);
    rows.extend(writer_rows(call, &pad, budget));
    rows.extend(facts.details.iter().map(|fact| {
        Line::from(Span::styled(
            format!("{pad}{}", truncate(fact, budget)),
            dim(),
        ))
    }));
    rows
}

/// The script a call's own arguments carry, where it carries one: the body of
/// every heredoc its command's first line opens, under one row that says what
/// it is.
///
/// A heredoc's body is the one part of a command no ask could show: the ask is
/// the command's first line, `python3 - <<'PY'` is a whole row of it, and the
/// work — forty lines of script, a commit message — was in the arguments the
/// whole time. It is painted at the call's own gutter, dim, in the row on which
/// a *result* would stand, so the saying is explicit: this is the command's
/// **input** ([`SCRIPT`]), not the payload under it, which is its output.
///
/// The body is folded by the payload's own rule
/// ([`crate::app::chat::folded_head_tail`]): a forty-line script costs the rows
/// a payload costs, and the compact log — which paints no detail row at all —
/// keeps its one row per call whatever the script says.
fn script_rows(call: &ToolCall, pad: &str, budget: usize) -> Vec<Line<'static>> {
    if ToolName::parse(&call.function.name) != Some(ToolName::RunCommand) {
        return Vec::new();
    }
    let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.function.arguments) else {
        return Vec::new();
    };
    let Some(command) = args.get("command").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for script in scripts(command) {
        // An empty body is nothing to show: the `<<'EOF'` the ask already
        // names says all there is.
        if script.body.is_empty() {
            continue;
        }
        let count = crate::agent::lines_label(script.body.len());
        rows.push(Line::from(Span::styled(
            format!(
                "{pad}{}",
                truncate(&format!("script {count} {SCRIPT}"), budget)
            ),
            dim(),
        )));
        rows.extend(
            crate::app::chat::folded_head_tail(&script.body)
                .into_iter()
                .map(|line| {
                    Line::from(Span::styled(
                        format!("{pad}{}", truncate(&line, budget)),
                        dim(),
                    ))
                }),
        );
    }
    rows
}

/// The text the two file writers' own arguments carry, under a lead row that
/// says what it is — the unfolded view's answer to a call whose ask **is** text:
/// an `edit_file`'s replacement and a `write_file`'s content.
///
/// **An edit is shown as it was asked for, not as a unified diff.** The
/// arguments hold the pair the tool applies — `old_string` and `new_string` per
/// edit — and nothing else: no line numbers, no surrounding file, so no `@@`
/// header is invented and no context line is guessed at. The block paints the
/// replaced lines behind [`crate::app::symbols::Symbols::REMOVED`] and the new
/// ones behind [`crate::app::symbols::Symbols::ADDED`], edit by edit, in the
/// order the tool would apply them, under a row that counts what is there
/// (`diff 2 edits · +4−3`). Whether the replacement matched is the result's news
/// and is said at the arrow (`3 hunks`, or the one word `error` with its own row
/// below).
///
/// **A write is its content, with no marks.** There is no old text in a
/// `write_file`'s arguments, so a diff against the file would be a fabrication
/// and a `+` on every line would be a diff this painter never took: the block is
/// the content's own lines, folded, under a row that says what they are
/// (`write 41L · content, not output`) — the explicit saying a heredoc body
/// wears, for the same reason: the block stands exactly where a result's
/// payload stands.
///
/// **Both are painted whether the call landed or failed.** The block is the
/// *ask* and not the result: the arguments are what the model sent, and a
/// refused edit's exact strings are what its `! error: …` row is about. The
/// measure at the pane's edge is the other half of that rule and *is* gated —
/// nothing was written by a call that failed ([`crate::agent::Measure`]).
///
/// Both are folded by the payload's own rule
/// ([`crate::app::chat::folded_head_tail`]) with the lead row's counts read from
/// the whole strings — a five-hundred-line edit costs the eight rows a payload
/// costs and still says exactly how much it hides — and every row is cut to the
/// pane's columns ([`truncate`]). The compact log paints none of it: its one row
/// is the whole design ([`details`]).
///
/// A shape the model invented paints no block at all — `edits` that is not a
/// list, an entry that is not an object, a string that is missing — as
/// [`script_rows`] paints nothing for a command that is not one. Nothing here
/// panics on arguments the schema would have refused: the transcript keeps what
/// the model sent.
fn writer_rows(call: &ToolCall, pad: &str, budget: usize) -> Vec<Line<'static>> {
    let named = ToolName::parse(&call.function.name);
    if named == Some(ToolName::EditFile) {
        return edit_rows(call, pad, budget);
    }
    if named == Some(ToolName::WriteFile) {
        return write_rows(call, pad, budget);
    }
    // Every other ask is a summary of what the call wanted, not the text itself:
    // nothing is painted here.
    Vec::new()
}

/// An `edit_file`'s block: the lead row and then the replacement, edit by edit —
/// every edit's replaced lines and then its new ones, the order the strings
/// stand in and the order the tool applies them.
fn edit_rows(call: &ToolCall, pad: &str, budget: usize) -> Vec<Line<'static>> {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.function.arguments) else {
        return Vec::new();
    };
    let Some(edits) = args.get("edits").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = Vec::new();
    let mut removed = 0;
    let mut added = 0;
    for edit in edits {
        // An entry the schema would have refused — not an object, a string that
        // is missing or is not one — is no edit at all, and a block that painted
        // the half it understood would be a claim about a call that never ran:
        // the whole block says nothing instead, the way [`script_rows`] paints
        // nothing for a command that is not one. `get` on a value that is not an
        // object is `None`, which is the whole of the malformed-shape rule.
        let (Some(old), Some(new)) = (
            edit.get("old_string").and_then(serde_json::Value::as_str),
            edit.get("new_string").and_then(serde_json::Value::as_str),
        ) else {
            return Vec::new();
        };
        for line in old.lines() {
            removed += 1;
            lines.push(format!("{REMOVED} {line}"));
        }
        for line in new.lines() {
            added += 1;
            lines.push(format!("{ADDED} {line}"));
        }
    }
    // No string at all — an empty list, entries with no strings — is nothing to
    // show, the way an empty heredoc body is.
    if lines.is_empty() {
        return Vec::new();
    }
    let lead = format!(
        "diff {} · {ADDED}{added}{REMOVED}{removed}",
        edits_label(edits.len())
    );
    block_rows(pad, budget, &lead, &lines)
}

/// A `write_file`'s block: the lead row and the content, line for line.
fn write_rows(call: &ToolCall, pad: &str, budget: usize) -> Vec<Line<'static>> {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.function.arguments) else {
        return Vec::new();
    };
    let Some(content) = args.get("content").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    let lines: Vec<String> = content.lines().map(str::to_string).collect();
    // An empty content writes an empty file: the verdict's `0L` already says so
    // and there is no line to paint.
    if lines.is_empty() {
        return Vec::new();
    }
    let lead = format!(
        "write {} · content, not output",
        crate::agent::lines_label(lines.len())
    );
    block_rows(pad, budget, &lead, &lines)
}

/// One ask-derived block, assembled: the lead row that says what it is, then the
/// ask's own lines folded by the payload's own rule
/// ([`crate::app::chat::folded_head_tail`]) — and every row padded to the call's
/// gutter and cut to the pane ([`truncate`], which sanitizes as it cuts).
fn block_rows(pad: &str, budget: usize, lead: &str, lines: &[String]) -> Vec<Line<'static>> {
    std::iter::once(lead.to_string())
        .chain(crate::app::chat::folded_head_tail(lines))
        .map(|line| {
            Line::from(Span::styled(
                format!("{pad}{}", truncate(&line, budget)),
                dim(),
            ))
        })
        .collect()
}

/// `2 edits`: the count the diff block's lead row names, one edit spelled
/// without the `s` — the pane's own habit for a count of one ([`stage_tail`]).
fn edits_label(count: usize) -> String {
    if count == 1 {
        "1 edit".to_string()
    } else {
        format!("{count} edits")
    }
}

/// The style the ask (and the mark that leads it) is painted in: the tool-label
/// yellow both views have always used.
fn ask_style() -> Style {
    Style::default().fg(Color::Yellow)
}

/// The style an outcome's [`Tone`] is painted in: clean green, the warning
/// yellow, the alert red, and everything still in flight or unknown dim — the
/// pane's own colours, so the grid invents no palette.
fn tone_style(tone: Tone) -> Style {
    match tone {
        Tone::Ok => Style::default().fg(Color::Green),
        Tone::Warn => Style::default().fg(Color::Yellow),
        Tone::Alert => Style::default().fg(Color::Red),
        Tone::Running | Tone::None => dim(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::CallOutcome;
    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: mush_core::FunctionCall {
                name: name.into(),
                arguments: "{}".into(),
            },
        }
    }

    /// A call whose arguments are `bytes` long, for the size marker: the
    /// arguments' *size* is what earns it, not their shape.
    fn long_call(name: &str, bytes: usize) -> ToolCall {
        let mut call = call(name);
        call.function.arguments = format!("\"{}\"", "x".repeat(bytes.saturating_sub(2)));
        call
    }

    /// A mark is supplied by the caller ([`crate::app::symbols`]), and the grid
    /// measures it: a glyph two columns wide pushes the ask's own text one
    /// column further right, leaves the arrow where the width put it, and still
    /// ends inside the pane at every width.
    #[test]
    fn a_wider_glyph_shifts_the_ask_by_its_own_width() {
        let facts = facts("src/a.rs 5→7", Some("exit 3"), Some("13L 13B"));
        for width in 20..=200 {
            let narrow = text(&many(&call("read_file"), &facts, width, "▤ ")[0]);
            let wide = text(&many(&call("read_file"), &facts, width, "▤▤ ")[0]);
            // Columns, not bytes: the glyphs are three bytes each, so a byte
            // offset would compare the glyphs' own lengths instead of the ask's
            // column.
            let column = |row: &str| {
                row.find("src/a.rs")
                    .map(|at| UnicodeWidthStr::width(&row[..at]))
            };
            assert_eq!(
                column(&wide),
                column(&narrow).map(|at| at + 1),
                "width {width}: the ask stands one column later"
            );
            for row in [&narrow, &wide] {
                assert!(
                    UnicodeWidthStr::width(row.as_str()) <= width,
                    "width {width}: {row:?} is wider than the pane"
                );
            }
        }
        // The details follow the mark too, so a wider glyph moves the whole
        // block — header, details and payload — and not just the ask; the pipe
        // leads each of them, padded to the same width ([`details`]).
        let mut read = facts.clone();
        read.details = vec!["of 812L".into()];
        assert_eq!(
            text(&details(&call("read_file"), &read, 60, "▤ ")[0]),
            "│ of 812L"
        );
        assert_eq!(
            text(&details(&call("read_file"), &read, 60, "▤▤ ")[0]),
            "│  of 812L"
        );
    }

    /// The mark *is* the tool's name: a call of a tool the table knows leads
    /// with its mark and no repeated name (`▤ src/a.rs 5→7`), a call that takes
    /// no arguments is its bare mark (`◐`, `⧗`), and a name no tool answers to
    /// keeps the generic mark *and* its own name, because the glyph alone says
    /// nothing about a call mush has never heard of.
    #[test]
    fn a_known_tool_is_its_mark_and_an_invented_name_keeps_both() {
        let known = facts("src/a.rs 5→7", Some("exit 3"), Some("13L 13B"));
        assert_eq!(
            text(&many(&call("read_file"), &known, 60, "▤ ")[0]),
            split_row("▤ src/a.rs 5→7", "exit 3", "13L 13B", 60),
            "the mark is the name: `read_file` is not painted again"
        );
        let none = facts("", Some("2 agents · 1 job"), None);
        assert_eq!(
            text(&many(&call("status"), &none, 60, "◐ ")[0]),
            one_clause_row("◐", "2 agents · 1 job", 60),
            "a tool that takes no arguments is its bare mark"
        );
        let invented = facts("x", Some("error"), None);
        assert_eq!(
            text(&many(&call("frobnicate"), &invented, 60, "⚙ ")[0]),
            one_clause_row("⚙ frobnicate x", "error", 60),
            "an invented name keeps the generic mark and its own name"
        );
    }

    /// One row as the split grid lays it out: the ask, the gap to the arrow's
    /// own column, the arrow, the verdict, and the measure right-aligned at the
    /// pane's edge — the shape
    /// [`a_known_tool_is_its_mark_and_an_invented_name_keeps_both`] reads
    /// instead of spelling the spaces by hand.
    fn split_row(ask: &str, verdict: &str, measure: &str, width: usize) -> String {
        let grid = Grid::of(width);
        let mut row = ask.to_string();
        row.push_str(&" ".repeat(grid.arrow_x().saturating_sub(UnicodeWidthStr::width(ask))));
        row.push_str("→ ");
        row.push_str(verdict);
        let used = grid.arrow_x() + GAP + UnicodeWidthStr::width(verdict);
        row.push_str(&" ".repeat(width.saturating_sub(used + UnicodeWidthStr::width(measure))));
        row.push_str(measure);
        row
    }

    fn facts(ask: &str, outcome: Option<&str>, measure: Option<&str>) -> CallFacts {
        let measure = measure.map(|text| {
            let (count, size) = match text.split_once(' ') {
                Some((count, size)) => (Some(count.to_string()), Some(size.to_string())),
                None => (Some(text.to_string()), None),
            };
            Measure { count, size }
        });
        CallFacts {
            ask: ask.into(),
            cwd: None,
            outcome: outcome.map(|text| CallOutcome {
                text: text.into(),
                tone: Tone::Ok,
            }),
            measure,
            details: Vec::new(),
        }
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The compact log's reading of a call: exactly one row, whatever the ask.
    /// The two views are named where they are read, because most tests below
    /// pin one of them and the difference is the whole of the ask's own work.
    fn one(call: &ToolCall, facts: &CallFacts, width: usize, mark: &str) -> Vec<Line<'static>> {
        header(call, facts, width, mark, AskRows::One)
    }

    /// The unfolded view's reading: as many rows as the ask needs.
    fn many(call: &ToolCall, facts: &CallFacts, width: usize, mark: &str) -> Vec<Line<'static>> {
        header(call, facts, width, mark, AskRows::Many)
    }

    /// The rows as the pane paints them, one string per row.
    fn painted(rows: &[Line<'_>]) -> Vec<String> {
        rows.iter().map(text).collect()
    }

    /// A `run_command` call carrying `command`, where both the digest and the
    /// painter read it: the ask's own text, and the heredoc a script block
    /// comes from.
    fn command_call(command: &str) -> ToolCall {
        let mut call = call("run_command");
        call.function.arguments = serde_json::json!({ "command": command }).to_string();
        call
    }

    /// Any call carrying `arguments` as its own JSON: for the script reader's
    /// other tools and shapes.
    fn call_for(name: &str, arguments: &str) -> ToolCall {
        let mut call = call(name);
        call.function.arguments = arguments.to_string();
        call
    }

    /// The same facts with the cwd chip's own directory ([`CallFacts::cwd`]):
    /// the one field the ask's head reads from outside the ask's own text.
    fn at_cwd(facts: CallFacts, cwd: &str) -> CallFacts {
        CallFacts {
            cwd: Some(cwd.to_string()),
            ..facts
        }
    }

    /// The gap and the arrow are each two columns: the arithmetic every row's
    /// padding rests on.
    #[test]
    fn the_gap_and_the_arrow_are_two_columns() {
        assert_eq!(UnicodeWidthStr::width("→ "), GAP);
    }

    /// The grid's arithmetic at the three widths the human named, and at a
    /// floor and a ceiling: the outcome column is a third of the pane between
    /// twelve and twenty-eight columns, and the arrow sits at `width -
    /// outcome_w` in every one of them. The rung is the box's own 18 columns:
    /// 60 columns of pane is where `(60 / 3) - 2` first holds it.
    #[test]
    fn the_grid_is_a_third_of_the_pane_between_its_bounds() {
        let grid = Grid::of(90);
        assert_eq!((grid.outcome_w, grid.arrow_x, grid.ask_w), (28, 62, 60));
        assert!(!grid.stacked());
        assert!(grid.splits());
        let grid = Grid::of(133);
        assert_eq!((grid.outcome_w, grid.arrow_x, grid.ask_w), (28, 105, 103));
        assert!(!grid.stacked());
        let grid = Grid::of(45);
        assert_eq!((grid.outcome_w, grid.arrow_x, grid.ask_w), (15, 30, 28));
        assert!(!grid.splits(), "a 13-column box is under the rung");
        let grid = Grid::of(36);
        assert_eq!((grid.outcome_w, grid.arrow_x, grid.ask_w), (12, 24, 22));
        let grid = Grid::of(30);
        assert_eq!((grid.outcome_w, grid.arrow_x, grid.ask_w), (12, 18, 16));
        assert!(!grid.stacked(), "16 columns of ask is above the floor");
        let grid = Grid::of(27);
        assert_eq!(grid.ask_w, 13);
        assert!(grid.stacked(), "13 columns of ask is below the floor");
        assert!(!grid.splits(), "a stacked pane is always under the rung");
        let grid = Grid::of(200);
        assert_eq!((grid.outcome_w, grid.arrow_x, grid.ask_w), (28, 172, 170));
        // The rung from both sides: 59 columns of pane give a 17-column box and
        // 60 gives exactly 18.
        assert_eq!(Grid::of(59).outcome_columns(), 17);
        assert!(!Grid::of(59).splits());
        assert_eq!(Grid::of(60).outcome_columns(), SPLIT_FLOOR);
        assert!(Grid::of(60).splits());
    }

    /// The floor is a property of the width and not of the text: a short ask at
    /// a narrow pane still moves its outcome to its own row, so the **compact
    /// log's** every call at that width wears the same shape — one row per
    /// call, whatever it says. (The unfolded view is the other half of that
    /// rule: it gives the ask the rows it needs, so its block is as tall as the
    /// ask.)
    #[test]
    fn the_two_row_block_is_the_floor_and_not_the_text() {
        let short = facts("wait", Some("#188 done"), None);
        let long = facts(&"x".repeat(500), Some("#188 done"), None);
        for width in 20..=200 {
            let rows = one(&call("wait"), &short, width, "⧗ ").len();
            assert_eq!(
                rows,
                one(&call("wait"), &long, width, "⧗ ").len(),
                "one shape per width, whatever the ask at {width}"
            );
            assert_eq!(rows, if width < 28 { 2 } else { 1 }, "width {width}");
        }
    }

    /// Every row of a header is exactly its own columns or fewer, and the arrow
    /// stands at the grid's column wherever an outcome is painted — in the
    /// compact log's one row, in the two-row block a stacked pane gives, and on
    /// the last row of an ask the unfolded view wrapped. Below two columns the
    /// pane cannot hold `→ ` at all, and the row clips to what it has.
    #[test]
    fn every_row_fits_and_every_arrow_stands_at_the_same_column() {
        for width in 0..=220 {
            let grid = Grid::of(width);
            let facts = facts(
                &"a call with a long ask ".repeat(20),
                Some("exit 3"),
                Some("41L 1.2KB"),
            );
            for rows in [
                one(&call("run_command"), &facts, width, "❯ "),
                many(&call("run_command"), &facts, width, "❯ "),
            ] {
                for row in &rows {
                    let row = text(row);
                    assert!(
                        UnicodeWidthStr::width(row.as_str()) <= width,
                        "width {width}: {row:?} is wider than the pane"
                    );
                }
                if width < UnicodeWidthStr::width("→ ") {
                    continue;
                }
                let painted = painted(&rows);
                let at = painted
                    .iter()
                    .position(|row| row.contains("→ "))
                    .unwrap_or_else(|| panic!("width {width}: an outcome is never dropped"));
                let column = UnicodeWidthStr::width(&painted[at][..painted[at].find('→').unwrap()]);
                assert_eq!(column, grid.arrow_x(), "width {width}: the arrow column");
                // The clauses sit on the ask's **last** row: the compact log's
                // own row, or the row after the ask where the pane stacks the
                // outcome — and an ask that grew rows still wears its arrow on
                // the row it ends in.
                assert_eq!(
                    at,
                    painted.len() - 1,
                    "width {width}: the arrow is on the block's last row: {painted:#?}"
                );
            }
        }
    }

    /// The split's own shape, at the widths where it is on: the verdict stands
    /// left-aligned at the arrow, the measure ends at the pane's **own right
    /// edge**, and the row is exactly the pane's width — two columns of pane,
    /// one call, no slack.
    #[test]
    fn the_verdict_sits_at_the_arrow_and_the_measure_at_the_panes_edge() {
        let facts = facts("cargo test -p mush", Some("exit 3"), Some("41L 1.2KB"));
        for width in 60..=200 {
            let row = text(&many(&call("run_command"), &facts, width, "❯ ")[0]);
            let grid = Grid::of(width);
            let at = row.find("exit 3").expect("the verdict is painted");
            assert_eq!(
                UnicodeWidthStr::width(&row[..at]),
                grid.arrow_x() + GAP,
                "width {width}: the verdict stands behind the arrow"
            );
            assert!(
                row.ends_with("41L 1.2KB"),
                "width {width}: the measure ends the row: {row:?}"
            );
            assert_eq!(
                UnicodeWidthStr::width(row.as_str()),
                width,
                "width {width}: the measure is right-aligned at the pane's edge"
            );
        }
    }

    /// A row with nothing to measure leaves the measure column empty — the
    /// sentence-tools (`status`, `control`, `spawn_agent`, `wait`) have a
    /// verdict and no measure — and a row that measures something but has no
    /// news keeps the arrow and leaves the verdict's own columns empty: the
    /// arrow is the column's mark, and the same row one rung down the pane
    /// paints as `→ 41L 1.2KB` ([`one_clause`]).
    #[test]
    fn the_measure_column_is_empty_for_the_sentence_tools() {
        let sentence = facts("", Some("2 agents · 1 job"), None);
        let row = text(&many(&call("status"), &sentence, 88, "◐ ")[0]);
        assert_eq!(row, one_clause_row("◐", "2 agents · 1 job", 88));
        assert!(
            !row.ends_with(' '),
            "nothing is painted at the right edge: {row:?}"
        );
        // A clean command whose only clause is its payload: the measure stands
        // at the edge, the verdict's columns stand empty, and the arrow — the
        // column's own — is painted either way, so the row keeps the shape its
        // one-clause twin has.
        let measured = facts("cargo test -p mush", None, Some("41L 1.2KB"));
        let row = text(&many(&call("run_command"), &measured, 88, "❯ ")[0]);
        assert_eq!(row, split_row("❯ cargo test -p mush", "", "41L 1.2KB", 88));
        let at = row.find('→').expect("the arrow is the column's own mark");
        assert_eq!(
            UnicodeWidthStr::width(&row[..at]),
            Grid::of(88).arrow_x(),
            "and it stands at the grid's column: {row:?}"
        );
        assert!(row.ends_with("41L 1.2KB"), "the measure ends the row");
    }

    /// The rung from the reader's side: under it every call paints one clause
    /// (`→ exit 3`), and above it a row whose verdict and measure both fit
    /// splits while a row whose pair does not fit **falls back whole** — the
    /// measure is dropped, never the verdict crushed to one column.
    #[test]
    fn the_rung_falls_back_to_one_clause_and_never_crushes_the_verdict() {
        let exit = facts("cargo test", Some("exit 3"), Some("41L 1.2KB"));
        let row = text(&many(&call("run_command"), &exit, 59, "❯ ")[0]);
        assert_eq!(row, one_clause_row("❯ cargo test", "exit 3", 59));
        assert!(!row.contains("41L"), "under the rung the measure is gone");
        let row = text(&many(&call("run_command"), &exit, 60, "❯ ")[0]);
        assert!(row.ends_with("41L 1.2KB"), "at the rung it splits: {row:?}");
        // The same pane, a pair that does not fit: the verdict keeps the whole
        // box and the measure waits for a wider pane.
        let long = facts(
            "cargo test",
            Some("no children and no jobs"),
            Some("12345L/67890L 1.2MB"),
        );
        let row = text(&many(&call("run_command"), &long, 88, "❯ ")[0]);
        assert_eq!(
            row,
            one_clause_row("❯ cargo test", "no children and no jobs", 88)
        );
        assert!(!row.contains("12345L"));
    }

    /// One clause as the narrow grammar paints it: the ask, the arrow at its
    /// column, and the clause cut to the box.
    fn one_clause_row(ask: &str, clause: &str, width: usize) -> String {
        let grid = Grid::of(width);
        let gap = grid.arrow_x().saturating_sub(UnicodeWidthStr::width(ask));
        format!(
            "{ask}{}→ {}",
            " ".repeat(gap),
            truncate(clause, grid.outcome_columns())
        )
    }

    /// The details are one row per fact, at the gutter, cut to the pane; and a
    /// call with no details paints no row at all.
    #[test]
    fn the_details_sit_at_the_gutter_and_are_cut_to_the_pane() {
        let mut read = facts("read_file", Some("exit 3"), None);
        read.details = vec!["of 812L".into(), "x".repeat(200)];
        let rows = details(&call("read_file"), &read, 60, "▤ ");
        assert_eq!(rows.len(), 2);
        assert_eq!(text(&rows[0]), "│ of 812L");
        assert_eq!(UnicodeWidthStr::width(text(&rows[1]).as_str()), 60);
        assert!(text(&rows[1]).starts_with("│ "));
        assert!(details(&command_call("ls"), &facts("x", None, None), 60, "▤ ").is_empty());
    }

    /// An ask is cut to its column from the right, the measure to its own room,
    /// and the whole row still lands on the arrow's column: the two cuts are
    /// one layout. The ask here has no separator at all, which is the cut the
    /// compact log has always made ([`cut_pieces`]).
    #[test]
    fn a_cut_ask_and_a_cut_measure_keep_the_arrow_on_its_column() {
        let rows = one(
            &call("run_command"),
            &facts(&"a long ask ".repeat(30), Some("exit 3"), Some("41L 1.2KB")),
            90,
            "❯ ",
        );
        assert_eq!(rows.len(), 1);
        let row = text(&rows[0]);
        assert_eq!(UnicodeWidthStr::width(row.as_str()), 90);
        let columns = UnicodeWidthStr::width(&row[..row.find('→').unwrap()]);
        assert_eq!(columns, 62, "the arrow's own column");
        assert!(row.contains('…'), "the ask says it was cut: {row:?}");
        assert!(
            row.ends_with("41L 1.2KB"),
            "the measure is not what the ask's cut eats: {row:?}"
        );
    }

    /// A pane narrower than the mark itself: the rows degenerate to what fits
    /// rather than overflow, and the outcome is still painted somewhere.
    #[test]
    fn a_pane_narrower_than_the_mark_clips_every_row() {
        for width in 0..crate::app::symbols::Symbols::GUTTER {
            let rows = many(&call("wait"), &facts("x", Some("done"), None), width, "⧗ ");
            for row in &rows {
                let row = text(row);
                assert!(
                    UnicodeWidthStr::width(row.as_str()) <= width,
                    "width {width}: {row:?}"
                );
            }
        }
        assert_eq!(
            text(&many(&call("wait"), &facts("", None, None), 0, "⧗ ")[0]),
            ""
        );
    }

    /// The size marker: a call whose arguments are 300 bytes or more wears
    /// `(3KB)` right after the ask, dim; a shorter one wears nothing. The
    /// threshold is the human's own number and the boundary is pinned on both
    /// sides.
    #[test]
    fn a_long_argument_gets_a_size_marker() {
        let facts = facts("python3 - <<'PY'", None, None);
        let short = long_call("run_command", ARGUMENT_MARKER - 1);
        let row = text(&many(&short, &facts, 88, "❯ ")[0]);
        assert_eq!(row, "❯ python3 - <<'PY'", "under the threshold: nothing");
        let long = long_call("run_command", ARGUMENT_MARKER);
        let row = text(&many(&long, &facts, 88, "❯ ")[0]);
        assert_eq!(
            row, "❯ python3 - <<'PY' (300B)",
            "at the threshold: the size"
        );
        // Dim, not bright: the marker is a fact about the arguments and not one
        // of them. The last span is the marker's.
        let spans = &many(&long, &facts, 88, "❯ ")[0].spans;
        assert_eq!(spans.last().unwrap().content.as_ref(), " (300B)");
        assert_eq!(spans.last().unwrap().style, dim());
    }

    /// A long command's own size is reserved before the ask is cut: the very
    /// cut the marker explains can never eat it — the marker is painted whole
    /// whenever the ask's column can hold it, and what goes is the ask's text.
    /// (The compact log's cut is the one that reserves: the unfolded view has
    /// the rows to paint the marker after the ask instead.)
    #[test]
    fn the_cut_never_eats_the_marker() {
        let long_ask = facts(&"x".repeat(400), None, None);
        let call = long_call("run_command", 4_000);
        for width in 20..=120 {
            let row = text(&one(&call, &long_ask, width, "❯ ")[0]);
            let budget = Grid::of(width).ask_columns();
            assert!(
                UnicodeWidthStr::width(row.as_str()) <= width,
                "width {width}: {row:?} is wider than the pane"
            );
            if budget >= UnicodeWidthStr::width(" (4KB)") + UnicodeWidthStr::width("❯ ") {
                assert!(
                    row.ends_with("(4KB)"),
                    "width {width}: the marker survives the cut: {row:?}"
                );
            }
        }
        // The ask's own text is what pays for the marker's columns.
        let row = text(&one(&call, &facts("a long command", None, None), 20, "❯ ")[0]);
        assert!(row.ends_with("(4KB)"), "{row:?}");
        assert!(row.contains('…'), "the ask is the part cut: {row:?}");
    }

    /// The unfolded view's own work: a `;`/`&&` stage starts a row, the operator
    /// that ends a stage is the last thing on its own row, and the stage behind
    /// it begins the next — so a wrapped chain reads as the stages the shell ran
    /// and no seam is invented where the line had none.
    #[test]
    fn a_chain_wraps_a_stage_to_a_row_and_ends_it_with_its_operator() {
        let command = "wc -l tools.rs && grep -n foo tools.rs";
        let call = command_call(command);
        let facts = facts(command, None, None);
        // 88 columns hold the whole line: the unfolded view does not break an
        // ask it can paint on one row.
        assert_eq!(
            painted(&many(&call, &facts, 88, "❯ ")),
            vec!["❯ wc -l tools.rs && grep -n foo tools.rs"],
            "an ask the pane holds is one row, whatever view paints it"
        );
        // 40 columns do not: the first stage and its operator take the row, and
        // the second stage starts under the mark.
        assert_eq!(
            painted(&many(&call, &facts, 40, "❯ ")),
            vec!["❯ wc -l tools.rs &&", "  grep -n foo tools.rs"],
        );
    }

    /// A pipeline is one thing: a `|` is not a seam, so the pipeline a pane
    /// holds keeps its row whole — and where it does not, it wraps at its own
    /// words like any long ask, never at the `|` as though it were a stage.
    #[test]
    fn a_pipeline_keeps_its_stage_on_the_row() {
        let command = "seq 1 20 | tail -3";
        assert_eq!(
            painted(&many(
                &command_call(command),
                &facts(command, None, None),
                40,
                "❯ "
            )),
            vec!["❯ seq 1 20 | tail -3"],
            "a pipeline the pane holds is one row"
        );
        // A chain behind the pipeline still starts its own row, and the seam
        // still ends the row its stage is in.
        let command = "seq 1 200 | awk '{n += $1} END {print n}' && echo done";
        let rows = painted(&many(
            &command_call(command),
            &facts(command, None, None),
            40,
            "❯ ",
        ));
        assert_eq!(rows.len(), 3, "{rows:#?}");
        assert!(
            rows[0].contains('|'),
            "the pipeline keeps its own row: {rows:#?}"
        );
        assert!(rows[1].ends_with("&&"), "{rows:#?}");
        assert!(rows[2].starts_with("  echo"), "{rows:#?}");
    }

    /// The degenerate chain: a first stage already wider than the pane. The
    /// unfolded view never cuts the ask — the stage's own words wrap, and a word
    /// wider than a whole row is split — and its seam still lands at the end of
    /// the row the stage ends in.
    #[test]
    fn a_stage_wider_than_the_pane_wraps_and_is_never_cut() {
        let word = "x".repeat(50);
        let command = format!("printf {word} && echo done");
        let rows = painted(&many(
            &command_call(&command),
            &facts(&command, None, None),
            40,
            "❯ ",
        ));
        assert!(
            !rows.iter().any(|row| row.contains('…')),
            "nothing is cut: {rows:#?}"
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.matches('x').count())
                .sum::<usize>(),
            50,
            "every byte of the word is painted: {rows:#?}"
        );
        let seam = rows
            .iter()
            .position(|row| row.ends_with("&&"))
            .unwrap_or_else(|| panic!("the seam ends its own row: {rows:#?}"));
        assert!(rows[seam + 1].starts_with("  echo done"), "{rows:#?}");
    }

    /// The compact row's cut: the stages the row holds whole, the next stage as
    /// far as it goes, the `…` that says the line went on, and the count of the
    /// stages behind the cut — the shell's own split, never an estimate. A
    /// command with no separator keeps the plain cut, and a row whose count
    /// would leave no ask at all keeps the plain cut too.
    #[test]
    fn the_compact_cut_ends_at_a_seam_and_counts_what_it_left() {
        let command = "wc -l tools.rs ; grep -n foo tools.rs ; head -1 ; tail -2";
        assert_eq!(
            painted(&one(
                &command_call(command),
                &facts(command, None, None),
                60,
                "❯ "
            )),
            vec!["❯ wc -l tools.rs ; grep … ; +2 stages"],
            "the cut ends at a stage and says what it left"
        );
        // One stage behind the cut is spelled without the `s`, like every other
        // count of one.
        let command = "wc -l tools.rs ; grep -n foo tools.rs ; head -1";
        let row = text(
            &one(
                &command_call(command),
                &facts(command, None, None),
                60,
                "❯ ",
            )[0],
        );
        assert!(row.contains(" ; +1 stage"), "{row:?}");
        assert!(!row.contains("stages"), "one stage is not stages: {row:?}");
        // No separator at all: the plain cut the pane has always made.
        let command = "cargo test --all-targets -- --nocapture --test-threads=1";
        let row = text(
            &one(
                &command_call(command),
                &facts(command, None, None),
                60,
                "❯ ",
            )[0],
        );
        assert!(
            row.ends_with('…'),
            "the plain cut ends with its `…`: {row:?}"
        );
        assert!(!row.contains(" ; +"), "nothing to count: {row:?}");
        // And a count that would stand alone — the chip's columns, a gap, and
        // the count with no ask behind it — is no count at all: the row keeps
        // the command instead.
        let command = "cd .mush/wt/198 && wc -l tools.rs ; grep -n foo tools.rs ; tail -1";
        let row = text(
            &one(
                &command_call(command),
                &at_cwd(
                    facts(
                        "wc -l tools.rs ; grep -n foo tools.rs ; tail -1",
                        None,
                        None,
                    ),
                    ".mush/wt/198",
                ),
                45,
                "❯ ",
            )[0],
        );
        assert!(row.starts_with("[.mush/wt/198] ❯ wc"), "{row:?}");
        assert!(!row.contains(" ; +"), "no count without a line: {row:?}");
    }

    /// The seam cut's own edges, at budgets a pane reaches at its narrowest: the
    /// operator a cut leaves with nothing behind it gives way to the `…` — in
    /// the seam arm and in the plain one — and the row the count is read from is
    /// the row the count pays for, so the cut and the count agree.
    #[test]
    fn a_cut_never_leaves_an_operator_with_nothing_behind_it() {
        let body = |command: &str| {
            ask_pieces(&command_call(command), &facts(command, None, None), "❯ ").body
        };
        let command = "wc -l tools.rs ; grep -n foo tools.rs ; tail -2";
        assert_eq!(
            flat(cut_seams(&body(command), 16)),
            "wc -l tools.rs …",
            "the count's own words cannot fit: the plain cut, and the operator the \
             cut landed on gives way to the `…`"
        );
        assert_eq!(
            flat(cut_seams(
                &body("wc -l tools.rs ; grep -n foo tools.rs"),
                29
            )),
            "wc -l tools.rs … ; +1 stage",
            "the seam the cut landed on is the `…`'s, and the count is the stage \
             behind it"
        );
        assert_eq!(
            flat(cut_seams(
                &body("wc -l tools.rs ; grep -n foo tools.rs"),
                30
            )),
            "wc -l tools.rs ; grep …",
            "one column more and the stage behind the cut stands on the row: its \
             own `…` says the line went on, and there is nothing left to count"
        );
        // Whatever the budget, the cut fits it and says what it left: the two
        // readings are one row's.
        for budget in 1..60 {
            let row = cut_seams(&body(command), budget);
            assert!(
                painted_pieces(&row) <= budget,
                "{budget}: {:?} does not fit",
                flat(row)
            );
        }
    }

    /// The same call's ask as one flat string: the pieces without the rows, for
    /// the cut's own arithmetic, which is about columns and not about lines.
    fn flat(pieces: Vec<Piece>) -> String {
        text(&Line::from(ask_spans(pieces)))
    }

    /// The wrap's invariants, swept over every budget a chain can meet: no row
    /// is ever a bare operator — a stage's last word keeps the operator's own
    /// columns free, so the operator ends the row that word is on — no row is
    /// wider than its columns, and nothing is cut (no `…` anywhere: the unfolded
    /// view drops no byte of the ask, and the only thing a break takes is the
    /// whitespace a row would otherwise open with).
    ///
    /// This is the sweep the frames earned: at 60 columns a stage's trailing
    /// space stood as an atom of its own, broke the row before the operator and
    /// took the columns the operator had been reserved, so a wrapped chain read
    /// `cp note.txt .mush/wt/198/note.txt` / `&&` / `echo …`.
    #[test]
    fn a_wrapped_chain_never_opens_a_row_with_an_operator() {
        for command in [
            "mkdir -p .mush/wt/198 && printf 'note\\n' > note.txt && cp note.txt .mush/wt/198/note.txt && echo wrote the note",
            "wc -l note.txt && wc -c note.txt && wc -w note.txt && wc -m note.txt && echo counted-every-way",
            "seq 1 200 | awk '{n += $1} END {print n}' && echo done",
        ] {
            let facts = facts(command, None, None);
            let ask: String = command.chars().filter(|ch| !ch.is_whitespace()).collect();
            // Six is the mark and the wrap's own floor: below it the compact cut
            // is what the pane paints, and a cut is meant to drop bytes.
            for budget in 6..120 {
                let rows = painted(&many(&command_call(command), &facts, budget, "❯ "));
                for row in &rows {
                    assert!(
                        UnicodeWidthStr::width(row.as_str()) <= budget,
                        "{budget}: {row:?} is wider than its row"
                    );
                    let text = row.trim_start();
                    assert!(
                        !["&&", ";", "||"]
                            .iter()
                            .any(|op| text.starts_with(op)),
                        "{budget}: a row opens with an operator: {rows:#?}"
                    );
                }
                assert!(
                    !rows.iter().any(|row| row.contains('…')),
                    "{budget}: the unfolded view cut the ask: {rows:#?}"
                );
                let painted: String = rows
                    .iter()
                    .flat_map(|row| row.chars())
                    .filter(|ch| !ch.is_whitespace())
                    .collect();
                assert_eq!(
                    painted.strip_prefix('❯'),
                    Some(ask.as_str()),
                    "{budget}: the wrap does not paint the ask, byte for byte: {rows:#?}"
                );
            }
        }
    }

    /// The cwd chip: a command whose own leading `cd` named a directory the
    /// workspace root is not wears it at the row's head, dim — and a command in
    /// the root wears nothing, because the root is where every command already
    /// runs. The ask is the command: the chip never eats a byte of it.
    #[test]
    fn the_cwd_chip_leads_the_row_only_outside_the_root() {
        let facts = facts("cargo test -p mush", None, None);
        let call = command_call("cd .mush/wt/198 && cargo test -p mush");
        let chipped = many(&call, &at_cwd(facts.clone(), ".mush/wt/198"), 88, "❯ ");
        assert_eq!(
            painted(&chipped),
            vec!["[.mush/wt/198] ❯ cargo test -p mush"],
            "the chip is the ask's head, outside the root"
        );
        assert_eq!(chipped[0].spans[0].content.as_ref(), "[.mush/wt/198] ");
        assert_eq!(chipped[0].spans[0].style, dim(), "the chip is a qualifier");
        assert_eq!(
            painted(&many(&call, &facts, 88, "❯ ")),
            vec!["❯ cargo test -p mush"],
            "in the root the chip is dropped: the ask is the whole row"
        );
        // A pane too narrow for the chip, the mark and a floor of ask drops the
        // chip rather than the mark, and the row reads as the command it is.
        let row = text(&many(&call, &at_cwd(facts.clone(), ".mush/wt/198"), 30, "❯ ")[0]);
        assert!(row.starts_with("❯ cargo test"), "{row:?}");
        assert!(!row.contains('['), "the chip is what goes: {row:?}");
    }

    /// The script a heredoc carries: the unfolded view paints the body under the
    /// ask, at the call's gutter, under one row that says what it is — the
    /// command's *input*, and not the payload under it, which is its output. The
    /// body is folded by the payload's own rule, so a twelve-line script costs
    /// the eight rows a payload costs; the compact log paints none of it.
    #[test]
    fn a_heredoc_body_is_painted_as_the_calls_own_input() {
        let command = std::iter::once("python3 - <<'PY'".to_string())
            .chain((1..=12).map(|n| format!("print({n})")))
            .chain(std::iter::once("PY".to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        let call = command_call(&command);
        let ask = facts("python3 - <<'PY'", None, None);
        let rows = painted(&details(&call, &ask, 60, "❯ "));
        assert_eq!(rows[0], "│ script 12L · input, not output");
        assert_eq!(rows.len(), 1 + 8, "the payload's own eight rows: {rows:#?}");
        assert_eq!(rows[1], "│ print(1)");
        assert_eq!(rows[4], "│ … 5 lines …", "{rows:#?}");
        assert_eq!(rows[8], "│ print(12)", "{rows:#?}");
        // A short body costs its own rows and no elision.
        let rows = painted(&details(
            &command_call("cat <<'EOF'\nbody\nEOF"),
            &facts("cat <<'EOF'", None, None),
            60,
            "❯ ",
        ));
        assert_eq!(rows, vec!["│ script 1L · input, not output", "│ body"]);
        // The compact log paints the ask and not one row of the body: its one
        // row per call is the whole design.
        assert_eq!(
            painted(&one(&call, &ask, 60, "❯ ")),
            vec!["❯ python3 - <<'PY'"]
        );
        // A quoted `<<` is a string and opens nothing, and a tool that is not a
        // command carries no script at all.
        assert!(painted(&details(
            &command_call("echo \"cat <<'EOF'\""),
            &facts("echo \"cat <<'EOF'\"", None, None),
            60,
            "❯ ",
        ))
        .is_empty());
        assert!(painted(&details(
            &call_for("read_file", r#"{"command":"cat <<'EOF'\nbody\nEOF"}"#),
            &facts("src/a.rs", None, None),
            60,
            "▤ ",
        ))
        .is_empty());
    }

    /// A call whose arguments are not the JSON an ask comes from (a refusal, an
    /// invented tool name, an empty object) paints no script rather than
    /// guessing at one.
    #[test]
    fn a_call_with_no_command_paints_no_script() {
        assert!(painted(&details(
            &call("run_command"),
            &facts("ls", None, None),
            60,
            "❯ "
        ))
        .is_empty());
        assert!(painted(&details(
            &call("frobnicate"),
            &facts("x", None, None),
            60,
            "⚙ "
        ))
        .is_empty());
    }

    /// An `edit_file`'s block is the replacement the model asked for: a lead row
    /// counting the edits and the lines, then each edit's replaced lines and its
    /// new ones — two edits, two blocks, in the order the tool would apply them
    /// — and nothing else under the call.
    #[test]
    fn an_edit_paints_each_edit_as_the_replacement_it_asked_for() {
        let call = call_for(
            "edit_file",
            r#"{"path":"src/lex.rs","edits":[{"old_string":"let x = 1;","new_string":"let x = 2;"},{"old_string":"fn a() {}\nfn b() {}","new_string":"fn ab() {}"}]}"#,
        );
        let rows = painted(&details(
            &call,
            &facts("src/lex.rs", Some("2 hunks"), Some("49B")),
            60,
            "± ",
        ));
        assert_eq!(
            rows,
            vec![
                "│ diff 2 edits · +2−3",
                "│ − let x = 1;",
                "│ + let x = 2;",
                "│ − fn a() {}",
                "│ − fn b() {}",
                "│ + fn ab() {}",
            ]
        );
    }

    /// A single-line edit is one `−` and one `+`, under a lead row that counts
    /// one edit without the `s` — the pane's own habit for a count of one.
    #[test]
    fn a_single_line_edit_paints_one_removed_and_one_added() {
        let call = call_for(
            "edit_file",
            r#"{"path":"src/lex.rs","edits":[{"old_string":"one","new_string":"two"}]}"#,
        );
        assert_eq!(
            painted(&details(
                &call,
                &facts("src/lex.rs", Some("1 hunk"), Some("6B")),
                60,
                "± "
            )),
            vec!["│ diff 1 edit · +1−1", "│ − one", "│ + two"]
        );
    }

    /// A five-hundred-line edit is folded by the payload's own rule: the first
    /// three lines, the `… N lines …` elision and the last four — so it costs
    /// the rows a payload costs — while the lead row's counts are read from the
    /// whole strings, so a folded block still says exactly how much it holds.
    #[test]
    fn a_long_edit_is_folded_to_the_rows_a_payload_costs() {
        let old: String = (0..500).map(|n| format!("old {n}\n")).collect();
        let new: String = (0..500).map(|n| format!("new {n}\n")).collect();
        let call = call_for(
            "edit_file",
            &serde_json::json!({
                "path": "src/lex.rs",
                "edits": [{"old_string": old, "new_string": new}],
            })
            .to_string(),
        );
        let rows = painted(&details(
            &call,
            &facts("src/lex.rs", Some("1 hunk"), Some("8KB")),
            60,
            "± ",
        ));
        assert_eq!(rows.len(), 1 + 8, "the payload's own rows: {rows:#?}");
        assert_eq!(rows[0], "│ diff 1 edit · +500−500");
        assert_eq!(rows[1], "│ − old 0");
        assert_eq!(rows[4], "│ … 993 lines …", "{rows:#?}");
        assert_eq!(rows[8], "│ + new 499", "{rows:#?}");
    }

    /// A write's block is the content itself — no `+` marks, because there is no
    /// old text it was measured against — under the row that says what it is,
    /// and folded like a heredoc body where the content is longer than the fold.
    #[test]
    fn a_write_paints_the_content_it_asked_to_make() {
        let short = call_for(
            "write_file",
            r#"{"path":"src/lex.rs","content":"line 1\nline 2\nline 3\n"}"#,
        );
        assert_eq!(
            painted(&details(
                &short,
                &facts("src/lex.rs", Some("new · 3L"), Some("21B")),
                60,
                "✎ "
            )),
            vec![
                "│ write 3L · content, not output",
                "│ line 1",
                "│ line 2",
                "│ line 3",
            ]
        );
        let content: String = (1..=40).map(|n| format!("line {n}\n")).collect();
        let long = call_for(
            "write_file",
            &serde_json::json!({"path": "src/lex.rs", "content": content}).to_string(),
        );
        let rows = painted(&details(
            &long,
            &facts("src/lex.rs", Some("new · 40L"), Some("380B")),
            60,
            "✎ ",
        ));
        assert_eq!(rows.len(), 1 + 8, "{rows:#?}");
        assert_eq!(rows[0], "│ write 40L · content, not output");
        assert_eq!(rows[1], "│ line 1");
        assert_eq!(rows[4], "│ … 33 lines …", "{rows:#?}");
        assert_eq!(rows[8], "│ line 40", "{rows:#?}");
    }

    /// An `edits` shape the schema would have refused paints no block at all — a
    /// list that is not a list, a `null`, an empty list, an entry that is not an
    /// object, a string that is missing or is not a string, arguments that are
    /// not JSON — and a write's missing or non-string `content` paints none
    /// either. None of it panics: the transcript keeps what the model sent.
    #[test]
    fn a_malformed_edit_or_write_argument_paints_no_block() {
        for arguments in [
            r#"{"path":"src/lex.rs"}"#,
            r#"{"path":"src/lex.rs","edits":null}"#,
            r#"{"path":"src/lex.rs","edits":[]}"#,
            r#"{"path":"src/lex.rs","edits":"nope"}"#,
            r#"{"path":"src/lex.rs","edits":{}}"#,
            r#"{"path":"src/lex.rs","edits":[{}]}"#,
            r#"{"path":"src/lex.rs","edits":[3,null,{"old_string":null}]}"#,
            r#"{"path":"src/lex.rs","edits":[{"old_string":3,"new_string":true}]}"#,
            r#"{"path":"src/lex.rs","edits":[{"old_string":"a"}]}"#,
            r#"{"path":"src/lex.rs","edits":[{"new_string":"b"}]}"#,
            r#"{"path":"src/lex.rs","edits":[{"old_string":"a","new_string":"b"},{}]}"#,
            r#"{"path":"src/lex.rs","edits":[{"old_string":"","new_string":""}]}"#,
            "not json at all",
            "[]",
        ] {
            let rows = details(
                &call_for("edit_file", arguments),
                &facts("src/lex.rs", Some("1 hunk"), None),
                60,
                "± ",
            );
            assert!(
                rows.is_empty(),
                "{arguments} painted {}",
                painted(&rows).join(" / ")
            );
        }
        for arguments in [
            r#"{"path":"src/lex.rs"}"#,
            r#"{"path":"src/lex.rs","content":null}"#,
            r#"{"path":"src/lex.rs","content":3}"#,
            r#"{"path":"src/lex.rs","content":""}"#,
            "nope",
        ] {
            let rows = details(
                &call_for("write_file", arguments),
                &facts("src/lex.rs", Some("new · 3L"), None),
                60,
                "✎ ",
            );
            assert!(
                rows.is_empty(),
                "{arguments} painted {}",
                painted(&rows).join(" / ")
            );
        }
        // A tool that is neither writer paints nothing, whatever text its
        // arguments happen to carry.
        assert!(painted(&details(
            &call_for(
                "read_file",
                r#"{"path":"a.rs","edits":[{"old_string":"x","new_string":"y"}],"content":"z"}"#
            ),
            &facts("a.rs", None, None),
            60,
            "▤ ",
        ))
        .is_empty());
    }

    /// The block is the unfolded view's, and the compact log's one row per call
    /// is the header — which now wears the writers' own measure: the compact row
    /// carries the verdict and the size while the replacement waits under
    /// `Ctrl-O`.
    #[test]
    fn a_writer_block_is_the_unfolded_views_and_the_header_is_its_own() {
        let call = call_for(
            "edit_file",
            r#"{"path":"src/lex.rs","edits":[{"old_string":"one","new_string":"two"}]}"#,
        );
        let facts = facts("src/lex.rs", Some("1 hunk"), Some("6B"));
        assert_eq!(
            painted(&one(&call, &facts, 60, "± ")),
            vec![split_row("± src/lex.rs", "1 hunk", "6B", 60)],
            "the compact log is one row per call, and no block row is painted"
        );
        assert!(
            !details(&call, &facts, 60, "± ").is_empty(),
            "the block is what `Ctrl-O` shows"
        );
    }

    /// The whole block — the lead row, the marks and every content line — is cut
    /// to the pane's own columns at every width the grid's matrix sweeps, a line
    /// wider than the pane included.
    #[test]
    fn a_writer_block_fits_every_pane_width() {
        let edit = call_for(
            "edit_file",
            &serde_json::json!({
                "path": "src/lex.rs",
                "edits": [{
                    "old_string": format!("{}\nshort", "x".repeat(400)),
                    "new_string": "y".repeat(400),
                }],
            })
            .to_string(),
        );
        let write = call_for(
            "write_file",
            &serde_json::json!({
                "path": "src/lex.rs",
                "content": format!("{}\nsecond line", "z".repeat(400)),
            })
            .to_string(),
        );
        for width in 0..=200 {
            for (call, mark) in [(&edit, "± "), (&write, "✎ ")] {
                let rows = painted(&details(
                    call,
                    &facts("src/lex.rs", Some("1 hunk"), Some("400B")),
                    width,
                    mark,
                ));
                assert!(!rows.is_empty(), "width {width}: the block is painted");
                for row in &rows {
                    assert!(
                        UnicodeWidthStr::width(row.as_str()) <= width,
                        "width {width}: {row:?} is wider than the pane"
                    );
                }
            }
        }
    }

    /// The heredocs a command opens: the delimiter's own spellings, `<<-`'s
    /// tab-stripped terminator, a `<<` a quote hides, a `<<<` that is a
    /// herestring, more than one on a line, and a delimiter that never came (the
    /// lines that are there, as a shell reading to the end of its input would
    /// find).
    #[test]
    fn a_command_opens_the_heredocs_its_own_shell_would() {
        let bodies = |command: &str| {
            scripts(command)
                .into_iter()
                .map(|script| script.body)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            bodies("cat > f <<'EOF'\nline \nEOF"),
            vec![vec!["line ".to_string()]]
        );
        assert_eq!(
            bodies("cat <<\"EOF\"\nbody\nEOF"),
            vec![vec!["body".to_string()]]
        );
        assert_eq!(
            bodies("cat <<\\EOF\nbody\nEOF"),
            vec![vec!["body".to_string()]]
        );
        // `<<-` strips the *terminator's* tabs, never a body line's bytes.
        assert_eq!(
            bodies("cat <<-EOF\n\tbody\n\tEOF"),
            vec![vec!["\tbody".to_string()]]
        );
        assert_eq!(
            bodies("cat <<EOF\nbody"),
            vec![vec!["body".to_string()]],
            "a delimiter that never came takes the lines that are there"
        );
        assert_eq!(
            bodies("cat <<A <<B\none\nA\ntwo\nB"),
            vec![vec!["one".to_string()], vec!["two".to_string()]],
            "two heredocs are read in the order the shell reads them"
        );
        assert_eq!(
            bodies("echo \"a << 'EOF'\"\nbody\nEOF"),
            Vec::<Vec<String>>::new(),
            "a quoted `<<` opens nothing"
        );
        assert_eq!(
            bodies("cat <<<EOF"),
            Vec::<Vec<String>>::new(),
            "a herestring is not a heredoc"
        );
        assert_eq!(
            bodies("echo hi\ncat <<EOF\nbody\nEOF"),
            Vec::<Vec<String>>::new(),
            "only the first line opens one: the ask is the first line"
        );
        assert_eq!(bodies("cat <<\nbody"), Vec::<Vec<String>>::new());
    }

    /// The ask's roles: a command's program is bright and its arguments dim —
    /// per stage, and per stage of a chain as well as of a pipeline — a read's
    /// window is dim, a search's `in …` and the case it ran under are dim, a
    /// control's quoted words are dim, and the marker is dim. The pieces always
    /// join back into the ask exactly, because the roles are a reading of the
    /// ask and never a rewrite of it.
    #[test]
    fn the_ask_is_split_into_roles() {
        let roles = |tool: &str, mark: &str, ask: &str| {
            let ask = ask_pieces(&call(tool), &facts(ask, None, None), mark);
            (ask.head, ask.body)
        };
        let (head, body) = roles("run_command", "❯ ", "seq 1 20 | tail -3");
        assert_eq!(head, vec![("❯ ".to_string(), Role::Named)]);
        assert_eq!(
            body,
            vec![
                ("seq".to_string(), Role::Named),
                (" 1 20 ".to_string(), Role::Qualifier),
                ("|".to_string(), Role::Qualifier),
                (" ".to_string(), Role::Qualifier),
                ("tail".to_string(), Role::Named),
                (" -3".to_string(), Role::Qualifier),
            ]
        );
        // A chain is read the same way, and its operators are the seams a row
        // may end at: the program of *every* stage is bright, not the first
        // stage's alone, and the operator itself is neither.
        let (_, body) = roles("run_command", "❯ ", "wc -l f && grep -n x f ; tail -2");
        assert_eq!(
            body,
            vec![
                ("wc".to_string(), Role::Named),
                (" -l f ".to_string(), Role::Qualifier),
                ("&&".to_string(), Role::Seam),
                (" ".to_string(), Role::Qualifier),
                ("grep".to_string(), Role::Named),
                (" -n x f ".to_string(), Role::Qualifier),
                (";".to_string(), Role::Seam),
                (" ".to_string(), Role::Qualifier),
                ("tail".to_string(), Role::Named),
                (" -2".to_string(), Role::Qualifier),
            ]
        );
        // A quoted operator is a character of the pattern the stage ran: no
        // seam, and the line is one stage's reading. The same goes for the
        // `|` of a quoted pattern and for the `2>&1` a shell line may carry.
        for ask in [
            "grep -n \"a;b\" f",
            "grep -n \"a|b\" f",
            "cargo test 2>&1",
            "cargo test > out.log",
        ] {
            let (_, body) = roles("run_command", "❯ ", ask);
            assert!(
                !body.iter().any(|(_, role)| *role == Role::Seam),
                "{ask:?} invented a seam: {body:?}"
            );
        }
        assert_eq!(
            roles("read_file", "▤ ", "src/a.rs 1408→1530"),
            (
                vec![("▤ ".to_string(), Role::Named)],
                vec![
                    ("src/a.rs".to_string(), Role::Named),
                    (" 1408→1530".to_string(), Role::Qualifier),
                ]
            ),
            "the mark is the read's own, whatever glyph the caller hands in"
        );
        assert_eq!(
            roles("search", "⌕ ", "\"column_widths\" in crates · ignore_case"),
            (
                vec![("⌕ ".to_string(), Role::Named)],
                vec![
                    ("\"column_widths\"".to_string(), Role::Named),
                    (" in crates · ignore_case".to_string(), Role::Qualifier),
                ]
            )
        );
        assert_eq!(
            roles("control", "⇄ ", "#4 message \"one more line\""),
            (
                vec![("⇄ ".to_string(), Role::Named)],
                vec![
                    ("#4 message".to_string(), Role::Named),
                    (" \"one more line\"".to_string(), Role::Qualifier),
                ]
            )
        );
        // The head leads with the chip where the call's own `cd` named one,
        // and the body is still *the ask*: the chip is not read out of it, so a
        // role reading cannot lose a byte of the command to it.
        let chipped = ask_pieces(
            &command_call("cd src/lexer && cargo test"),
            &at_cwd(facts("cargo test", None, None), "src/lexer"),
            "❯ ",
        );
        assert_eq!(
            chipped.head,
            vec![
                ("[src/lexer] ".to_string(), Role::Qualifier),
                ("❯ ".to_string(), Role::Named),
            ]
        );
        let joined: String = chipped.body.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(joined, "cargo test");
        // Every tool's pieces join back into the ask they were read from — the
        // role split is a reading and not a rewrite.
        for ask in [
            "seq 1 20 | tail -3",
            "src/a.rs 1408→1530",
            "\"x\" in crates · ignore_case",
            "#4 message \"one more line\"",
            "cargo test · background",
            "",
        ] {
            for tool in ToolName::ALL {
                let pieces = ask_pieces(&call(tool.as_str()), &facts(ask, None, None), "");
                let joined: String = pieces
                    .head
                    .into_iter()
                    .chain(pieces.body)
                    .map(|(text, _)| text)
                    .collect();
                if tool == ToolName::Status {
                    assert!(joined.is_empty(), "status takes no arguments");
                    continue;
                }
                assert_eq!(joined, ask, "{tool:?} rewrote its ask");
            }
        }
    }
}
