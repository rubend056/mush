//! The pointer: what a click and a wheel notch mean.
//!
//! mush used to take no mouse mode at all (finding K3): the terminal owned
//! selection, and the price of a wheel notch was not worth the drag. The human
//! asked for clicks — a row selects a conversation, a tool call opens on its
//! own — so the mode is taken now (`main`'s `take_mouse`) and this module is
//! what the events mean. The trade is not hidden: the terminal's own
//! drag-to-select is taken with its bypass key (Shift+drag in most terminals),
//! `Ctrl-F` is still the road to a rectangle of one pane, and the wheel — which
//! the terminal can no longer spend itself — is spent here.
//!
//! Nothing here has a verb of its own. Every arm moves what the keyboard also
//! moves — a row's cursor, the focused pane, a picker's row — so a mouse that
//! is never touched costs this program nothing but the modes, and a mouse that
//! is used cannot reach a state the keys could not.

use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Position;

use super::screen::{inner, Panes, Screen};
use super::{AgentId, App, Focus};

/// How many rows one wheel notch moves. The terminal's own scrollback step is
/// not on offer any more (the notch is mush's now), and a step has to be
/// chosen: one row crawls through a long session while a page loses the place,
/// so a notch is three — a round number, and the step pagers and terminals have
/// spent on a notch for as long as anyone has counted. The keys stay exact:
/// `↑`/`↓` move one row and `PgUp`/`PgDn` a page.
const WHEEL_ROWS: i64 = 3;

impl App {
    /// Handle one mouse event: the click, and the wheel notch the terminal can
    /// no longer spend itself.
    ///
    /// The frame is asked for the size `main` last reported (`Self::frame_area`)
    /// and hit-tested through the panes that derivation publishes, so a click
    /// resolves against the frame the human clicked on and not against a second
    /// layout of the same state.
    pub(super) fn on_mouse(&mut self, event: MouseEvent) {
        // Below the floor the screen is one notice and the panes are not
        // derived at all: a click has nothing to land on and a notch nothing to
        // scroll. The keyboard's own predicate, so the two inputs agree about
        // when the screen is unreadable (finding P11 / refactor B3).
        if self.below_floor() {
            return;
        }
        // A modified click or notch is not mush's. Shift is the terminal's own
        // selection bypass — which is where drag-to-select lives now — and most
        // terminals keep the event to themselves while it is held, but not all
        // of them do; Ctrl and Alt with a click are each some terminal's own
        // gesture (open a link, resize a pane). No mush verb needs a modifier,
        // so guessing what one means here would only take a verb away from the
        // terminal.
        if !event.modifiers.is_empty() {
            return;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.click(event.column, event.row),
            MouseEventKind::ScrollUp => self.wheel(event.column, event.row, -WHEEL_ROWS),
            MouseEventKind::ScrollDown => self.wheel(event.column, event.row, WHEEL_ROWS),
            // A press of another button, a release, motion, a horizontal notch:
            // named one by one, and none of them a verb. The press is the whole
            // gesture — a release is not a second click, and a press that
            // wandered before it let go is still the press (the mode set takes
            // no motion: `main`'s `take_mouse`) — and nothing on this screen has
            // a context menu or a middle-click paste (the terminal's paste is a
            // key, `Ctrl-V`). A `Drag` or `Moved` arriving anyway is a terminal
            // sending motion mush did not ask for: ignored, never acted on.
            MouseEventKind::Down(MouseButton::Right)
            | MouseEventKind::Down(MouseButton::Middle)
            | MouseEventKind::Up(MouseButton::Left)
            | MouseEventKind::Up(MouseButton::Right)
            | MouseEventKind::Up(MouseButton::Middle)
            | MouseEventKind::Drag(MouseButton::Left)
            | MouseEventKind::Drag(MouseButton::Right)
            | MouseEventKind::Drag(MouseButton::Middle)
            | MouseEventKind::Moved
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {}
        }
    }

    /// What a left click does, by where it lands.
    ///
    /// A click in a pane puts the keyboard in it, the way clicking a window
    /// does, and then moves what was under the pointer: an agent's row selects
    /// that conversation, a picker's row walks the picker's cursor. The bar, a
    /// border, a pane's footer and the popup's own blank cells carry no verb,
    /// so a click there does nothing at all — not even a focus change, because
    /// there is nothing under it the click could be about.
    fn click(&mut self, column: u16, row: u16) {
        let Some(panes) = self.panes() else {
            return;
        };
        match hit(&panes, column, row) {
            // The row the human pointed at becomes the tree's cursor and the
            // chat pane's agent, through the same two doors `Enter` on that row
            // uses ([`Self::focus_cursor_row`]): the cursor moves through
            // `AgentTree::point_cursor_at`, which walks the *painted* rows, so
            // the row the click named and the row `j`/`k` carry on from are the
            // same row, and the bar says the row's brief exactly as the key
            // does.
            Hit::Agent(id) => {
                self.focus = Focus::Agents;
                self.tree.point_cursor_at(id);
                self.focus_cursor_row();
            }
            // The pane under the keyboard, as `Tab` moves it. The box's own
            // text cursor is not moved: a click would have to name a *column*
            // the box's grapheme walk agrees with, which is an editor's job and
            // not a click's, and the pane's own keys already put the cursor
            // where the human wants it.
            Hit::Transcript | Hit::Input => self.focus = Focus::Chat,
            // A picker's row: the cursor moves to the row that was clicked, the
            // move `↑`/`↓` make. It does not *pick*: picking a model writes the
            // session and picking a provider writes the home config, and a
            // single click is not the decision `Enter` is.
            Hit::Picker(at) => self.set_picker_cursor(at),
            Hit::Nothing => {}
        }
    }

    /// What a wheel notch does: the pane under the pointer moves `rows`, or the
    /// picker's list while one is up.
    ///
    /// `rows` is positive *down* the wheel — toward the newest line, the way a
    /// notch away from the human points — and each pane's own door is handed
    /// its own sign: [`Chat::scroll_by`] counts *older* positive, while
    /// [`Self::move_picker`] and [`Self::move_tree_cursor`] count down the list
    /// positive. One physical direction, three conventions, and the flip is
    /// written once, here, where the notch is read.
    ///
    /// The picker is modal, so a notch anywhere moves its cursor: the popup is
    /// the only thing on screen answering input while it is up, and scrolling
    /// rows it covers would be a scroll nobody can see. Its clicks are the same
    /// rule, in `hit`.
    fn wheel(&mut self, column: u16, row: u16, rows: i64) {
        if self.picker.is_some() {
            self.move_picker(rows);
            return;
        }
        let Some(panes) = self.panes() else {
            return;
        };
        match hit(&panes, column, row) {
            // The transcript the pane shows, by the door the arrow keys use.
            // The message box is the same pane — it has no scrollback of its
            // own, so a notch over it moves the conversation it sits under.
            Hit::Transcript | Hit::Input => self.chat.scroll_by(self.tree.focused, -rows),
            // The tree's window *is* its cursor: the rows above and below are
            // what `first` counts and `j`/`k` walk. There is no separate scroll
            // position to move, so a notch over the tree walks the cursor the
            // way a page key does — the one reading of "scroll this pane" that
            // does not invent a second window for the cursor to disagree with.
            Hit::Agent(_) => self.move_tree_cursor(rows),
            // A notch over the popup with no picker up is over nothing, and so
            // is one over the bar or a border.
            Hit::Picker(_) | Hit::Nothing => {}
        }
    }

    /// The panes of the last frame, from the one derivation both roads share
    /// ([`Self::screen`] at [`Self::frame_area`]).
    ///
    /// `App::screen` is pure — a function of the state and the frame's rect —
    /// so asking again between two paints is the same answer the last paint was
    /// given, and the answer the coming paint will be given. `None` is a screen
    /// below the floor, which has no panes to hit at all; `on_mouse` refuses it
    /// before asking, and it is spelled here because a hit test with no frame
    /// is not a hit test.
    fn panes(&self) -> Option<Box<Panes>> {
        match self.screen(self.frame_area()) {
            Screen::Panes(panes) => Some(panes),
            Screen::Floor { .. } => None,
        }
    }
}

/// What one point of a frame lands on.
///
/// The one hit test: a click and a wheel notch both resolve through it, so the
/// two cannot disagree about which pane the pointer is over. It reads the rects
/// and the window indices the panes published ([`Panes`]) and never computes a
/// layout of its own — a second derivation of the panes is how a click starts
/// landing on the wrong row.
enum Hit {
    /// A painted row of the agents pane: the agent that row names.
    Agent(AgentId),
    /// The chat pane's transcript.
    Transcript,
    /// The chat pane's message box.
    Input,
    /// A painted row of an open picker: the item's index in `Picker::items`.
    Picker(usize),
    /// The bar, a border, a pane's footer, the popup's hint — cells that carry
    /// no verb.
    Nothing,
}

/// Where `point` lands, as a verb or as nothing.
///
/// The picker is asked first because it is painted last and covers what is
/// under it: while one is up, a click outside it must not reach the panes
/// beneath — it is `Nothing`, not the row it happened to be over.
fn hit(panes: &Panes, column: u16, row: u16) -> Hit {
    let point = Position::new(column, row);
    if let Some(picker) = &panes.picker {
        if !picker.area.contains(point) {
            return Hit::Nothing;
        }
        let list = inner(picker.area);
        // The pane windows its list to the popup's last row *minus one*, which
        // is the hint's row (`App::picker_pane`); the hint is the popup's and
        // not an item, so a click there is not a pick.
        let hint = list.y + list.height.saturating_sub(1);
        if !list.contains(point) || row >= hint {
            return Hit::Nothing;
        }
        let at = (row - list.y) as usize;
        return match picker.items.get(at) {
            // The window's own index plus the offset the pane published: the
            // item the popup painted on this row, in the list's own numbering.
            Some(_) => Hit::Picker(picker.first + at),
            None => Hit::Nothing,
        };
    }
    // The tree: a painted row, and only a painted row. The pane's whole area is
    // not the target — its border is the frame's and its footer is the cursor
    // row's facts, and neither is a verb — so the test is `list_area`, which is
    // the rect the rows were painted in.
    if panes.agents.list_area.contains(point) {
        let at = panes.agents.first + (row - panes.agents.list_area.y) as usize;
        return match panes.agents.rows.get(at) {
            Some(agent) => Hit::Agent(agent.id),
            // A row the window ran out of: `list_area` can be taller than the
            // rows left after `first`, and the cells under them are blank.
            None => Hit::Nothing,
        };
    }
    // The chat's two halves, through the same `inner` the painter's
    // `Block::inner` is: the border between them belongs to neither pane.
    if inner(panes.chat.transcript_area).contains(point) {
        return Hit::Transcript;
    }
    if inner(panes.chat.input_area).contains(point) {
        return Hit::Input;
    }
    Hit::Nothing
}
