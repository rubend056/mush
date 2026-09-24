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
//!   ❯ cargo test                              → exit 0 · 41 lines
//!   │←mark→│←  ask_w  →│←gap→│←  outcome_w    →│
//!   0      2            arrow_x-2    arrow_x   width
//! ```
//!
//! - `outcome_w = (width / 3).clamp(12, 28)` — the whole right-hand column,
//!   arrow included. A third of the pane is the news' share; 12 columns is the
//!   least an outcome sentence reads in (the arrow takes two, leaving ten), and
//!   28 stops the outcome from swallowing a wide terminal's pane.
//! - The outcome's own text budget is `outcome_w - 2`: `→ ` costs two columns.
//! - `arrow_x = width - outcome_w`, and `ask_w = arrow_x - 2` — the two columns
//!   before the arrow are the gap that keeps the ask from touching it.
//! - The ask starts after the call's own `mark` — the glyph and the space after
//!   it the tool's row wears ([`crate::app::symbols`]) — so its text budget is
//!   the ask column less **the mark's measured width**, never an assumed one.
//!   Both the ask and the outcome are cut with `…` from the right: the first
//!   clause of either is the one that matters, and the digest already orders an
//!   outcome's clauses exit-status first.
//!
//! **The two-row block.** The outcome is never dropped. When `ask_w` falls under
//! [`ASK_FLOOR`] — the ask's own column would be a name and three letters — the
//! ask keeps the pane's whole width and the outcome moves to its own row,
//! indented so its `→` sits in the same outcome column as the row above. That
//! two-row shape is a property of the layout rather than a special case: a pane
//! that is narrow for *any* reason gets the same block, and a call that later
//! wants a second row under its header ("`edit_file` occupies X to Y") has the
//! precedent here.
//!
//! **No row is ever wider than the pane.** Every row is cut to its own columns,
//! and at a width below the mark's own four columns the rows degenerate to what
//! fits rather than overflow — the sweep in `chat`'s tests pins it from 20 to
//! 200, and a unit test below pins the refuse-to-overflow below that.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use mush_core::text::truncate;
use mush_core::ToolCall;

use crate::agent::{CallFacts, CallOutcome, Tone};
use crate::ui::dim;

/// The gap between the ask's column and the arrow: two columns, so the `…` of a
/// cut ask never touches the `→`.
const GAP: usize = 2;

/// The outcome column's bounds. See the module doc for why these two.
const OUTCOME_MIN: usize = 12;
const OUTCOME_MAX: usize = 28;

/// The narrowest ask column that still shares its row with the outcome: the
/// mark plus ten columns of ask. Under it the outcome takes its own row.
const ASK_FLOOR: usize = 14;

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

    /// The columns row one has for the mark and the ask: the ask's own column
    /// when the outcome shares the row, the whole pane when it does not.
    pub(crate) fn ask_columns(self) -> usize {
        if self.stacked() {
            self.width
        } else {
            self.ask_w
        }
    }

    /// The columns the outcome's own text has, the arrow's two already spent —
    /// and never more than the pane has left of the arrow, so the degenerate
    /// widths below [`OUTCOME_MIN`] clip instead of overflow.
    pub(crate) fn outcome_columns(self) -> usize {
        let after_arrow = self.width.saturating_sub(self.arrow_x.saturating_add(2));
        self.outcome_w.saturating_sub(2).min(after_arrow)
    }
}

/// One call's header: the mark — which *is* the tool's name — the ask, and,
/// where the result has landed, its outcome in the fixed grid above — one row
/// where they share the pane, two where they do not ([`Grid::stacked`]).
///
/// `mark` is the call's own — the tool's glyph and the space after it
/// ([`crate::app::symbols`]) — and it is **measured**, not assumed: the ask's
/// text stands behind it in the ask's column whatever the glyph's width is. The
/// ask itself is the call's own ([`CallFacts::ask`], already normalised), cut
/// to its column from the right so the name and the start of the ask survive. A
/// call with no outcome yet paints the ask alone: a `→` with nothing after it
/// would be a claim about a phase the transcript cannot see, and the ask keeps
/// exactly the columns it will have when the result lands, so a row does not
/// jump under the human's eyes.
pub(crate) fn header(
    call: &ToolCall,
    facts: &CallFacts,
    width: usize,
    mark: &str,
) -> Vec<Line<'static>> {
    let grid = Grid::of(width);
    let mut rows = vec![Line::from(Span::styled(
        truncate(&ask_text(call, facts, mark), grid.ask_columns()),
        ask_style(),
    ))];
    let Some(outcome) = &facts.outcome else {
        return rows;
    };
    if grid.stacked() {
        rows.push(outcome_row(grid, outcome));
        return rows;
    }
    // The outcome shares the ask's row: pad from the ask's own painted width to
    // the arrow's column and paint the arrow there.
    let ask = truncate(&ask_text(call, facts, mark), grid.ask_columns());
    let gap = grid
        .arrow_x()
        .saturating_sub(UnicodeWidthStr::width(ask.as_str()));
    let mut spans = vec![Span::styled(ask, ask_style())];
    if gap > 0 {
        spans.push(Span::styled(" ".repeat(gap), dim()));
    }
    spans.push(Span::styled("→ ", dim()));
    spans.push(Span::styled(
        truncate(&outcome.text, grid.outcome_columns()),
        tone_style(outcome.tone),
    ));
    rows = vec![Line::from(spans)];
    rows
}

/// The dim fact rows the unfolded view paints under a call's header, each at
/// the call's own **mark width** ([`crate::app::symbols`]'s gutter) and cut to
/// what the pane has left of it. The compact log paints none of them: its one
/// row per call is the header, and these are what the human reads when the call
/// is open.
pub(crate) fn details(facts: &CallFacts, width: usize, mark: &str) -> Vec<Line<'static>> {
    let gutter = UnicodeWidthStr::width(mark);
    let pad = " ".repeat(gutter.min(width));
    let budget = width.saturating_sub(gutter);
    facts
        .details
        .iter()
        .map(|fact| {
            Line::from(Span::styled(
                format!("{pad}{}", truncate(fact, budget)),
                dim(),
            ))
        })
        .collect()
}

/// The outcome's row when it stands alone: the arrow at the grid's own column,
/// under the row above's.
fn outcome_row(grid: Grid, outcome: &CallOutcome) -> Line<'static> {
    let pad = grid.arrow_x().min(grid.width);
    let text = truncate(&outcome.text, grid.outcome_columns());
    // The row is assembled before it is padded, so a pane too narrow even for
    // `→ ` clips the sentence instead of overflowing its columns.
    let content = truncate(&format!("→ {text}"), grid.width.saturating_sub(pad));
    let mut spans = vec![Span::styled(" ".repeat(pad), dim())];
    match content.strip_prefix("→ ") {
        Some(rest) => {
            spans.push(Span::styled("→ ", dim()));
            spans.push(Span::styled(rest.to_string(), tone_style(outcome.tone)));
        }
        // A cut that landed inside the arrow itself: what is left is the arrow's
        // own dim text, and there is no outcome left to colour.
        None => spans.push(Span::styled(content, dim())),
    }
    Line::from(spans)
}

/// The ask under its head: `▤ src/a.rs 5→7`. The mark *is* the tool's name
/// ([`crate::app::symbols`]), so a tool the table knows is not named again;
/// the tools that take no arguments (`status`, `wait`) are their bare mark,
/// `◐` or `⧗`. A name no tool answers to keeps the generic mark *and* its own
/// name — `⚙ frobnicate x` — because `⚙` alone would say nothing about a call
/// mush has never heard of.
fn ask_text(call: &ToolCall, facts: &CallFacts, mark: &str) -> String {
    let name = match mush_core::tools::ToolName::parse(&call.function.name) {
        Some(_) => "",
        None => call.function.name.as_str(),
    };
    // The mark carries its own trailing space, so a known tool's row joins the
    // ask to it directly; only an invented name needs a space of its own before
    // the ask.
    let mut head = String::from(mark);
    if !name.is_empty() {
        head.push_str(name);
        if !facts.ask.is_empty() {
            head.push(' ');
        }
    }
    head.push_str(&facts.ask);
    head
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

    /// A mark is supplied by the caller ([`crate::app::symbols`]), and the grid
    /// measures it: a glyph two columns wide pushes the ask's own text one
    /// column further right, leaves the arrow where the width put it, and still
    /// ends inside the pane at every width.
    #[test]
    fn a_wider_glyph_shifts_the_ask_by_its_own_width() {
        let facts = facts("src/a.rs 5→7", Some("3 lines · 13 B"));
        for width in 20..=200 {
            let narrow = text(&header(&call("read_file"), &facts, width, "▤ ")[0]);
            let wide = text(&header(&call("read_file"), &facts, width, "▤▤ ")[0]);
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
        // block — header, details and payload — and not just the ask.
        let mut read = facts.clone();
        read.details = vec!["of 812 lines".into()];
        assert_eq!(text(&details(&read, 60, "▤ ")[0]), "  of 812 lines");
        assert_eq!(text(&details(&read, 60, "▤▤ ")[0]), "   of 812 lines");
    }

    /// The mark *is* the tool's name: a call of a tool the table knows leads
    /// with its mark and no repeated name (`▤ src/a.rs 5→7`), a call that takes
    /// no arguments is its bare mark (`◐`, `⧗`), and a name no tool answers to
    /// keeps the generic mark *and* its own name, because the glyph alone says
    /// nothing about a call mush has never heard of.
    #[test]
    fn a_known_tool_is_its_mark_and_an_invented_name_keeps_both() {
        let known = facts("src/a.rs 5→7", Some("3 lines"));
        assert_eq!(
            text(&header(&call("read_file"), &known, 60, "▤ ")[0]),
            grid_row_text("▤ src/a.rs 5→7", "3 lines", 60),
            "the mark is the name: `read_file` is not painted again"
        );
        let none = facts("", Some("2 agents · 1 job"));
        assert_eq!(
            text(&header(&call("status"), &none, 60, "◐ ")[0]),
            grid_row_text("◐", "2 agents · 1 job", 60),
            "a tool that takes no arguments is its bare mark"
        );
        let invented = facts("x", Some("error: no such tool"));
        assert_eq!(
            text(&header(&call("frobnicate"), &invented, 60, "⚙ ")[0]),
            grid_row_text("⚙ frobnicate x", "error: no such tool", 60),
            "an invented name keeps the generic mark and its own name"
        );
    }

    /// One row as the grid lays it out: the ask, the gap to the arrow's own
    /// column, and the outcome cut to its budget — what
    /// [`a_known_tool_is_its_mark_and_an_invented_name_keeps_both`] reads
    /// instead of spelling the spaces by hand.
    fn grid_row_text(ask: &str, outcome: &str, width: usize) -> String {
        let grid = Grid::of(width);
        let gap = grid.arrow_x().saturating_sub(UnicodeWidthStr::width(ask));
        format!(
            "{ask}{}→ {}",
            " ".repeat(gap),
            truncate(outcome, grid.outcome_columns())
        )
    }

    fn facts(ask: &str, outcome: Option<&str>) -> CallFacts {
        CallFacts {
            ask: ask.into(),
            outcome: outcome.map(|text| CallOutcome {
                text: text.into(),
                tone: Tone::Ok,
            }),
            details: Vec::new(),
        }
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
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
    /// outcome_w` in every one of them.
    #[test]
    fn the_grid_is_a_third_of_the_pane_between_its_bounds() {
        let grid = Grid::of(90);
        assert_eq!((grid.outcome_w, grid.arrow_x, grid.ask_w), (28, 62, 60));
        assert!(!grid.stacked());
        let grid = Grid::of(133);
        assert_eq!((grid.outcome_w, grid.arrow_x, grid.ask_w), (28, 105, 103));
        assert!(!grid.stacked());
        let grid = Grid::of(45);
        assert_eq!((grid.outcome_w, grid.arrow_x, grid.ask_w), (15, 30, 28));
        let grid = Grid::of(36);
        assert_eq!((grid.outcome_w, grid.arrow_x, grid.ask_w), (12, 24, 22));
        let grid = Grid::of(30);
        assert_eq!((grid.outcome_w, grid.arrow_x, grid.ask_w), (12, 18, 16));
        assert!(!grid.stacked(), "16 columns of ask is above the floor");
        let grid = Grid::of(27);
        assert_eq!(grid.ask_w, 13);
        assert!(grid.stacked(), "13 columns of ask is below the floor");
        let grid = Grid::of(200);
        assert_eq!((grid.outcome_w, grid.arrow_x, grid.ask_w), (28, 172, 170));
    }

    /// The floor is a property of the width and not of the text: a short ask at
    /// a narrow pane still moves its outcome to its own row, so every call in a
    /// pane of that width has the same shape.
    #[test]
    fn the_two_row_block_is_the_floor_and_not_the_text() {
        let short = facts("wait", Some("#188 done"));
        let long = facts(&"x".repeat(500), Some("#188 done"));
        for width in 20..=200 {
            let rows = header(&call("wait"), &short, width, "⧗ ").len();
            assert_eq!(
                rows,
                header(&call("wait"), &long, width, "⧗ ").len(),
                "one shape per width, whatever the ask at {width}"
            );
            assert_eq!(rows, if width < 28 { 2 } else { 1 }, "width {width}");
        }
    }

    /// Every row of a header is exactly its own columns or fewer, and the arrow
    /// stands at the grid's column wherever an outcome is painted — in the
    /// one-row shape and the two-row one. Below two columns the pane cannot hold
    /// `→ ` at all, and the row clips to what it has.
    #[test]
    fn every_row_fits_and_every_arrow_stands_at_the_same_column() {
        for width in 0..=220 {
            let grid = Grid::of(width);
            let facts = facts(
                &"a call with a long ask ".repeat(20),
                Some("exit 0 · 41 lines"),
            );
            let rows = header(&call("run_command"), &facts, width, "❯ ");
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
            let painted = rows.iter().map(|row| text(row)).collect::<Vec<_>>();
            let at = painted
                .iter()
                .position(|row| row.contains('→'))
                .unwrap_or_else(|| panic!("width {width}: an outcome is never dropped"));
            let column = UnicodeWidthStr::width(&painted[at][..painted[at].find('→').unwrap()]);
            assert_eq!(column, grid.arrow_x(), "width {width}: the arrow column");
            assert_eq!(at, usize::from(grid.stacked()), "width {width}: row");
        }
    }

    /// The details are one row per fact, at the gutter, cut to the pane; and a
    /// call with no details paints no row at all.
    #[test]
    fn the_details_sit_at_the_gutter_and_are_cut_to_the_pane() {
        let mut read = facts("read_file", Some("123 lines"));
        read.details = vec!["of 812 lines".into(), "x".repeat(200)];
        let rows = details(&read, 60, "▤ ");
        assert_eq!(rows.len(), 2);
        assert_eq!(text(&rows[0]), "  of 812 lines");
        assert_eq!(UnicodeWidthStr::width(text(&rows[1]).as_str()), 60);
        assert!(text(&rows[1]).starts_with("  "));
        assert!(details(&facts("x", None), 60, "▤ ").is_empty());
    }

    /// An ask is cut to its column from the right, the outcome to its budget,
    /// and the whole row still lands on the arrow's column: the two cuts are
    /// one layout.
    #[test]
    fn a_cut_ask_and_a_cut_outcome_keep_the_arrow_on_its_column() {
        let long = format!("exit 0 · {} lines", "9".repeat(60));
        let rows = header(
            &call("run_command"),
            &facts(&"a long ask ".repeat(30), Some(&long)),
            90,
            "❯ ",
        );
        assert_eq!(rows.len(), 1);
        let row = text(&rows[0]);
        assert_eq!(UnicodeWidthStr::width(row.as_str()), 90);
        let columns = UnicodeWidthStr::width(&row[..row.find('→').unwrap()]);
        assert_eq!(columns, 62, "the arrow's own column");
        assert!(row.contains('…'), "both cuts say they cut: {row:?}");
        assert!(row.ends_with('…'), "the outcome's tail is what goes");
    }

    /// A pane narrower than the mark itself: the rows degenerate to what fits
    /// rather than overflow, and the outcome is still painted somewhere.
    #[test]
    fn a_pane_narrower_than_the_mark_clips_every_row() {
        for width in 0..crate::app::symbols::Symbols::GUTTER {
            let rows = header(&call("wait"), &facts("x", Some("done")), width, "⧗ ");
            for row in &rows {
                let row = text(row);
                assert!(
                    UnicodeWidthStr::width(row.as_str()) <= width,
                    "width {width}: {row:?}"
                );
            }
        }
        assert_eq!(
            text(&header(&call("wait"), &facts("", None), 0, "⧗ ")[0]),
            ""
        );
    }
}
