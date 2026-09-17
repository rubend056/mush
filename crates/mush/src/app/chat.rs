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
//! The transcript body is built here too (`visible_lines`), because which rows
//! a pane shows is a fact about the conversation and its scrollback, and not
//! about the terminal it is painted on: width and height are arguments, and the
//! blank separator that closes a message is trimmed before the window is cut.
//! `ui.rs` keeps the frame around it — the border, the title, the prompt and
//! the cursor — and paints what this returns.

use std::collections::HashMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use mush_core::message::Message;
use mush_core::text::{truncate, wrap_text, wrap_text_capped};

use crate::agent::summarize_args;
use crate::app::tree::AgentId;
use crate::input::Input;
use crate::ui::dim;

/// A spinner's frames, so a run in flight looks alive in the pane.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A line for the transcript that is not a message: a note from mush itself.
/// It is tagged with the agent it concerns, so a root-level failure is not
/// rendered into every child's transcript (finding B19).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub agent: AgentId,
    pub kind: NoticeKind,
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

/// One of the lines a pane paints under its transcript, bottom of the pane
/// first. Which of them is last is not the pane's decision: it is the
/// precedence table's (finding B12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Footnote<'a> {
    /// Derived from the phases: the agent's run has not answered yet.
    Activity,
    /// A line mush wrote about this agent.
    Notice(&'a Notice),
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

    /// Drop an agent's transcript: a forgotten agent is not coming back, and a
    /// transcript without a node is text nobody can see or steer.
    pub fn forget(&mut self, agent: AgentId) {
        self.agents.remove(&agent);
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

    pub fn note_for(&mut self, agent: AgentId, text: impl Into<String>) {
        self.notices.push(Notice {
            agent,
            kind: NoticeKind::Info,
            text: text.into(),
        });
    }

    pub fn note_error(&mut self, text: impl Into<String>) {
        self.note_error_for(AgentId::ROOT, text);
    }

    pub fn note_error_for(&mut self, agent: AgentId, text: impl Into<String>) {
        self.notices.push(Notice {
            agent,
            kind: NoticeKind::Error,
            text: text.into(),
        });
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

    /// The lines a pane paints under the transcript, bottom of the pane first:
    /// the agent's own notices, and — while its run is in flight — one derived
    /// activity line.
    ///
    /// A failure is ranked last, so a spinner cannot push it off the bottom of
    /// the pane, and the rest follow newest first — the same order the bar
    /// ranks by (finding B12).
    pub fn footnotes(&self, agent: AgentId, busy: bool) -> Vec<Footnote<'_>> {
        let notices: Vec<&Notice> = self.notices_for(agent).collect();
        let alert = notices
            .iter()
            .rposition(|notice| notice.rank() == Rank::Alert);
        let mut foot = Vec::with_capacity(notices.len() + 1);
        if let Some(at) = alert {
            foot.push(Footnote::Notice(notices[at]));
        }
        if busy {
            foot.push(Footnote::Activity);
        }
        for (at, notice) in notices.iter().enumerate().rev() {
            if Some(at) != alert {
                foot.push(Footnote::Notice(notice));
            }
        }
        foot
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
    /// separator that closes a message trimmed before the window is cut.
    ///
    /// The trim is what keeps a one-row pane from showing that blank instead of
    /// the message it separates (finding B4); only the tail is built, so a long
    /// session costs the visible rows and not the scrollback.
    pub fn visible_lines(
        &self,
        pane: &Pane<'_>,
        width: usize,
        height: usize,
    ) -> Vec<Line<'static>> {
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

        // Built back to front and then reversed: each chunk is one message's or
        // one footnote's rows in their own order, and the pane is anchored at
        // the bottom, so the newest line is the one that must be there.
        let want = height + self.scroll;
        let mut chunks: Vec<Vec<Line<'static>>> = Vec::new();
        let mut count = 0usize;

        for footnote in self.footnotes(pane.agent, pane.busy) {
            if count >= want {
                break;
            }
            let chunk = match footnote {
                Footnote::Activity => vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("{} working…", SPINNER[(pane.spin as usize) % SPINNER.len()]),
                        Style::default().fg(Color::Cyan),
                    )),
                ],
                Footnote::Notice(notice) => footnote_lines(notice, width),
            };
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
/// a failure shouts.
fn footnote_lines(notice: &Notice, width: usize) -> Vec<Line<'static>> {
    let (prefix, style) = match notice.kind {
        NoticeKind::Info => ("·", dim()),
        NoticeKind::Error => ("!", Style::default().fg(Color::Red)),
    };
    wrap_text(&notice.text, width.saturating_sub(2))
        .into_iter()
        .map(|line| Line::from(Span::styled(format!("{prefix} {line}"), style)))
        .collect()
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

        let rows = shown(&chat.visible_lines(&pane, 38, 1));
        assert_eq!(rows, vec!["mush › done — 3× on the bench"]);
        assert!(
            rows.iter().all(|row| !row.trim().is_empty()),
            "the pane's one row must not be a separator"
        );

        // One row taller, and the message the reply belongs to is in view: the
        // transcript is anchored at the bottom.
        let rows = shown(&chat.visible_lines(&pane, 38, 3));
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
        let bottom = shown(&chat.visible_lines(&pane, 20, 2));
        assert_eq!(bottom, vec!["you › line 4"], "anchored at the newest");

        // Up and down are the pane's own keys.
        assert!(chat.key(key(KeyCode::Up)));
        let up = shown(&chat.visible_lines(&pane, 20, 2));
        assert_ne!(up, bottom, "scrolling shows what was above the fold");
        assert!(up.iter().any(|row| row.contains("line 3")), "{up:?}");

        assert!(chat.key(key(KeyCode::Down)));
        assert_eq!(shown(&chat.visible_lines(&pane, 20, 2)), bottom);
        chat.scroll_to_bottom();
        assert_eq!(shown(&chat.visible_lines(&pane, 20, 2)), bottom);
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
        let one = chat.visible_lines(&pane, 5, 1);
        assert_eq!(one.len(), 1);
        assert!(
            one[0].to_string().contains("dddd"),
            "the last row of the message, not its first: {:?}",
            one[0].to_string()
        );

        // A tall reply under a short one: the reply's own end, again.
        chat.push_message(AgentId::ROOT, Message::assistant("aaaa bbbb cccc dddd"));
        let two = chat.visible_lines(&pane, 5, 2);
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
        let bottom = chat.visible_lines(&pane, 40, 3);
        let text: Vec<String> = bottom.iter().map(|l| l.to_string()).collect();
        assert!(text.last().unwrap().contains("line 5"), "{text:?}");

        chat.scroll_by(2);
        let scrolled = chat.visible_lines(&pane, 40, 3);
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

        // The pane reads through the same accessor, so it inherits the scope.
        let foot = chat.footnotes(AgentId(1), false);
        assert_eq!(foot.len(), 1, "the root failure is not in this pane");
        assert!(matches!(foot[0], Footnote::Notice(notice) if notice.agent == AgentId(1)));
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

        // And the pane ranks the same way: while a run is in flight, the
        // failure is the last line it paints, so a spinner cannot push it off
        // the bottom.
        let mut chat = Chat::bare();
        chat.note_for(AgentId(1), "reading the lexer");
        chat.note_error_for(AgentId(1), "no route to host");

        let foot = chat.footnotes(AgentId(1), true);
        assert!(
            matches!(foot[0], Footnote::Notice(notice) if notice.kind == NoticeKind::Error),
            "the bottom of the pane is the failure, not the run in flight"
        );
        assert!(
            foot.iter().any(|line| matches!(line, Footnote::Activity)),
            "the run in flight is still shown — ranked below the failure, not hidden"
        );
        assert_eq!(foot.len(), 3, "nothing was dropped off the pane");

        // At rest, the activity line is what the pane adds.
        let idle = chat.footnotes(AgentId(1), false);
        assert_eq!(idle.len(), 2);
        assert!(!idle.iter().any(|line| matches!(line, Footnote::Activity)));
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
