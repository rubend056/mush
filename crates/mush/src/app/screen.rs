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
use mush_core::text::{sanitize, truncate};

use crate::ui::dim;

use super::{
    is_below_floor, short_age, AgentId, AgentNode, App, Focus, Landed, Pane, Phase, PickerKind,
    Rank, StatusKind, MIN_HEIGHT, MIN_WIDTH,
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

/// The popup the pickers paint in: a share of the terminal, floored so a model
/// list is readable and capped so it does not sprawl on a wide one. One formula,
/// because the popup's own rect is sized with it and `picker_text_width` says
/// how much of it a `/notes` row may use.
const PICKER_MIN_WIDTH: u16 = 40;
const PICKER_MAX_WIDTH: u16 = 80;

fn picker_width(terminal_width: u16) -> u16 {
    (terminal_width * 60 / 100).clamp(PICKER_MIN_WIDTH, PICKER_MAX_WIDTH)
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
    let share = (terminal_width as u32 * 34 / 100) as u16;
    share
        .clamp(AGENTS_MIN_COLUMNS, AGENTS_MAX_COLUMNS)
        .min(terminal_width.saturating_sub(CHAT_MIN_COLUMNS))
}

/// The rect inside a pane's border — the same arithmetic `Block::inner` does in
/// the painter, computed here because the words a pane paints (its title, its
/// row's fields, the wrapped message box) are laid out for that width.
fn inner(area: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(area)
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
    pub focused: bool,
    /// The title's clauses, ranked so that the ones that exist *only* here come
    /// first: the hidden-row counts (`▲3`, `▼17`), then what the whole tree is
    /// doing (`2 working`, `1 waiting`), then the branches' totals (`Σ +324
    /// −40`). The painter keeps the longest prefix of them that fits the pane,
    /// dropping whole clauses — a clause cut mid-number is a count that is not
    /// the count.
    pub title_cells: Vec<String>,
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
    /// `waiting on agents 3s`, `compacting 2s`.
    pub activity: String,
}

/// The chat column: the transcript and the message box beside it.
pub struct ChatPane {
    /// The transcript's whole rect, border included.
    pub transcript_area: Rect,
    /// The message box's whole rect, border included.
    pub input_area: Rect,
    pub focused: bool,
    /// The transcript as `Chat` rendered it, windowed to the room it has, or
    /// `None` when the pane has no inner room at all — a pane that short paints
    /// its border and nothing else.
    pub transcript: Option<Painted>,
    /// The message box as it is painted, or `None` for the same reason.
    pub input: Option<InputPane>,
}

/// The message box: the prompt, the windowed lines, and where the cursor is in
/// them.
pub struct InputPane {
    pub prompt: String,
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
    pub focus: Focus,
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
    /// Whether the popup is tall enough to paint the hint row at all.
    pub show_hint: bool,
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
        // Two rows from 24 up, so the facts line — the branch, the dirty count
        // and the line delta — is on screen at the ubiquitous 80×24, where it
        // used to need 26 and was simply absent (finding P12). Below 24 the
        // extra row is worth more to the transcript, and the compact footer
        // carries the selected agent's own branch and worktree instead.
        let bar_rows = if area.height >= 24 { 2 } else { 1 };

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

        Screen::Panes(Box::new(Panes {
            agents: self.agents_pane(agents_area),
            chat: self.chat_pane(chat_area),
            bar: self.bar_pane(bar_area),
            picker: self.picker_pane(area),
        }))
    }

    /// The tree pane: the rows in painted order — pre-order over the parent
    /// links, so a child is drawn under its parent rather than after everything
    /// spawned before it (finding U4) — the cursor row's footer, and the title.
    fn agents_pane(&self, area: Rect) -> AgentsPane {
        let focused = self.focus == Focus::Agents;
        let inner = inner(area);
        let nodes = self.tree.rows();
        let cursor = self.tree.cursor();

        // The cursor row's facts live in a footer under the list, so the list
        // may degrade to `◐ #2` on a narrow pane without losing anything: facts
        // move, they do not vanish. A tall pane spends up to three lines on it;
        // a short (compact) pane still owes the selected row one line — under
        // six inner rows it got none, so in compact the selected row's branch,
        // worktree and landing commands were nowhere on screen (finding P12).
        let footer = if inner.width == 0 || inner.height < 3 || nodes.is_empty() {
            Vec::new()
        } else {
            let budget = if inner.height >= 8 { 3 } else { 1 };
            compact_footer(
                agent_footer(self, nodes[cursor], inner.width as usize),
                budget,
            )
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
            focused,
            title_cells: agents_title(self, above, below),
            rows: nodes.iter().map(|node| self.agent_row(node)).collect(),
            cursor,
            footer,
        }
    }

    /// One row of the tree, with its fields derived from the node. `pub(super)`
    /// so the attach roster can serialize the very row the pane paints instead
    /// of deriving it a second time (finding R21).
    pub(super) fn agent_row(&self, node: &AgentNode) -> AgentRow {
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
            waiting: self.tree.busy_children(node.id),
            result_unread: node.result_unread,
            unread_children: self.tree.unread_children(node.id).len(),
            title: node.title(),
            place,
            activity: phase_detail(node),
        }
    }

    /// The chat column: the transcript `Chat` renders and the message box.
    fn chat_pane(&self, area: Rect) -> ChatPane {
        let focused = self.focus == Focus::Chat;
        // The box grows with the message: a multi-line draft has to be visible,
        // not hidden behind a one-line window. It stops growing so the
        // transcript keeps the screen.
        let input_lines = (self.chat.input().line_count() as u16).clamp(1, MAX_INPUT_LINES);
        let rows =
            Layout::vertical([Constraint::Min(3), Constraint::Length(input_lines + 2)]).split(area);
        let transcript_area = rows[0];
        let input_area = rows[1];

        let label = self.cfg().label();
        let room = inner(transcript_area);
        let transcript = (room.height > 0 && room.width > 0).then(|| {
            // A 200-column transcript is not read, it is skimmed. Cap the
            // measure and leave the rest as margin.
            let width = (room.width as usize).min(MAX_TRANSCRIPT as usize);
            let pane = Pane {
                agent: self.tree.focused,
                // A run in flight is what the pane's own activity line is
                // derived from, and the spinner is the frame `App::tick`
                // advanced.
                //
                // A run parked in a wait is *not* one: the foot's `working…` may
                // only claim a model call, and `wait_agents` is not one — the
                // agent is waiting for somebody else's result, and the row says
                // so (`waiting on agents 3s`). Painting the spinner over that
                // was exactly the lie finding U7 named.
                busy: self
                    .tree
                    .node(self.tree.focused)
                    .map(|node| node.phase.is_busy() && node.phase.waiting().is_none())
                    .unwrap_or(false),
                compacting: self
                    .tree
                    .node(self.tree.focused)
                    .and_then(|node| node.phase.compacting()),
                spin: self.spin,
                label: &label,
            };
            // Only the rows the window can show are built — the whole
            // scrollback to display forty lines cost 55 ms a frame on a long
            // session, and `tick` repaints every frame while an agent works.
            self.chat.painted(&pane, width, room.height as usize)
        });

        let field = inner(input_area);
        let input = (field.height > 0 && field.width > 0).then(|| {
            let prompt = if self.tree.focused == AgentId::ROOT {
                "› ".to_string()
            } else {
                format!("#{} › ", self.tree.focused)
            };
            let prompt_width = UnicodeWidthStr::width(prompt.as_str());
            let columns = (field.width as usize).saturating_sub(prompt_width);
            // The box scrolls with the cursor instead of clipping its tail:
            // what the human is editing is always the part on screen.
            // Multi-line drafts are painted line by line, so the cursor's own
            // line is the one kept in view.
            let (lines, cursor_row, column) =
                self.chat.input().view(field.height as usize, columns);
            InputPane {
                prompt,
                lines,
                cursor_row,
                column,
            }
        });

        ChatPane {
            transcript_area,
            input_area,
            focused,
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
            focus: self.focus,
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

        // A terminal this short has no room for a list; the hint line and the
        // window below both need at least one row.
        let room = inner(popup);
        if room.height == 0 || room.width == 0 {
            return Some(PickerPane {
                area: popup,
                title: picker.title(),
                hint: picker.hint(),
                items: Vec::new(),
                cursor: 0,
                show_hint: false,
            });
        }

        let visible = room.height.saturating_sub(1) as usize;
        let start = picker.cursor.saturating_sub(visible / 2);
        let mut items = Vec::new();
        for item in picker.items.iter().skip(start).take(visible) {
            let current = match picker.kind {
                PickerKind::Model => picker
                    .items
                    .get(picker.cursor)
                    .map(|item| item.split(" · ").next().unwrap_or(item) == self.cfg().model)
                    .unwrap_or(false),
                PickerKind::Provider => item == self.cfg().provider.name(),
                // Nothing in this list is a choice, so nothing is marked as
                // one.
                PickerKind::Notes => false,
            };
            let label = if current {
                format!("• {item}")
            } else {
                format!("  {item}")
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
            show_hint: true,
        })
    }
}

/// What the bar's first line says. Pure so the priority is testable without a
/// frame: an error must never lose to work in progress (finding B12), and a quit
/// warning ranks with a failure for the same reason (finding H9) — the human who
/// pressed `Ctrl-Q` must not see the derived activity line instead of the names
/// of what their second press kills, or the arm becomes a silent one. The order
/// itself is `chat::Rank`, the one table; this only picks the winner, and the
/// painter maps its rank to a colour.
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
        .filter(|(_, kind)| matches!(*kind, StatusKind::Error | StatusKind::Quit))
        .map(|(text, _)| text);
    let said = status
        .filter(|(_, kind)| *kind == StatusKind::Info)
        .map(|(text, _)| text);
    Rank::last_word(alert, tree, said).map(|(rank, text)| (rank, text.to_string()))
}

/// `⌂ ~/p/demo │ master ±3 +12 −3 │ qwen2.5-coder · ctx ~500k │ /help` — the
/// stable facts, in the order that matters, cut from the right when the
/// terminal is narrow.
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
    while cells.len() > 1 {
        let joined: String = cells.join(" │ ");
        if UnicodeWidthStr::width(joined.as_str()) <= width {
            break;
        }
        cells.pop();
    }
    cells.join(" │ ")
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

/// ` agents · 3 working · 2 waiting · Σ +324 −40`: the clauses the agent pane's
/// title is built from, ranked so that the least may be lost last.
///
/// Every clause is a count of the phases, named for what it counts, and no
/// agent is in two of them: `N working` is the agents whose own run is in
/// flight, `M waiting` the ones at rest with children working (the `⏸` rows),
/// and the totals are the branches'. It used to say `N running` over a number
/// that included the napping ones, which is how the title came to contradict
/// the rows under it (finding U2).
///
/// The hidden-row counts are first because they exist *only* here: a 4-row pane
/// over nineteen agents used to hide fifteen with nothing on screen saying so
/// (finding P12). `▲`/`▼` names the side the missing rows are on.
///
/// The totals are last because the least is lost last: every branch's own
/// `+add −del` is on its row and in the selected row's footer, while who is
/// working exists only here. The painter drops clauses from the right until the
/// title fits — it knows the columns, this knows the numbers.
fn agents_title(app: &App, above: usize, below: usize) -> Vec<String> {
    let roster = app.tree.roster();
    let mut cells = Vec::new();
    if above > 0 {
        cells.push(format!("▲{above}"));
    }
    if below > 0 {
        cells.push(format!("▼{below}"));
    }
    if roster.working > 0 {
        cells.push(format!("{} working", roster.working));
    }
    if roster.waiting > 0 {
        cells.push(format!("{} waiting", roster.waiting));
    }
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
/// still tells the whole story.
fn agent_footer(app: &App, node: &AgentNode, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(format!(" #{} ", node.id), Style::default().fg(Color::Cyan)),
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
    let activity = phase_detail(node);
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
            .map(|id| format!("#{id}"))
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
            Some(parent) => format!("✉ result unread by #{parent}"),
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
/// promise can be asserted: a landed worktree must not name a `git diff` or a
/// `/merge` that can no longer work.
fn agent_detail(node: &AgentNode) -> Vec<String> {
    match node.landed {
        Some(Landed::Merged) => vec!["merged into HEAD".to_string()],
        Some(Landed::Discarded) => vec!["discarded — its work is gone".to_string()],
        None => match &node.branch {
            // This is the one place on screen that says an isolated agent
            // exists at all, and the commands that land it.
            Some(branch) => vec![
                // The worktree path comes from core like every other one: the
                // row must name the directory `/discard` would remove.
                format!("{}/{}", git::WORKTREE_DIR, node.id),
                format!("git diff HEAD...{branch}"),
                format!("/merge {}", node.id),
            ],
            None => Vec::new(),
        },
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
        Phase::Thinking | Phase::Activity(_) => "◐",
    }
}

/// What the row says the agent is doing, ageing with the phase so a slow model
/// is visible as `thinking 42s` rather than a static word.
///
/// A run parked in a wait says so instead of naming the tool: `wait_agents 3s`
/// reads like a model call in flight, and the human asked for an hourglass for
/// the case where nothing is being computed — a napping orchestrator was the
/// one agent on the screen claiming work it was not doing (finding U7).
///
/// A fold says what it is too, and its own words: a fold the human asked for,
/// one the window triggered, and one parked behind the run in flight are three
/// answers, and `◐ #0 root` for all of them is how a compaction became
/// invisible (finding U11).
fn phase_detail(node: &AgentNode) -> String {
    let age = short_age(node.since.elapsed());
    match &node.phase {
        Phase::Thinking => format!("thinking {age}"),
        Phase::Activity(what) => match node.phase.waiting() {
            Some(waiting) => format!("waiting on {} {age}", waiting.noun()),
            // The actor's label is the tool name and its summarized arguments;
            // with no arguments it ends in a space, which the row would paint
            // as a double one (`wait_agents  3s`).
            None => format!("{} {age}", what.trim_end()),
        },
        Phase::Compacting(kind) => format!("{} {age}", kind.words().trim_end_matches('…')),
        Phase::Cancelling => "cancelling…".to_string(),
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
    }

    /// A node carrying nothing but the facts a row test needs.
    fn node(phase: Phase, age: u64) -> AgentNode {
        AgentNode {
            id: AgentId(2),
            parent: None,
            depth: 0,
            brief: "lexer".to_string(),
            phase,
            since: std::time::Instant::now() - std::time::Duration::from_secs(age),
            branch: None,
            summary: None,
            leftover: false,
            landed: None,
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
        assert_eq!(phase_detail(&node(Phase::Cancelling, 1)), "cancelling…");
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

    /// A landed worktree still has a branch recorded, so the row must key off
    /// `landed` to stop offering a diff and a merge that can no longer work.
    #[test]
    fn a_landed_agent_does_not_offer_commands_that_cannot_work() {
        let mut merged = node(Phase::Done, 1);
        merged.branch = Some("mush/9".to_string());
        merged.landed = Some(Landed::Merged);
        let text = agent_detail(&merged).join(" · ");
        assert_eq!(text, "merged into HEAD");
        assert!(!text.contains("git diff"), "{text}");
        assert!(!text.contains("/merge"), "{text}");

        let mut discarded = node(Phase::Done, 1);
        discarded.branch = Some("mush/9".to_string());
        discarded.landed = Some(Landed::Discarded);
        let text = agent_detail(&discarded).join(" · ");
        assert!(text.contains("discarded"), "{text}");
        assert!(!text.contains("/merge"), "{text}");
    }

    /// Before anything lands, the row is the one place that says where an
    /// isolated agent's work is and how to bring it in.
    #[test]
    fn an_unmerged_agent_names_its_worktree_and_the_command_to_merge_it() {
        let mut open = node(Phase::Done, 1);
        open.branch = Some("mush/9".to_string());
        let text = agent_detail(&open).join(" · ");
        // The commands are keyed by the *id* (the worktree is `.mush/wt/<id>`),
        // which need not match the number in the branch name.
        assert_eq!(
            text,
            format!(
                ".mush/wt/{} · git diff HEAD...mush/9 · /merge {}",
                open.id, open.id
            )
        );
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
}
