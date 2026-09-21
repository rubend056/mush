//! Painting. This module is deliberately dumb: it takes a [`Screen`] — a value
//! [`crate::app::App::screen`] derived every word of — and paints it, without reading any
//! state. No state transitions and no derivation live here, which keeps the
//! update logic testable and lets the draw sweep assert painted text instead of
//! "does not panic" (refactor B17).
//!
//! Every colour lives here, and every colour is one of two kinds. The *chrome*
//! — the focused border, the selected row, the picker's frame, the message
//! prompt, the bar's badge and an activity line — wears the workspace's
//! [`Theme`] accent, so two windows are told apart at a glance.
//! The *content* — dimmed text, the alert red, the floor notice's yellow, the
//! body gray — stays fixed: a failure reads the same in every workspace.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use mush_core::text::fit_row;

use crate::app::{AgentRow, AgentsPane, BarPane, ChatPane, Focus, PickerPane, Rank, Screen};
use crate::theme::Theme;

/// The idle bar hint, when there is nothing to report. The commands it names
/// are checked against `app::commands::COMMANDS` by a test there, so the bar
/// cannot advertise a command the parser does not have (finding B2).
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
        .map(|row| ListItem::new(agent_line(row, row_width)))
        .collect();
    let list =
        List::new(items).highlight_style(Style::default().fg(Color::Black).bg(theme.accent()));
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
    if row.waiting > 0 {
        // R4's `⏸`, owned by the children it is about: the parent's own state
        // stays in the glyph, and this says how much it has out.
        head.push_str(&format!(" ⏸{}", row.waiting));
    }
    let tail: Vec<String> = if row.activity.is_empty() {
        Vec::new()
    } else {
        vec![row.activity.clone()]
    };
    fit_row(&head, &row.title, &row.place, &tail, width)
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
            frame.render_widget(Paragraph::new(Text::from(painted.lines.clone())), inner);
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
        // The attachment rows sit above the cursor's own line, so the cursor's
        // row inside the box is that many rows further down.
        let row = input.attachments.len() + input.cursor_row;
        let y = input_inner.y + (row as u16).min(input_inner.height.saturating_sub(1));
        frame.set_cursor_position(Position::new(x, y));
    }
}

/// The message box's title: ` message `, and how many images are attached when
/// any are. The count is the whole one, not the rows': past the row cap the box
/// shows `▣ +2 more`, and a title that repeated that number would be counting
/// the abbreviation instead of the message.
fn input_title(pane: &ChatPane) -> String {
    match pane.input.as_ref().map(|input| input.attachment_count) {
        Some(1) => " message · 1 image ".to_string(),
        Some(count) if count > 1 => format!(" message · {count} images "),
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
    let (message, style) = match &pane.word {
        Some((rank, text)) => (text.as_str(), rank_style(*rank, theme)),
        None => (HINT, dim()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

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
}
