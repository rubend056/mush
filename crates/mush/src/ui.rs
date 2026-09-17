//! Rendering. This module is deliberately dumb: it reads `App` and paints it.
//! No state transitions live here, which keeps the update logic testable.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use mush_core::git;
use mush_core::text::{fit_row, truncate};

use crate::app::{
    short_age, AgentId, AgentNode, App, Focus, Landed, Pane, Phase, PickerKind, Rank, StatusKind,
};

/// The idle bar hint, when there is nothing to report. The commands it names
/// are checked against `app::commands::COMMANDS` by a test there, so the bar
/// cannot advertise a command the parser does not have (finding B2).
pub(crate) const HINT: &str = "Tab cycles panes · /help lists commands · Ctrl-P picks a model";
/// Beyond this the transcript is unreadable, however wide the terminal is.
const MAX_TRANSCRIPT: u16 = 110;
/// How many columns the agent pane is given, and why it is a length rather than
/// a share of the terminal.
///
/// R1's row spends its fields left to right — `state · branch +delta · what it
/// is doing · its title` — and that is about forty columns of real labels. Below
/// thirty the chat is the better use of a narrow screen; past forty-six the tree
/// has nothing else to put there (a tool label is the widest field it has) while
/// a wider terminal is what the transcript's measure is for (it is capped at 110
/// columns anyway). The share this replaced was 26%, which is 31 columns at 120:
/// `▶◐ #0` left 22 for a 23-column `edit_file src/lib.rs 12s`, so a busy agent's
/// tool call and its age were dropped there — every frame, on the size the
/// audit photographs.
const AGENTS_MIN_COLUMNS: u16 = 30;
const AGENTS_MAX_COLUMNS: u16 = 46;
/// The chat below this is a column of broken words, whatever the tree wants.
const CHAT_MIN_COLUMNS: u16 = 40;
/// Below this mush has no room to be honest: say so instead of painting shreds.
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 10;

/// The popup the pickers paint in: a share of the terminal, floored so a model
/// list is readable and capped so it does not sprawl on a wide one. One formula,
/// because `draw_picker` sizes the popup with it and `picker_text_width` says
/// how much of it a `/notes` row may use.
const PICKER_MIN_WIDTH: u16 = 40;
const PICKER_MAX_WIDTH: u16 = 80;

fn picker_width(terminal_width: u16) -> u16 {
    (terminal_width * 60 / 100).clamp(PICKER_MIN_WIDTH, PICKER_MAX_WIDTH)
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

/// The columns the picker's list gives one item's text. The term carries the
/// popup's two border columns, the two the `› ` symbol reserves, and the two the
/// `  ` indent every row wears. `App` wraps a `/notes` report to this, so the
/// lines it hands the list already fit the width they are painted at — wrapping
/// to any other width (the old fixed 74) is how the documented escape hatch
/// clipped.
pub(crate) fn picker_text_width(terminal_width: u16) -> usize {
    picker_width(terminal_width).saturating_sub(6) as usize
}

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
        // Every pane's height is computed here, and every constraint is a
        // `Length`, so the three add up to the terminal exactly and none of
        // them can lose rows to another. The bar used to be a trailing
        // `Length` behind a `Min(6)` chat, and at 40×10 the chat took the row
        // the bar was owed: the frame painted the tree, the transcript and the
        // message box, and the ` chat ` row — the focus badge, the key hint,
        // and the only home an Info line or a command's usage error has — was
        // simply absent.
        // Six is the least the chat can be and still hold what it is for: a
        // three-row transcript over a message box that has a row to type in.
        // The box lost that row instead after the bar's floor was added, which
        // is the same defect one pane over (§4.5's audit, defect 7).
        let chat_min = 6;
        let agent_rows = (app.tree.agents.len() as u16 + 2)
            .clamp(3, 6)
            .min(area.height.saturating_sub(bar_rows + chat_min));
        let chat_rows = area.height - agent_rows - bar_rows;
        let rows = Layout::vertical([
            Constraint::Length(agent_rows),
            Constraint::Length(chat_rows),
            Constraint::Length(bar_rows),
        ])
        .split(area);
        draw_agents(frame, app, rows[0]);
        draw_chat(frame, app, rows[1]);
        draw_status(frame, app, rows[2]);
    } else {
        let rows = Layout::vertical([
            Constraint::Length(area.height - bar_rows),
            Constraint::Length(bar_rows),
        ])
        .split(area);
        // On a very wide terminal the tree stops growing: past a point it is
        // empty space, and the chat is what the width belongs to. Below that it
        // gets the columns R1's row needs, so the fields the row is built from
        // are the fields it can paint.
        let agents_pane = Constraint::Length(agents_columns(area.width));
        let columns = Layout::horizontal([agents_pane, Constraint::Min(20)]).split(rows[0]);
        draw_agents(frame, app, columns[0]);
        draw_chat(frame, app, columns[1]);
        draw_status(frame, app, rows[1]);
    }
    draw_picker(frame, app);
}

pub(crate) fn dim() -> Style {
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
        .title(agents_title(app, area.width.saturating_sub(2) as usize));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 || app.tree.agents.is_empty() {
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
    // The rows in painted order: pre-order over the parent links, so a child is
    // drawn under its parent rather than after everything spawned before it
    // (finding U4). The tree derives that order, this only paints it.
    let rows = app.tree.rows();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|node| ListItem::new(agent_line(app, node, row_width)))
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
        .highlight_symbol("› ");
    let mut state = ListState::default();
    let cursor = app.tree.cursor();
    state.select(Some(cursor));
    frame.render_stateful_widget(list, list_area, &mut state);

    if footer_rows > 0 {
        let node = rows[cursor];
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

/// ` agents · 3 working · 2 waiting · Σ +324 −40`: what the whole tree is doing,
/// and how much its branches carry.
///
/// Every clause is a count of the phases, named for what it counts, and no
/// agent is in two of them: `N working` is the agents whose own run is in
/// flight, `M waiting` the ones at rest with children working (the `⏸` rows),
/// and the totals are the branches'. It used to say `N running` over a number
/// that included the napping ones, which is how the title came to contradict
/// the rows under it (finding U2).
///
/// The clauses are ranked and dropped whole from the right while they do not
/// fit — the way `facts_line` elides — because this pane is 32 columns wide at
/// its widest and a clause cut mid-number (`Σ +324 −`, `2 waitin`) is a count
/// that is not the count. The totals are last because the least is lost last:
/// every branch's own `+add −del` is on its row and in the selected row's
/// footer, while who is working exists only here.
fn agents_title(app: &App, width: usize) -> String {
    let roster = app.tree.roster();
    let mut cells = Vec::new();
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
    while !cells.is_empty() {
        let joined = format!(" agents · {}", cells.join(" · "));
        if UnicodeWidthStr::width(joined.as_str()) <= width {
            return joined;
        }
        cells.pop();
    }
    " agents ".to_string()
}

/// One tree row, with the fields it can afford.
///
/// The row answers "what is happening": activity, branch and line delta survive
/// as long as there is any room, and the brief — which the footer and the
/// transcript carry in full — is what yields first.
fn agent_line(app: &App, node: &AgentNode, width: usize) -> String {
    let indent = "  ".repeat(node.depth);
    let marker = if app.tree.focused == node.id {
        "▶"
    } else {
        " "
    };
    let waiting = app.tree.busy_children(node.id);
    // Two facts, two marks: `glyph · id` is this agent's own phase, and `⏸N`
    // counts the children that are working. The old row derived the glyph from
    // "has live children", so a busy agent wore `⏸` and its own work vanished
    // from the screen (finding U1).
    let mut head = format!(
        "{indent}{marker}{glyph} #{id}",
        id = node.id,
        glyph = phase_glyph(&node.phase),
    );
    if waiting > 0 {
        // R4's `⏸`, owned by the children it is about: the parent's own state
        // stays in the glyph, and this says how much it has out.
        head.push_str(&format!(" ⏸{waiting}"));
    }

    let mut tail = Vec::new();
    let activity = phase_detail(node);
    if !activity.is_empty() {
        tail.push(activity);
    }
    let mut where_and_how = node.branch.clone().unwrap_or_default();
    if let Some(stat) = app.tree.agent_stats.get(&node.id) {
        if !stat.is_empty() {
            if !where_and_how.is_empty() {
                where_and_how.push(' ');
            }
            where_and_how.push_str(&stat.compact());
        }
    }
    // The jobs on this machine, on the row of whoever started them. It rides
    // with the branch and the stat — facts that exist nowhere else on the
    // screen — because the human should not have to ask a model what is
    // running; the count is derived from the registry every frame, never
    // stored, and the selected row's footer names each job.
    let jobs = app.live_jobs(node.id).len();
    if jobs > 0 {
        if !where_and_how.is_empty() {
            where_and_how.push(' ');
        }
        where_and_how.push_str(&format!("⚙{jobs}"));
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
/// a run finished, `✗` when it failed.
///
/// Waiting on children is a *different fact* from working and is drawn as a
/// different mark (`agent_line`'s `⏸N`), because a parent that is mid-turn with
/// children running is working, not paused — the row that said `⏸` about it was
/// claiming a park that never happened (finding U1).
fn phase_glyph(phase: &Phase) -> &'static str {
    match phase {
        Phase::Failed(_) => "✗",
        // `⊘` while a cancel is in flight and after it lands: a stopped agent
        // is not a finished one, and must not borrow `✓`.
        Phase::Cancelling | Phase::Stopped => "⊘",
        Phase::Idle => "·",
        Phase::Done => "✓",
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
    let input_lines = (app.chat.input().line_count() as u16).clamp(1, MAX_INPUT_LINES);
    let rows =
        Layout::vertical([Constraint::Min(3), Constraint::Length(input_lines + 2)]).split(area);

    // The pane's own title, not one built here: a pane with no room for the
    // foot's count line says what it is hiding in the title instead, and that
    // is arithmetic about the conversation, not about the frame.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border(focused));
    let inner = block.inner(rows[0]);

    if inner.height > 0 && inner.width > 0 {
        // A 200-column transcript is not read, it is skimmed. Cap the measure
        // and leave the rest as margin.
        let width = (inner.width as usize).min(MAX_TRANSCRIPT as usize);
        let height = inner.height as usize;
        let label = app.cfg().label();
        let pane = Pane {
            agent: app.tree.focused,
            // A run in flight is what the pane's own activity line is derived
            // from, and the spinner is the frame `App::tick` advanced.
            //
            // A run parked in a wait is *not* one: the foot's `working…` may
            // only claim a model call, and `wait_agents` is not one — the
            // agent is waiting for somebody else's result, and the row says so
            // (`waiting on agents 3s`). Painting the spinner over that was
            // exactly the lie finding U7 named.
            busy: app
                .tree
                .node(app.tree.focused)
                .map(|node| node.phase.is_busy() && node.phase.waiting().is_none())
                .unwrap_or(false),
            spin: app.spin,
            label: &label,
        };
        // Only the rows the window can show are built — the whole scrollback to
        // display forty lines cost 55 ms a frame on a long session, and `tick`
        // repaints every frame while an agent works.
        let painted = app.chat.painted(&pane, width, height);
        frame.render_widget(block.title(painted.title), rows[0]);
        frame.render_widget(Paragraph::new(Text::from(painted.lines)), inner);
    } else {
        frame.render_widget(block, rows[0]);
    }

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(border(focused))
        .title(" message ");
    let input_inner = input_block.inner(rows[1]);
    frame.render_widget(input_block, rows[1]);

    if input_inner.height > 0 && input_inner.width > 0 {
        let prompt = if app.tree.focused == AgentId::ROOT {
            "› ".to_string()
        } else {
            format!("#{} › ", app.tree.focused)
        };
        let prompt_width = UnicodeWidthStr::width(prompt.as_str());
        let field = (input_inner.width as usize).saturating_sub(prompt_width);
        // The box scrolls with the cursor instead of clipping its tail: what
        // the human is editing is always the part on screen. Multi-line drafts
        // are painted line by line, so the cursor's own line is the one kept in
        // view.
        let (lines, cursor_row, column) = app.chat.input().view(input_inner.height as usize, field);
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

/// A centered modal list for `/model` and `/provider`. The current selection
/// is marked with a bullet; Enter picks, Esc cancels.
fn draw_picker(frame: &mut Frame, app: &App) {
    let Some(picker) = &app.picker else {
        return;
    };
    let area = frame.area();
    let width = picker_width(area.width);
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
                .map(|item| item.split(" · ").next().unwrap_or(item) == app.cfg().model)
                .unwrap_or(false),
            PickerKind::Provider => item == app.cfg().provider.name(),
            // Nothing in this list is a choice, so nothing is marked as one.
            PickerKind::Notes => false,
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
        Paragraph::new(Line::from(Span::styled(picker.hint(), dim()))),
        hint,
    );
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let focus = match app.focus {
        Focus::Agents => "agents",
        Focus::Chat => "chat",
    };
    // Priority: a failure first (the Ctrl-Q warning included, so it is never
    // hidden behind work in progress), then what the whole tree is doing that
    // its rows cannot say, then what just happened (fades), then the static
    // hint.
    let (message, style) = bar_line(app.status_line(), app.tree_line());
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
/// (finding B12 — the Ctrl-Q warning included). The order itself is
/// `chat::Rank`, the one table; this only maps it to a colour.
///
/// The focused agent's activity is deliberately not a candidate here. It has
/// two homes already — the row's own tail, with its age, and the transcript's
/// `⚙` line — and a bar that repeated it spent its only row on the same
/// sentence a third time (finding U5). What the bar says instead is what no row
/// and no transcript can: the newest *event* (a failure, a stop, a job's
/// report, a command's answer) or the one derived state the rows only imply
/// (`tree_line`'s napping root).
fn bar_line(status: Option<(&str, StatusKind)>, tree: Option<String>) -> (String, Style) {
    let alert = status
        .filter(|(_, kind)| *kind == StatusKind::Error)
        .map(|(text, _)| text);
    let said = status
        .filter(|(_, kind)| *kind == StatusKind::Info)
        .map(|(text, _)| text);
    match Rank::last_word(alert, tree.as_deref(), said) {
        Some((rank, text)) => (
            text.to_string(),
            match rank {
                Rank::Alert => Style::default().fg(Color::Red),
                Rank::Activity => Style::default().fg(Color::Cyan),
                Rank::Said => Style::default().fg(Color::Gray),
            },
        ),
        None => (HINT.to_string(), dim()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_outranks_the_tree_line() {
        let (text, _) = bar_line(
            Some(("cannot reach http://127.0.0.1:1", StatusKind::Error)),
            Some("waiting on 1 subagent(s) — the root resumes as they finish".to_string()),
        );
        assert_eq!(
            text, "cannot reach http://127.0.0.1:1",
            "an error is visible"
        );

        let (text, _) = bar_line(
            Some(("opened notes.txt", StatusKind::Info)),
            Some("#0 thinking 3s".to_string()),
        );
        assert_eq!(
            text, "#0 thinking 3s",
            "derived state beats a fading info line"
        );

        let (text, _) = bar_line(Some(("opened notes.txt", StatusKind::Info)), None);
        assert_eq!(text, "opened notes.txt");

        let (text, _) = bar_line(None, None);
        assert!(text.contains("/help"), "{text}");
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
    }

    /// The detail line carries the age of the *phase*, so a slow model looks
    /// slow instead of looking stuck.
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
