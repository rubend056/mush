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
//! **The ask is spans by role.** The mark is the row's one bright thing and the
//! call's named target keeps the ask's own colour — a path, a pattern, a
//! command's program per stage — while what *qualifies* the target goes dim: a
//! read's window, a search's `in crates · ignore_case`, a command's arguments
//! and its `· background`, a control's quoted words. [`ask_pieces`] is the one
//! place that decides which is which; the painter only lays the spans out.
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
pub(crate) fn header(
    call: &ToolCall,
    facts: &CallFacts,
    width: usize,
    mark: &str,
) -> Vec<Line<'static>> {
    let grid = Grid::of(width);
    let pieces = cut_ask(ask_pieces(call, facts, mark), grid.ask_columns());
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
        return vec![Line::from(ask_spans(pieces))];
    }
    // The split: both clauses whole, in the pane's box, above the rung.
    let split = measure.is_some()
        && !grid.stacked()
        && grid.splits()
        && verdict_w + GAP + measure_w <= grid.outcome_columns();
    if !split {
        return one_clause(grid, pieces, facts);
    }
    let mut spans = ask_spans(pieces);
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
    vec![Line::from(spans)]
}

/// The one-clause shape: the news at the arrow, or — where the result's only
/// clause is its payload's count — the count, because a `→` with nothing behind
/// it is the claim the grid refuses. A pane whose ask cannot share the row (the
/// [`ASK_FLOOR`] block) gives the outcome a row of its own.
fn one_clause(grid: Grid, pieces: Vec<Piece>, facts: &CallFacts) -> Vec<Line<'static>> {
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
        let mut rows = vec![Line::from(ask_spans(pieces))];
        rows.push(clause_row(grid, &clause, tone));
        return rows;
    }
    let mut spans = ask_spans(pieces);
    let ask_w = painted_width(&spans);
    if grid.arrow_x() > ask_w {
        spans.push(Span::styled(" ".repeat(grid.arrow_x() - ask_w), dim()));
    }
    spans.push(Span::styled("→ ", dim()));
    spans.push(Span::styled(clause, tone_style(tone)));
    vec![Line::from(spans)]
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
    /// The `(3KB)` size marker: dim, and reserved before the ask is cut
    /// ([`cut_ask`]).
    Marker,
}

/// One span of the ask before it is styled: its text and its role.
type Piece = (String, Role);

/// The ask in the roles the pane paints it in — **the one place** that decides
/// which part of a call's ask is its target and which part qualifies it.
///
/// The mark is always the first piece and always [`Role::Named`]. A tool the
/// table knows joins its ask to the mark directly (the mark carries its own
/// trailing space); a name no tool answers to keeps the generic mark *and* its
/// own name — `⚙ frobnicate x` — because `⚙` alone would say nothing about a
/// call mush has never heard of.
fn ask_pieces(call: &ToolCall, facts: &CallFacts, mark: &str) -> Vec<Piece> {
    let mut pieces: Vec<Piece> = vec![(mark.to_string(), Role::Named)];
    match ToolName::parse(&call.function.name) {
        Some(tool) => pieces.extend(ask_roles(tool, &facts.ask)),
        None => {
            pieces.push((call.function.name.clone(), Role::Named));
            if !facts.ask.is_empty() {
                pieces.push((format!(" {}", facts.ask), Role::Qualifier));
            }
        }
    }
    if let Some(marker) = argument_marker(call) {
        pieces.push((format!(" {marker}"), Role::Marker));
    }
    pieces
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
        // A command is read per stage: each `|` stage's program is the target
        // it ran, and that program's arguments qualify it.
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
        // A path, a title, a wait's target: nothing qualifies them here.
        ToolName::Outline
        | ToolName::WriteFile
        | ToolName::EditFile
        | ToolName::ListFiles
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

/// A command's ask, one span per stage: the program each `|` stage runs is a
/// target, everything else — its arguments, the pipe, the spaces around it — a
/// qualifier. The pieces join back into the ask exactly, so the roles never
/// change what the row says.
fn stages(ask: &str) -> Vec<Piece> {
    let mut pieces: Vec<Piece> = Vec::new();
    for (at, stage) in ask.split('|').enumerate() {
        if at > 0 {
            pieces.push(("|".to_string(), Role::Qualifier));
        }
        let lead = &stage[..stage.len() - stage.trim_start().len()];
        let rest = stage.trim_start();
        let trimmed = rest.trim_end();
        let trail = &rest[trimmed.len()..];
        if !lead.is_empty() {
            pieces.push((lead.to_string(), Role::Qualifier));
        }
        match trimmed.find(char::is_whitespace) {
            Some(at) => {
                pieces.push((trimmed[..at].to_string(), Role::Named));
                pieces.push((trimmed[at..].to_string(), Role::Qualifier));
            }
            None if !trimmed.is_empty() => pieces.push((trimmed.to_string(), Role::Named)),
            None => {}
        }
        if !trail.is_empty() {
            pieces.push((trail.to_string(), Role::Qualifier));
        }
    }
    pieces
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
/// that does not fit ends the row with `…`.
fn cut_ask(pieces: Vec<Piece>, budget: usize) -> Vec<Piece> {
    let mut pieces = pieces;
    let marker = match pieces.last() {
        Some((_, Role::Marker)) => pieces.pop(),
        _ => None,
    };
    let marker_w = marker
        .as_ref()
        .map_or(0, |(text, _)| UnicodeWidthStr::width(text.as_str()));
    let text_budget = budget.saturating_sub(marker_w);
    let mut out: Vec<Piece> = Vec::new();
    let mut used = 0;
    for (text, role) in pieces {
        if used >= text_budget {
            break;
        }
        let room = text_budget - used;
        let width = UnicodeWidthStr::width(text.as_str());
        if width <= room {
            used += width;
            out.push((text, role));
            continue;
        }
        out.push((truncate(&text, room), role));
        used = text_budget;
        break;
    }
    if let Some((text, role)) = marker {
        let room = budget.saturating_sub(used);
        if room > 0 {
            out.push((truncate(&text, room), role));
        }
    }
    out
}

/// The ask's pieces as painted spans: the named parts in the ask's colour,
/// everything else dim.
fn ask_spans(pieces: Vec<Piece>) -> Vec<Span<'static>> {
    pieces
        .into_iter()
        .map(|(text, role)| match role {
            Role::Named => Span::styled(text, ask_style()),
            Role::Qualifier | Role::Marker => Span::styled(text, dim()),
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

/// The dim fact rows the unfolded view paints under a call's header, each at
/// the block's own **gutter** ([`crate::app::symbols`]'s pipe) and cut to what
/// the pane has left of it. The compact log paints none of them: its one row
/// per call is the header, and these are what the human reads when the call is
/// open.
///
/// The gutter is the same one the result's payload wears ([`crate::app::chat`]
/// paints it through the same constant), so the header, its facts and the dump
/// under them read as one block — which is what the human's `Ctrl-Y` walk and
/// the pane's own columns both measure against.
pub(crate) fn details(facts: &CallFacts, width: usize, mark: &str) -> Vec<Line<'static>> {
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
        // block — header, details and payload — and not just the ask; the pipe
        // leads each of them, padded to the same width ([`details`]).
        let mut read = facts.clone();
        read.details = vec!["of 812L".into()];
        assert_eq!(text(&details(&read, 60, "▤ ")[0]), "│ of 812L");
        assert_eq!(text(&details(&read, 60, "▤▤ ")[0]), "│  of 812L");
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
            text(&header(&call("read_file"), &known, 60, "▤ ")[0]),
            split_row("▤ src/a.rs 5→7", "exit 3", "13L 13B", 60),
            "the mark is the name: `read_file` is not painted again"
        );
        let none = facts("", Some("2 agents · 1 job"), None);
        assert_eq!(
            text(&header(&call("status"), &none, 60, "◐ ")[0]),
            one_clause_row("◐", "2 agents · 1 job", 60),
            "a tool that takes no arguments is its bare mark"
        );
        let invented = facts("x", Some("error"), None);
        assert_eq!(
            text(&header(&call("frobnicate"), &invented, 60, "⚙ ")[0]),
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
    /// a narrow pane still moves its outcome to its own row, so every call in a
    /// pane of that width has the same shape.
    #[test]
    fn the_two_row_block_is_the_floor_and_not_the_text() {
        let short = facts("wait", Some("#188 done"), None);
        let long = facts(&"x".repeat(500), Some("#188 done"), None);
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
                Some("exit 3"),
                Some("41L 1.2KB"),
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
                .position(|row| row.contains("→ "))
                .unwrap_or_else(|| panic!("width {width}: an outcome is never dropped"));
            let column = UnicodeWidthStr::width(&painted[at][..painted[at].find('→').unwrap()]);
            assert_eq!(column, grid.arrow_x(), "width {width}: the arrow column");
            assert_eq!(at, usize::from(grid.stacked()), "width {width}: row");
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
            let row = text(&header(&call("run_command"), &facts, width, "❯ ")[0]);
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
        let row = text(&header(&call("status"), &sentence, 88, "◐ ")[0]);
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
        let row = text(&header(&call("run_command"), &measured, 88, "❯ ")[0]);
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
        let row = text(&header(&call("run_command"), &exit, 59, "❯ ")[0]);
        assert_eq!(row, one_clause_row("❯ cargo test", "exit 3", 59));
        assert!(!row.contains("41L"), "under the rung the measure is gone");
        let row = text(&header(&call("run_command"), &exit, 60, "❯ ")[0]);
        assert!(row.ends_with("41L 1.2KB"), "at the rung it splits: {row:?}");
        // The same pane, a pair that does not fit: the verdict keeps the whole
        // box and the measure waits for a wider pane.
        let long = facts(
            "cargo test",
            Some("no children and no jobs"),
            Some("12345L/67890L 1.2MB"),
        );
        let row = text(&header(&call("run_command"), &long, 88, "❯ ")[0]);
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
        let rows = details(&read, 60, "▤ ");
        assert_eq!(rows.len(), 2);
        assert_eq!(text(&rows[0]), "│ of 812L");
        assert_eq!(UnicodeWidthStr::width(text(&rows[1]).as_str()), 60);
        assert!(text(&rows[1]).starts_with("│ "));
        assert!(details(&facts("x", None, None), 60, "▤ ").is_empty());
    }

    /// An ask is cut to its column from the right, the measure to its own room,
    /// and the whole row still lands on the arrow's column: the two cuts are
    /// one layout.
    #[test]
    fn a_cut_ask_and_a_cut_measure_keep_the_arrow_on_its_column() {
        let rows = header(
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
            let rows = header(&call("wait"), &facts("x", Some("done"), None), width, "⧗ ");
            for row in &rows {
                let row = text(row);
                assert!(
                    UnicodeWidthStr::width(row.as_str()) <= width,
                    "width {width}: {row:?}"
                );
            }
        }
        assert_eq!(
            text(&header(&call("wait"), &facts("", None, None), 0, "⧗ ")[0]),
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
        let row = text(&header(&short, &facts, 88, "❯ ")[0]);
        assert_eq!(row, "❯ python3 - <<'PY'", "under the threshold: nothing");
        let long = long_call("run_command", ARGUMENT_MARKER);
        let row = text(&header(&long, &facts, 88, "❯ ")[0]);
        assert_eq!(
            row, "❯ python3 - <<'PY' (300B)",
            "at the threshold: the size"
        );
        // Dim, not bright: the marker is a fact about the arguments and not one
        // of them. The last span is the marker's.
        let spans = &header(&long, &facts, 88, "❯ ")[0].spans;
        assert_eq!(spans.last().unwrap().content.as_ref(), " (300B)");
        assert_eq!(spans.last().unwrap().style, dim());
    }

    /// A long command's own size is reserved before the ask is cut: the very
    /// cut the marker explains can never eat it — the marker is painted whole
    /// whenever the ask's column can hold it, and what goes is the ask's text.
    #[test]
    fn the_cut_never_eats_the_marker() {
        let long_ask = facts(&"x".repeat(400), None, None);
        let call = long_call("run_command", 4_000);
        for width in 20..=120 {
            let row = text(&header(&call, &long_ask, width, "❯ ")[0]);
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
        let row = text(&header(&call, &facts("a long command", None, None), 20, "❯ ")[0]);
        assert!(row.ends_with("(4KB)"), "{row:?}");
        assert!(row.contains('…'), "the ask is the part cut: {row:?}");
    }

    /// The ask's roles: a command's program is bright and its arguments dim —
    /// per stage — a read's window is dim, a search's `in …` and the case it ran
    /// under are dim, a control's quoted words are dim, and the marker is dim.
    /// The pieces always join back into the ask exactly, because the roles are a
    /// reading of the ask and never a rewrite of it.
    #[test]
    fn the_ask_is_split_into_roles() {
        let roles = |tool: &str, mark: &str, ask: &str| {
            ask_pieces(&call(tool), &facts(ask, None, None), mark)
        };
        assert_eq!(
            roles("run_command", "❯ ", "seq 1 20 | tail -3"),
            vec![
                ("❯ ".to_string(), Role::Named),
                ("seq".to_string(), Role::Named),
                (" 1 20".to_string(), Role::Qualifier),
                (" ".to_string(), Role::Qualifier),
                ("|".to_string(), Role::Qualifier),
                (" ".to_string(), Role::Qualifier),
                ("tail".to_string(), Role::Named),
                (" -3".to_string(), Role::Qualifier),
            ]
        );
        assert_eq!(
            roles("read_file", "▤ ", "src/a.rs 1408→1530"),
            vec![
                ("▤ ".to_string(), Role::Named),
                ("src/a.rs".to_string(), Role::Named),
                (" 1408→1530".to_string(), Role::Qualifier),
            ],
            "the mark is the read's own, whatever glyph the caller hands in"
        );
        assert_eq!(
            roles("search", "⌕ ", "\"column_widths\" in crates · ignore_case"),
            vec![
                ("⌕ ".to_string(), Role::Named),
                ("\"column_widths\"".to_string(), Role::Named),
                (" in crates · ignore_case".to_string(), Role::Qualifier),
            ]
        );
        assert_eq!(
            roles("control", "⇄ ", "#4 message \"one more line\""),
            vec![
                ("⇄ ".to_string(), Role::Named),
                ("#4 message".to_string(), Role::Named),
                (" \"one more line\"".to_string(), Role::Qualifier),
            ]
        );
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
                let joined: String = ask_pieces(&call(tool.as_str()), &facts(ask, None, None), "")
                    .into_iter()
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
