//! The `Screen` view: everything one frame paints, derived from `App` once.
//!
//! The painter used to read `App` directly, which put two derivations of the
//! same fact in the tree — one in the state, one in the drawing code — and made
//! the only test a frame could carry "does not panic": a test that wanted to ask
//! a question about a *row* had to build a whole `App` and read a buffer back.
//! This module builds the frame as a value: the size tiers, the pane rects, the
//! words on every row, the bar's line, the popup's items. `ui.rs` then only
//! paints, and no render function takes `&App` any more (refactor B17, Stage 3).
//!
//! Everything here is *read*: `App::screen` takes `&self` and cannot move a
//! phase, age a status or write a session, so the value a frame is painted from
//! is the state, not a copy that could drift from it.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

use mush_core::git;
use mush_core::message::Image;
use mush_core::text::{sanitize, truncate};

use crate::ui::dim;

use super::{
    image_label, is_below_floor, short_age, AgentId, AgentNode, App, Focus, Landed, Pane, Phase,
    PickerKind, Rank, StatusKind, MIN_HEIGHT, MIN_WIDTH,
};

/// Beyond this the transcript is unreadable, however wide the terminal is.
const MAX_TRANSCRIPT: u16 = 110;
/// How many columns the agent pane is given, and why it is a length rather than
/// a share of the terminal.
///
/// R1's row spends its fields left to right — `state · branch +delta · what it
/// is doing · its title` — and that is about forty-five columns of real labels.
/// Below thirty the chat is the better use of a narrow screen; past fifty the
/// tree has nothing else to put there (a tool label is the widest field it has)
/// while a wider terminal is what the transcript's measure is for (it is capped
/// at 110 columns anyway). The share this replaced was 26%, which is 31 columns
/// at 120: `▶◐ #0` left 22 for a 23-column `edit_file src/lib.rs 12s`, so a busy
/// agent's tool call and its age were dropped there — every frame, on the size
/// the audit photographs.
const AGENTS_MIN_COLUMNS: u16 = 30;
const AGENTS_MAX_COLUMNS: u16 = 50;
/// The chat below this is a column of broken words, whatever the tree wants.
const CHAT_MIN_COLUMNS: u16 = 40;
/// How old the git read may be before the facts line says so. The bar's
/// convention is that a line is "just happened" within five seconds; a snapshot
/// a little older than that is still a glance, but past ten seconds an
/// untouched screen is showing a read no event has refreshed, and a cached fact
/// must not read as a live one (finding P8).
const GIT_STALE: Duration = Duration::from_secs(10);
/// How many rows a message box may grow to.
const MAX_INPUT_LINES: u16 = 6;

/// How many rows the attached images may take in the message box. Three: a box
/// that grows with every picture would push the transcript off the screen, and
/// past this the last row counts the rest instead of naming them — the title
/// still says how many, so nothing is hidden, only abbreviated.
///
/// This is the cap on the rows the box *asks* for; a box the layout granted
/// fewer rows paints fewer, because the text's own row comes first
/// ([`content_rows`]).
const MAX_ATTACHMENT_ROWS: usize = 3;

/// The popup the pickers paint in: a share of the terminal, floored so a model
/// list is readable and capped so it does not sprawl on a wide one. One formula,
/// because the popup's own rect is sized with it and `picker_text_width` says
/// how much of it a `/notes` row may use.
const PICKER_MIN_WIDTH: u16 = 40;
const PICKER_MAX_WIDTH: u16 = 80;

/// A share of `whole`, clamped between `min` and `max`.
///
/// One integer type, wide enough to hold the multiply: `whole * percent`
/// overflows a `u16` above a few hundred columns — at the picker's 60% it
/// panics in a debug build above 1092 columns, while a release build wraps and
/// the clamp quietly turns the wrap into the floor (finding R72).
fn share(whole: u16, percent: u32, min: u16, max: u16) -> u16 {
    ((whole as u32 * percent / 100) as u16).clamp(min, max)
}

fn picker_width(terminal_width: u16) -> u16 {
    share(terminal_width, 60, PICKER_MIN_WIDTH, PICKER_MAX_WIDTH)
}

/// The columns the picker's list gives one item's text. The term carries the
/// popup's two border columns, the two the `› ` symbol reserves, and the two the
/// `  ` indent every row wears. `App` wraps a `/notes` report to this, so the
/// lines it hands the list already fit the width they are painted at — wrapping
/// to any other width (the old fixed 74) is how the documented escape hatch
/// clipped.
pub(crate) fn picker_text_width(terminal_width: u16) -> usize {
    picker_width(terminal_width).saturating_sub(6) as usize
}

/// The columns the agent pane is painted in — see the constants above for why
/// this is a length: a row's four ranked fields need about forty of them, the
/// chat keeps its own floor, and past the cap the extra columns are empty.
fn agents_columns(terminal_width: u16) -> u16 {
    share(terminal_width, 34, AGENTS_MIN_COLUMNS, AGENTS_MAX_COLUMNS)
        .min(terminal_width.saturating_sub(CHAT_MIN_COLUMNS))
}

/// The rect inside a pane's border — the same arithmetic `Block::inner` does in
/// the painter, computed here because the words a pane paints (its title, its
/// row's fields, the wrapped message box) are laid out for that width.
fn inner(area: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(area)
}

/// The message box's attachment rows: `▣ path (format · size)`, one per image,
/// at most `cap` rows and never more than [`MAX_ATTACHMENT_ROWS`], in the order
/// they were attached.
///
/// The cap is the room the caller has for them — [`content_rows`] passes the
/// rows the box was granted, one of them kept for the text. Past the cap the
/// last row counts the rest instead of naming them — the box is a box, and
/// there is no room to spend on the rest. The count of *everything* attached is
/// the title's, not that row's, so an abbreviated list never claims to be the
/// whole one.
fn attachment_rows(images: &[Image], cap: usize) -> Vec<String> {
    let cap = cap.min(MAX_ATTACHMENT_ROWS);
    if cap == 0 {
        return Vec::new();
    }
    let label = |image: &Image| format!("▣ {}", image_label(image));
    if images.len() <= cap {
        return images.iter().map(label).collect();
    }
    let mut rows: Vec<String> = images.iter().take(cap - 1).map(label).collect();
    rows.push(format!("▣ +{} more", images.len() - (cap - 1)));
    rows
}

/// The box's content rows for the room it was granted: the attachment rows that
/// fit above the draft, and how many rows are left for the text.
///
/// The one owner of "how many rows the content has" when the box is painted.
/// [`App::input_rows`] asks the layout for the same count from the other side —
/// the draft's lines, the attachments and the two border rows — and the two
/// agree whenever the layout grants the ask. When it does not (a short column at
/// a small terminal), this is what gives: the attachment rows are capped at one
/// row short of the room, so the text has a row first and
/// `text_rows = field_height - attachments.len()` is true by construction. A row
/// the box does not have is a row the message being typed would be pushed out
/// of (finding D5).
fn content_rows(field_height: u16, images: &[Image]) -> (Vec<String>, usize) {
    let attachments = attachment_rows(images, (field_height as usize).saturating_sub(1));
    let text_rows = (field_height as usize).saturating_sub(attachments.len());
    (attachments, text_rows)
}

/// The bar's rows: two from 24 up, so the facts line — the branch, the dirty
/// count and the line delta — is on screen at the ubiquitous 80×24, where it
/// used to need 26 and was simply absent (finding P12). Below 24 the extra row
/// is worth more to the transcript, and the compact footer carries the selected
/// agent's own branch and worktree instead.
///
/// One predicate, because the prover's layout and the test helper that reads
/// back the rows the bar does not cover must agree on where the bar starts
/// (finding D10).
pub(super) fn bar_rows(height: u16) -> u16 {
    if height >= 24 {
        2
    } else {
        1
    }
}

/// The longest honest spelling of the floor that fits `width` columns.
///
/// The notice was one fixed 25-column string, so a 24-column terminal painted
/// `mush needs at least 40×1` — a truncation that names a size the program does
/// not need, which is a lie the audit caught (finding P11). The spellings are
/// ranked, longest first, and the first that fits is the one painted; only a
/// terminal narrower than `40×10` itself gets a shorter form still.
fn floor_notice(width: u16) -> String {
    let size = format!("{MIN_WIDTH}×{MIN_HEIGHT}");
    let candidates = [
        format!("mush needs at least {size}"),
        format!("needs at least {size}"),
        format!("{size} minimum"),
        format!("needs {size}"),
        size,
    ];
    candidates
        .into_iter()
        .find(|text| UnicodeWidthStr::width(text.as_str()) <= width as usize)
        .unwrap_or_else(|| format!("{MIN_WIDTH}×{MIN_HEIGHT}"))
}

/// One painted frame.
///
/// The two variants are the whole of R3: a terminal below the floor paints one
/// notice and *nothing else* — the panes are not built at all, so a frame below
/// the floor cannot accidentally show a shard of a pane — and a terminal with
/// room paints the panes.
pub enum Screen {
    /// Below the floor (`is_below_floor`): one notice, centred on both axes,
    /// and the same predicate `App` refuses the keyboard with, so the notice
    /// and the keys agree (finding P11 / refactor B3).
    Floor {
        /// The frame's own rect: the notice is centred in it.
        area: Rect,
        /// The longest honest spelling of the floor that fits `area.width`.
        text: String,
    },
    /// A terminal with room for the panes. Boxed so the common case is a
    /// pointer, not a copy of every word on the screen.
    Panes(Box<Panes>),
}

/// The panes of one frame, in paint order.
pub struct Panes {
    pub agents: AgentsPane,
    pub chat: ChatPane,
    pub bar: BarPane,
    /// The modal list, painted last because it covers what is under it.
    pub picker: Option<PickerPane>,
    /// Which pane has the keyboard: the one fact the two borders, the message
    /// box's cursor and the bar's badge are painted from (finding T2 §11).
    pub focus: Focus,
}

/// The agent tree pane: one row per agent, and the cursor row's facts.
pub struct AgentsPane {
    /// The pane's whole rect, border included.
    pub area: Rect,
    /// Where the rows are painted: the inner rect minus the footer and its
    /// separator. The painter reads this instead of working the same geometry
    /// out again, so the hidden-row counts (which are arithmetic over it) are
    /// counts of the rows that are really on screen (finding V1).
    pub list_area: Rect,
    /// The pane's title, already elided to the columns this pane has: the
    /// clauses, ranked so that the ones that exist *only* here come first (the
    /// hidden-row counts `▲3`, `▼17`, then what the whole tree is doing, then
    /// the branches' totals), with the ones that do not fit dropped whole from
    /// the right. Whole, because a clause cut mid-number is a count that is not
    /// the count; and elided here rather than in the painter, so the `Screen`
    /// owns every word a frame paints (finding D9).
    pub title: String,
    pub rows: Vec<AgentRow>,
    /// The row the cursor is on: an index into `rows`.
    pub cursor: usize,
    /// The cursor row's full facts, at most three rows, already chosen and cut
    /// for the room the pane has. Empty when the pane has no room for a footer.
    pub footer: Vec<Line<'static>>,
}

/// One tree row, with its fields already derived: the glyph from the node's own
/// phase, the title from its brief, the branch and delta, the activity with its
/// age. The row answers "what is happening" with the fields that answer it —
/// and the painter only has to fit them into the columns it has.
pub struct AgentRow {
    pub id: AgentId,
    /// How many `  ` indents the row is drawn with.
    pub depth: usize,
    /// `·`, `◐`, `✓`, `✗`, `⊘`, `≡` — derived from the node's own phase.
    pub glyph: &'static str,
    /// The tree's focused agent: the row wearing `▶`.
    pub focused: bool,
    /// Children whose own run is in flight, drawn as `⏸N`.
    pub waiting: usize,
    /// This agent's own is in the list and its parent has not read its result
    /// yet, drawn as `✉` (finding H4).
    pub result_unread: bool,
    /// How many of this agent's children's results it owes a read on, drawn as
    /// `✉N`: the same fact as the children's own `✉`, read from the other end,
    /// and the one that survives a pane too short to show their rows.
    pub unread_children: usize,
    pub title: String,
    /// The branch, its delta and any jobs: `mush/3 +12−4 ⚙1`.
    pub place: String,
    /// What it is doing, with its age: `thinking 3s`, `edit_file a.rs 12s`,
    /// `waiting on results 3s` (the `wait` tool's own noun), `compacting 2s`.
    pub activity: String,
}

/// The chat column: the transcript and the message box beside it.
pub struct ChatPane {
    /// The transcript's whole rect, border included.
    pub transcript_area: Rect,
    /// The message box's whole rect, border included.
    pub input_area: Rect,
    /// The transcript as `Chat` rendered it, windowed to the room it has, or
    /// `None` when the pane has no inner room at all — a pane that short paints
    /// its border and nothing else — or no rows to paint by construction, which
    /// is how the zen view leaves the message box alone on screen.
    pub transcript: Option<Painted>,
    /// The message box as it is painted, or `None` for the same reason.
    pub input: Option<InputPane>,
}

/// The message box: the prompt, the windowed lines, and where the cursor is in
/// them.
pub struct InputPane {
    pub prompt: String,
    /// The attachment rows, above the text: `▣ path (format · size)`, one per
    /// image, at most [`MAX_ATTACHMENT_ROWS`] — the last of which counts the
    /// rest when there are more — and never more than the room the box has
    /// above the text's own row. Dim, because they are what is about to be
    /// said and not what is being typed.
    pub attachments: Vec<String>,
    /// How many images are attached. The rows are capped, so the title's count
    /// cannot be read off them (`▣ +2 more` counts what is left, not the whole);
    /// the number lives once, beside the rows it is already arithmetic over.
    pub attachment_count: usize,
    /// The lines the box shows, already windowed around the cursor.
    pub lines: Vec<String>,
    /// The line the cursor is on, in `lines`.
    pub cursor_row: usize,
    /// The cursor's display column within that line.
    pub column: usize,
}

/// The bar: the focus badge, the newest word with no other home, and the stable
/// facts.
pub struct BarPane {
    /// The bar's rows: one, or two on a terminal at least 24 rows tall.
    pub area: Rect,
    /// The line that won the precedence table, or `None` for the idle hint.
    /// The rank is carried rather than a colour because the colour is a
    /// painting decision and the order is not.
    pub word: Option<(Rank, String)>,
    /// The facts line, when the bar has the second row for it.
    pub facts: Option<String>,
}

/// The modal list `/model`, `/provider` and `/notes` are read in.
pub struct PickerPane {
    /// The popup's whole rect.
    pub area: Rect,
    pub title: String,
    pub hint: &'static str,
    /// The window of items, each already marked with `• `/`  ` and defanged.
    pub items: Vec<String>,
    /// The cursor's index into `items`.
    pub cursor: usize,
}

use super::chat::Painted;

impl App {
    /// Everything one frame paints, derived from the state in one place.
    ///
    /// `area` is the frame's own rect — the same one `main` reports to the app
    /// and the same one `App::below_floor` answers from — so the layout tiers
    /// are arithmetic about the terminal, computed once, instead of being
    /// recomputed by the painter from a size it read off the frame.
    pub fn screen(&self, area: Rect) -> Screen {
        if is_below_floor(area.width, area.height) {
            return Screen::Floor {
                area,
                text: floor_notice(area.width),
            };
        }

        // Size tiers (docs/mush.md §4.5 R3). Narrow or short terminals stack the
        // agent strip above the chat, because two columns starve both panes.
        let compact = area.width < 80 || area.height < 20;
        let bar_rows = bar_rows(area.height);

        let (agents_area, chat_area, bar_area) = if compact {
            // Every pane's height is a `Length`, so the three add up to the
            // terminal exactly and none of them can lose rows to another. The
            // bar used to be a trailing `Length` behind a `Min(6)` chat, and at
            // 40×10 the chat took the row the bar was owed: the frame painted
            // the tree, the transcript and the message box, and the ` chat `
            // row — the focus badge, the key hint, and the only home an Info
            // line or a command's usage error has — was simply absent.
            // Six is the least the chat can be and still hold what it is for: a
            // three-row transcript over a message box that has a row to type
            // in.
            let chat_min = 6;
            let agent_rows = (self.tree.agents.len() as u16 + 2)
                .clamp(3, 6)
                .min(area.height.saturating_sub(bar_rows + chat_min));
            let chat_rows = area.height - agent_rows - bar_rows;
            let rows = Layout::vertical([
                Constraint::Length(agent_rows),
                Constraint::Length(chat_rows),
                Constraint::Length(bar_rows),
            ])
            .split(area);
            (rows[0], rows[1], rows[2])
        } else {
            let rows = Layout::vertical([
                Constraint::Length(area.height - bar_rows),
                Constraint::Length(bar_rows),
            ])
            .split(area);
            // On a very wide terminal the tree stops growing: past a point it is
            // empty space, and the chat is what the width belongs to. Below that
            // it gets the columns R1's row needs, so the fields the row is built
            // from are the fields it can paint.
            let columns = Layout::horizontal([
                Constraint::Length(agents_columns(area.width)),
                Constraint::Min(20),
            ])
            .split(rows[0]);
            (columns[0], columns[1], rows[1])
        };

        // The chat column's split, once: the transcript above the message box.
        // Both zen arms read these same rows, so the box cannot move when the
        // tree gives up its rows (finding D13).
        let chat_split =
            Layout::vertical([Constraint::Min(3), Constraint::Length(self.input_rows())])
                .split(chat_area);

        // The zen view ([`App::zen`]): the focused pane takes what the two
        // panes shared, because the terminal's own drag — mush deliberately
        // does not capture the mouse (finding K3) — takes a rectangle of cells,
        // and at 80 columns that rectangle starts in the agents pane. The
        // two-pane layout above is the source of every row this hands over —
        // the chat column's split and the message box's rows included, so the
        // box keeps exactly the rows it had (finding D13).
        let (agents_area, chat) = match (self.zen, self.focus) {
            (false, _) => (
                agents_area,
                self.chat_pane_with(chat_split[0], chat_split[1]),
            ),
            // The chat takes the rows above the bar whole — what the agents
            // pane and the chat had between them — and the agents pane becomes
            // a zero rect, so nothing can paint in it. The box keeps the rows
            // the two-pane split gave it, widened to the frame, and the
            // transcript is what remains above it.
            (true, Focus::Chat) => {
                let box_area = Rect {
                    x: area.x,
                    width: area.width,
                    ..chat_split[1]
                };
                (
                    Rect::new(area.x, area.y, 0, 0),
                    self.chat_pane_with(
                        Rect {
                            y: area.y,
                            height: box_area.y.saturating_sub(area.y),
                            ..box_area
                        },
                        box_area,
                    ),
                )
            }
            // The message box keeps the rows the two-pane layout gave it — the
            // split above, not a re-derivation — and takes the width; the tree
            // gets everything above it.
            (true, Focus::Agents) => {
                let box_area = Rect {
                    x: area.x,
                    width: area.width,
                    ..chat_split[1]
                };
                (
                    Rect {
                        y: area.y,
                        height: box_area.y.saturating_sub(area.y),
                        ..box_area
                    },
                    self.chat_box(box_area),
                )
            }
        };

        Screen::Panes(Box::new(Panes {
            agents: self.agents_pane(agents_area),
            chat,
            bar: self.bar_pane(bar_area),
            picker: self.picker_pane(area),
            focus: self.focus,
        }))
    }

    /// The tree pane: the rows in painted order — pre-order over the parent
    /// links, so a child is drawn under its parent rather than after everything
    /// spawned before it (finding U4) — the cursor row's footer, and the title.
    fn agents_pane(&self, area: Rect) -> AgentsPane {
        // A pane zen hid is a zero rect: there are no columns to fit a row into
        // and no row to paint, so nothing is derived for it — the same choice
        // `Screen::Floor` makes when it builds no panes at all, so a hidden
        // pane cannot paint a shard of itself. The counts a title would carry
        // are not lost: the conversation pane's title reads them from
        // `agent_count_cells`.
        if area.width == 0 || area.height == 0 {
            return AgentsPane {
                area,
                list_area: inner(area),
                title: String::new(),
                rows: Vec::new(),
                cursor: 0,
                footer: Vec::new(),
            };
        }
        let inner = inner(area);
        let nodes = self.tree.rows();
        let cursor = self.tree.cursor();
        // The rows are built before the footer, because the footer reads the
        // cursor row's activity off the row already built for it. Deriving
        // `phase_detail` a second time here would call `node.since.elapsed()`
        // again, and a clock tick between the two calls would paint two ages
        // for one frame (finding R26).
        let rows = self.rows(&nodes);

        // The cursor row's facts live in a footer under the list, so the list
        // may degrade to `◐ #2` on a narrow pane without losing anything: facts
        // move, they do not vanish. A tall pane spends up to three lines on it;
        // a short (compact) pane still owes the selected row one line — under
        // six inner rows it got none, so in compact the selected row's branch,
        // worktree and landing commands were nowhere on screen (finding P12).
        //
        // The cursor names a row `tree.rows()` painted, and the two vectors the
        // pane would index are that call's own; `get` rather than `[]` keeps a
        // pane handed a cursor past its rows painting a list with no footer
        // instead of taking the frame — and the session — down with it (D2).
        let footer = if inner.width == 0 || inner.height < 3 {
            Vec::new()
        } else {
            let budget = if inner.height >= 8 { 3 } else { 1 };
            match nodes.get(cursor).zip(rows.get(cursor)) {
                Some((node, row)) => {
                    compact_footer(agent_footer(self, node, row, inner.width as usize), budget)
                }
                None => Vec::new(),
            }
        };
        // One row is the separator between the list and the facts.
        let footer_rows = if footer.is_empty() {
            0
        } else {
            footer.len() as u16 + 1
        };
        let list_area = Rect {
            height: inner.height.saturating_sub(footer_rows),
            ..inner
        };

        // How many rows the list leaves off screen, and which side. `List`
        // scrolls to keep the cursor visible and this pane is one row per agent,
        // so the window is arithmetic rather than a guess: with the cursor in
        // view the first visible row is the cursor's row minus the rows above
        // it. `▲`/`▼` name the side, which a bare `+17` cannot — at the bottom
        // of a 4-row pane over nineteen agents the hidden rows are all above
        // (finding P12).
        let visible = list_area.height as usize;
        let first = cursor.saturating_sub(visible.saturating_sub(1));
        let above = if visible == 0 {
            0
        } else {
            first.min(nodes.len())
        };
        let below = if visible == 0 {
            nodes.len()
        } else {
            nodes.len().saturating_sub(first + visible)
        };

        AgentsPane {
            area,
            list_area,
            // The pane's title: ` agents · 3 working · 2 jobs · 2 waiting ·
            // Σ +324 −40`, with the clauses that do not fit dropped whole from
            // the right and the pane's own name kept when none of them fit.
            title: elide(
                &title_cells(self, above, below),
                " · ",
                " agents · ",
                " agents ",
                inner.width as usize,
            ),
            rows,
            cursor,
            footer,
        }
    }

    /// The rows for `nodes`, in their order, with the parent's busy-child count
    /// derived in one [`super::tree::AgentTree::busy_counts`] walk for the whole
    /// call rather than once per row, which was quadratic in the tree the pane
    /// paints (finding R29).
    ///
    /// `pub(super)` so the attach roster serializes the very rows the pane
    /// paints instead of deriving them a second time (finding R21).
    pub(super) fn rows(&self, nodes: &[&AgentNode]) -> Vec<AgentRow> {
        let busy = self.tree.busy_counts();
        nodes
            .iter()
            .map(|node| self.row(node, busy.get(&node.id).copied().unwrap_or(0)))
            .collect()
    }

    /// One row, with the parent's busy-child count supplied: [`Self::rows`] has
    /// already built them all in one walk, so this does not ask the tree for
    /// them (finding R29).
    fn row(&self, node: &AgentNode, waiting: usize) -> AgentRow {
        // Two facts, two marks: `glyph · id` is this agent's own phase, and
        // `⏸N` counts the children that are working. The old row derived the
        // glyph from "has live children", so a busy agent wore `⏸` and its own
        // work vanished from the screen (finding U1).
        let mut place = node.branch.clone().unwrap_or_default();
        if let Some(stat) = self.tree.agent_stats.get(&node.id) {
            if !stat.is_empty() {
                if !place.is_empty() {
                    place.push(' ');
                }
                place.push_str(&stat.compact());
            }
        }
        // The jobs on this machine, on the row of whoever started them. It
        // rides with the branch and the stat — facts that exist nowhere else on
        // the screen — because the human should not have to ask a model what is
        // running; the count is derived from the registry every frame, never
        // stored, and the selected row's footer names each job.
        let jobs = self.live_jobs(node.id).len();
        if jobs > 0 {
            if !place.is_empty() {
                place.push(' ');
            }
            place.push_str(&format!("⚙{jobs}"));
        }
        // Two more facts, both marks rather than text, because a row is a
        // glance: `✉` on a result its parent has not read, and `✉N` for how many
        // of this agent's own children's results *it* has not read. One fact
        // read from either end — the child's mark says which result, the
        // parent's count says who is owed a read (finding H4). The value is
        // derived here, from the tree, because it is a fact about the agent and
        // not a decision about the frame: the painter only fits it into the
        // columns it has.
        AgentRow {
            id: node.id,
            depth: node.depth,
            glyph: phase_glyph(&node.phase),
            focused: self.tree.focused == node.id,
            waiting,
            result_unread: node.result_unread,
            unread_children: self.tree.unread_children(node.id).len(),
            title: node.title(),
            place,
            activity: phase_detail(node),
        }
    }

    /// The rows the message box asks the layout for: the draft's lines, capped
    /// so the pane keeps the screen, plus the attachment rows and the box's two
    /// border rows. The box grows with the message — a multi-line draft has to
    /// be visible, not hidden behind a one-line window — and with the
    /// attachments, which are painted above the text: a row the box does not
    /// have is a row the message being typed is pushed out of.
    ///
    /// This is the *ask*. [`content_rows`] owns what the box paints when the
    /// layout grants fewer rows than this — a short column at a small terminal —
    /// and there the text's row comes first: the attachment rows are capped to
    /// the room that is really there, so the draft on the cursor's line is
    /// never the part that is left out (finding D5). The two derivations agree
    /// whenever the ask is granted.
    ///
    /// One derivation, because the split is the one place the box's rows come
    /// from: the zen views hand the box exactly the rows this gives it, so the
    /// box does not move when the tree takes the screen.
    pub(super) fn input_rows(&self) -> u16 {
        let input_lines = (self.chat.input().line_count() as u16).clamp(1, MAX_INPUT_LINES);
        let attachment_count = self.chat.attachments().len().min(MAX_ATTACHMENT_ROWS) as u16;
        input_lines + attachment_count + 2
    }

    /// The chat pane when zen hides its transcript: the message box alone, in
    /// `area`, with a transcript area of no rows — so `ChatPane::transcript` is
    /// `None` and the painter has no transcript to draw. The rows are the
    /// caller's: the zen layout hands the box the very rect the two-pane split
    /// gave it.
    ///
    /// The chat's own title goes with the transcript (`+N more lines`,
    /// `scrolled ↑N rows`): it is arithmetic about rows this view does not
    /// paint, which is the same rule that keeps the agents pane's `▲N`/`▼N`
    /// counts off the conversation pane's title when the *agents* pane is the
    /// hidden one.
    fn chat_box(&self, area: Rect) -> ChatPane {
        self.chat_pane_with(Rect { height: 0, ..area }, area)
    }

    /// What the chat column paints, from the two rects it is made of: the
    /// transcript at `transcript_area` and the box at `input_area`. `App::screen`
    /// splits a column into the two; [`Self::chat_box`] hands over the box
    /// alone.
    fn chat_pane_with(&self, transcript_area: Rect, input_area: Rect) -> ChatPane {
        let label = self.cfg().label();
        let room = inner(transcript_area);
        // One lookup of the focused node for the pane's activity line: what the
        // run is doing is a projection of the same node the row beside the pane
        // is built from, and asking the tree twice is one thing more to keep in
        // step (finding R9).
        let node = self.tree.node(self.tree.focused);
        let transcript = (room.height > 0 && room.width > 0).then(|| {
            // A 200-column transcript is not read, it is skimmed. Cap the
            // measure and leave the rest as margin.
            let width = (room.width as usize).min(MAX_TRANSCRIPT as usize);
            let pane = Pane {
                agent: self.tree.focused,
                // The run's own words, the same derivation the row paints
                // (`phase_detail`): the foot can no longer spell a phase
                // differently from the row, and a run parked in a `wait` gets a
                // line naming what it waits on instead of the old silence
                // (finding U7, superseded).
                words: node.and_then(|node| node.phase.words()),
                // The beat the foot counts its dots from is the one `App::tick`
                // advanced.
                spin: self.spin,
                label: &label,
            };
            // Only the rows the window can show are built — the whole
            // scrollback to display forty lines cost 55 ms a frame on a long
            // session, and `tick` repaints every frame while an agent works.
            let mut painted = self.chat.painted(&pane, width, room.height as usize);
            // Zen takes the agents pane off the screen, and its title with it:
            // the counts that say who is working move to the one title still
            // painted, the conversation pane's (`zen_title`).
            if self.zen && self.focus == Focus::Chat {
                painted.title = zen_title(
                    &painted.title,
                    &agent_count_cells(self),
                    room.width as usize,
                );
            }
            painted
        });

        let field = inner(input_area);
        let input = (field.height > 0 && field.width > 0).then(|| {
            let prompt = if self.tree.focused == AgentId::ROOT {
                "› ".to_string()
            } else {
                format!("{} › ", self.tree.focused)
            };
            let prompt_width = UnicodeWidthStr::width(prompt.as_str());
            let columns = (field.width as usize).saturating_sub(prompt_width);
            // The box scrolls with the cursor instead of clipping its tail:
            // what the human is editing is always the part on screen.
            // Multi-line drafts are painted line by line, so the cursor's own
            // line is the one kept in view. The text's rows are what the
            // attachments leave, and `content_rows` is where that is decided:
            // it caps the attachment rows to the room the box really has, one
            // row kept for the text, so the split below cannot hand the draft
            // the row the attachments took (finding D5).
            let (attachments, text_rows) = content_rows(field.height, self.chat.attachments());
            let (lines, cursor_row, column) = self.chat.input().view(text_rows, columns);
            InputPane {
                prompt,
                attachments,
                attachment_count: self.chat.attachments().len(),
                lines,
                cursor_row,
                column,
            }
        });

        ChatPane {
            transcript_area,
            input_area,
            transcript,
            input,
        }
    }

    /// The bar: the one line that wins the precedence table, and the facts.
    fn bar_pane(&self, area: Rect) -> BarPane {
        // Priority: a failure first (the Ctrl-Q warning included, so it is never
        // hidden behind work in progress), then what the whole tree is doing
        // that its rows cannot say, then what just happened (fades), then the
        // static hint — which the painter owns, because it advertises keys.
        let tree = self.tree_line();
        BarPane {
            area,
            word: bar_word(self.status_line(), tree.as_deref()),
            // The facts line: where this is, what it is on, how much has moved.
            // Elided from the right, so the repository survives longest and the
            // hints go first.
            facts: (area.height > 1).then(|| facts_line(self, area.width as usize)),
        }
    }

    /// The modal list, windowed to the popup this terminal paints it in.
    fn picker_pane(&self, area: Rect) -> Option<PickerPane> {
        let picker = self.picker.as_ref()?;
        let width = picker_width(area.width);
        // A list too long for the terminal keeps its cursor in the window and
        // never overflows the frame.
        let height = ((picker.items.len() as u16 + 3).min(24)).min(area.height.saturating_sub(2));
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let popup = Rect::new(x, y, width, height);

        let room = inner(popup);
        let visible = room.height.saturating_sub(1) as usize;
        let start = picker.cursor.saturating_sub(visible / 2);
        let mut items = Vec::new();
        for item in picker.items.iter().skip(start).take(visible) {
            let current = match picker.kind {
                // The row being painted, not the row the cursor sits on: read
                // from the cursor, every visible row wore `• ` while the picker
                // opened on the current model and none did after one `j`
                // (Tier 3 §1). And the id is the row's own field, not a parse
                // of its label — a model id containing the separator is still
                // one id (Tier 3 §7).
                PickerKind::Model => item.id.as_deref() == Some(self.cfg().model.as_str()),
                PickerKind::Provider => item.id.as_deref() == Some(self.cfg().provider.name()),
                // Nothing in this list is a choice, so nothing is marked as
                // one.
                PickerKind::Notes | PickerKind::Help => false,
            };
            let label = if current {
                format!("• {}", item.label)
            } else {
                format!("  {}", item.label)
            };
            // A model id comes from the endpoint and a note can carry a
            // failure's own words, so the row is defanged where it is painted
            // rather than where it is stored: an item is *data* — the id itself
            // is sent back to the endpoint when it is picked — and rewriting it
            // would change what is chosen (see `mush_core::text::sanitize`).
            items.push(sanitize(&label));
        }
        Some(PickerPane {
            area: popup,
            title: picker.title(),
            hint: picker.hint(),
            items,
            cursor: picker.cursor.saturating_sub(start),
        })
    }
}

/// What the bar's first line says. Pure so the priority is testable without a
/// frame: an error must never lose to work in progress (finding B12), and a quit
/// warning ranks with a failure for the same reason (finding H9) — the human who
/// pressed `Ctrl-Q` must not see the derived activity line instead of the names
/// of what their second press kills, or the arm becomes a silent one. A new
/// chat's warning ranks there too, and for the same reason: the second `Ctrl-N`
/// must be as visible as the first (finding C4). The order itself is
/// `chat::Rank`, the one table; this only picks the winner, and the painter maps
/// its rank to a colour.
///
/// The focused agent's activity is deliberately not a candidate here. It has two
/// homes already — the row's own tail, with its age, and the transcript's `⚙`
/// line — and a bar that repeated it spent its only row on the same sentence a
/// third time (finding U5). What the bar says instead is what no row and no
/// transcript can: the newest *event* (a failure, a stop, a job's report, a
/// command's answer, a fold the human is waiting on) or the one derived state
/// the rows only imply (`tree_line`'s napping root).
fn bar_word(status: Option<(&str, StatusKind)>, tree: Option<&str>) -> Option<(Rank, String)> {
    let alert = status
        .filter(|(_, kind)| {
            matches!(
                *kind,
                StatusKind::Error | StatusKind::Quit | StatusKind::NewChat
            )
        })
        .map(|(text, _)| text);
    let said = status
        .filter(|(_, kind)| *kind == StatusKind::Info)
        .map(|(text, _)| text);
    Rank::last_word(alert, tree, said).map(|(rank, text)| (rank, text.to_string()))
}

/// `⌂ ~/p/demo │ master ±3 +12 −3 │ qwen2.5-coder · ctx 3.1k/6.4k (fold 5.8k) ~8k
/// │ /help` — the stable facts, in the order that matters, cut from the right
/// when the terminal is narrow.
fn facts_line(app: &App, width: usize) -> String {
    let root = app.ws.root_str();
    let home = std::env::var("HOME").unwrap_or_default();
    // `~` only stands for the home *directory*: `/home/ru` must not elide
    // `/home/ruben/x` into `~ben/x` (finding B18).
    let shown = if !home.is_empty() && root == home {
        "~".to_string()
    } else if let Some(rest) = root
        .strip_prefix(&home)
        .filter(|rest| rest.starts_with('/'))
    {
        format!("~{rest}")
    } else {
        root
    };
    let mut cells = vec![format!(" ⌂ {shown}")];
    if let Some(git) = &app.git {
        cells.push(git_cell(git, app.git_age()));
    }
    cells.push(format!("{} · {}", app.cfg().label(), app.context_meter()));
    // The workspace cell is never given up: it is the one fact that says which
    // tree the screen is about, so it is this line's floor.
    elide(&cells, " │ ", "", &cells[0], width)
}

/// Drop cells from the right until the line fits: one rule for every line that
/// is built this way — the two panes' titles and the facts under them — and the
/// one home of it (finding D9).
///
/// A cell goes whole, because a clause cut mid-number (`Σ +324 −`, `2 waitin`)
/// is a count that is not the count. The first cell is never given up, and
/// `floor` is what is painted when even it does not fit: the pane keeps its own
/// name, and the facts line keeps the `⌂` cell that says which tree the screen
/// is about. `prefix` opens every kept line, so the separator *inside* the line
/// (` · `, ` │ `) and the one joining it to what precedes are each said once.
///
/// The conversation pane's title is built by the same rule rather than painted
/// as it stands: `Chat::painted` hands its clauses here, so the chat's title
/// cannot be the one title on screen a painter cuts mid-word at the border
/// (finding D11).
pub(super) fn elide(
    cells: &[String],
    separator: &str,
    prefix: &str,
    floor: &str,
    width: usize,
) -> String {
    for kept in (1..=cells.len()).rev() {
        let line = format!("{prefix}{}", cells[..kept].join(separator));
        if UnicodeWidthStr::width(line.as_str()) <= width {
            return line;
        }
    }
    floor.to_string()
}

/// The conversation pane's title while zen takes the agents pane away: the
/// chat's own title with the agents' count clauses appended, dropped whole
/// rather than cut.
///
/// The surface is the conversation pane's title because it is the one title
/// still on screen, and its own facts are of the same kind: `+N more lines` and
/// `scrolled ↑N rows` are clauses about a pane with no row to spend on them, so
/// a count of who is working reads as one more of those. The bar could not
/// carry it — its single line has a precedence table of its own, where a
/// failure, a quit warning and the tree's derived line already win, and a count
/// that must not vanish cannot be made to compete with a failure — and the
/// message box's title is the draft's, not the tree's.
///
/// The clauses are [`agent_count_cells`]'s, the same strings the agents pane's
/// own title is built from, so the two titles cannot count the tree
/// differently. `▲N`/`▼N` keep their distance: they are arithmetic about the
/// list this view does not paint, and a count of hidden rows in a pane with no
/// rows is not a fact about anything on screen.
///
/// `width` is the columns the pane really has — its inner width, the budget its
/// own title is elided to — so the appended clauses never reach under the
/// right border, and one that does not fit is dropped with every clause after
/// it: the clauses are ranked, and a gap in the middle would read as a count
/// that is not the count. `title` is the chat's own title, space-terminated by
/// construction (`chat::Chat::painted`), so the first appended clause reads as
/// one more of its own.
fn zen_title(title: &str, counts: &[String], width: usize) -> String {
    let mut line = title.to_string();
    for cell in counts {
        let candidate = format!("{line}· {cell} ");
        if UnicodeWidthStr::width(candidate.as_str()) > width {
            break;
        }
        line = candidate;
    }
    line
}

/// The branch cell of the facts line: the branch, how many paths are dirty, the
/// uncommitted delta — and, once the read has aged past [`GIT_STALE`], how old
/// it is. A cached read must not read as a live one, so the age rides with the
/// fact it qualifies and is elided with it, never after it (finding P8).
fn git_cell(git: &git::RepoStatus, age: Option<Duration>) -> String {
    let mut cell = if git.branch.is_empty() {
        "detached".to_string()
    } else {
        git.branch.clone()
    };
    if git.dirty > 0 {
        cell.push_str(&format!(" ±{}", git.dirty));
    }
    if !git.stat.is_empty() {
        cell.push_str(&format!(" {}", git.stat.compact()));
    }
    if let Some(age) = age.filter(|age| *age >= GIT_STALE) {
        cell.push_str(&format!(" · {} ago", short_age(age)));
    }
    cell
}

/// ` agents · 3 working · 2 jobs · 2 waiting · Σ +324 −40`: the clauses the
/// agent pane's title is built from, ranked so that the least may be lost last.
///
/// Every clause is a count of the phases, named for what it counts, and no
/// agent is in two of them: `N working` is the agents whose own run is in
/// flight, `M waiting` the ones at rest with children working — a subset of the
/// rows wearing `⏸N`, which a working parent wears too (finding R10) — and the
/// totals are the branches'. It used to say `N running` over a number that
/// included the napping ones, which is how the title came to contradict the
/// rows under it (finding U2).
///
/// The hidden-row counts are first because they exist *only* here: a 4-row pane
/// over nineteen agents used to hide fifteen with nothing on screen saying so
/// (finding P12). `▲`/`▼` names the side the missing rows are on.
///
/// The totals are last because the least is lost last: every branch's own
/// `+add −del` is on its row and in the selected row's footer, while who is
/// working exists only here — which is why the clauses between them are
/// [`agent_count_cells`]'s, and the conversation pane's title reads the very
/// same ones while zen hides this pane. This knows the numbers; [`elide`]
/// spends the columns on them.
fn title_cells(app: &App, above: usize, below: usize) -> Vec<String> {
    let mut cells = Vec::new();
    if above > 0 {
        cells.push(format!("▲{above}"));
    }
    if below > 0 {
        cells.push(format!("▼{below}"));
    }
    cells.extend(agent_count_cells(app));
    let mut added = 0;
    let mut removed = 0;
    for stat in app.tree.agent_stats.values() {
        added += stat.added;
        removed += stat.removed;
    }
    if added + removed > 0 {
        cells.push(format!("Σ +{added} −{removed}"));
    }
    cells
}

/// The clauses that say what the agents themselves are doing: `N working`,
/// `N jobs`, `N waiting`, in that order.
///
/// Split out of [`title_cells`] because two titles read them: the agents pane's
/// own, with the hidden-row counts and the branch totals around them, and the
/// conversation pane's while zen hides that pane ([`zen_title`]). One
/// derivation, so the two titles cannot count the tree differently.
///
/// A working count is the agents whose own run is in flight; a waiting count is
/// the ones at rest with children working; and the job count is the machine's
/// load, the same fact every row wears as `⚙N`, summed over the tree — one
/// agent's share is a row, this is the whole (finding H8). It is jobs and only
/// jobs: a command still under its tool call is not one, so it is on neither.
/// Ranked before `waiting` because a napping agent is already visible on its
/// own row as `⏸N`, while the machine's load exists nowhere else.
fn agent_count_cells(app: &App) -> Vec<String> {
    let roster = app.tree.roster();
    let mut cells = Vec::new();
    if roster.working > 0 {
        cells.push(format!("{} working", roster.working));
    }
    let load = app.tree.live_job_count();
    if load > 0 {
        cells.push(if load == 1 {
            "1 job".to_string()
        } else {
            format!("{load} jobs")
        });
    }
    if roster.waiting > 0 {
        cells.push(format!("{} waiting", roster.waiting));
    }
    cells
}

/// The footer lines a pane of `budget` rows can afford, chosen by what a short
/// pane most needs to say. `agent_footer` builds the identity line (`#2 lexer`)
/// first because it names the row; but a one-line footer keeps the *detail*
/// line instead — where the work is and how to land it — because the selected
/// row already wears its identity in the list, and the detail is the fact that
/// exists nowhere else on a compact screen (finding P12).
fn compact_footer(mut full: Vec<Line<'static>>, budget: usize) -> Vec<Line<'static>> {
    if budget >= 2 || full.len() < 2 {
        full.truncate(budget);
        return full;
    }
    // budget == 1 and there is a detail line: keep it, drop the identity.
    full.split_off(1).into_iter().take(1).collect()
}

/// The footer under the tree: the cursor row's full facts, so a narrow pane
/// still tells the whole story. The row is the one already built for the
/// cursor: its `activity` is the age the list is painting, so the footer and
/// the row cannot show two ages for one frame (finding R26).
fn agent_footer(app: &App, node: &AgentNode, row: &AgentRow, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(format!(" {} ", node.id), Style::default().fg(Color::Cyan)),
        Span::styled(
            truncate(&node.brief, width.saturating_sub(6)),
            Style::default(),
        ),
    ]));
    // The cursor row's facts in full, and always: the row above may have had to
    // give up its activity or its brief to fit, and this is where they are not
    // lost — what the agent is doing *now*, where its work is, and the commands
    // that land it. It used to be painted only for a landed, isolated, idle or
    // stopped agent, so the one row the human is reading was the one whose
    // activity could vanish from the screen entirely (finding P4).
    let mut detail = agent_detail(node);
    let activity = row.activity.clone();
    if !activity.is_empty() {
        detail.insert(0, activity);
    }
    if !detail.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                " {}",
                truncate(&detail.join(" · "), width.saturating_sub(2))
            ),
            dim(),
        )));
    }
    // The selected row's unread results, in full. A mark is a glance and this is
    // the sentence under it: which results nobody has read yet, and by whom —
    // the question a human arrives at the pane with ("did #2 see #6?"), which
    // one envelope on one row cannot answer for a tree of twelve (finding H4).
    let unread = unread_footer(app, node);
    if !unread.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {}", truncate(&unread, width.saturating_sub(2))),
            dim(),
        )));
    }
    // The selected row's jobs, in full: which command, how long, and whether it
    // is the one holding the machine. Read from the same registry the row's
    // count comes from, so the two can never disagree.
    let jobs = app.job_lines(node.id);
    if !jobs.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                " {} jobs · {}",
                jobs.len(),
                truncate(&jobs.join(" · "), width.saturating_sub(14))
            ),
            dim(),
        )));
    }
    lines
}

/// What the selected row's `✉` marks mean, spelled out: its own result if its
/// parent has not read it, and how many of its children's results it owes a
/// read on.
///
/// Derived from the nodes here rather than stored beside them, so the sentence
/// and the marks are the same fact twice read (finding H4). It lives in this
/// module because it is a derived value like every other one the frame paints;
/// `ui.rs` never sees `App` (refactor B17).
fn unread_footer(app: &App, node: &AgentNode) -> String {
    /// How many ids a list names before it counts the rest: three is what fits a
    /// row of the footer at the pane's narrowest, and the count behind it is
    /// what a human needs next. The same shape the hidden-row counts use.
    const NAMED: usize = 3;

    let named = |ids: &[AgentId]| {
        let head = ids
            .iter()
            .take(NAMED)
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        if ids.len() > NAMED {
            format!("{head} +{}", ids.len() - NAMED)
        } else {
            head
        }
    };

    let mut parts = Vec::new();
    if node.result_unread {
        parts.push(match node.parent {
            Some(parent) => format!("✉ result unread by {parent}"),
            None => "✉ result unread".to_string(),
        });
    }
    let owed = app.tree.unread_children(node.id);
    if !owed.is_empty() {
        parts.push(format!("✉{} unread from {}", owed.len(), named(&owed)));
    }
    parts.join(" · ")
}

/// Where an isolated agent's work is — or where it went. Pure, so the row's
/// promise can be asserted: a landed worktree must not name a `git diff` that
/// can no longer work, and its landing must be spelled in [`Landed::past`]'s
/// own word, which the row's own prose is painted around.
///
/// A worktree the sweep looked at and left is a third answer, and it is said
/// rather than implied: the row still names the worktree and the branch, and the
/// reason they are still there — a human who expected a merged worktree to be
/// reclaimed learns here why it was not (finding H10).
fn agent_detail(node: &AgentNode) -> Vec<String> {
    match node.landed {
        // The landing's own word alone. Which base the work went into is not a
        // fact this row has, and "into HEAD" was false for every nested child
        // whose work landed in its parent's branch.
        Some(Landed::Merged) => vec![Landed::Merged.past().to_string()],
        // The row is the only thing that ever said what happened to a branch
        // the run never committed to: there is no diff to show and no merge to
        // claim, so the row says the one fact that is true of it.
        Some(Landed::NothingCommitted) => vec![Landed::NothingCommitted.past().to_string()],
        Some(Landed::Discarded) => vec!["discarded — its work is gone".to_string()],
        None => {
            let mut detail = match &node.branch {
                // This is the one place on screen that says an isolated agent
                // exists at all, and the git command that reads its work.
                Some(branch) => vec![
                    // The worktree path comes from core like every other one: the
                    // row must name the directory a `git worktree remove` takes.
                    git::worktree_rel(node.id.0),
                    format!("git diff HEAD...{branch}"),
                ],
                None => Vec::new(),
            };
            if let Some(why) = &node.kept {
                detail.push(format!("kept — {why}"));
            }
            detail
        }
    }
}

/// The glyph is derived from the agent's own phase, never stored and never
/// borrowed from the tree: `·` until it does something, `◐` while its own run is
/// in flight, `⊘` while a cancel is in flight and after it lands, `✓` only when
/// a run finished, `✗` when it failed, `⚠` when the run never ended at all,
/// `≡` while its conversation is being folded.
///
/// A cut-off run cannot borrow `⊘`: a stop is the human's doing and the actor is
/// alive to be nudged again, while a cut-off agent's run died where it stood and
/// left nothing committed (finding H2). It cannot borrow `✗` either — nothing
/// the model or the endpoint did failed, and the agent's work is still on disk,
/// untouched and unlanded.
///
/// Waiting on children is a *different fact* from working and is drawn as a
/// different mark (`agent_line`'s `⏸N`), because a parent that is mid-turn with
/// children running is working, not paused — the row that said `⏸` about it was
/// claiming a park that never happened (finding U1). Folding is a different fact
/// again: it is a request of its own, and `◐` for it is what made a compaction
/// look like the run's own model call (finding U11).
///
/// A run parked in a `wait` is the third: `⧗`, because the agent is not
/// computing anything and `◐` for it said *working* about the one thing on
/// screen that was doing nothing. Finding U7 fixed the words (`waiting on
/// results 3s`) and the transcript's foot and stopped one surface short of the
/// glyph, which is the surface a glance reads (finding U14).
fn phase_glyph(phase: &Phase) -> &'static str {
    match phase {
        Phase::Failed(_) => "✗",
        // `⊘` while a cancel is in flight and after it lands: a stopped agent
        // is not a finished one, and must not borrow `✓`.
        Phase::Cancelling | Phase::Stopped => "⊘",
        Phase::CutOff => "⚠",
        Phase::Idle => "·",
        Phase::Done => "✓",
        Phase::Compacting(_) => "≡",
        // Not `◐`: `Phase::waiting` is the one derivation of "this run is
        // parked on somebody else's result", the same one the row's words and
        // the transcript's foot already read.
        Phase::Activity(_) if phase.waiting().is_some() => "⧗",
        Phase::Thinking | Phase::Activity(_) => "◐",
    }
}

/// What the row says the agent is doing, ageing with the phase so a slow model
/// is visible as `thinking 42s` rather than a static word.
///
/// The words are [`Phase::words`], the same derivation the transcript's foot
/// paints: the row is that plus the age, so the two surfaces cannot drift into
/// two spellings of one phase (the drift this function and the foot's own
/// `working` used to be free to have).
///
/// A run parked in a wait says what it is waiting on instead of naming the
/// tool: `wait 3s` reads like a model call in flight, and the human asked for
/// an hourglass for the case where nothing is being computed — a napping
/// orchestrator was the one agent on the screen claiming work it was not doing
/// (finding U7). The noun comes from the same [`Phase::waiting`] the `⧗` glyph
/// and the foot read, so no surface can name a different thing.
///
/// A fold says what it is too, and its own words: a fold the human asked for,
/// one the window triggered, and one parked behind the run in flight are three
/// answers, and `◐ #0 root` for all of them is how a compaction became
/// invisible (finding U11).
fn phase_detail(node: &AgentNode) -> String {
    let age = short_age(node.since.elapsed());
    match &node.phase {
        // A stopped run has no result to show: its last summary belongs to a
        // run that was interrupted, so showing it would claim work that was
        // never delivered. `node.summary` is deliberately not consulted.
        Phase::Stopped => "stopped · re-send to resume".to_string(),
        // No age, deliberately: the moment the run died is the moment mush
        // went away with it, and the only clock left to age it is the next
        // launch's — which would count from the restart, not from the cut-off
        // (finding H2).
        Phase::CutOff => "cut off · nothing committed".to_string(),
        Phase::Failed(error) => error.clone(),
        Phase::Idle | Phase::Done => node.summary.clone().unwrap_or_default(),
        // Everything in flight reads the one derivation the foot paints too:
        // the row is those words and its age. The pattern lists the busy
        // phases explicitly, so a phase added to neither list fails to compile
        // instead of quietly painting nothing.
        phase @ (Phase::Thinking
        | Phase::Activity(_)
        | Phase::Compacting(_)
        | Phase::Cancelling) => {
            let words = phase.words().expect("a phase in flight says what it is");
            format!("{words} {age}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Compacting;

    #[test]
    fn an_error_outranks_the_tree_line() {
        let (rank, text) = bar_word(
            Some(("cannot reach http://127.0.0.1:1", StatusKind::Error)),
            Some("waiting on 1 subagent(s) — the root resumes as they finish"),
        )
        .expect("an error is a word");
        assert_eq!(rank, Rank::Alert);
        assert_eq!(
            text, "cannot reach http://127.0.0.1:1",
            "an error is visible"
        );

        let (rank, text) = bar_word(
            Some(("opened notes.txt", StatusKind::Info)),
            Some("#0 thinking 3s"),
        )
        .expect("derived state is a word");
        assert_eq!(rank, Rank::Activity);
        assert_eq!(
            text, "#0 thinking 3s",
            "derived state beats a fading info line"
        );

        let (rank, text) =
            bar_word(Some(("opened notes.txt", StatusKind::Info)), None).expect("a said line");
        assert_eq!(rank, Rank::Said);
        assert_eq!(text, "opened notes.txt");

        assert!(bar_word(None, None).is_none(), "nothing to say is the hint");
    }

    /// The quit warning is the one line that explains what a second `Ctrl-Q`
    /// kills, so it ranks with a failure: the derived activity line — the work
    /// the human is quitting *from* — must not hide it (finding H9).
    #[test]
    fn the_quit_warning_outranks_the_tree_line() {
        let (rank, text) = bar_word(
            Some(("Ctrl-Q again quits · kills #0 thinking", StatusKind::Quit)),
            Some("#0 thinking 3s"),
        )
        .expect("the warning is a word");
        assert_eq!(rank, Rank::Alert, "a quit warning ranks with a failure");
        assert_eq!(
            text, "Ctrl-Q again quits · kills #0 thinking",
            "and it is what the human reads, not the run it names"
        );
    }

    /// A new chat's warning *is* the arm (finding C4), so it ranks with the
    /// quit's and with a failure: the derived activity line must not hide the
    /// sentence that explains what the second `Ctrl-N` drops.
    #[test]
    fn the_new_chat_warning_outranks_the_tree_line() {
        let warning = "Ctrl-N again clears 12 lines — kept as .mush/session.json.previous";
        let (rank, text) = bar_word(Some((warning, StatusKind::NewChat)), Some("#0 thinking 3s"))
            .expect("the warning is a word");
        assert_eq!(rank, Rank::Alert, "a new chat warning ranks with a failure");
        assert_eq!(
            text, warning,
            "and it is what the human reads, not the run under it"
        );
    }

    /// The `/notes` report is wrapped for the popup's own content width, and
    /// that width is this one function: the popup the terminal size paints,
    /// minus the two border columns, the two the `› ` symbol reserves and the
    /// two the `  ` indent carries. A `/notes` line wider than this is a line
    /// the list has to clip.
    #[test]
    fn the_notes_width_follows_the_popup_it_is_painted_in() {
        // The two ends the tests name: the floor and the ceiling.
        assert_eq!(picker_text_width(40), 34);
        assert_eq!(picker_text_width(200), 74);
        assert!(picker_text_width(60) < picker_text_width(200));

        // Never wider than the popup it is painted in, at any size.
        for terminal in 40..=240u16 {
            assert!(
                picker_text_width(terminal) <= picker_width(terminal) as usize,
                "a report wider than its popup at {terminal}"
            );
        }
    }

    /// A share of the terminal is arithmetic about a `u16`, so it is computed
    /// in one integer type wide enough to hold the product: `terminal_width *
    /// 60` in `u16` overflows above 1092 columns — a debug build panics on the
    /// multiply and a release build wraps, which the clamp then quietly turns
    /// into the popup's floor (finding R72). The sweep crosses that width, and
    /// both shares are asserted to be the natural one, clamped, at every size a
    /// terminal can name.
    #[test]
    fn no_share_of_a_terminal_overflows_its_integer() {
        let natural = |whole: u16, percent: u32| (whole as u32 * percent / 100) as u16;
        for terminal in 40..=2000u16 {
            assert_eq!(
                picker_width(terminal),
                natural(terminal, 60).clamp(PICKER_MIN_WIDTH, PICKER_MAX_WIDTH),
                "the picker's share at {terminal}"
            );
            assert_eq!(
                agents_columns(terminal),
                natural(terminal, 34)
                    .clamp(AGENTS_MIN_COLUMNS, AGENTS_MAX_COLUMNS)
                    .min(terminal.saturating_sub(CHAT_MIN_COLUMNS)),
                "the agents pane's share at {terminal}"
            );
        }
    }

    /// The conversation pane's title while zen hides the agents pane: the
    /// counts are appended to the chat's own title, each clause whole or not at
    /// all, and never past the columns the pane really has. A clause cut
    /// mid-number is a count that is not the count, and a gap in the middle
    /// would read as one too.
    #[test]
    fn a_hidden_panes_counts_are_added_to_the_title_whole_or_not_at_all() {
        let counts = [
            "1 working".to_string(),
            "2 jobs".to_string(),
            "1 waiting".to_string(),
        ];
        let all = " mush · 1 working · 2 jobs · 1 waiting ";
        // Wide enough for every clause: they arrive in the agents pane's own
        // order, one more clause of the chat's title.
        assert_eq!(zen_title(" mush ", &counts, all.chars().count()), all);

        // One column short of the last clause: it is dropped whole, and with it
        // the clause after it — never `· 1 waitin`.
        assert_eq!(
            zen_title(" mush ", &counts, all.chars().count() - 1),
            " mush · 1 working · 2 jobs "
        );

        // Room for nothing but the chat's own title: the title stands alone
        // rather than taking a count under the border.
        assert_eq!(zen_title(" mush ", &counts, 6), " mush ");
        assert_eq!(zen_title(" mush ", &[], 6), " mush ");
    }

    /// A row's glyph is the whole status vocabulary in one character; it must
    /// never claim a run that did not happen (`·`, not `✓`), and it is a
    /// function of the agent's *own* phase only — an agent that is working is
    /// `◐` even while its children work, because "waiting on children" is a
    /// fact about the children, drawn as its own mark (finding U1).
    #[test]
    fn glyphs_are_truthful() {
        assert_eq!(phase_glyph(&Phase::Idle), "·");
        assert_eq!(phase_glyph(&Phase::Thinking), "◐");
        assert_eq!(phase_glyph(&Phase::Activity("edit_file a.rs".into())), "◐");
        assert_eq!(phase_glyph(&Phase::Cancelling), "⊘");
        assert_eq!(phase_glyph(&Phase::Done), "✓");
        assert_eq!(phase_glyph(&Phase::Failed("boom".into())), "✗");
        // A stopped agent is not a finished one, and must not borrow the tick.
        assert_eq!(phase_glyph(&Phase::Stopped), "⊘");
        // A fold is not the run's own model call either (finding U11).
        assert_eq!(phase_glyph(&Phase::Compacting(Compacting::Parked)), "≡");
        // Nor is a run parked on somebody else's result a model call: the
        // hourglass for the agent that is doing nothing (finding U14).
        assert_eq!(phase_glyph(&Phase::Activity("wait ".into())), "⧗");
        assert_eq!(phase_glyph(&Phase::Activity("wait c2".into())), "⧗");
    }

    /// Every mark a row carries is one column in the arithmetic the rows are
    /// fitted with (`mush_core::text::fit_row` measures with `unicode-width`):
    /// a glyph the crate calls two columns wide shifts the whole row, and the
    /// hourglass the waiting row wanted is the case in point — `⌛` (U+231B) is
    /// an emoji-presentation character and measures 2, so the hourglass that
    /// fits a row is `⧗` (U+29D7).
    #[test]
    fn every_row_mark_is_one_column() {
        let glyphs = [
            phase_glyph(&Phase::Idle),
            phase_glyph(&Phase::Thinking),
            phase_glyph(&Phase::Activity("edit_file a.rs".into())),
            phase_glyph(&Phase::Activity("wait ".into())),
            phase_glyph(&Phase::Compacting(Compacting::Parked)),
            phase_glyph(&Phase::Cancelling),
            phase_glyph(&Phase::Stopped),
            phase_glyph(&Phase::CutOff),
            phase_glyph(&Phase::Done),
            phase_glyph(&Phase::Failed("boom".into())),
        ];
        // The marks `ui::agent_line` adds beside the glyph, in the same head.
        for mark in glyphs.into_iter().chain(["▶", "⏸", "✉", "⚙", "⚠"]) {
            assert_eq!(
                UnicodeWidthStr::width(mark),
                1,
                "{mark:?} is not one column"
            );
        }
    }

    /// A node carrying nothing but the facts a row test needs.
    fn node(phase: Phase, age: u64) -> AgentNode {
        AgentNode {
            id: AgentId(2),
            parent: None,
            depth: 0,
            title: None,
            brief: "lexer".to_string(),
            phase,
            since: std::time::Instant::now() - std::time::Duration::from_secs(age),
            branch: None,
            fork: None,
            summary: None,
            leftover: false,
            landed: None,
            kept: None,
            result_unread: false,
        }
    }

    #[test]
    fn details_age_with_the_phase() {
        assert_eq!(phase_detail(&node(Phase::Thinking, 3)), "thinking 3s");
        assert_eq!(
            phase_detail(&node(Phase::Activity("edit_file src/a.rs".into()), 75)),
            "edit_file src/a.rs 1m15s"
        );
        assert_eq!(phase_detail(&node(Phase::Cancelling, 1)), "cancelling 1s");
        // A run parked in a `wait` names what it waits on, from the one
        // derivation the foot reads too (finding U7).
        assert_eq!(
            phase_detail(&node(Phase::Activity("wait ".into()), 5)),
            "waiting on results 5s"
        );
        // A label with no words still has one to paint.
        assert_eq!(
            phase_detail(&node(Phase::Activity(String::new()), 2)),
            "working 2s"
        );
        assert_eq!(
            phase_detail(&node(Phase::Failed("no route".into()), 9)),
            "no route"
        );
        assert_eq!(phase_detail(&node(Phase::Idle, 9)), "");
        // A fold's words are the fold's own, and its age is its age (U11).
        assert_eq!(
            phase_detail(&node(Phase::Compacting(Compacting::Parked), 3)),
            "folding at the next step 3s"
        );
        assert_eq!(
            phase_detail(&node(Phase::Compacting(Compacting::NearlyFull), 3)),
            "context nearly full — compacting 3s"
        );

        // A stopped run has no result; showing the interrupted run's summary
        // would claim work that was never delivered.
        let mut stopped = node(Phase::Stopped, 9);
        stopped.summary = Some("wrote half the parser".to_string());
        assert_eq!(phase_detail(&stopped), "stopped · re-send to resume");
        assert!(
            !phase_detail(&stopped).contains("half the parser"),
            "a stop must not show the previous run's summary"
        );
    }

    /// The row and the foot cannot disagree: the row's activity *is* the one
    /// derivation the foot paints ([`Phase::words`]) plus the age. The drift
    /// this exists to prevent is a phase spelled one way on the row and another
    /// under the transcript — `working.` over a `run_command cargo test`, or
    /// silence over a row that names a parked `wait`.
    #[test]
    fn the_row_and_the_foot_read_one_derivation() {
        for phase in [
            Phase::Thinking,
            Phase::Activity("run_command cargo test".to_string()),
            Phase::Activity("wait ".to_string()),
            Phase::Activity(String::new()),
            Phase::Compacting(Compacting::Parked),
            Phase::Compacting(Compacting::Requested),
            Phase::Compacting(Compacting::NearlyFull),
            Phase::Cancelling,
        ] {
            let words = phase
                .words()
                .unwrap_or_else(|| panic!("{phase:?} says what it is doing"));
            assert_eq!(
                phase_detail(&node(phase.clone(), 9)),
                format!("{words} 9s"),
                "{phase:?}: the row is the foot's words and the age"
            );
        }
    }

    /// A landed worktree still has a branch recorded, so the row must key off
    /// `landed` to stop offering a `git diff` that can no longer work — and say
    /// how it landed. The facts, not the sentence: the prose around the word is
    /// the row's to word.
    #[test]
    fn a_landed_agent_does_not_offer_commands_that_cannot_work() {
        for landed in [Landed::Merged, Landed::NothingCommitted, Landed::Discarded] {
            let mut node = node(Phase::Done, 1);
            node.branch = Some("mush/9".to_string());
            node.landed = Some(landed);
            let text = agent_detail(&node).join(" · ");
            assert!(!text.contains("git diff"), "{text}");
            assert!(
                !text.contains("mush/9"),
                "nor a branch nobody can read any more: {text}"
            );
            assert!(
                text.contains(landed.past()),
                "and it says how it landed: {text}"
            );
        }
    }

    /// A landing *is* one word, and the row is where that word is read: this
    /// asserts the coupling — the row contains `Landed::past()` — so a row that
    /// re-spells the landing (`landed in HEAD`, typed by hand) fails here, where
    /// asserting the constant `Landed::Merged.past() == "merged"` could not
    /// fail however the row was worded (refactor R12).
    #[test]
    fn a_landed_row_spells_the_landing_in_the_landings_own_word() {
        for landed in [Landed::Merged, Landed::NothingCommitted, Landed::Discarded] {
            let mut node = node(Phase::Done, 1);
            node.branch = Some("mush/9".to_string());
            node.landed = Some(landed);
            let text = agent_detail(&node).join(" · ");
            assert!(
                text.contains(landed.past()),
                "the row says where the work went, in the landing's own word: {text:?}"
            );
        }
    }

    /// Before anything lands, the row is the one place that says where an
    /// isolated agent's work is and the git command that reads it.
    #[test]
    fn an_unmerged_agent_names_its_worktree_and_the_git_command_to_read_it() {
        let mut open = node(Phase::Done, 1);
        open.branch = Some("mush/9".to_string());
        let text = agent_detail(&open).join(" · ");
        // The worktree is keyed by the *id* (`.mush/wt/<id>`), which need not
        // match the number in the branch name.
        assert_eq!(
            text,
            format!("{} · git diff HEAD...mush/9", git::worktree_rel(open.id.0))
        );
    }

    /// A worktree the sweep left alone says *why* it is still there, and keeps
    /// saying where it is: the reason is a refusal to reclaim, not a landing,
    /// so the `git diff` a human lands the work with must still be on the row
    /// (finding H10).
    #[test]
    fn a_kept_worktree_says_why_it_is_still_there() {
        let mut open = node(Phase::Done, 1);
        open.branch = Some("mush/9".to_string());
        open.kept = Some("mush/9 has 2 commits nobody merged into HEAD".to_string());

        let text = agent_detail(&open).join(" · ");
        assert!(text.contains("kept — mush/9 has 2 commits"), "{text:?}");
        assert!(text.contains("git diff HEAD...mush/9"), "{text:?}");

        // A landed node has no worktree to explain, and its landing is the
        // whole of what the row says about where the work went.
        open.landed = Some(Landed::Merged);
        let text = agent_detail(&open).join(" · ");
        assert_eq!(text, "merged");
    }

    /// A merge by hand lands in whatever base the branch was forked from, which
    /// is not necessarily HEAD: a nested child's branch goes into its parent's
    /// branch, so a row that says `into HEAD` names a ref the work was never in
    /// (the defect this patch is about). The row's word is the one git proved
    /// and no more.
    #[test]
    fn a_merged_row_does_not_claim_head() {
        let mut node = node(Phase::Done, 1);
        node.branch = Some("mush/9".to_string());
        node.landed = Some(Landed::Merged);
        assert_eq!(agent_detail(&node).join(" · "), "merged");
    }

    /// The third landing has its own row, because it is the only surface that
    /// ever said what happened to a branch the run never committed to: there is
    /// no diff to offer and no merge to claim.
    #[test]
    fn a_run_that_committed_nothing_says_so_on_its_row() {
        let mut node = node(Phase::Done, 1);
        node.branch = Some("mush/9".to_string());
        node.landed = Some(Landed::NothingCommitted);
        assert_eq!(agent_detail(&node).join(" · "), "nothing committed");
    }

    /// The floor notice must never name a size the program does not need: the
    /// fixed 25-column string was truncated at 24 columns to `40×1` (finding
    /// P11). Every spelling that fits is tried longest-first, and the one that
    /// fits is the one painted.
    #[test]
    fn the_floor_notice_fits_the_terminal_it_is_painted_in() {
        // The full sentence at its own width and wider.
        assert_eq!(floor_notice(80), "mush needs at least 40×10");
        assert_eq!(floor_notice(25), "mush needs at least 40×10");
        // Narrower than the sentence: a shorter honest spelling.
        assert_eq!(floor_notice(24), "needs at least 40×10");
        assert_eq!(floor_notice(19), "40×10 minimum");
        assert_eq!(floor_notice(11), "needs 40×10");
        assert_eq!(floor_notice(5), "40×10");
        // And it never lies about the size, at any narrow width.
        for width in 0..=25u16 {
            assert!(
                floor_notice(width).contains(&format!("{MIN_WIDTH}×{MIN_HEIGHT}")),
                "the notice at {width} must name the real floor"
            );
        }
    }

    /// The facts line's git cell says how old a read is once it has aged past
    /// the point an event would have refreshed it (finding P8), and stays plain
    /// while the read is fresh.
    #[test]
    fn a_stale_git_read_is_labelled_in_the_facts_cell() {
        let git = git::RepoStatus {
            branch: "main".to_string(),
            dirty: 2,
            stat: git::Stat {
                files: 2,
                added: 5,
                removed: 1,
            },
        };
        assert_eq!(git_cell(&git, Some(Duration::from_secs(1))), "main ±2 +5−1");
        assert_eq!(git_cell(&git, None), "main ±2 +5−1");
        assert_eq!(
            git_cell(&git, Some(Duration::from_secs(30))),
            "main ±2 +5−1 · 30s ago"
        );
        // An empty branch is a detached HEAD, not a blank cell.
        let detached = git::RepoStatus::default();
        assert_eq!(git_cell(&detached, None), "detached");
    }

    /// The `/model` bullet marks the row being painted, not the row the cursor
    /// happens to sit on: reading `items[picker.cursor]` made `current`
    /// constant for the whole visible window — every row wore `• ` while the
    /// picker opened on the current model, and none did after one `j` (Tier 3
    /// §1). The mark is derived here, so this paints a frame and counts the rows
    /// that carry it.
    #[test]
    fn the_model_picker_marks_the_current_model_and_only_it() {
        use crate::agent::spawn;
        use crate::app::ConfigCell;
        use crate::session_save;
        use crate::Msg;
        use crossbeam_channel::unbounded;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let root = std::env::temp_dir().join(format!("mush-bullet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let ws = mush_core::Workspace::new(&root).unwrap();
        let cell = ConfigCell::own(mush_core::Config::new(
            "http://127.0.0.1:1",
            "test-model",
            None,
        ));
        let (tx, _rx) = unbounded::<Msg>();
        let handle = spawn(cell.handle(), tx.clone(), root.clone());
        let mut app = App::new(
            ws,
            cell,
            None,
            handle,
            tx,
            session_save::fake::Recorder::new(),
        );
        app.models = vec![
            crate::http::Model {
                id: "test-model".to_string(),
                context: Some(500_000),
            },
            crate::http::Model {
                id: "deepseek-chat".to_string(),
                context: Some(128_000),
            },
        ];
        app.open_model_picker();
        // One `j` off the current model: it is still the model that is marked,
        // and the row the cursor moved to is not.
        app.move_picker(1);

        let (width, height) = (80u16, 24u16);
        app.set_term_size(width, height);
        let screen = app.screen(Rect::new(0, 0, width, height));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &screen, &crate::theme::Theme::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let marked: Vec<String> = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .filter(|row| row.contains('•'))
            .collect();
        assert_eq!(marked.len(), 1, "exactly one row is marked: {marked:?}");
        assert!(
            marked[0].contains("test-model"),
            "and it is the current model: {marked:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
