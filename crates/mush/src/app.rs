//! Application state and the update function.
//!
//! mush has exactly one owner of editor state: the UI thread. Every input —
//! a keystroke, an agent event, a tool request — becomes a `Msg` and flows
//! through `App::update`. That single entry point is why the editor can hand
//! live buffers to an agent without races or locks.

use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::Value;

use mush_core::message::Message;
use mush_core::workspace::truncate_for_model;
use mush_core::{
    prompt, session, tools, userconfig, Config, Provider, Session, UserConfig, Workspace,
    LIST_LIMIT, READ_CAP,
};

use crate::agent::{self, spawn, AgentEvent, AgentMsg, RootHandle};
use crate::http;

/// Tools that the model may call but that must execute on the UI thread,
/// because only the UI thread knows about unsaved buffer content.
pub struct ToolCallRequest {
    pub name: String,
    pub args: Value,
    pub reply: Sender<Result<String, String>>,
}

pub enum Msg {
    Key(KeyEvent),
    /// An event from an agent actor. `conversation` identifies the tree that
    /// sent it, so an actor left over from `/new` cannot write into the new
    /// chat: events are tagged and the UI drops the stale ones.
    Agent {
        conversation: u64,
        id: u64,
        event: AgentEvent,
    },
    /// A file tool to run on this thread, on behalf of an agent acting in the
    /// main workspace. Carries the conversation for the same reason.
    Tool {
        conversation: u64,
        request: ToolCallRequest,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Agents,
    Editor,
    Chat,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Model,
    Provider,
}

/// A small modal list (models or providers) that grabs the keyboard until
/// Enter or Esc. Drawn as a centered popup by `ui::draw_picker`.
pub struct Picker {
    pub kind: PickerKind,
    pub items: Vec<String>,
    pub cursor: usize,
}

impl Picker {
    pub fn title(&self) -> String {
        match self.kind {
            PickerKind::Model => " models ".to_string(),
            PickerKind::Provider => " provider ".to_string(),
        }
    }
}

/// One entry in the agent tree. Order in the vector is tree order; ids are
/// stable, so positions do not shift while agents are alive.
pub struct AgentNode {
    pub id: u64,
    pub parent: Option<u64>,
    pub depth: usize,
    pub brief: String,
    pub running: bool,
    /// A Stop is on its way and the actor has not yielded yet. The row says so
    /// instead of looking like work that is still going somewhere.
    pub cancelling: bool,
    pub last: String,
    pub branch: Option<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

/// A single open file. Lines are stored without their terminators; whether the
/// file ends in a newline is tracked separately so saves round-trip exactly.
pub struct Buffer {
    pub rel: String,
    pub lines: Vec<String>,
    pub trailing_newline: bool,
    pub dirty: bool,
    pub cursor: (usize, usize),
    pub scroll: usize,
    pub h_scroll: usize,
}

impl Buffer {
    pub fn from_text(rel: String, text: &str) -> Self {
        let trailing_newline = text.ends_with('\n');
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        if trailing_newline {
            lines.pop();
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            rel,
            lines,
            trailing_newline,
            dirty: false,
            cursor: (0, 0),
            scroll: 0,
            h_scroll: 0,
        }
    }

    pub fn set_text(&mut self, text: &str) {
        let trailing_newline = text.ends_with('\n');
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        if trailing_newline {
            lines.pop();
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        self.lines = lines;
        self.trailing_newline = trailing_newline;
        self.clamp();
    }

    pub fn text(&self) -> String {
        let mut text = self.lines.join("\n");
        if self.trailing_newline {
            text.push('\n');
        }
        text
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn line(&self, index: usize) -> &str {
        self.lines.get(index).map(String::as_str).unwrap_or("")
    }

    pub fn clamp(&mut self) {
        let rows = self.lines.len().max(1);
        if self.cursor.0 >= rows {
            self.cursor.0 = rows - 1;
        }
        let width = self.line(self.cursor.0).chars().count();
        if self.cursor.1 > width {
            self.cursor.1 = width;
        }
    }

    fn dirty(&mut self) {
        self.dirty = true;
    }

    pub fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let (row, col) = self.cursor;
        let byte = byte_index(self.line(row), col);
        if !text.contains('\n') {
            self.lines[row].insert_str(byte, text);
            self.cursor.1 += text.chars().count();
        } else {
            let parts: Vec<&str> = text.split('\n').collect();
            let head = self.lines[row][..byte].to_string();
            let tail = self.lines[row][byte..].to_string();
            let mut replacement = Vec::with_capacity(parts.len());
            replacement.push(format!("{head}{}", parts[0]));
            for part in &parts[1..parts.len() - 1] {
                replacement.push((*part).to_string());
            }
            let last = parts[parts.len() - 1];
            replacement.push(format!("{last}{tail}"));
            let new_cursor = (row + parts.len() - 1, last.chars().count());
            self.lines.splice(row..=row, replacement);
            self.cursor = new_cursor;
        }
        self.dirty();
    }

    pub fn backspace(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            let line = &mut self.lines[row];
            let end = byte_index(line, col);
            let start = byte_index(line, col - 1);
            line.replace_range(start..end, "");
            self.cursor.1 -= 1;
        } else if row > 0 {
            let current = self.lines.remove(row);
            let previous_width = self.lines[row - 1].chars().count();
            self.lines[row - 1].push_str(&current);
            self.cursor = (row - 1, previous_width);
        } else {
            return;
        }
        self.dirty();
    }

    pub fn delete_forward(&mut self) {
        let (row, col) = self.cursor;
        let width = self.line(row).chars().count();
        if col < width {
            let line = &mut self.lines[row];
            let start = byte_index(line, col);
            let end = byte_index(line, col + 1);
            line.replace_range(start..end, "");
        } else if row + 1 < self.lines.len() {
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
        } else {
            return;
        }
        self.dirty();
    }

    pub fn move_left(&mut self) {
        if self.cursor.1 > 0 {
            self.cursor.1 -= 1;
        } else if self.cursor.0 > 0 {
            self.cursor.0 -= 1;
            self.cursor.1 = self.line(self.cursor.0).chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let width = self.line(self.cursor.0).chars().count();
        if self.cursor.1 < width {
            self.cursor.1 += 1;
        } else if self.cursor.0 + 1 < self.lines.len() {
            self.cursor.0 += 1;
            self.cursor.1 = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor.0 > 0 {
            self.cursor.0 -= 1;
            self.clamp();
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor.0 + 1 < self.lines.len() {
            self.cursor.0 += 1;
            self.clamp();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor.1 = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor.1 = self.line(self.cursor.0).chars().count();
    }

    pub fn move_top(&mut self) {
        self.cursor = (0, 0);
    }

    pub fn move_bottom(&mut self) {
        self.cursor = (self.lines.len().saturating_sub(1), 0);
    }

    pub fn scroll_view(&mut self, view_height: usize, view_width: usize) {
        let height = view_height.max(1);
        if self.cursor.0 < self.scroll {
            self.scroll = self.cursor.0;
        } else if self.cursor.0 >= self.scroll + height {
            self.scroll = self.cursor.0 + 1 - height;
        }
        let column = display_column(self.line(self.cursor.0), self.cursor.1);
        let width = view_width.max(1);
        if column < self.h_scroll {
            self.h_scroll = column;
        } else if column >= self.h_scroll + width {
            self.h_scroll = column + 1 - width;
        }
    }
}

fn byte_index(line: &str, char_col: usize) -> usize {
    line.char_indices()
        .nth(char_col)
        .map(|(index, _)| index)
        .unwrap_or(line.len())
}

/// Column including tab expansion, used for cursor placement.
pub fn display_column(line: &str, char_col: usize) -> usize {
    line.chars()
        .take(char_col)
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

pub struct App {
    pub ws: Workspace,
    pub cfg: Config,
    pub buffers: Vec<Buffer>,
    pub current: Option<usize>,
    pub focus: Focus,
    pub mode: Mode,
    pub chat: Vec<Message>,
    pub notices: Vec<String>,
    pub input: String,
    pub chat_scroll: usize,
    pub models: Vec<String>,
    pub picker: Option<Picker>,
    pub agents: Vec<AgentNode>,
    pub agent_cursor: usize,
    /// The agent whose transcript the chat shows and whose mailbox typing targets.
    pub focused: u64,
    /// Transcripts of non-root agents; the root's lives in `chat`.
    pub agent_msgs: HashMap<u64, Vec<Message>>,
    /// Steering handles: one mailbox per agent, keyed by id.
    pub agent_tx: HashMap<u64, Sender<AgentMsg>>,
    /// Each running agent's cancellation flag. The HTTP reader polls it, so a
    /// Ctrl-C stops a model call that has not answered yet — the mailbox alone
    /// cannot: the actor is blocked inside the request.
    pub agent_cancel: HashMap<u64, Arc<AtomicBool>>,
    /// Shared with the agent actors so runtime config changes apply everywhere.
    pub cfg_shared: Arc<Mutex<Config>>,
    /// The UI event channel, needed to respawn the root actor on /new.
    ui_tx: Sender<Msg>,
    /// Which conversation the live actor tree belongs to; events tagged with
    /// any other are from an abandoned tree and are ignored.
    conversation: u64,
    pub status: String,
    pub busy: bool,
    pub should_quit: bool,
    pub dirty_screen: bool,
    pub quit_armed: bool,
    pub spin: u64,
    system: Message,
}

impl App {
    pub fn new(
        ws: Workspace,
        cfg: Config,
        stored: Option<Session>,
        root: RootHandle,
        ui_tx: Sender<Msg>,
        models: Vec<String>,
        open_file: Option<String>,
    ) -> Self {
        let system = Message::system(prompt::system_prompt(&ws.root_str()));
        let chat = stored.map(|s| s.messages).unwrap_or_default();
        let mut app = Self {
            ws,
            cfg,
            buffers: Vec::new(),
            current: None,
            focus: Focus::Chat,
            mode: Mode::Normal,
            chat,
            notices: Vec::new(),
            input: String::new(),
            chat_scroll: 0,
            models,
            picker: None,
            agents: vec![AgentNode {
                id: 0,
                parent: None,
                depth: 0,
                brief: "you (root agent)".to_string(),
                running: false,
                cancelling: false,
                last: String::new(),
                branch: None,
                summary: None,
                error: None,
            }],
            agent_cursor: 0,
            focused: 0,
            agent_msgs: HashMap::new(),
            agent_tx: HashMap::from([(0, root.tx)]),
            agent_cancel: HashMap::new(),
            cfg_shared: root.cfg,
            ui_tx,
            conversation: root.conversation,
            status: String::new(),
            busy: false,
            should_quit: false,
            dirty_screen: true,
            quit_armed: false,
            spin: 0,
            system,
        };
        app.status = format!(
            "{}  •  Tab to move around · Ctrl-P pick a model",
            app.cfg.label()
        );
        app.discover_worktrees();
        if let Some(rel) = open_file {
            app.open_file(&rel);
        }
        app
    }

    /// Register git worktrees left over from earlier sessions (`mush/<id>`
    /// branches) as finished tree nodes, so `/diff`, `/merge`, `/discard` keep
    /// working after a restart.
    pub fn discover_worktrees(&mut self) {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.ws.root())
            .args(["worktree", "list", "--porcelain"])
            .output();
        let Ok(output) = output else { return };
        if !output.status.success() {
            return;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let root = self.ws.root().to_path_buf();
        // Drop stale leftovers whose worktree no longer exists.
        self.agents.retain(|node| {
            if node.brief != "leftover worktree" {
                return true;
            }
            let Some(branch) = node.branch.as_deref() else {
                return false;
            };
            let Some(id) = branch
                .strip_prefix("mush/")
                .and_then(|s| s.parse::<u64>().ok())
            else {
                return false;
            };
            root.join(format!(".mush/wt/{id}")).exists()
        });
        for block in text.split("\n\n") {
            let Some(branch) = block
                .lines()
                .find_map(|line| line.strip_prefix("branch refs/heads/mush/"))
            else {
                continue;
            };
            let Some(id) = branch.parse::<u64>().ok() else {
                continue;
            };
            if self.agents.iter().any(|node| node.id == id) {
                continue;
            }
            let full = format!("mush/{id}");
            self.agents.push(AgentNode {
                id,
                parent: None,
                depth: 1,
                brief: "leftover worktree".to_string(),
                running: false,
                cancelling: false,
                last: String::new(),
                branch: Some(full),
                summary: Some("found on startup".to_string()),
                error: None,
            });
        }
    }

    // ---------------------------------------------------------------- updates

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Key(key) => self.on_key(key),
            Msg::Agent {
                conversation,
                id,
                event,
            } => {
                if conversation == self.conversation {
                    self.on_agent(id, event);
                } else if let AgentEvent::Spawned { cmd, .. } = &event {
                    // A tree `/new` abandoned can still spawn children. They are
                    // not ours, but they must not run either — and because this
                    // event is dropped, telling the child here is the only
                    // chance it gets to end.
                    let _ = cmd.send(AgentMsg::Shutdown);
                }
            }
            Msg::Tool {
                conversation,
                request,
            } => self.on_tool(conversation, request),
        }
        self.dirty_screen = true;
    }

    /// Called once per event-loop pass so the status spinner animates.
    pub fn tick(&mut self) {
        if self.busy {
            self.spin = self.spin.wrapping_add(1);
            self.dirty_screen = true;
        }
    }

    fn on_agent(&mut self, id: u64, event: AgentEvent) {
        match event {
            AgentEvent::Spawned {
                child,
                parent,
                brief,
                depth,
                branch,
                cmd,
            } => {
                self.agent_tx.insert(child, cmd);
                self.agent_msgs.entry(child).or_default();
                self.agents.push(AgentNode {
                    id: child,
                    parent: Some(parent),
                    depth,
                    brief,
                    running: true,
                    cancelling: false,
                    last: "spawned".to_string(),
                    branch,
                    summary: None,
                    error: None,
                });
                self.busy = true;
            }
            AgentEvent::Running { cancel } => {
                // A run started, possibly one the UI did not ask for (an idle
                // agent woken by a child's result). Mark it so `busy`, the
                // spinner, and Ctrl-C agree with the actor.
                if let Some(node) = self.agent_node_mut(id) {
                    node.running = true;
                    node.cancelling = false;
                    node.error = None;
                }
                // Keep the run's flag: a Stop must be able to reach a model
                // call that is still waiting, not just the actor's mailbox.
                self.agent_cancel.insert(id, cancel);
                self.recompute_busy();
            }
            AgentEvent::Status(status) => {
                if let Some(node) = self.agent_node_mut(id) {
                    node.last = status.clone();
                }
                if id == self.focused {
                    self.status = status;
                }
            }
            AgentEvent::Message(message) => {
                if id == 0 {
                    self.chat.push(message);
                    self.save_session();
                } else if let Some(msgs) = self.agent_msgs.get_mut(&id) {
                    msgs.push(message);
                }
                self.chat_scroll = 0;
            }
            AgentEvent::Error(error) => {
                let cancelled = error == agent::CANCELLED;
                if let Some(node) = self.agent_node_mut(id) {
                    node.running = false;
                    node.cancelling = false;
                    // A cancellation is the human's doing, not a failure.
                    if !cancelled {
                        node.error = Some(error.clone());
                    }
                }
                self.agent_cancel.remove(&id);
                if cancelled {
                    if id == self.focused || id == 0 {
                        self.status = "cancelled".to_string();
                    }
                } else {
                    self.notices.push(error);
                }
                self.chat_scroll = 0;
                self.recompute_busy();
            }
            AgentEvent::Done => {
                let summary = self.last_assistant_text(id);
                if let Some(node) = self.agent_node_mut(id) {
                    node.running = false;
                    node.cancelling = false;
                    if node.summary.is_none() {
                        node.summary = summary;
                    }
                }
                self.agent_cancel.remove(&id);
                self.recompute_busy();
            }
            AgentEvent::Resync => self.resync_from_disk(),
            AgentEvent::Compact { summary } => {
                // The actor's transcript is now [system, user(summary)];
                // mirror it so nudges, saves, and the visible chat stay in
                // sync with what the model actually sees.
                let carried = Message::user(prompt::compaction_message(&summary));
                if id == 0 {
                    self.chat = vec![carried];
                    self.save_session();
                    self.notices
                        .push("context compacted — continuing from a summary".to_string());
                } else if let Some(msgs) = self.agent_msgs.get_mut(&id) {
                    *msgs = vec![carried];
                }
                self.chat_scroll = 0;
            }
        }
    }

    /// An agent stopped working: we are busy while any agent in the tree is
    /// running. If the root ends while subagents still run, say so — the root
    /// is woken with their results as they finish.
    fn recompute_busy(&mut self) {
        let root_running = self.agents.iter().any(|node| node.id == 0 && node.running);
        let waiting = self
            .agents
            .iter()
            .filter(|node| node.parent.is_some() && node.running)
            .count();
        self.busy = root_running || waiting > 0;
        if waiting > 0 && !root_running {
            self.status = format!(
                "waiting on {waiting} subagent(s) — the root resumes as they finish or reply"
            );
        }
    }

    /// The last assistant reply in an agent's transcript (its final summary).
    fn last_assistant_text(&self, id: u64) -> Option<String> {
        let messages: &[Message] = if id == 0 {
            &self.chat
        } else {
            self.agent_msgs.get(&id).map(Vec::as_slice)?
        };
        messages
            .iter()
            .rev()
            .find(|message| message.role == "assistant")
            .map(|message| message.text().trim().to_string())
            .filter(|text| !text.is_empty())
    }

    fn agent_node_mut(&mut self, id: u64) -> Option<&mut AgentNode> {
        self.agents.iter_mut().find(|node| node.id == id)
    }

    /// A shell command may have changed files behind our back. Reload any
    /// buffer without local edits so the agent and the human stay consistent.
    fn resync_from_disk(&mut self) {
        for buffer in &mut self.buffers {
            if buffer.dirty {
                continue;
            }
            if let Ok(text) = self.ws.read_file(&buffer.rel, usize::MAX) {
                if buffer.text() != text {
                    buffer.set_text(&text);
                }
            }
        }
    }

    /// A file tool for an agent in the main workspace: it runs on this thread
    /// because only the UI knows about live buffers. Only for the conversation
    /// that is still live, though — a tree `/new` abandoned must not touch the
    /// workspace, and its agent should be told so rather than blocking until
    /// the tool timeout.
    fn on_tool(&mut self, conversation: u64, request: ToolCallRequest) {
        if conversation != self.conversation {
            let _ = request
                .reply
                .send(Err("this conversation has ended".to_string()));
            return;
        }
        let result = self.exec_tool(&request.name, &request.args);
        let _ = request.reply.send(result);
    }

    // ------------------------------------------------------------ agent tools

    fn exec_tool(&mut self, name: &str, args: &Value) -> Result<String, String> {
        match name {
            "list_files" => tools::list_result(&self.ws, args, LIST_LIMIT),
            "read_file" => {
                let rel = tools::arg_string(args, "path")?;
                self.read_live(&rel)
            }
            "write_file" => {
                let rel = tools::arg_string(args, "path")?;
                let content = tools::arg_string(args, "content")?;
                self.write_live(&rel, &content)
            }
            "edit_file" => {
                let rel = tools::arg_string(args, "path")?;
                let old = tools::arg_string(args, "old_string")?;
                let new = tools::arg_string(args, "new_string")?;
                let current = self.read_for_edit(&rel)?;
                let updated = tools::edit_text(&current, &old, &new, &rel)?;
                self.write_live(&rel, &updated)?;
                Ok(format!("edited {rel}"))
            }
            other => Err(format!("unknown tool `{other}`")),
        }
    }

    /// Read through the live buffer when the file is open, so an agent always
    /// sees what the human sees. Large files are capped for the model's benefit.
    fn read_live(&self, rel: &str) -> Result<String, String> {
        Ok(truncate_for_model(self.read_for_edit(rel)?, READ_CAP))
    }

    /// The complete current text of a file: buffer first, disk second. Used by
    /// edit operations, which must never operate on a truncated view.
    fn read_for_edit(&self, rel: &str) -> Result<String, String> {
        match self.buffer_index(rel) {
            Some(index) => Ok(self.buffers[index].text()),
            None => self.ws.read_file(rel, usize::MAX),
        }
    }

    fn write_live(&mut self, rel: &str, content: &str) -> Result<String, String> {
        let mut note = "";
        if let Some(index) = self.buffer_index(rel) {
            if self.buffers[index].dirty {
                note = " (unsaved editor changes were replaced)";
            }
            self.ws.write_file(rel, content)?;
            let buffer = &mut self.buffers[index];
            buffer.set_text(content);
            buffer.dirty = false;
        } else {
            self.ws.write_file(rel, content)?;
        }
        Ok(format!("wrote {rel}{note}"))
    }

    // -------------------------------------------------------------- workspace

    fn buffer_index(&self, rel: &str) -> Option<usize> {
        let normalized = rel.trim_start_matches("./");
        self.buffers
            .iter()
            .position(|buffer| buffer.rel == normalized)
    }

    pub fn open_file(&mut self, rel: &str) {
        if let Some(index) = self.buffer_index(rel) {
            self.current = Some(index);
            self.focus = Focus::Editor;
            self.mode = Mode::Normal;
            return;
        }
        match self.ws.read_file(rel, usize::MAX) {
            Ok(text) => {
                self.buffers.push(Buffer::from_text(rel.to_string(), &text));
                self.current = Some(self.buffers.len() - 1);
                self.focus = Focus::Editor;
                self.mode = Mode::Normal;
                self.status = format!("opened {rel}");
            }
            Err(error) => self.status = error,
        }
    }

    pub fn save_current(&mut self) {
        let Some(index) = self.current else {
            self.status = "no file open".to_string();
            return;
        };
        let rel = self.buffers[index].rel.clone();
        let text = self.buffers[index].text();
        match self.ws.write_file(&rel, &text) {
            Ok(()) => {
                self.buffers[index].dirty = false;
                self.status = format!("saved {rel}");
            }
            Err(error) => self.status = error,
        }
    }

    pub fn reload_current(&mut self) {
        let Some(index) = self.current else {
            return;
        };
        let rel = self.buffers[index].rel.clone();
        match self.ws.read_file(&rel, usize::MAX) {
            Ok(text) => {
                self.buffers[index].set_text(&text);
                self.buffers[index].dirty = false;
                self.status = format!("reloaded {rel}");
            }
            Err(error) => self.status = error,
        }
    }

    pub fn current_rel(&self) -> Option<&str> {
        self.current.map(|index| self.buffers[index].rel.as_str())
    }

    fn with_buffer(&mut self, action: impl FnOnce(&mut Buffer)) {
        if let Some(index) = self.current {
            action(&mut self.buffers[index]);
        }
    }

    // ------------------------------------------------------------- chat / LLM

    fn send_message(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        if text.starts_with('/') {
            self.run_command(&text);
            return;
        }
        let target = self.focused;
        if target == 0 {
            // The human's words belong in the transcript they can see, whether
            // the root is starting a run or already in one.
            self.chat.push(Message::user(text.clone()));
            self.chat_scroll = 0;
            self.save_session();
            if self.busy {
                // Human steering while the root runs: queued as a nudge. If the
                // root's mailbox is dead (it was cancelled), fall through and
                // start a fresh run instead of spinning forever on a ghost.
                let alive = self
                    .agent_tx
                    .get(&0)
                    .map(|tx| tx.send(AgentMsg::Nudge(text.clone())).is_ok())
                    .unwrap_or(false);
                if alive {
                    self.status = "noted — folded in as the agent continues".to_string();
                    return;
                }
                self.busy = false;
                if let Some(node) = self.agent_node_mut(0) {
                    node.running = false;
                }
            }
            let mut messages = Vec::with_capacity(self.chat.len() + 1);
            messages.push(self.system.clone());
            messages.extend(self.chat.iter().cloned());
            match self.agent_tx.get(&0) {
                Some(tx) if tx.send(AgentMsg::Run(messages)).is_ok() => {
                    self.busy = true;
                    self.status = "thinking…".to_string();
                    if let Some(node) = self.agent_node_mut(0) {
                        node.running = true;
                        node.error = None;
                    }
                }
                _ => {
                    self.status = "root agent is gone — /new restarts it".to_string();
                }
            }
        } else {
            // Nudge a specific agent; running ones fold it in, idle ones rerun.
            if let Some(msgs) = self.agent_msgs.get_mut(&target) {
                msgs.push(Message::user(text.clone()));
            }
            if let Some(node) = self.agent_node_mut(target) {
                node.running = true;
                node.error = None;
            }
            match self.agent_tx.get(&target) {
                Some(tx) if tx.send(AgentMsg::Nudge(text)).is_ok() => {}
                _ => {
                    if let Some(node) = self.agent_node_mut(target) {
                        node.running = false;
                    }
                    self.status = format!("agent #{target} is gone");
                    self.recompute_busy();
                }
            }
        }
    }

    fn run_command(&mut self, command: &str) {
        let (name, rest) = command
            .split_once(' ')
            .map(|(name, rest)| (name, rest.trim()))
            .unwrap_or((command, ""));
        match name {
            "/new" | "/clear" => self.new_chat(),
            "/quit" | "/q" => self.should_quit = true,
            "/help" | "/?" => {
                self.notices.push(
                    "mush: Tab cycles agents/editor/chat · Enter sends to the focused agent · \
                     Ctrl-P pick a model · Ctrl-S save · Ctrl-R reload · Ctrl-N new chat · \
                     Ctrl-C cancel all. Commands: /provider /model /url /key /models /open \
                     /worktrees /diff /merge /discard /new /quit"
                        .to_string(),
                );
            }
            "/open" => {
                if rest.is_empty() {
                    self.status = "usage: /open <path>".to_string();
                    return;
                }
                self.open_file(rest);
            }
            "/diff" | "/merge" | "/discard" => self.worktree_command(name, rest),
            "/worktrees" => {
                self.discover_worktrees();
                let count = self
                    .agents
                    .iter()
                    .filter(|n| n.brief == "leftover worktree")
                    .count();
                self.status = if count > 0 {
                    format!("{count} leftover worktree(s) registered — /diff, /merge, /discard work on them")
                } else {
                    "no leftover worktrees".to_string()
                };
            }
            "/provider" => {
                if rest.is_empty() {
                    self.open_provider_picker();
                } else {
                    self.apply_provider(rest);
                }
            }
            "/model" => self.open_model_picker(),
            "/url" => {
                if rest.is_empty() {
                    self.status =
                        "usage: /url http://host:port — base URL of an OpenAI-compatible \
                                  endpoint"
                            .to_string();
                    return;
                }
                self.cfg.set_base_url(rest);
                self.refresh_models();
                self.status = format!("endpoint: {}", self.cfg.base_url);
                self.apply_config();
                self.persist_user_config();
            }
            "/key" => {
                if rest.is_empty() {
                    match &self.cfg.api_key {
                        Some(key) => self.status = format!("api key set ({}…)", mask_key(key)),
                        None => {
                            self.status =
                                "no api key — /key <secret> sets one (memory only)".to_string()
                        }
                    }
                    return;
                }
                self.cfg.api_key = Some(rest.to_string());
                self.apply_config();
                self.persist_user_config();
                self.status = format!(
                    "api key set ({}…) — saved to {}",
                    mask_key(rest),
                    userconfig::config_path().display()
                );
            }
            "/models" => {
                self.refresh_models();
                self.status = if self.models.is_empty() {
                    format!("no models from {}", self.cfg.models_url())
                } else {
                    format!(
                        "{} models from {}",
                        self.models.len(),
                        self.cfg.models_url()
                    )
                };
            }
            other => self.notices.push(format!("unknown command: {other}")),
        }
    }

    // ------------------------------------------------------------ providers

    /// The config is shared with every agent actor, so a runtime change here
    /// applies to the root *and* all subagents on their next request.
    fn apply_config(&self) {
        if let Ok(mut shared) = self.cfg_shared.lock() {
            *shared = self.cfg.clone();
        }
    }

    /// Remember the current setup in the home config file so the API key (and
    /// endpoint defaults) survive restarts. Never writes to the workspace.
    fn persist_user_config(&mut self) {
        let user = UserConfig {
            api_key: self.cfg.api_key.clone(),
            provider: self.cfg.provider.name().to_string(),
            base_url: self.cfg.base_url.clone(),
            model: self.cfg.model.clone(),
        };
        if let Err(error) = user.save() {
            self.status = format!("could not save home config: {error}");
        }
    }

    /// Re-fetch the model list from the current endpoint, falling back to the
    /// provider's built-in list when the endpoint cannot answer.
    pub fn refresh_models(&mut self) {
        self.models = http::list_models(&self.cfg);
    }

    fn open_model_picker(&mut self) {
        if self.models.is_empty() {
            self.refresh_models();
        }
        if self.models.is_empty() {
            self.status =
                "no models — point at an endpoint with /url or /provider first".to_string();
            return;
        }
        let cursor = self
            .models
            .iter()
            .position(|model| *model == self.cfg.model)
            .unwrap_or(0);
        self.picker = Some(Picker {
            kind: PickerKind::Model,
            items: self.models.clone(),
            cursor,
        });
    }

    fn open_provider_picker(&mut self) {
        let items: Vec<String> = Provider::ALL.iter().map(|p| p.name().to_string()).collect();
        let cursor = items
            .iter()
            .position(|name| Provider::parse(name) == Some(self.cfg.provider))
            .unwrap_or(0);
        self.picker = Some(Picker {
            kind: PickerKind::Provider,
            items,
            cursor,
        });
    }

    fn apply_provider(&mut self, name: &str) {
        let Some(provider) = Provider::parse(name) else {
            self.status = format!("unknown provider `{name}` — try deepseek or custom");
            return;
        };
        self.cfg.provider = provider;
        // Switching to DeepSeek points at its hosted API; coming back to
        // custom keeps whatever endpoint is set.
        if provider == Provider::DeepSeek {
            self.cfg.base_url = provider.default_base_url().to_string();
        }
        let known = self.cfg.default_models();
        if !known.contains(&self.cfg.model) {
            if let Some(first) = known.first() {
                self.cfg.model = first.clone();
            }
        }
        self.refresh_models();
        self.apply_config();
        self.persist_user_config();
        self.status = format!("provider: {}", provider.name());
    }

    fn key_picker(&mut self, key: KeyEvent) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Enter => {
                let kind = picker.kind;
                let item = picker.items.get(picker.cursor).cloned();
                self.picker = None;
                if let Some(item) = item {
                    self.pick(kind, &item);
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if picker.cursor + 1 < picker.items.len() {
                    picker.cursor += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                picker.cursor = picker.cursor.saturating_sub(1);
            }
            KeyCode::Char('g') | KeyCode::Home => picker.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => {
                picker.cursor = picker.items.len().saturating_sub(1)
            }
            _ => {}
        }
    }

    fn pick(&mut self, kind: PickerKind, item: &str) {
        match kind {
            PickerKind::Model => {
                self.cfg.model = item.to_string();
                self.apply_config();
                self.persist_user_config();
                self.status = format!("model: {}", self.cfg.label());
            }
            PickerKind::Provider => self.apply_provider(item),
        }
    }

    /// Print the exact git commands for an isolated agent's branch. The human
    /// merges in their own IDE — mush never auto-merges.
    fn worktree_command(&mut self, command: &str, rest: &str) {
        let Ok(id) = rest.trim().parse::<u64>() else {
            self.status = format!("usage: {command} <agent id>");
            return;
        };
        let Some(node) = self.agent_node_mut(id) else {
            self.status = format!("no agent #{id}");
            return;
        };
        let Some(branch) = node.branch.clone() else {
            self.status = format!("agent #{id} has no worktree branch (not isolated)");
            return;
        };
        let wtree = format!(".mush/wt/{id}");
        let command_text = match command {
            "/diff" => format!("git diff HEAD...{branch}"),
            "/merge" => format!("git merge {branch}    (in {})", self.ws.root_str()),
            "/discard" => format!("git worktree remove --force {wtree} && git branch -D {branch}"),
            _ => return,
        };
        self.status = command_text.clone();
        self.notices.push(command_text);
    }

    /// Reset the conversation: stop every actor in the old tree and start a
    /// fresh root, so the new chat has a clean slate and a live mailbox.
    ///
    /// Both `Ctrl-N` and `/new` land here — a chat that is cleared without
    /// restarting the root would leave the actor holding the old transcript
    /// (and a busy flag) while the UI shows an empty one.
    fn new_chat(&mut self) {
        self.stop_all();
        let root = spawn(
            self.cfg.clone(),
            self.ui_tx.clone(),
            self.ws.root().to_path_buf(),
        );
        // The respawned root owns its own config cell and conversation tag;
        // adopt both, or a later /model would never reach the agent and its
        // events would look stale.
        self.cfg_shared = root.cfg.clone();
        self.conversation = root.conversation;
        self.agent_tx = HashMap::from([(0, root.tx)]);
        self.agent_cancel.clear();
        self.agent_msgs.clear();
        // Running agents vanish with the old conversation; worktrees they left
        // behind are still reviewable (they are re-listed below).
        self.agents = vec![AgentNode {
            id: 0,
            parent: None,
            depth: 0,
            brief: "you (root agent)".to_string(),
            running: false,
            cancelling: false,
            last: String::new(),
            branch: None,
            summary: None,
            error: None,
        }];
        self.agent_cursor = 0;
        self.chat.clear();
        self.notices.clear();
        self.chat_scroll = 0;
        self.focused = 0;
        self.busy = false;
        self.spin = 0;
        self.discover_worktrees();
        self.save_session();
        self.status = "new chat — agents stopped, root restarted".to_string();
    }

    /// Ask every actor in the tree to shut down. `Shutdown`, not `Stop`: a
    /// cancelled actor goes back to waiting for work (which is what Ctrl-C
    /// should do), while `/new` needs the threads to be gone — and an actor
    /// holds its own mailbox open, so it never notices that the UI let go.
    fn stop_all(&self) {
        for tx in self.agent_tx.values() {
            let _ = tx.send(AgentMsg::Shutdown);
        }
    }

    fn save_session(&mut self) {
        let session = Session {
            root: self.ws.root_str(),
            model: self.cfg.model.clone(),
            provider: self.cfg.provider.name().to_string(),
            base_url: self.cfg.base_url.clone(),
            updated: session::now_secs(),
            messages: self.chat.clone(),
        };
        if let Err(error) = session.save(self.ws.root()) {
            self.status = format!("could not save session: {error}");
        }
    }

    pub fn scroll_chat(&mut self, delta: i64) {
        let next = self.chat_scroll as i64 + delta;
        self.chat_scroll = next.max(0) as usize;
    }

    // ------------------------------------------------------------------ input

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        let is_quit_key = ctrl && key.code == KeyCode::Char('q');
        if !is_quit_key {
            self.quit_armed = false;
        }

        if ctrl {
            match key.code {
                KeyCode::Char('q') => return self.request_quit(),
                KeyCode::Char('c') => return self.interrupt(),
                KeyCode::Char('s') => return self.save_current(),
                KeyCode::Char('r') => return self.reload_current(),
                KeyCode::Char('n') => return self.new_chat(),
                KeyCode::Char('p') => return self.open_model_picker(),
                _ => {}
            }
        }

        match key.code {
            KeyCode::Tab => return self.cycle_focus(1),
            KeyCode::BackTab => return self.cycle_focus(-1),
            _ => {}
        }

        if self.picker.is_some() {
            self.key_picker(key);
            return;
        }

        match self.focus {
            Focus::Agents => self.key_agents(key),
            Focus::Editor => self.key_editor(key, ctrl, alt),
            Focus::Chat => self.key_chat(key, ctrl, alt),
        }
    }

    fn request_quit(&mut self) {
        let dirty = self.buffers.iter().any(|buffer| buffer.dirty);
        if dirty && !self.quit_armed {
            self.quit_armed = true;
            self.status = "unsaved changes — Ctrl-Q again to discard, Ctrl-S to save".to_string();
            return;
        }
        self.should_quit = true;
    }

    /// Ask one agent's current run to stop: flip the flag its in-flight model
    /// call polls, and leave a Stop in the mailbox for everything else (a
    /// parked wait, a shell command, the next message boundary).
    fn stop_agent(&mut self, id: u64) -> bool {
        if let Some(flag) = self.agent_cancel.get(&id) {
            flag.store(true, Ordering::SeqCst);
        }
        self.agent_tx
            .get(&id)
            .map(|tx| tx.send(AgentMsg::Stop).is_ok())
            .unwrap_or(false)
    }

    /// Cancel the work that is actually running. An idle agent has nothing to
    /// cancel, and stopping it would kill the root for good (it only comes back
    /// with `/new`), so Ctrl-C leaves idle agents alone.
    fn interrupt(&mut self) {
        let targets: Vec<u64> = self
            .agents
            .iter()
            .filter(|node| node.running)
            .map(|node| node.id)
            .collect();
        let mut stopped = 0usize;
        for id in targets {
            if !self.stop_agent(id) {
                continue;
            }
            stopped += 1;
            // Say so immediately: the actor may be mid-request, and a row that
            // keeps spinning looks like the Stop was never heard.
            if let Some(node) = self.agent_node_mut(id) {
                node.cancelling = true;
            }
        }
        self.status = if stopped > 0 {
            format!("cancelling {stopped} agent(s)…")
        } else {
            "nothing running · Ctrl-Q quits · Ctrl-N starts a new chat".to_string()
        };
    }

    fn cycle_focus(&mut self, direction: i64) {
        let order = [Focus::Agents, Focus::Editor, Focus::Chat];
        let index = match self.focus {
            Focus::Agents => 0,
            Focus::Editor => 1,
            Focus::Chat => 2,
        };
        let next = (index as i64 + direction).rem_euclid(order.len() as i64) as usize;
        self.focus = order[next];
        if self.focus == Focus::Editor {
            self.mode = Mode::Normal;
        }
    }

    fn key_agents(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.agent_cursor + 1 < self.agents.len() {
                    self.agent_cursor += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.agent_cursor = self.agent_cursor.saturating_sub(1);
            }
            KeyCode::Char('g') | KeyCode::Home => self.agent_cursor = 0,
            KeyCode::Char('G') | KeyCode::End => {
                self.agent_cursor = self.agents.len().saturating_sub(1)
            }
            KeyCode::Enter => {
                if let Some(node) = self.agents.get(self.agent_cursor) {
                    self.focused = node.id;
                    self.status = format!("agent #{}: {}", node.id, node.brief);
                }
            }
            KeyCode::Char('c') => {
                if let Some(node) = self.agents.get(self.agent_cursor) {
                    let id = node.id;
                    if !node.running {
                        // Stop cancels work; an idle agent has none. Ending one
                        // is `/new`'s job.
                        self.status = format!("agent #{id} is not running");
                        return;
                    }
                    if self.stop_agent(id) {
                        if let Some(node) = self.agent_node_mut(id) {
                            node.cancelling = true;
                        }
                        self.status = format!("stopping agent #{id}…");
                    }
                }
            }
            KeyCode::Esc => {
                self.focused = 0;
            }
            _ => {}
        }
    }

    fn key_editor(&mut self, key: KeyEvent, ctrl: bool, alt: bool) {
        if self.mode == Mode::Insert {
            match key.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Enter => self.with_buffer(|buffer| buffer.insert_text("\n")),
                KeyCode::Tab => self.with_buffer(|buffer| buffer.insert_text("    ")),
                KeyCode::Backspace => self.with_buffer(|buffer| buffer.backspace()),
                KeyCode::Delete => self.with_buffer(|buffer| buffer.delete_forward()),
                KeyCode::Left => self.with_buffer(|buffer| buffer.move_left()),
                KeyCode::Right => self.with_buffer(|buffer| buffer.move_right()),
                KeyCode::Up => self.with_buffer(|buffer| buffer.move_up()),
                KeyCode::Down => self.with_buffer(|buffer| buffer.move_down()),
                KeyCode::Home => self.with_buffer(|buffer| buffer.move_home()),
                KeyCode::End => self.with_buffer(|buffer| buffer.move_end()),
                KeyCode::Char(c) if !ctrl && !alt => {
                    self.with_buffer(|buffer| buffer.insert_text(&c.to_string()))
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Esc => {}
            KeyCode::Char('d') if ctrl => self.with_buffer(|buffer| {
                for _ in 0..10 {
                    buffer.move_down();
                }
            }),
            KeyCode::Char('u') if ctrl => self.with_buffer(|buffer| {
                for _ in 0..10 {
                    buffer.move_up();
                }
            }),
            KeyCode::Char('i') if !ctrl && !alt => self.mode = Mode::Insert,
            KeyCode::Char('a') if !ctrl && !alt => {
                self.with_buffer(|buffer| buffer.move_right());
                self.mode = Mode::Insert;
            }
            KeyCode::Char('I') => {
                self.with_buffer(|buffer| buffer.move_home());
                self.mode = Mode::Insert;
            }
            KeyCode::Char('A') => {
                self.with_buffer(|buffer| buffer.move_end());
                self.mode = Mode::Insert;
            }
            KeyCode::Char('o') => {
                self.with_buffer(|buffer| {
                    buffer.move_end();
                    buffer.insert_text("\n");
                });
                self.mode = Mode::Insert;
            }
            KeyCode::Char('O') => {
                self.with_buffer(|buffer| {
                    buffer.move_home();
                    buffer.insert_text("\n");
                    buffer.move_up();
                });
                self.mode = Mode::Insert;
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                self.with_buffer(|buffer| buffer.delete_forward())
            }
            KeyCode::Char('h') | KeyCode::Left => self.with_buffer(|buffer| buffer.move_left()),
            KeyCode::Char('j') | KeyCode::Down => self.with_buffer(|buffer| buffer.move_down()),
            KeyCode::Char('k') | KeyCode::Up => self.with_buffer(|buffer| buffer.move_up()),
            KeyCode::Char('l') | KeyCode::Right => self.with_buffer(|buffer| buffer.move_right()),
            KeyCode::Char('0') | KeyCode::Home => self.with_buffer(|buffer| buffer.move_home()),
            KeyCode::Char('$') | KeyCode::End => self.with_buffer(|buffer| buffer.move_end()),
            KeyCode::Char('g') => self.with_buffer(|buffer| buffer.move_top()),
            KeyCode::Char('G') => self.with_buffer(|buffer| buffer.move_bottom()),
            _ => {}
        }
    }

    fn key_chat(&mut self, key: KeyEvent, ctrl: bool, alt: bool) {
        match key.code {
            KeyCode::Enter => self.send_message(),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) if !ctrl && !alt => self.input.push(c),
            KeyCode::Up => self.scroll_chat(1),
            KeyCode::Down => self.scroll_chat(-1),
            KeyCode::PageUp => self.scroll_chat(10),
            KeyCode::PageDown => self.scroll_chat(-10),
            KeyCode::Esc => self.input.clear(),
            KeyCode::Home => {}
            KeyCode::End => {}
            _ => {}
        }
    }
}

/// Show only the edges of a secret for confirmation without leaking it.
fn mask_key(key: &str) -> String {
    let key = key.trim();
    if key.len() <= 8 {
        return "••••".to_string();
    }
    format!("{}…{}", &key[..4], &key[key.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::Receiver;

    fn buffer(text: &str) -> Buffer {
        Buffer::from_text("test".to_string(), text)
    }

    /// A real `App` on a scratch directory, with a real (idle) root actor. The
    /// returned receiver keeps the UI channel alive for the life of the test.
    fn test_app(label: &str) -> (App, Receiver<Msg>) {
        let root = std::env::temp_dir().join(format!("mush-app-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let ws = Workspace::new(&root).unwrap();
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let handle = spawn(cfg.clone(), tx.clone(), root.clone());
        let app = App::new(
            ws,
            cfg,
            None,
            handle,
            tx,
            vec!["test-model".to_string()],
            None,
        );
        (app, rx)
    }

    /// `/new` must do what Ctrl-N does: a cleared chat with a live root, not
    /// an empty pane over a stale conversation.
    #[test]
    fn slash_new_restarts_the_root_and_clears_the_conversation() {
        let (mut app, _rx) = test_app("new");
        app.chat.push(Message::user("an old task"));
        app.notices.push("old noise".to_string());
        app.busy = true;
        let before = app.cfg_shared.clone();

        app.run_command("/new");

        assert!(app.chat.is_empty(), "the conversation is gone");
        assert!(app.notices.is_empty(), "notices are gone");
        assert_eq!(app.agents.len(), 1, "the tree is reset to the root");
        assert!(!app.busy);
        // The respawned root owns a fresh config cell and the UI adopted it;
        // without that, a later /model would never reach the agent.
        assert!(!Arc::ptr_eq(&before, &app.cfg_shared));

        // Its mailbox is alive, so the next message starts a run instead of
        // reporting that the root is gone.
        app.input = "hello".to_string();
        app.send_message();
        assert!(app.busy);
        assert_eq!(app.status, "thinking…");
    }

    /// An actor `/new` abandoned can still be finishing a request (up to the
    /// HTTP timeout); its events must not land in the new conversation. Ids
    /// collide by design — the new root is #0 too.
    #[test]
    fn events_from_an_abandoned_conversation_are_ignored() {
        let (mut app, _rx) = test_app("stale");
        let abandoned = app.conversation;
        app.run_command("/new");
        assert_ne!(app.conversation, abandoned, "a new conversation tag");
        app.chat.push(Message::user("current work"));

        app.update(Msg::Agent {
            conversation: abandoned,
            id: 0,
            event: AgentEvent::Message(Message::assistant("stale reply")),
        });
        app.update(Msg::Agent {
            conversation: abandoned,
            id: 0,
            event: AgentEvent::Compact {
                summary: "stale summary".to_string(),
            },
        });
        assert_eq!(app.chat.len(), 1, "the stale reply and summary are dropped");

        app.update(Msg::Agent {
            conversation: app.conversation,
            id: 0,
            event: AgentEvent::Message(Message::assistant("fresh reply")),
        });
        assert_eq!(app.chat.len(), 2, "the live conversation still lands");
    }

    /// A steering message sent while the root is busy is folded into the run,
    /// and it must also be visible: the human has to see what they said.
    #[test]
    fn steering_text_is_echoed_in_the_chat() {
        let (mut app, _rx) = test_app("steer");
        app.busy = true;
        app.agents[0].running = true;
        app.input = "also rename the module".to_string();

        app.send_message();

        assert_eq!(app.chat.len(), 1, "the steering message is echoed");
        assert_eq!(app.chat[0].text(), "also rename the module");
        assert!(app.status.contains("noted"), "{}", app.status);
    }

    /// The file tools of an abandoned tree must not touch the workspace: its
    /// `write_file` would otherwise land in the live buffers of the new chat.
    #[test]
    fn file_tools_from_an_abandoned_conversation_are_refused() {
        let (mut app, _rx) = test_app("stale-tools");
        let abandoned = app.conversation;
        app.run_command("/new");
        let (reply, replies) = crossbeam_channel::bounded(1);

        app.update(Msg::Tool {
            conversation: abandoned,
            request: ToolCallRequest {
                name: "write_file".to_string(),
                args: serde_json::json!({"path": "evil.txt", "content": "boom"}),
                reply,
            },
        });

        let result = replies.recv().unwrap();
        assert!(result.is_err(), "the stale tool is refused, not run");
        assert!(!app.ws.exists("evil.txt"), "the workspace is untouched");
    }

    /// A tree `/new` abandoned can still spawn children, and their `Spawned`
    /// events are dropped — so this is the only moment the UI can tell such a
    /// child to go away. Without it the child runs unseen forever.
    #[test]
    fn a_child_spawned_by_an_abandoned_tree_is_shut_down() {
        let (mut app, _rx) = test_app("stale-child");
        let abandoned = app.conversation;
        app.run_command("/new");
        let (child_tx, child_rx) = crossbeam_channel::unbounded::<AgentMsg>();

        app.update(Msg::Agent {
            conversation: abandoned,
            id: 1,
            event: AgentEvent::Spawned {
                child: 1,
                parent: 0,
                brief: "sneaky".to_string(),
                depth: 1,
                branch: None,
                cmd: child_tx,
            },
        });

        assert!(
            matches!(child_rx.recv().unwrap(), AgentMsg::Shutdown),
            "the stray child is told to end"
        );
        assert_eq!(app.agents.len(), 1, "and it never joins the new tree");
    }

    /// Ctrl-C cancels work; it must not end an idle root, which only comes
    /// back with `/new`.
    #[test]
    fn ctrl_c_stops_running_agents_only() {
        let (mut app, _rx) = test_app("interrupt");
        app.interrupt();
        assert!(app.status.contains("nothing running"), "{}", app.status);

        app.input = "hello".to_string();
        app.send_message();
        assert_eq!(app.status, "thinking…", "the idle root is still usable");

        let (mut app, _rx) = test_app("interrupt-running");
        app.agents[0].running = true;
        app.busy = true;
        app.interrupt();
        assert!(app.status.starts_with("cancelling"), "{}", app.status);
        assert!(
            app.agents[0].cancelling,
            "the row must show that a cancel is in flight"
        );
    }

    /// The cancel mark lasts exactly as long as the cancel does: the actor
    /// acknowledges by ending the run, and a fresh run clears it too.
    #[test]
    fn a_cancel_mark_clears_when_the_actor_yields() {
        let (mut app, _rx) = test_app("cancel-mark");
        let conversation = app.conversation;
        app.agents[0].running = true;
        app.interrupt();
        assert!(app.agents[0].cancelling);

        app.update(Msg::Agent {
            conversation,
            id: 0,
            event: AgentEvent::Error(agent::CANCELLED.to_string()),
        });
        assert!(!app.agents[0].cancelling, "the actor has stopped");

        app.agents[0].running = true;
        app.interrupt();
        app.update(Msg::Agent {
            conversation,
            id: 0,
            event: AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        });
        assert!(
            !app.agents[0].cancelling,
            "a new run is not a cancelled one"
        );
    }

    #[test]
    fn text_roundtrips_with_and_without_trailing_newline() {
        assert_eq!(buffer("a\nb\n").text(), "a\nb\n");
        assert_eq!(buffer("a\nb").text(), "a\nb");
        assert_eq!(buffer("").text(), "");
        assert_eq!(buffer("\n").text(), "\n");
    }

    #[test]
    fn insert_newline_splits_the_line() {
        let mut b = buffer("hello world\n");
        b.cursor = (0, 5);
        b.insert_text("\n");
        assert_eq!(b.text(), "hello\n world\n");
        assert_eq!(b.cursor, (1, 0));
    }

    #[test]
    fn backspace_joins_lines() {
        let mut b = buffer("ab\ncd\n");
        b.cursor = (1, 0);
        b.backspace();
        assert_eq!(b.text(), "abcd\n");
        assert_eq!(b.cursor, (0, 2));
    }

    #[test]
    fn multibyte_columns_are_char_based() {
        let mut b = buffer("héllo\n");
        b.cursor = (0, 1);
        b.insert_text("X");
        assert_eq!(b.text(), "hXéllo\n");
        b.cursor = (0, 3);
        b.backspace();
        assert_eq!(b.text(), "hXllo\n");
    }

    #[test]
    fn delete_forward_joins_lines_at_eol() {
        let mut b = buffer("ab\ncd\n");
        b.cursor = (0, 2);
        b.delete_forward();
        assert_eq!(b.text(), "abcd\n");
    }
}
