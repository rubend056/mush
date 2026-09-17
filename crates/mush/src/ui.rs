//! Rendering. This module is deliberately dumb: it reads `App` and paints it.
//! No state transitions live here, which keeps the update logic testable.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use mush_core::message::Message;

use crate::app::{
    display_column, short_age, AgentNode, App, Focus, Mode, NoticeKind, Phase, PickerKind,
    StatusKind,
};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Beyond this the transcript is unreadable, however wide the terminal is.
const MAX_TRANSCRIPT: u16 = 110;
/// Below this mush has no room to be honest: say so instead of painting shreds.
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 10;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let line = Line::from(Span::styled(
            format!("mush needs at least {MIN_WIDTH}×{MIN_HEIGHT}"),
            Style::default().fg(Color::Yellow),
        ));
        frame.render_widget(Paragraph::new(line).centered(), area);
        return;
    }

    // Size tiers (docs/mush.md §4.5 R3). Narrow or short terminals stack the
    // agent strip above the chat, because two columns starve both panes; an
    // empty editor is hidden there rather than eating the rows chat needs.
    let compact = area.width < 80 || area.height < 20;
    let editor_open = app.current.is_some();
    let bar_rows = if area.height >= 26 { 2 } else { 1 };

    if compact {
        let agent_rows = (app.agents.len() as u16 + 2).clamp(3, 6);
        let show_editor = editor_open || app.focus == Focus::Editor;
        // The chat pane draws its own message box, so there is no separate
        // constraint for it here.
        let mut constraints = vec![Constraint::Length(agent_rows)];
        if show_editor {
            constraints.push(Constraint::Length(4));
        }
        constraints.push(Constraint::Min(6)); // chat + message box
        constraints.push(Constraint::Length(bar_rows));
        let rows = Layout::vertical(constraints).split(area);
        let mut index = 0;
        draw_agents(frame, app, rows[index]);
        index += 1;
        if show_editor {
            draw_editor(frame, app, rows[index]);
            index += 1;
        }
        draw_chat(frame, app, rows[index]);
        draw_status(frame, app, rows[rows.len() - 1]);
    } else {
        let rows = Layout::vertical([
            Constraint::Min(6),
            Constraint::Percentage(45),
            Constraint::Length(bar_rows),
        ])
        .split(area);
        // On a very wide terminal the tree stops growing: past a point it is
        // empty space, and the editor is what the width belongs to.
        let agents_pane = if area.width >= 160 {
            Constraint::Length(34)
        } else {
            Constraint::Percentage(26)
        };
        let columns = Layout::horizontal([agents_pane, Constraint::Min(20)]).split(rows[0]);
        draw_agents(frame, app, columns[0]);
        draw_editor(frame, app, columns[1]);
        draw_chat(frame, app, rows[1]);
        draw_status(frame, app, rows[2]);
    }
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
        .title(agents_title(app));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 || app.agents.is_empty() {
        return;
    }

    // The cursor row's facts live in a footer, so the list may degrade to
    // `◐ #2` on a narrow pane without losing anything: it moves, not vanishes.
    let footer_rows = if inner.height >= 6 {
        3.min(inner.height - 2)
    } else {
        0
    };
    let list_area = Rect {
        height: inner.height - footer_rows,
        ..inner
    };

    // `List` draws `› ` outside the item's width, so the selected row would be
    // two columns narrower than its neighbours. Budget for it up front.
    let row_width = (inner.width as usize).saturating_sub(2);
    let items: Vec<ListItem> = app
        .agents
        .iter()
        .map(|node| ListItem::new(agent_line(app, node, row_width)))
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
        .highlight_symbol("› ");
    let mut state = ListState::default();
    let cursor = app.agent_cursor.min(app.agents.len().saturating_sub(1));
    state.select(Some(cursor));
    frame.render_stateful_widget(list, list_area, &mut state);

    if footer_rows > 0 {
        let node = &app.agents[cursor];
        let lines = agent_footer(app, node, inner.width as usize);
        let start = inner.y + inner.height - lines.len() as u16;
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                dim(),
            ))),
            Rect::new(inner.x, start - 1, inner.width, 1),
        );
        for (offset, line) in lines.into_iter().enumerate() {
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, start + offset as u16, inner.width, 1),
            );
        }
    }
}

/// `agents · 2 running · Σ +324 −40`: what the whole tree is doing, and how
/// much its branches carry.
fn agents_title(app: &App) -> String {
    let mut title = String::from(" agents ");
    let busy = app
        .agents
        .iter()
        .filter(|node| node.phase.is_busy())
        .count();
    if busy > 0 {
        title.push_str(&format!(" · {busy} running"));
    }
    let mut added = 0;
    let mut removed = 0;
    for stat in app.agent_stats.values() {
        added += stat.added;
        removed += stat.removed;
    }
    if added + removed > 0 {
        title.push_str(&format!(" · Σ +{added} −{removed}"));
    }
    title
}

/// One tree row, with the fields it can afford.
///
/// The row answers "what is happening": activity, branch and line delta survive
/// as long as there is any room, and the brief — which the footer and the
/// transcript carry in full — is what yields first.
fn agent_line(app: &App, node: &AgentNode, width: usize) -> String {
    let indent = "  ".repeat(node.depth);
    let marker = if app.focused == node.id { "▶" } else { " " };
    let waiting = app
        .agents
        .iter()
        .any(|n| n.parent == Some(node.id) && n.phase.is_busy());
    let head = format!(
        "{indent}{marker}{} #{:<3}",
        phase_glyph(&node.phase, waiting),
        node.id
    );

    let mut tail = Vec::new();
    let activity = phase_detail(node);
    if !activity.is_empty() {
        tail.push(activity);
    }
    let mut where_and_how = node.branch.clone().unwrap_or_default();
    if let Some(stat) = app.agent_stats.get(&node.id) {
        if !stat.is_empty() {
            if !where_and_how.is_empty() {
                where_and_how.push(' ');
            }
            where_and_how.push_str(&stat.compact());
        }
    }
    fit_row(&head, &node.brief, &where_and_how, &tail, width)
}

/// Lay out one row in the width it has.
///
/// The row answers "what is happening": the state (glyph, id) is never
/// sacrificed, then the branch and line delta — facts that exist nowhere else on
/// the screen — then the brief, then the activity, which the bar already repeats
/// for the focused agent. Fields are dropped from the right when the pane is
/// narrow, and the cursor row's full facts are one row below in the footer.
fn fit_row(head: &str, brief: &str, branch_stat: &str, tail: &[String], width: usize) -> String {
    let head_width = UnicodeWidthStr::width(head);
    if width <= head_width + 2 {
        return head.to_string();
    }
    let budget = width - head_width - 1;
    let branch_width = UnicodeWidthStr::width(branch_stat);
    let show_branch = branch_width > 0 && branch_width + 2 <= budget.saturating_sub(4);
    let after_branch = budget.saturating_sub(if show_branch { branch_width + 2 } else { 0 });

    let mut line = head.to_string();
    let mut remaining = budget;
    if after_branch >= 7 && !brief.is_empty() {
        let text = truncate(brief, after_branch - 1);
        // `truncate` counts characters, not columns: a wide glyph can make the
        // text one column wider than asked for, so never subtract past zero.
        remaining = remaining.saturating_sub(UnicodeWidthStr::width(text.as_str()) + 1);
        line.push(' ');
        line.push_str(&text);
    }
    if show_branch {
        line.push_str("  ");
        line.push_str(branch_stat);
        remaining = remaining.saturating_sub(branch_width + 2);
    }
    for cell in tail {
        let cell_width = UnicodeWidthStr::width(cell.as_str());
        if remaining < cell_width + 2 {
            break;
        }
        line.push_str("  ");
        line.push_str(cell);
        remaining -= cell_width + 2;
    }
    line.trim_end().to_string()
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
    if node.branch.is_some() || matches!(node.phase, Phase::Idle) {
        let mut detail = Vec::new();
        if let Some(branch) = &node.branch {
            // Where the work is, and the two commands that land it: this is the
            // one place on screen that says an isolated agent exists at all.
            detail.push(format!(".mush/wt/{}", node.id));
            detail.push(format!("git diff HEAD...{branch}"));
            detail.push(format!("/merge {}", node.id));
        }
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
        let _ = app;
    }
    lines
}

/// The glyph is derived from the phase and the tree, never stored: an agent is
/// `·` until it does something, `✓` only when a run finished, `⏸` when it is
/// busy *because* its children are, and `⊘` while a cancel is in flight.
fn phase_glyph(phase: &Phase, waiting_on_children: bool) -> &'static str {
    match phase {
        Phase::Failed(_) => "✗",
        Phase::Cancelling => "⊘",
        Phase::Idle => "·",
        Phase::Done => "✓",
        Phase::Thinking | Phase::Activity(_) => {
            if waiting_on_children {
                "⏸"
            } else {
                "◐"
            }
        }
    }
}

/// What the row says the agent is doing, ageing with the phase so a slow model
/// is visible as `thinking 42s` rather than a static word.
fn phase_detail(node: &AgentNode) -> String {
    let age = short_age(node.since.elapsed());
    match &node.phase {
        Phase::Thinking => format!("thinking {age}"),
        Phase::Activity(what) => format!("{what} {age}"),
        Phase::Cancelling => "cancelling…".to_string(),
        Phase::Failed(error) => error.clone(),
        Phase::Idle | Phase::Done => node.summary.clone().unwrap_or_default(),
    }
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
            Paragraph::new(
                "No file open — /open <path> (no path: pick one), or start mush with a file.",
            )
            .style(dim()),
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
            .map(|c| {
                if c == '\t' {
                    "    ".to_string()
                } else {
                    c.to_string()
                }
            })
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
    let rows = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).split(area);

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
        // A 200-column transcript is not read, it is skimmed. Cap the measure
        // and leave the rest as margin.
        let width = (inner.width as usize).min(MAX_TRANSCRIPT as usize);
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
                + UnicodeWidthStr::width(app.input.as_str())) as u16;
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
    // A terminal this short has no room for a list; the hint line and the
    // window below both need at least one row.
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Window the list so many items never overflow the popup.
    let visible = inner.height.saturating_sub(1) as usize;
    let start = picker.cursor.saturating_sub(visible / 2);
    let mut items = Vec::new();
    for item in picker.items.iter().skip(start).take(visible) {
        let current = match picker.kind {
            PickerKind::Model => picker
                .items
                .get(picker.cursor)
                .map(|item| item.split(" · ").next().unwrap_or(item) == app.cfg.model)
                .unwrap_or(false),
            PickerKind::Provider => item == app.cfg.provider.name(),
            PickerKind::File => app.current_rel() == Some(item.as_str()),
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

    let hint = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(" Enter pick · Esc cancel ", dim()))),
        hint,
    );
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let focus = match app.focus {
        Focus::Agents => "agents",
        Focus::Editor => match app.mode {
            Mode::Normal => "editor · normal",
            Mode::Insert => "editor · insert",
        },
        Focus::Chat => "chat",
    };
    // Priority: what the tree is doing (derived) › what just happened (fades) ›
    // the static hint. Work in progress is never stored, so it cannot linger.
    let (message, style) = match (app.activity_line(), app.status_line()) {
        (Some(activity), _) => (activity, Style::default().fg(Color::Cyan)),
        (None, Some((text, StatusKind::Error))) => {
            (text.to_string(), Style::default().fg(Color::Red))
        }
        (None, Some((text, StatusKind::Info))) => {
            (text.to_string(), Style::default().fg(Color::Gray))
        }
        (None, None) => (
            "Tab cycles panes · /help lists commands · Ctrl-P picks a model".to_string(),
            dim(),
        ),
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" {focus} "),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ),
        Span::raw(" "),
        Span::styled(message, style),
    ]);
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    frame.render_widget(Paragraph::new(line), rows[0]);
    if area.height > 1 {
        // The facts line: where this is, what it is on, how much has moved.
        // Elided from the right, so the repository survives longest and the
        // hints go first.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                facts_line(app, area.width as usize),
                dim(),
            ))),
            rows[1],
        );
    }
}

/// `⌂ ~/p/demo │ master ±3 +12 −3 │ deepseek-flash · ctx ~500k │ /help` — the
/// stable facts, in the order that matters, cut from the right when the
/// terminal is narrow.
fn facts_line(app: &App, width: usize) -> String {
    let root = app.ws.root_str();
    let home = std::env::var("HOME").unwrap_or_default();
    let shown = if !home.is_empty() && root.starts_with(&home) {
        format!("~{}", &root[home.len()..])
    } else {
        root
    };
    let mut cells = vec![format!(" ⌂ {shown}")];
    if let Some(git) = &app.git {
        let branch = if git.branch.is_empty() {
            "detached".to_string()
        } else {
            git.branch.clone()
        };
        let mut cell = branch;
        if git.dirty > 0 {
            cell.push_str(&format!(" ±{}", git.dirty));
        }
        if !git.stat.is_empty() {
            cell.push_str(&format!(" {}", git.stat.compact()));
        }
        cells.push(cell);
    }
    cells.push(format!("{} · {}", app.cfg.label(), app.context_label()));
    while cells.len() > 1 {
        let joined: String = cells.join(" │ ");
        if UnicodeWidthStr::width(joined.as_str()) <= width {
            break;
        }
        cells.pop();
    }
    cells.join(" │ ")
}

fn transcript_lines(app: &App, messages: &[Message], width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if app.focused == 0 {
        if messages.is_empty() && app.notices.is_empty() {
            out.push(Line::from(Span::styled(
                "Ask for a change — the agent reads and edits this workspace directly.",
                dim(),
            )));
            out.push(Line::from(""));
            out.push(Line::from(Span::styled(app.cfg.label(), dim())));
            out.push(Line::from(Span::styled(
                "Tab cycles panes · Enter sends · /open <file> edits · /help lists commands",
                dim(),
            )));
            return out;
        }
    } else if messages.is_empty() {
        out.push(Line::from(Span::styled(
            format!(
                "Agent #{} has no messages yet — typing here sends it a nudge.",
                app.focused
            ),
            dim(),
        )));
        return out;
    }

    for message in messages {
        render_message(&mut out, message, width);
    }
    for notice in &app.notices {
        let (prefix, style) = match notice.kind {
            NoticeKind::Info => ("·", dim()),
            NoticeKind::Error => ("!", Style::default().fg(Color::Red)),
        };
        for line in wrap_text(&notice.text, width.saturating_sub(2)) {
            out.push(Line::from(Span::styled(format!("{prefix} {line}"), style)));
        }
    }
    // The spinner belongs to the transcript on screen: another agent working
    // elsewhere is not this conversation's business.
    let focused_busy = app
        .agents
        .iter()
        .find(|node| node.id == app.focused)
        .map(|node| node.phase.is_busy())
        .unwrap_or(false);
    if focused_busy {
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
                // `agent::summarize_args` is the same reading the tree shows:
                // `edit_file src/lex.rs`, not forty lines of JSON.
                let label = format!(
                    "  ⚙ {} {}",
                    call.function.name,
                    truncate(&crate::agent::summarize_args(&call.function.arguments), 60)
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

/// Shorten to at most `max` characters *including* the ellipsis, so callers can
/// budget columns with it (the old contract silently returned `max + 1`).
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row's glyph is the whole status vocabulary in one character; it must
    /// never claim a run that did not happen (`·`, not `✓`).
    #[test]
    fn glyphs_are_truthful() {
        assert_eq!(phase_glyph(&Phase::Idle, false), "·");
        assert_eq!(phase_glyph(&Phase::Thinking, false), "◐");
        assert_eq!(phase_glyph(&Phase::Thinking, true), "⏸");
        assert_eq!(
            phase_glyph(&Phase::Activity("edit_file a.rs".into()), true),
            "⏸"
        );
        assert_eq!(phase_glyph(&Phase::Cancelling, false), "⊘");
        assert_eq!(phase_glyph(&Phase::Done, false), "✓");
        assert_eq!(phase_glyph(&Phase::Failed("boom".into()), false), "✗");
    }

    /// The detail line carries the age of the *phase*, so a slow model looks
    /// slow instead of looking stuck.
    #[test]
    fn details_age_with_the_phase() {
        let node = |phase: Phase, age: u64| AgentNode {
            id: 2,
            parent: None,
            depth: 0,
            brief: "lexer".to_string(),
            phase,
            since: std::time::Instant::now() - std::time::Duration::from_secs(age),
            branch: None,
            summary: None,
        };
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
    }

    /// A row gives up its least useful field first: the activity goes before
    /// the branch, the branch before the brief, and the state never goes.
    #[test]
    fn a_row_gives_up_its_brief_before_its_facts() {
        let head = "▶◐ #2  ";
        let activity = ["write deep.txt 3s".to_string()];
        let wide = fit_row(head, "create a file", "mush/2 +8−0", &activity, 70);
        assert_eq!(
            wide,
            "▶◐ #2   create a file  mush/2 +8−0  write deep.txt 3s"
        );

        // Narrow: the activity goes, the branch and stat stay.
        let narrow = fit_row(head, "create a file", "mush/2 +8−0", &activity, 34);
        assert!(narrow.contains("mush/2 +8−0"), "{narrow}");
        assert!(!narrow.contains("write deep.txt"), "{narrow}");

        // Narrower: the brief yields too, the branch still stays.
        let tighter = fit_row(head, "create a file", "mush/2 +8−0", &activity, 26);
        assert!(tighter.contains("mush/2 +8−0"), "{tighter}");
        assert!(!tighter.contains("create"), "{tighter}");

        // Narrowest: the state alone, which is never dropped (the row is
        // trimmed, so the padded id loses its trailing spaces).
        assert_eq!(
            fit_row(head, "create a file", "mush/2 +8−0", &activity, 10),
            head.trim_end()
        );
    }

    #[test]
    fn wraps_on_word_boundaries() {
        let lines = wrap_text("the quick brown fox jumps", 10);
        assert!(lines.iter().all(|line| line.chars().count() <= 10));
        assert_eq!(lines.concat().replace(' ', ""), "thequickbrownfoxjumps");
    }

    #[test]
    fn preserves_newlines() {
        assert_eq!(
            wrap_text("a\nb", 10),
            vec!["a".to_string(), "b".to_string()]
        );
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
