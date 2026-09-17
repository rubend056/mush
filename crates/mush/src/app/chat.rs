//! The conversation: the transcript the human reads, the lines mush itself
//! wrote about it, and the box they type in.
//!
//! Everything a chat is made of lives here as one value — the root transcript,
//! each subagent's own transcript, the notices, the message box with its
//! grapheme cursor, and the scrollback. They belong together because they are
//! read together (a pane shows one agent's transcript with its notices under
//! it) and because every way a line reaches the screen is a method on this
//! type: `app::mod` routes events and keys into it and never writes a
//! transcript itself, so "what the human is looking at" has exactly one writer.
//!
//! A notice is a line *about* a conversation, and it has a lifetime here rather
//! than a life of its own. It carries when it happened and which agent it
//! concerns; a command's answer is dropped when that agent runs again, a run's
//! failure replaces the agent's older failure and is written to the session, and
//! `/new` or `/forget` takes them both away. Before this, every notice ever
//! written stayed until `/new`, a failure from twenty runs ago was painted under
//! the newest message as if it were the newest thing said, and none of it
//! survived a restart.
//!
//! What a pane paints is built here too (`painted`), because which rows it shows
//! is a fact about the conversation, its scrollback and its notes — not about the
//! terminal: width and height are arguments, the blank separator that closes a
//! message is trimmed before the window is cut, and the foot is capped and
//! counted. `ui.rs` keeps the frame around it — the border, the prompt and the
//! cursor — and paints what this returns, title included, because a pane one row
//! tall has no row to spend on saying what it is hiding.

use std::collections::HashMap;
use std::time::Duration;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use mush_core::message::Message;
use mush_core::session;
use mush_core::text::{truncate, wrap_text, wrap_text_capped};

use crate::agent::summarize_args;
use crate::app::short_age;
use crate::app::tree::AgentId;
use crate::input::Input;
use crate::ui::dim;

/// A spinner's frames, so a run in flight looks alive in the pane.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The most rows the foot may take from the transcript: two rows of notes and
/// the one that says how many lines are not shown. The transcript is the point
/// of the pane, so a foot of twenty lines is a transcript of four.
const FOOT_ROWS: usize = 3;
/// Of those rows, how many the notes themselves may have before the rest are
/// arithmetical. Two, because the third is what says they are an excerpt.
const FOOT_NOTE_ROWS: usize = 2;
/// The width `/notes` wraps a note at: the popup is at most 80 columns wide,
/// minus its border, the list's cursor and the age that leads each line.
pub const NOTES_WIDTH: usize = 74;

/// A line for the transcript that is not a message: a note from mush itself.
/// It is tagged with the agent it concerns, so a root-level failure is not
/// rendered into every child's transcript (finding B19), and stamped with when
/// it happened, so a restored one can say *when* and not only *what*.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub agent: AgentId,
    pub kind: NoticeKind,
    /// When it was written, in Unix seconds. This is the only clock a notice
    /// carries: the row's phase has an `Instant`, which cannot survive a
    /// restart, so the line is where the age of a failure lives.
    pub at: u64,
    pub text: String,
}

/// Only failures are red. Hints — `/help`, the git command to merge a branch —
/// are information, and colouring them like errors is how a screen cries wolf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Error,
}

/// What wins when more than one line wants to be a pane's last (finding B12):
/// a failure first, then derived activity, then what mush merely said.
///
/// This is the one precedence table. The bar (`ui::bar_line`) and a transcript
/// pane both rank their lines through it, so the two cannot disagree about
/// which of two things the human needs to see first — which is how an `Error`
/// status came to lose to a `thinking…` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
    /// A line mush wrote: a hint, `opened notes.txt`, a merge that landed.
    Said,
    /// Derived from the phases: a run in flight, a tree still working.
    Activity,
    /// A failure: the thing the human has to read.
    Alert,
}

impl Rank {
    /// The line that wins between these, with its rank. `None` when there is
    /// nothing to say at all.
    pub fn last_word<'a>(
        alert: Option<&'a str>,
        activity: Option<&'a str>,
        said: Option<&'a str>,
    ) -> Option<(Rank, &'a str)> {
        if let Some(alert) = alert {
            return Some((Rank::Alert, alert));
        }
        if let Some(activity) = activity {
            return Some((Rank::Activity, activity));
        }
        said.map(|said| (Rank::Said, said))
    }
}

impl Notice {
    /// Where this line sits in the precedence table. Only failures are alerts.
    pub fn rank(&self) -> Rank {
        match self.kind {
            NoticeKind::Error => Rank::Alert,
            NoticeKind::Info => Rank::Said,
        }
    }
}

/// One transcript pane as it is painted: the rows, top first, and the title it
/// wears.
///
/// The title is part of this because it is the one place a pane too short for a
/// foot can say how many lines it is hiding — a count that lived in the pane
/// instead would need a row the pane does not have.
pub struct Painted {
    pub lines: Vec<Line<'static>>,
    pub title: String,
}

/// The foot of one transcript pane: the rows a pane paints under the
/// conversation, how many of the foot's lines it has no room for, and whether
/// the row that says so is one of them.
struct Foot {
    lines: Vec<Line<'static>>,
    hidden: usize,
    counted: bool,
}

/// What a pane knows that the conversation does not: which agent it is showing,
/// whether that agent's run is in flight, where the animation is, and the
/// endpoint/model line the empty state names.
#[derive(Clone, Copy)]
pub struct Pane<'a> {
    pub agent: AgentId,
    pub busy: bool,
    pub spin: u64,
    pub label: &'a str,
}

/// One conversation: what has been said, what mush added to it, and what the
/// human is typing.
pub struct Chat {
    /// The system prompt the root conversation runs with. It is not stored in
    /// the session — it names a workspace that may have moved — so it lives
    /// here, next to the transcript it opens.
    system: Message,
    /// The root conversation: what the chat pane shows by default, what the
    /// root actor is sent, and what the context meter weighs.
    root: Vec<Message>,
    /// Each subagent's own transcript, keyed by id. The tree owns which ids
    /// exist; this owns what each one has said.
    agents: HashMap<AgentId, Vec<Message>>,
    /// Lines that are not messages: what mush wrote about an agent.
    notices: Vec<Notice>,
    /// The message box, whose cursor counts graphemes, not chars (finding N2).
    input: Input,
    /// Rows of scrollback the pane is showing; 0 is the bottom, where a new
    /// line puts it back.
    scroll: usize,
}

impl Chat {
    pub fn new(system: Message, root: Vec<Message>) -> Self {
        Self {
            system,
            root,
            agents: HashMap::new(),
            notices: Vec::new(),
            input: Input::default(),
            scroll: 0,
        }
    }

    /// An empty chat, for this module's own tests.
    #[cfg(test)]
    pub fn bare() -> Self {
        Self::new(Message::system("you are mush"), Vec::new())
    }

    /// The system prompt this conversation opens with. Every run is sent it
    /// through [`Self::conversation`]; this is for weighing it on its own.
    #[cfg(test)]
    pub fn system(&self) -> &Message {
        &self.system
    }

    /// The root conversation exactly as the actor wants it: the system prompt
    /// first, then what has been said.
    pub fn conversation(&self) -> Vec<Message> {
        let mut messages = Vec::with_capacity(self.root.len() + 1);
        messages.push(self.system.clone());
        messages.extend(self.root.iter().cloned());
        messages
    }

    /// An agent's transcript; the root's is the conversation, every other
    /// agent's its own. An id with nothing said yet reads as empty, so the
    /// caller never has to tell "no node" from "nothing said".
    pub fn transcript(&self, agent: AgentId) -> &[Message] {
        if agent == AgentId::ROOT {
            &self.root
        } else {
            self.agents
                .get(&agent)
                .map(Vec::as_slice)
                .unwrap_or_default()
        }
    }

    /// Append a line to an agent's transcript.
    pub fn push_message(&mut self, agent: AgentId, message: Message) {
        if agent == AgentId::ROOT {
            self.root.push(message);
        } else {
            self.agents.entry(agent).or_default().push(message);
        }
    }

    /// Replace an agent's transcript: the root's is compacted to
    /// `[system, user(summary)]`, a restored one arrives whole.
    pub fn replace_transcript(&mut self, agent: AgentId, messages: Vec<Message>) {
        if agent == AgentId::ROOT {
            self.root = messages;
        } else {
            self.agents.insert(agent, messages);
        }
    }

    /// Drop an agent's transcript, and the lines mush wrote about it: a
    /// forgotten agent is not coming back, and notes about a conversation
    /// nobody can read or steer are text with no owner.
    pub fn forget(&mut self, agent: AgentId) {
        self.agents.remove(&agent);
        self.notices.retain(|notice| notice.agent != agent);
    }

    /// How big one conversation is, in tokens, roughly — the same
    /// three-bytes-per-token heuristic the trimmer uses.
    ///
    /// Derived on read, per agent, and never counted beside the transcript:
    /// there is no push site left to forget, and the human's own words weigh as
    /// soon as they are in the transcript they are in (finding B8). Per agent
    /// because the pane a human is looking at can be a subagent's, and its own
    /// next request is what this number measures.
    pub fn used_tokens_for(&self, id: AgentId) -> usize {
        let transcript = if id == AgentId::ROOT {
            &self.root
        } else {
            match self.agents.get(&id) {
                Some(messages) => messages,
                None => return 0,
            }
        };
        let bytes = self.system.weight() + transcript.iter().map(Message::weight).sum::<usize>();
        bytes / 3
    }

    /// `/new`: the conversation is gone, the box and the scrollback with it.
    pub fn clear(&mut self) {
        self.root.clear();
        self.agents.clear();
        self.notices.clear();
        self.scroll = 0;
    }

    /// A line for the transcript that is not a message: a hint, or a failure.
    /// It concerns the root conversation unless tagged otherwise.
    pub fn note(&mut self, text: impl Into<String>) {
        self.note_for(AgentId::ROOT, text);
    }

    /// An information line — what a command just did, what a run just hit.
    ///
    /// It belongs to the moment it answers: the agent's next run is a different
    /// moment (see [`Self::clear_notes_for`]), and nothing about it is written
    /// to the session, because a restart has no moment to answer. A new line
    /// does not replace the old one; the run does, or `/new` does.
    pub fn note_for(&mut self, agent: AgentId, text: impl Into<String>) {
        self.push_notice(agent, NoticeKind::Info, text);
    }

    pub fn note_error(&mut self, text: impl Into<String>) {
        self.note_error_for(AgentId::ROOT, text);
    }

    /// A run failed. This is the line that outlives the run: it is the agent's
    /// own record of its last failure, so a new one replaces the old rather
    /// than piling up beside it — two failures for one agent would disagree
    /// about which is current, and the pane would print both.
    pub fn note_error_for(&mut self, agent: AgentId, text: impl Into<String>) {
        self.notices
            .retain(|notice| !(notice.agent == agent && notice.kind == NoticeKind::Error));
        self.push_notice(agent, NoticeKind::Error, text);
    }

    /// Forget what mush said about one agent, and say whether it said anything.
    ///
    /// Called when the agent starts a run: a line that answered a command
    /// answered the moment before it, and a failure belongs to the run that is
    /// now being superseded — the agent is either finishing something newer or
    /// it will fail again and say so. Only this agent's lines go: a root
    /// failure is not a child's business and a child's is not the root's
    /// (finding B19).
    pub fn clear_notes_for(&mut self, agent: AgentId) -> bool {
        let before = self.notices.len();
        self.notices.retain(|notice| notice.agent != agent);
        self.notices.len() != before
    }

    /// Every note this conversation would hand back to a restarted mush: the
    /// failures, oldest first. The information lines are deliberately absent —
    /// they answered a command in a moment that is over, and a restored
    /// `git diff HEAD...mush/2` would name a worktree nobody is looking at.
    pub fn stored_notices(&self) -> Vec<session::StoredNotice> {
        self.notices
            .iter()
            .filter(|notice| notice.kind == NoticeKind::Error)
            .map(|notice| session::StoredNotice {
                agent: notice.agent.0,
                at: notice.at,
                text: notice.text.clone(),
            })
            .collect()
    }

    /// Adopt the failures of a previous process. They are older than anything
    /// this process can write, so they go in front and the oldest-first order
    /// of [`Self::notices_for`] holds without sorting.
    pub fn restore_notices(&mut self, stored: Vec<session::StoredNotice>) {
        let mut restored: Vec<Notice> = Vec::new();
        for notice in stored {
            // One failure per agent, the newest: two would disagree about which
            // of them is current, exactly as two live ones would, and a file
            // written by hand (or by another version) is not a reason to paint
            // a pane that cannot be read.
            restored.retain(|earlier| earlier.agent != AgentId(notice.agent));
            restored.push(Notice {
                agent: AgentId(notice.agent),
                kind: NoticeKind::Error,
                at: notice.at,
                text: notice.text,
            });
        }
        self.notices.splice(0..0, restored);
    }

    /// The lines mush wrote about one agent, oldest first. Scoped by
    /// construction: a pane asks for its own agent and can get no other's, so
    /// a root-level failure cannot be rendered into a child's transcript
    /// (finding B19). There is deliberately no unscoped read: a list of every
    /// notice is a list a pane could paint into the wrong transcript.
    pub fn notices_for(&self, agent: AgentId) -> impl Iterator<Item = &Notice> {
        self.notices
            .iter()
            .filter(move |notice| notice.agent == agent)
    }

    /// Every note this pane holds, oldest first, as the rows of a list: when it
    /// happened, what it said, and — because the foot can only ever show two of
    /// its lines — the rest of it, wrapped here rather than clipped, since a
    /// list row cannot wrap itself.
    pub fn notes_report(&self, agent: AgentId, now: u64, width: usize) -> Vec<String> {
        let mut rows = Vec::new();
        for notice in self.notices_for(agent) {
            let marker = match notice.kind {
                NoticeKind::Info => "·",
                NoticeKind::Error => "!",
            };
            let age = short_age(Duration::from_secs(now.saturating_sub(notice.at)));
            let lead = format!("{age} {marker} ");
            for (index, line) in wrap_text(&notice.text, width).into_iter().enumerate() {
                if index == 0 {
                    rows.push(format!("{lead}{line}"));
                } else {
                    rows.push(format!("{}{line}", " ".repeat(lead.len())));
                }
            }
        }
        rows
    }

    fn push_notice(&mut self, agent: AgentId, kind: NoticeKind, text: impl Into<String>) {
        self.notices.push(Notice {
            agent,
            kind,
            at: session::now_secs(),
            text: text.into(),
        });
    }

    /// Follow the newest line: a message, a notice, or the end of a run puts
    /// the pane back at the bottom.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_by(&mut self, delta: i64) {
        let next = self.scroll as i64 + delta;
        self.scroll = next.max(0) as usize;
    }

    /// The rows a pane `height` rows tall and `width` columns wide is showing:
    /// the tail of the agent's transcript, bottom-anchored, with the blank
    /// separator that closes a message trimmed before the window is cut, and the
    /// foot — the notes mush wrote, capped and counted — pinned under it.
    ///
    /// The trim is what keeps a one-row pane from showing that blank instead of
    /// the message it separates (finding B4); only the tail is built, so a long
    /// session costs the visible rows and not the scrollback.
    pub fn painted(&self, pane: &Pane<'_>, width: usize, height: usize) -> Painted {
        // The foot is a foot: at most `FOOT_ROWS`, and never the transcript's
        // last row, so a pane too short for both still has a conversation in
        // it. A pane with nothing said yet has no such row to protect — there,
        // a blank one above mush's own lines would be the least useful row the
        // pane could spend. `scroll` stays the transcript's: the foot does not
        // scroll away, which is what makes it a foot rather than the newest
        // message.
        let protected = usize::from(!self.transcript(pane.agent).is_empty());
        let room = FOOT_ROWS.min(height.saturating_sub(protected));
        let foot = self.foot(pane, width, room);
        let mut lines = self.body(pane, width, height.saturating_sub(foot.lines.len()));
        lines.extend(foot.lines);

        let mut title = if pane.agent == AgentId::ROOT {
            " mush ".to_string()
        } else {
            format!(" agent #{} ", pane.agent)
        };
        // A pane with no row to spare for the foot's own count line is the case
        // the title exists for: wherever the human looks, the pane says how
        // many lines it is hiding.
        if foot.hidden > 0 && !foot.counted {
            title.push_str(&format!("· {} ", more_label(foot.hidden)));
        }
        Painted { lines, title }
    }

    /// The transcript itself, without the foot: the rows the conversation fills
    /// in `height`, newest at the bottom.
    fn body(&self, pane: &Pane<'_>, width: usize, height: usize) -> Vec<Line<'static>> {
        let messages = self.transcript(pane.agent);

        // A pane with nothing in it says what it is waiting for rather than
        // being blank.
        if messages.is_empty() && self.notices_for(pane.agent).next().is_none() {
            return if pane.agent == AgentId::ROOT {
                vec![
                    Line::from(Span::styled(
                        "Ask for a change — the agent reads and edits this workspace directly.",
                        dim(),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(pane.label.to_string(), dim())),
                    Line::from(Span::styled(
                        "Tab cycles panes · Enter sends · /help lists commands",
                        dim(),
                    )),
                ]
            } else {
                vec![Line::from(Span::styled(
                    format!(
                        "Agent #{} has no messages yet — typing here sends it a nudge.",
                        pane.agent
                    ),
                    dim(),
                ))]
            };
        }

        // Built back to front and then reversed: each chunk is one message's
        // rows in their own order, and the pane is anchored at the bottom, so
        // the newest line is the one that must be there.
        let want = height + self.scroll;
        let mut chunks: Vec<Vec<Line<'static>>> = Vec::new();
        let mut count = 0usize;

        for message in messages.iter().rev() {
            if count >= want {
                break;
            }
            let mut chunk = Vec::new();
            render_message(&mut chunk, message, width);
            count += chunk.len();
            chunks.push(chunk);
        }

        let mut lines = Vec::with_capacity(count);
        for chunk in chunks.into_iter().rev() {
            lines.extend(chunk);
        }
        trim_trailing_blanks(&mut lines);

        // Anchor the window at the bottom: the newest `height` rows, with
        // `scroll` rows of older ones above them. Building backwards means the
        // last chunk can overshoot `want`, so the window cannot be assumed to
        // be exactly `want` rows — deriving the start from what was built is
        // the only arithmetic that is right in both cases. Taking `0` when it
        // overshot painted the *oldest* rows of the window, which made a
        // message taller than the pane freeze the view and hide its own end.
        let start = lines.len().saturating_sub(height + self.scroll);
        lines.into_iter().skip(start).take(height).collect()
    }

    /// The rows of the foot, capped at `room` and counted, in the order the pane
    /// paints them: the notes as they happened, then the run in flight, then a
    /// failure — the last two are the ranked pair finding B12 pinned at the
    /// bottom of the pane.
    ///
    /// Which of them survive the cap is a different question from the order they
    /// are painted in, and it is ranked the same way: a failure is never spent on
    /// a hint, and among the hints the newest is the one kept — anything else
    /// would be a foot that drops today's line to keep last week's. What did not
    /// fit is one row of arithmetic, so the count is a decision and not a
    /// discovery.
    fn foot(&self, pane: &Pane<'_>, width: usize, room: usize) -> Foot {
        let notices: Vec<&Notice> = self.notices_for(pane.agent).collect();
        let alert = notices
            .iter()
            .rposition(|notice| notice.rank() == Rank::Alert);
        let said = notices.len() - usize::from(alert.is_some());

        // Blocks in paint order, each with the order it is worth keeping in:
        // lower is less hideable.
        let mut blocks: Vec<Vec<Line<'static>>> = Vec::new();
        let mut worth: Vec<usize> = Vec::new();
        let mut said_at = 0usize;
        for (at, notice) in notices.iter().enumerate() {
            if Some(at) == alert {
                continue;
            }
            // Newest first among these: `said` is how many there are, so the
            // last one gets the lowest rank.
            worth.push(2 + (said - 1 - said_at));
            said_at += 1;
            blocks.push(footnote_lines(notice, width));
        }
        if pane.busy {
            worth.push(1);
            blocks.push(vec![Line::from(Span::styled(
                format!("{} working…", SPINNER[(pane.spin as usize) % SPINNER.len()]),
                Style::default().fg(Color::Cyan),
            ))]);
        }
        if let Some(at) = alert {
            worth.push(0);
            blocks.push(footnote_lines(notices[at], width));
        }

        let rows: Vec<usize> = blocks.iter().map(Vec::len).collect();
        let total: usize = rows.iter().sum();
        // More lines than a foot may show: one row of what is left is spent on
        // saying so. The transcript keeps the row that arithmetic costs it.
        let mut budget = if total <= FOOT_NOTE_ROWS {
            total.min(room)
        } else {
            FOOT_NOTE_ROWS.min(room.saturating_sub(1))
        };
        let mut order: Vec<usize> = (0..blocks.len()).collect();
        order.sort_by_key(|index| worth[*index]);
        let mut keep = vec![0usize; blocks.len()];
        for index in order {
            let kept = rows[index].min(budget);
            keep[index] = kept;
            budget -= kept;
        }
        let shown: usize = keep.iter().sum();
        let hidden = total - shown;
        // The count row needs a row the pane has; without both, the title says
        // it instead.
        let counted = hidden > 0 && shown < room;

        let mut lines = Vec::with_capacity(shown + usize::from(counted));
        if counted {
            lines.push(Line::from(Span::styled(
                format!("  {} · /notes", more_label(hidden)),
                dim(),
            )));
        }
        for (index, block) in blocks.into_iter().enumerate() {
            lines.extend(block.into_iter().take(keep[index]));
        }
        Foot {
            lines,
            hidden,
            counted,
        }
    }

    /// The message box, for painting it.
    pub fn input(&self) -> &Input {
        &self.input
    }

    /// Put text in the box at the cursor: a paste, delivered whole.
    pub fn insert(&mut self, text: &str) {
        self.input.insert(text);
    }

    /// Take the box's text out, as a send does.
    pub fn take_input(&mut self) -> String {
        self.input.take()
    }

    /// A key that means something to the chat itself: editing the message box,
    /// or scrolling the transcript. Returns whether it was consumed —
    /// `<Enter>` is not, because sending is the agents' business, and neither
    /// is any key this value has no opinion about.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // A new line instead of sending. Only terminals that report the
            // modifier can deliver Shift+Enter (kitty, WezTerm, foot, Ghostty,
            // recent Alacritty); elsewhere it arrives as a plain Enter, which
            // is why Alt+Enter does the same thing and is the reliable one.
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) || alt => {
                self.input.insert("\n")
            }
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete_forward(),
            KeyCode::Left => self.input.move_left(),
            KeyCode::Right => self.input.move_right(),
            KeyCode::Home => self.input.move_home(),
            KeyCode::End => self.input.move_end(),
            KeyCode::Char(c) if !ctrl && !alt => self.input.insert(&c.to_string()),
            KeyCode::Up => self.scroll_by(1),
            KeyCode::Down => self.scroll_by(-1),
            KeyCode::PageUp => self.scroll_by(10),
            KeyCode::PageDown => self.scroll_by(-10),
            KeyCode::Esc => self.input.clear(),
            _ => return false,
        }
        true
    }
}

/// The rows of one notice, wrapped at the pane's width and marked by kind: only
/// a failure shouts. The mark leads the first row only — a wrapped line is one
/// line, and a column of `!` reads as several failures.
fn footnote_lines(notice: &Notice, width: usize) -> Vec<Line<'static>> {
    let (prefix, style) = match notice.kind {
        NoticeKind::Info => ("·", dim()),
        NoticeKind::Error => ("!", Style::default().fg(Color::Red)),
    };
    wrap_text(&notice.text, width.saturating_sub(2))
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let lead = if index == 0 {
                format!("{prefix} ")
            } else {
                "  ".to_string()
            };
            Line::from(Span::styled(format!("{lead}{line}"), style))
        })
        .collect()
}

/// How a pane says it is showing an excerpt. One wording, because the foot's own
/// count row and the title are two places saying the same number.
fn more_label(hidden: usize) -> String {
    format!("+{hidden} more lines")
}

/// Every message ends with a blank separator line. At one row of transcript that
/// blank would be the only visible line — the reply would be invisible — so the
/// separator is trimmed before windowing (finding B4).
fn trim_trailing_blanks(lines: &mut Vec<Line<'static>>) {
    while lines.last().map(|line| line.width()) == Some(0) {
        lines.pop();
    }
}

/// One message's rows: who said it, wrapped at the pane's width.
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
                    truncate(&summarize_args(&call.function.arguments), 60)
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn pane(agent: AgentId) -> Pane<'static> {
        Pane {
            agent,
            busy: false,
            spin: 0,
            label: "test-model · ctx ~500k",
        }
    }

    /// The text of the rows a pane would paint.
    fn shown(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    /// What a pane of this size shows, without its title: the rows are what
    /// every size test here reads.
    fn pane_rows(chat: &Chat, pane: &Pane<'_>, width: usize, height: usize) -> Vec<Line<'static>> {
        chat.painted(pane, width, height).lines
    }

    /// A 40×10 terminal leaves the transcript pane one row tall, and every
    /// message ends with a blank separator: that blank was the only row shown,
    /// so the reply was invisible (finding B4). The trim happens before the
    /// window is cut, for the same reason at every height.
    #[test]
    fn a_one_row_pane_shows_a_message_and_not_the_blank_after_it() {
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::user("make the lexer faster"));
        chat.push_message(AgentId::ROOT, Message::assistant("done — 3× on the bench"));
        let pane = pane(AgentId::ROOT);

        let rows = shown(&pane_rows(&chat, &pane, 38, 1));
        assert_eq!(rows, vec!["mush › done — 3× on the bench"]);
        assert!(
            rows.iter().all(|row| !row.trim().is_empty()),
            "the pane's one row must not be a separator"
        );

        // One row taller, and the message the reply belongs to is in view: the
        // transcript is anchored at the bottom.
        let rows = shown(&pane_rows(&chat, &pane, 38, 3));
        assert_eq!(
            rows,
            vec![
                "you › make the lexer faster",
                "",
                "mush › done — 3× on the bench"
            ]
        );
    }

    /// The window follows the scrollback, and the bottom is where a new line
    /// puts it back.
    #[test]
    fn the_window_follows_the_scrollback() {
        let mut chat = Chat::bare();
        for i in 0..5 {
            chat.push_message(AgentId::ROOT, Message::user(format!("line {i}")));
        }
        let pane = pane(AgentId::ROOT);
        let bottom = shown(&pane_rows(&chat, &pane, 20, 2));
        assert_eq!(bottom, vec!["you › line 4"], "anchored at the newest");

        // Up and down are the pane's own keys.
        assert!(chat.key(key(KeyCode::Up)));
        let up = shown(&pane_rows(&chat, &pane, 20, 2));
        assert_ne!(up, bottom, "scrolling shows what was above the fold");
        assert!(up.iter().any(|row| row.contains("line 3")), "{up:?}");

        assert!(chat.key(key(KeyCode::Down)));
        assert_eq!(shown(&pane_rows(&chat, &pane, 20, 2)), bottom);
        chat.scroll_to_bottom();
        assert_eq!(shown(&pane_rows(&chat, &pane, 20, 2)), bottom);
    }

    /// A transcript belongs to one agent: what a child was told is in the
    /// child's pane, and the root's conversation is not shown to it.
    #[test]
    fn a_transcript_belongs_to_one_agent() {
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::user("the human's question"));
        chat.push_message(AgentId(1), Message::user("the child's brief"));

        assert_eq!(
            chat.transcript(AgentId::ROOT)[0].text(),
            "the human's question"
        );
        assert_eq!(chat.transcript(AgentId(1))[0].text(), "the child's brief");
        assert_eq!(
            chat.transcript(AgentId(2)).len(),
            0,
            "an agent that has said nothing reads as empty, not as someone else's text"
        );

        // The actor is sent the root's conversation with the system prompt in
        // front of it, and nothing else.
        let conversation = chat.conversation();
        assert_eq!(conversation[0].role, "system");
        assert_eq!(conversation.len(), 2);
        assert_eq!(conversation[1].text(), "the human's question");
    }

    /// A key that means "new line" edits the box; a plain `<Enter>` is not the
    /// chat's to consume, because sending is the agents' business.
    #[test]
    fn the_chat_takes_the_editing_keys_and_leaves_enter_alone() {
        let mut chat = Chat::bare();
        let shift = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);

        assert!(chat.key(shift), "a modified Enter is an edit");
        assert_eq!(chat.input().text(), "\n");
        assert!(
            !chat.key(key(KeyCode::Enter)),
            "a plain Enter must reach the agents"
        );
        assert!(!chat.key(key(KeyCode::Tab)), "and so must the pane keys");
    }

    /// A message taller than the pane must show its *end*, not its start: the
    /// pane is anchored at the bottom (scroll 0), so the newest rows are the
    /// ones a human is looking for — and with scroll 0 there is no other way to
    /// reach them.
    #[test]
    fn a_message_taller_than_the_pane_shows_its_end() {
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::user("aaaa bbbb cccc dddd"));
        let pane = pane(AgentId::ROOT);
        let one = pane_rows(&chat, &pane, 5, 1);
        assert_eq!(one.len(), 1);
        assert!(
            one[0].to_string().contains("dddd"),
            "the last row of the message, not its first: {:?}",
            one[0].to_string()
        );

        // A tall reply under a short one: the reply's own end, again.
        chat.push_message(AgentId::ROOT, Message::assistant("aaaa bbbb cccc dddd"));
        let two = pane_rows(&chat, &pane, 5, 2);
        let painted: Vec<String> = two.iter().map(|line| line.to_string()).collect();
        assert_eq!(painted.len(), 2);
        assert!(
            painted.last().unwrap().contains("dddd"),
            "the newest row is the message's end: {painted:?}"
        );
    }

    /// Scrolling up moves the window without changing its size, and the bottom
    /// stays reachable at 0.
    #[test]
    fn scrolling_moves_the_window_not_its_size() {
        let mut chat = Chat::bare();
        for index in 0..6 {
            chat.push_message(AgentId::ROOT, Message::user(format!("line {index}")));
        }
        let pane = pane(AgentId::ROOT);
        let bottom = pane_rows(&chat, &pane, 40, 3);
        let text: Vec<String> = bottom.iter().map(|l| l.to_string()).collect();
        assert!(text.last().unwrap().contains("line 5"), "{text:?}");

        chat.scroll_by(2);
        let scrolled = pane_rows(&chat, &pane, 40, 3);
        assert_eq!(scrolled.len(), 3, "the window is the pane's height");
        assert!(
            scrolled[0].to_string() != text[0],
            "scrolling showed older rows"
        );
    }

    /// A notice belongs to one agent: a root failure must not be painted into a
    /// child's pane, and a child's line must not turn up in a sibling's
    /// (finding B19).
    #[test]
    fn a_notice_belongs_to_one_agent_only() {
        let mut chat = Chat::bare();
        chat.note_error("cannot reach the endpoint");
        chat.note_for(AgentId(1), "the lexer subagent hit its turn limit");
        chat.note_for(AgentId(2), "compacted its own history");

        let of = |agent: AgentId| -> Vec<String> {
            chat.notices_for(agent).map(|n| n.text.clone()).collect()
        };
        assert_eq!(of(AgentId::ROOT), vec!["cannot reach the endpoint"]);
        assert_eq!(
            of(AgentId(1)),
            vec!["the lexer subagent hit its turn limit"],
            "a child sees its own line and nobody else's"
        );
        assert_eq!(of(AgentId(2)), vec!["compacted its own history"]);
        assert!(
            of(AgentId(3)).is_empty(),
            "an agent nothing was said about has no lines"
        );

        // The pane reads through the same accessor, so it inherits the scope:
        // the root's failure is not a row of the child's pane.
        let child = pane(AgentId(1));
        let painted = chat.painted(&child, 40, 6);
        let foot = painted.lines[painted.lines.len() - 1].to_string();
        assert!(
            foot.contains("turn limit") && !foot.contains("cannot reach"),
            "the child's pane shows its own line and no other: {foot:?}"
        );
    }

    /// The one precedence table (finding B12): a failure outranks derived
    /// activity, which outranks what mush merely said.
    #[test]
    fn a_failure_outranks_the_activity_line() {
        let (alert, activity, said) = (
            Some("no route to host"),
            Some("#0 thinking 3s"),
            Some("opened notes.txt"),
        );
        assert_eq!(
            Rank::last_word(alert, activity, said),
            Some((Rank::Alert, "no route to host"))
        );
        assert_eq!(
            Rank::last_word(None, activity, said),
            Some((Rank::Activity, "#0 thinking 3s"))
        );
        assert_eq!(
            Rank::last_word(None, None, said),
            Some((Rank::Said, "opened notes.txt"))
        );
        assert_eq!(Rank::last_word(None, None, None), None);

        // The pane keeps the rank too, in the only place it can still bite: what
        // survives the cap. A foot of three rows with a failure, a run in flight
        // and a hint in it spends its rows on the failure and the run and gives
        // up the hint — never the other way round.
        let mut chat = Chat::bare();
        chat.note_for(AgentId(1), "reading the lexer");
        chat.note_error_for(AgentId(1), "no route to host");
        let busy = Pane {
            busy: true,
            ..pane(AgentId(1))
        };

        let painted = chat.painted(&busy, 40, 4);
        let rows = shown(&painted.lines);
        assert!(
            rows.iter().any(|row| row.contains("no route to host")),
            "the failure is never the line the cap gives up: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("working…")),
            "and the run in flight is still shown, ranked below the failure"
        );
        assert!(
            !rows.iter().any(|row| row.contains("reading the lexer")),
            "the hint is what yields, and the count is what says so: {rows:?}"
        );
        assert_eq!(rows[0], "  +1 more lines · /notes", "{rows:?}");

        // At rest the failure is what the pane adds, and both fit: the count row
        // only appears when something really is missing.
        let painted = chat.painted(&pane(AgentId(1)), 40, 6);
        let rows = shown(&painted.lines);
        assert_eq!(
            rows,
            vec!["· reading the lexer", "! no route to host"],
            "the foot reads in the order the lines were written, oldest first"
        );
    }

    /// The order is a decision and not an accident: a list of notices is a
    /// history, so it reads oldest first and the pane paints it the way it
    /// paints the conversation — oldest at the top, newest nearest the message
    /// box. A restored line is older than anything this process can write, so it
    /// is in front of them all; a live failure for an agent that already had one
    /// replaces it, because the two would disagree about which is current.
    #[test]
    fn notices_read_oldest_first_and_a_restored_one_reads_before_them_all() {
        let mut chat = Chat::bare();
        chat.restore_notices(vec![session::StoredNotice {
            agent: 0,
            at: 1,
            text: "yesterday's failure".into(),
        }]);
        chat.note("a hint");

        let texts = |chat: &Chat| -> Vec<String> {
            chat.notices_for(AgentId::ROOT)
                .map(|notice| notice.text.clone())
                .collect()
        };
        assert_eq!(texts(&chat), vec!["yesterday's failure", "a hint"]);

        chat.note_error("today's failure");
        assert_eq!(
            texts(&chat),
            vec!["a hint", "today's failure"],
            "one failure per agent, and it is the newest one"
        );

        // The foot paints in that order, under the transcript.
        chat.push_message(AgentId::ROOT, Message::assistant("the reply"));
        let rows = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 40, 8));
        assert_eq!(
            rows,
            vec!["mush › the reply", "· a hint", "! today's failure",]
        );
    }

    /// The transcript is the point of the pane: the foot never takes its last
    /// row, and a pane with no room for even the count says it in the title
    /// instead of spending a row the conversation needs.
    #[test]
    fn the_foot_never_pushes_the_transcript_out_of_the_pane() {
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::assistant("the newest reply"));
        for index in 0..5 {
            chat.note_for(AgentId::ROOT, format!("note {index}"));
        }
        let pane = pane(AgentId::ROOT);

        // One row of pane: the transcript keeps it, and the title carries the
        // count because the foot has no row to spend on saying it.
        let painted = chat.painted(&pane, 40, 1);
        assert_eq!(shown(&painted.lines), vec!["mush › the newest reply"]);
        assert_eq!(painted.title, " mush · +5 more lines ");

        // Two rows: one is the count, one is the conversation — five note lines
        // were written and none of them is painted, which is what the count is
        // for.
        let painted = chat.painted(&pane, 40, 2);
        assert_eq!(
            shown(&painted.lines),
            vec!["mush › the newest reply", "  +5 more lines · /notes"]
        );
        assert_eq!(
            painted.title, " mush ",
            "the foot said it, so the title need not say it twice"
        );
    }

    /// `/notes` is the other half of the cap and it wraps: a note longer than a
    /// list row is shown whole, which is what `/help` and a multi-line failure
    /// need, and every note says when it happened.
    #[test]
    fn the_notes_report_wraps_and_stamps_every_line() {
        let mut chat = Chat::bare();
        chat.note_for(AgentId(1), "could not compact the conversation");
        chat.note_error_for(AgentId(1), "no route to host");
        let at = chat.notices_for(AgentId(1)).next().expect("the hint").at;

        // Narrow enough to wrap the hint into several list rows.
        let rows = chat.notes_report(AgentId(1), at, 20);
        assert!(rows.len() > 2, "the long line wraps: {rows:?}");
        assert!(rows[0].starts_with("0s · could not"), "{:?}", rows[0]);
        assert!(
            rows[1].starts_with("     "),
            "a continuation lines up under the text, not under the stamp: {:?}",
            rows[1]
        );
        assert!(
            rows.iter().any(|row| row.ends_with("! no route to host")),
            "the failure is in the same list, marked: {rows:?}"
        );
        assert!(
            chat.notes_report(AgentId(2), at, 20).is_empty(),
            "and it is one agent's list, not every agent's (finding B19)"
        );
    }

    /// Clearing is per agent, because a line about one conversation is not a
    /// line about another (finding B19), and `/forget` takes an agent's lines
    /// with it: notes about a conversation nobody can read have no owner.
    #[test]
    fn clearing_and_forgetting_are_one_agent_at_a_time() {
        let mut chat = Chat::bare();
        chat.note_error_for(AgentId(1), "boom");
        chat.note_for(AgentId::ROOT, "a hint");

        assert!(
            chat.clear_notes_for(AgentId(1)),
            "there was a line to clear"
        );
        assert!(!chat.clear_notes_for(AgentId(1)), "and now there is not");
        assert_eq!(
            chat.notices_for(AgentId::ROOT).count(),
            1,
            "the root's hint is not the child's to lose"
        );

        chat.note_error_for(AgentId(2), "boom");
        chat.forget(AgentId(2));
        assert_eq!(chat.notices_for(AgentId(2)).count(), 0);
        assert_eq!(
            chat.stored_notices().len(),
            0,
            "a forgotten failure is not written to the session either"
        );
    }

    /// The context meter is derived from the conversation, not counted beside
    /// it: the human's own words weigh as soon as they are in the transcript,
    /// and a transcript replaced wholesale moves the meter with it — there is
    /// nothing to forget (finding B8).
    #[test]
    fn the_context_meter_is_derived_from_the_conversation() {
        let mut chat = Chat::bare();
        let idle = chat.used_tokens_for(AgentId::ROOT);
        assert_eq!(
            idle,
            chat.system().weight() / 3,
            "the system prompt alone, for a conversation with nothing said"
        );

        let asked = Message::user("a question long enough to weigh something");
        chat.push_message(AgentId::ROOT, asked.clone());
        assert!(
            chat.used_tokens_for(AgentId::ROOT) > idle,
            "the meter must count the human's own message"
        );

        // Compaction replaces the transcript with a summary; the meter follows
        // the transcript, because it is the transcript.
        let summary = Message::user("a summary");
        chat.replace_transcript(AgentId::ROOT, vec![summary.clone()]);
        assert_eq!(
            chat.used_tokens_for(AgentId::ROOT),
            (chat.system().weight() + summary.weight()) / 3,
            "the meter reads what is there now"
        );

        // A subagent's transcript is not the root's conversation, so it does
        // not weigh on it.
        chat.push_message(AgentId(1), Message::assistant("x".repeat(1000)));
        assert_eq!(
            chat.used_tokens_for(AgentId::ROOT),
            (chat.system().weight() + summary.weight()) / 3
        );
    }

    /// The box that the chat routes keys to keeps its cursor in grapheme
    /// clusters, so a multi-codepoint glyph is one edit and not three
    /// (finding N2 — the cursor must not have been left behind when the box
    /// moved in here).
    #[test]
    fn the_message_box_keeps_its_grapheme_cursor() {
        let mut chat = Chat::bare();
        // A family emoji is one grapheme and three code points.
        chat.insert("x\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f466}y");

        assert!(chat.key(key(KeyCode::Backspace)));
        assert_eq!(
            chat.input().text(),
            "x\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f466}"
        );
        assert!(chat.key(key(KeyCode::Backspace)));
        assert_eq!(chat.input().text(), "x", "the whole emoji went at once");

        // And the cursor is where the typing goes, not only where it can be
        // deleted from: after moving left, the character is inserted before `x`.
        assert!(chat.key(key(KeyCode::Left)));
        assert!(chat.key(key(KeyCode::Char('A'))));
        assert_eq!(chat.input().text(), "Ax");
    }
}
