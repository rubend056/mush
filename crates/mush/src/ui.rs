//! Rendering. This module is deliberately dumb: it reads `App` and paints it.
//! No state transitions live here, which keeps the update logic testable.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use mush_core::git;
use mush_core::message::Message;
use mush_core::text::{fit_row, truncate, wrap_text, wrap_text_capped};

use crate::app::{
    short_age, AgentNode, App, Focus, Landed, NoticeKind, Phase, PickerKind, StatusKind,
};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// The idle bar hint, when there is nothing to report.
const HINT: &str = "Tab cycles panes · /help lists commands · Ctrl-P picks a model";
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
    // agent strip above the chat, because two columns starve both panes.
    let compact = area.width < 80 || area.height < 20;
    let bar_rows = if area.height >= 26 { 2 } else { 1 };

    if compact {
        let agent_rows = (app.agents.len() as u16 + 2).clamp(3, 6);
        let rows = Layout::vertical([
            Constraint::Length(agent_rows),
            Constraint::Min(6), // chat transcript + message box
            Constraint::Length(bar_rows),
        ])
        .split(area);
        draw_agents(frame, app, rows[0]);
        draw_chat(frame, app, rows[1]);
        draw_status(frame, app, rows[2]);
    } else {
        let rows = Layout::vertical([Constraint::Min(6), Constraint::Length(bar_rows)]).split(area);
        // On a very wide terminal the tree stops growing: past a point it is
        // empty space, and the chat is what the width belongs to.
        let agents_pane = if area.width >= 160 {
            Constraint::Length(34)
        } else {
            Constraint::Percentage(26)
        };
        let columns = Layout::horizontal([agents_pane, Constraint::Min(20)]).split(rows[0]);
        draw_agents(frame, app, columns[0]);
        draw_chat(frame, app, columns[1]);
        draw_status(frame, app, rows[1]);
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
    if node.landed.is_some()
        || node.branch.is_some()
        || matches!(node.phase, Phase::Idle | Phase::Stopped)
    {
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
        let _ = app;
    }
    lines
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

/// The glyph is derived from the phase and the tree, never stored: an agent is
/// `·` until it does something, `✓` only when a run finished, `⏸` when it is
/// busy *because* its children are, and `⊘` while a cancel is in flight.
fn phase_glyph(phase: &Phase, waiting_on_children: bool) -> &'static str {
    match phase {
        Phase::Failed(_) => "✗",
        // `⊘` while a cancel is in flight and after it lands: a stopped agent
        // is not a finished one, and must not borrow `✓`.
        Phase::Cancelling | Phase::Stopped => "⊘",
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
        // A stopped run has no result to show: its last summary belongs to a
        // run that was interrupted, so showing it would claim work that was
        // never delivered. `node.summary` is deliberately not consulted.
        Phase::Stopped => "stopped · re-send to resume".to_string(),
        Phase::Failed(error) => error.clone(),
        Phase::Idle | Phase::Done => node.summary.clone().unwrap_or_default(),
    }
}

fn draw_chat(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Chat;
    // The box grows with the message: a multi-line draft has to be visible, not
    // hidden behind a one-line window. It stops growing so the transcript keeps
    // the screen.
    const MAX_INPUT_LINES: u16 = 6;
    let input_lines = (app.input.line_count() as u16).clamp(1, MAX_INPUT_LINES);
    let rows =
        Layout::vertical([Constraint::Min(3), Constraint::Length(input_lines + 2)]).split(area);

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
        // Render only the lines the window can show, counting from the bottom —
        // which is where the transcript is anchored. Building the whole
        // scrollback to display forty lines cost 55 ms a frame on a long
        // session, and `tick` repaints every frame while an agent works.
        let want = height + app.chat_scroll;
        let mut lines = transcript_tail(app, messages, width, want);
        trim_trailing_blanks(&mut lines);
        let start = if lines.len() >= want {
            // There is more above, so what we rendered already *is* the window.
            0
        } else {
            // The whole transcript fits: the original top-index arithmetic.
            let max_scroll = lines.len().saturating_sub(height);
            max_scroll.saturating_sub(app.chat_scroll.min(max_scroll))
        };
        let visible: Vec<Line> = lines.into_iter().skip(start).take(height).collect();
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
        let prompt_width = UnicodeWidthStr::width(prompt.as_str());
        let field = (input_inner.width as usize).saturating_sub(prompt_width);
        // The box scrolls with the cursor instead of clipping its tail: what
        // the human is editing is always the part on screen. Multi-line drafts
        // are painted line by line, so the cursor's own line is the one kept in
        // view.
        let (lines, cursor_row, column) = app.input.view(input_inner.height as usize, field);
        let mut rendered: Vec<Line> = Vec::with_capacity(lines.len());
        for (index, line) in lines.into_iter().enumerate() {
            let (lead, style) = if index == 0 {
                (prompt.clone(), Style::default().fg(Color::Cyan))
            } else {
                // Continuation lines line up under the first, so the prompt
                // reads as a margin rather than as part of the message.
                (" ".repeat(prompt_width), Style::default())
            };
            rendered.push(Line::from(vec![Span::styled(lead, style), Span::raw(line)]));
        }
        frame.render_widget(Paragraph::new(Text::from(rendered)), input_inner);
        if focused {
            let x = input_inner.x
                + ((prompt_width + column).min(input_inner.width.saturating_sub(1) as usize)
                    as u16);
            let y = input_inner.y + (cursor_row as u16).min(input_inner.height.saturating_sub(1));
            frame.set_cursor_position(Position::new(x, y));
        }
    }
}

/// Every message ends with a blank separator line. At one row of transcript that
/// blank would be the only visible line — the reply would be invisible — so the
/// separator is trimmed before windowing (finding B4).
fn trim_trailing_blanks(lines: &mut Vec<Line<'static>>) {
    while lines.last().map(|line| line.width()) == Some(0) {
        lines.pop();
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
        Focus::Chat => "chat",
    };
    // Priority: a failure first (the Ctrl-Q warning included, so it is never
    // hidden behind work in progress), then what the tree is doing (derived),
    // then what just happened (fades), then the static hint.
    let (message, style) = bar_line(app.status_line(), app.activity_line());
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

/// What the bar's first line says, and how it looks. Pure so the priority is
/// testable without a frame: an error must never lose to work in progress
/// (finding B12 — the Ctrl-Q warning included).
fn bar_line(status: Option<(&str, StatusKind)>, activity: Option<String>) -> (String, Style) {
    match status {
        Some((text, StatusKind::Error)) => (text.to_string(), Style::default().fg(Color::Red)),
        _ => match activity {
            Some(activity) => (activity, Style::default().fg(Color::Cyan)),
            None => match status {
                Some((text, _)) => (text.to_string(), Style::default().fg(Color::Gray)),
                None => (HINT.to_string(), dim()),
            },
        },
    }
}

/// `⌂ ~/p/demo │ master ±3 +12 −3 │ deepseek-flash · ctx ~500k │ /help` — the
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

/// The last `want` transcript lines, rendered from the bottom up.
///
/// The window is anchored at the bottom, so rendering forwards from the first
/// message meant building the entire scrollback — every tool result re-wrapped —
/// to paint about forty lines. This walks backwards and stops once it has
/// enough, which is O(visible) for a normal session.
fn transcript_tail(
    app: &App,
    messages: &[Message],
    width: usize,
    want: usize,
) -> Vec<Line<'static>> {
    // Notices are tagged with the agent they concern, so a root-level failure
    // is not painted into a focused child's transcript (finding B19).
    let notices: Vec<&crate::app::Notice> = app
        .notices
        .iter()
        .filter(|notice| notice.agent == app.focused)
        .collect();
    if app.focused == 0 {
        if messages.is_empty() && notices.is_empty() {
            return vec![
                Line::from(Span::styled(
                    "Ask for a change — the agent reads and edits this workspace directly.",
                    dim(),
                )),
                Line::from(""),
                Line::from(Span::styled(app.cfg.label(), dim())),
                Line::from(Span::styled(
                    "Tab cycles panes · Enter sends · /help lists commands",
                    dim(),
                )),
            ];
        }
    } else if messages.is_empty() {
        return vec![Line::from(Span::styled(
            format!(
                "Agent #{} has no messages yet — typing here sends it a nudge.",
                app.focused
            ),
            dim(),
        ))];
    }

    // Collected back to front, then reversed: each chunk is one message's or
    // one notice's lines in their own order.
    let mut chunks: Vec<Vec<Line<'static>>> = Vec::new();
    let mut count = 0usize;

    // What is painted last is collected first.
    let focused_busy = app
        .agents
        .iter()
        .find(|node| node.id == app.focused)
        .map(|node| node.phase.is_busy())
        .unwrap_or(false);
    if focused_busy {
        chunks.push(vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("{} working…", SPINNER[(app.spin as usize) % SPINNER.len()]),
                Style::default().fg(Color::Cyan),
            )),
        ]);
        count += 2;
    }

    for notice in notices.iter().rev() {
        if count >= want {
            break;
        }
        let (prefix, style) = match notice.kind {
            NoticeKind::Info => ("·", dim()),
            NoticeKind::Error => ("!", Style::default().fg(Color::Red)),
        };
        let mut chunk = Vec::new();
        for line in wrap_text(&notice.text, width.saturating_sub(2)) {
            chunk.push(Line::from(Span::styled(format!("{prefix} {line}"), style)));
        }
        count += chunk.len();
        chunks.push(chunk);
    }

    for message in messages.iter().rev() {
        if count >= want {
            break;
        }
        let mut chunk = Vec::new();
        render_message(&mut chunk, message, width);
        count += chunk.len();
        chunks.push(chunk);
    }

    let mut out = Vec::with_capacity(count);
    for chunk in chunks.into_iter().rev() {
        out.extend(chunk);
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
            // Only the first eight lines are ever shown, so only those are
            // wrapped; the ninth is what tells us to print the `…`. Wrapping
            // the whole result was most of a frame's cost on a long session.
            const SHOWN: usize = 8;
            let wrapped = wrap_text_capped(message.text(), width.saturating_sub(2), SHOWN + 1);
            let clipped = wrapped.len() > SHOWN;
            for line in wrapped.iter().take(SHOWN) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_outranks_activity() {
        let (text, _) = bar_line(
            Some(("cannot reach http://127.0.0.1:1", StatusKind::Error)),
            Some("#0 thinking 3s".to_string()),
        );
        assert_eq!(
            text, "cannot reach http://127.0.0.1:1",
            "an error is visible"
        );

        let (text, _) = bar_line(
            Some(("opened notes.txt", StatusKind::Info)),
            Some("#0 thinking 3s".to_string()),
        );
        assert_eq!(text, "#0 thinking 3s", "activity beats a fading info line");

        let (text, _) = bar_line(Some(("opened notes.txt", StatusKind::Info)), None);
        assert_eq!(text, "opened notes.txt");

        let (text, _) = bar_line(None, None);
        assert!(text.contains("/help"), "{text}");
    }

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
        // A stopped agent is not a finished one, and must not borrow the tick.
        assert_eq!(phase_glyph(&Phase::Stopped, false), "⊘");
    }

    /// The detail line carries the age of the *phase*, so a slow model looks
    /// slow instead of looking stuck.
    /// A node carrying nothing but the facts a row test needs.
    fn node(phase: Phase, age: u64) -> AgentNode {
        AgentNode {
            id: 2,
            parent: None,
            depth: 0,
            brief: "lexer".to_string(),
            phase,
            since: std::time::Instant::now() - std::time::Duration::from_secs(age),
            branch: None,
            summary: None,
            leftover: false,
            landed: None,
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
}
