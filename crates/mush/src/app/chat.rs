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
#[derive(Clone, Debug)]
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

    pub fn notices(&self) -> &[Notice] {
        &self.notices
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
