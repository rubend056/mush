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

use std::collections::HashMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use mush_core::message::Message;

use crate::app::tree::AgentId;
use crate::input::Input;

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

    /// How many tokens the root conversation is holding, roughly — the same
    /// three-bytes-per-token heuristic the trimmer uses.
    ///
    /// Derived on read, from the system prompt and the transcript, and never
    /// counted beside them: there is no push site left to forget, and the
    /// human's own words weigh as soon as they are in the transcript they are
    /// in (finding B8).
    pub fn used_tokens(&self) -> usize {
        let bytes = self.system.weight() + self.root.iter().map(Message::weight).sum::<usize>();
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

    /// Rows of scrollback the pane is showing.
    pub fn scrollback(&self) -> usize {
        self.scroll
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
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
        let idle = chat.used_tokens();
        assert_eq!(
            idle,
            chat.system().weight() / 3,
            "the system prompt alone, for a conversation with nothing said"
        );

        let asked = Message::user("a question long enough to weigh something");
        chat.push_message(AgentId::ROOT, asked.clone());
        assert!(
            chat.used_tokens() > idle,
            "the meter must count the human's own message"
        );

        // Compaction replaces the transcript with a summary; the meter follows
        // the transcript, because it is the transcript.
        let summary = Message::user("a summary");
        chat.replace_transcript(AgentId::ROOT, vec![summary.clone()]);
        assert_eq!(
            chat.used_tokens(),
            (chat.system().weight() + summary.weight()) / 3,
            "the meter reads what is there now"
        );

        // A subagent's transcript is not the root's conversation, so it does
        // not weigh on it.
        chat.push_message(AgentId(1), Message::assistant("x".repeat(1000)));
        assert_eq!(
            chat.used_tokens(),
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
