//! Rendering. This module is deliberately dumb: it reads `App` and paints it.
//! No state transitions live here, which keeps the update logic testable.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use mush_core::message::Message;

use crate::app::{display_column, AgentNode, App, Focus, Mode, PickerKind};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let rows = Layout::vertical([
        Constraint::Min(6),
        Constraint::Percentage(45),
        Constraint::Length(1),
    ])
    .split(area);
    let columns =
        Layout::horizontal([Constraint::Percentage(24), Constraint::Min(20)]).split(rows[0]);

    draw_agents(frame, app, columns[0]);
    draw_editor(frame, app, columns[1]);
    draw_chat(frame, app, rows[1]);
    draw_status(frame, app, rows[2]);
    draw_picker(frame, app);
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn border(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn draw_agents(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Agents;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border(focused))
        .title(" agents ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 || app.agents.is_empty() {
        return;
    }
    let items: Vec<ListItem> = app
        .agents
        .iter()
        .map(|node| agent_item(app, node, inner.width as usize))
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
        .highlight_symbol("› ");
    let mut state = ListState::default();
    state.select(Some(app.agent_cursor.min(app.agents.len().saturating_sub(1))));
    frame.render_stateful_widget(list, inner, &mut state);
}

/// One tree row: indent by depth, status glyph, id, brief, last action, branch.
/// `▶` marks the focused agent (whose chat the bottom pane shows).
fn agent_item(app: &App, node: &AgentNode, width: usize) -> ListItem<'static> {
    let indent = "  ".repeat(node.depth);
    let glyph = if node.error.is_some() {
        "✗"
    } else if node.running {
        if app.agents.iter().any(|n| n.parent == Some(node.id) && n.running) {
            "⏸" // waiting on children
        } else {
            "◐"
        }
    } else {
        "✓"
    };
    let marker = if app.focused == node.id { "▶" } else { " " };
    let mut text = format!("{indent}{marker}{glyph} #{:<3}", node.id);
    if width > text.chars().count() + 1 {
        let detail = if node.running {
            node.last.clone()
        } else {
            node.summary.clone().unwrap_or_default()
        };
        let tail = format!(
            "{}  {}  {}",
            truncate(&node.brief, 18),
            truncate(&detail, 14),
            node.branch.as_deref().unwrap_or("")
        );
        text.push(' ');
        text.push_str(&truncate(&tail, width.saturating_sub(text.chars().count() + 1)));
    }
    ListItem::new(text.trim_end().to_string())
}

fn draw_editor(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Editor;
    let title = match app.current_rel() {
        Some(rel) => {
            let dirty = app
                .current
                .map(|index| app.buffers[index].dirty)
                .unwrap_or(false);
            if dirty {
                format!(" {rel} • ")
            } else {
                format!(" {rel} ")
            }
        }
        None => " editor ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border(focused))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let Some(index) = app.current else {
        frame.render_widget(
            Paragraph::new("No file open — Tab to files, Enter to open.").style(dim()),
            inner,
        );
        return;
    };

    let buffer = &mut app.buffers[index];
    let view_height = inner.height as usize;
    let gutter = buffer.line_count().to_string().len().max(3);
    let text_width = (inner.width as usize).saturating_sub(gutter + 3);
    buffer.scroll_view(view_height, text_width);

    let start = buffer.scroll;
    let end = (start + view_height).min(buffer.line_count());
    let mut lines = Vec::with_capacity(end.saturating_sub(start));
    for row in start..end {
        let number = format!("{:>width$}", row + 1, width = gutter);
        let current = row == buffer.cursor.0 && focused;
        let line_style = if current {
            Style::default().bg(Color::Indexed(236))
        } else {
            Style::default()
        };
        let expanded: String = buffer
            .line(row)
            .chars()
            .map(|c| if c == '\t' { "    ".to_string() } else { c.to_string() })
            .collect();
        let content = slice_columns(&expanded, buffer.h_scroll, text_width);
        lines.push(Line::from(vec![
            Span::styled(number, dim()),
            Span::styled(" │ ", dim()),
            Span::styled(content, line_style),
        ]));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);

    if focused && app.mode == Mode::Insert {
        let row = buffer.cursor.0.saturating_sub(buffer.scroll) as u16;
        let column = display_column(buffer.line(buffer.cursor.0), buffer.cursor.1)
            .saturating_sub(buffer.h_scroll) as u16;
        let x = inner.x + gutter as u16 + 3 + column;
        let y = inner.y + row;
        if y < inner.y + inner.height && x < inner.x + inner.width {
            frame.set_cursor_position(Position::new(x, y));
        }
    }
}

fn draw_chat(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Chat;
    let rows =
        Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).split(area);

    let title = if app.focused == 0 {
        " mush ".to_string()
    } else {
        format!(" agent #{} ", app.focused)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border(focused))
        .title(title);
    let inner = block.inner(rows[0]);
    frame.render_widget(block, rows[0]);

    if inner.height > 0 && inner.width > 0 {
        let width = inner.width as usize;
        let height = inner.height as usize;
        let messages = focused_messages(app);
        let lines = transcript_lines(app, messages, width);
        let max_scroll = lines.len().saturating_sub(height);
        let scroll = max_scroll.saturating_sub(app.chat_scroll.min(max_scroll));
        let visible: Vec<Line> = lines.into_iter().skip(scroll).take(height).collect();
        frame.render_widget(Paragraph::new(Text::from(visible)), inner);
    }

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(border(focused))
        .title(" message ");
    let input_inner = input_block.inner(rows[1]);
    frame.render_widget(input_block, rows[1]);

    if input_inner.height > 0 && input_inner.width > 0 {
        let prompt = if app.focused == 0 {
            "› ".to_string()
        } else {
            format!("#{} › ", app.focused)
        };
        let line = Line::from(vec![
            Span::styled(prompt.clone(), Style::default().fg(Color::Cyan)),
            Span::raw(app.input.clone()),
        ]);
        frame.render_widget(Paragraph::new(line), input_inner);
        if focused {
            let offset = (UnicodeWidthStr::width(prompt.as_str())
                + UnicodeWidthStr::width(app.input.as_str()))
                as u16;
            let max_x = input_inner.x + input_inner.width.saturating_sub(1);
            let x = (input_inner.x + offset).min(max_x);
            frame.set_cursor_position(Position::new(x, input_inner.y));
        }
    }
}

/// The transcript the chat pane shows: the root's conversation by default,
/// otherwise the focused agent's.
fn focused_messages(app: &App) -> &[Message] {
    if app.focused == 0 {
        &app.chat
    } else {
        app.agent_msgs
            .get(&app.focused)
            .map(Vec::as_slice)
            .unwrap_or(EMPTY_MESSAGES)
    }
}

const EMPTY_MESSAGES: &[Message] = &[];

/// A centered modal list for `/model` and `/provider`. The current selection
/// is marked with a bullet; Enter picks, Esc cancels.
fn draw_picker(frame: &mut Frame, app: &App) {
    let Some(picker) = &app.picker else {
        return;
    };
    let area = frame.area();
    let width = (area.width * 60 / 100).clamp(40, 80);
    let height = ((picker.items.len() as u16 + 3).min(24)).min(area.height.saturating_sub(2));
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(picker.title());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Window the list so many items never overflow the popup.
    let visible = inner.height.saturating_sub(1) as usize;
    let start = picker.cursor.saturating_sub(visible / 2);
    let mut items = Vec::new();
    for item in picker.items.iter().skip(start).take(visible) {
        let current = match picker.kind {
            PickerKind::Model => *item == app.cfg.model,
            PickerKind::Provider => item == app.cfg.provider.name(),
        };
        let label = if current {
            format!("• {item}")
        } else {
            format!("  {item}")
        };
        items.push(ListItem::new(label));
    }
    let list = List::new(items)
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
        .highlight_symbol("› ");
    let mut state = ListState::default();
    state.select(Some(picker.cursor.saturating_sub(start)));
    frame.render_stateful_widget(list, inner, &mut state);

    let hint = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(" Enter pick · Esc cancel ", dim()))),
        hint,
    );
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {    let focus = match app.focus {
        Focus::Agents => "agents",
        Focus::Editor => match app.mode {
            Mode::Normal => "editor · normal",
            Mode::Insert => "editor · insert",
        },
        Focus::Chat => "chat",
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" {focus} "),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ),
        Span::raw(" "),
        Span::styled(app.status.clone(), Style::default().fg(Color::Gray)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn transcript_lines(app: &App, messages: &[Message], width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if app.focused == 0 {
        if messages.is_empty() && app.notices.is_empty() {
            out.push(Line::from(Span::styled(
                "Ask for a change. The agent reads and edits this workspace directly.",
                dim(),
            )));
            out.push(Line::from(""));
            out.push(Line::from(Span::styled(format!("model: {}", app.cfg.label()), dim())));
            out.push(Line::from(Span::styled(
                "Tab cycles panes · Enter sends · Ctrl-P pick a model",
                dim(),
            )));
            return out;
        }
    } else if messages.is_empty() {
        out.push(Line::from(Span::styled(
            format!("Agent #{} has no messages yet — typing here sends it a nudge.", app.focused),
            dim(),
        )));
        return out;
    }

    for message in messages {
        render_message(&mut out, message, width);
    }
    for notice in &app.notices {
        for line in wrap_text(notice, width.saturating_sub(2)) {
            out.push(Line::from(Span::styled(
                format!("! {line}"),
                Style::default().fg(Color::Red),
            )));
        }
    }
    if app.busy {
        out.push(Line::from(""));
        out.push(Line::from(Span::styled(
            format!("{} working…", SPINNER[(app.spin as usize) % SPINNER.len()]),
            Style::default().fg(Color::Cyan),
        )));
    }
    out
}

fn render_message(out: &mut Vec<Line<'static>>, message: &Message, width: usize) {
    match message.role.as_str() {
        "user" => {
            for (index, line) in wrap_text(message.text(), width).into_iter().enumerate() {
                if index == 0 {
                    out.push(Line::from(vec![
                        Span::styled("you › ", Style::default().fg(Color::Cyan)),
                        Span::raw(line),
                    ]));
                } else {
                    out.push(Line::from(vec![Span::raw("      "), Span::raw(line)]));
                }
            }
            out.push(Line::from(""));
        }
        "assistant" => {
            let text = message.text();
            if !text.trim().is_empty() {
                for (index, line) in wrap_text(text, width).into_iter().enumerate() {
                    if index == 0 {
                        out.push(Line::from(vec![
                            Span::styled("mush › ", Style::default().fg(Color::Green)),
                            Span::raw(line),
                        ]));
                    } else {
                        out.push(Line::from(vec![Span::raw("       "), Span::raw(line)]));
                    }
                }
            }
            for call in message.tool_calls() {
                let args = call.function.arguments.replace('\n', " ");
                let label = format!(
                    "  ⚙ {}({})",
                    call.function.name,
                    truncate(&args, width.saturating_sub(24))
                );
                out.push(Line::from(Span::styled(
                    label,
                    Style::default().fg(Color::Yellow),
                )));
            }
            out.push(Line::from(""));
        }
        "tool" => {
            let wrapped = wrap_text(message.text(), width.saturating_sub(2));
            let clipped = wrapped.len() > 8;
            for line in wrapped.iter().take(8) {
                out.push(Line::from(Span::styled(format!("  {line}"), dim())));
            }
            if clipped {
                out.push(Line::from(Span::styled("  …", dim())));
            }
            out.push(Line::from(""));
        }
        _ => {}
    }
}

/// Char-aware slice by display column, used for horizontal scrolling.
fn slice_columns(line: &str, skip: usize, width: usize) -> String {
    let mut out = String::new();
    let mut column = 0usize;
    for ch in line.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
        if column + char_width <= skip {
            column += char_width;
            continue;
        }
        if column >= skip && column + char_width <= skip + width {
            out.push(ch);
        }
        column += char_width;
        if column >= skip + width {
            break;
        }
    }
    out
}

/// Word-aware wrapping that preserves explicit newlines and never splits a
/// grapheme's display width arithmetic.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let mut current = String::new();
        let mut current_width = 0usize;
        let mut last_space: Option<usize> = None;

        for ch in raw.chars() {
            let (rendered, char_width) = if ch == '\t' {
                ("    ".to_string(), 4)
            } else {
                (
                    ch.to_string(),
                    UnicodeWidthChar::width(ch).unwrap_or(1).max(1),
                )
            };

            if current_width + char_width > width && !current.is_empty() {
                if let Some(space) = last_space {
                    let rest = current.split_off(space);
                    out.push(std::mem::take(&mut current));
                    current = rest.trim_start().to_string();
                } else {
                    out.push(std::mem::take(&mut current));
                }
                current_width = UnicodeWidthStr::width(current.as_str());
                last_space = None;
            }

            current.push_str(&rendered);
            current_width += char_width;
            if ch == ' ' {
                last_space = Some(current.len() - 1);
            }
        }
        out.push(current);
    }
    out
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_on_word_boundaries() {
        let lines = wrap_text("the quick brown fox jumps", 10);
        assert!(lines.iter().all(|line| line.chars().count() <= 10));
        assert_eq!(lines.concat().replace(' ', ""), "thequickbrownfoxjumps");
    }

    #[test]
    fn preserves_newlines() {
        assert_eq!(wrap_text("a\nb", 10), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn hard_splits_long_words() {
        let lines = wrap_text("abcdefghijklmnop", 5);
        assert!(lines.iter().all(|line| line.chars().count() <= 5));
        assert_eq!(lines.join(""), "abcdefghijklmnop");
    }

    #[test]
    fn slices_by_display_column() {
        assert_eq!(slice_columns("hello world", 6, 5), "world");
        assert_eq!(slice_columns("hello", 3, 10), "lo");
    }
}