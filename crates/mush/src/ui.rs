//! Painting. This module is deliberately dumb: it takes a [`Screen`] — a value
//! [`crate::app::App::screen`] derived every word of — and paints it, without reading any
//! state. No state transitions and no derivation live here, which keeps the
//! update logic testable and lets the draw sweep assert painted text instead of
//! "does not panic" (refactor B17).
//!
//! Every colour lives here, and every colour is one of two kinds. The *chrome*
//! wears the workspace's [`Theme`] accent — so two windows are told apart at a
//! glance — and the hue only ever *points*: at whose window this is, or at
//! where the keyboard is, never at what happened. This is the whole list: the
//! focused border, the bar's badge, the message prompt, the picker's frame,
//! the selected row of whichever list is acting for the keyboard (the agent
//! tree's — a band while that pane has the keyboard, the hue as the row's own
//! ink while the chat does — and the picker's), the transcript select mode's
//! cursor band and its selection, and an activity line.
//! The *content* — dimmed text, the alert red, the floor notice's yellow, the
//! body gray — stays fixed: a failure reads the same in every workspace.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use mush_core::text::fit_row;

use crate::app::{
    elide, AgentRow, AgentsPane, BarPane, ChatPane, Focus, PickerPane, Rank, Screen, SelectRows,
};
use crate::theme::Theme;

/// The idle bar hint, when there is nothing to report. The commands it names
/// are checked against `app::commands::COMMANDS` by a test there, so the bar
/// cannot advertise a command the parser does not have (finding B2).
///
/// It is clauses joined by ` · `, and the painter paints as many of them as
/// fit the row it has (`idle_hint`), each clause whole: a hint that loses its
/// tail to the renderer mid-word is not the sentence the keys do (PM2).
pub(crate) const HINT: &str = "Tab cycles panes · /help lists commands · Ctrl-P picks a model";

pub(crate) fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn border(focused: bool, theme: &Theme) -> Style {
    if focused {
        Style::default().fg(theme.accent())
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// How the bar paints the line that won the precedence table. `chat::Rank` is
/// the order; the colour of each rank is a painting decision and lives here.
/// An alert keeps its red wherever it is read — the accent says *whose*
/// window this is, not what happened — while an activity line is chrome.
fn rank_style(rank: Rank, theme: &Theme) -> Style {
    match rank {
        Rank::Alert => Style::default().fg(Color::Red),
        Rank::Activity => Style::default().fg(theme.accent()),
        Rank::Said => Style::default().fg(Color::Gray),
    }
}

pub fn draw(frame: &mut Frame, screen: &Screen, theme: &Theme) {
    match screen {
        // One notice, centred on both axes, and nothing else. `Paragraph::centered`
        // is horizontal only, and R3's "centred" means the middle of the screen,
        // not the top row — the notice used to sit on row one (finding P11).
        Screen::Floor { area, text } => {
            let line = Line::from(Span::styled(
                text.clone(),
                Style::default().fg(Color::Yellow),
            ));
            let y = area.y + area.height.saturating_sub(1) / 2;
            frame.render_widget(
                Paragraph::new(line).centered(),
                Rect::new(area.x, y, area.width, 1),
            );
        }
        Screen::Panes(panes) => {
            // One focus, passed to every pane that is painted from it: the
            // borders, the bar's badge and the message box's cursor are three
            // readers of one fact (finding T2 §11).
            draw_agents(frame, &panes.agents, panes.focus, theme);
            draw_chat(frame, &panes.chat, panes.focus, theme);
            draw_status(frame, &panes.bar, panes.focus, theme);
            if let Some(picker) = &panes.picker {
                draw_picker(frame, picker, theme);
            }
        }
    }
}

fn draw_agents(frame: &mut Frame, pane: &AgentsPane, focus: Focus, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border(focus == Focus::Agents, theme))
        // The pane's own title, already elided by `App::agents_pane` to the
        // columns this pane has: the painter paints words, it does not choose
        // them.
        .title(pane.title.clone());
    let inner = block.inner(pane.area);
    frame.render_widget(block, pane.area);

    if inner.height == 0 || inner.width == 0 || pane.rows.is_empty() {
        return;
    }

    // The row gets the pane's whole inner width. A `› ` highlight symbol used
    // to be drawn outside it, which spent two columns on a mark the highlight
    // style already made — and put a second arrow beside the row's own `▶`.
    let row_width = inner.width as usize;
    let items: Vec<ListItem> = pane
        .rows
        .iter()
        .map(|row| {
            let line = ListItem::new(agent_line(row, row_width));
            // A row whose parent the history window forgot is history: its
            // stored link was cut, and neither the row nor the `⚮` it wears
            // borrows a colour of its own — the whole row takes `dim()`, the
            // module's content ink, which is the one ink that reports what
            // happened. The accent only ever points (whose window this is,
            // where the keyboard is), so it is not this row's to wear; the
            // list's own highlight patches over the dim on the cursor row,
            // wherever the keyboard is.
            if row.parent_gone {
                line.style(dim())
            } else {
                line
            }
        })
        .collect();
    // The cursor's mark depends on who has the keyboard, because the band *is*
    // the cursor: focused, the row is `Color::Black` on the hue — mush's one
    // way of putting text on a colour — and it is the same band the chat's
    // selection wears. With the chat focused the human asked for a quieter mark
    // ("an outline or something less intrusive (instead of fill)"): a filled
    // band in a pane that is not active reads as a second cursor, and the only
    // thing the row has to say there is "this is the agent the pane would act
    // on". The hue as the row's own ink says it with no cell's background
    // changed and no column spent, which is why an outline, an underline or the
    // old `› ` highlight symbol were all worse: any of them spends or moves a
    // cell, and the row's first cells are the tree's `▶` and the agent's own
    // `⊘`/`✓` glyph, which are state and not this mark's to paint over.
    let highlight = if focus == Focus::Agents {
        Style::default().fg(Color::Black).bg(theme.accent())
    } else {
        Style::default().fg(theme.accent())
    };
    let list = List::new(items).highlight_style(highlight);
    let mut state = ListState::default();
    state.select(Some(pane.cursor));
    // The rows go where the pane said they go. The geometry is derived once, in
    // `App::agents_pane`, because the hidden-row counts in the title are
    // arithmetic over it: a painter that worked it out again could place the
    // list one row off from the count that names what it hides (finding V1).
    frame.render_stateful_widget(list, pane.list_area, &mut state);

    if !pane.footer.is_empty() {
        // One row is the separator between the list and the facts.
        let start = inner.y + inner.height - pane.footer.len() as u16;
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                dim(),
            ))),
            Rect::new(inner.x, start - 1, inner.width, 1),
        );
        for (offset, line) in pane.footer.iter().enumerate() {
            frame.render_widget(
                Paragraph::new(line.clone()),
                Rect::new(inner.x, start + offset as u16, inner.width, 1),
            );
        }
    }
}

/// One row of the tree, fitted into the columns the pane has.
///
/// `fit_row` is the ranked-field rule R1 states: the state (`▶◐ #2)`, then the
/// branch and its delta, then the activity with its age, then the title — the
/// title yields first because the footer and the transcript carry the brief in
/// full. The fields themselves are derived by the tree; this only spends the
/// columns on them.
///
/// The head is where a mark that must never be given up rides: `✉`/`✉N` and
/// the `⚮` a row wears when its parent is gone. The title yields its columns
/// first, so a mark left in the tail could vanish on a narrow pane where the
/// fact is most needed. A row wearing `⚮` is painted whole in `dim()`
/// ([`draw_agents`]): mark and ink are the two halves of one fact — the stored
/// link was cut — and neither wears the accent, which only ever points.
///
/// `pub(crate)`, not private, because "every row fits its pane" is an assertion
/// a frame has to carry: the sweep fits each row at the width it is painted at
/// and reads the result, which is the one way a row that silently loses its
/// tail is caught (refactor B17).
pub(crate) fn agent_line(row: &AgentRow, width: usize) -> String {
    let indent = "  ".repeat(row.depth);
    let marker = if row.focused { "▶" } else { " " };
    let mut head = format!(
        "{indent}{marker}{glyph} {id}",
        id = row.id,
        glyph = row.glyph
    );
    if row.parent_gone {
        // `⚮` — this row's stored parent link was cut, and the row says so
        // while it hangs under its nearest surviving ancestor
        // (`AgentTree::painted_parent`): the placement is the one the tree can
        // still reach, and the mark is the only thing that says the row's own
        // parent is not the row it sits under. `rows()` orders and (after D9)
        // indents it by that surviving ancestor, which is exactly the shape a
        // family that really is there wears, so the structure itself cannot
        // tell the two apart; the human's own report is the case: a reaped #49
        // left its probe `✓ #58 Adversarial write-road …` sitting among the
        // root's current children as one of them. U+26AE is the one symbol
        // Unicode has for a severed pair — the pair here being the parent link
        // — and one column is what a mark on this row costs
        // (`every_row_mark_is_one_column`). It rides the head, right after the
        // id it qualifies: the head is the one field `fit_row` never gives up
        // (R1). `draw_agents` paints the whole row in `dim()`, and the mark
        // wears no ink of its own.
        head.push_str(" ⚮");
    }
    if row.result_unread {
        // `✉` — this result has not been read by its parent — and `✉N` for the
        // reads this agent owes its own children (finding H4). Both marks ride
        // with the glyph: they are facts about the agent, and the row says them
        // in the head, before the title it can give up (R1).
        head.push_str(" ✉");
    }
    if row.unread_children > 0 {
        head.push_str(&format!(" ✉{}", row.unread_children));
    }
    let tail: Vec<String> = if row.activity.is_empty() {
        Vec::new()
    } else {
        vec![row.activity.clone()]
    };
    fit_row(&head, &row.title, &row.place, &tail, width)
}

/// The pane's rows with the select mode's two marks on them: the cursor, the
/// selection, or both.
///
/// The mode hands over *which* rows ([`SelectRows`]) and the painter says what
/// they wear, the way every other colour decision lives here.
///
/// A mark is **a band behind the text, and it ends where the text ends**: the
/// row's own spans are patched and the line's own style never is. The cells a
/// row has past its last glyph stay the pane's background, and a row holding
/// nothing but whitespace — a blank source line, which the pane draws as the
/// message's indent and nothing else — has no text for a band to stand behind,
/// so it stays blank inside a selection. The mark this replaces patched every
/// span *and* the line's own style, which put the band over the row rather than
/// behind its text: the indent of a blank source line wore the hue, and a
/// dense multi-paragraph selection read as one filled rectangle. What the mark
/// is now is bands under the lines, and their ragged right edge is the honest
/// one — it is where the words stop.
///
/// The selection is the theme's hue as a background with `Color::Black` on it:
/// mush has exactly one way of putting a colour behind text, and this is it —
/// the bar's badge and the agents pane's selected row, while that pane has the
/// keyboard, paint the same pair, and the hue is chosen in the L* 65–84 band
/// precisely so it works *under* black text. The old mark kept each span's own
/// ink over the band instead, so a dimmed tool result and a green reply stayed
/// themselves inside it; that is given up on purpose, because a light band
/// under light text is mud.
///
/// The cursor is that band's inverse — the hue as the text's own colour on a
/// `Color::Black` background — and span-only for the same reason, so a
/// cursor-only row is a dark band carrying the hue's characters rather than a
/// block the pane's width, and a cursor inside a selection reads as the
/// selection's inverse: the cursor's row is never the same shade as its
/// neighbours, and a row that wears both marks wears the cursor's, because
/// that is where the keyboard is. A cursor is a *place* and not a range, so its
/// row is marked even when it is blank: a blank source line has only its indent
/// cells, and they are still its own.
///
/// Neither mark is the agents pane's selected row. That row wears `Black` on
/// the hue too while the tree has the keyboard — there is one way to put the
/// hue behind text — but it is a whole row of a *list*, a place in the tree
/// painted by the list's own highlight, while these are a *range of the
/// transcript* painted under the lines it covers and the shape of the
/// keyboard's own row. The select row's pad stays the pane's background, which
/// is what the tree's row never does.
fn select_painted(
    lines: &[Line<'static>],
    select: &SelectRows,
    theme: &Theme,
) -> Vec<Line<'static>> {
    lines
        .iter()
        .enumerate()
        .map(|(at, line)| {
            let selected = select.selected.contains(&at);
            let cursor = select.cursor.contains(&at);
            if !selected && !cursor {
                return line.clone();
            }
            // A row with no text has nothing for a band to stand behind, so a
            // blank row inside a selection is left alone. The cursor's own row
            // is marked either way: the keyboard is on it, and the blank row's
            // indent cells are all it has to say so with.
            if selected && !cursor && line_is_blank(line) {
                return line.clone();
            }
            let mark = if cursor {
                // The band's inverse: the hue's characters on `Black`.
                Style::default().fg(theme.accent()).bg(Color::Black)
            } else {
                // The one way mush paints text on the hue.
                Style::default().fg(Color::Black).bg(theme.accent())
            };
            // Patched onto every span rather than set as the line's own style:
            // a span's colour (the dim of a result, the reply's green) is what
            // the row *is*, and the mark is laid over it — the same order the
            // rest of the frame paints in, where content is chosen first and
            // the chrome patches what it must. The line's own style is carried
            // through untouched: it belongs to the row, not to the mark.
            let spans = line
                .spans
                .iter()
                .map(|span| Span::styled(span.content.clone(), span.style.patch(mark)))
                .collect::<Vec<Span<'static>>>();
            Line::from(spans).style(line.style)
        })
        .collect()
}

/// Whether a row holds nothing but whitespace — the shape a blank source line
/// takes once the pane has drawn its message's indent under it. Such a row
/// wears no selection band: a band is behind the text, and there is no text.
fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|span| span.content.trim().is_empty())
}

fn draw_chat(frame: &mut Frame, pane: &ChatPane, focus: Focus, theme: &Theme) {
    let focused = focus == Focus::Chat;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border(focused, theme));
    match &pane.transcript {
        Some(painted) => {
            let inner = block.inner(pane.transcript_area);
            frame.render_widget(block.title(painted.title.clone()), pane.transcript_area);
            let lines = match &painted.select {
                Some(select) => select_painted(&painted.lines, select, theme),
                None => painted.lines.clone(),
            };
            frame.render_widget(Paragraph::new(Text::from(lines)), inner);
        }
        None => frame.render_widget(block, pane.transcript_area),
    }

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(border(focused, theme))
        .title(input_title(pane));
    let input_inner = input_block.inner(pane.input_area);
    frame.render_widget(input_block, pane.input_area);

    let Some(input) = &pane.input else {
        return;
    };
    let prompt_width = UnicodeWidthStr::width(input.prompt.as_str());
    let mut rendered: Vec<Line> = Vec::with_capacity(input.attachments.len() + input.lines.len());
    // The attachments are painted above the text, as part of the message being
    // written: they are what will ride with the words below them, and the box
    // reads top to bottom the way the message does.
    for row in &input.attachments {
        rendered.push(Line::from(Span::styled(row.clone(), dim())));
    }
    for (index, line) in input.lines.iter().enumerate() {
        let (lead, style) = if index == 0 {
            (input.prompt.clone(), Style::default().fg(theme.accent()))
        } else {
            // Continuation lines line up under the first, so the prompt reads
            // as a margin rather than as part of the message.
            (" ".repeat(prompt_width), Style::default())
        };
        rendered.push(Line::from(vec![
            Span::styled(lead, style),
            Span::raw(line.clone()),
        ]));
    }
    frame.render_widget(Paragraph::new(Text::from(rendered)), input_inner);
    if focused {
        let x = input_inner.x
            + ((prompt_width + input.column).min(input_inner.width.saturating_sub(1) as usize)
                as u16);
        // The cursor lives in the text area, never on an attachment row: the
        // box gives the text its row before the attachments (`content_rows`),
        // so the rows above the cursor are only the attachment rows it was
        // really granted, and there is a painted line below them. The last-row
        // clamp is a backstop for a view that outlived its rect.
        let row =
            input.attachments.len() + input.cursor_row.min(input.lines.len().saturating_sub(1));
        let y = input_inner.y + (row as u16).min(input_inner.height.saturating_sub(1));
        frame.set_cursor_position(Position::new(x, y));
    }
}

/// `1 image` / `2 images`, the way the box's title counts them. Shared with
/// the line a clear says ([`crate::app::Chat::apply`]), so the same count
/// cannot be spelled two ways on one screen.
pub(crate) fn image_count(images: usize) -> String {
    if images == 1 {
        "1 image".to_string()
    } else {
        format!("{images} images")
    }
}

/// The message box's title: ` message `, and how many images are attached when
/// any are. The count is the whole one, not the rows': past the row cap the box
/// shows `▣ +2 more`, and a title that repeated that number would be counting
/// the abbreviation instead of the message.
fn input_title(pane: &ChatPane) -> String {
    match pane.input.as_ref().map(|input| input.attachment_count) {
        Some(count) if count > 0 => format!(" message · {} ", image_count(count)),
        _ => " message ".to_string(),
    }
}

/// A centered modal list for `/model` and `/provider`. The current selection
/// is marked with a bullet; Enter picks, Esc cancels.
fn draw_picker(frame: &mut Frame, picker: &PickerPane, theme: &Theme) {
    frame.render_widget(Clear, picker.area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent()))
        .title(picker.title.clone());
    let inner = block.inner(picker.area);
    frame.render_widget(block, picker.area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let items: Vec<ListItem> = picker
        .items
        .iter()
        .map(|item| ListItem::new(item.clone()))
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().fg(Color::Black).bg(theme.accent()))
        .highlight_symbol("› ");
    let mut state = ListState::default();
    state.select(Some(picker.cursor));
    frame.render_stateful_widget(list, inner, &mut state);

    let hint = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(picker.hint, dim()))),
        hint,
    );
}

fn draw_status(frame: &mut Frame, pane: &BarPane, focus: Focus, theme: &Theme) {
    let badge = match focus {
        Focus::Agents => "agents",
        Focus::Chat => "chat",
    };
    // The idle hint is the one word on this row that can be wider than the
    // row: it gets the columns the badge and the space after it leave (PM2).
    let idle;
    let (message, style) = match &pane.word {
        Some((rank, text)) => (text.as_str(), rank_style(*rank, theme)),
        None => {
            idle = idle_hint(pane.area.width.saturating_sub(badge.len() as u16 + 3) as usize);
            (idle.as_str(), dim())
        }
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" {badge} "),
            Style::default().fg(Color::Black).bg(theme.accent()),
        ),
        Span::raw(" "),
        Span::styled(message, style),
    ]);
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(pane.area);
    frame.render_widget(Paragraph::new(line), rows[0]);
    if let Some(facts) = &pane.facts {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(facts.clone(), dim()))),
            rows[1],
        );
    }
}

/// The idle hint, cut to the columns the bar's badge leaves it.
///
/// [`HINT`]'s clauses are dropped whole from the right by [`elide`] — the one
/// rule the panes' titles and the facts line are cut by (finding D9) — because
/// a hint cut mid-word names no key at all: at the 40-column floor the old
/// ` agents Tab cycles panes · /help lis` lost its tail to the renderer, and
/// the clause that fits (`Tab cycles panes`) is the honest line. A terminal
/// wide enough for the whole sentence keeps every clause; the sample frame on
/// the front page is painted at 100 columns and is one (PM2).
fn idle_hint(columns: usize) -> String {
    let cells: Vec<String> = HINT.split(" · ").map(str::to_string).collect();
    let floor = cells[0].clone();
    elide(&cells, " · ", "", &floor, columns)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    use crate::app::{AgentId, Painted};
    use crate::theme::EnvText;

    /// A picker at `area` with one item, so a frame has a border, a selected
    /// row and a hint to paint.
    fn picker(area: Rect) -> PickerPane {
        PickerPane {
            area,
            title: " models ".to_string(),
            hint: "j/k or PgUp/PgDn",
            items: vec!["• test-model · 500k".to_string()],
            cursor: 0,
        }
    }

    /// One popup at a 20×6 terminal, painted the way the panes screen draws it.
    fn picker_buffer(theme: &Theme) -> Buffer {
        let area = Rect::new(0, 0, 20, 6);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|frame| draw_picker(frame, &picker(area), theme))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// The style of one cell, for the tests that assert what a *colour*
    /// reached the frame rather than what text did.
    fn style_at(buffer: &Buffer, x: u16, y: u16) -> Style {
        buffer[(x, y)].style()
    }

    /// The record disagreed with itself about this: `docs/audits/tui.md:898`
    /// read ratatui 0.29's `Buffer::set_stringn` as filtering graphemes
    /// holding controls, while `33409d4` had measured an escape reaching the
    /// terminal. The one-line experiment settles it, and the answer is that
    /// the crate does **not** filter: painting `Span::raw("\u{1b}[2J")` at
    /// 10×1 leaves the ESC, the `[`, the `2` and the `J` in four cells of the
    /// buffer (`crossterm`'s backend writes `cell.symbol()` verbatim, so these
    /// bytes reach the terminal as a frame wipe). That is why every surface
    /// that paints a string mush did not write defangs it at the paint
    /// boundary (PM1/IN5).
    #[test]
    fn a_raw_span_paints_a_control_byte_verbatim() {
        let mut terminal = Terminal::new(TestBackend::new(10, 1)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::raw("\u{1b}[2Jx"))),
                    frame.area(),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let symbols: Vec<String> = (0..4u16)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect();
        assert_eq!(
            symbols,
            vec!["\u{1b}", "[", "2", "J"],
            "ratatui 0.29's `Buffer::set_stringn` paints a control byte into its cell"
        );
    }

    /// A popup with no inner room — a terminal too narrow or too short for a
    /// list — paints `Clear`, its border, and nothing else: the list and the
    /// hint need the row and the column `Block::inner` does not leave, which is
    /// the one place that question is answered (Tier 1 §23).
    #[test]
    fn a_popup_with_no_inner_room_paints_nothing_but_its_border() {
        for area in [Rect::new(0, 0, 2, 6), Rect::new(0, 0, 20, 2)] {
            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            terminal
                .draw(|frame| draw_picker(frame, &picker(area), &Theme::default()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let painted: String = (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!painted.contains('•'), "{area:?}: {painted}");
            assert!(!painted.contains("test-model"), "{area:?}: {painted}");
            assert!(
                !painted.contains("PgUp"),
                "the hint needs the row the popup does not have: {area:?}: {painted}"
            );
        }
    }

    /// A chat pane with six transcript rows and no box, so the select mode's
    /// two marks can be read off the frame's own cells. The lines carry the
    /// shapes a real transcript's do — plain text, a dim result, the indent row
    /// a blank source line is painted as — because the two things the mark must
    /// never do are fill a blank row and reach past a row's text.
    fn select_pane(area: Rect, select: Option<SelectRows>) -> ChatPane {
        ChatPane {
            transcript_area: area,
            // Below the transcript, with no rows: the box is not what this test
            // reads, and a zero-height one paints nothing.
            input_area: Rect::new(area.x, area.y + area.height, area.width, 0),
            transcript: Some(Painted {
                lines: vec![
                    Line::from("plain"),
                    Line::from(Span::styled("dim result", dim())),
                    Line::from(Span::raw("      ")),
                    Line::from("cursor"),
                    Line::from("bare"),
                    Line::from("plain again"),
                ],
                title: " mush ".to_string(),
                select,
            }),
            input: None,
        }
    }

    /// The select mode reaches the frame as two marks that are neither each
    /// other nor the agents pane's selected row. The pick is the hue as a
    /// *background* with `Color::Black` on it — mush's one way of putting a
    /// colour behind text — and it is behind the text only: the cells past a
    /// row's last word, and every cell of a blank row, stay the pane's own
    /// background. The cursor is the pick's inverse, the hue's characters on
    /// `Black`, and a row that carries both marks wears the cursor's.
    #[test]
    fn the_select_mode_paints_its_cursor_and_its_selection_on_their_own_cells() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 20, 8);
        let select = SelectRows {
            // Row 3 is both marks at once (the frame must show the cursor's own
            // inverse, not the band) and row 4 is a bare cursor. Row 2 is a
            // blank source line inside the selection, which wears nothing.
            cursor: vec![3, 4],
            selected: vec![1, 2, 3],
        };
        let painted = |pane: &ChatPane| {
            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            terminal
                .draw(|frame| draw_chat(frame, pane, Focus::Chat, &theme))
                .unwrap();
            terminal.backend().buffer().clone()
        };
        let buffer = painted(&select_pane(area, Some(select)));
        let ordinary = painted(&select_pane(area, None));

        // The pane's border takes (0, 0), so the transcript's six rows start at
        // (1, 1) and the cells past their text run to (18, y). A *cell's* style
        // carries `underline_color` where a span's does not, so the two marks
        // are read as the pair of colours they are rather than as whole styles.
        let wears_band =
            |style: Style| style.fg == Some(Color::Black) && style.bg == Some(theme.accent());
        let wears_inverse =
            |style: Style| style.fg == Some(theme.accent()) && style.bg == Some(Color::Black);

        // The rows nobody marked are the rows the pane painted before the mode.
        for y in [1, 6] {
            for x in 1..19 {
                assert_eq!(
                    style_at(&buffer, x, y),
                    style_at(&ordinary, x, y),
                    "row {y} is not the mode's to paint"
                );
            }
        }

        // "dim result": ten cells of text at (1, 2) and eight columns of pane
        // past it. Every text cell wears the band, and no cell past the text
        // does — the rectangle fix, read one column at a time.
        for x in 1..=10 {
            assert!(
                wears_band(style_at(&buffer, x, 2)),
                "the text at ({x}, 2) wears the band: {:?}",
                style_at(&buffer, x, 2)
            );
        }
        for x in 11..19 {
            assert_eq!(
                style_at(&buffer, x, 2),
                style_at(&ordinary, x, 2),
                "({x}, 2) is past the text and must not wear the band"
            );
        }

        // The blank source line: six cells of indent with the selection over
        // it. A row with no text has nothing for a band to stand behind, so it
        // is not painted at all.
        for x in 1..19 {
            assert_eq!(
                style_at(&buffer, x, 3),
                style_at(&ordinary, x, 3),
                "a blank row inside the selection stays blank: ({x}, 3)"
            );
        }

        // "cursor": the pick's inverse on the six text cells, and nothing past
        // them. The row is selected and the cursor's, and the cursor wins.
        for x in 1..=6 {
            assert!(
                wears_inverse(style_at(&buffer, x, 4)),
                "the cursor at ({x}, 4) is the band's inverse: {:?}",
                style_at(&buffer, x, 4)
            );
        }
        for x in 7..19 {
            assert_eq!(
                style_at(&buffer, x, 4),
                style_at(&ordinary, x, 4),
                "({x}, 4) is past the cursor's text"
            );
        }

        // "bare": a cursor with no selection wears the same inverse, and it is
        // neither the band nor the agents pane's selected row.
        for x in 1..=4 {
            assert!(
                wears_inverse(style_at(&buffer, x, 5)),
                "the bare cursor at ({x}, 5) is the band's inverse: {:?}",
                style_at(&buffer, x, 5)
            );
        }
        assert!(
            !wears_band(style_at(&buffer, 1, 5)),
            "which is neither the selection's band nor the tree's selected row"
        );
        for x in 5..19 {
            assert_eq!(
                style_at(&buffer, x, 5),
                style_at(&ordinary, x, 5),
                "({x}, 5) is past the cursor's text"
            );
        }
    }

    /// A cursor's row is marked even when it is blank — a blank source line is
    /// painted as its message's indent and nothing else, so those few cells are
    /// all the cursor has to say where the keyboard is — while the same blank
    /// row under the selection alone stays exactly as the pane painted it.
    #[test]
    fn a_blank_row_wears_the_cursor_but_not_the_selection() {
        let theme = Theme::default();
        let lines = vec![
            Line::from(Span::raw("      ")),
            Line::from(Span::raw("      ")),
            Line::from("text"),
        ];
        let select = SelectRows {
            // The first row is the cursor's own blank line; the second is a
            // blank line the selection alone covers.
            cursor: vec![0],
            selected: vec![0, 1, 2],
        };
        let painted = select_painted(&lines, &select, &theme);
        assert_eq!(
            painted[0].spans[0].style,
            Style::default().fg(theme.accent()).bg(Color::Black),
            "the cursor's own blank row keeps the keyboard visible on its indent"
        );
        assert_eq!(
            painted[1], lines[1],
            "a blank row the selection covers keeps its own style"
        );
        assert_eq!(
            painted[2].spans[0].style,
            Style::default().fg(Color::Black).bg(theme.accent()),
            "and a row with text wears the band"
        );
    }

    /// The default theme paints exactly what the tree painted before hues
    /// existed: the accent cells wear `Cyan`, the selected row's text is
    /// `Black`, and no cell asks for an `Rgb` or `Indexed` colour that a
    /// 256-colour terminal would paint as something else. This is the promise
    /// `Default` makes to every caller that does not care about hues.
    #[test]
    fn the_default_theme_paints_the_fixed_palette() {
        let buffer = picker_buffer(&Theme::default());
        for y in 0..6 {
            for x in 0..20 {
                let style = style_at(&buffer, x, y);
                for colour in [style.fg, style.bg].into_iter().flatten() {
                    assert!(
                        !matches!(colour, Color::Rgb(..) | Color::Indexed(_)),
                        "({x}, {y}) asks for {colour:?}"
                    );
                }
            }
        }
        // The left border is the accent, and the selected row is black text on
        // it — the two sites a hue would repaint.
        assert_eq!(style_at(&buffer, 0, 1).fg, Some(Color::Cyan));
        assert_eq!(style_at(&buffer, 1, 1).fg, Some(Color::Black));
        assert_eq!(style_at(&buffer, 1, 1).bg, Some(Color::Cyan));
        // The hint is dimmed text, and dimming is not chrome: it stays gray in
        // every workspace.
        assert_eq!(style_at(&buffer, 1, 4).fg, Some(Color::DarkGray));
    }

    /// A theme with a hue in it reaches the frame: the border and the selected
    /// row wear the hue's own `Rgb`, while the dim hint keeps the fixed gray.
    /// This is the test that fails if a painter site quietly goes back to the
    /// fixed palette.
    #[test]
    fn a_themed_frame_paints_the_hue() {
        let env = EnvText {
            theme: None,
            colorterm: Some("truecolor".to_string()),
            term: None,
        };
        let theme = Theme::resolve(&env, std::path::Path::new("/nonexistent/workspace")).unwrap();
        let hue = theme.hue().expect("a truecolor terminal gets the hue");
        assert_eq!(theme.accent(), Color::Rgb(hue.rgb.0, hue.rgb.1, hue.rgb.2));

        let buffer = picker_buffer(&theme);
        assert_eq!(style_at(&buffer, 0, 1).fg, Some(theme.accent()));
        assert_eq!(style_at(&buffer, 1, 1).fg, Some(Color::Black));
        assert_eq!(style_at(&buffer, 1, 1).bg, Some(theme.accent()));
        assert_eq!(style_at(&buffer, 1, 4).fg, Some(Color::DarkGray));
    }

    /// An agents pane at `area` with the tree's four leading shapes — an idle
    /// `·`, the `⊘` a stopped agent wears, a finished `✓`, and a running `◐`
    /// three levels in — and the cursor on the `✓` row, so
    /// the mark has rows on both sides of it and the leading glyphs it must not
    /// paint over. The geometry is `App::agents_pane`'s at a plain size: a
    /// bordered pane with no footer, so the list gets the whole inner rect.
    fn agents_pane(area: Rect) -> AgentsPane {
        let rows = ["·", "⊘", "✓", "◐"]
            .into_iter()
            .enumerate()
            .map(|(at, glyph)| AgentRow {
                id: AgentId(at as u64),
                depth: at,
                // The fixture's four rows hang under the root or nowhere; the
                // severed-parent mark is `screen.rs`'s own test's business.
                parent_gone: false,
                glyph,
                focused: false,
                result_unread: false,
                unread_children: 0,
                title: format!("row {at}"),
                place: String::new(),
                activity: String::new(),
            })
            .collect();
        AgentsPane {
            area,
            list_area: Block::default().borders(Borders::ALL).inner(area),
            title: " agents ".to_string(),
            rows,
            cursor: 2,
            footer: Vec::new(),
        }
    }

    /// [`agents_pane`] painted by the pane's own painter with `focus` holding
    /// the keyboard, the way `draw` hands the focus over — so what the test
    /// reads are the cells `draw_agents` really paints, not the styles it was
    /// handed.
    fn agents_buffer(area: Rect, focus: Focus, theme: &Theme) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|frame| draw_agents(frame, &agents_pane(area), focus, theme))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// [`agents_pane`]'s sibling with a history in it: the root, a root child,
    /// a leftover worktree found on disk (no parent here, nothing lost) and a
    /// row whose stored parent the history window forgot (marked `⚮`, dim).
    /// The depths are the painted ones a real tree hands over. A sibling, not
    /// an extension: the four-row fixture's own depths are the ones the frames
    /// the other tests read are painted with.
    fn agents_pane_with_history(area: Rect, cursor: usize) -> AgentsPane {
        let row = |id: u64, depth: usize, title: &str, parent_gone: bool| AgentRow {
            id: AgentId(id),
            depth,
            parent_gone,
            glyph: "✓",
            focused: false,
            result_unread: false,
            unread_children: 0,
            title: title.to_string(),
            place: String::new(),
            activity: String::new(),
        };
        AgentsPane {
            area,
            list_area: Block::default().borders(Borders::ALL).inner(area),
            title: " agents ".to_string(),
            rows: vec![
                row(0, 0, "root", false),
                row(1, 1, "child", false),
                // A leftover worktree: no parent in this tree, nothing lost.
                row(2, 1, "leftover", false),
                // Its stored parent is an id the tree no longer holds.
                row(3, 1, "forgotten", true),
            ],
            cursor,
            footer: Vec::new(),
        }
    }

    /// A row whose stored parent the history window forgot paints dim — the
    /// module's content ink ([`dim`]) — and a leftover worktree on disk does
    /// not: the dim and the `⚮` are one fact, that a link was cut, and a
    /// leftover never had a link here to cut. The cursor row's highlight still
    /// wins over the dim ink, whichever pane holds the keyboard.
    #[test]
    fn a_forgotten_row_paints_dim_and_a_leftover_worktree_does_not() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 44, 8);
        let inner = Block::default().borders(Borders::ALL).inner(area);
        let painted = |cursor: usize, focus: Focus| -> Buffer {
            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            terminal
                .draw(|frame| {
                    draw_agents(
                        frame,
                        &agents_pane_with_history(area, cursor),
                        focus,
                        &theme,
                    )
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        let row_text = |buffer: &Buffer, y: u16| -> String {
            (inner.x..inner.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        /// The ink of every cell that carries a symbol, spaces excluded.
        fn inks(buffer: &Buffer, y: u16, inner: Rect) -> Vec<Option<Color>> {
            (inner.x..inner.right())
                .filter(|x| !buffer[(*x, y)].symbol().trim().is_empty())
                .map(|x| buffer[(x, y)].style().fg)
                .collect()
        }

        let quiet = painted(0, Focus::Chat);
        assert_eq!(row_text(&quiet, inner.y + 3), "   ✓ #3 ⚮ forgotten");
        assert!(
            inks(&quiet, inner.y + 3, inner)
                .iter()
                .all(|fg| *fg == dim().fg),
            "every cell of the forgotten row carries the content ink: {:?}",
            inks(&quiet, inner.y + 3, inner)
        );
        assert_eq!(row_text(&quiet, inner.y + 2), "   ✓ #2 leftover");
        assert!(
            inks(&quiet, inner.y + 2, inner)
                .iter()
                .all(|fg| *fg != dim().fg),
            "a leftover lost nothing, so it is not dimmed: {:?}",
            inks(&quiet, inner.y + 2, inner)
        );
        assert_eq!(row_text(&quiet, inner.y + 1), "   ✓ #1 child");

        // The cursor row's mark wins where the dim would be: the filled band
        // while the pane has the keyboard, the accent as the row's own ink
        // while the chat does.
        let accent = theme.accent();
        let focused = painted(3, Focus::Agents);
        for x in inner.x..inner.right() {
            let style = style_at(&focused, x, inner.y + 3);
            assert!(
                style.fg == Some(Color::Black) && style.bg == Some(accent),
                "the band wins over the dim at ({x}, {}): {style:?}",
                inner.y + 3
            );
        }
        let cursor_quiet = painted(3, Focus::Chat);
        for x in inner.x..inner.right() {
            let style = style_at(&cursor_quiet, x, inner.y + 3);
            assert!(
                style.fg == Some(accent) && style.bg != Some(accent),
                "the quiet mark wins over the dim at ({x}, {}): {style:?}",
                inner.y + 3
            );
        }
    }

    /// The agents pane's cursor is a filled band only while that pane has the
    /// keyboard; with the chat focused the selected row is marked with the
    /// accent as its own ink instead. The fill *is* the cursor, and a filled
    /// band in a pane that is not active reads as a second cursor — the human
    /// asked for a quieter mark there ("an outline or something less intrusive
    /// (instead of fill)"). The quiet mark spends no column and moves no cell,
    /// which an outline, an underline or a `highlight_symbol` could not promise:
    /// the selected row's leading cells are the tree's `▶` and the agent's own
    /// `⊘`/`✓` glyph, and those are state, not this mark's to paint over.
    #[test]
    fn the_agents_cursor_is_a_band_only_while_the_pane_has_the_keyboard() {
        /// The cell wears the band: `Black` text on the accent's background.
        fn wears_band(buffer: &Buffer, x: u16, y: u16, accent: Color) -> bool {
            let style = style_at(buffer, x, y);
            style.fg == Some(Color::Black) && style.bg == Some(accent)
        }
        /// The cell's text is the accent and its background is not: the quiet
        /// mark, which is ink and never a fill.
        fn wears_ink(buffer: &Buffer, x: u16, y: u16, accent: Color) -> bool {
            let style = style_at(buffer, x, y);
            style.fg == Some(accent) && style.bg != Some(accent)
        }

        let area = Rect::new(0, 0, 30, 8);
        let inner = Block::default().borders(Borders::ALL).inner(area);
        let cursor = inner.y + 2;
        // Every form the accent resolves in: the fixed palette, a workspace
        // hue's own bytes, and that hue's nearest 256-colour entry. One accent
        // is both marks in each, so the cursor reads the same through all of
        // them — a theme changes the colour, not the shape.
        let root = std::path::Path::new("/nonexistent/mush/cursor");
        let hued = Theme::resolve(
            &EnvText {
                theme: None,
                colorterm: Some("truecolor".to_string()),
                term: None,
            },
            root,
        )
        .unwrap();
        let indexed = Theme::resolve(
            &EnvText {
                theme: None,
                colorterm: None,
                term: Some("linux".to_string()),
            },
            root,
        )
        .unwrap();
        for theme in [Theme::default(), hued, indexed] {
            let accent = theme.accent();
            let focused = agents_buffer(area, Focus::Agents, &theme);
            let quiet = agents_buffer(area, Focus::Chat, &theme);

            // Focused, the cursor is the band across the pane's own row —
            // every cell of it, the way `List` paints its highlight — and no
            // other row wears it.
            for x in inner.x..inner.right() {
                assert!(
                    wears_band(&focused, x, cursor, accent),
                    "{accent:?}: the focused cursor at ({x}, {cursor})"
                );
            }
            for y in inner.y..inner.bottom() {
                if y == cursor {
                    continue;
                }
                for x in inner.x..inner.right() {
                    assert!(
                        !wears_band(&focused, x, y, accent),
                        "{accent:?}: ({x}, {y}) is not the cursor"
                    );
                }
            }

            // With the chat focused no cell of the pane is filled with the
            // accent — the band does not survive — and the cursor's row is the
            // one row whose text is the accent.
            for y in area.y..area.bottom() {
                for x in area.x..area.right() {
                    assert_ne!(
                        style_at(&quiet, x, y).bg,
                        Some(accent),
                        "{accent:?}: the band survived at ({x}, {y})"
                    );
                }
            }
            for x in inner.x..inner.right() {
                assert!(
                    wears_ink(&quiet, x, cursor, accent),
                    "{accent:?}: the quiet mark at ({x}, {cursor})"
                );
            }
            for y in inner.y..inner.bottom() {
                if y == cursor {
                    continue;
                }
                for x in inner.x..inner.right() {
                    assert!(
                        !wears_ink(&quiet, x, y, accent),
                        "{accent:?}: ({x}, {y}) is not the cursor's row"
                    );
                }
            }

            // And nothing moved: the two frames carry the same glyphs in the
            // same cells — the pane's geometry, every row's fields, and the
            // cursor row's own `✓` among them.
            for y in area.y..area.bottom() {
                for x in area.x..area.right() {
                    assert_eq!(
                        quiet[(x, y)].symbol(),
                        focused[(x, y)].symbol(),
                        "{accent:?}: ({x}, {y}) moved with the focus"
                    );
                }
            }
            assert_eq!(
                quiet[(inner.x + 5, cursor)].symbol(),
                "✓",
                "{accent:?}: the cursor row's own glyph"
            );
            assert_eq!(
                quiet[(inner.x + 3, inner.y + 1)].symbol(),
                "⊘",
                "{accent:?}: and the row above it keeps its own"
            );
        }
    }

    /// The repository root, from this crate's manifest directory: a test's cwd
    /// is the crate, and a doc path in a block below is the workspace's.
    fn repo_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// Compare a generated block in `file` against `rendered`, or — with
    /// `MUSH_BLESS_DOCS` set — write `rendered` in its place.
    ///
    /// This is the whole zero-drift mechanism: a block whose words the code
    /// prints is rewritten by the one command its own head names, and any other
    /// `cargo test` fails while the two disagree. The comparison is the whole
    /// region — the head comment, `rendered` and the tail — so a hand edit
    /// anywhere in it is drift, whitespace included. `rendered` is the block's
    /// whole text, fences and all when the block is a code block, because a
    /// manual is read as markdown and the check must not care what markdown
    /// does with it.
    ///
    /// One mutex, because three checks write `docs/mush.md`: a blessing run
    /// rewrites whole files, and two tests interleaving a read with a write
    /// would lose one block.
    pub(crate) fn doc_block(file: &str, id: &str, check: &str, rendered: &str) {
        static WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _held = WRITER.lock().unwrap_or_else(|poison| poison.into_inner());

        let path = repo_root().join(file);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let head = format!(
            "<!-- generated: {id} (blessed by MUSH_BLESS_DOCS=1 cargo test -p mush --bin mush {check}) -->"
        );
        let tail = format!("<!-- /generated: {id} -->");
        let block = format!("{head}\n{rendered}\n{tail}");

        // The head is a whole line in the file, never a substring of one.
        let start = text
            .match_indices(&head)
            .find(|(at, _)| *at == 0 || text.as_bytes()[at - 1] == b'\n')
            .unwrap_or_else(|| panic!("{file} has no `{id}` block"))
            .0;
        let after = start + head.len();
        let end = after
            + text[after..]
                .find(&tail)
                .unwrap_or_else(|| panic!("{file}'s `{id}` block has no tail"))
            + tail.len();

        if std::env::var_os("MUSH_BLESS_DOCS").is_some() {
            let next = format!("{}{block}{}", &text[..start], &text[end..]);
            std::fs::write(&path, next)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            return;
        }
        assert_eq!(
            &text[start..end],
            block,
            "{file}'s `{id}` block is stale — regenerate it with:\n  MUSH_BLESS_DOCS=1 cargo test -p mush --bin mush {check}"
        );
    }

    /// One painted frame as text: the cells the real painters put in a
    /// `TestBackend` of the frame's own size, one line per row. Colour is
    /// dropped — a manual cannot carry it — and a row's trailing spaces are
    /// cut, because they are the pane's padding and not a word it says.
    fn frame_text(width: u16, height: u16, screen: &Screen) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, screen, &Theme::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One row of the mark sweep: `agent_line`'s own fields, with every mark
    /// the row can wear set by the flag that produces it.
    #[allow(clippy::too_many_arguments)]
    fn mark_row(
        glyph: &'static str,
        title: &str,
        place: &str,
        activity: &str,
        focused: bool,
        result_unread: bool,
        unread_children: usize,
        parent_gone: bool,
    ) -> AgentRow {
        AgentRow {
            id: AgentId(0),
            depth: 0,
            parent_gone,
            glyph,
            focused,
            result_unread,
            unread_children,
            title: title.to_string(),
            place: place.to_string(),
            activity: activity.to_string(),
        }
    }

    /// Every phase glyph and every mark `agent_line` adds, as rows. The glyphs
    /// are inputs here — `phase_glyph` is private to `app::screen`, and that
    /// module's own sweep pins each phase to its glyph — while every mark this
    /// file paints is read back through the function that paints it, so a mark
    /// added, moved or removed changes this block and fails its check.
    fn marks_rows() -> Vec<AgentRow> {
        vec![
            mark_row("·", "idle", "", "", false, false, 0, false),
            mark_row("◐", "thinking", "", "thinking 3s", false, false, 0, false),
            mark_row(
                "◐",
                "working",
                "",
                "edit_file src/lib.rs 12s",
                false,
                false,
                0,
                false,
            ),
            mark_row(
                "≡",
                "compacting",
                "",
                "compacting 2s",
                false,
                false,
                0,
                false,
            ),
            mark_row(
                "⧗",
                "waiting",
                "",
                "waiting on results 3s",
                false,
                false,
                0,
                false,
            ),
            mark_row(
                "⊘",
                "cancelling",
                "",
                "cancelling 0s",
                false,
                false,
                0,
                false,
            ),
            mark_row(
                "⊘",
                "stopped",
                "",
                "stopped · re-send to resume",
                false,
                false,
                0,
                false,
            ),
            mark_row(
                "⚠",
                "cut off",
                "",
                "cut off · nothing committed",
                false,
                false,
                0,
                false,
            ),
            mark_row("✓", "done", "", "wrote README.md", false, false, 0, false),
            mark_row(
                "✗",
                "failed",
                "",
                "no route to host",
                false,
                false,
                0,
                false,
            ),
            mark_row(
                "◐",
                "the focused row",
                "",
                "thinking 3s",
                true,
                false,
                0,
                false,
            ),
            mark_row(
                "◐",
                "lexer",
                "mush/1 +12−3 ⚙1",
                "edit_file src/lex.rs 3s",
                false,
                false,
                0,
                false,
            ),
            mark_row(
                "✓",
                "result unread",
                "",
                "wrote README.md",
                false,
                true,
                0,
                false,
            ),
            mark_row("·", "two reads owed", "", "", false, false, 2, false),
            mark_row(
                "✓",
                "parent gone",
                "",
                "wrote src/lex.rs",
                false,
                false,
                0,
                true,
            ),
        ]
    }

    /// The rows a tree draws, painted by the row painter itself: `ui`'s side of
    /// the mark set (`agent_line`'s `▶`, `⚮` and `✉`/`✉N`) and the phase
    /// glyph column, in the manual's `marks` block — and on the front page,
    /// which carries the same block.
    #[test]
    fn the_marks_block_matches_the_code() {
        let rows = marks_rows();
        let painted: Vec<String> = rows.iter().map(|row| agent_line(row, 56)).collect();
        let rendered = format!("```\n{}\n```", painted.join("\n"));
        for file in ["docs/mush.md", "README.md"] {
            doc_block(
                file,
                "marks",
                "ui::tests::the_marks_block_matches_the_code",
                &rendered,
            );
        }
    }

    /// The sample frame on the manual's front page, painted by the real
    /// painters at 100×28 — a wide enough terminal for the whole facts line and
    /// the pane title's Σ. Its `Screen` is built by `app::commands`'s test
    /// fixture, because a message box's `InputPane` cannot be named from this
    /// module (`app::screen` is private to `app`), and a sample that showed an
    /// empty box would be a picture no `App` ever paints.
    #[test]
    fn the_readme_frame_matches_the_code() {
        let screen = crate::app::commands::tests::readme_sample_screen();
        doc_block(
            "README.md",
            "frame",
            "ui::tests::the_readme_frame_matches_the_code",
            &format!("```\n{}\n```", frame_text(100, 28, &screen)),
        );
    }

    /// The frame §4.5 photographs: one row per phase and per mark, painted the
    /// way a real terminal paints them.
    #[test]
    fn the_manual_frame_matches_the_code() {
        let screen = crate::app::commands::tests::manual_sample_screen();
        doc_block(
            "docs/mush.md",
            "frame",
            "ui::tests::the_manual_frame_matches_the_code",
            &format!("```\n{}\n```", frame_text(100, 28, &screen)),
        );
    }
}
