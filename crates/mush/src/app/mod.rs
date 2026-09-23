//! Application state and the update function.
//!
//! The UI thread owns no file state: agents read and write the workspace
//! themselves, and this module is the human's view of them — the agent tree,
//! the focused transcript, the git facts, and the message box. Every input — a
//! keystroke, an agent event — becomes a `Msg` and flows through `App::update`.
//!
//! The agents themselves live in [`tree`], which owns their ids, phases and
//! focus; the conversation lives in [`chat`], which owns every transcript the
//! screen shows, the notices, the message box and the context meter; and the
//! endpoint, model, key and context window live in [`settings`], whose one cell
//! the agents read too. This module routes messages into all of them and
//! renders what they say.

mod chat;
pub mod commands;
pub mod keys;
mod screen;
mod settings;
mod tree;

pub use chat::{Chat, Pane, Rank, SelectRows};
// The select mode's rows are painted by their positions, and the ui's frame test
// builds a `Painted` by hand to check they land on the cells: the re-export is
// for it, because a frame never names the type itself — `ChatPane::transcript`
// already holds one — and a plain `pub use` of it is an unused import in every
// non-test build.
#[cfg(test)]
pub use chat::Painted;
pub use screen::{AgentRow, AgentsPane, BarPane, ChatPane, PickerPane, Screen};
pub use settings::{ConfigCell, ConfigHandle, WindowSource};
pub use tree::{AgentNode, AgentTree, Compacting, ConversationId, Existing, Landed, Phase, Spawn};

// The id types live in their own module (two spaces, two newtypes); they keep
// the `crate::app::` path they had when `tree` defined them, so the many
// id-taking modules do not each learn a new one.
pub use crate::ids::AgentId;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use ratatui::crossterm::event::KeyEvent;

use mush_core::config::{vision_capable, BYTES_PER_TOKEN};
use mush_core::message::{Image, Message};
use mush_core::{
    git, prompt, session, text::mask_key, userconfig, Config, Provider, Session, UserConfig,
    Workspace,
};

use crate::agent::{self, spawn, AgentEvent, AgentMsg, RootHandle};
use crate::attach;
use crate::clipboard;
use crate::http;
use crate::session_save::SessionSave;

use chat::{Copied, SelectKey};
use commands::{Command, CommandError};
use keys::Intent;

pub enum Msg {
    Key(KeyEvent),
    /// Pasted text, delivered whole by the terminal's bracketed paste. Inserted
    /// in one update: a paste must not cost one message per character.
    Paste(String),
    /// A model list that finished fetching on its own thread. `endpoint` is the
    /// endpoint it was fetched from: a fetch outlives the human who typed
    /// `/url`, and a list from the endpoint they left must not land
    /// (finding A9).
    Models {
        endpoint: String,
        models: Vec<http::Model>,
    },
    /// A repository read that finished on its own thread.
    Git {
        stats: HashMap<AgentId, git::Stat>,
        status: Option<git::RepoStatus>,
        /// What a sweep found at the worktree of every agent that is *at rest*
        /// and has a branch: the decision, not the deed. The read happens off
        /// the UI thread because git is subprocesses; the removal it may lead to
        /// happens on the thread that owns the tree, because only there can
        /// "this node is not running" and the removal be one decision
        /// (finding H10).
        sweep: Vec<(AgentId, git::Reclaimable)>,
    },
    /// An event from an agent actor. `conversation` identifies the tree that
    /// sent it, so an actor left over from Ctrl-N cannot write into the new
    /// chat: events are tagged and the UI drops the stale ones.
    Agent {
        conversation: ConversationId,
        id: AgentId,
        event: AgentEvent,
    },
    /// A request from the attach socket (M3). The socket thread owns the
    /// connection and blocks on `reply` for the answer, so the socket never
    /// touches `App`'s state: it is parsed off the connection and answered in
    /// the one message loop, which is what keeps `App` the only effector.
    Attach {
        from: String,
        request: attach::Request,
        reply: Sender<attach::Response>,
    },
    /// What a clipboard read found, off the UI thread (`Ctrl-V`). The read is
    /// subprocesses with a deadline ([`crate::clipboard`]), so it cannot happen
    /// where the frame is painted; `conversation` stamps the answer the way
    /// [`Msg::Agent`]'s does, so a read that outlives a Ctrl-N is dropped
    /// instead of attaching a screenshot to a chat that never asked for one.
    Clipboard {
        conversation: ConversationId,
        result: Result<Option<Image>, String>,
    },
    /// What a clipboard write found, off the UI thread (`Enter` in the select
    /// mode). The write is subprocesses with a deadline too, so it is on a
    /// thread for the same reason the read is, and `conversation` stamps it the
    /// same way: a copy that outlives a Ctrl-N reports nothing into the new
    /// chat. `line` is the line the copy built — `copied 12 lines from #1's
    /// reply — 1,284 bytes` — said only if the clipboard took the text.
    Copied {
        conversation: ConversationId,
        line: String,
        result: Result<(), String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Agents,
    Chat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    Model,
    Provider,
    /// The lines mush wrote about the focused agent, for `/notes`. Nothing here
    /// is a choice, so `Enter` and `Esc` do the same thing.
    Notes,
    /// The keys and the commands, for `/help`. Like `Notes`, nothing here is a
    /// choice: the list exists to be read, and it opens at the top.
    Help,
}

/// One row of a `Picker`: what it stands for, and what it says.
///
/// The two are separate fields because the label is built *from* the id and
/// other facts: a model row reads `{id} · {tokens}`. Reading the id back out of
/// that label made both the bullet and the pick take the text before the first
/// separator, so one model id containing `" · "` was drawn as one thing and
/// chosen as another (Tier 3 §7).
///
/// `id` is the machine value — a model id, a provider name — and `None` for a
/// list that is not a choice (`Notes`, `Help`).
#[derive(Clone, Debug)]
pub struct PickerItem {
    pub id: Option<String>,
    pub label: String,
}

/// A small modal list that grabs the keyboard until Enter or Esc: the models,
/// the providers, and the notes mush wrote about the focused agent. Drawn as a
/// centered popup by `ui::draw_picker`.
pub struct Picker {
    pub kind: PickerKind,
    pub items: Vec<PickerItem>,
    pub cursor: usize,
}

impl Picker {
    pub fn title(&self) -> String {
        match self.kind {
            PickerKind::Model => " models · Enter picks ".to_string(),
            PickerKind::Provider => " provider · Enter picks ".to_string(),
            PickerKind::Notes => format!(
                // The position is part of the title because the list opens in
                // the middle of a long note (see `open_notes_picker`): without
                // it, a popup whose head is above the fold looks like the whole
                // note, and nothing says a keypress reads the rest.
                " notes · newest last · line {}/{} ",
                self.cursor + 1,
                self.items.len()
            ),
            PickerKind::Help => format!(
                // Same reason: the list is longer than any popup, so the title
                // says where in it the reader is.
                " help · line {}/{} ",
                self.cursor + 1,
                self.items.len()
            ),
        }
    }

    /// What the popup's own last row says the keys do. It belongs to the picker
    /// rather than to the painter because it is the same fact as the title: what
    /// this list is for. A long list is paged the same way the transcript is,
    /// so `PgUp`/`PgDn` are named beside `j`/`k`.
    pub fn hint(&self) -> &'static str {
        match self.kind {
            PickerKind::Model | PickerKind::Provider => {
                " j/k or PgUp/PgDn · Enter pick · Esc cancel "
            }
            PickerKind::Notes | PickerKind::Help => " j/k or PgUp/PgDn scrolls · Esc closes ",
        }
    }
}

/// `500k`, `8192`, `1.2M` — the way a token count wants to be read. A run's
/// real numbers come from the endpoint and are large (`1213866`), and a number
/// a human has to count digits in says nothing at a glance.
pub fn tokens_label(tokens: usize) -> String {
    /// One decimal of the unit, and no `.0`: `500k` stays `500k`, and 1,213,866
    /// reads as `1.2M` rather than as `1M` (the first is the size, the second is
    /// a rounding that hides it).
    fn scaled(tokens: usize, divisor: f64, unit: &str) -> String {
        let text = format!("{:.1}", tokens as f64 / divisor);
        format!("{}{unit}", text.strip_suffix(".0").unwrap_or(&text))
    }
    // 999,950 is the last count that rounds to `1000k`; above it the million
    // unit is the honest one.
    if tokens >= 999_950 {
        scaled(tokens, 1_000_000.0, "M")
    } else if tokens >= 1_000 {
        scaled(tokens, 1_000.0, "k")
    } else {
        tokens.to_string()
    }
}

/// One image as a row reads it: `shots/a.png (png · 1.2 MB)`.
///
/// The one spelling, used by the message box's attachment rows and by the row
/// the transcript paints under the words: a picture the human is about to send
/// and that same picture once it is in the conversation must read alike, or the
/// two surfaces are describing two different things. The format is the mime
/// without its `image/` head, exactly as a shed payload's placeholder spells it.
/// The path goes through [`mush_core::text::sanitize`], like every other row a
/// name reaches: these rows are painted raw, and a file name is the one part of
/// this label an outside hand wrote.
pub fn image_label(image: &Image) -> String {
    let format = image.mime.strip_prefix("image/").unwrap_or(&image.mime);
    format!(
        "{} ({format} · {})",
        mush_core::text::sanitize(&image.path),
        size_label(image.bytes.len())
    )
}

/// The line an image gets when the model is not documented to accept image
/// parts. One sentence for one fact, because two gates stop a picture on it —
/// the box that takes the attachment and the wire that sends it — and a human
/// who meets both must not read two different explanations of the same
/// refusal.
fn blind_model_line(model: &str) -> String {
    format!(
        "`{model}` is not a model mush knows to accept images — Ctrl-P picks one whose row \
         documents vision"
    )
}

/// How many bytes of pictures the message box may hold: eight of the files the
/// transport already caps one at ([`mush_core::workspace::IMAGE_FILE_CAP`]).
///
/// The window cannot be this bound, and the arithmetic is why: a picture is
/// priced by its pixels, so a 100×100 png weighing 2 MB costs fourteen tokens —
/// a hundred of them pass every token bound the window has while the box holds
/// 200 MB of bytes and the copies in `.mush/paste/` grow by the same. What the
/// box is made of is bytes, so bytes are what the box counts: eight files of
/// the size the transport already lets one be is the queue a message may hold.
const BOX_IMAGE_BYTES: usize = (mush_core::workspace::IMAGE_FILE_CAP * 8) as usize;

/// A byte count the way a glance wants it: `900 B`, `340 KB`, `1.2 MB`. One
/// decimal for the unit that needs one — a megabyte is where the rounding is
/// visible, and `1.2` says more than `1258` or than a bare `1`.
fn size_label(bytes: usize) -> String {
    const KB: usize = 1_000;
    const MB: usize = 1_000_000;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{} KB", bytes / KB)
    } else {
        let text = format!("{:.1}", bytes as f64 / MB as f64);
        format!("{} MB", text.strip_suffix(".0").unwrap_or(&text))
    }
}

/// Whether a stored transcript ends where a finished run left it: the last
/// message is an assistant turn that asked for no tools, which is an answer and
/// nothing else. A run that failed before it changed the transcript — the
/// endpoint refusing the first request of a resumed agent — leaves one behind,
/// so this is what tells a restored agent's own work apart from a later
/// refusal (see `restore_agents`).
fn ended_on_an_answer(messages: &[Message]) -> bool {
    matches!(
        messages.last(),
        Some(message) if message.role == "assistant" && message.tool_calls().is_empty()
    )
}

/// What a restored row's phase and summary mean in the books' vocabulary: the
/// outcome a parent's `status`/`wait` should report for a child the tree
/// brought back (finding H25).
///
/// `None` for a phase with no result behind it: idle, or a run the tree still
/// shows in flight. The `Done` fallback is the actor's own — a run that
/// produced no words becomes `Outcome::Finished("(finished)")` at the actor
/// boundary, and a row whose last run left no text is that same run seen from
/// the UI.
fn seeded_outcome(phase: &Phase, summary: Option<&str>) -> Option<agent::Outcome> {
    match phase {
        Phase::Done => Some(agent::Outcome::Finished(
            summary.unwrap_or("(finished)").to_string(),
        )),
        Phase::Stopped => Some(agent::Outcome::Stopped(agent::Stop::Unrecorded)),
        Phase::CutOff => Some(agent::Outcome::CutOff),
        Phase::Failed(error) => Some(agent::Outcome::Failed(error.clone())),
        Phase::Idle
        | Phase::Thinking
        | Phase::Activity(_)
        | Phase::Compacting(_)
        | Phase::Cancelling => None,
    }
}

/// What a cut-off agent's own pane says under its `⚠` row.
///
/// The row is the mark; this is the sentence a human needs to act on it — that
/// the run did not produce a result, and that whatever it wrote is *uncommitted*,
/// which is what makes the difference between "recoverable" and "lost"
/// (finding H2). One wording, shared by the two paths that can prove a run was
/// cut off: a restored status that still said `running`, and an actor whose
/// mailbox is dead with work in flight.
fn cut_off_notice() -> String {
    "cut off — its run never ended, so nothing was committed; its work is where it left it"
        .to_string()
}

/// `/help`: the key table, then the command table.
///
/// Both tables are the same one source their CLI counterparts print —
/// [`keys::help_table`] is `mush --help`'s KEYS block and [`commands::table`]
/// is its COMMANDS block — so no surface can advertise a binding or a command
/// the others do not. `/help` used to name six keys by hand and miss `j`/`k`,
/// `Enter`, `c`, `Esc` and `Ctrl-Q`; rendering [`keys::KEYS`] here is what stops
/// a human learning the keyboard from a subset of it (the key half of finding
/// B2). It is a notice rather than a status line: it is a thing to read, not a
/// thing that just happened.
/// The two tables as one text, wrapped to the width the surface reading it
/// paints at: `mush --help` passes `usize::MAX`, the `/help` popup passes the
/// columns it has.
fn help_notice(width: usize) -> String {
    format!(
        "mush keys:\n{}\nCommands:\n{}",
        keys::help_table_at(width),
        commands::table_at(&mush_core::provider::names_piped(), width)
    )
}

/// An age the way a glance wants it: seconds, then minutes, then hours — never
/// a five-digit number that takes arithmetic to read.
pub fn short_age(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

/// Below this the screen has no room to be honest: the painter shows a single
/// notice instead of shreds, and [`App::below_floor`] stops keys that would act
/// without anything visible to show for it. One pair of numbers, owned here
/// rather than by the painter, so the notice, the input gate and the tests
/// cannot disagree about where the floor is (finding P11 / refactor B3).
pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 10;

/// Whether a terminal of this size is below the floor. The predicate is one
/// function so the painter — which passes the frame's own size — and the input
/// gate — which passes the size `main` reported — cannot disagree.
pub const fn is_below_floor(width: u16, height: u16) -> bool {
    width < MIN_WIDTH || height < MIN_HEIGHT
}

/// A transient line for the workspace bar. Nothing here describes work in
/// progress — that is derived from the agents' phases — so it cannot go stale.
/// `Info` fades; `Error` stays until something replaces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Error,
    /// The warning a quit waits on: `Ctrl-Q` again and mush goes, killing what
    /// the line names (finding H9).
    ///
    /// A kind of its own because it needs two rules nothing else does. It is
    /// ranked an alert, like a failure: the human who pressed `Ctrl-Q` must
    /// never see the derived activity line instead — `waiting on 2 subagent(s)`
    /// is exactly the state they are quitting *from*, and a warning hidden
    /// behind it is how a second press becomes a silent kill. And it fades like
    /// `Info`: an arm nobody can see any more is not an arm, so the warning and
    /// the arming it stands for end together.
    Quit,
    /// The warning a new chat waits on: `Ctrl-N` again and every transcript
    /// goes, the conversation kept as `.mush/session.json.previous` (finding
    /// C4).
    ///
    /// A kind of its own for the two rules a warning has, exactly as
    /// [`StatusKind::Quit`]: it is ranked an alert, so the derived activity line
    /// cannot hide the sentence that explains the second press — an arm nobody
    /// can see is not an arm — and it fades like `Info`, so the warning and the
    /// arming it stands for end together.
    NewChat,
}

/// How long an `Info` line is worth showing. Long enough to read after a
/// command, short enough that it never becomes furniture.
const INFO_TTL: Duration = Duration::from_secs(5);

/// How often the transcript foot's dots move: `thinking.`, `thinking..`,
/// `thinking...`, one a second while anything is in flight, with the run's own
/// words ([`Phase::words`]) in front of them.
///
/// The cadence *is* the animation. [`App::tick`] runs on the event loop's 30 ms
/// poll — that is the heartbeat for the clock, the git snapshot and the lines
/// that age off the screen — and an animation driven by it turned over thirty
/// times a second: a flicker, not a pulse (`chat::dotted`).
const DOT_PERIOD: Duration = Duration::from_secs(1);

/// What a stop key answers with when there is no work to stop.
///
/// The same words from both stop keys — `Ctrl-C` on an idle tree and `Ctrl-X`
/// with nothing in flight — because it is one fact and one place it is read.
///
/// The hint names the half of `Ctrl-N` a human cannot undo. "starts a new
/// chat" alone read as if only a beginning were at stake, the understatement
/// D1 fixed in the key's own help; the key stops every agent, kills what they
/// left running and drops every transcript (root and children). Here the line
/// is only ever read when *nothing is running*, so the half still worth a
/// warning is the one that goes: every transcript. It fits the row the badge
/// and the two key hints already share (finding H26).
const NOTHING_RUNNING: &str = "nothing running · Ctrl-Q quits · Ctrl-N drops every transcript";

/// How long the session file may lag the conversation.
///
/// The human reads the transcript, not the file, and the file is only read
/// again by the *next* mush. What the interval buys is the UI thread: a save
/// rebuilds the snapshot — every live transcript, cloned, then handed to the
/// writer — and that rebuild is what a long run would otherwise pay once a
/// second. What it costs is the crash window: at most a minute of
/// machine-generated conversation — streamed responses and tool results — is
/// lost when the process dies, and nothing else, because the human's own turn,
/// a fold, a new chat and quitting are all written before they return (see
/// `App::flush_session` and its callers).
const SESSION_DEBOUNCE: Duration = Duration::from_secs(60);

/// How wide a job's handle may be: the same bound as an agent's title, for the
/// same reason — a handle, whose full text is the report in the transcript.
const JOB_TITLE_COLUMNS: usize = 30;

/// `short_age`'s companion for a job: the command's own work, on one line.
///
/// A `command` the model wrote can be thirty lines of heredoc with a
/// `cd /w &&` in front of it, and the bar and the row's footer each have one
/// row to name it in: what is left is the last clause of the first line
/// (`cargo build` out of `cd /w && cargo build --release`), collapsed by
/// [`mush_core::text::first_line`] and bounded like an agent's title. One
/// derivation, so the two surfaces cannot spell the same job differently.
fn job_title(command: &str) -> String {
    let first = mush_core::text::first_line(command);
    let clause = first.rsplit("&&").next().unwrap_or(&first).trim();
    mush_core::text::truncate(clause, JOB_TITLE_COLUMNS)
}

/// How much of one bar row the cursor row's brief may spend.
///
/// `Enter` in the tree says which row the chat pane now shows, and it says it
/// with the brief the row carries — a sentence written *for a model*, often a
/// paragraph. The bar paints its line whole and does no width arithmetic of its
/// own (the rule [`QUIT_LINE_COLUMNS`] states), so the brief used to run off
/// the end of the row and lose its tail, which is the part that says what the
/// task is. This is not the agent *row's* bound (`app/tree.rs`'s
/// `TITLE_COLUMNS` is 24): that number is a handle on a row, and this one is a
/// sentence on the bar, so the two are two decisions.
///
/// The line's head, `agent #id: `, is not part of the budget: it is what says
/// whose brief this is, and it is short enough that keeping it whole costs the
/// brief a few columns. What is left is one row at the ubiquitous 80×24 — 73
/// columns, the ` chat ` badge and the space after it taking seven of the
/// eighty — with a column spare.
const CURSOR_LINE_COLUMNS: usize = 72;

/// How much of one bar row a typo'd command's name may spend.
///
/// The line is `unknown command: {name} — /help lists them`, and the name is
/// whatever the human typed: a paste is unbounded, and the bar paints its line
/// whole and does no width arithmetic of its own, so an over-long one would
/// have its tail clipped — and the tail is the half that says where the
/// commands are. The name is therefore the part that gets cut, visibly, and
/// this is what one row leaves for it: 73 columns at the ubiquitous 80×24, less
/// the 36 the fixed head and tail take between them (finding D3).
const UNKNOWN_NAME_COLUMNS: usize = 36;

/// How much of one bar row the quit warning may spend.
///
/// One row at the ubiquitous 80×24 is 73 columns — the ` chat ` badge and the
/// space after it take seven of the eighty — and a column is left spare. The
/// bar paints its line whole and does no width arithmetic of its own, so a
/// warning longer than this would have its tail clipped: the last names, and
/// the count of what could not be named, would be the parts silently lost
/// (finding H9).
const QUIT_LINE_COLUMNS: usize = 72;

/// The line a quit over live work paints: `Ctrl-Q again quits · kills #0
/// thinking, #3 wait + 1 job`.
///
/// The first clause is the decision the human is making twice; everything after
/// it is [`App::what_a_quit_kills`], one item per thing, joined until the row
/// is spent. What does not fit is *counted*, never dropped: a tree with more
/// live agents than a row can name says `+2 more`, which is honest, where a
/// clipped list reads as a short one. The first item is taken whatever it
/// costs — an item is an id, a word and a count, so it always fits — because a
/// warning that named nothing would be the silence this line exists to end.
fn quit_warning(items: &[String], columns: usize) -> String {
    const OPENING: &str = "Ctrl-Q again quits · kills ";
    /// What the line ends with when `left` things go unnamed.
    fn more(left: usize) -> String {
        format!(", +{left} more")
    }
    let mut text = OPENING.to_string();
    let mut width = OPENING.chars().count();
    let mut named = 0;
    for (index, item) in items.iter().enumerate() {
        let separator = if index == 0 { 0 } else { 2 }; // ", "
                                                        // Stopping after this item means ending with the count of the rest, so
                                                        // the room that tail needs is reserved now — a line that fit its names
                                                        // and not its count would drop exactly the word that keeps it honest.
        let tail = if index + 1 < items.len() {
            more(items.len() - index - 1).chars().count()
        } else {
            0
        };
        if index > 0 && width + separator + item.chars().count() + tail > columns {
            break;
        }
        width += separator + item.chars().count();
        if index > 0 {
            text.push_str(", ");
        }
        text.push_str(item);
        named += 1;
    }
    if named < items.len() {
        text.push_str(&more(items.len() - named));
    }
    text
}

/// The line the first `Ctrl-N` over a non-empty conversation paints:
/// `Ctrl-N again clears 12 lines — kept as .mush/session.json.previous`.
///
/// Both halves are the promise the second press makes: what goes — a count of
/// lines, because a conversation is read in lines — and where it can be
/// reclaimed. The name is spelled from [session]'s own constants rather than
/// retyped here, so the line and the file the copy is written to cannot drift.
fn new_chat_warning(lines: usize) -> String {
    let lines = if lines == 1 {
        "1 line".to_string()
    } else {
        format!("{lines} lines")
    };
    format!(
        "Ctrl-N again clears {lines} — kept as {}/{}",
        session::MUSH_DIR,
        session::PREVIOUS_FILE
    )
}

#[derive(Clone, Debug)]
pub struct Status {
    pub kind: StatusKind,
    pub text: String,
    pub set_at: Instant,
}

/// The write road as a value: the function [`App::write_clipboard`] hands the
/// select mode's text to. A type of its own because the one field it describes
/// is the whole seam, and the trait bounds are its contract, not an accident of
/// where it is written.
type ClipboardWrite = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

pub struct App {
    pub ws: Workspace,
    /// The endpoint, the model, the key and the context window: the copy the
    /// screen reads and the cell every actor reads, in one owner.
    pub cell: ConfigCell,
    pub focus: Focus,
    /// Whether the zen view is on: the focused pane takes the whole screen
    /// (`Ctrl-F`).
    ///
    /// A view, so it changes what a frame paints and nothing else: no notice is
    /// said, no session is written, no message goes out. The mouse is never
    /// captured on purpose (finding K3), so the terminal owns selection and a
    /// drag takes a rectangle of screen cells — at 80 columns that rectangle
    /// starts in the agents pane, which is how a paragraph copied from the
    /// conversation arrives with the tree's lines in front of it. `Ctrl-N`
    /// leaves it as the human set it: the view is the human's, not the
    /// conversation's.
    pub zen: bool,
    /// The conversation: the transcripts the screen shows, the notices, the
    /// message box and the context meter, in one value.
    pub chat: Chat,
    pub models: Vec<http::Model>,
    pub picker: Option<Picker>,
    /// The main worktree's branch, dirty count, and uncommitted line delta.
    pub git: Option<git::RepoStatus>,
    /// The write road as a value the app holds: what the select mode's `Enter`
    /// hands the text to, and the one place that road can be stood in for.
    ///
    /// The default is [`clipboard::write_text`] — the real `wl-copy`/`xclip`/
    /// `pbcopy` sequence. The seam exists because `Enter` is a *key*, and a key
    /// mush can press must be a key a test can press: without it, a test that
    /// copied would either clobber the human's real clipboard (the key is the
    /// one the whole mode exists for, and a test run has no business writing
    /// to the clipboard the human is using) or depend on which of the three
    /// programs the machine happens to have on `PATH`. The read road needs no
    /// such seam: its answer is a message a test hands in directly
    /// (`Msg::Clipboard`), and pressing `Ctrl-V` where the machine has no
    /// reader costs a status line and nothing else.
    write_clipboard: ClipboardWrite,
    /// The agents, their phases, the focus and the per-agent mailboxes,
    /// cancel flags and git stats.
    pub tree: AgentTree,
    /// Where a snapshot of this conversation goes. The serialization and the
    /// write happen on the writer's own thread; this thread only hands the
    /// state over.
    session_save: Arc<dyn SessionSave>,
    /// When the conversation last changed with no snapshot handed over since.
    /// `None` means the file is current. `tick` is where it is turned into a
    /// write, so a burst of messages costs one rebuild instead of one each.
    session_dirty_at: Option<Instant>,
    /// The UI event channel, needed to respawn the root actor on Ctrl-N.
    ui_tx: Sender<Msg>,
    /// When the git snapshot was last taken, so a long run refreshes it.
    git_at: Option<Instant>,
    /// A `git` read is already running on its own thread; asking again would
    /// only queue another one behind it.
    git_in_flight: bool,
    /// A transient line for the bar: what just happened, or what went wrong.
    /// Work in progress does not live here — it is derived from the phases.
    pub status: Option<Status>,
    pub should_quit: bool,
    pub dirty_screen: bool,
    /// The terminal's size, as of the last size `main` reported. `/notes` wraps
    /// its lines to the popup this size paints them in, and [`App::below_floor`]
    /// reads it to refuse input the screen cannot show the effect of — so the
    /// floor is a state `App` knows rather than only the painter's early return
    /// (finding P11 / refactor B3).
    term_width: u16,
    term_height: u16,
    /// The beat the transcript foot's dots are on ([`DOT_PERIOD`]), and the
    /// clock that says when the next one is owed. [`App::tick`] advances it
    /// while anything is in flight; the dots read it as `thinking.`,
    /// `thinking..`, `thinking...` — or the phase's own words in front of them.
    pub spin: u64,
    spin_at: Instant,
}

impl App {
    /// The configuration every screen reads: the UI's copy of the cell.
    pub fn cfg(&self) -> &Config {
        self.cell.ui()
    }

    /// Record the terminal size `main` read, so a `/notes` report can be
    /// wrapped to the popup that size paints and so the floor is known. One
    /// setter, called at startup and from the resize event — the only two
    /// places the terminal's size changes.
    pub fn set_term_size(&mut self, width: u16, height: u16) {
        self.term_width = width;
        self.term_height = height;
    }

    /// Whether the terminal is too small for anything but the floor notice.
    /// `ui.rs` paints the notice; this is what stops a key from acting with no
    /// visible result — the destructive `Ctrl-N` on a screen showing only
    /// `mush needs at least 40×10` was the bug this exists for (finding P11).
    pub fn below_floor(&self) -> bool {
        is_below_floor(self.term_width, self.term_height)
    }

    pub fn new(
        ws: Workspace,
        cell: ConfigCell,
        stored: Option<Session>,
        root: RootHandle,
        ui_tx: Sender<Msg>,
        session_save: Arc<dyn SessionSave>,
    ) -> Self {
        let system = Message::system(prompt::system_prompt(&ws.root_str()));
        let (messages, stored_agents, stored_notices) = match stored {
            Some(session) => (session.messages, session.agents, session.notices),
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let mut app = Self {
            ws,
            cell,
            focus: Focus::Chat,
            zen: false,
            chat: Chat::new(system, messages),
            // Empty until a fetch says otherwise: the model list is discovered
            // on its own thread so nothing about an endpoint delays the first
            // frame (finding A9), and `/model` refetches if this is still
            // empty when the human asks.
            models: Vec::new(),
            picker: None,
            git: None,
            write_clipboard: Arc::new(clipboard::write_text),
            tree: AgentTree::rooted(root),
            session_save,
            session_dirty_at: None,
            ui_tx,
            git_at: None,
            git_in_flight: false,
            status: None,
            should_quit: false,
            dirty_screen: true,
            // The ubiquitous terminal, and above the floor: `main` reports the
            // real size before the first key can be read.
            term_width: 80,
            term_height: 24,
            spin: 0,
            spin_at: Instant::now(),
        };
        // The failures come back before the agents do, because the agent that
        // went back to idle takes its line with it (see `restore_agents`).
        app.chat.restore_notices(stored_notices);
        // The sweep runs before the agents come back: a restored agent's actor
        // is built on its worktree (`agent::revive`), and a branch whose work
        // the main checkout already holds must be gone before that, or the
        // same breath deletes the directory the actor was just built in — and
        // the row it answers for is frozen as `merged` while the actor still
        // writes into a path that no longer exists (finding H21).
        app.reclaim_isolated();
        app.restore_agents(stored_agents);
        app.discover_worktrees();
        app.refresh_git();
        app
    }

    /// Decide which of a stored file's rows the tree can hold, and spend every
    /// id the file names, before one of them is registered.
    ///
    /// A session file is hand-editable, and a repository can still commit one —
    /// an ignore rule does not untrack a tracked file — so its rows are not
    /// trusted. A stored id has to be a *child's*: nonzero (0 is
    /// [`AgentId::ROOT`]), not already taken by the root or by a row accepted
    /// before it, reachable from the root through the file's own parent links (a
    /// dangling parent or a cycle has no chain to the root), and one the id
    /// counter can be kept above — the floor [`AgentTree::reserve_agents`]
    /// exists to keep (finding B1) — so a `u64::MAX` row, which has nothing
    /// above it, is refused too. Trusting the file instead registered a `0` row
    /// *as the root*: its transcript replaced the root's conversation on screen
    /// and its mailbox replaced the root's actor, and a `u64::MAX` row panicked
    /// the debug build at `agent.id + 1` (finding C9).
    ///
    /// Every row the file names raises the floor before it is judged — refused
    /// rows included, because the repository may hold a `mush/<id>` branch or a
    /// `.mush/wt/<id>` worktree whatever the row said. The reservation is
    /// saturated, and skipped where it saturates: a row at the ceiling has no
    /// floor above it, and pinning the counter there would hand the next draw a
    /// number it cannot pass. One bad row is refused and reported; the rows
    /// after it still restore, because a file is not all-or-nothing.
    fn vet_stored_agents(
        &mut self,
        stored: Vec<session::AgentSession>,
    ) -> (Vec<session::AgentSession>, Vec<String>) {
        let file = self.ws.rel(&session::session_path(self.ws.root()));
        // The ids whose parent chain reaches the root: the root's own, plus
        // every row accepted below. A row whose parent is in here is a child the
        // restore can attach; one whose parent is not would need its parent
        // registered first, which the file's spawn order should have done.
        let mut reachable: HashSet<u64> = HashSet::from([AgentId::ROOT.0]);
        let mut accepted = Vec::new();
        let mut refused = Vec::new();
        for agent in stored {
            let floor = agent.id.saturating_add(1);
            if floor > agent.id {
                self.tree.reserve_agents(floor);
            }
            let why = if agent.id == AgentId::ROOT.0 {
                "it holds the root's id"
            } else if agent.id == u64::MAX {
                "the counter cannot be kept above its id"
            } else if reachable.contains(&agent.id) {
                "its id is already taken"
            } else if !agent
                .parent
                .is_some_and(|parent| reachable.contains(&parent))
            {
                "its parent chain does not reach the root"
            } else {
                reachable.insert(agent.id);
                accepted.push(agent);
                continue;
            };
            refused.push(format!(
                "could not restore agent #{} in {file} — {why}; the row was skipped",
                agent.id
            ));
        }
        (accepted, refused)
    }

    /// Adopt the subagents of the previous conversation: their nodes, their
    /// transcripts, and — through `agent::revive` — a live actor each, so a
    /// follow-up message continues the agent instead of starting over.
    ///
    /// A restored agent whose worktree is gone continues in the main checkout,
    /// which is where its work ended up once it was merged.
    fn restore_agents(&mut self, stored: Vec<session::AgentSession>) {
        if stored.is_empty() {
            return;
        }
        let (stored, refused) = self.vet_stored_agents(stored);
        for line in refused {
            self.session_unreadable(line);
        }
        let cfg = self.cell.handle();
        let ui_tx = self.ui_tx.clone();
        let conversation = self.tree.conversation().0;
        let handles = self.tree.handles();
        let root = self.ws.root().to_path_buf();
        // The agents whose last run never ended, and who they report to. The
        // parent cannot be told while this loop runs (a child may be restored
        // before its parent is a node), so it is told after it.
        let mut cut_off: Vec<(AgentId, Option<AgentId>)> = Vec::new();
        for agent in stored {
            // Keep the counter above every restored id, or the next spawn hands
            // a live child an id a restored agent already holds (finding B1).
            self.tree.reserve_agents(agent.id + 1);
            let (phase, summary) = match &agent.status {
                session::StoredStatus::Done => (Phase::Done, agent.summary.clone()),
                session::StoredStatus::Stopped => (Phase::Stopped, agent.summary.clone()),
                // A stored failure is only the last thing that happened if the
                // transcript still ends where a failure would have left it. A
                // run that fails on its first request appends nothing, so an
                // agent that had already answered (its transcript ends on a
                // plain assistant turn) is restored as done, with the refusal
                // as its line. Showing `✗` over a transcript that ends
                // "Done." claims the agent lost work it still has — which is
                // how a wall of `✗` appeared over finished work when a
                // thinking endpoint refused a replayed turn.
                session::StoredStatus::Failed(error) if ended_on_an_answer(&agent.messages) => {
                    (Phase::Done, Some(format!("last attempt refused: {error}")))
                }
                session::StoredStatus::Failed(error) => {
                    (Phase::Failed(error.clone()), agent.summary.clone())
                }
                // A run that was still in flight when the file was last written
                // never ended: the process — or the harness around it — went
                // away first. Three endings used to be flattened into `Idle`
                // here, and the one that cost real work was this one: on screen
                // and in the file a killed agent looked exactly like an agent
                // nobody had ever asked to do anything, and its work sat
                // uncommitted until a human went looking (finding H2).
                //
                // It is not `Stopped` either: a stop is the human's Ctrl-C and
                // the actor is alive to be nudged again, while this run died
                // where it stood. `Running` and `CutOff` are the same fact a
                // restart apart — the file's last word was that a run was in
                // flight, or that one never ended — and they come back the
                // same way.
                session::StoredStatus::Running | session::StoredStatus::CutOff => {
                    (Phase::CutOff, agent.summary.clone())
                }
                session::StoredStatus::Idle => (Phase::Idle, agent.summary.clone()),
            };
            // The row is derived and cannot lie, so the line that disagrees with
            // it is the one that goes: an agent come back idle or done keeps no
            // failure notice, and a `✓` row with a `!` line under it is exactly
            // the pair this rule exists to prevent.
            if !matches!(phase, Phase::Failed(_)) {
                self.chat.clear_notes_for(AgentId(agent.id));
            }
            let landed = agent.landed.map(|landed| match landed {
                session::StoredLanded::Merged => Landed::Merged,
                session::StoredLanded::NothingCommitted => Landed::NothingCommitted,
                session::StoredLanded::Discarded => Landed::Discarded,
            });
            // One decision, two readers: a stored branch whose worktree is gone
            // is not this agent's any more. `revive` runs such an agent in the
            // main checkout, and a node that kept the branch would offer a diff
            // against a reclaimed directory, paint a dead path
            // in the footer, and refuse a nudge the actor would have run
            // (finding U13). Computed once, handed to both.
            let branch = agent::live_branch(&root, agent.id, agent.branch.clone());
            let parent = agent.parent.map(AgentId);
            // The row is derived and cannot lie; a `⚠` row with nothing under it
            // would say *that* a run never ended without saying what follows
            // from it, which is the whole of what the human needs (finding H2).
            if phase == Phase::CutOff {
                self.chat
                    .note_cut_off_for(AgentId(agent.id), cut_off_notice());
                cut_off.push((AgentId(agent.id), parent));
            }
            let tx = agent::revive(
                handles.clone(),
                cfg.clone(),
                ui_tx.clone(),
                conversation,
                root.clone(),
                agent::ReviveSpec {
                    id: agent.id,
                    depth: agent.depth.max(1),
                    brief: agent.brief.clone(),
                    branch: branch.clone(),
                    messages: agent.messages.clone(),
                    // A restored child reports to the parent the tree names: its
                    // row is on screen, the parent's books were seeded with it
                    // (`seed_children`, below), and the human's own nudge
                    // already tells that parent it is running
                    // (`tell_parent_running`) — so the completion is the only
                    // thing that can ever settle those books, and a child that
                    // reported into a dead channel left a `wait` burning its
                    // whole cap under a row that said `✓` (§8.39).
                    //
                    // The mailbox is read here, before the revive, because the
                    // file is in spawn order: a parent is registered the
                    // moment its own actor is built, so a nested child's parent
                    // is in the tree by the time this runs. A parent with no
                    // actor of its own — a worktree found on disk — has none to
                    // give, and the child gets a dead mailbox whose failed
                    // sends the UI delivers instead.
                    //
                    // What does *not* change is that no run starts: `revive`
                    // ends in `start(_, _, false)`, and a restart is not a
                    // request. This wires where a run would report, not that
                    // one happens.
                    parent: parent.and_then(|parent| self.tree.agent_tx.get(&parent).cloned()),
                },
            );
            self.tree.register(Existing {
                id: AgentId(agent.id),
                parent,
                depth: agent.depth.max(1),
                brief: agent.brief,
                title: agent.title,
                phase,
                branch,
                // A stored session has no fork revision: nothing wrote one to
                // the file, so the sweep keeps the answer it has always given
                // for this node rather than guessing (see `git::reclaimable`).
                fork: None,
                summary,
                leftover: agent.leftover,
                landed,
                result_unread: agent.result_unread,
                tx: Some(tx),
            });
            // What it said in the previous conversation is where it resumes.
            self.chat
                .replace_transcript(AgentId(agent.id), agent.messages);
        }
        // The parent's own transcript is where a completion lands while mush is
        // running, so it is where a completion that never happened has to land
        // too — the model reads it on its next turn, which is the only way it
        // can know that a child's work is uncommitted rather than finished
        // (finding H2). Only a parent that is really in the tree: a transcript
        // for an agent that does not exist is a pane nobody can open.
        for (id, parent) in cut_off {
            let Some(parent) = parent.filter(|parent| self.tree.has(*parent)) else {
                continue;
            };
            let line = agent::Outcome::CutOff.line(id.0);
            self.chat.push_message(parent, Message::user(line));
        }
        // The rows are back; the parents' books are not. Every parent restored
        // above — the root included: its children came back from the file and
        // its actor started empty — is handed the tree's rows now that every
        // node is registered (finding H25).
        self.seed_children();
        self.tree.repair_focus();
        // The root's conversation, in the root's own actor — the one actor that
        // starts with no transcript at all (`agent::spawn`: the history lives in
        // the UI and travels with the first `Run`). A child's completion that
        // reaches it before the human has said anything would fold into a
        // transcript with no system message and no history, and the run it wakes
        // would be a request whose whole content is a bare `#2 done: …` line
        // (§8.39).
        //
        // At the end, not the start: the actor's copy is the conversation the
        // human is looking at, cut-off lines and all, rather than one the UI has
        // already moved on from. It is not a run — a restart is not a request
        // ([`agent::revive`]) — and the human's next message replaces this copy
        // wholesale (`AgentMsg::Run`).
        //
        // A session with no subagents needs none of this: no row means no actor
        // can report a completion before the human's own words.
        if let Some(tx) = self.tree.agent_tx.get(&AgentId::ROOT) {
            let _ = tx.send(AgentMsg::Adopt(self.chat.conversation()));
        }
    }

    /// Write one parent's books from the tree: one [`AgentMsg::ChildBook`] per
    /// row whose parent it is.
    ///
    /// A revived parent is built with an empty `ActorState` — `agent::revive`
    /// replays the transcript and nothing else — so `status` answered "no
    /// children and no jobs" and `control` refused `no such child agent #N`
    /// about children whose rows are on screen (finding H25). The books live in
    /// the actor and the rows in the UI, so the UI is the hand that writes them,
    /// through the same door every other fact about a child takes: the parent's
    /// own book is written where it learns whose child this is.
    ///
    /// Two moments build a parent's actor under rows that already exist, and
    /// both write the books with this: a session restore, after every row is
    /// back ([`Self::seed_children`]), and the wake of a parked child, whose
    /// revival replaces the actor its books were in ([`Self::deliver_to_actor`]).
    ///
    /// A row with no mailbox of its own is skipped: a leftover worktree found
    /// on disk was never given an actor, so there is no sender to hand over —
    /// the same absence `control` reads as "no actor" (finding H18). A send into
    /// a mailbox with no actor behind it is not kept either: that parent has no
    /// books left to write, so the answer *is* the answer, not an error.
    fn seed_parent(&self, parent: AgentId, tx: &Sender<AgentMsg>) {
        for node in self.tree.agents.iter() {
            if node.parent != Some(parent) {
                continue;
            }
            let Some(cmd) = self.tree.agent_tx.get(&node.id) else {
                continue;
            };
            let _ = tx.send(AgentMsg::ChildBook {
                id: node.id.0,
                cmd: cmd.clone(),
                outcome: seeded_outcome(&node.phase, node.summary.as_deref()),
                // The row's `✉` mark: a result nobody has read comes back
                // unread, so `wait` still hands it over exactly once.
                read: !node.result_unread,
                // A stored branch whose worktree is gone became the main
                // checkout on the way in (`agent::live_branch`), i.e. the
                // child now runs in a shared workspace like any child with no
                // branch — the same fact `agent::spawn_tool` records at a
                // spawn.
                shared: node.branch.is_none(),
            });
        }
    }

    /// Write every parent's books from the tree, once, after a restore has put
    /// the rows back.
    ///
    /// One message per child, and not one moment earlier: a parent is revived
    /// before its children are (the file is in spawn order), so there is no
    /// point during the restore at which the tree holds the rows its books need
    /// — the root included, whose children came back from the file too.
    fn seed_children(&self) {
        for (parent, tx) in self.tree.agent_tx.iter() {
            self.seed_parent(*parent, tx);
        }
    }

    /// Re-read what the repository looks like: the main worktree's branch,
    /// dirty count and uncommitted delta, plus each isolated branch's own work.
    /// Called on events, never from `draw` — a `git` process per frame would be
    /// absurd (docs/mush.md §8).
    /// Ask for a fresh read of the repository. The work happens on its own
    /// thread and comes back as `Msg::Git`: `git` is subprocesses, and
    /// subprocesses on the UI thread are dropped frames. This used to fork up to
    /// three per isolated agent, synchronously, every two seconds of a run —
    /// which is felt while an agent works, exactly when the screen is busiest.
    pub fn refresh_git(&mut self) {
        if self.git_in_flight {
            return;
        }
        self.git_in_flight = true;
        let root = self.ws.root().to_path_buf();
        // Resolved here: the tree is UI state, and the worker must not touch it.
        // A nested agent forked from its parent's branch, so that is what its
        // work is measured against; a top-level one forked from HEAD. The node's
        // fork revision goes with them, so the worker can tell a branch that
        // never committed from one whose work was merged (see
        // `git::reclaimable`); a node without one gets the conservative answer.
        //
        // The same walk yields the worktrees a sweep may take, and it takes only
        // the ones *at rest*: a worktree whose agent is running — or whose job is
        // — is being used right now, whatever git says about its branch, and the
        // read must not even propose it (finding H10).
        let mut branches: Vec<(AgentId, String, String)> = Vec::new();
        let mut sweep: Vec<(AgentId, String, Option<String>)> = Vec::new();
        // A worktree may only be swept while nothing of its agent's own is out: a
        // child that is still working, or a result the agent has not read yet,
        // wakes its actor into a fresh run *in that directory* — and a run in a
        // directory that is gone recreates it as a plain path no surface can see
        // (finding S1). One walk over the children, because both answers are set
        // by them.
        let mut waking: HashSet<AgentId> = HashSet::new();
        for node in self.tree.agents.iter() {
            if !node.phase.is_busy() && !node.result_unread {
                continue;
            }
            if let Some(parent) = node.parent {
                waking.insert(parent);
            }
        }
        for node in self.tree.agents.iter() {
            let Some(branch) = node.branch.clone() else {
                continue;
            };
            let base = node
                .parent
                .and_then(|parent| self.tree.node(parent))
                .and_then(|parent| parent.branch.clone())
                .unwrap_or_else(|| "HEAD".to_string());
            if !self.in_flight(node) && !waking.contains(&node.id) {
                sweep.push((node.id, base.clone(), node.fork.clone()));
            }
            branches.push((node.id, base, branch));
        }
        let tx = self.ui_tx.clone();
        std::thread::spawn(move || {
            let mut stats = HashMap::new();
            for (id, base, branch) in branches {
                if let Some(stat) = git::branch_stat(&root, &base, &branch) {
                    stats.insert(id, stat);
                }
            }
            // One answer per at-rest worktree, read here and acted on there:
            // this thread never removes anything.
            let sweep = sweep
                .into_iter()
                .map(|(id, base, fork)| {
                    let found = git::reclaimable(&root, id.0, &base, fork.as_deref());
                    (id, found)
                })
                .collect();
            let status = git::status(&root);
            let _ = tx.send(Msg::Git {
                stats,
                status,
                sweep,
            });
        });
    }

    /// Adopt a repository read that finished on its own thread.
    fn adopt_git(
        &mut self,
        stats: HashMap<AgentId, git::Stat>,
        status: Option<git::RepoStatus>,
        sweep: Vec<(AgentId, git::Reclaimable)>,
    ) {
        // A reap can land between the read and this adoption (a leftover whose
        // checkout went away, say), and the row title sums every entry: a
        // stat for an id with no node would be counted as a ghost. The tree
        // owns which ids exist, so ask it rather than trusting the snapshot.
        let stats = stats
            .into_iter()
            .filter(|(id, _)| self.tree.has(*id))
            .collect();
        self.tree.agent_stats = stats;
        self.git = status;
        self.git_at = Some(Instant::now());
        self.git_in_flight = false;
        self.sweep_worktrees(sweep);
        self.dirty_screen = true;
    }

    /// Act on what the read found at each at-rest agent's worktree: take the ones
    /// whose work is already in their base, and say in the row why the others
    /// stay.
    ///
    /// This is where a hand merge becomes visible without a restart: the sweep
    /// runs on every git read, the read runs every couple of seconds while
    /// anything is working, and the row is marked `merged` or `nothing
    /// committed`, whichever git told (finding H10).
    ///
    /// The removal happens *here*, on the thread that owns the tree, and only
    /// after asking the tree again: the read above is a snapshot, and a node that
    /// started running since it was taken must not have its worktree pulled out
    /// from under its agent — the agent's file tools resolve their directory from
    /// the workspace it was spawned with, so a run would recreate the path as a
    /// plain directory no surface can see (finding S1). `git::reclaim` decides a
    /// second time from git facts, so a worktree that went dirty in the meantime
    /// is kept rather than destroyed.
    fn sweep_worktrees(&mut self, sweep: Vec<(AgentId, git::Reclaimable)>) {
        for (id, found) in sweep {
            // A node can be gone by now (a reap, a new conversation): the tree
            // owns which ids exist, and a sweep for a ghost must do nothing.
            if !self.tree.has(id) {
                continue;
            }
            match found {
                git::Reclaimable::Nothing => self.tree.mark_kept(id, None),
                git::Reclaimable::Kept(why) => self.tree.mark_kept(id, Some(why)),
                git::Reclaimable::Landable(_) => {
                    if self.in_flight_id(id) {
                        continue;
                    }
                    let base = self.fork_base(id);
                    // The node's fork revision, asked again for the same reason
                    // the base is: the decision above is a snapshot, and this is
                    // the moment the removal happens.
                    let fork = self.tree.node(id).and_then(|node| node.fork.clone());
                    let root = self.ws.root().to_path_buf();
                    match git::reclaim(&root, id.0, &base, fork.as_deref()) {
                        git::Reclaimed::Removed { landing, .. } => {
                            self.tree.mark_reclaimed(id, landing.into());
                            // `landed` is what a restart shows, so it goes in the
                            // same file the tree does.
                            self.mark_session_dirty();
                        }
                        git::Reclaimed::Kept(why) => self.tree.mark_kept(id, Some(why)),
                        git::Reclaimed::Nothing => self.tree.mark_kept(id, None),
                    }
                }
            }
        }
    }

    /// Whether agent `id` has work in flight: its own run, or a job of its own.
    /// The tree is the only thing that knows, so the question is asked here
    /// before a directory is taken away.
    fn in_flight_id(&self, id: AgentId) -> bool {
        self.tree
            .node(id)
            .map(|node| self.in_flight(node))
            .unwrap_or(true)
    }

    /// The ref an agent's branch was forked from: its parent's branch, or `HEAD`
    /// for a child of the root — the same derivation the git read used, asked
    /// again at the moment the removal happens.
    fn fork_base(&self, id: AgentId) -> String {
        self.tree
            .node(id)
            .and_then(|node| node.parent)
            .and_then(|parent| self.tree.node(parent))
            .and_then(|parent| parent.branch.clone())
            .unwrap_or_else(|| "HEAD".to_string())
    }

    /// What the open conversation weighs in tokens — the unit the window is
    /// stated in, divided once from the one sum. The meter works in the
    /// budget's own bytes (`Chat::used_weight_for`, compared against
    /// `history_budget`), so this is for callers that want the number in
    /// tokens; either way it is derived on read, not counted beside the
    /// transcript, so nothing can go stale between a push and a draw.
    #[cfg(test)]
    pub fn context_used_tokens(&self) -> usize {
        self.chat.used_tokens_for(self.tree.focused)
    }

    /// How long ago the git snapshot was read, for the bar to say when it is
    /// old. The read is refreshed on the transitions a human drives (a focus
    /// change, a command, a save) and every two seconds while an agent works,
    /// so between events it ages — and an aged fact must not look live
    /// (finding P8).
    pub fn git_age(&self) -> Option<Duration> {
        self.git_at.map(|at| at.elapsed())
    }

    /// Whether agent `id` holds its worktree right now: its own run or one of
    /// its jobs is out ([`Self::in_flight`]), or a busy child or unread result
    /// will wake it into that directory — the sweep's guard, asked as a question
    /// about one id, because `reclaim_isolated` runs on the human's key, where
    /// [`Self::refresh_git`]'s walk has not run. An id with no node holds
    /// nothing: a leftover discovered on disk has no actor to pull a directory
    /// out from under.
    fn worktree_in_use(&self, id: AgentId) -> bool {
        let Some(node) = self.tree.node(id) else {
            return false;
        };
        if self.in_flight(node) {
            return true;
        }
        self.tree
            .agents
            .iter()
            .any(|child| child.parent == Some(id) && (child.phase.is_busy() || child.result_unread))
    }

    /// Reclaim every `mush/<id>` the repository still names — checkout or
    /// not — and reserve the numbers of the ones the sweep keeps.
    ///
    /// A branch whose work is already in the main checkout is H10's specimen:
    /// `mush/2` and `mush/3` were merged child work whose checkouts were long
    /// gone, and the next isolated spawn died on `a branch named 'mush/2'
    /// already exists`. A branch the sweep will not take is left exactly where
    /// it was and says why, and its number stays spent.
    ///
    /// `discover_worktrees` calls this for the repository as it stands, and
    /// [`App::new`] calls it once *before* `restore_agents` — the order is the
    /// fix for H21: a restored agent's actor is built on `.mush/wt/<id>`
    /// (`agent::revive`), so a branch this pass takes has to be taken before
    /// that, or the actor is built in a worktree the same breath deletes and
    /// the node it answers for is frozen by `App::worktree_gone` while the
    /// actor still writes into a path that no longer exists. A second call on
    /// the same repository is nearly free: the branches it took are no longer
    /// named.
    ///
    /// Every reservation here saturates (`saturating_add`): the id comes from a
    /// branch name, and a name at the top of the id space has no `id + 1` —
    /// `worktree_id` refuses that name at the door, and this is the belt for a
    /// floor that arrives anyway (D3).
    fn reclaim_isolated(&mut self) {
        let root = self.ws.root().to_path_buf();
        for id in git::isolated_ids(&root).unwrap_or_default() {
            // A worktree a node is using right now is not this pass's to take:
            // the pass runs on the human's key, against a tree whose actors were
            // only *told* to stop, so a life can still be inside the directory —
            // its own run, its job, or the wake a child's result is about to
            // bring it. The ordinary sweep refuses those already; without the
            // same guard here the removal happens anyway, the next write
            // recreates the path as a plain directory inside the human's
            // checkout, and the run's end would commit there (finding F8).
            if self.worktree_in_use(AgentId(id)) {
                continue;
            }
            match git::reclaim(&root, id, "HEAD", None) {
                git::Reclaimed::Removed {
                    branch_kept,
                    landing,
                } => {
                    // A restored agent that already has a row is marked here; a
                    // leftover's row is registered by `discover_worktrees`, and
                    // on the pre-restore call there is no node yet at all —
                    // `landing` is `Merged` on this path, the answer a run with
                    // no stored fork revision gets, and a node cannot be told a
                    // merge it was never part of.
                    self.tree.mark_reclaimed(AgentId(id), landing.into());
                    // A ref git would not delete still holds the name — `-d` is
                    // the only deletion mush runs, and it deletes what it can
                    // certify — so the number is spent anyway, exactly as the
                    // residue loop below treats a live `mush/<id>`.
                    if branch_kept.is_some() {
                        self.tree.reserve_agents(id.saturating_add(1));
                    }
                }
                git::Reclaimed::Kept(why) => {
                    // Kept work is not a reason to hand the number out again.
                    self.tree.reserve_agents(id.saturating_add(1));
                    // A restored agent has a row already; a leftover's is
                    // registered below, and the next git read fills its line in.
                    self.tree.mark_kept(AgentId(id), Some(why));
                }
                git::Reclaimed::Nothing => {}
            }
        }
    }

    /// Register git worktrees left over from earlier sessions (`mush/<id>`
    /// branches) as finished tree nodes, so a leftover's branch is on its row
    /// after a restart — and reclaim the ones whose work is already in the main
    /// checkout *first*, so a merged branch is not a row and not a name the next
    /// isolated spawn dies on (finding H10).
    ///
    /// A registry entry whose checkout is gone is not work on disk and gets no
    /// row — `rm -rf .mush` leaves git naming those until they are pruned
    /// (finding P13) — but its id is still reserved: the branch outlives the
    /// directory (see the loop below).
    pub fn discover_worktrees(&mut self) {
        let root = self.ws.root().to_path_buf();
        // Git's registry outlives the directory, and an entry pointing at a
        // directory nobody has is not a worktree: prune before anything reads
        // the list, so the sweep and the rows both see the repository as it is.
        // A merge by hand while the checkout was gone leaves *only* that entry
        // between a merged branch and reclaiming it.
        let _ = git::run(&root, &["worktree", "prune"]);
        self.reclaim_isolated();
        // `None` means git could not answer (no binary, not a repository). The
        // tree then keeps the leftovers it already knows about: dropping them
        // on a failed read would look like the work had been reclaimed.
        let Some(worktrees) = git::worktrees(&root) else {
            return;
        };
        // Drop stale leftovers whose worktree no longer exists. Reaping takes
        // the focus and the cursor off a ghost with them (finding B11).
        let gone: Vec<AgentId> = self
            .tree
            .agents
            .iter()
            .filter(|node| node.leftover)
            .filter(
                |node| match node.branch.as_deref().and_then(git::worktree_id) {
                    Some(id) => !git::worktree_path(&root, id).exists(),
                    None => true,
                },
            )
            .map(|node| node.id)
            .collect();
        self.tree.reap(&gone);
        // A name in mush's own branch namespace that [`git::worktree_id`]
        // refuses — `mush/x`, a hand-made name, or an id with no room above it
        // for the floor — is not a child's, and it is not another program's
        // either. Git's own branches are not mush's to mention, but a name that
        // claims a child's shape and is not one is refused *by name*, with a
        // sentence: a name mush cannot read is not a child (D3).
        let mut refused: Vec<String> = Vec::new();
        for worktree in worktrees {
            let Some(id) = worktree.id else {
                if let Some(branch) = worktree
                    .branch
                    .as_deref()
                    .filter(|branch| git::is_child_branch(branch))
                {
                    refused.push(branch.to_string());
                }
                continue;
            };
            // Every id the repository still names raises the floor, checkout or
            // not — this is the branch half of the invariant in [`crate::ids`].
            // A `mush/<id>` branch whose directory was removed survives as a
            // branch, so the next `git worktree add -b mush/<id>` fails on it
            // even though `on_disk` says there is nothing here; leaving the
            // number free spends a spawn on a name git will keep refusing
            // (finding B1, P13). No row is registered for the residue below:
            // reserving a number claims nothing about work. The add saturates:
            // a name at the top of the space has no `id + 1`, and refusing that
            // name is `worktree_id`'s job, not this arithmetic's.
            self.tree.reserve_agents(id.saturating_add(1));
            // A dead registry entry is git's residue, not a worktree: a row for
            // it would claim a directory that is not there (finding P13).
            if !worktree.on_disk() {
                continue;
            }
            if self.tree.has(AgentId(id)) {
                continue;
            }
            let full = git::branch_name(id);
            // The commit mush made for this worktree names the task and how the
            // run ended, so a leftover is shown as the work it is instead of an
            // anonymous placeholder. A branch the human committed to by hand
            // carries no such subject and stays unnamed.
            let (brief, phase, summary) =
                match git::subject_of(&root, &full).and_then(|s| agent::parse_commit_subject(&s)) {
                    Some((agent::Committed::Finished, brief)) => {
                        (brief, Phase::Done, "last run finished")
                    }
                    Some((agent::Committed::Stopped, brief)) => {
                        (brief, Phase::Stopped, "last run was stopped")
                    }
                    Some((agent::Committed::CutOff, brief)) => {
                        (brief, Phase::CutOff, "last run was cut off")
                    }
                    Some((agent::Committed::Failed(error), brief)) => {
                        (brief, Phase::Failed(error), "last run failed")
                    }
                    None => (
                        "leftover worktree".to_string(),
                        Phase::Done,
                        "found on startup",
                    ),
                };
            self.tree.register(Existing {
                id: AgentId(id),
                parent: None,
                depth: 1,
                brief,
                title: None,
                phase,
                branch: Some(full),
                // A leftover found on disk has no fork revision to read: git
                // never wrote one down, and the branch it would identify is
                // exactly the thing under question.
                fork: None,
                summary: Some(summary.to_string()),
                leftover: true,
                landed: None,
                result_unread: false,
                tx: None,
            });
        }
        if !refused.is_empty() {
            self.say(format!(
                "{} names no agent id mush can hold — left alone",
                refused.join(", ")
            ));
        }
        self.tree.repair_focus();
    }

    // ---------------------------------------------------------------- updates

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Models { endpoint, models } => self.adopt_models(endpoint, models),
            Msg::Git {
                stats,
                status,
                sweep,
            } => self.adopt_git(stats, status, sweep),
            Msg::Paste(text) => {
                // A paste is something the human wants to say, so it lands in
                // the message box whichever pane has focus. An open picker is
                // the one place a paste has no meaning; below the floor there
                // is no box on screen for it to land in, so it is refused the
                // way a key is (finding P11).
                if self.picker.is_none() && !self.below_floor() {
                    // Terminals disagree about line endings in a paste.
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    // A paste that is nothing but image paths attaches the
                    // images — that is what drag-and-drop and a file manager's
                    // "copy" produce, and four dragged files are one such
                    // gesture — and one that is anything else is the words it
                    // is.
                    match self.ws.pasted_images(&text) {
                        // A refused attachment is not a swallowed paste: the
                        // paths go in as text, which is also the road to the
                        // downscale a refusal may name.
                        Ok(Some(images)) => {
                            if !self.attach_images(images) {
                                self.chat.insert(&text);
                            }
                        }
                        Ok(None) => self.chat.insert(&text),
                        // It *is* an image (or a batch holding one), and it
                        // cannot ride. The words go in anyway — a paste is
                        // never swallowed, and the paths are still useful,
                        // since the model can be asked to downscale one — and
                        // the reason is said as the refusal it is.
                        Err(line) => {
                            self.chat.insert(&text);
                            self.fail(line);
                        }
                    }
                }
            }
            Msg::Clipboard {
                conversation,
                result,
            } => {
                // A read that outlives the chat that asked for it is not news
                // about this chat: dropped, exactly as a stale `Msg::Agent` is.
                if conversation == self.tree.conversation() {
                    match result {
                        Ok(Some(image)) => {
                            self.attach_image(image);
                        }
                        Ok(None) => self.say(
                            "the clipboard holds no image — copy a screenshot, or paste the path \
                             of an image file",
                        ),
                        Err(line) => self.fail(line),
                    }
                }
            }
            Msg::Key(key) => self.on_key(key),
            // The write road's answer, and the mode's line said only when the
            // clipboard actually took the text: the count of lines and bytes is
            // the copy's fact, and a line that read `copied 12 lines` over a
            // write that failed would be the one lie the bar must not tell.
            Msg::Copied {
                conversation,
                line,
                result,
            } => {
                if conversation == self.tree.conversation() {
                    match result {
                        Ok(()) => self.say(line),
                        Err(why) => self.fail(why),
                    }
                }
            }
            Msg::Agent {
                conversation,
                id,
                event,
            } => {
                if conversation == self.tree.conversation() {
                    self.on_agent(id, event);
                } else if let AgentEvent::Spawned { cmd, .. } = &event {
                    // A tree Ctrl-N abandoned can still spawn children. They are
                    // not ours, but they must not run either — and because this
                    // event is dropped, telling the child here is the only
                    // chance it gets to end.
                    let _ = cmd.send(AgentMsg::Shutdown);
                }
            }
            Msg::Attach {
                from,
                request,
                reply,
            } => {
                let response = self.handle_attach(&from, &request);
                // A client that hung up while the answer was being built leaves
                // no receiver; that is not an error here.
                let _ = reply.send(response);
            }
        }
        self.dirty_screen = true;
    }

    /// Called once per event-loop pass so the foot's dots move on time, and so
    /// a line that has outlived its welcome leaves the screen even when nothing
    /// else is happening.
    pub fn tick(&mut self) {
        if self.busy() {
            // One dot a second, not one per pass: the poll is every 30 ms and
            // the animation is a second hand ([`DOT_PERIOD`]). The repaint is
            // owed when a beat lands — the dots moved, and the ages beside them
            // did too — rather than thirty times a second for a decoration.
            if self.spin_at.elapsed() >= DOT_PERIOD {
                self.spin = self.spin.wrapping_add(1);
                self.spin_at = Instant::now();
                self.dirty_screen = true;
            }
            // A long run keeps changing the workspace; the bar and the rows
            // should not need a keystroke to notice.
            if self
                .git_at
                .map(|at| at.elapsed() > Duration::from_secs(2))
                .unwrap_or(true)
            {
                self.refresh_git();
            }
        }
        if let Some(status) = &self.status {
            // A failure stays until something replaces it; everything else —
            // the quit warning included, whose arm ends when its line does —
            // fades.
            if status.kind != StatusKind::Error && status.set_at.elapsed() >= INFO_TTL {
                self.status = None;
                self.dirty_screen = true;
            }
        }
        // While a quit waits for its second key, its warning is rewritten from
        // the tree it is about: the line names what is running now, not what
        // was running when the key was pressed (finding H9).
        self.refresh_quit_warning();
        // The same for a new chat's warning, about the conversation the key
        // would drop: the count follows the chat (finding C4).
        self.refresh_new_chat_warning();
        // The same expiry for the pane's own transient lines, on the same tick:
        // a command's answer or a hint that nobody ended by acting must not sit
        // in the foot for the life of the session (finding U8). Failures are not
        // chatter and are never taken away here.
        if self.chat.expire_said(session::now_secs()) {
            self.dirty_screen = true;
        }
        // A `⊘` whose acknowledgement never arrives leaves a row spinning
        // forever, which is worse than an idle one (finding B6).
        if self.tree.expire_cancels() {
            self.dirty_screen = true;
        }
        // The run's memory, bounded (§8.21): finished children past the history
        // window are forgotten, and the actor threads of the ones past the warm
        // window are parked. Both are filters over a tree this very window keeps
        // small, so this is not a reason to skip a frame's worth of either — and
        // both are *idempotent*, which is what lets them live on a tick rather
        // than on a transition nobody would remember to add.
        self.reap_history();
        self.park_history();
        // The file is allowed to lag the conversation by `SESSION_DEBOUNCE`.
        // This is where that lag is paid: the rebuild and the hand-over happen
        // once per interval, on a tick, and never in the handler that received
        // a message — which is what makes a tool result cost the UI nothing.
        if self
            .session_dirty_at
            .map(|dirty_at| dirty_at.elapsed() >= SESSION_DEBOUNCE)
            .unwrap_or(false)
        {
            self.save_session();
        }
        // A write that failed on the writer's thread has no caller to return
        // to, so it is picked up here — the next tick after it happened.
        if let Some(error) = self.session_save.take_error() {
            self.fail(format!("could not save session: {error}"));
        }
    }

    fn on_agent(&mut self, id: AgentId, event: AgentEvent) {
        match event {
            AgentEvent::Spawned {
                child,
                parent,
                brief,
                depth,
                branch,
                fork,
                title,
                cmd,
            } => {
                let opened = self.tree.insert(Spawn {
                    id: AgentId(child),
                    parent: AgentId(parent),
                    brief,
                    depth,
                    branch,
                    fork,
                    cmd,
                });
                // The caller's name for this child, when it chose one: the row
                // prefers it over the handle derived from the brief (the
                // spawn tool's `title`).
                if let Some(title) = title {
                    self.tree.named(AgentId(child), title);
                }
                // The brief opens the child's transcript: the model sees the
                // brief, so the human should too (finding B13).
                self.chat.push_message(opened.id, opened.opening);
                // The node — its brief, branch and parent — is stored, so a
                // restart comes back with the same tree.
                self.mark_session_dirty();
            }
            AgentEvent::SystemPrompt(prompt) => {
                // The actor built its own prompt — a child's names the workspace
                // its tools resolve paths in — and the app weighs it for the
                // meter and the attach gate (`Chat::used_weight_for`). It is not
                // stored: a prompt names a workspace that may have moved, so a
                // session leaves the system message out and the actor builds a
                // fresh one on the way back in.
                self.chat.learn_system(id, prompt);
            }
            AgentEvent::Running { cancel } => {
                // A run started, possibly one the UI did not ask for (an idle
                // agent woken by a child's result). Mark it so `busy`, the
                // spinner, and Ctrl-C agree with the actor. The last run's
                // summary belongs to that run, not this one (finding B14).
                self.tree.begin(id, Some(cancel));
                // A new run supersedes everything mush said about the last one:
                // the hints answered a command in a moment that is over, and the
                // failure belonged to the run this one is replacing. The row is
                // derived from the phase and cannot go stale, so the line is the
                // one that ages out — which is also what keeps a pane from
                // holding a `!` line next to a run in flight that is fixing it.
                if self.chat.clear_notes_for(id) {
                    self.mark_session_dirty();
                }
            }
            AgentEvent::Status(status) => {
                // A status that arrives after the run's own end (a late or
                // duplicated commit line) must not put a finished agent back to
                // work (finding B5).
                self.tree.activity(id, status);
            }
            AgentEvent::Thinking => {
                // The model's turn is starting, so the tool before it is over:
                // what the row and the foot wear until the next tool names
                // itself is `thinking…`, not the label of a command that has
                // already exited. Refused for an agent at rest and for a fold,
                // exactly as a status is (`AgentTree::thinking`).
                self.tree.thinking(id);
            }
            AgentEvent::Notice(text) => {
                // A limit the run reached (it still produced a result), or a
                // reply that was empty: a line in the transcript, tagged with
                // the agent it concerns (finding B19).
                self.chat.note_for(id, text);
            }
            AgentEvent::Message(message) => {
                // The pane is deliberately not sent to the bottom here: the
                // position is the human's, and a pane that is at the bottom
                // follows the newest line by construction (finding U3).
                self.chat.push_message(id, message);
                // Every message is part of what the file stores — a subagent's
                // as much as the root's — but the mark is O(1): the rebuild and
                // the write wait for the debounced tick, so a streamed tool
                // result cannot stall the frame that shows it.
                self.mark_session_dirty();
            }
            AgentEvent::ResultRead { child } => {
                // The parent's actor has handed a child's result to the model —
                // folded it, or answered a `wait` for it — so the child's
                // row stops wearing `✉`. Only the owner of that fact moves the
                // mark: a row that guessed itself clear would be claiming a
                // reading that never happened (finding H4).
                self.tree.result_read(AgentId(child));
            }
            AgentEvent::ChildAsleep { child, command } => {
                // A parent's `control` found its child's mailbox empty: the
                // actor behind it is gone — parked, which reclaims the thread
                // and nothing else (`Self::park_history`), or replaced by a
                // newer actor the parent's one mailbox cannot know about. The
                // command is handed over through the same door the human's own
                // message uses, because the UI is the only hand holding the
                // transcript a new actor is rebuilt from. A child whose node is
                // gone really is gone, and `deliver_to_actor` starts nothing
                // for it.
                //
                // The answer is dropped rather than used, but whether a
                // *resume* just landed is not: the parent wrote its model a
                // reply before this event reached the UI thread, and what
                // settles the parent's books from here is the child's own
                // `ChildRunning` and `ChildDone` (finding H18) — while the
                // tree's row is settled by the child's `Running`, which the
                // words this door just delivered will make it emit.
                let resumes = matches!(command, AgentMsg::Steer(_));
                if self.deliver_to_actor(AgentId(child), command) && resumes {
                    // The run those words start is already walking toward the
                    // UI: a `tick` before its `Running` lands is a
                    // `park_history` whose `Shutdown` cancels it, and the
                    // revived actor is exactly the thread to receive one. The
                    // mark is the optimistic one the human's own nudge sets;
                    // the child's own events settle it a moment later.
                    self.tree.nudge(AgentId(child));
                }
            }
            AgentEvent::ChildResumed { child } => {
                // A parent's `control message` sent words into a child the tree
                // still reads as at rest: the child's `Running` event is on its
                // way, and one `tick` in between is a `park_history` whose
                // `Shutdown` cancels the run the words started. Mark the row
                // with the same optimistic phase the human's own nudge sets
                // (its doc says why it is safe); the child's own events replace
                // it as they arrive.
                self.tree.nudge(AgentId(child));
            }
            AgentEvent::ParentAsleep { command } => {
                // An agent's report found no actor behind the mailbox it holds
                // for its parent — the parent's thread was reclaimed, or replaced
                // by a revival the child's copy of the sender outlived — and the
                // parent's books are where a child's run is booked. A dropped
                // completion is a `wait` that burns its whole cap and a shared
                // workspace the guard refuses to a sibling, under a row that
                // says `✓` (§8.39). The tree is the truth about whose child this
                // is and which sender is live now, so the command is delivered to
                // *that* mailbox — the door a human's own message uses, in the
                // other direction (`ChildAsleep`, finding H18).
                //
                // The root is refused by the tree rather than by a flag: it has
                // no parent row, so its own reports are dropped here instead of
                // being filed back into the mailbox that emitted them — the one
                // wake-up that could never end.
                let _ = self.hand_to_parent(id, command);
            }
            AgentEvent::Reclaimed { landing } => {
                // The actor's own run end swept its worktree: the checkout and
                // the branch are gone, so the row stops offering a `git diff`
                // against either and says where the work is instead — the base
                // the run was forked from (finding U13, H10). A row still
                // offering `.mush/wt/<id>` after this would be offering a path
                // nothing can run in. `landing` is git's answer about the branch
                // (merged, or never committed), in the tree's own vocabulary.
                self.tree.mark_reclaimed(id, landing.into());
                // `landed` is what a restart shows, so it is stored.
                self.mark_session_dirty();
            }
            AgentEvent::Stopped => {
                // Stopped is not failed and not done: the run produced nothing,
                // and the actor is idle and resumable. Saying which one it is
                // is the difference between a lost agent and a parked one.
                self.tree.stopped(id);
                // The phase is stored, so a restart must not show a stopped run
                // as one that never happened.
                self.mark_session_dirty();
                self.refresh_git();
                // Only the agent the human is looking at needs the bar; a
                // stop they did not ask for still shows as ⊘ on its row.
                if id == self.tree.focused {
                    self.say(format!("agent {id} stopped — send a message to resume it"));
                }
            }
            AgentEvent::Error(error) => {
                self.tree.fail(id, error.clone());
                self.refresh_git();
                // The failure reaches the bar *when it happens*, whoever it
                // belongs to. It used to be the focused agent's line only —
                // another agent's failure was on its own row's `✗` and in its
                // own pane's foot — but a row can be scrolled out of the
                // history window and a pane the human is not reading shows
                // nothing, so a run could be dead for as long as it took them to
                // look at that child (finding B27's live shape). The sentence
                // names the agent, so the bar is unambiguous whichever pane is
                // open. A loop-stop is this same event (the loop guard's
                // `stopped as a loop…` is the run's error), so one arm covers
                // both endings.
                let line = format!("agent {id} failed — {error}");
                // The durable half too: the row's `✗` is derived and dies with
                // the next run, while the notice is tagged, stamped and written
                // to the session, so a restart still says what broke.
                self.fail_for(id, error, Some(line));
            }
            AgentEvent::Done => {
                let summary = self.last_assistant_text(id);
                // Every Done replaces the row's summary; keeping the first one
                // described a run that ended long ago (finding B14).
                self.tree.finish(id, summary);
                self.mark_session_dirty();
                self.refresh_git();
            }
            AgentEvent::JobStarted { job, command } => {
                // The bar says what just happened, and the registry — which the
                // rows read every frame — is what says what is running now. No
                // copy of the job is kept here: the row's `⚙N` count is derived
                // from the registry on every frame, so it cannot go stale.
                let note = format!(
                    "{} detached · {}",
                    crate::jobs::label(job),
                    job_title(&command)
                );
                self.say_for(id, note);
            }
            AgentEvent::JobDone { job, line } => {
                // A job's report is the owner's to read in its transcript (the
                // actor folds it in); on the screen it is the bar's line, and
                // the badge the job was on goes out with it.
                let _ = job;
                self.say_for(id, line);
            }
            AgentEvent::Context { tokens, source } => {
                // The actor learned the endpoint's real window from a server
                // complaint; the UI owns the copy the bar and the tool caps
                // read, so it has to adopt the same number or the
                // next `/model` clobbers it (finding B7). The cell is the one
                // path: this is the UI adopting on the same terms the actor
                // did, and it writes the actors' copy with it.
                self.cell.learn_context(tokens, source);
            }
            AgentEvent::Compacting { why, cancel } => {
                // A fold was accepted, parked, or put on the wire. It is a
                // phase, not a status line: the row, the bar and the foot all
                // read this one answer, and it lasts exactly as long as the
                // fold does — where a status line is dropped for an agent at
                // rest, which is the agent an idle `/compact` runs on, and
                // fades on a timer for one that is not (finding U11).
                self.tree.compacting(id, why, cancel);
            }
            AgentEvent::CompactingEnded { in_run } => {
                self.tree.compacting_ended(id, in_run);
            }
            AgentEvent::Compact { summary, in_run } => {
                // The actor's transcript is now [system, user(summary)];
                // mirror it so nudges, saves, and the visible chat stay in
                // sync with what the model actually sees.
                let carried = Message::user(prompt::compaction_message(&summary));
                self.chat.replace_transcript(id, vec![carried]);
                // The fold is over, and the row must stop saying it is folding:
                // the summary is read where it now lives, in the transcript.
                self.tree.compacted(id, in_run);
                if id == AgentId::ROOT {
                    // A fold is deliberate and expensive, and the transcript it
                    // leaves is what a restart resumes from — so it is written
                    // before this returns rather than waiting out the debounce.
                    self.flush_session();
                    self.chat.note_for(
                        AgentId::ROOT,
                        "context compacted — continuing from a summary",
                    );
                } else {
                    self.mark_session_dirty();
                }
            }
        }
    }

    /// Whether anything in the tree is working: derived from the phases and the
    /// job registry, so it cannot disagree with the rows. A detached job counts:
    /// the agent may be napping, but the machine is not idle, and the tick uses
    /// this to keep the git snapshot fresh while something runs.
    pub fn busy(&self) -> bool {
        // The tree answers the common case with no registry read; the per-node
        // check only adds the jobs.
        self.tree.busy() || self.tree.agents.iter().any(|node| self.in_flight(node))
    }

    /// Whether one agent has work in flight: its own run, or one of its jobs.
    /// The tree owns the nodes and the job registry, so the question is asked
    /// there ([`AgentTree::in_flight`]); this is the app-side spelling behind
    /// [`Self::busy`] and [`Self::working_agents`]. [`AgentTree::busy`] stays
    /// agent-only, with no opinion about jobs.
    fn in_flight(&self, node: &AgentNode) -> bool {
        self.tree.in_flight(node)
    }

    /// The jobs `id` has running, read from the one registry that holds them.
    /// Every place the screen mentions a job goes through here, so the row's
    /// count, the footer's list and the bar's line cannot disagree about what
    /// is running on this machine.
    pub fn live_jobs(&self, id: AgentId) -> Vec<crate::jobs::JobView> {
        self.tree.live_jobs(id)
    }

    /// `#c2 cargo build 1m20s` — one job, as the footer and the bar read it.
    pub fn job_lines(&self, id: AgentId) -> Vec<String> {
        self.live_jobs(id)
            .into_iter()
            .map(|job| {
                let held = if job.exclusive { " · the machine" } else { "" };
                format!(
                    "{} {} {}{held}",
                    crate::jobs::label(job.id),
                    job_title(&job.command),
                    short_age(job.age)
                )
            })
            .collect()
    }

    /// The one door a bar line goes through: the text is
    /// [`mush_core::text::sanitize`]d here, because the bar paints it whole —
    /// it does no width arithmetic, so the `truncate`/`fit_row` rule cannot
    /// reach it. Private, so every string the bar can show is created by
    /// [`Self::say`], [`Self::fail`] or a quit's own warning ([`StatusKind::Quit`]),
    /// which differ only in the kind.
    fn set_status(&mut self, kind: StatusKind, text: impl Into<String>) {
        let text = text.into();
        self.status = Some(Status {
            kind,
            text: mush_core::text::sanitize(&text),
            set_at: Instant::now(),
        });
    }

    /// Remember a transient line for the bar: what a command just did, what the
    /// human just asked for. It fades.
    pub fn say(&mut self, text: impl Into<String>) {
        self.set_status(StatusKind::Info, text);
    }

    /// A bar line about `id`, named with the agent unless it is the focused
    /// one: the focused agent's own line needs no name.
    fn say_for(&mut self, id: AgentId, text: String) {
        if id == self.tree.focused {
            self.say(text);
        } else {
            self.say(format!("agent {id}: {text}"));
        }
    }

    /// `500k`, `8192`, `1M` — one glance, no counting zeroes.
    pub fn context_label(&self) -> String {
        let spelling = tokens_label(self.cfg().context_tokens);
        if self.cfg().context_explicit {
            format!("ctx {spelling} (set)")
        } else {
            format!("ctx ~{spelling}")
        }
    }

    /// How full the conversation in the open pane is, against the number every
    /// decision uses: `ctx 3.1k/6.4k (fold 5.8k) ~8k` — what this conversation
    /// weighs against the history budget, where the fold's trigger sits inside
    /// it, and the window itself. The pane can be a subagent's, whose own next
    /// request is what this number measures.
    ///
    /// The mark is the budget ([`mush_core::Config::history_budget`]): the
    /// trimmer cuts a transcript past it, the fold fires at nine tenths of it
    /// ([`mush_core::transcript::compaction_trigger`], printed at its own
    /// place), and the attach gate measures its room from it. Comparing the
    /// same `used` to the *window* instead made the marks unreachable in
    /// normal operation: on the 8 K default the fold fires at ≈3.7k of the
    /// budget, which the old meter read as 45 % — so `full` never happened,
    /// and the human had no way to see a fold or a cut coming (the audit's
    /// finding). The window keeps its own number, with the `~` that says it is
    /// the assumed one, so what the budget is a reserve off stays visible.
    ///
    /// At the budget and past it there is no longer a fraction to print: a
    /// learned window can be smaller than the transcript already held, so the
    /// meter read `ctx 1.2k/1k` — a ratio greater than one with nothing saying
    /// so (finding P9). At the limit it says `full`; past it, it says `over`.
    pub fn context_meter(&self) -> String {
        let budget = self.cfg().history_budget();
        let fold = mush_core::transcript::compaction_trigger(budget);
        let used = self.chat.used_weight_for(self.tree.focused);
        let mark = if self.cfg().context_explicit { "" } else { "~" };
        let used_label = tokens_label(used / BYTES_PER_TOKEN);
        let budget_label = tokens_label(budget / BYTES_PER_TOKEN);
        let fold_label = tokens_label(fold / BYTES_PER_TOKEN);
        let window_label = tokens_label(self.cfg().context_tokens);
        let state = match used.cmp(&budget) {
            std::cmp::Ordering::Greater => " over",
            std::cmp::Ordering::Equal => " full",
            std::cmp::Ordering::Less => "",
        };
        format!("ctx {used_label}/{budget_label}{state} (fold {fold_label}) {mark}{window_label}")
    }

    /// The conversation this workspace was left holding could not be read, and
    /// the human has to hear it before they mistake the empty screen for an
    /// empty workspace (finding S3). A stored *row* the restore refuses is said
    /// through the same door for the same reason (finding C9).
    ///
    /// It takes the two homes a failure takes: the root pane's foot — wrapped
    /// to the pane, ranked `Alert`, read back whole by `/notes` — and the bar's
    /// line one, so it is visible without opening anything and stays visible
    /// until something replaces it. It is news rather than chatter, so the
    /// human's next send does not end it, and the session is marked dirty so
    /// the next save writes it: a workspace that could not be read is a fact
    /// about the workspace, not about the moment it was noticed. (The root's
    /// own next run supersedes it, as it supersedes any failure — but by then
    /// the human has run something in the conversation it opened.) The words
    /// come from the caller (`main`), which is the one place that knows the
    /// path, the reason and where the only copy went — and they take the same
    /// three homes [`Self::fail_for`] gives a run's own failure, so the two
    /// cannot drift about what a failure does (refactor R19).
    pub fn session_unreadable(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.fail_for(AgentId::ROOT, text.clone(), Some(text));
    }

    /// A failure, in the three places one outlives the moment: the durable
    /// notice in `id`'s pane (stamped, ranked `Alert`, read back whole by the
    /// pane's foot and `/notes`), the mark that makes the next save write it —
    /// a failure is a fact about the workspace or the agent, not about the
    /// moment it was noticed — and the bar's line (`line`), which the caller
    /// owns because only it knows whether the bar is the place for this one.
    ///
    /// One door, so a new kind of failure cannot take two of the three and
    /// forget the last: the run's own failure and a conversation that could not
    /// be read are the same shape (refactor R19).
    fn fail_for(&mut self, id: AgentId, text: impl Into<String>, line: Option<String>) {
        let text = text.into();
        self.chat.note_error_for(id, text);
        self.mark_session_dirty();
        if let Some(line) = line {
            self.fail(line);
        }
    }

    /// Remember something that went wrong. Errors do not fade: they stay until
    /// a later line replaces them.
    ///
    /// A failure is where the *endpoint's* own words reach the bar — a 500's
    /// error body, a refusal's reason — so it goes through the same door `say`
    /// does (see [`Self::set_status`]).
    pub fn fail(&mut self, text: impl Into<String>) {
        self.set_status(StatusKind::Error, text);
    }

    /// The transient line, if it is still worth showing.
    pub fn status_line(&self) -> Option<(&str, StatusKind)> {
        let status = self.status.as_ref()?;
        if status.kind == StatusKind::Error || status.set_at.elapsed() < INFO_TTL {
            Some((status.text.as_str(), status.kind))
        } else {
            None
        }
    }

    /// The one derived line about the *tree*, when the rows cannot say it
    /// themselves.
    ///
    /// This used to be the focused agent's newest activity
    /// (`#0 edit_file src/lib.rs 12s`), which the agent's own row had said
    /// (`◐ #0 edit_file src/lib.rs 12s`) and the transcript had said again
    /// (`⚙ edit_file src/lib.rs`): one fact, three homes, and the bar's only
    /// line spent on a sentence the human had already read twice (finding U5).
    /// What survives is the fact no row states even though its `⏸N` implies
    /// it: an orchestrator that has ended its turn with children still working
    /// is napping and *will* resume by itself (§5.5), which is a promise about
    /// what happens next rather than a report of what is happening now.
    ///
    /// Everything else the bar's line one carries is an event with no other
    /// home — a failure, a stop, a job's report, a command's answer — and the
    /// newest of those is the status, not this.
    ///
    /// A fold the human is waiting for comes first, because it is the one
    /// derived state that answers a question they are holding in their head:
    /// the fold is running, and the words they are about to type are not lost.
    /// The verb is [`Compacting::verb`] — the row says which kind of fold it is
    /// (`compacting…`, `folding at the next step…`) with its age, and the bar
    /// says the same word, so the two cannot describe one fold two ways
    /// (finding U11, refactor R8). This is the sentence that lets them keep
    /// typing.
    ///
    /// The count is the root's *own* busy children — [`AgentTree::busy_counts`] —
    /// the same derivation the row's `⏸N` mark and the title's `M waiting`
    /// read, and not every busy node in the tree. The sentence is a promise
    /// about when the root resumes, and it resumes when its children finish: a
    /// grandchild working under a child that is itself parked promised a resume
    /// the grandchild's finish does not cause, and it contradicted the title of
    /// the very frame it was painted in (a second owner of the fact U2 named).
    ///
    /// Whether the root is the one napping is [`AgentTree::napping`] too — the
    /// predicate the title's bucket reads. A stopped or failed root over a
    /// working child does resume (the completion folds in and starts a run), so
    /// the bar promising it while the title said `0 waiting` was one fact, two
    /// derivations (finding U12).
    pub fn tree_line(&self) -> Option<String> {
        let focused = self.tree.focused;
        if let Some(kind) = self
            .tree
            .node(focused)
            .and_then(|node| node.phase.compacting())
        {
            return Some(format!(
                "{} {focused} · keep typing — your message is answered after the fold",
                kind.verb()
            ));
        }
        if !self.tree.napping(AgentId::ROOT) {
            return None;
        }
        // The count is the root's own busy children, read from the same map the
        // rows' `⏸N` marks and the title's buckets are built from: one
        // derivation, so a count and a mark cannot disagree (finding U1).
        let waiting = self
            .tree
            .busy_counts()
            .get(&AgentId::ROOT)
            .copied()
            .unwrap_or(0);
        // Through the door like every other bar string: the sentence is numbers
        // and fixed words *today*, and the bar's documented invariant is that
        // nothing reaches it unsanitized (finding V6).
        Some(mush_core::text::sanitize(&format!(
            "waiting on {waiting} subagent(s) — the root resumes as they finish"
        )))
    }

    /// The last assistant reply in an agent's transcript (its final summary).
    fn last_assistant_text(&self, id: AgentId) -> Option<String> {
        self.chat
            .transcript(id)
            .iter()
            .rev()
            .find(|message| message.role == "assistant")
            .map(|message| message.text().trim().to_string())
            .filter(|text| !text.is_empty())
    }

    // ------------------------------------------------------------- chat / LLM

    /// The human's next act ends the moment the last chatter line answered
    /// (finding U8): a `/help`, a diff or a hint was about the thing they typed
    /// *before* this one, and leaving it in the foot spends rows on a question
    /// nobody is asking any more. Failures stay — they are the run's record, not
    /// a moment's — and so does the pane's red line, which is the bar's copy of
    /// the same fact.
    fn ends_the_moment(&mut self) {
        if self.chat.dismiss_said() {
            self.dirty_screen = true;
        }
    }

    /// Send what is in the message box: a command to run, or a message for the
    /// focused agent.
    ///
    /// The two are told apart by the one parser (`app::commands`), which returns
    /// a value rather than a string to compare here — so what a command *is*
    /// has exactly one definition, and the help text reads the same table the
    /// arms do.
    fn send_message(&mut self) {
        let text = self.chat.take_input().trim().to_string();
        // An empty box is a send when an image is attached: a picture with no
        // words is a legal message, and only the human can judge whether it
        // needs a sentence.
        if text.is_empty() && self.chat.attachments().is_empty() {
            return;
        }
        // The draft is on its way out, so the road back goes with it: the
        // `Ctrl-Z` slot is spent here and not by `take_input`, because the
        // check above comes first — Enter on an empty box sends nothing, and
        // spending the slot on a keystroke that sent nothing would be a loss of
        // its own.
        self.chat.forget_lost();
        let parsed = commands::parse_command(&text);
        // `/notes` is the reader of the chatter lines, not the act that
        // supersedes them: a human asking for the rest of the foot is reading
        // it, and dismissing what they came to read would make the pane's
        // `+N more · /notes` point at nothing (see `ends_the_moment`).
        if !matches!(&parsed, Ok(Command::Notes)) {
            self.ends_the_moment();
        }
        match parsed {
            Ok(command) => self.apply_command(command),
            // No slash: the human is talking to an agent. A message that does
            // not land is said on the bar by `deliver` itself; the human's own
            // key has no client to answer with a refusal. The refusal changes
            // nothing in the box either, so the words and the images go back
            // where they were: a message that cannot be sent is not something
            // the human should have to retype or paste again.
            Err(CommandError::NotACommand) => {
                let words = text.clone();
                // The attachments leave the box with the send; a refusal hands
                // them back with the words. They are taken *here* and not
                // before the parse, because a command is not a send and its
                // attachments stay where they are (a `/model` is not a send).
                let images = self.chat.take_attachments();
                if self.deliver(text, images.clone()).is_err() {
                    self.chat.insert(&words);
                    self.chat.restore_attachments(images);
                }
            }
            // A command that exists but whose argument does not read. The line
            // was spelled beside the rule that rejected it, and the bar is
            // where every other complaint of this kind goes.
            Err(CommandError::Usage(line)) => self.say(line),
            // A slash nobody implements answered a command the human just
            // typed, in a moment that ends the instant they type again — an
            // informational line in the focused pane, not a failure. As an
            // error it was ranked an alert (never the line the cap yielded),
            // painted red, and written to the session, so a typo outlived the
            // run it answered and came back at the next start.
            Err(CommandError::Unknown(name)) => {
                // The way out is named with the complaint: the bar is the one
                // line a human reads after a typo, and `/help` is where the
                // commands are. The name is what a paste can stretch, so the
                // name is what the row's budget cuts (finding D3).
                let name = mush_core::text::truncate(&name, UNKNOWN_NAME_COLUMNS);
                self.chat.note_for(
                    self.tree.focused,
                    format!("unknown command: {name} — /help lists them"),
                )
            }
        }
    }

    /// Why a message to `id` cannot run, if it cannot: its worktree is gone.
    ///
    /// A hand-run merge or discard reclaims an isolated agent's worktree and
    /// branch but leaves its actor alive, and that actor's file tools resolve
    /// their directory from the workspace it was spawned with — so a run would
    /// recreate the reclaimed path as a plain directory where no surface could
    /// see the work (finding S1). The row and its footer already tell the landed
    /// story; this is the same story for
    /// typing. A worktree a hand-run `git worktree remove` took reads the same
    /// way: the branch is named, the worktree is not on disk, so a run would
    /// write into a phantom.
    fn worktree_gone(&self, id: AgentId) -> Option<String> {
        let node = self.tree.node(id)?;
        if let Some(landed) = node.landed {
            // "was nothing committed" is not a sentence, so the third landing's
            // refusal says the fact the way a human does. It is still the
            // landing's own story — no merge is claimed anywhere — and the row
            // spells it with `Landed::past` just as before (refactor R12).
            return Some(match landed {
                Landed::NothingCommitted => format!(
                    "agent {id} committed nothing — its worktree is gone; \
                     spawn a fresh agent or work in the root"
                ),
                landed => format!(
                    "agent {id} was {} — its worktree is gone; \
                     spawn a fresh agent or work in the root",
                    landed.past()
                ),
            });
        }
        if node.branch.is_some() && !git::worktree_path(self.ws.root(), id.0).exists() {
            return Some(format!(
                "agent {id}'s worktree is gone — spawn a fresh agent or work in the root"
            ));
        }
        None
    }

    /// A typed message, from the human to the focused agent — the words and
    /// whatever images are attached to them.
    ///
    /// `Err(line)` is a message that did **not** land, with the line the human's
    /// bar says as the reason — a dead mailbox or a gone root. A refusal changes
    /// nothing: not the transcript, not the message box, not the revision — so
    /// an attach client can be answered with the refusal instead of an `Ok`
    /// revision, and the human's caller puts the words and images back in the
    /// box. It used to answer a client's message *after* committing it to the
    /// root's transcript, so a message that never ran was in the conversation
    /// the next run would read (Tier 3 §2).
    ///
    /// The unit of "the human's message" is text plus images, and it is built
    /// once, here: [`Message::user_with_images`] is what the run carries, what
    /// a nudge carries and what the transcript pushes, so the three cannot
    /// disagree about which picture went with which words. The pictures are
    /// carried to the focused agent before that message is built, so what each
    /// of the three holds is a path that agent's own tools resolve
    /// ([`Self::carry_images`]).
    fn deliver(&mut self, text: String, images: Vec<Image>) -> Result<(), String> {
        // A request without a model is a guaranteed refusal from the endpoint,
        // and since discovery runs after the first frame this state is
        // reachable for as long as one fetch takes (finding A9). Saying so is
        // better than the endpoint's own complaint about an empty model id —
        // and the caller owns the box the words go back in.
        if self.cfg().model.is_empty() {
            let line = "no model yet — /model picks one, /url points mush at an endpoint";
            self.fail(line);
            return Err(line.to_string());
        }
        // The model can change between the attachment and the `Enter` that
        // sends it (`Ctrl-P` is a keystroke away), so the gate the box applies
        // when a picture is attached ([`Self::attach_image`]) is asked again
        // here, at the wire: an endpoint that never documented image parts may
        // reject the whole request, which is a turn and the human's money for a
        // message that was never going to arrive. The refusal changes nothing,
        // so the caller's box keeps the words and the pictures.
        if !images.is_empty() {
            let model = self.cfg().model.clone();
            if !vision_capable(&model) {
                let line = blind_model_line(&model);
                self.fail(&line);
                return Err(line);
            }
        }
        let target = self.tree.focused;
        // The focus can move between the `Ctrl-V`/paste and the `Enter` that
        // sends it (`Tab`, `j`/`k` in the tree), so the invariant the attach
        // gate keeps is asked again for the agent that actually receives the
        // message: every path must be one *its* workspace reads back as these
        // bytes, and a picture the attach gate copied for another agent is
        // copied here for this one ([`Self::carry_images`]).
        let images = if images.is_empty() {
            images
        } else {
            match self.carry_images(target, images) {
                Ok(images) => images,
                Err(line) => {
                    self.fail(&line);
                    return Err(line);
                }
            }
        };
        let message = Message::user_with_images(text, images);
        if target == AgentId::ROOT {
            // The root's own phase, not the tree's: a napping orchestrator is idle, and
            // idle, and its next message starts a run rather than nudging a
            // conversation that is not in flight.
            let root_busy = self
                .tree
                .node(AgentId::ROOT)
                .map(|node| node.phase.is_busy())
                .unwrap_or(false);
            if root_busy {
                // Human steering while the root runs: queued as a nudge. If the
                // root's mailbox is dead (it was cancelled), fall through and
                // start a fresh run instead of spinning forever on a ghost.
                let alive = self
                    .tree
                    .agent_tx
                    .get(&AgentId::ROOT)
                    .map(|tx| tx.send(AgentMsg::Nudge(message.clone())).is_ok())
                    .unwrap_or(false);
                if alive {
                    self.chat.expect_human(message.text());
                    self.chat.push_message(AgentId::ROOT, message);
                    self.flush_session();
                    self.say("noted — folded in as the agent continues");
                    return Ok(());
                }
                self.tree.idle(AgentId::ROOT);
            }
            // The human's words belong in the transcript they can see, and the
            // run carries them in the conversation itself: the messages are
            // built *with* the words and the transcript is only committed once
            // the send landed, so a refused message leaves it exactly as it was.
            //
            // Sending does not send the pane to the bottom either: the human
            // chose where to read, and the key that puts a pane back at the
            // newest line is the one they press (finding U3).
            let mut messages = self.chat.conversation();
            messages.push(message.clone());
            match self.tree.agent_tx.get(&AgentId::ROOT) {
                Some(tx) if tx.send(AgentMsg::Run(messages)).is_ok() => {
                    self.chat.expect_human(message.text());
                    self.chat.push_message(AgentId::ROOT, message);
                    // The human's own words are the one thing worth blocking on:
                    // the run they start may take minutes, and a crash in it
                    // must not lose the request. This is one write per turn, not
                    // one per response.
                    self.flush_session();
                    // The run starts now as far as the human is concerned; the
                    // actor's `Running` event will agree with this, and brings
                    // the run's cancel flag with it.
                    self.tree.begin(AgentId::ROOT, None);
                    Ok(())
                }
                _ => {
                    let line = "root agent is gone — Ctrl-N restarts it";
                    self.fail(line);
                    Err(line.to_string())
                }
            }
        } else {
            // A landed agent cannot run again: its file tools resolve their
            // directory from the workspace it was spawned with, so a run would
            // recreate the reclaimed path as a plain directory where no surface
            // — not `git status`, not `git diff`, not `git merge` — could see the
            // work (finding S1). The words stay out of the box and nothing runs.
            if let Some(line) = self.worktree_gone(target) {
                self.fail(&line);
                return Err(line);
            }
            // Nudge a specific agent; running ones fold it in, idle ones rerun.
            // A *parked* child — its thread reclaimed by `park_history`, which
            // leaves the node and the transcript — is woken by these words, and
            // the message is what starts it. If the words cannot be delivered at
            // all the node's phase is put back exactly as it was, instead of
            // leaving a lie on the row (finding B10).
            let previous = self.tree.nudge(target);
            let delivered = self.deliver_to_actor(target, AgentMsg::Nudge(message.clone()));
            if !delivered {
                let line = self.no_actor_line(target);
                self.tree.nudge_failed(target, previous);
                self.fail(&line);
                return Err(line);
            }
            // The human's line lands in the pane on the same turn, and *after*
            // the send: a revived actor's first event cannot be applied until
            // this `update` returns, so the answer can never be painted above
            // the question that asked for it.
            self.chat.expect_human(message.text());
            self.chat.push_message(target, message);
            // The human resumed a child its parent may believe is at rest: the
            // parent's books decide its waits and the one-shared-child guard, so
            // they are told (audit row 1).
            self.tell_parent_running(target);
            // The human's own words are written before this returns, to a child
            // as on the root: the turn they asked for may take minutes, and a
            // crash must not lose the request to the debounce. The phase above
            // is settled first, so the snapshot stores the line and not a
            // `thinking…` that never ran.
            self.flush_session();
            Ok(())
        }
    }

    /// Hand an agent's actor `command`, waking a parked one to take it. Returns
    /// whether somebody took the command.
    ///
    /// A parked child has no actor thread ([`Self::park_history`]); what is left
    /// of its actor is the mailbox, and a send into it fails — which is how this
    /// knows, without a stored "parked" flag that could disagree with the thread
    /// it describes. The node, the id and the transcript never moved, so the
    /// words wake it through the door a restart already uses (`agent::revive`),
    /// seeded with the transcript the pane is showing; the command comes back
    /// with the failed send, so nothing is lost to the probe.
    ///
    /// The root is never parked — it is nobody's child, and the window keeps it —
    /// so a dead root mailbox stays the answer it was: `Ctrl-N` restarts it
    /// (`App::deliver`'s root half, `App::new_chat`).
    fn deliver_to_actor(&mut self, id: AgentId, command: AgentMsg) -> bool {
        let refused = match self.tree.agent_tx.get(&id) {
            Some(tx) => match tx.send(command) {
                Ok(()) => return true,
                Err(error) => error.0,
            },
            // No mailbox at all: a leftover worktree found on disk was never
            // given an actor to wake, and there is no transcript it was asked
            // for one with.
            None => return false,
        };
        if id == AgentId::ROOT {
            return false;
        }
        let Some(node) = self.tree.node(id) else {
            return false;
        };
        let spec = agent::ReviveSpec {
            id: id.0,
            depth: node.depth,
            brief: node.brief.clone(),
            branch: node.branch.clone(),
            messages: self.chat.transcript(id).to_vec(),
            // Its parent's mailbox, when it has one: the completion belongs
            // where every other completion of that child's went, and the
            // parent's books have just been told (or are about to be told) that
            // this child is running (audit row 1). A parent the tree cannot name
            // a mailbox for gets a dead one, and the failed send finds its way
            // back here through the UI (§8.39).
            parent: node
                .parent
                .and_then(|parent| self.tree.agent_tx.get(&parent).cloned()),
        };
        let tx = agent::revive(
            self.tree.handles(),
            self.cell.handle(),
            self.ui_tx.clone(),
            self.tree.conversation().0,
            self.ws.root().to_path_buf(),
            spec,
        );
        // The revival replaced the mailbox the tree holds for this child, and
        // the parent's books still hold the dead one: without this its next
        // `control` finds no actor and takes this same wake path again, every
        // time, for the rest of the session — the message lands, which is why
        // the stale sender went unnoticed (finding H22). The tree is the truth
        // about which sender is live, so the parent is handed exactly the one
        // just built. A parent with no actor of its own needs nothing: its
        // books are gone with the thread, and a parent restored later is seeded
        // from the tree ([`Self::seed_children`]).
        if let Some(parent_tx) = node
            .parent
            .and_then(|parent| self.tree.agent_tx.get(&parent))
        {
            let _ = parent_tx.send(AgentMsg::ChildMailbox {
                id: id.0,
                cmd: tx.clone(),
            });
        }
        // The revived actor's own books start empty, and its children's rows are
        // on screen: they are written here, *before* the command that wakes it,
        // or a `status` in the run those words start answers "no children and
        // no jobs" about a child the human can see (finding H25). The send above
        // is to the parent; this one is to the actor itself, and the send below
        // is the run — so the order that matters is this one coming first.
        self.seed_parent(id, &tx);
        let taken = tx.send(refused).is_ok();
        self.tree.agent_tx.insert(id, tx);
        taken
    }

    /// Why a message to `id` cannot start a run, in the words of the row it is
    /// aimed at.
    ///
    /// A node whose absence of a mailbox is by *design* is not an agent that is
    /// gone: a leftover worktree found on disk was registered with a row and a
    /// branch and never given an actor, and it is sitting on the screen while
    /// `agent::gone` says no such agent exists (finding H10). The refusal is
    /// the same in both cases — nothing runs — but only one of the two is a
    /// disappearance, and the sentence has to say which one the human is
    /// looking at.
    fn no_actor_line(&self, id: AgentId) -> String {
        let leftover = self
            .tree
            .node(id)
            .is_some_and(|node| node.leftover && !self.tree.agent_tx.contains_key(&id));
        if leftover {
            return format!(
                "agent {id} was found on disk and never given an actor — work in the root \
                 or spawn a fresh agent"
            );
        }
        agent::gone(id)
    }

    /// Deliver a command an agent could not hand its own parent: to the parent
    /// the tree names, through the door a human's message uses.
    ///
    /// The actor holds a mailbox with nobody behind it and no way to learn which
    /// one is live; the tree holds the rows, so "who is this agent's parent" is
    /// answered here (`AgentEvent::ParentAsleep`, §8.39). A child whose row is gone
    /// is gone, and the root has no parent row: both are dropped, which is the
    /// answer rather than an error.
    fn hand_to_parent(&mut self, child: AgentId, command: AgentMsg) -> bool {
        let Some(parent) = self.tree.node(child).and_then(|node| node.parent) else {
            return false;
        };
        self.deliver_to_actor(parent, command)
    }

    /// Tell `id`'s parent, when it has one, that the child is running again.
    ///
    /// The parent's own books are the only place a wait and the
    /// one-shared-child guard look, and a human's nudge is a resume the parent
    /// cannot see from its side (audit of the prompt vs behaviour, row 1).
    ///
    /// `pub(crate)` because the attach surface delivers through the same door
    /// as the message box.
    pub(crate) fn tell_parent_running(&self, id: AgentId) {
        let Some(parent) = self.tree.node(id).and_then(|node| node.parent) else {
            return;
        };
        if let Some(tx) = self.tree.agent_tx.get(&parent) {
            let _ = tx.send(AgentMsg::ChildRunning { id: id.0 });
        }
    }

    // -------------------------------------------------------------- attach (M3)

    /// Answer one request from the attach socket. The socket thread hands the
    /// request over and waits; this is the only place an external agent moves
    /// mush, and every op runs the same code a keystroke would — `focus` is
    /// `Enter` on a row, `edit` is the message box and the send — so the
    /// socket cannot reach a state the human could not.
    pub fn handle_attach(&mut self, from: &str, request: &attach::Request) -> attach::Response {
        // A client's op must not end a warning a key armed ([`Self::disarm_quit`]
        // or [`Self::disarm_new_chat`]), so the warning is put back exactly as
        // it stood — but not over a failure the op itself caused, a line the
        // human has to read (finding H9).
        let armed = self
            .status
            .clone()
            .filter(|status| matches!(status.kind, StatusKind::Quit | StatusKind::NewChat));
        let reply = match &request.op {
            attach::Op::Read { agent, since } => self.attach_read(*agent, *since),
            attach::Op::Agents => self.attach_agents(),
            attach::Op::Focus { agent } => self.attach_focus(*agent),
            attach::Op::Edit {
                agent,
                base,
                text,
                send,
            } => self.attach_edit(from, *agent, *base, text, *send),
        };
        if let Some(warning) = armed {
            let said = self.status.as_ref().map(|status| status.kind);
            if !matches!(said, Some(StatusKind::Error)) {
                self.status = Some(warning);
                self.dirty_screen = true;
            }
        }
        attach::Response {
            id: request.id.clone(),
            reply,
        }
    }

    /// The transcript lines of `agent` from `since` (0-based, inclusive), with
    /// a revision a later `edit` must carry.
    ///
    /// The conversation's id travels with every answer: a revision is only
    /// meaningful *within* one conversation, and Ctrl-N is where a client that
    /// polls with its own token would otherwise mistake a fresh transcript for
    /// the one it has been reading (finding A1).
    fn attach_read(&self, agent: u64, since: usize) -> attach::Reply {
        let id = AgentId(agent);
        if !self.tree.has(id) {
            return attach::Reply::Err(attach::ReplyError::bad_request(format!("no agent {id}")));
        }
        let lines: Vec<serde_json::Value> = self
            .chat
            .transcript(id)
            .iter()
            .enumerate()
            .skip(since)
            .map(|(line, message)| {
                serde_json::json!({
                    "line": line,
                    "role": message.role,
                    "text": message.text(),
                })
            })
            .collect();
        attach::Reply::Ok(serde_json::json!({
            "agent": agent,
            "conversation": self.tree.conversation().0,
            "revision": self.chat.revision(id),
            "lines": lines,
        }))
    }

    /// The roster the tree pane paints, read from the tree and never from the
    /// session file, so a client can see the whole tree — phases, parents,
    /// working children — without a copy that lags it (M3 / H1).
    ///
    /// Each entry is a row from [`App::rows`] — the one derivation of what a row
    /// says — plus what a row cannot carry: where the node hangs, its phase's
    /// machine name, its raw branch and worktree, the summary, and the
    /// revision a client edits against. Deriving the row again here is how a
    /// roster starts claiming things the pane does not say (finding R21).
    fn attach_agents(&self) -> attach::Reply {
        let nodes = self.tree.rows();
        let rows = self.rows(&nodes);
        let agents: Vec<serde_json::Value> = nodes
            .iter()
            .zip(rows)
            .map(|(node, row)| {
                serde_json::json!({
                    "id": row.id.0,
                    "parent": node.parent.map(|parent| parent.0),
                    "depth": row.depth,
                    "phase": node.phase.label(),
                    // The row's activity, empty when the agent has nothing to
                    // say; the wire keeps its `null` shape for that.
                    "activity": (!row.activity.is_empty()).then_some(row.activity),
                    "title": row.title,
                    "branch": node.branch.clone(),
                    "worktree": self.attach_worktree(node.id),
                    "focused": row.focused,
                    "children_working": row.waiting,
                    "result_unread": row.result_unread,
                    "unread_children": row.unread_children,
                    "leftover": node.leftover,
                    "summary": node.summary.clone(),
                    "revision": self.chat.revision(node.id),
                })
            })
            .collect();
        attach::Reply::Ok(serde_json::json!({
            // The root's transcript is the conversation; its revision is the
            // one token that covers the chat a client is most likely to edit.
            "conversation": self.tree.conversation().0,
            "revision": self.chat.revision(AgentId::ROOT),
            "agents": agents,
        }))
    }

    /// Where an agent works: its worktree when it has one that is still on
    /// disk, else the main checkout.
    ///
    /// A merged or discarded agent keeps no branch (`live_branch` drops one
    /// whose worktree is gone at restore; a stored session may carry a landing),
    /// and a hand-run
    /// `git worktree remove` takes the directory out from under a branch that
    /// still exists — either way, reporting a dead path is a path a client must
    /// not read files or run commands from (finding A8). The same question
    /// `worktree_gone` asks, answered for the wire.
    fn attach_worktree(&self, id: AgentId) -> String {
        self.agent_root(id).display().to_string()
    }

    /// The root of the workspace an agent's own tools resolve paths in: its
    /// worktree while it has one on disk, else the shared checkout.
    ///
    /// The same answer `agent::revive` gives the actor — `live_branch` keeps a
    /// branch only while its worktree exists, and the workspace is built on the
    /// worktree when there is one — so the UI and the actor cannot disagree
    /// about what a workspace-relative path means to this agent. Two surfaces
    /// ask it: the roster's `worktree` string, through [`Self::attach_worktree`],
    /// and the attach gate's copy of a picture for an agent that works
    /// elsewhere ([`Self::carry_images`]).
    fn agent_root(&self, id: AgentId) -> std::path::PathBuf {
        let path = git::worktree_path(self.ws.root(), id.0);
        match self.tree.node(id) {
            Some(node) if node.branch.is_some() && path.exists() => path,
            _ => self.ws.root().to_path_buf(),
        }
    }

    /// Focus `agent` exactly as `Enter` on its row does: point the tree cursor
    /// at it and run the same path the key does, so the pane, the bar and the
    /// keyboard all move together.
    fn attach_focus(&mut self, agent: u64) -> attach::Reply {
        let id = AgentId(agent);
        // One question, one answer: `point_cursor_at` asks the same "is #N in
        // the tree?" of the rows the human actually sees and answers with
        // whether the cursor landed on one. A separate `has` walk ahead of it
        // was a second answer that agrees only while `rows()` paints every node
        // (refactor R24).
        if !self.tree.point_cursor_at(id) {
            return attach::Reply::Err(attach::ReplyError::bad_request(format!("no agent {id}")));
        }
        self.focus_cursor_row();
        attach::Reply::Ok(serde_json::json!({}))
    }

    /// An external agent's edit: set the message box's draft for `agent`, or
    /// deliver `text` as the human's message, but only when that agent's
    /// transcript still stands at `base`. Otherwise it is a `conflict` with the
    /// revision that moved — never a guess at what the client meant.
    fn attach_edit(
        &mut self,
        from: &str,
        agent: u64,
        base: u64,
        text: &str,
        send: bool,
    ) -> attach::Reply {
        let id = AgentId(agent);
        if !self.tree.has(id) {
            return attach::Reply::Err(attach::ReplyError::bad_request(format!("no agent {id}")));
        }
        let revision = self.chat.revision(id);
        if revision != base {
            return attach::Reply::Err(attach::ReplyError::conflict(revision));
        }
        if send {
            // A client's words are a *message*, not a command: `/quit` typed
            // here would be text an agent reads, where the same words at the
            // human's keyboard would leave mush. Saying that is the
            // contract; the empty case is the one the typed path refuses
            // (finding A4).
            let text = text.trim();
            if text.is_empty() {
                return attach::Reply::Err(attach::ReplyError::bad_request(
                    "refusing to send an empty message",
                ));
            }
            // The same path a typed message takes: `deliver` sends to the
            // focused agent, so aim it there for the turn. The keyboard focus
            // is put back, because an external client steering an agent must
            // not move the human's pane. `deliver` owns the refusals — no
            // model, a gone worktree, a dead mailbox — and makes none of them
            // leave a mark: a message that did not land is a refusal, not the
            // `Ok` revision the client used to be handed, and not a line in a
            // transcript no run will read (findings §6, Tier 2 §3).
            let previous = self.tree.focused;
            self.tree.focused = id;
            let delivered = self.deliver(text.to_string(), Vec::new());
            self.tree.focused = previous;
            if let Err(line) = delivered {
                return attach::Reply::Err(attach::ReplyError::bad_request(line));
            }
        } else {
            self.chat.set_draft(id, text);
            self.say(format!("{from}: set the draft for {id}"));
        }
        attach::Reply::Ok(serde_json::json!({ "revision": self.chat.revision(id) }))
    }

    /// Run one parsed command.
    ///
    /// Matching the variant rather than the typed line is what makes a command
    /// the parser knows but this match does not a compile error instead of a
    /// silent fall-through, and it is what lets the help text, the parser and
    /// these arms all read the same table (`app::commands`).
    fn apply_command(&mut self, command: Command) {
        // A command that is not `/quit` is the human choosing something else,
        // and it takes an armed quit back the way any other key does
        // ([`Self::disarm_quit`]). `/quit` itself is left alone: it is the
        // second step of the two-step quit, typed.
        if !matches!(command, Command::Quit) {
            self.disarm_quit();
        }
        match command {
            Command::Quit => self.request_quit(),
            Command::Help => self.open_help_picker(),
            Command::Notes => self.open_notes_picker(),
            Command::Compact => self.compact_focused(),
            Command::Provider(None) => self.open_provider_picker(),
            Command::Provider(Some(name)) => self.apply_provider(&name),
            Command::Model => self.open_model_picker(),
            Command::Url(url) => {
                // A new endpoint may host a different model with a different
                // window; re-derive it unless the human stated one (finding A5).
                self.switch_endpoint(&url);
                self.refresh_models();
                self.say(format!(
                    "endpoint: {} · {}",
                    self.cfg().base_url,
                    self.context_label()
                ));
                self.persist_user_config();
            }
            Command::ApiKey(None) => match &self.cell.ui().api_key {
                Some(key) => self.say(format!("api key set ({}…)", mask_key(key))),
                // Where a secret lands is the fact a human needs *before*
                // typing one, and this arm said "memory only": the arm below
                // writes the key into the home config in plain text and says
                // so, so the promise was wrong in the one direction that costs
                // a secret (finding A1).
                None => self.say(format!(
                    "no api key — /key <secret> sets one (saved to {})",
                    userconfig::config_path().display()
                )),
            },
            Command::ApiKey(Some(secret)) => {
                // Masked before it is moved: the message quotes the same secret
                // the config now holds, and only its head is ever printed.
                let shown = mask_key(&secret);
                self.cell.edit(|cfg| cfg.api_key = Some(secret));
                self.persist_user_config();
                self.say(format!(
                    "api key set ({shown}…) — saved to {}",
                    userconfig::config_path().display()
                ));
            }
            Command::Models => {
                self.refresh_models();
                self.say(if self.models.is_empty() {
                    format!("no models from {}", self.cfg().models_url())
                } else {
                    format!(
                        "{} models from {}",
                        self.models.len(),
                        self.cfg().models_url()
                    )
                });
            }
        }
        // A command is a transition the human drove: whatever they asked for
        // may have changed the workspace, and the bar they read next should
        // not be yesterday's answer (finding P8). The arms that already
        // refreshed are no worse for the second ask — `git_in_flight` drops it.
        self.refresh_git();
    }

    // ------------------------------------------------------------ providers

    /// Remember the current setup in the home config file so the API key (and
    /// endpoint defaults) survive restarts. Never writes to the workspace.
    fn persist_user_config(&mut self) {
        let user = UserConfig {
            api_key: self.cfg().api_key.clone(),
            provider: self.cfg().provider.name().to_string(),
            base_url: self.cfg().base_url.clone(),
            model: self.cfg().model.clone(),
            // The fields this save does not state — the window, the temperature,
            // the reply cap's name, and the two thinking knobs — keep whatever
            // the file already holds.
            ..UserConfig::default()
        };
        if let Err(error) = user.save() {
            self.fail(format!("could not save home config: {error}"));
        }
    }

    /// Re-fetch the model list from the current endpoint, falling back to the
    /// provider's built-in list when the endpoint cannot answer. An advertised
    /// context window is adopted here, so it lands before the next request.
    pub fn refresh_models(&mut self) {
        self.models = http::list_models(self.cfg());
        self.adopt_advertised_context();
    }

    /// Adopt a model list that finished fetching on its own thread.
    ///
    /// The first entry is only a guess, and only when nothing has named a
    /// model: that is the one case that sent the fetch in the first place
    /// (finding A9). A model the human picked, or one the startup precedence
    /// stated, is not overwritten by whatever the endpoint lists first — and a
    /// list from an endpoint the human has since left is dropped, because the
    /// picker is about the endpoint in use.
    fn adopt_models(&mut self, endpoint: String, models: Vec<http::Model>) {
        if endpoint != self.cfg().base_url {
            return;
        }
        self.models = models;
        if self.cfg().model.is_empty() {
            match self.models.first().map(|model| model.id.clone()) {
                Some(id) => {
                    self.cell.edit(|cfg| cfg.set_model(&id));
                    self.say(format!(
                        "model: {} · {}",
                        self.cfg().label(),
                        self.context_label()
                    ));
                }
                None => self.fail(format!(
                    "no model given and none discovered at {} — pick one with /model",
                    self.cfg().models_url()
                )),
            }
        }
        // The endpoint's advertised window can only be adopted from a fetch
        // that happened, and this one just did.
        self.adopt_advertised_context();
    }

    /// Take the endpoint's word for the window of the model in use, unless the
    /// human stated one: a model list is metadata, so it is taken as stated —
    /// the clamp and the refusal of a stated window are the cell's.
    fn adopt_advertised_context(&mut self) {
        let advertised = self
            .models
            .iter()
            .find(|model| model.id == self.cfg().model)
            .and_then(|model| model.context);
        if let Some(tokens) = advertised {
            self.cell.learn_context(tokens, WindowSource::Advertised);
        }
    }

    fn open_model_picker(&mut self) {
        if self.models.is_empty() {
            self.refresh_models();
        }
        if self.models.is_empty() {
            self.fail("no models — point at an endpoint with /url or /provider first");
            return;
        }
        let cursor = self
            .models
            .iter()
            .position(|model| model.id == self.cfg().model)
            .unwrap_or(0);
        let items = self
            .models
            .iter()
            .map(|model| {
                let label = match model.context {
                    Some(tokens) => format!("{} · {}", model.id, tokens_label(tokens)),
                    None => model.id.clone(),
                };
                PickerItem {
                    id: Some(model.id.clone()),
                    label,
                }
            })
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::Model,
            items,
            cursor,
        });
    }

    /// `/notes`: the lines mush wrote about the focused agent, in full.
    ///
    /// The foot shows at most two of them, so this is the other half of that
    /// cap: a list where a long line is wrapped rather than clipped, which is
    /// what `/help` and a multi-line failure need. Newest last, the same order
    /// the pane reads in, and the cursor opens on the *head of the newest* note
    /// rather than on the list's last row: a note longer than the popup wraps
    /// into many rows, and its last row is the middle of a sentence with the
    /// stamp that says when it happened above the fold (finding T7). One
    /// keypress down from there reads the rest of it.
    fn open_notes_picker(&mut self) {
        let agent = self.tree.focused;
        // Wrapped for the popup this terminal actually paints: the width comes
        // from the same formula `ui::draw_picker` sizes with, so the lines fit
        // the list instead of being clipped by it (`picker_text_width` says
        // what).
        let width = screen::picker_text_width(self.term_width);
        let notes = self.chat.notes_report(agent, session::now_secs(), width);
        if notes.rows.is_empty() {
            self.say(format!("nothing written about {agent} yet"));
            return;
        }
        self.picker = Some(Picker {
            kind: PickerKind::Notes,
            items: notes
                .rows
                .into_iter()
                .map(|row| PickerItem {
                    id: None,
                    label: row,
                })
                .collect(),
            cursor: notes.newest,
        });
    }

    /// `/help`: the keys and the commands, in the readable list `/notes` opens.
    ///
    /// It used to be a notice in the transcript's foot, which shows two of its
    /// forty-odd lines and counts the rest — help a human had to go hunting for
    /// in a list named after something else, if they noticed the count at all
    /// (finding U15). The popup is where a long text is actually read: wrapped
    /// to the width it is painted at, scrollable, and opened at the top.
    fn open_help_picker(&mut self) {
        let width = screen::picker_text_width(self.term_width);
        let items = help_notice(width)
            .lines()
            .map(|line| PickerItem {
                id: None,
                label: line.to_string(),
            })
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::Help,
            items,
            cursor: 0,
        });
    }

    fn open_provider_picker(&mut self) {
        let items: Vec<PickerItem> = Provider::ALL
            .iter()
            .map(|provider| PickerItem {
                id: Some(provider.name().to_string()),
                label: provider.name().to_string(),
            })
            .collect();
        let cursor = items
            .iter()
            .position(|item| item.id.as_deref() == Some(self.cfg().provider.name()))
            .unwrap_or(0);
        self.picker = Some(Picker {
            kind: PickerKind::Provider,
            items,
            cursor,
        });
    }

    fn apply_provider(&mut self, name: &str) {
        let Some(provider) = Provider::parse(name) else {
            self.fail(format!(
                "unknown provider `{name}` — try {}",
                mush_core::provider::names_hint()
            ));
            return;
        };
        self.switch_provider(provider);
        self.refresh_models();
        self.persist_user_config();
        self.say(format!(
            "provider: {} · {}",
            provider.name(),
            self.context_label()
        ));
    }

    /// Point mush at another endpoint, re-deriving the window for it (finding
    /// A5). One write, so no actor sees the new endpoint with the old window.
    fn switch_endpoint(&mut self, url: &str) {
        self.cell.edit(|cfg| {
            cfg.set_base_url(url);
            cfg.rederive_context();
        });
    }

    /// Select a provider: the endpoint it owns, the model mush knows for it,
    /// and the window that goes with both — in one write, so a request cannot
    /// go out against the new provider with the old one's model or window
    /// (finding A5; a window the human stated is kept by `rederive_context`).
    fn switch_provider(&mut self, provider: Provider) {
        self.cell.edit(|cfg| {
            cfg.provider = provider;
            // A provider that owns an endpoint points at it; one that stands for
            // "wherever the human pointed mush" keeps the endpoint already set.
            if provider.spec().switches_endpoint {
                cfg.base_url = provider.default_base_url().to_string();
            }
            let known = cfg.default_models();
            let mut model = cfg.model.clone();
            if !known.contains(&model) {
                if let Some(first) = known.first() {
                    model = first.clone();
                }
            }
            cfg.set_model(&model);
        });
    }

    /// Apply the row `Enter` landed on. The row carries its own id, so nothing
    /// here reads a display string back into a value: the picker's label is
    /// `{id} · {tokens}` for a model, and parsing it picked the half before the
    /// first separator for any id that contained one (Tier 3 §7). A row with no
    /// id is a reading (`Notes`, `Help`), and `Enter` on it changes nothing.
    fn pick(&mut self, kind: PickerKind, item: &PickerItem) {
        let Some(id) = item.id.as_deref() else {
            return;
        };
        match kind {
            PickerKind::Model => {
                // A new model means a new documented window, unless the human
                // stated one (finding A5).
                self.cell.edit(|cfg| cfg.set_model(id));
                self.adopt_advertised_context();
                self.persist_user_config();
                self.say(format!(
                    "model: {} · {}",
                    self.cfg().label(),
                    self.context_label()
                ));
            }
            PickerKind::Provider => self.apply_provider(id),
            // Nothing to apply: the list is a reading, and `key_picker` closes it
            // on Enter exactly as it does on Esc.
            PickerKind::Notes | PickerKind::Help => {}
        }
    }

    /// `/compact`: ask the focused agent to fold its conversation into a
    /// summary now, instead of waiting for the window to fill.
    ///
    /// The request goes to the agent, not to its row: the fold itself is the
    /// actor's job, and its `Compact` event is what replaces the transcript
    /// here, saves the session and moves the meter. What the row shows is the
    /// actor's own answer — `Compacting::Parked` the moment it takes a request
    /// it cannot run yet, `Compacting::Requested` while the summarize call is
    /// on the wire (finding U11); the line this writes is the acknowledgement
    /// for the instant before either arrives. A mailbox that is gone is the one
    /// thing the human has to hear, and it is said plainly rather than left as
    /// a status line about work nobody is doing (finding B10).
    fn compact_focused(&mut self) {
        let target = self.tree.focused;
        // The transcript travels with the request, for the root only: an actor
        // restored from a session starts with none, and a fold is not a run, so
        // this is the only hand-over it will ever get (a child is revived with
        // its transcript). Sending it unconditionally would be wrong — the
        // actor's own copy is the newer one while a run is in flight.
        let messages = if target == AgentId::ROOT {
            self.chat.conversation()
        } else {
            Vec::new()
        };
        // A parked child is woken by the request: folding is a command like any
        // other, and `gone` about an agent whose row and transcript are on
        // screen is exactly the lie parking must not tell (§8.21).
        if self.deliver_to_actor(target, AgentMsg::Compact(messages)) {
            // The word comes from the fold the human just asked for, so the
            // acknowledgement and the row it is answered by read the same
            // verb (refactor R8): a run in flight turns this request into a
            // `Parked` fold a moment later, whose row says `folding at the
            // next step…`.
            self.say(format!("{} {target}…", Compacting::Requested.verb()));
        } else {
            self.fail(self.no_actor_line(target));
        }
    }

    /// Reset the conversation: stop every actor in the old tree, kill what the
    /// old tree left running on the machine, and start a fresh root, so the new
    /// chat has a clean slate and a live mailbox.
    ///
    /// Ctrl-N lands here: a chat that is cleared without restarting the root
    /// would leave the actor holding the old transcript (and a busy flag) while
    /// the UI shows an empty one.
    ///
    /// A conversation with something in it is cleared in two steps, the shape
    /// `Ctrl-Q` uses over live work: the first press says what would go and
    /// where it is kept ([`Self::arm_new_chat`]), the second keeps it as
    /// `.mush/session.json.previous` and only then clears (finding C4). An empty
    /// conversation is cleared by one press, because it has nothing at stake.
    /// The copy is written *before* anything is stopped or cleared, and a copy
    /// that cannot be written refuses the whole key: a keystroke must not be
    /// able to lose what the copy exists to save.
    fn new_chat(&mut self) {
        let lines = self.chat.lines_to_drop();
        if lines > 0 && !self.new_chat_armed() {
            self.arm_new_chat(lines);
            return;
        }
        if lines > 0 {
            if let Err(cannot) = self.keep_cleared_conversation() {
                self.fail(format!("{cannot}; nothing cleared"));
                return;
            }
        }
        self.stop_all();
        // Ctrl-N kills the old tree's processes here and not by dropping the
        // tree: a running job's watch thread holds its own `Arc<Registry>`
        // (`Registry::launch`), so the registry is not dropped with the tree and
        // its `Drop` backstop cannot fire until the job ends — which is exactly
        // the job it is meant to end. The one handle left, the tree's, is
        // replaced two lines below; this is the last moment the job list is
        // reachable, so it is where a kill has to be said out loud (see the
        // `Drop` docs in `jobs.rs`).
        self.tree.handles().jobs.kill_all();
        // The respawned root owns its own config cell, conversation tag, and id
        // counter; the UI adopts the handle with the fresh tree, or a later
        // /model would never reach the agent.
        let root = spawn(
            ConfigHandle::own(self.cell.ui().clone()),
            self.ui_tx.clone(),
            self.ws.root().to_path_buf(),
        );
        self.cell.adopt_handle(root.cfg.clone());
        self.tree = AgentTree::rooted(root);
        // Running agents vanish with the old conversation; worktrees they left
        // behind are still reviewable (they are re-listed below).
        self.chat.clear();
        self.spin = 0;
        self.discover_worktrees();
        self.refresh_git();
        // The old conversation is gone from this moment: if the write were left
        // to the debounce, a crash would bring it back with the next start.
        self.flush_session();
        self.say("new chat — agents stopped, root restarted");
    }

    /// Keep the conversation a new chat is about to clear, beside the store,
    /// under the name the warning points at.
    ///
    /// The snapshot is the live conversation, not the file: the file may lag the
    /// screen by up to [`SESSION_DEBOUNCE`], and the copy the key promises is of
    /// what the human was reading. Written here, synchronously, for the one
    /// caller that must wait on its own copy — the clear that follows must not
    /// happen before the copy is safe ([`session::keep_previous`]).
    fn keep_cleared_conversation(&self) -> Result<(), String> {
        session::keep_previous(self.ws.root(), self.session_snapshot()).map(|_| ())
    }

    /// `Ctrl-T`: show or hide the model's reasoning above the turn it decided.
    ///
    /// A view, so it is not said and not stored: a notice would be written into
    /// the conversation ([`Chat::stored_notices`]) and the reasoning is already
    /// there — `Ctrl-T` changes only what the panes paint, which is what
    /// `dirty_screen` records. Every pane reads through the same `Chat`, so one
    /// flip is every pane's.
    fn toggle_reasoning(&mut self) {
        self.chat.set_reasoning(!self.chat.shows_reasoning());
        self.dirty_screen = true;
    }

    /// `Ctrl-F`: the focused pane takes the whole screen, and back.
    ///
    /// Why the view exists: mush deliberately never captures the mouse (finding
    /// K3), so selection belongs to the terminal and a drag takes a rectangle
    /// of screen cells — at 80 columns and up, the left column is the agents
    /// pane, and a drag across the conversation comes back with the tree's rows
    /// in front of the paragraph. The cheapest honest answer is a view where
    /// one pane covers the screen, so a rectangle can hold one pane's text and
    /// nothing else.
    ///
    /// A view like [`Self::toggle_reasoning`], so it is not said and not
    /// stored: `dirty_screen` is the whole record, and `Ctrl-N` keeps the
    /// choice — the view is the human's and not the conversation's.
    /// [`Self::screen`] lays the panes out from this and `self.focus`,
    /// so `Tab` is what switches which pane is full-screen.
    fn toggle_zen(&mut self) {
        self.zen = !self.zen;
        self.dirty_screen = true;
    }

    /// Forget the finished children the history window is done with, and park
    /// the actor threads of the ones it keeps — the two halves of §8.21's
    /// "everything needs a cap, even a high one", run from `tick` because both
    /// are filters over the tree rather than events.
    ///
    /// **The five steps, in one place, in this order.** Forgetting a child is
    /// not dropping a node: it is five things that used to happen nowhere. The
    /// actor has to be told to end — [`AgentMsg::Shutdown`] is the only thing
    /// that stops a thread, and its own arm ends the run it is in and kills the
    /// jobs the agent owns with it (`agent::absorb`, `drain_signals`); then the
    /// child's parent is told to forget it — [`AgentMsg::ForgetChild`] drops the
    /// name from the parent's own books, or `status` lists a row that is no
    /// longer on screen (finding H19); then the tree drops the node and
    /// everything keyed by its id; then the conversation drops the transcript,
    /// the voices, the revision an attach client holds and the pane's reading
    /// position ([`Chat::forget`]); and then the file is told, or the next save
    /// writes back the child this forgot (`mark_session_dirty` — the write
    /// itself waits for the debounce, so a reap costs no write).
    ///
    /// The forget step is a plain send into the parent's mailbox on purpose. A
    /// parent whose actor is parked has no books left to correct — the send
    /// finds no receiver and is dropped — and a parent whose session is
    /// restored later re-derives its books from the tree, which no longer has
    /// the child (`Self::seed_children`); neither is woken just to drop a name.
    /// An idle parent that *is* live absorbs the command as book-keeping and
    /// stays idle (`agent::absorb`), so this step starts no run anywhere. The
    /// parent is read before `tree.reap` below, the last moment the node is
    /// still there to say whose child it was.
    fn reap_history(&mut self) {
        for id in self.tree.past_history() {
            let parent = self.tree.node(id).and_then(|node| node.parent);
            if let Some(tx) = self.tree.agent_tx.get(&id) {
                let _ = tx.send(AgentMsg::Shutdown);
            }
            if let Some(tx) = parent.and_then(|parent| self.tree.agent_tx.get(&parent)) {
                let _ = tx.send(AgentMsg::ForgetChild { id: id.0 });
            }
            self.tree.reap(&[id]);
            self.chat.forget(id);
            self.mark_session_dirty();
        }
    }

    /// Park the actor threads of the children the tree has stopped listening
    /// to, keeping the newest few warm (see `tree::WARM_CHILDREN`).
    ///
    /// A node costs a struct; a thread costs a stack, an entry in every
    /// scheduler's picture and a name in `/proc`, and every finished child used
    /// to hold `mush-agent-{id}` until mush quit — a run with a hundred
    /// children held a hundred threads, of which at most the handful a human is
    /// talking to were ever going to wake again (§8.21). Parking ends the
    /// thread and nothing else: the node, the id and the transcript stay
    /// exactly where they fall, and the next message to that child rebuilds its
    /// actor from the transcript on screen ([`Self::deliver_to_actor`]), so what
    /// the human sees does not change by one row.
    ///
    /// The send is the whole probe. A parked child's mailbox still exists —
    /// it is what the tree and its parent hold — and it has no receiver, so a
    /// `Shutdown` into it fails and says so; a live actor takes it and ends.
    /// Nothing here reads a stored "parked" flag, because there is none to get
    /// out of step with the thread it describes.
    fn park_history(&mut self) {
        for id in self.tree.parkable() {
            let Some(tx) = self.tree.agent_tx.get(&id) else {
                // A leftover worktree found on disk: no actor was ever started
                // for it, so there is no thread to reclaim.
                continue;
            };
            if tx.send(AgentMsg::Shutdown).is_err() {
                // Already parked. The parent was told when it happened, and the
                // window recomputes the same set every frame.
                continue;
            }
            // The parent's books outlive the child's actor, and its run
            // numbering does not: a woken child reports run 1 again, and a book
            // that still holds that number as read would swallow the result
            // (finding B24, `agent::note_parked`).
            if let Some(parent) = self.tree.node(id).and_then(|node| node.parent) {
                if let Some(tx) = self.tree.agent_tx.get(&parent) {
                    let _ = tx.send(AgentMsg::ChildParked { id: id.0 });
                }
            }
        }
    }

    /// Ask every actor in the tree to shut down. `Shutdown`, not `Stop`: a
    /// cancelled actor goes back to waiting for work (which is what Ctrl-C
    /// should do), while Ctrl-N needs the threads to be gone — and an actor
    /// holds its own mailbox open, so it never notices that the UI let go.
    fn stop_all(&self) {
        for tx in self.tree.agent_tx.values() {
            let _ = tx.send(AgentMsg::Shutdown);
        }
    }

    /// Note that what the session stores has changed.
    ///
    /// O(1), because the caller is a message handler on the UI thread: a
    /// streamed response must not rebuild a session, let alone write one. The
    /// mark is deliberately *not* moved by later changes — a stream that never
    /// pauses still reaches the file once per `SESSION_DEBOUNCE` instead of
    /// being deferred until it stops.
    fn mark_session_dirty(&mut self) {
        if self.session_dirty_at.is_none() {
            self.session_dirty_at = Some(Instant::now());
        }
    }

    /// Rebuild the session and hand it to the writer, which serializes it and
    /// writes it on its own thread.
    ///
    /// Called from the debounced tick. The rebuild is the one part of a save
    /// that cannot leave this thread — the conversation lives here — so it is
    /// paid once per `SESSION_DEBOUNCE` rather than once per streamed message,
    /// and never while a burst of them is being drained.
    fn save_session(&mut self) {
        self.session_dirty_at = None;
        let session = self.session_snapshot();
        self.session_save.save(session);
    }

    /// Write the session and wait for the disk: the call sites that mean "this
    /// must not be lost" — the human's own message, a command that changed what
    /// is stored, a compaction, quitting.
    ///
    /// Nothing else waits, which is what keeps the wait off the message path: a
    /// streamed response is covered by the debounce and by the flush on the way
    /// out, so the most a crash can cost is the last `SESSION_DEBOUNCE` of chat.
    fn flush_session(&mut self) {
        self.session_dirty_at = None;
        let session = self.session_snapshot();
        self.session_save.save(session);
        self.session_save.flush();
        if let Some(error) = self.session_save.take_error() {
            self.fail(format!("could not save session: {error}"));
        }
        // A save is a moment the state is being fixed; the repository is part
        // of that picture, so refresh it here rather than leaving the bar with
        // a read older than the file on disk (finding P8). Only the human-
        // driven flushes come through here — a streamed response uses the
        // debounced `save_session` — so this does not put a git process on the
        // message path.
        self.refresh_git();
    }

    /// The conversation as it is stored: the root transcript, every subagent's,
    /// and the endpoint selection it was held against.
    fn session_snapshot(&self) -> Session {
        // Every subagent, not just the root: without this a relaunch forgot
        // each child's context, and "continue that agent" meant writing the
        // brief again from scratch.
        let agents = self
            .tree
            .agents
            .iter()
            .filter(|node| node.id != AgentId::ROOT)
            .map(|node| session::AgentSession {
                id: node.id.0,
                parent: node.parent.map(|parent| parent.0),
                depth: node.depth,
                brief: node.brief.clone(),
                title: node.title.clone(),
                branch: node.branch.clone(),
                status: match &node.phase {
                    Phase::Done => session::StoredStatus::Done,
                    Phase::Stopped => session::StoredStatus::Stopped,
                    Phase::Failed(error) => session::StoredStatus::Failed(error.clone()),
                    // A run that never ended is stored as exactly that, and it
                    // stays stored that way until a new run replaces it: the
                    // fact must not decay into `Idle` on a second restart, or a
                    // `⚠` would be a mark that lasts one launch (finding H2).
                    Phase::CutOff => session::StoredStatus::CutOff,
                    // At rest is not in flight. `Idle` has to be named here
                    // because the wildcard below is the *phases of a run in
                    // flight*, and an agent nobody has asked to do anything is
                    // not one of them: storing it as `running` would bring it
                    // back `⚠ cut off` on the next launch, which is the same
                    // flattened lie — one run's state written for an agent that
                    // had no run — in the other direction (finding H2).
                    Phase::Idle => session::StoredStatus::Idle,
                    // A run *in flight* is stored as `running` — not as `Idle`,
                    // which is the one thing it is not. That value is never an
                    // ending, so a file that still carries it is the record of a
                    // run nobody finished: the harness was SIGTERM'd, the
                    // terminal closed, the process crashed. This is what turns a
                    // killed agent into a `⚠` on the next launch instead of a
                    // row that looks like it was never asked to do anything
                    // (finding H2).
                    _ => session::StoredStatus::Running,
                },
                landed: node.landed.map(|landed| match landed {
                    Landed::Merged => session::StoredLanded::Merged,
                    Landed::NothingCommitted => session::StoredLanded::NothingCommitted,
                    Landed::Discarded => session::StoredLanded::Discarded,
                }),
                leftover: node.leftover,
                summary: node.summary.clone(),
                result_unread: node.result_unread,
                // The system prompt is regenerated on the way back in, since it
                // names a workspace that may have moved.
                messages: self
                    .chat
                    .transcript(node.id)
                    .iter()
                    .filter(|message| message.role != "system")
                    .cloned()
                    .collect(),
            })
            .collect();
        Session {
            model: self.cfg().model.clone(),
            provider: self.cfg().provider.name().to_string(),
            base_url: self.cfg().base_url.clone(),
            // Only a window the human stated is worth remembering; a discovered
            // one is re-read next time, so it cannot go stale.
            context: self
                .cfg()
                .context_explicit
                .then_some(self.cfg().context_tokens),
            messages: self.chat.transcript(AgentId::ROOT).to_vec(),
            agents,
            // A failure is the one line worth coming back to; a command's answer
            // is not (see `Chat::stored_notices`).
            notices: self.chat.stored_notices(),
        }
    }

    // ------------------------------------------------------------------ input

    /// One key: [`keys::key`] decides what it means and [`Self::apply_intent`]
    /// carries the decision out. Nothing from here down reads a `KeyCode`, so a
    /// binding is testable without an `App` — which the old shape, where an arm
    /// both matched a key and did its work, made impossible (finding B2).
    fn on_key(&mut self, key: KeyEvent) {
        let intent = keys::key(
            self.focus,
            self.picker.is_some(),
            self.chat.selecting(),
            key,
        );
        // Below the floor the screen is a single notice: a key whose effect the
        // human cannot see — `Ctrl-N` wipes the conversation and starts a new
        // one — must not act. `Ctrl-Q` is the exception: a terminal too small
        // to read is still a way out. The floor is `App`'s state, not the
        // painter's early return (finding P11 / refactor B3).
        if self.below_floor() && intent != Intent::Quit {
            return;
        }
        self.apply_intent(intent);
    }

    /// Do what an intent says. One arm per intent, every side effect of the
    /// keyboard in one readable list: which pane moves, what is sent, what is
    /// picked, and which config change is remembered.
    fn apply_intent(&mut self, intent: Intent) {
        // Any key but `Ctrl-Q` takes an armed quit back — `Ctrl-C` above all,
        // because "stop this agent" is the opposite of "quit" — and the keys
        // the human is *typing* are the exception: `/quit` is spelled out key
        // by key, and a warning that disarmed itself on the way in could never
        // run it. What the human sends from the box is judged where it lands,
        // in `apply_command`.
        if intent != Intent::Quit && !matches!(intent, Intent::Chat(_) | Intent::Send) {
            self.disarm_quit();
        }
        // A new chat's warning has no typed road to protect, so anything but
        // its own key takes it back — a human who is typing a message is not
        // pressing `Ctrl-N` twice in a row. Its own key is left standing for
        // the same reason `Ctrl-Q` is: the second press reads the arm here,
        // before `new_chat` (finding C4).
        if intent != Intent::NewChat {
            self.disarm_new_chat();
        }
        match intent {
            Intent::Ignore => {}
            Intent::Quit => self.request_quit(),
            Intent::NewChat => self.new_chat(),
            Intent::Interrupt => self.interrupt(),
            Intent::InterruptAll => self.interrupt_all(),
            Intent::OpenModelPicker => self.open_model_picker(),
            Intent::ToggleReasoning => self.toggle_reasoning(),
            Intent::ToggleZen => self.toggle_zen(),
            // `Tab` leaves the select mode behind: the mode is what the keyboard
            // was doing, and the key the human pressed is the one that says they
            // are done with the pane they were reading (the mode's own `Esc` is
            // the other way out, and it is the mode's).
            Intent::CycleFocus(direction) => {
                self.chat.cancel_select();
                self.cycle_focus(direction);
            }
            Intent::Select(key) => self.select_key(key),
            Intent::PickerClose => self.picker = None,
            Intent::PickerPick => self.pick_cursor(),
            Intent::PickerMove(step) => self.move_picker(step),
            Intent::PickerFirst => self.set_picker_cursor(0),
            Intent::PickerLast => self.set_picker_cursor(usize::MAX),
            Intent::TreeMove(step) => self.move_tree_cursor(step),
            Intent::TreeWalk(direction) => self.tree_walk(direction),
            Intent::TreeFirst => self.tree.cursor_top(),
            Intent::TreeLast => self.tree.cursor_bottom(),
            Intent::TreeFocus => self.focus_cursor_row(),
            Intent::TreeCancel => self.cancel_cursor_row(),
            Intent::TreeBackToRoot => {
                self.tree.focus(AgentId::ROOT);
            }
            Intent::Send => self.send_message(),
            Intent::AttachClipboardImage => self.attach_clipboard_image(),
            // The chat does what the key says to the box, and a key that leaves
            // a line does it here: the box's losses are the chat's fact, the bar
            // is the app's surface, and this is the one place the two meet.
            Intent::Chat(key) => {
                if let Some(line) = self.chat.apply(self.tree.focused, key) {
                    self.say(line);
                }
            }
        }
    }

    /// The select mode's keys, where the mode meets the app: `Ctrl-Y` opens it
    /// on the pane the tree has focused, `Enter` hands the copied text to the
    /// clipboard, and every other key is the chat's own.
    ///
    /// The mode belongs to the conversation the chat pane shows, so the agent
    /// here is [`Tree::focused`] — the same agent every other chat key is about.
    /// A pane with nothing to stand on says so in the bar rather than opening a
    /// cursor over nothing.
    fn select_key(&mut self, key: SelectKey) {
        let on = self.tree.focused;
        match key {
            // Already on: the mode is the keyboard's, and "start" has nothing
            // left to start. A second `Ctrl-Y` that restarted the cursor would
            // silently throw away a selection the human was building.
            SelectKey::Start => {
                if !self.chat.selecting() {
                    if let Some(line) = self.chat.start_select(on) {
                        self.say(line);
                    }
                }
            }
            SelectKey::Copy => {
                if let Some(copied) = self.chat.select_apply(on, SelectKey::Copy) {
                    self.copy_text(copied);
                }
            }
            key => {
                self.chat.select_apply(on, key);
            }
        }
    }

    /// `Enter` in the select mode: put the copied text on the system clipboard.
    ///
    /// The write is subprocesses with a deadline ([`clipboard::write_text`]),
    /// so it happens on a thread of its own and the answer comes back as a
    /// message — the read road's rule (`Ctrl-V`), for the write road: a
    /// clipboard owner that never answers costs the human a status line, never a
    /// frame. The line mush says on success is the one built with the copy
    /// (`Chat::copy`), because only there is the count of lines and bytes; the
    /// write decides whether it is said at all.
    fn copy_text(&mut self, copied: Copied) {
        let tx = self.ui_tx.clone();
        let conversation = self.tree.conversation();
        // The road the app holds is what the thread runs, so a test's writer is
        // the one `Enter` reaches — no program is spawned by it.
        let write = Arc::clone(&self.write_clipboard);
        std::thread::spawn(move || {
            let result = write(&copied.text);
            let _ = tx.send(Msg::Copied {
                conversation,
                line: copied.line,
                result,
            });
        });
    }

    /// `Ctrl-V`: read the clipboard for an image and attach it to the box.
    ///
    /// The read is subprocesses with a deadline (`crate::clipboard`), so it
    /// happens on a thread of its own and the answer comes back as a message:
    /// a clipboard owner that never answers must cost the human a status line,
    /// never a frame. The message is stamped with the conversation that asked
    /// for it, exactly as [`Msg::Agent`] is, so a Ctrl-N in between drops the
    /// answer instead of attaching it to the new chat.
    fn attach_clipboard_image(&mut self) {
        let ws = self.ws.clone();
        let tx = self.ui_tx.clone();
        let conversation = self.tree.conversation();
        std::thread::spawn(move || {
            let result = clipboard::read_image(&ws);
            let _ = tx.send(Msg::Clipboard {
                conversation,
                result,
            });
        });
    }

    /// The images to carry to the agent at `id`: every one whose path that
    /// agent's own workspace reads back as its own bytes, and a fresh copy
    /// under its `.mush/paste/` for every one it does not.
    ///
    /// A picture's file is read in the shared checkout — a paste resolves there
    /// and a clipboard copy is written there — while an agent with a worktree
    /// of its own resolves the same relative path against that worktree, where
    /// the file is not, or is another revision of it. The `Image` holds its
    /// bytes, so the file the placeholder names can be written where the
    /// receiving agent's tools look: [`Workspace::save_pasted_image`] is the
    /// one writer of `.mush/paste/`, and the copy is what the placeholder's
    /// "read the file again" promises to keep. The bytes are compared, not just
    /// the path's existence, because that promise is the *same picture*: a
    /// worktree can hold an older commit of the file the human pasted.
    ///
    /// The comparison is the receiving workspace's own reader
    /// ([`Workspace::read_image`]), not a raw read of the path: the reader
    /// stats before it opens, so a FIFO named like the picture answers
    /// `Ok(None)` instead of blocking this call — which runs on the UI thread
    /// at every attach and every send — and its whole read is bounded by the
    /// transport cap whatever the file claims or grows to. A file that is not
    /// there, not a regular file, not an image, or not these bytes is "the
    /// receiver does not hold it", and the copy is what it gets.
    ///
    /// `Err` is a picture whose copy cannot be written: the line names the path
    /// and the reason, and the caller refuses the attachment rather than hand
    /// an agent a path that resolves nowhere.
    ///
    /// The bytes are the picture whole: every road that builds an [`Image`]
    /// reads it to its end and refuses a buffer that stopped at a cap
    /// ([`Workspace::pasted_images`], the clipboard's read), so the copy is
    /// written from bytes the request will carry entire — never a prefix of
    /// one — and [`Workspace::save_pasted_image`] is handed `false` for its
    /// `cut_at_the_cap`.
    fn carry_images(&self, id: AgentId, images: Vec<Image>) -> Result<Vec<Image>, String> {
        let root = self.agent_root(id);
        let ws =
            Workspace::new(&root).map_err(|e| format!("cannot open {}: {e}", root.display()))?;
        images
            .into_iter()
            .map(|image| {
                // The receiver's own reader answers, not a raw `fs::read`: it
                // stats before it opens (a FIFO is `Ok(None)`, never a blocked
                // open) and bounds its read by the transport cap
                // ([`Workspace::read_image`], this method's doc).
                let same = ws
                    .read_image(&image.path)
                    .ok()
                    .flatten()
                    .is_some_and(|seen| seen.bytes == image.bytes);
                if same {
                    return Ok(image);
                }
                let from = image.path.clone();
                // Whole bytes, not a prefix: an `Image` exists only when a road
                // read the picture to its end within the cap, so this is the
                // `false` side of `cut_at_the_cap` (see this method's doc).
                ws.save_pasted_image(image.bytes, false).map_err(|e| {
                    format!(
                        "cannot copy {from} into {}/.mush/paste: {e}",
                        root.display()
                    )
                })
            })
            .collect()
    }

    /// The gate for a paste that named several images: the same facts
    /// [`Self::attach_image`] weighs, asked once for the gesture instead of
    /// once per picture.
    ///
    /// A paste of four paths is one "these pictures" the way one path is one
    /// "this picture", and the box's lines are one row: four `attached` lines
    /// would be noise fighting for it. So a one-image batch *is*
    /// [`Self::attach_image`] — the single paste keeps every line it has —
    /// and a batch of several says one line naming the count
    /// (`attached 4 images — Enter sends them with the message`).
    ///
    /// Four facts can stop the whole gesture before any of it attaches, and
    /// each is asked once, of the batch: no model at all, a model not
    /// documented to see, the window's bound and the box's own bound. A stop
    /// means *nothing* attaches and the whole paste lands as text with the one
    /// refusal [`Self::attach_image`] says — a gesture is atomic, so half a
    /// batch is not a thing this door makes.
    ///
    /// The room warning is one line too — the room is the budget minus the
    /// *focused agent's* transcript, the same arithmetic
    /// [`Self::attach_image`] does, with the images already in the box counted
    /// by hand — and it carries the count a single picture's line cannot: how
    /// many of the batch's pictures the room left cannot hold, each asked at
    /// its turn the question [`Self::attach_image`] asks of one
    /// (`cost + pending > room`). Every picture is attached whatever the room
    /// says: the human decides what to send.
    ///
    /// The pictures are carried to the focused agent first, because the paste
    /// resolved them in the shared checkout and the box must hold paths that
    /// the agent the message is sent to can read ([`Self::carry_images`]).
    ///
    /// What bounds a paste of a hundred pictures is what bounds one image — the
    /// window's budget and the box's own byte bound ([`BOX_IMAGE_BYTES`]), both
    /// of which refuse, and the per-file transport cap the reader applies
    /// ([`mush_core::workspace::IMAGE_FILE_CAP`]) — not a batch limit invented
    /// at this door.
    ///
    /// Answers whether the batch was attached, so the paste arm inserts the
    /// words as text when it was not.
    fn attach_images(&mut self, mut images: Vec<Image>) -> bool {
        if images.len() == 1 {
            let only = images.pop().expect("a batch of one holds one image");
            return self.attach_image(only);
        }
        let model = self.cfg().model.clone();
        if model.is_empty() {
            let line = "no model yet — /model picks one, /url points mush at an endpoint";
            self.fail(line);
            return false;
        }
        if !vision_capable(&model) {
            self.fail(blind_model_line(&model));
            return false;
        }
        // The pictures are carried to the agent this paste is for before the
        // weights are read, because the paths — and so the weights and the
        // lines — are about what the box will hold: pictures that agent's own
        // tools resolve ([`Self::carry_images`]).
        let target = self.tree.focused;
        let images = match self.carry_images(target, images) {
            Ok(images) => images,
            Err(line) => {
                self.fail(&line);
                return false;
            }
        };
        let budget = self.cfg().history_budget();
        let room = budget.saturating_sub(self.chat.used_weight_for(target));
        let pending: usize = self
            .chat
            .attachments()
            .iter()
            .map(Image::weight)
            .fold(0, usize::saturating_add);
        let count = images.len();
        // The window's bound, asked of the batch at once: the pictures already
        // in the box plus every picture of the paste, in paste order, against
        // the whole history budget. A paste that crosses it attaches nothing —
        // the request could never fit however much history is trimmed, so
        // attaching would only spend a turn on a refusal. Every sum saturates,
        // because a header can claim a picture larger than any `usize`.
        let mut running = pending;
        let mut over_budget = 0usize;
        let mut first_over: Option<String> = None;
        for image in &images {
            let cost = image.weight();
            if cost.saturating_add(running) > budget {
                over_budget += 1;
                if first_over.is_none() {
                    first_over = Some(image.path.clone());
                }
            }
            running = running.saturating_add(cost);
        }
        if over_budget > 0 {
            let first = first_over.unwrap_or_default();
            self.fail(format!(
                "{over_budget} of {count} images would put the box over the whole history budget \
                 ({}) — the pictures already in the box weigh {}, and `/compact` folds the \
                 conversation, not the pictures: a request carrying them would go out over the \
                 window and the endpoint would refuse it. Downscale them (`convert {first} \
                 -resize 50% small.png`) and paste them again, or send the box's pictures first",
                size_label(budget),
                size_label(pending)
            ));
            return false;
        }
        // The box's own bound, in the currency the window cannot weigh: the
        // bytes the box holds while the pictures wait. Attaching this batch
        // would keep more file than the box allows, so nothing attaches.
        let pending_bytes: usize = self
            .chat
            .attachments()
            .iter()
            .map(|image| image.bytes.len())
            .fold(0, usize::saturating_add);
        let adding: usize = images
            .iter()
            .map(|image| image.bytes.len())
            .fold(0, usize::saturating_add);
        if adding.saturating_add(pending_bytes) > BOX_IMAGE_BYTES {
            let first = images
                .first()
                .map(|image| image.path.clone())
                .unwrap_or_default();
            self.fail(format!(
                "{count} images weighing {} would put the box over the {} of picture bytes it \
                 may hold — the box already holds {}, and the window's token bound cannot bound \
                 bytes. Downscale them (`convert {first} -resize 50% small.png`) and paste them \
                 again, or send the box's pictures first",
                size_label(adding),
                size_label(BOX_IMAGE_BYTES),
                size_label(pending_bytes)
            ));
            return false;
        }
        // The count the warning says: each picture, in paste order, is asked
        // the question [`Self::attach_image`] asks of one — does its cost fit
        // the room left once the pictures before it have had theirs? Every
        // sum saturates, because a header can claim a picture larger than any
        // `usize`.
        let mut running = pending;
        let mut at_stake = 0usize;
        let mut first_at_stake: Option<String> = None;
        for image in &images {
            let cost = image.weight();
            if cost.saturating_add(running) > room {
                at_stake += 1;
                if first_at_stake.is_none() {
                    first_at_stake = Some(image.path.clone());
                }
            }
            running = running.saturating_add(cost);
        }
        for image in images {
            self.chat.attach(image);
        }
        if at_stake > 0 {
            let first = first_at_stake.unwrap_or_default();
            self.fail(format!(
                "{at_stake} of {count} images will not fit the room left for history ({}) — \
                 attaching them costs the oldest turns of the conversation, which are dropped \
                 at the next request to make room. `/compact` folds them into a summary \
                 instead, or downscale them (`convert {first} -resize 50% small.png`) and paste \
                 them again",
                size_label(room)
            ));
            return true;
        }
        self.say(format!(
            "attached {count} images — Enter sends them with the message"
        ));
        true
    }

    /// The one gate an image goes through, whichever road it came by — a paste
    /// that named a file, or `Ctrl-V`. Answers whether it was attached, so the
    /// paste arm knows to insert the path as text when it was not.
    ///
    /// Five facts are said here, and each gets its own line because each needs
    /// a different move from the human. Four refuse the attachment:
    ///
    /// - **No model at all.** Sending would be a guaranteed refusal, and it is
    ///   said in the words [`Self::deliver`] already uses for the same state.
    /// - **A model not documented to see.** An image sent to one is a rejected
    ///   request — a whole turn and the human's money — so it is refused
    ///   *before* the wire, and `Ctrl-P` is named as the road: the one road
    ///   that changes the fact.
    /// - **A picture the window cannot carry.** Its weight — with the pictures
    ///   already in the box — is past the whole history budget
    ///   ([`mush_core::Config::history_budget`]). The system prompt and the
    ///   opening task cannot be dropped and no trim shrinks a picture, so a
    ///   request carrying it would go out over the window and the endpoint
    ///   would refuse it: attaching would only spend a turn discovering that.
    ///   The line names the road: a downscale. `/compact` is named only when
    ///   the pictures already in the box are part of the sum, and it is named
    ///   for what it is — a fold of the *conversation*, whose room is not the
    ///   weight over this bound. (The batch door's line says how many of how
    ///   many are at stake.)
    /// - **A box past its own byte bound.** [`BOX_IMAGE_BYTES`] is the box's
    ///   memory — what is held while the pictures wait — and it is not the
    ///   window's bound: a picture is priced by its pixels, so a 2 MB file of a
    ///   100×100 png weighs fourteen tokens, and a hundred of them pass every
    ///   token bound the window has while the box holds 200 MB. Also refused;
    ///   the line names which bound it was.
    ///
    /// The fifth attaches anyway — the human decides what to send — and says
    /// the fact they cannot see:
    ///
    /// - **A picture with no room left in the conversation.** The room is what
    ///   remains of [`mush_core::Config::history_budget`] once the system
    ///   prompt and the *focused agent's* transcript are weighed, and the
    ///   images already in the box count against it too: they are not in the
    ///   transcript yet. Attaching it costs the oldest turns —
    ///   [`mush_core::transcript::trim_history`] drops them at the next
    ///   request to make room — so the line says so and names the two roads
    ///   that spare them: `/compact`, which folds those turns into a summary,
    ///   and a downscale, which makes the picture cost less.
    ///
    /// A picture whose weight exactly equals the room left fits, and one that
    /// lands exactly on the budget fits too: this gate refuses (and warns) only
    /// *above* `cost + pending` — past the budget, past the box's bytes, past
    /// the room — the same inclusive comparison the trimmer makes at the
    /// ceiling ([`mush_core::transcript::trim_history`]: a transcript at
    /// `budget` is inside, one byte more is cut), so a picture parked on a
    /// boundary is not refused by one rule and sent by another.
    ///
    /// Every path that leaves this gate is one the agent the message is sent to
    /// can read: the picture is carried to the focused agent first
    /// ([`Self::carry_images`]), because the road that read it resolved it in
    /// the shared checkout and an agent with a worktree of its own resolves it
    /// against that worktree instead.
    ///
    /// Everything else attaches, and the line says the image, its format and
    /// its size, and how to send it.
    fn attach_image(&mut self, image: Image) -> bool {
        let model = self.cfg().model.clone();
        if model.is_empty() {
            let line = "no model yet — /model picks one, /url points mush at an endpoint";
            self.fail(line);
            return false;
        }
        if !vision_capable(&model) {
            self.fail(blind_model_line(&model));
            return false;
        }
        let target = self.tree.focused;
        let mut images = match self.carry_images(target, vec![image]) {
            Ok(images) => images,
            Err(line) => {
                self.fail(&line);
                return false;
            }
        };
        let image = images.pop().expect("one image in, one image out");
        let label = image_label(&image);
        let path = image.path.clone();
        // What this picture costs, weighed the one way the budget weighs a
        // picture: pixels when its header named them, bytes when it did not
        // ([`Image::weight`]). The images already in the box are added by hand
        // — they are not in the transcript yet, and two pictures that each fit
        // can still not fit together. Saturating, both ways: a header can claim
        // a picture larger than any `usize`, and the sum must not be the thing
        // that panics on the way to "it does not fit".
        let cost = image.weight();
        let image_bytes = image.bytes.len();
        let pending: usize = self
            .chat
            .attachments()
            .iter()
            .map(Image::weight)
            .fold(0, usize::saturating_add);
        let budget = self.cfg().history_budget();
        // The focused agent's room, not the root's: the picture is attached to
        // the conversation the human is looking at and will be sent to that
        // agent, and a child's own transcript is what its next request pays for
        // (`Chat::used_weight_for`).
        let room = budget.saturating_sub(self.chat.used_weight_for(target));
        if cost.saturating_add(pending) > budget {
            // Past the whole history budget, with the pictures already in the
            // box counted: the system prompt and the opening task are not
            // droppable, so no trim of the conversation can bring the request
            // inside the window, and attaching would spend a turn on the
            // refusal. Which road to name depends on what is over: the picture
            // alone, or the box's pictures together with it. The fold is named
            // only in the second case, and named for what it is: a fold of the
            // conversation, and the weight over the bound is the pictures'.
            let line = if cost > budget {
                format!(
                    "{label} is bigger than the whole history budget ({}) — even with every \
                     older turn dropped, a request carrying it would go out over the window, \
                     and the endpoint would refuse it. Downscale it (`convert {path} -resize 50% \
                     small.png`) and attach that",
                    size_label(budget)
                )
            } else {
                format!(
                    "{label} would put the box over the whole history budget ({}) — the \
                     pictures already in the box weigh {}, and `/compact` folds the conversation, \
                     not the pictures. Downscale one (`convert {path} -resize 50% small.png`) and \
                     attach that, or send the box's pictures first",
                    size_label(budget),
                    size_label(pending)
                )
            };
            self.fail(line);
            return false;
        }
        let held: usize = self
            .chat
            .attachments()
            .iter()
            .map(|image| image.bytes.len())
            .fold(0, usize::saturating_add);
        if image_bytes.saturating_add(held) > BOX_IMAGE_BYTES {
            // Past the box's own bound: the box is bytes while the pictures
            // wait, and the window's numbers cannot be this bound. Attaching
            // would hold more than the box allows, so it is refused.
            self.fail(format!(
                "{label} would put the box over the {} of picture bytes it may hold — {} is \
                 already waiting, and the window's token bound cannot bound bytes. Downscale it \
                 (`convert {path} -resize 50% small.png`) and attach that, or send the box's \
                 pictures first",
                size_label(BOX_IMAGE_BYTES),
                size_label(held)
            ));
            return false;
        }
        self.chat.attach(image);
        if cost.saturating_add(pending) > room {
            // Inside the budget, none left in the conversation: a trim makes
            // room by dropping the oldest turns at the next request. The line
            // says what attaching it costs, and names the two roads that spare
            // those turns.
            self.fail(format!(
                "{label} will not fit the room left for history ({}) — attaching it costs the \
                 oldest turns of the conversation, which are dropped at the next request to make \
                 room. `/compact` folds them into a summary instead, or downscale it (`convert \
                 {path} -resize 50% small.png`) and attach that",
                size_label(room)
            ));
            return true;
        }
        self.say(format!(
            "attached {label} — Enter sends it with the message"
        ));
        true
    }

    /// A signal — the terminal closing, a session manager's stop, an IDE's stop
    /// button — takes the quit road, at once and without the arming press.
    ///
    /// `Ctrl-Q` warns first when work is live because the human is at the
    /// keyboard and a second press is still theirs to make ([`Self::request_quit`]).
    /// A signal has no second press to offer: the process is the thing being
    /// signalled, and the road it must not take is the kernel's default, which
    /// kills mush before any `Drop` runs and leaves every process group and the
    /// attach socket behind (finding E1). What this costs is exactly what a
    /// *confirmed* `Ctrl-Q` costs — the in-flight turn dies, and the exit flush
    /// writes what the debounce had not — so the flag becomes `should_quit`
    /// directly. This is not `request_quit`: nothing here can be an accident of
    /// a stray keystroke, and a signal that is refused for want of a second
    /// press would not be a signal at all.
    pub fn signal_quit(&mut self) {
        self.should_quit = true;
    }

    /// `Ctrl-Q` (and `/quit`): leave — but never silently over live work.
    ///
    /// The exit kills the agents' process groups (finding H9): a model call in
    /// flight, the command an agent is parked on, every detached job. The human
    /// who quits is owed the names of what that costs, so the first press
    /// *arms* — the bar's line says what will die — and the next one does it.
    /// The second press is read off the line itself ([`Self::quit_armed`]), so
    /// an arm cannot outlive the warning that explains it. With nothing live
    /// there is nothing to warn about, and the ordinary quit stays one
    /// keystroke.
    fn request_quit(&mut self) {
        let kills = self.what_a_quit_kills();
        if kills.is_empty() || self.quit_armed() {
            self.should_quit = true;
            return;
        }
        self.arm_quit(&kills);
    }

    /// Whether a quit is waiting for its second key: the warning is the line
    /// the bar is showing, inside that line's own life ([`INFO_TTL`]).
    ///
    /// Derived from the line and not stored beside it, so nothing can leave an
    /// arm standing with no notice to show for it: whatever replaces the line —
    /// a job's report, an agent's failure, [`Self::disarm_quit`] — has
    /// disarmed the quit by being written, and a warning nobody can see any
    /// more is not a warning.
    fn quit_armed(&self) -> bool {
        matches!(self.status_line(), Some((_, StatusKind::Quit)))
    }

    /// Whether a new chat is waiting for its second key: the warning is the
    /// line the bar is showing, inside that line's own life ([`INFO_TTL`]).
    ///
    /// Derived from the line and not stored beside it, exactly as
    /// [`Self::quit_armed`]: whatever replaces the line — another key, a
    /// failure, [`Self::disarm_new_chat`] — has disarmed the key by being
    /// written.
    fn new_chat_armed(&self) -> bool {
        matches!(self.status_line(), Some((_, StatusKind::NewChat)))
    }

    /// Take an armed new chat back: the human did something other than press
    /// `Ctrl-N` again, and the warning belongs to the moment it was said.
    fn disarm_new_chat(&mut self) {
        if self.new_chat_armed() {
            self.status = None;
        }
    }

    /// Take an armed quit back: the human did something other than quit, and
    /// the warning belongs to the moment it was said, so it goes with it.
    fn disarm_quit(&mut self) {
        if self.quit_armed() {
            self.status = None;
        }
    }

    /// What a quit would kill, one item per thing that dies: `#0 thinking`,
    /// `#3 run_command + 1 job`.
    ///
    /// The set is [`Self::working_agents`] — the predicate `Ctrl-C` and the
    /// tree's `c` already ask, so the line cannot name an agent that has
    /// stopped or miss one that is running — plus, per agent, how many jobs go
    /// with it. The jobs' *commands* are deliberately not spelled out here: the
    /// agent's row and its cursor row's footer already name each one
    /// (`cargo build 1m20s`, finding U5), and the bar has a single row for the
    /// whole tree. The registry's own count closes the one gap the rows have: a
    /// job whose agent has no row any more runs on, is killed by the quit like
    /// any other, and is owned by a node that is gone.
    fn what_a_quit_kills(&self) -> Vec<String> {
        let mut items = Vec::new();
        let mut named_jobs = 0;
        for id in self.working_agents() {
            let Some(node) = self.tree.node(id) else {
                continue;
            };
            let jobs = self.tree.live_jobs(id).len();
            named_jobs += jobs;
            let jobs = match jobs {
                0 => String::new(),
                1 => " + 1 job".to_string(),
                count => format!(" + {count} jobs"),
            };
            items.push(format!("{id} {}{jobs}", node.phase.doing()));
        }
        let stray = self
            .tree
            .handles()
            .jobs
            .running()
            .saturating_sub(named_jobs);
        if stray > 0 {
            items.push(match stray {
                1 => "a job whose agent is gone".to_string(),
                count => format!("{count} jobs whose agents are gone"),
            });
        }
        items
    }

    /// Arm the quit: the bar's line becomes the warning for `kills`.
    ///
    /// A warning that is *still standing* keeps its clock, so recomposing it on
    /// a tick cannot extend the arming — and one whose five seconds are up does
    /// not: arming again shows a warning with a life of its own, rather than one
    /// that expires as it appears.
    fn arm_quit(&mut self, kills: &[String]) {
        let set_at = if self.quit_armed() {
            self.status.as_ref().map(|status| status.set_at)
        } else {
            None
        };
        // The one door a bar line goes through, so the text is sanitized once
        // by one rule.
        self.set_status(StatusKind::Quit, quit_warning(kills, QUIT_LINE_COLUMNS));
        if let (Some(at), Some(status)) = (set_at, self.status.as_mut()) {
            status.set_at = at;
        }
    }

    /// Arm the new chat: the bar's line becomes the warning for `lines`.
    ///
    /// [`Self::arm_quit`]'s shape, for the same reason: a warning that is still
    /// standing keeps its clock, so a tick that recomposes the count cannot
    /// extend the arming — and one whose five seconds are up arms again with a
    /// life of its own, rather than expiring as it appears (finding C4).
    fn arm_new_chat(&mut self, lines: usize) {
        let set_at = if self.new_chat_armed() {
            self.status.as_ref().map(|status| status.set_at)
        } else {
            None
        };
        self.set_status(StatusKind::NewChat, new_chat_warning(lines));
        if let (Some(at), Some(status)) = (set_at, self.status.as_mut()) {
            status.set_at = at;
        }
    }

    /// Keep an armed quit's warning true to the tree.
    ///
    /// The line is composed from the phases and the registry, so a tick
    /// re-composes it: work that ends while the human is deciding leaves the
    /// line, and a quit with nothing left to warn about stops being armed — the
    /// next `Ctrl-Q` is then an ordinary one, which is the truth, because there
    /// is nothing left to kill. The clock is *not* restarted, so the warning
    /// still fades with the moment the first key made it.
    fn refresh_quit_warning(&mut self) {
        if !self.quit_armed() {
            return;
        }
        let kills = self.what_a_quit_kills();
        if kills.is_empty() {
            self.status = None;
            self.dirty_screen = true;
            return;
        }
        let text = quit_warning(&kills, QUIT_LINE_COLUMNS);
        if self
            .status
            .as_ref()
            .is_some_and(|status| status.text != text)
        {
            self.arm_quit(&kills);
            self.dirty_screen = true;
        }
    }

    /// Keep an armed new chat's warning true to the conversation.
    ///
    /// The line counts the lines the second press would drop, and a turn that
    /// lands while the human is deciding changes that number; a conversation
    /// that empties — the last child's transcript reaped, say — has nothing
    /// left to warn about and ends the arm, so the next `Ctrl-N` is an ordinary
    /// one. [`Self::refresh_quit_warning`]'s shape, clock included (finding
    /// C4).
    fn refresh_new_chat_warning(&mut self) {
        if !self.new_chat_armed() {
            return;
        }
        let lines = self.chat.lines_to_drop();
        if lines == 0 {
            self.status = None;
            self.dirty_screen = true;
            return;
        }
        let text = new_chat_warning(lines);
        if self
            .status
            .as_ref()
            .is_some_and(|status| status.text != text)
        {
            self.arm_new_chat(lines);
            self.dirty_screen = true;
        }
    }

    /// Ask one agent's current run to stop: flip the flag its in-flight model
    /// call polls, and leave a Stop in the mailbox for everything else (a
    /// parked wait, a shell command, the next message boundary).
    /// Stop the agent the human is looking at. Ctrl-C used to stop *every*
    /// busy agent at once, which is the wrong default: the agents it killed
    /// were usually the ones already finished and about to report, and their
    /// work was lost with them. Stopping one agent is what the key should do;
    /// stopping the whole tree is `interrupt_all`.
    fn interrupt(&mut self) {
        // What the human is looking at: the focused agent if it is working, else
        // the one agent that is — and "working" includes a detached job, which
        // is work in flight even while its owner naps.
        let busy = self.working_agents();
        let target = if busy.contains(&self.tree.focused) {
            Some(self.tree.focused)
        } else if busy.len() == 1 {
            Some(busy[0])
        } else {
            None
        };
        let Some(id) = target else {
            if busy.is_empty() {
                self.say(NOTHING_RUNNING);
            } else {
                // Several agents are busy and the focused one is not among
                // them: stopping the wrong one silently would be worse than
                // asking, so name the scope instead.
                self.say(format!(
                    "{} agents running · Enter picks one to stop · Ctrl-X stops them all",
                    busy.len()
                ));
            }
            return;
        };
        self.stop_one(id);
    }

    /// Stop every busy agent. The old Ctrl-C, now on its own key: it is the
    /// emergency brake, not the everyday one.
    fn interrupt_all(&mut self) {
        let targets = self.working_agents();
        if targets.is_empty() {
            self.say(NOTHING_RUNNING);
            return;
        }
        let ids = targets.clone();
        for id in targets {
            self.stop_one(id);
        }
        let list = ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        self.say(format!("stopped {} agents ({list})", ids.len()));
    }

    /// The agents with work in flight: a run, or a job. `Stop` is aimed at the
    /// work, not at the phase, so both count ([`Self::in_flight`]).
    fn working_agents(&self) -> Vec<AgentId> {
        self.tree
            .agents
            .iter()
            .filter(|node| self.in_flight(node))
            .map(|node| node.id)
            .collect()
    }

    /// Ask one agent to stop and show it immediately. A dead mailbox is a gone
    /// actor: mark the row so it stops showing work that can never finish
    /// (finding B6) — and when it was mid-run, say what that *is*, because
    /// "stopped" claims an actor that a message resumes and there is none
    /// (finding H2).
    fn stop_one(&mut self, id: AgentId) {
        // `true` is the one case that is not a stop: the mailbox was dead with a
        // run in flight, so the row and the parent must hear it.
        if self.tree.cancel_requested(id) {
            self.report_cut_off(id);
        }
    }

    /// An agent's actor is gone with its run in flight: the one ending no actor
    /// can report, because the actor that would have reported it is the thing
    /// that vanished.
    ///
    /// So the UI — the only observer left, and the one that just proved it by
    /// finding the mailbox dead — says it in both places the fact has a reader.
    /// The agent's own pane gets the `⚠` line under its `⚠` row. Its parent is
    /// told through the very message a completion travels in (`ChildDone`),
    /// which means the parent's own actor folds it into its transcript, wakes a
    /// napping parent for it, and reports it to the model in the same words
    /// every other ending uses — one owner of the line, one road into the
    /// conversation (finding H2).
    ///
    /// A parent whose node is gone too, and the root — which is nobody's child
    /// and never revived — have nobody to tell; the row and the pane still say
    /// it.
    fn report_cut_off(&mut self, id: AgentId) {
        self.chat.note_cut_off_for(id, cut_off_notice());
        self.mark_session_dirty();
        // The actor that owned this agent's jobs is the thing that vanished, so
        // nothing else will ever stop them: `Registry::stop` refuses any caller
        // but the owner, and `kill_owned` is otherwise reached only from inside
        // the owning actor's own `Stop`/`Shutdown` handlers (`agent.rs`). Left
        // alone, a build the agent started would run until mush quits, owned by
        // a row that says the work was cut off. The UI is the only observer
        // left, so the kill is the UI's to make — and it is what the row, the
        // notice and the parent's message are describing.
        self.tree.handles().jobs.kill_owned(id.0);
        if id == self.tree.focused {
            self.say(format!(
                "agent {id} was already gone — its run was cut off, nothing committed"
            ));
        }
        let Some(parent) = self.tree.node(id).and_then(|node| node.parent) else {
            return;
        };
        // Through the door `AgentEvent::ChildAsleep` uses, not a raw send: the
        // parent may be *parked* — a node with a mailbox and no thread behind
        // it (`Self::park_history`) — and a send into that mailbox drops the
        // one report of a run that never ended. `deliver_to_actor` wakes a
        // parked parent and hands it the completion, which is what this
        // message's whole reason for existing promises: it wakes a napping
        // parent.
        let _ = self.deliver_to_actor(
            parent,
            AgentMsg::ChildDone {
                id: id.0,
                run: agent::CUT_OFF_RUN,
                outcome: agent::Outcome::CutOff,
            },
        );
    }

    fn cycle_focus(&mut self, direction: i64) {
        let order = [Focus::Agents, Focus::Chat];
        let index = match self.focus {
            Focus::Agents => 0,
            Focus::Chat => 1,
        };
        let next = (index as i64 + direction).rem_euclid(order.len() as i64) as usize;
        self.focus = order[next];
        // Looking somewhere new is a moment the human reads the bar, and the
        // git line has no other reason to move: refresh on the transition, or
        // a file written outside mush stays invisible on a screen nobody has
        // touched (finding P8). The read is cheap and `git_in_flight` drops a
        // second ask while the first is out.
        self.refresh_git();
    }

    /// Move the tree's cursor `step` rows — one for `j`/`k`, a whole page for
    /// `PgUp`/`PgDn`, the pair `PickerMove` carries.
    ///
    /// `move_cursor` is the tree's only cursor setter and it moves a *sign*,
    /// one row per call, stopping at either end of the list — so the step is
    /// taken one row at a time, and a page from the first or the last row is an
    /// honest no-op rather than a wrap or an index off the end. Rows are the
    /// painted ones: `rows()` paints every agent exactly once, so the storage
    /// length `move_cursor` clamps against is the length the pane draws, and
    /// the cursor it lands on is a row the human can see.
    fn move_tree_cursor(&mut self, step: i64) {
        for _ in 0..step.abs() {
            self.tree.move_cursor(step.signum());
        }
    }

    /// `←`/`→` in the agents pane: walk the painted rows along the parent links
    /// (finding U10). `direction < 0` selects the selected agent's parent;
    /// `> 0` its first child. Both read the order the pane paints and `j`/`k`
    /// walk, so the cursor lands on the row the human sees (finding U4), and
    /// the parent link — not the row above — is what `←` follows: a later
    /// sibling's row sits directly above a node without being its parent.
    ///
    /// The root has no parent and a leaf has no child, so the cursor stays put:
    /// an honest no-op, chosen over jumping to `g` (the root) or to a sibling,
    /// either of which would move the selection somewhere the human did not
    /// point.
    fn tree_walk(&mut self, direction: i64) {
        let Some(from) = self.tree.cursor_id() else {
            return;
        };
        let target = if direction < 0 {
            self.tree
                .node(from)
                .and_then(|node| node.parent)
                .filter(|parent| self.tree.has(*parent))
        } else {
            // The first child in painted order: `rows` is pre-order, so the
            // first node whose parent is `from` is the one directly under it.
            self.tree
                .rows()
                .iter()
                .find(|node| node.parent == Some(from))
                .map(|node| node.id)
        };
        let Some(target) = target else {
            return;
        };
        if let Some(index) = self.tree.rows().iter().position(|node| node.id == target) {
            // The tree's only cursor setter is `move_cursor`, and it moves a
            // *sign*, one row per call — so the walk takes one step for each row
            // it must cross rather than a second way to write the cursor. The
            // cursor lands on `target` exactly, because `steps` is the gap.
            let steps = index as i64 - self.tree.cursor() as i64;
            for _ in 0..steps.abs() {
                self.tree.move_cursor(steps.signum());
            }
        }
    }

    /// Focus the row the tree's cursor is on, and say whose pane the chat now
    /// shows: `Enter` in the agent pane is a move of the *view*, so the brief
    /// goes to the bar where a human can read it before typing.
    ///
    /// The pane follows the row; the keyboard stays in the tree, because the
    /// human is walking the rows and the next `j` must still be a row move —
    /// `Tab` is what puts the keys in the box. The bar's `agents` badge says
    /// where they are, so the split is not a silent one.
    fn focus_cursor_row(&mut self) {
        if let Some(id) = self.tree.focus_cursor() {
            let brief = self
                .tree
                .node(id)
                .map(|node| node.brief.clone())
                .unwrap_or_default();
            // The head is kept whole — it is what says whose brief this is —
            // and the brief takes what is left of the row.
            let head = format!("agent {id}: ");
            let room = CURSOR_LINE_COLUMNS.saturating_sub(head.chars().count());
            let brief = mush_core::text::truncate(&brief, room);
            self.say(format!("{head}{brief}"));
            // A focus change is a read-the-bar moment; refresh the git line so
            // it is current when the human looks (finding P8).
            self.refresh_git();
        }
    }

    /// `c` on the tree's cursor row: stop it if it has work to stop.
    ///
    /// Stopping an idle agent is not a no-op to be swallowed — the human asked
    /// for something that cannot happen, and the row's phase is left alone
    /// because it has no work in flight to cancel. Ending an agent is Ctrl-N's
    /// job.
    fn cancel_cursor_row(&mut self) {
        // The painted row under the cursor, not the storage vector: they are
        // different orders (finding U4).
        let Some(id) = self.tree.cursor_id() else {
            return;
        };
        // The same question `working_agents` answers: a run *or* a job is work
        // in flight. An agent whose run ended while its `cargo bench` still runs
        // is not idle on the machine, and the key is aimed at the work — so
        // answering "not running" here, while Ctrl-C stops the very same job,
        // made the two paths disagree about the same agent.
        let working = self.tree.node(id).is_some_and(|node| self.in_flight(node));
        if !working {
            self.say(format!("agent {id} is not running"));
            return;
        }
        // The row's own `⊘` is the feedback; the bar shows what the tree as a
        // whole is doing.
        self.stop_one(id);
    }

    /// Take the picker's selected row: the picker is gone either way, because
    /// `Enter` is a decision even when there is nothing to decide.
    fn pick_cursor(&mut self) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let kind = picker.kind;
        let item = picker.items.get(picker.cursor).cloned();
        self.picker = None;
        if let Some(item) = item {
            self.pick(kind, &item);
        }
    }

    /// Move the picker's cursor, stopping at both ends: a picker is not a
    /// wheel, and wrapping from the last model to the first hides how many
    /// there are.
    fn move_picker(&mut self, step: i64) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        let last = picker.items.len().saturating_sub(1) as i64;
        picker.cursor = (picker.cursor as i64 + step).clamp(0, last) as usize;
    }

    /// Put the picker's cursor on a row, clamped to the list — `usize::MAX` is
    /// the end of it.
    fn set_picker_cursor(&mut self, row: usize) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        picker.cursor = row.min(picker.items.len().saturating_sub(1));
    }
}

impl Drop for App {
    /// The exit flush: whatever the debounce had not written yet goes out here,
    /// so ending mush — a clean quit, and every signal that takes this road
    /// (`main::take_signal_quit`) — costs nothing. This is what bounds a crash
    /// to `SESSION_DEBOUNCE` of streamed chat rather than to everything since
    /// the last boundary. A failure here is reported the usual way and then lost
    /// with the status line: there is no screen left to read it on.
    fn drop(&mut self) {
        if self.session_dirty_at.is_some() {
            self.flush_session();
        }
        // Jobs die with mush itself. Each one is a process group, and a build an
        // agent started used to outlive a clean quit — the human's next `cargo
        // build` then fought a ghost for the target directory. Killing here,
        // on the way out of the process, is the last moment it can happen.
        self.tree.handles().jobs.kill_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach;
    use crate::ids::JobId;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    use crossbeam_channel::Receiver;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::layout::Rect;
    use ratatui::widgets::{Block, Borders};
    use ratatui::Terminal;

    use crate::session_save;
    use crate::session_save::SessionSave;

    use crate::model::fake::{tool_call, Asked, Gate, Scripted};

    /// Run a typed line the way a send does: through the pure parser, then the
    /// arms. A test that names a command string this way exercises both, so the
    /// parser cannot be right about an argument the executor reads differently
    /// (finding B2).
    fn run(app: &mut App, line: &str) {
        let command = commands::parse_command(line)
            .unwrap_or_else(|error| panic!("`{line}` is not a command: {error:?}"));
        app.apply_command(command);
    }

    /// The transient line the bar would show, or the empty string.
    fn text_of(app: &App) -> &str {
        app.status_line().map(|(text, _)| text).unwrap_or("")
    }

    /// What a picker's rows say, for a test that reads the list as the human
    /// does. The ids behind the labels are what `pick` reads, and a test that
    /// asserts on the *pick* compares those instead.
    fn labels(items: &[PickerItem]) -> Vec<&str> {
        items.iter().map(|item| item.label.as_str()).collect()
    }

    /// Pretend a status line was written `seconds` ago.
    fn age_status(app: &mut App, seconds: u64) {
        if let Some(status) = app.status.as_mut() {
            status.set_at = Instant::now() - Duration::from_secs(seconds);
        }
    }

    /// A `Ctrl-` key as the terminal delivers it: one press, through the key
    /// table and the arms, exactly as `main` routes it.
    fn ctrl(app: &mut App, key: char) {
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char(key),
            KeyModifiers::CONTROL,
        )));
    }

    /// Start a new chat the way the key does now: one press over an empty
    /// conversation, two over one with something to lose (finding C4). The
    /// arming is pinned where it belongs —
    /// `a_new_chat_keeps_the_old_conversation_and_arms_the_key` — so the tests
    /// that only need the clear say so with this rather than repeating the two
    /// presses.
    fn new_chat(app: &mut App) {
        ctrl(app, 'n');
        if app.new_chat_armed() {
            ctrl(app, 'n');
        }
    }

    /// A press of the pane cycle, through the key table and the arms: the key
    /// that decides which pane is focused — and so, under zen, which pane is
    /// full-screen.
    fn tab(app: &mut App) {
        app.update(Msg::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    }

    /// A real `App` on a scratch directory, with a real (idle) root actor. The
    /// returned receiver keeps the UI channel alive for the life of the test.
    /// A real repository, because worktree discovery shells out to git — a fake
    /// would test nothing it actually does.
    fn repo(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mush-land-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["-c", "init.defaultBranch=master", "init", "-q"]);
        git(&dir, &["config", "user.email", "mush@test"]);
        git(&dir, &["config", "user.name", "mush"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "init"]);
        dir
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The fixture every `App` test starts from: a real workspace at `root`, an
    /// endpoint that answers nothing (`127.0.0.1:1`), and the session store the
    /// caller chose. One construction, so the twelve call sites cannot drift
    /// from what `App::new` takes.
    fn app_root(
        root: &std::path::Path,
        stored: Option<Session>,
        save: Arc<dyn SessionSave>,
    ) -> (App, Receiver<Msg>) {
        app_root_at(root, stored, save, "http://127.0.0.1:1")
    }

    /// The same fixture with the endpoint named, for the one thing an actor's
    /// *books* cannot be read without: they live in the actor and come out
    /// through a tool result, so a test that wants to see them runs a loopback
    /// endpoint of its own ([`status_endpoint`]) and hands its URL over here.
    fn app_root_at(
        root: &std::path::Path,
        stored: Option<Session>,
        save: Arc<dyn SessionSave>,
        base_url: &str,
    ) -> (App, Receiver<Msg>) {
        let ws = Workspace::new(root).unwrap();
        let cfg = Config::new(base_url, "test-model", None);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let cell = ConfigCell::own(cfg);
        let handle = spawn(cell.handle(), tx.clone(), root.to_path_buf());
        let app = App::new(ws, cell, stored, handle, tx, save);
        (app, rx)
    }

    fn app_at(root: std::path::PathBuf) -> App {
        app_root(&root, None, session_save::fake::Recorder::new()).0
    }

    /// An `App` whose root actor answers from `scripted` instead of the network,
    /// with the channel its events would travel on handed back.
    ///
    /// [`app_root`]'s root calls the real endpoint, which is what almost every
    /// test wants: the root's *books* are the exception. They live in its actor
    /// and only `status` and `control` read them, so the only way to see what
    /// they say is to drive a run through that actor and read what the model was
    /// handed (finding H25). The mailbox is the test's to reach through
    /// `tree.agent_tx`, which is where a run is started from.
    fn app_with_scripted_root(
        root: &std::path::Path,
        stored: Option<Session>,
        scripted: Arc<Scripted>,
    ) -> (App, Receiver<Msg>) {
        app_with_scripted_root_at(root, stored, scripted, "http://127.0.0.1:1")
    }

    /// The same, with the cell's endpoint named.
    ///
    /// The root answers from the script whatever this says; it matters because
    /// every *revived* actor is built with the cell's own client (`agent::revive`
    /// takes `cfg` from the app), so a test that wants a restored child's run to
    /// finish points this at a loopback endpoint (§8.39).
    fn app_with_scripted_root_at(
        root: &std::path::Path,
        stored: Option<Session>,
        scripted: Arc<Scripted>,
        base_url: &str,
    ) -> (App, Receiver<Msg>) {
        let ws = Workspace::new(root).unwrap();
        let cfg = Config::new(base_url, "test-model", None);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let handle = agent::spawn_scripted(
            cfg.clone(),
            crate::events::fake::Recorder::new(),
            root.to_path_buf(),
            scripted,
        );
        let app = App::new(
            ws,
            ConfigCell::own(cfg),
            stored,
            handle,
            tx,
            session_save::fake::Recorder::new(),
        );
        (app, rx)
    }

    /// The same, with the actor's events travelling the UI's own channel
    /// instead of a recorder: how a test reads a *real* run as the window does
    /// — through `App::update`, one event at a time.
    ///
    /// [`app_with_scripted_root_at`] keeps them out of the app's way on purpose:
    /// its tests read what the model was asked, which the recorder holds. A test
    /// about what the screen says needs the road the events take to it.
    fn app_with_live_scripted_root(
        root: &std::path::Path,
        scripted: Arc<Scripted>,
    ) -> (App, Receiver<Msg>) {
        let ws = Workspace::new(root).unwrap();
        let cfg = Config::new("http://127.0.0.1:1", "test-model", None);
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let handle =
            agent::spawn_scripted_ui(cfg.clone(), tx.clone(), root.to_path_buf(), scripted);
        let app = App::new(
            ws,
            ConfigCell::own(cfg),
            None,
            handle,
            tx,
            session_save::fake::Recorder::new(),
        );
        (app, rx)
    }

    /// An `App` with the UI channel kept, so a read that finishes on its own
    /// thread (`Msg::Git`) can be waited for instead of raced.
    fn app_and_rx(root: std::path::PathBuf) -> (App, Receiver<Msg>) {
        app_root(&root, None, session_save::fake::Recorder::new())
    }

    /// Adopt the next `Msg::Git` that arrives within the deadline, applying any
    /// message in front of it. A read that never comes back is a test failure,
    /// so this panics rather than proceeding on a stale snapshot.
    fn wait_git(app: &mut App, rx: &Receiver<Msg>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(msg) => {
                    let is_git = matches!(msg, Msg::Git { .. });
                    app.update(msg);
                    if is_git {
                        return;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        panic!("no git read came back");
    }

    /// A finished child of the root, the state both windows are about: inserted
    /// with a live mailbox, its run over, and its result read by its parent
    /// (which is what takes the `✉` off).
    ///
    /// The receive half comes back because it is the difference between the two
    /// things a mailbox can be: a receiver that is *there* is a live actor, and
    /// a parked one is exactly a mailbox with no receiver behind it — which is
    /// what the tests that want one drop.
    fn finished_child(app: &mut App, id: u64) -> Receiver<AgentMsg> {
        finished_child_of(app, id, AgentId::ROOT)
    }

    /// The same child, under a parent of its own: a reap's forget goes to
    /// whoever the *row* names, not to the root, and a test that wants to read
    /// the message has to hold that parent's mailbox (finding H19).
    fn finished_child_of(app: &mut App, id: u64, parent: AgentId) -> Receiver<AgentMsg> {
        let (tx, rx) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(id),
            parent,
            brief: format!("task {id}"),
            depth: 1,
            branch: None,
            fork: None,
            cmd: tx,
        });
        app.tree.finish(AgentId(id), Some(format!("did {id}")));
        app.tree.result_read(AgentId(id));
        app.chat
            .push_message(AgentId(id), Message::user(format!("task {id}")));
        rx
    }

    /// Wait for `id` to report that a run began, applying whatever else arrives.
    /// The actor's own word is the only evidence that words woken with started
    /// work, so this is what "the child runs" is asserted on.
    fn runs(app: &mut App, rx: &Receiver<Msg>, id: AgentId) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(msg) => {
                    let began = matches!(
                        &msg,
                        Msg::Agent { id: who, event: AgentEvent::Running { .. }, .. } if *who == id
                    );
                    app.update(msg);
                    if began {
                        return true;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        false
    }

    /// A run that spawned fifty-one children keeps fifty of them: the history
    /// window drops the oldest row, its transcript with it, and the file the
    /// next save writes carries what is left — a cap on the thing that had none
    /// (finding H16, §8.21).
    #[test]
    fn the_history_window_forgets_the_oldest_child_and_keeps_fifty() {
        let (mut app, _rx) = test_app("history-window");
        // The receivers are kept: the child the window forgets is told to end
        // its actor, and a mailbox with no receiver behind it could not show it.
        let mailboxes: Vec<Receiver<AgentMsg>> =
            (1..=51).map(|id| finished_child(&mut app, id)).collect();

        // One tick — the frame every one of these happens on.
        app.tick();

        assert!(
            matches!(mailboxes[0].try_recv(), Ok(AgentMsg::Shutdown)),
            "the thread is told before the node goes"
        );
        assert!(!app.tree.has(AgentId(1)), "the oldest row is gone");
        assert!(
            app.chat.transcript(AgentId(1)).is_empty(),
            "and its transcript with it — no archive (§8.21)"
        );
        assert!(
            app.tree.has(AgentId(2)),
            "the next-oldest child is the newest of the window"
        );
        let stored = app.session_snapshot();
        assert_eq!(
            stored.agents.len(),
            50,
            "the save carries a window, not everything a run ever said"
        );
        let ids: Vec<u64> = stored.agents.iter().map(|agent| agent.id).collect();
        assert_eq!(
            ids,
            (2..=51).collect::<Vec<u64>>(),
            "reaping from the oldest end leaves the surviving numbers contiguous"
        );
    }

    /// The row goes and so does the name: a child the history window reaps is
    /// dropped from *its parent's* books, or `status` lists a child whose row is
    /// off the screen and `control` aims at a ghost (finding H19).
    ///
    /// The books live in the actor, so the tree can only ask — this is that
    /// message, read off a parent whose mailbox the test holds. The child below
    /// it is #2, the oldest of fifty-one: the same overflow the window's own
    /// test walks.
    #[test]
    fn a_reaped_child_is_dropped_from_its_parents_books() {
        let (mut app, _rx) = test_app("reap-forgets");
        let (parent_tx, parent_rx) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "a parent".into(),
            depth: 1,
            // Isolated, and its work never landed: the window keeps the row,
            // which is what leaves the mailbox alive to hear about its children.
            branch: Some("mush/1".into()),
            // Hand-made: no worktree was ever created for it, so there is no
            // fork revision to store (the branch above is a name in the books,
            // not a checkout the sweep found).
            fork: None,
            cmd: parent_tx,
        });
        let mailboxes: Vec<Receiver<AgentMsg>> = (2..=52)
            .map(|id| finished_child_of(&mut app, id, AgentId(1)))
            .collect();

        app.tick();

        assert!(!app.tree.has(AgentId(2)), "the oldest child is reaped");
        let told: Vec<AgentMsg> = parent_rx.try_iter().collect();
        assert!(
            told.iter()
                .any(|msg| matches!(msg, AgentMsg::ForgetChild { id: 2 })),
            "its parent is told which name to drop: {told:?}"
        );
        assert!(
            !told
                .iter()
                .any(|msg| matches!(msg, AgentMsg::ForgetChild { id } if *id != 2)),
            "and only the child that went: {told:?}"
        );
        drop(mailboxes);
    }

    /// A thread is the resource, not a node: the newest children keep theirs,
    /// and the rest are parked — ending the actor and nothing else, so the row
    /// and the transcript are exactly where they were (finding H16, §8.21).
    #[test]
    fn the_parking_window_keeps_the_newest_children_warm() {
        let (mut app, _rx) = test_app("parking-window");
        let (parent_tx, parent_rx) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.agent_tx.insert(AgentId::ROOT, parent_tx);
        let mailboxes: Vec<Receiver<AgentMsg>> =
            (1..=10).map(|id| finished_child(&mut app, id)).collect();

        app.tick();

        assert!(
            matches!(mailboxes[0].try_recv(), Ok(AgentMsg::Shutdown))
                && matches!(mailboxes[1].try_recv(), Ok(AgentMsg::Shutdown)),
            "the two oldest actors are told to end"
        );
        assert!(
            mailboxes[9].try_recv().is_err(),
            "the newest child's thread is untouched"
        );
        assert!(
            app.tree.agent_tx[&AgentId(3)]
                .send(AgentMsg::Stop(agent::Stop::Human))
                .is_ok(),
            "and so is the third-newest: the window is the newest eight"
        );
        // The parent's books need the fact the child's actor can no longer
        // report: its run numbering restarts with the next actor (B24).
        assert!(
            matches!(parent_rx.try_recv(), Ok(AgentMsg::ChildParked { id }) if id == 1),
            "the parent is told which child's thread went away"
        );
        // Parking is invisible to the screen: the node and the transcript stay.
        assert!(app.tree.has(AgentId(1)));
        assert_eq!(app.chat.transcript(AgentId(1)).len(), 1);
        // And a second frame re-decides the same set rather than re-parking.
        app.tick();
        assert!(app.tree.has(AgentId(1)));
    }

    /// A parked child is woken by a message: the words rebuild its actor from
    /// the transcript on screen — the same door a restart uses — and the child
    /// runs (finding H16, §8.21).
    #[test]
    fn a_parked_child_is_woken_by_a_message_and_runs() {
        let root = repo("parked-wake");
        let (mut app, rx) = app_and_rx(root.clone());
        // A finished child whose actor is parked: the tree holds the mailbox and
        // nobody holds the receiver, which is what a reclaimed thread leaves.
        let (tx, parked) = crossbeam_channel::unbounded::<AgentMsg>();
        drop(parked);
        let opened = app.tree.insert(Spawn {
            id: AgentId(2),
            parent: AgentId::ROOT,
            brief: "port the parser".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd: tx,
        });
        app.chat.push_message(opened.id, opened.opening);
        app.tree.finish(AgentId(2), Some("did it".to_string()));
        app.tree.result_read(AgentId(2));
        app.tree.focus(AgentId(2));
        app.chat.insert("carry on");

        app.send_message();

        assert!(app.tree.has(AgentId(2)), "the node is still there");
        assert_eq!(
            app.chat.transcript(AgentId(2)).len(),
            2,
            "and the transcript it resumes from is the one on screen"
        );
        assert!(
            app.tree.agent_tx[&AgentId(2)]
                .send(AgentMsg::Stop(agent::Stop::Human))
                .is_ok(),
            "the mailbox has an actor behind it again"
        );
        assert!(
            runs(&mut app, &rx, AgentId(2)),
            "and the woken child began a run"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other hand that can wake a parked child is not the human's: a
    /// parent's own `control message` finds the same empty mailbox and reaches
    /// the same door. The parent holds no transcript and cannot revive, so the
    /// command travels to the UI as an event — and the words land in the child's
    /// transcript, where the human can read what their model was told
    /// (finding H18).
    #[test]
    fn a_parents_message_wakes_a_parked_child_through_the_ui() {
        let root = repo("parent-wake");
        let (mut app, rx) = app_and_rx(root.clone());
        // A parked child: the tree holds the mailbox and nobody holds the
        // receiver, which is what a reclaimed thread leaves.
        let (tx, parked) = crossbeam_channel::unbounded::<AgentMsg>();
        drop(parked);
        let opened = app.tree.insert(Spawn {
            id: AgentId(2),
            parent: AgentId::ROOT,
            brief: "port the parser".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd: tx,
        });
        app.chat.push_message(opened.id, opened.opening);
        app.tree.finish(AgentId(2), Some("did it".to_string()));
        app.tree.result_read(AgentId(2));

        // What the parent's actor emits when its send finds no actor there.
        app.update(Msg::Agent {
            conversation: app.tree.conversation(),
            id: AgentId::ROOT,
            event: AgentEvent::ChildAsleep {
                child: 2,
                command: AgentMsg::Steer("carry on".into()),
            },
        });

        assert!(
            app.tree.agent_tx[&AgentId(2)]
                .send(AgentMsg::Stop(agent::Stop::Human))
                .is_ok(),
            "the mailbox has an actor behind it again"
        );
        assert!(
            runs(&mut app, &rx, AgentId(2)),
            "and the words the parent sent start the child"
        );
        assert!(
            app.chat
                .transcript(AgentId(2))
                .iter()
                .any(|message| message.text() == "carry on"),
            "the human reads the words their model was told"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A parent's `control message` can land in a child the tree still reads as
    /// at rest: the send succeeds the moment the tool call returns, and the
    /// child's own `Running` event — the only thing that would mark the row —
    /// is a moment behind. A `tick` in that window used to run `park_history`,
    /// whose `Shutdown` cancels the run the words just started, and the child
    /// then reported `Stopped` about a run nobody stopped (§8.21).
    #[test]
    fn a_control_message_keeps_a_child_out_of_the_parking_tick() {
        let root = repo("control-park-window");
        // The parent's one turn: message the child nothing has marked busy.
        // A *real* root, so its events land on the UI channel this test reads —
        // a scripted root records them instead.
        let call = serde_json::json!({
            "id": "#2",
            "action": "message",
            "text": "carry on"
        });
        let first = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "c0",
                        "type": "function",
                        "function": { "name": "control", "arguments": call.to_string() }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
        .to_string();
        let then = r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#
            .to_string();
        let port = two_reply_endpoint(first, then);
        let (mut app, rx) = app_root_at(
            &root,
            None,
            session_save::fake::Recorder::new(),
            &format!("http://127.0.0.1:{port}"),
        );
        // Ten finished children of the root: the newest keep their threads
        // warm, so #2 is exactly what the parking pass may reclaim.
        let mailboxes: Vec<Receiver<AgentMsg>> =
            (1..=10).map(|id| finished_child(&mut app, id)).collect();
        // The rows are on screen; the books a `control` reads are the actor's,
        // so the parent is handed them (`App::seed_children`).
        app.seed_children();
        assert!(
            app.tree.parkable().contains(&AgentId(2)),
            "the child this test messages is park-eligible to start with"
        );

        // The parent runs and messages #2, whose mailbox the test holds. There
        // is no child actor to emit `Running` — which is exactly the moment the
        // race lives in — so the test does not wait for one.
        app.tree.agent_tx[&AgentId::ROOT]
            .send(AgentMsg::Nudge("go".into()))
            .unwrap();
        let words = mailboxes[1].recv_timeout(Duration::from_secs(5));
        assert!(
            matches!(words, Ok(AgentMsg::Steer(ref text)) if text == "carry on"),
            "the parent's message landed in the child's live mailbox: {words:?}"
        );
        // Everything the UI has heard before the frame: the parent's run, and
        // the mark the send travels with.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut done = false;
        while !done && Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(msg) => {
                    done = matches!(
                        &msg,
                        Msg::Agent {
                            id,
                            event: AgentEvent::Done,
                            ..
                        } if *id == AgentId::ROOT
                    );
                    app.update(msg);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(done, "the parent's run never came back");

        // One frame, the one `park_history` runs on.
        app.tick();

        assert!(
            !matches!(mailboxes[1].try_recv(), Ok(AgentMsg::Shutdown)),
            "the parking tick must not Shutdown the run the message started"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other direction of the same road: a report finds no actor behind the
    /// mailbox an agent holds for its *parent*, and the UI delivers it to the
    /// parent the tree names (§8.39).
    ///
    /// The shape is production's. A parent's thread is reclaimed or replaced
    /// (`App::park_history`, `App::deliver_to_actor`) while its child's actor
    /// keeps the sender it was born with — a restored child held a dead one for
    /// its whole life — so the completion, the one report that settles the
    /// parent's books, used to land nowhere: the books kept a running child
    /// under a `✓` row, a `wait` burned its whole cap, and the guard refused the
    /// next shared spawn. A missing actor is not a missing parent, so the UI —
    /// which holds the tree's rows and the transcript a new actor is built from
    /// — delivers it through the door a human's own message uses.
    #[test]
    fn a_childs_report_reaches_the_parent_the_tree_names() {
        let root = repo("parent-asleep");
        let (mut app, rx) = app_and_rx(root.clone());
        // A parent whose actor is parked: the tree holds the mailbox and nobody
        // holds the receiver.
        let (parent_tx, parked) = crossbeam_channel::unbounded::<AgentMsg>();
        drop(parked);
        let opened = app.tree.insert(Spawn {
            id: AgentId(2),
            parent: AgentId::ROOT,
            brief: "a parent".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd: parent_tx,
        });
        app.chat.push_message(opened.id, opened.opening);
        app.tree.finish(AgentId(2), Some("did it".to_string()));
        app.tree.result_read(AgentId(2));
        // Its own child, whose report could not be handed over.
        let (child_tx, _child_rx) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(3),
            parent: AgentId(2),
            brief: "the task".to_string(),
            depth: 2,
            branch: None,
            fork: None,
            cmd: child_tx,
        });

        // What the child's actor emits when its send finds no actor there.
        app.update(Msg::Agent {
            conversation: app.tree.conversation(),
            id: AgentId(3),
            event: AgentEvent::ParentAsleep {
                command: AgentMsg::ChildDone {
                    id: 3,
                    run: 1,
                    outcome: agent::Outcome::Finished("ported it".into()),
                },
            },
        });

        assert!(
            app.tree.agent_tx[&AgentId(2)]
                .send(AgentMsg::Stop(agent::Stop::Human))
                .is_ok(),
            "the parent's mailbox has an actor behind it again"
        );
        assert!(
            runs(&mut app, &rx, AgentId(2)),
            "and the completion the child could not deliver starts the parent's run"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other door into the same window: the parent's `control` found the
    /// child *parked*, so the UI is the hand that rebuilds its actor
    /// (`ChildAsleep`) — and the revived actor's `Running` is again a moment
    /// behind. One tick in between must not park the thread the words just
    /// woke (§8.21).
    #[test]
    fn a_woken_child_is_not_parked_before_its_own_running_lands() {
        let (mut app, rx) = test_app("child-wake-park-window");
        // Ten finished children of the root, so #2 is past `WARM_CHILDREN` and
        // the parking pass may reclaim it.
        let mut mailboxes: Vec<Receiver<AgentMsg>> =
            (1..=10).map(|id| finished_child(&mut app, id)).collect();
        // #2 parked: the tree holds the mailbox and nobody holds the receiver,
        // which is what reclaiming a finished child's thread leaves.
        drop(mailboxes.remove(1));
        assert!(
            app.tree.parkable().contains(&AgentId(2)),
            "the child the parent woke is park-eligible to start with"
        );

        // What the parent's actor emits when its send finds no actor there,
        // and the UI revives the child to take the words.
        app.update(Msg::Agent {
            conversation: app.tree.conversation(),
            id: AgentId::ROOT,
            event: AgentEvent::ChildAsleep {
                child: 2,
                command: AgentMsg::Steer("carry on".into()),
            },
        });

        // One frame, the one `park_history` runs on — taken before the woken
        // actor's `Running` has been applied, which is the whole race.
        app.tick();

        assert!(
            runs(&mut app, &rx, AgentId(2)),
            "the woken child's run begins instead of being Shutdown"
        );
        let after = events_from(&rx, AgentId(2), Duration::from_millis(500));
        assert!(
            !after.iter().any(|event| event.starts_with("Stopped")),
            "and is not cancelled a moment later: {after:?}"
        );
    }

    /// The root is the one agent this road must not carry: it has no parent row,
    /// so its own reports are dropped rather than filed back into the mailbox
    /// that emitted them — a completion the root absorbs would wake it again,
    /// and again, for the rest of the session (§8.39).
    #[test]
    fn the_roots_own_completion_is_not_filed_back_into_it() {
        let (mut app, _rx) = test_app("root-asleep");
        // The test holds the root's mailbox, so what the UI does with the report
        // is readable — and it must do nothing at all with it.
        let (root_tx, root_rx) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.agent_tx.insert(AgentId::ROOT, root_tx);
        app.update(Msg::Agent {
            conversation: app.tree.conversation(),
            id: AgentId::ROOT,
            event: AgentEvent::ParentAsleep {
                command: AgentMsg::ChildDone {
                    id: 0,
                    run: 1,
                    outcome: agent::Outcome::Finished("my own work".into()),
                },
            },
        });

        assert!(
            root_rx.try_recv().is_err(),
            "the root has no parent row, so a report about itself is dropped rather than \
             filed back into the mailbox that emitted it"
        );
    }

    /// A command for a child the tree no longer has starts nothing: the UI opens
    /// doors to the agents that are here, and a message — however old — never
    /// makes it invent one (finding H18).
    ///
    /// The command is one a *stale mailbox* delivered. The window's reap drops
    /// the node, and the child's name goes from the parent's books with it
    /// (`App::reap_history`, finding H19); a send made a moment before that is
    /// still in flight, and this is the door it arrives at.
    #[test]
    fn a_command_for_a_child_the_tree_has_forgotten_wakes_nothing() {
        let (mut app, _rx) = test_app("parent-wake-forgotten");
        let (tx, parked) = crossbeam_channel::unbounded::<AgentMsg>();
        drop(parked);
        app.tree.insert(Spawn {
            id: AgentId(2),
            parent: AgentId::ROOT,
            brief: "port the parser".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd: tx,
        });
        app.tree.finish(AgentId(2), Some("did it".to_string()));
        // What the window does with a child nobody is listening to any more:
        // the node and the mailbox go.
        app.tree.reap(&[AgentId(2)]);

        app.update(Msg::Agent {
            conversation: app.tree.conversation(),
            id: AgentId::ROOT,
            event: AgentEvent::ChildAsleep {
                child: 2,
                command: AgentMsg::Steer("hello?".into()),
            },
        });

        assert!(!app.tree.has(AgentId(2)), "no row is conjured");
        assert!(
            !app.tree.agent_tx.contains_key(&AgentId(2)),
            "and no actor behind the mailbox either"
        );
    }

    /// A message to a leftover worktree is refused — no actor was ever built
    /// for it, and that absence is the design — but the sentence must not say
    /// the agent is gone: the row, its worktree and its branch are on disk and
    /// on screen. `agent::gone` is for an agent whose node the UI reached for
    /// and found nothing (finding H10).
    #[test]
    fn a_message_to_a_leftover_says_it_was_never_given_an_actor() {
        use std::fs;

        let root = repo("leftover-actor");
        git(
            &root,
            &["worktree", "add", "-q", "-b", "mush/7", ".mush/wt/7"],
        );
        // Unmerged work: the startup pass keeps the worktree and registers the
        // leftover this test is about.
        fs::write(root.join(".mush/wt/7/work.txt"), "the leftover's work\n").unwrap();
        git(&root.join(".mush/wt/7"), &["add", "-A"]);
        git(
            &root.join(".mush/wt/7"),
            &["commit", "-qm", "leftover work"],
        );
        let (mut app, _rx) = app_root(&root, None, session_save::fake::Recorder::new());
        assert!(app.tree.has(AgentId(7)), "the leftover is a row on screen");

        app.tree.focus(AgentId(7));
        app.chat.insert("carry on");
        app.send_message();

        let line = text_of(&app);
        assert!(
            line.contains("never given an actor"),
            "the refusal says which absence this is: {line}"
        );
        assert!(
            !line.contains("is gone"),
            "and it is not a disappearance: {line}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Waking a parked child replaces its mailbox, and the *parent's* books hold
    /// the sender the revival replaced: its next `control` found no actor and
    /// took this same wake path again — every time, for the rest of the session,
    /// while the message landed anyway (finding H22).
    ///
    /// The books are the actor's, so the tree is the only hand that can correct
    /// them; this is it saying which sender is live, into the mailbox of the
    /// parent the row names.
    #[test]
    fn a_revived_child_hands_its_parent_the_live_mailbox() {
        let (mut app, _rx) = test_app("revive-mailbox");
        // The parent, whose books are the ones being corrected: the test holds
        // its mailbox, so the message is what it can read.
        let (parent_tx, parent_rx) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "a parent".to_string(),
            depth: 1,
            branch: Some("mush/1".to_string()),
            // No worktree was created, so no fork revision exists (see
            // `finished_child_of`).
            fork: None,
            cmd: parent_tx,
        });
        // The child, parked: a mailbox with nobody behind it is exactly what
        // reclaiming a finished child's thread leaves.
        let (tx, parked) = crossbeam_channel::unbounded::<AgentMsg>();
        drop(parked);
        app.tree.insert(Spawn {
            id: AgentId(2),
            parent: AgentId(1),
            brief: "port the parser".to_string(),
            depth: 2,
            branch: None,
            fork: None,
            cmd: tx,
        });
        app.tree.finish(AgentId(2), Some("did it".to_string()));

        assert!(
            app.deliver_to_actor(AgentId(2), AgentMsg::Stop(agent::Stop::Human)),
            "the revival took the command"
        );

        let told: Vec<AgentMsg> = parent_rx.try_iter().collect();
        let live = told
            .iter()
            .find_map(|msg| match msg {
                AgentMsg::ChildMailbox { id: 2, cmd } => Some(cmd.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the parent is handed the live mailbox: {told:?}"));
        assert!(
            live.send(AgentMsg::Stop(agent::Stop::Human)).is_ok(),
            "and it is the sender the revived actor is listening on"
        );
        assert!(
            app.tree.agent_tx[&AgentId(2)]
                .send(AgentMsg::Stop(agent::Stop::Human))
                .is_ok(),
            "the tree's own copy is the same live mailbox"
        );
    }

    /// The git line is refreshed on the transitions a human drives — a focus
    /// change and a command — so a file written outside mush does not leave the
    /// bar asserting a clean tree indefinitely (finding P8). The snapshot only
    /// moved on agent events and on a tick while something ran.
    #[test]
    fn a_focus_change_and_a_command_refresh_the_git_read() {
        let root = repo("refresh-git");
        let (mut app, rx) = app_and_rx(root.clone());
        wait_git(&mut app, &rx);
        assert_eq!(
            app.git.as_ref().map(|g| g.dirty),
            Some(0),
            "clean at the first read"
        );

        // A file written outside mush, after the read.
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        app.cycle_focus(1);
        wait_git(&mut app, &rx);
        assert_eq!(
            app.git.as_ref().map(|g| g.dirty),
            Some(1),
            "a focus change re-reads the repository"
        );

        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        app.apply_command(Command::Help);
        wait_git(&mut app, &rx);
        assert_eq!(
            app.git.as_ref().map(|g| g.dirty),
            Some(2),
            "a command re-reads the repository"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Over-full is a real state — a learned window smaller than the transcript
    /// already held — and the meter must not print a ratio greater than one
    /// with no mark (finding P9). The mark is the history budget's, the number
    /// every decision uses: at it `full`, past it `over`.
    #[test]
    fn the_context_meter_says_full_and_over_at_the_budget() {
        let (mut app, _rx) = test_app("meter-full");
        app.cell.edit(|cfg| cfg.set_context(32_768));
        let budget = app.cfg().history_budget();
        let system = app.chat.system().weight();
        // Exactly the budget: `full`, not an over-full ratio.
        app.chat.push_message(
            AgentId::ROOT,
            Message::user("x".repeat(budget - system - "user".len())),
        );
        assert_eq!(app.chat.used_weight_for(AgentId::ROOT), budget);
        let full = app.context_meter();
        assert!(full.contains(" full"), "{full}");
        assert!(!full.contains(" over"), "{full}");

        // One byte past it: the trimmer will cut, and the meter says so. The
        // transcript is still well inside the window this time — which is the
        // point: the old meter compared the same number to the *window*, so it
        // read this state as ordinary and the mark could not be reached in
        // normal operation (the audit's finding).
        app.chat.push_message(AgentId::ROOT, Message::user("!"));
        assert!(app.chat.used_weight_for(AgentId::ROOT) > budget);
        assert!(budget < app.cfg().context_tokens * BYTES_PER_TOKEN);
        let over = app.context_meter();
        assert!(over.contains(" over"), "{over}");
    }

    /// The meter prints the three numbers the run's decisions are made of:
    /// what the conversation weighs, the history budget it is measured
    /// against (the cut), and the fold's trigger inside that budget — so the
    /// human can see a fold or a cut coming. The window keeps its own number
    /// beside them, marked `~` when it is the assumed one.
    #[test]
    fn the_context_meter_shows_the_budget_the_fold_and_the_window() {
        let (mut app, _rx) = test_app("meter-marks");
        app.cell.edit(|cfg| cfg.set_context(32_768));
        let budget = app.cfg().history_budget();
        let fold = mush_core::transcript::compaction_trigger(budget);
        let used = app.chat.used_weight_for(AgentId::ROOT);
        let meter = app.context_meter();

        assert!(
            meter.contains(&tokens_label(used / BYTES_PER_TOKEN)),
            "what the conversation weighs: {meter}"
        );
        assert!(
            meter.contains(&format!("/{}", tokens_label(budget / BYTES_PER_TOKEN))),
            "the budget it is measured against: {meter}"
        );
        assert!(
            meter.contains(&tokens_label(fold / BYTES_PER_TOKEN)),
            "where the fold fires: {meter}"
        );
        assert!(
            !meter.contains('~'),
            "a window the human stated is not marked as derived: {meter}"
        );
        assert!(
            meter.ends_with(&tokens_label(app.cfg().context_tokens)),
            "the window's own number: {meter}"
        );
    }

    /// Below the floor the screen is one notice, so a key whose effect the
    /// human cannot see must not act: `Ctrl-N` used to wipe the conversation
    /// and start a new one from a screen showing only the floor message
    /// (finding P11 / refactor B3).
    #[test]
    fn the_floor_refuses_keys_except_quit() {
        let (mut app, _rx) = test_app("floor-keys");
        app.chat
            .push_message(AgentId::ROOT, Message::user("keep me"));
        app.set_term_size(30, 8);
        assert!(app.below_floor());

        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(text_of(&app), "", "Ctrl-N did not start a new chat");
        assert!(
            app.chat
                .conversation()
                .iter()
                .any(|m| m.content.as_deref() == Some("keep me")),
            "the conversation survives"
        );

        // A paste has no box on screen to land in either.
        app.update(Msg::Paste("typed while tiny".to_string()));
        assert!(!app.chat.input().text().contains("typed while tiny"));

        // Quit still works: a terminal too small to read is still a way out.
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        )));
        assert!(app.should_quit);

        // Grown back, the keymap drives the app again.
        let (mut app, _rx) = test_app("floor-grown");
        app.chat
            .push_message(AgentId::ROOT, Message::user("keep me"));
        app.set_term_size(30, 8);
        app.set_term_size(120, 32);
        assert!(!app.below_floor());
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
        )));
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
        )));
        assert!(
            app.chat
                .conversation()
                .iter()
                .all(|m| m.content.as_deref() != Some("keep me")),
            "Ctrl-N runs once the terminal is big enough"
        );
    }

    /// At the ubiquitous 80×24 the facts line carries the branch, the dirty
    /// count and the delta. The bar needed 26 rows for its second line, so at 24
    /// the doc's "always in view" was simply false (finding P12).
    #[test]
    fn the_facts_line_survives_at_80x24() {
        let (mut app, _rx) = test_app("facts-24");
        app.git = Some(git::RepoStatus {
            branch: "main".to_string(),
            dirty: 3,
            stat: git::Stat {
                files: 1,
                added: 12,
                removed: 4,
            },
        });
        app.git_at = Some(Instant::now());
        let rows = screen(&mut app, 80, 24);
        let facts = rows.last().unwrap();
        assert!(
            facts.contains("main") && facts.contains("±3") && facts.contains("+12−4"),
            "the facts row is missing the repository: {facts}"
        );
    }

    /// In compact mode the pane still owes the selected row a footer: the
    /// worktree and the git command to read it, which its row had to drop. Under
    /// six inner rows the footer used to vanish entirely (finding P12).
    #[test]
    fn a_compact_pane_pays_the_selected_row_a_footer() {
        let (mut app, _rx) = test_app("compact-footer");
        let conversation = app.tree.conversation();
        for id in 1..=4u64 {
            app.update(Msg::Agent {
                conversation,
                id: AgentId::ROOT,
                event: AgentEvent::Spawned {
                    child: id,
                    parent: 0,
                    brief: format!("task {id}"),
                    depth: 1,
                    branch: Some(format!("mush/{id}")),
                    fork: None,
                    title: None,
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }
        app.tree.move_cursor(1);
        let rows = screen(&mut app, 60, 17);
        assert!(
            rows.iter().any(|row| row.contains(".mush/wt/1")),
            "the selected row's worktree is on screen: {rows:?}"
        );
    }

    /// A pane smaller than the tree says how many rows it is hiding and which
    /// side they are on: nineteen agents in a four-row pane hid fifteen with
    /// nothing on screen saying so (finding P12).
    #[test]
    fn a_pane_smaller_than_the_tree_names_the_hidden_rows() {
        let (mut app, _rx) = test_app("hidden-rows");
        crowd(&mut app, 19);
        let title = screen(&mut app, 60, 17)[0].clone();
        assert!(title.contains('▼'), "rows below are named: {title}");

        app.tree.cursor_bottom();
        let title = screen(&mut app, 60, 17)[0].clone();
        assert!(title.contains('▲'), "at the bottom they are above: {title}");
    }

    /// An `App` whose session writes go to a real writer on a real path, for
    /// the tests that read the file back. The writer is returned so a test can
    /// see how many writes the conversation cost.
    fn app_writing(root: &std::path::Path) -> (App, Arc<session_save::Writer>) {
        let writer =
            Arc::new(session_save::Writer::new(root.to_path_buf()).expect("the worker starts"));
        let (app, _rx) = app_root(root, None, writer.clone());
        (app, writer)
    }

    /// An `App` on a scratch directory whose saves are recorded instead of
    /// written, so a test sees what the UI thread handed over and when.
    fn app_recording(label: &str) -> (App, Arc<session_save::fake::Recorder>) {
        let recorder = session_save::fake::Recorder::new();
        let (app, _rx) = app_root(&dir(label), None, recorder.clone());
        (app, recorder)
    }

    /// An empty directory for a session to be written into.
    fn dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mush-save-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A real `App` that adopted what is on disk in `root`, the way a restart
    /// does.
    fn reopened(root: &std::path::Path) -> App {
        let (app, _rx) = app_root(
            root,
            Session::load(root),
            session_save::fake::Recorder::new(),
        );
        app
    }

    /// One painted frame: the `Screen` `App` derived and the grid it was
    /// painted into, cell by cell and borders included.
    ///
    /// The layout tiers, the panes and the foot are only true together, which
    /// is why the audit that found these defects read rows instead of reasoning
    /// about them — and why the sweep reads the `Screen` too: a rect is a
    /// promise about where the words are allowed to be.
    struct Shot {
        screen: Screen,
        /// The frame, cell by cell: not the rows joined, because a cell can hold
        /// a grapheme of more than one character and a column is a column.
        cells: Vec<Vec<String>>,
    }

    impl Shot {
        /// One row, as it is read.
        fn line(&self, y: u16) -> String {
            self.cells[y as usize].concat()
        }

        /// The rows as a test reads them, with a pane's border stripped.
        fn rows(&self) -> Vec<String> {
            self.cells
                .iter()
                .map(|row| row.concat().trim_matches('│').trim_end().to_string())
                .collect()
        }

        /// The rows as a terminal *shows* them: the column after a wide glyph
        /// belongs to that glyph, so the blank the buffer keeps there is not a
        /// space of its own (finding B9's arithmetic, read back).
        fn shown(&self) -> Vec<String> {
            self.cells
                .iter()
                .map(|row| {
                    let mut out = String::new();
                    let mut covered = 0usize;
                    for cell in row {
                        if covered > 0 {
                            covered -= 1;
                            continue;
                        }
                        out.push_str(cell);
                        covered =
                            unicode_width::UnicodeWidthStr::width(cell.as_str()).saturating_sub(1);
                    }
                    out
                })
                .collect()
        }

        /// The whole frame as one string, for a `contains` assertion.
        fn text(&self) -> String {
            (0..self.cells.len() as u16)
                .map(|y| self.line(y))
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// The cells inside a rect, row by row.
        fn inside(&self, rect: Rect) -> Vec<String> {
            (rect.y..rect.y.saturating_add(rect.height))
                .map(|y| {
                    (rect.x..rect.x.saturating_add(rect.width))
                        .map(|x| self.cell(x, y))
                        .collect::<String>()
                })
                .collect()
        }

        fn cell(&self, x: u16, y: u16) -> String {
            self.cells
                .get(y as usize)
                .and_then(|row| row.get(x as usize))
                .cloned()
                .unwrap_or_default()
        }

        /// Every rect a pane owns, the popup included.
        fn pane_rects(&self) -> Vec<Rect> {
            match &self.screen {
                Screen::Floor { .. } => Vec::new(),
                Screen::Panes(panes) => {
                    let mut rects = vec![
                        panes.agents.area,
                        panes.chat.transcript_area,
                        panes.chat.input_area,
                        panes.bar.area,
                    ];
                    if let Some(picker) = &panes.picker {
                        rects.push(picker.area);
                    }
                    rects
                }
            }
        }

        /// Every rule a frame must keep whatever it says: nothing painted
        /// outside a pane, every pane's own frame intact, the bar keeping its
        /// row, and every line a pane was handed fitting the pane it is painted
        /// in.
        fn assert_shape(&self, case: &str, width: u16, height: u16) {
            let at = format!("{case} at {width}×{height}");
            let Screen::Panes(panes) = &self.screen else {
                panic!("{at}: the floor notice has no panes to check");
            };
            let rects = self.pane_rects();
            let holds = |rect: &Rect, x: u16, y: u16| {
                (rect.x..rect.x + rect.width).contains(&x)
                    && (rect.y..rect.y + rect.height).contains(&y)
            };

            // Nothing is painted outside a pane: the frame is blank everywhere
            // else, so a cell that is not is a widget that spilled over its
            // rect — or a pane whose rect is not where it paints.
            for (y, row) in self.cells.iter().enumerate() {
                for (x, cell) in row.iter().enumerate() {
                    if cell == " " {
                        continue;
                    }
                    assert!(
                        rects.iter().any(|rect| holds(rect, x as u16, y as u16)),
                        "{at}: `{cell}` at {x},{y} is outside every pane"
                    );
                }
            }

            // Every pane's own frame is intact — a border that moved, vanished
            // or was painted over is a pane whose neighbour is wrong. The cells
            // a popup covers are skipped: `Clear` erases what is under it on
            // purpose.
            let covered = panes.picker.as_ref().map(|picker| picker.area);
            for rect in [
                panes.agents.area,
                panes.chat.transcript_area,
                panes.chat.input_area,
            ] {
                // A pane zen hid is a zero rect: it has no rows to keep
                // intact, and no bottom border to find.
                if rect.height == 0 {
                    continue;
                }
                // The sides, from just under the top border to just above the
                // bottom one: the top row carries the pane's title, which may
                // reach either corner.
                for y in rect.y + 1..rect.y + rect.height.saturating_sub(1) {
                    for x in [rect.x, rect.x + rect.width - 1] {
                        if covered.is_some_and(|covered| holds(&covered, x, y)) {
                            continue;
                        }
                        assert_eq!(self.cell(x, y), "│", "{at}: the border at {x},{y}");
                    }
                }
                for x in rect.x..rect.x + rect.width {
                    let y = rect.y + rect.height - 1;
                    if covered.is_some_and(|covered| holds(&covered, x, y)) {
                        continue;
                    }
                    assert!(
                        "─└┘".contains(&self.cell(x, y)),
                        "{at}: the bottom border at {x},{y}: {:?}",
                        self.cell(x, y)
                    );
                }
            }
            if let Some(picker) = &panes.picker {
                let rect = picker.area;
                for y in rect.y + 1..rect.y + rect.height.saturating_sub(1) {
                    for x in [rect.x, rect.x + rect.width - 1] {
                        assert_eq!(self.cell(x, y), "│", "{at}: the popup's border at {x},{y}");
                    }
                }
                for x in rect.x..rect.x + rect.width {
                    let y = rect.y + rect.height - 1;
                    assert!(
                        "─└┘".contains(&self.cell(x, y)),
                        "{at}: the popup's bottom border at {x},{y}: {:?}",
                        self.cell(x, y)
                    );
                }
            }

            // The bar keeps its row on every terminal: the focus badge and the
            // line that is the only home an Info line, a failure or a command's
            // usage error has. It was the trailing constraint once, and at 40×10
            // it was simply not painted.
            let bar = self
                .inside(Rect {
                    height: 1,
                    ..panes.bar.area
                })
                .join("");
            assert!(
                bar.contains(" chat ") || bar.contains(" agents "),
                "{at}: the bar lost its badge: {bar:?}"
            );
            assert!(
                bar.trim().chars().count() > " chat ".len(),
                "{at}: the bar is a badge and nothing else: {bar:?}"
            );

            // Every row the agents pane was handed fits the columns it is
            // painted in: `fit_row`'s promise, read at the width the row is
            // really painted at, and a row that loses its tail to the renderer
            // is the defect this reads for (R1).
            let rows_area = Block::default()
                .borders(Borders::ALL)
                .inner(panes.agents.area);
            for row in &panes.agents.rows {
                let line = crate::ui::agent_line(row, rows_area.width as usize);
                assert!(
                    unicode_width::UnicodeWidthStr::width(line.as_str())
                        <= rows_area.width as usize,
                    "{at}: an agent row is wider than its pane: {line:?}"
                );
            }

            // Every line a pane was handed fits the pane it is painted in. A
            // wider line is clipped silently by the renderer, which is how a
            // wrapped message loses its tail.
            let room = Block::default()
                .borders(Borders::ALL)
                .inner(panes.chat.transcript_area);
            if let Some(painted) = &panes.chat.transcript {
                for line in &painted.lines {
                    assert!(
                        line.width() <= room.width as usize,
                        "{at}: a transcript line is wider than its pane: {line:?}"
                    );
                }
            }
            let field = Block::default()
                .borders(Borders::ALL)
                .inner(panes.chat.input_area);
            if let Some(input) = &panes.chat.input {
                let prompt = unicode_width::UnicodeWidthStr::width(input.prompt.as_str());
                for line in &input.lines {
                    assert!(
                        prompt + unicode_width::UnicodeWidthStr::width(line.as_str())
                            <= field.width as usize,
                        "{at}: a message-box line is wider than its pane: {line:?}"
                    );
                }
            }
        }
    }

    /// Paint one frame at a real terminal size, the way `main` does: the size
    /// the next frame (and any `/notes` opened between frames) sees, one
    /// `Screen` derived for the frame's own area, and it painted into a
    /// `TestBackend`. The `Screen` and the painted buffer are handed back
    /// together, because the tests read both: the derived words and the cells
    /// they reached — including the ones `screen` trims away (finding R28).
    ///
    /// The fixed palette, because the tests that read text do not care about
    /// hues and must keep reading what mush painted before them.
    fn painted(app: &mut App, width: u16, height: u16) -> (Screen, Buffer) {
        painted_with(app, width, height, &crate::theme::Theme::default())
    }

    /// [`painted`] with a theme handed in, for the test that reads what a hue
    /// reaches.
    fn painted_with(
        app: &mut App,
        width: u16,
        height: u16,
        theme: &crate::theme::Theme,
    ) -> (Screen, Buffer) {
        app.set_term_size(width, height);
        let screen = app.screen(Rect::new(0, 0, width, height));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &screen, theme))
            .unwrap();
        (screen, terminal.backend().buffer().clone())
    }

    fn shot(app: &mut App, width: u16, height: u16) -> Shot {
        let (screen, buffer) = painted(app, width, height);
        let cells = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect();
        Shot { screen, cells }
    }

    /// The pane rects one frame derives, and whether the chat pane has a
    /// transcript at all. The zen view's claims are claims about these — "the
    /// focused pane took the width", "the box kept its rows" — so these tests
    /// read the `Screen` the frame was derived from, not the text it painted.
    #[derive(Debug, PartialEq)]
    struct PaneRects {
        agents: Rect,
        transcript: Rect,
        input: Rect,
        bar: Rect,
        transcript_painted: bool,
    }

    fn pane_rects(app: &mut App, width: u16, height: u16) -> PaneRects {
        match painted(app, width, height).0 {
            Screen::Panes(panes) => PaneRects {
                agents: panes.agents.area,
                transcript: panes.chat.transcript_area,
                input: panes.chat.input_area,
                bar: panes.bar.area,
                transcript_painted: panes.chat.transcript.is_some(),
            },
            Screen::Floor { .. } => panic!("{width}×{height} is below the floor"),
        }
    }

    /// One painted frame, cell by cell and borders included: a leak lands *on*
    /// a border column, which is the one thing `screen` trims away.
    fn frame_grid(app: &mut App, width: u16, height: u16) -> Vec<String> {
        shot(app, width, height).rows()
    }

    /// No cell may carry a character that commands the display instead of being
    /// read: a control byte, a carriage return, an escape, or a bidi isolate.
    /// Untrusted words — a model reply, a tool result, an endpoint's error body,
    /// a model id, a job's command — reach every surface on this screen, so this
    /// is asserted over the *painted frame*, borders included, and never over
    /// the text those words came from (findings U9/N3).
    fn assert_no_command(at: &str, shot: &Shot) {
        for (y, row) in shot.cells.iter().enumerate() {
            for (x, cell) in row.iter().enumerate() {
                for ch in cell.chars() {
                    assert!(
                        !ch.is_control()
                            && !matches!(
                                ch,
                                '\u{202a}'..='\u{202e}'
                                    | '\u{2066}'..='\u{2069}'
                                    | '\u{200e}'
                                    | '\u{200f}'
                            ),
                        "{at}: {ch:?} at {x},{y} commands the display"
                    );
                }
            }
        }
    }

    /// The painted screen, row by row, at a real terminal size. The layout
    /// tiers, the panes and the foot are only true together, which is why the
    /// audit that found these defects read rows instead of reasoning about
    /// them. A pane's border is stripped: what a test reads is the row's text.
    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        shot(app, width, height).rows()
    }

    /// The painted rows that wear the agents pane's selection highlight — the
    /// cyan background `List` gives the cursor row. `screen` reads text only,
    /// and after the `› ` marker went the selection *is* that style, so this is
    /// how a test still reads which row the pane paints as selected. The bar is
    /// excluded because its focus badge wears the same cyan and is not a row.
    fn selected_rows(app: &mut App, width: u16, height: u16) -> Vec<usize> {
        let (_, buffer) = painted(app, width, height);
        let bar_rows = super::screen::bar_rows(height);
        (0..(height - bar_rows) as usize)
            .filter(|&y| {
                (0..width as usize)
                    .any(|x| buffer[(x as u16, y as u16)].bg == ratatui::style::Color::Cyan)
            })
            .collect()
    }

    /// What the chat pane paints, and only it: the agents pane is the columns
    /// to its left at this size.
    fn chat_rows(app: &mut App) -> Vec<String> {
        screen(app, 120, 32)
            .into_iter()
            .map(|row| row.chars().skip(31).collect())
            .collect()
    }

    /// One streamed message from the root, the way its actor sends them.
    fn streamed(app: &mut App, text: &str) {
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Message(Message::assistant(text)),
        });
    }

    /// Pretend the session became dirty `elapsed` ago, so the debounce is
    /// tested instead of waited out.
    fn age_session(app: &mut App, elapsed: Duration) {
        if app.session_dirty_at.is_some() {
            app.session_dirty_at = Some(Instant::now() - elapsed);
        }
    }

    /// An isolated agent's worktree with one commit on it, committed exactly the
    /// way the actor commits (so the subject is real, not test-shaped).
    fn isolated_work(root: &std::path::Path, id: u64, brief: &str) {
        let worktree = git::worktree_path(root, id);
        std::fs::create_dir_all(root.join(".mush")).unwrap();
        git(
            root,
            &[
                "worktree",
                "add",
                "-q",
                worktree.to_str().unwrap(),
                "-b",
                &git::branch_name(id),
            ],
        );
        std::fs::write(worktree.join("work.txt"), "the work\n").unwrap();
        git(&worktree, &["add", "-A"]);
        git(
            &worktree,
            &[
                "commit",
                "-qm",
                &crate::agent::commit_subject(id, brief, &agent::Outcome::Finished("ok".into())),
            ],
        );
    }

    /// A worktree left on disk is identified by what its commit says, not by a
    /// placeholder: the row must show the task the agent was actually given.
    #[test]
    fn a_leftover_worktree_recovers_its_brief_from_git() {
        let root = repo("recover");
        isolated_work(&root, 3, "port the parser module");
        let app = app_at(root.clone());

        let node = app
            .tree
            .agents
            .iter()
            .find(|node| node.id == AgentId(3))
            .expect("the worktree must be registered");
        assert_eq!(node.brief, "port the parser module");
        assert!(node.leftover);
        assert_eq!(node.branch.as_deref(), Some("mush/3"));
        assert_eq!(node.phase, Phase::Done, "the subject says it finished");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `reclaim_isolated` runs on the human's key, against a tree whose actors
    /// were only *told* to stop — so a node can still be running in its
    /// worktree when the pass goes looking. The sweep's live-tree guard must
    /// hold there too: a directory pulled out from under a live agent is
    /// recreated as a plain path by the next write, and the run's end would
    /// then commit inside the human's checkout (finding F8).
    #[test]
    fn ctrl_n_never_reclaims_a_worktree_a_live_node_holds() {
        let root = repo("ctrl-n-live");
        let (mut app, _rx) = app_and_rx(root.clone());

        // A worktree whose branch adds nothing to HEAD and whose checkout is
        // clean: exactly the state the isolated pass takes.
        let held = git::worktree_path(&root, 1);
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &git::branch_name(1),
                held.to_str().unwrap(),
                "HEAD",
            ],
        );
        let (tx, _child_rx) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "work in here".into(),
            depth: 1,
            branch: Some(git::branch_name(1)),
            fork: None,
            cmd: tx,
        });
        // The node is thinking: a run is in that directory right now.
        app.reclaim_isolated();
        assert!(held.exists(), "a running agent keeps its worktree");
        assert!(
            git::resolve(&root, &git::branch_name(1)).is_some(),
            "and its branch"
        );

        // The other half of the guard: a node at rest whose child is running is
        // about to be woken into its worktree, and that wake is a run in this
        // directory too.
        app.tree.finish(AgentId(1), Some("done".into()));
        app.tree.result_read(AgentId(1));
        let (tx2, _rx2) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(2),
            parent: AgentId(1),
            brief: "the child".into(),
            depth: 2,
            branch: None,
            fork: None,
            cmd: tx2,
        });
        app.reclaim_isolated();
        assert!(held.exists(), "a parent about to be woken keeps it too");
        assert!(git::resolve(&root, &git::branch_name(1)).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A branch mush did not commit to has no subject to read, so it keeps the
    /// placeholder rather than inventing a task.
    #[test]
    fn a_hand_made_branch_keeps_the_placeholder() {
        let root = repo("hand-made");
        isolated_work(&root, 5, "first");
        let worktree = root.join(".mush/wt/5");
        std::fs::write(worktree.join("more.txt"), "more\n").unwrap();
        git(&worktree, &["add", "-A"]);
        git(&worktree, &["commit", "-qm", "my own commit message"]);

        let app = app_at(root.clone());
        let node = app
            .tree
            .agents
            .iter()
            .find(|node| node.id == AgentId(5))
            .unwrap();
        assert_eq!(node.brief, "leftover worktree");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A stored conversation comes back with its subagents: a live mailbox each
    /// (so a follow-up message is delivered, not lost) and the transcript it had
    /// (which is the whole point of storing it).
    #[test]
    fn a_stored_conversation_restores_its_agents_with_a_live_mailbox() {
        let root = repo("restore");
        let stored = Session {
            model: "test-model".into(),
            provider: "custom".into(),
            base_url: "http://127.0.0.1:1".into(),
            context: None,
            messages: vec![Message::user("the root task")],
            agents: vec![session::AgentSession {
                id: 2,
                parent: Some(0),
                depth: 1,
                brief: "port the parser".into(),
                title: Some("parser port".into()),
                branch: None,
                status: session::StoredStatus::Done,
                landed: None,
                leftover: false,
                summary: Some("finished it".into()),
                result_unread: true,
                messages: vec![Message::user("port the parser"), Message::assistant("done")],
            }],
            notices: Vec::new(),
        };

        let (app, _rx) = app_root(&root, Some(stored), session_save::fake::Recorder::new());

        let node = app
            .tree
            .agents
            .iter()
            .find(|node| node.id == AgentId(2))
            .expect("the stored agent comes back");
        assert_eq!(node.brief, "port the parser");
        assert_eq!(
            node.title.as_deref(),
            Some("parser port"),
            "the name the caller gave survives the restart"
        );
        assert_eq!(node.phase, Phase::Done);
        assert_eq!(node.summary.as_deref(), Some("finished it"));
        assert!(
            node.result_unread,
            "a result nobody read before the restart still wears ✉ after it"
        );
        // The transcript is what makes a follow-up possible: without it the
        // human is back to writing the brief from scratch.
        assert_eq!(app.chat.transcript(AgentId(2)).len(), 2);
        assert_eq!(app.chat.transcript(AgentId(2))[1].text(), "done");
        // A live mailbox: a follow-up is delivered rather than dropped, which is
        // what "revive" has to mean to be worth anything.
        let tx_to_child = app
            .tree
            .agent_tx
            .get(&AgentId(2))
            .expect("a restored agent gets a mailbox");
        assert!(
            tx_to_child
                .send(AgentMsg::Nudge("one more thing".into()))
                .is_ok(),
            "the restored actor must still be listening"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A stored conversation whose root says `the root's own words` and whose
    /// `agents` are `rows` — id, parent, brief, one message each.
    fn stored_with_rows(rows: Vec<(u64, Option<u64>, &str, &str)>) -> Session {
        Session {
            model: "test-model".into(),
            provider: "custom".into(),
            base_url: "http://127.0.0.1:1".into(),
            context: None,
            messages: vec![Message::user("the root's own words")],
            agents: rows
                .into_iter()
                .map(|(id, parent, brief, line)| session::AgentSession {
                    id,
                    parent,
                    depth: 1,
                    brief: brief.into(),
                    title: None,
                    branch: None,
                    status: session::StoredStatus::Done,
                    landed: None,
                    leftover: false,
                    summary: None,
                    result_unread: false,
                    messages: vec![Message::user(line)],
                })
                .collect(),
            notices: Vec::new(),
        }
    }

    /// A stored row's id is not trusted: `0` is the root's, and registering a
    /// row under it made the row's transcript replace the root's conversation
    /// and its mailbox replace the root's actor (finding C9). The row is
    /// refused with one line naming the file and the row, and the root's own
    /// conversation is untouched — while the row *after* it still restores.
    #[test]
    fn a_stored_root_id_cannot_replace_the_root() {
        let root = repo("stored-root-id");
        let stored = stored_with_rows(vec![
            (0, Some(0), "impostor", "IMPOSTOR LINE"),
            (1, Some(0), "a real child", "a real line"),
        ]);
        let (app, _rx) = app_root(&root, Some(stored), session_save::fake::Recorder::new());

        assert_eq!(
            app.chat.transcript(AgentId::ROOT).len(),
            1,
            "the impostor did not replace the root's transcript"
        );
        assert_eq!(
            app.chat.transcript(AgentId::ROOT)[0].text(),
            "the root's own words"
        );
        assert_eq!(
            app.tree.agents.len(),
            2,
            "no row was registered under the root's id"
        );
        assert!(
            app.tree.agents.iter().any(|node| node.id == AgentId(1)),
            "the row after the refused one still came back"
        );
        let lines: Vec<String> = app
            .chat
            .stored_notices()
            .iter()
            .map(|notice| notice.text.clone())
            .collect();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("#0") && line.contains(".mush/session.json")),
            "one line names the file and the row: {lines:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An id already taken is not handed to a second row: the first row keeps
    /// its node and its transcript, the duplicate is refused and named, and the
    /// counter ends above every id the file held (finding C9 / B1) so a later
    /// child can collide with neither.
    #[test]
    fn a_duplicate_id_keeps_the_first_row() {
        let root = repo("stored-duplicate-id");
        let stored = stored_with_rows(vec![
            (2, Some(0), "first", "the first row's line"),
            (2, Some(0), "second", "the second row's line"),
        ]);
        let (app, _rx) = app_root(&root, Some(stored), session_save::fake::Recorder::new());

        let node = app
            .tree
            .agents
            .iter()
            .find(|node| node.id == AgentId(2))
            .expect("the first row comes back");
        assert_eq!(node.brief, "first");
        assert_eq!(app.chat.transcript(AgentId(2)).len(), 1);
        assert_eq!(
            app.chat.transcript(AgentId(2))[0].text(),
            "the first row's line"
        );
        assert_eq!(
            app.tree.agents.len(),
            2,
            "the duplicate registered no second #2"
        );
        let lines: Vec<String> = app
            .chat
            .stored_notices()
            .iter()
            .map(|notice| notice.text.clone())
            .collect();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("#2") && line.contains(".mush/session.json")),
            "the refused duplicate is named: {lines:?}"
        );
        assert!(
            app.tree.handles().ids.agents_floor() > 2,
            "the counter is above every id the file held"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An id at the ceiling used to panic the debug build (`agent.id + 1` in
    /// `reserve_agents`); the reservation now saturates, and the row is refused
    /// as one no floor can be kept above (finding C9).
    #[test]
    fn a_u64_max_id_does_not_panic_the_restore() {
        let root = repo("stored-max-id");
        let stored = stored_with_rows(vec![(u64::MAX, Some(0), "at the ceiling", "IMPOSTOR LINE")]);
        let (app, _rx) = app_root(&root, Some(stored), session_save::fake::Recorder::new());

        assert_eq!(
            app.chat.transcript(AgentId::ROOT)[0].text(),
            "the root's own words"
        );
        assert_eq!(
            app.tree.agents.len(),
            1,
            "no row was registered at the ceiling"
        );
        let lines: Vec<String> = app
            .chat
            .stored_notices()
            .iter()
            .map(|notice| notice.text.clone())
            .collect();
        assert!(
            lines.iter().any(|line| line.contains("session.json")),
            "the refused row is named: {lines:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A parent restored from a stored session starts with empty books while its
    /// children's rows are on screen: `status` answered "no children and no
    /// jobs" and `control` refused `no such child agent #2` about the child the
    /// human was looking at (finding H25).
    ///
    /// The books live in the actor, so the tree hands it every row as a message
    /// (`App::seed_children`) — and the only way to read what they say is to ask
    /// the parent. This drives a real run of the restored root whose first turn
    /// makes both questions, and reads the answers off the request that carries
    /// the two tool results back to the model.
    #[test]
    fn a_restored_parent_can_name_the_child_the_tree_shows() {
        let root = repo("seed-books");
        let scripted = Arc::new(
            Scripted::new()
                // The root's first turn: the two readers of its books.
                .when(|asked: &Asked| asked.depth().is_none())
                .calls(vec![
                    tool_call(
                        "c0",
                        "control",
                        serde_json::json!({ "id": "#2", "action": "stop" }),
                    ),
                    tool_call("c1", "status", serde_json::json!({})),
                ])
                // Anything after that has nothing left to answer.
                .says("done"),
        );
        let stored = Session {
            model: "test-model".into(),
            provider: "custom".into(),
            base_url: "http://127.0.0.1:1".into(),
            context: None,
            messages: vec![Message::user("the root task")],
            agents: vec![session::AgentSession {
                id: 2,
                parent: Some(0),
                depth: 1,
                brief: "port the parser".into(),
                title: None,
                branch: None,
                status: session::StoredStatus::Done,
                landed: None,
                leftover: false,
                summary: Some("finished it".into()),
                result_unread: true,
                messages: vec![Message::user("port the parser")],
            }],
            notices: Vec::new(),
        };
        let (app, _rx) = app_with_scripted_root(&root, Some(stored), scripted.clone());

        app.tree.agent_tx[&AgentId::ROOT]
            .send(AgentMsg::Nudge("what is agent #2 doing?".into()))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let saw = |needle: &str| {
            scripted
                .asked()
                .iter()
                .any(|ask| ask.depth().is_none() && ask.saw(needle))
        };
        // The results of the two calls come back in the request after them.
        while !saw("stopping agent #2") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(
            saw("stopping agent #2"),
            "the child on the screen is in its parent's books: {:?}",
            scripted.asked().len()
        );
        assert!(
            !saw("no such child agent"),
            "and `control` does not refuse the child the human can see"
        );
        assert!(
            saw("✉ #2 ✓ finished it"),
            "`status` lists it by the row's own summary and unread mark: {:?}",
            scripted
                .asked()
                .last()
                .map(|ask| ask.messages.last().map(Message::text))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A loopback endpoint that answers one run: the first request gets a
    /// `status` call, the second a plain "done".
    ///
    /// Nothing else can read a parent's books out loud. They live in the actor,
    /// the only reader is a tool, and `agent::revive` builds the revived actor
    /// its own `HttpModel` from the cell — so the URL is the one hand a test has
    /// on it, and this is the smallest endpoint that gets a `status` out of it
    /// (finding H25).
    fn status_endpoint() -> u16 {
        two_reply_endpoint(
            r#"{"choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":"c0","type":"function","function":{"name":"status","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#
                .to_string(),
            r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#
                .to_string(),
        )
    }

    /// A loopback endpoint that answers the first request with `first` and
    /// every one after with `then`.
    ///
    /// Nothing else can put words in an actor's model: `agent::revive` — and
    /// the root [`app_root_at`] starts — build their own `HttpModel` from the
    /// config cell, so the URL is the one hand a test has on it. The answers go
    /// out in request order across connections, because the client keeps one
    /// connection alive and reuses it; anything past the second reply gets
    /// `then` again, so a stray retry ends the run rather than hanging it.
    fn two_reply_endpoint(first: String, then: String) -> u16 {
        use std::net::TcpListener;

        let answers = [first, then];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        /// Answer requests on one connection until the client stops writing.
        fn answer(connection: std::net::TcpStream, answers: &[String; 2], served: &mut usize) {
            use std::io::{BufRead, BufReader, Read, Write};

            let mut connection = BufReader::new(connection);
            loop {
                // The head, then exactly the body its `Content-Length`
                // promises: the shape `http::write_request` writes.
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    // A read that ends, or fails, is this connection's end —
                    // the client went away, or let it go.
                    if connection.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("Content-Length: ") {
                        length = value.parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                if connection.read_exact(&mut body).is_err() {
                    return;
                }
                let answer = answers.get(*served).unwrap_or(&answers[1]);
                *served += 1;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{answer}",
                    answer.len()
                );
                let out = connection.get_mut();
                if out.write_all(reply.as_bytes()).is_err() || out.flush().is_err() {
                    return;
                }
            }
        }

        std::thread::spawn(move || {
            let mut served = 0usize;
            loop {
                let Ok((connection, _)) = listener.accept() else {
                    return;
                };
                answer(connection, &answers, &mut served);
            }
        });
        port
    }

    /// A loopback endpoint that answers every request with "done" and hands
    /// each request body back, so a test can read what an *actor* was built
    /// with: its system prompt names its workspace, and that workspace belongs
    /// to `agent::revive`, not to the tree the UI can see.
    fn recording_endpoint() -> (u16, Receiver<String>) {
        use std::net::TcpListener;

        let answer = r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = crossbeam_channel::unbounded::<String>();

        /// Answer requests on one connection until the client stops writing.
        fn serve(connection: std::net::TcpStream, answer: &str, seen: &Sender<String>) {
            use std::io::{BufRead, BufReader, Read, Write};

            let mut connection = BufReader::new(connection);
            loop {
                // The head, then exactly the body its `Content-Length`
                // promises: the shape `http::write_request` writes.
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if connection.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("Content-Length: ") {
                        length = value.parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                if connection.read_exact(&mut body).is_err() {
                    return;
                }
                let _ = seen.send(String::from_utf8_lossy(&body).into_owned());
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{answer}",
                    answer.len()
                );
                let out = connection.get_mut();
                if out.write_all(reply.as_bytes()).is_err() || out.flush().is_err() {
                    return;
                }
            }
        }

        std::thread::spawn(move || loop {
            let Ok((connection, _)) = listener.accept() else {
                return;
            };
            serve(connection, answer, &tx);
        });
        (port, rx)
    }

    /// A loopback endpoint that answers every request with one finished reply.
    ///
    /// The smallest server that lets a *revived* actor finish a run: its model
    /// is the cell's (`HttpModel`), not the scripted one the root answers from,
    /// so the URL is the only hand a test has on it — the same argument
    /// [`status_endpoint`] makes for a parent's books. One thread per connection,
    /// because two revived actors in one tree keep a connection each and a
    /// single-threaded accept loop would answer the first and hang the second.
    fn say_endpoint(text: &str) -> u16 {
        use std::net::TcpListener;

        let answer = format!(
            r#"{{"choices":[{{"message":{{"role":"assistant","content":{}}},"finish_reason":"stop"}}]}}"#,
            serde_json::to_string(text).unwrap()
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for connection in listener.incoming() {
                let Ok(connection) = connection else {
                    return;
                };
                let answer = answer.clone();
                std::thread::spawn(move || {
                    use std::io::{BufRead, BufReader, Read, Write};

                    let mut connection = BufReader::new(connection);
                    loop {
                        // The head, then exactly the body its `Content-Length`
                        // promises: the shape `http::write_request` writes.
                        let mut length = 0usize;
                        loop {
                            let mut line = String::new();
                            if connection.read_line(&mut line).unwrap_or(0) == 0 {
                                return;
                            }
                            let line = line.trim_end();
                            if line.is_empty() {
                                break;
                            }
                            if let Some(value) = line.strip_prefix("Content-Length: ") {
                                length = value.parse().unwrap_or(0);
                            }
                        }
                        let mut body = vec![0u8; length];
                        if connection.read_exact(&mut body).is_err() {
                            return;
                        }
                        let reply = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{answer}",
                            answer.len()
                        );
                        let out = connection.get_mut();
                        if out.write_all(reply.as_bytes()).is_err() || out.flush().is_err() {
                            return;
                        }
                    }
                });
            }
        });
        port
    }

    /// Drive the app's own loop by hand: apply every event the actors emitted
    /// until `until` holds, or the deadline passes.
    ///
    /// Most tests in this module read what an *actor* was asked, which is a
    /// value the scripted model records with the UI nowhere in the way. A test
    /// that needs a fact the *tree* owns — that a child's run is over, say,
    /// which a parent knows before the UI does — has to apply the events that
    /// move it, exactly as the window would (§8.39).
    fn pump(
        app: &mut App,
        rx: &Receiver<Msg>,
        scripted: &Arc<Scripted>,
        until: impl Fn(&App, &[Asked]) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            while let Ok(msg) = rx.try_recv() {
                app.update(msg);
            }
            if until(app, &scripted.asked()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The reported session, minus the one fact this test has no use for: two
    /// children the human had stopped on purpose before the restart — so they
    /// come back `⊘ stopped`, and with no branch and no worktree left
    /// (`live_branch` sends them to the main checkout) — whose stop lines the
    /// human had already read. Unread stored results would seed the parent's
    /// books with a line it owes a read, and the fold of *that* line, not the
    /// wake, would be the restored root's first boundary (finding H25).
    fn stored_continued_children() -> Session {
        let child = |id: u64, brief: &str| session::AgentSession {
            id,
            parent: Some(0),
            depth: 1,
            brief: brief.into(),
            title: None,
            branch: None,
            status: session::StoredStatus::Stopped,
            landed: None,
            leftover: false,
            summary: Some("stopped mid-task".into()),
            result_unread: false,
            messages: vec![Message::user(brief)],
        };
        Session {
            model: "test-model".into(),
            provider: "custom".into(),
            base_url: "http://127.0.0.1:1".into(),
            context: None,
            messages: vec![Message::user("the root task")],
            agents: vec![child(2, "port the parser"), child(3, "port the lexer")],
            notices: Vec::new(),
        }
    }

    /// The shape the defect was reported in, end to end: the root had stopped
    /// every child on purpose before a restart, the human told the restored root
    /// to keep going, and the root woke its children with a follow-up
    /// (`control message` — *continued*, not spawned). The children finished and
    /// nothing woke the root: each child's row wore `✉`, the root's wore `✉2`,
    /// and its own books answered `status` with `#30 ◐ running` while its
    /// transcript never received the line at all (§8.39).
    ///
    /// A nudge at the *root* is what gives its restored actor a transcript, so
    /// this is not the empty-transcript case (that one is
    /// `a_restored_childs_completion_wakes_the_root`): the root here is alive
    /// and mid-run when the news lands, which is the other road a completion
    /// takes — the fold at a message boundary (`drain_mailbox`), not the wake of
    /// an idle actor. What the completion road is then asked to carry is exactly
    /// what the session showed it could not.
    ///
    /// The test's own order comes from [`Gate`], never from sleeping: the root's
    /// second turn is a *held* scripted reply, so both children finish — and
    /// their completions are in the root's mailbox — before that turn's answer
    /// lands, and the boundary at its end is where they fold. Nothing waits for
    /// the root to nap: a root that is answering its children is what the fix
    /// produces, and the reported session's failure was that it stayed at rest.
    #[test]
    fn a_restored_root_is_woken_by_the_children_it_continued() {
        let root = repo("restored-continued");
        let held = Arc::new(Gate::new());
        let port = say_endpoint("does the thing");
        let scripted = Arc::new(
            Scripted::new()
                // The human's message starts the restored root's run, and its
                // first turn is the follow-up the session's root sent: two
                // `control message` calls, which continue stopped children
                // rather than spawn new ones.
                .calls(vec![
                    tool_call(
                        "c0",
                        "control",
                        serde_json::json!({ "id": "#2", "action": "message", "text": "carry on with the parser" }),
                    ),
                    tool_call(
                        "c1",
                        "control",
                        serde_json::json!({ "id": "#3", "action": "message", "text": "carry on with the lexer" }),
                    ),
                ])
                // …and its second turn is held, so that it ends without having
                // heard them: the news lands in the mailbox of a run that is
                // still going, which is the road this test is about.
                .held(held.clone())
                .says("they are on it")
                // The run the news buys, in the order a parent reads its books:
                // a `wait` for the results, a `status` for what the books say
                // now, and the end.
                .calls(vec![tool_call("c2", "wait", serde_json::json!({}))])
                .calls(vec![tool_call("c3", "status", serde_json::json!({}))])
                .says("done"),
        );
        let (mut app, rx) = app_with_scripted_root_at(
            &root,
            Some(stored_continued_children()),
            scripted.clone(),
            &format!("http://127.0.0.1:{port}"),
        );

        // The human tells the restored root to keep going. Its first turn
        // resumes both children; it is then parked in the held turn while they
        // work.
        app.deliver("keep going".into(), Vec::new())
            .expect("the restored root takes the human's words");
        assert!(
            held.wait_until_asked(Duration::from_secs(5)),
            "the root reaches the turn the hold is on"
        );

        // Both children run to their end. The tree is how this test knows: a
        // child tells its parent *before* it tells the UI, so a row reading `✓`
        // is proof that the completion is already in the parent's mailbox —
        // which is what the released turn's boundary then has to fold.
        let finished = |app: &App, _: &[Asked]| {
            [AgentId(2), AgentId(3)].iter().all(|id| {
                app.tree
                    .node(*id)
                    .is_some_and(|node| node.phase == Phase::Done)
            })
        };
        assert!(
            pump(&mut app, &rx, &scripted, finished),
            "both continued children run to their end"
        );

        held.release();
        assert!(
            pump(&mut app, &rx, &scripted, |_, asked| asked.len() >= 5),
            "the root runs on to its own end"
        );

        let asked = scripted.asked();
        let shape = |ask: &Asked| {
            ask.messages
                .iter()
                .map(|message| message.text().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            asked.len(),
            5,
            "the root asks exactly the five turns scripted for it: {:?}",
            asked.last().map(shape)
        );

        // (a) The wake the reported session never got: the root's *next*
        // request is a conversation — the system prompt and the human's own
        // words are in it, so it is not the bare completion line a run woken
        // onto an empty transcript would make — and both children's results are
        // folded into it, on the run that news buys.
        let woken = &asked[2];
        assert_eq!(
            woken.messages.first().map(|message| message.role.as_str()),
            Some("system"),
            "the news wakes a conversation, not a bare line: {:?}",
            shape(woken)
        );
        assert!(
            woken.saw("keep going")
                && woken.saw("#2 done: does the thing")
                && woken.saw("#3 done: does the thing"),
            "…the human's own words and both children's results are in it: {:?}",
            shape(woken)
        );

        // (b) The books, read the way a parent reads them: `status` answers each
        // child's *outcome*, not the `◐ running` a lost completion left on the
        // reported session.
        let books = asked.last().expect("the status request");
        assert!(
            books.saw("#2 ✓ does the thing") && books.saw("#3 ✓ does the thing"),
            "`status` answers both outcomes: {:?}",
            shape(books)
        );
        assert!(
            !books.saw("◐ running"),
            "…and no child is still booked running: {:?}",
            shape(books)
        );

        // (c) The books settled: the `wait` was *answered* out of them — with
        // both results the run had already read — instead of parking for its
        // whole 600 s cap, which is what the reported session's `wait` did. And
        // the listing `status` answered carries no `✉` for either child.
        //
        // The *books* and not the tree's `✉` marks, deliberately: `AgentTree`'s
        // `finish` (and `fail`/`stopped`) re-arm `result_unread = parent.is_some()`
        // unconditionally, and that arm and the parent's read travel on different
        // threads. `finish_run` hands the parent its `ChildDone` *before* it
        // emits the run's `Done` to the UI, so the parent can be scheduled in
        // between, fold the result, and have its `ResultRead` applied to the tree
        // *before* the child's `Done` re-lights the mark — a mark nothing then
        // clears, because the books hold that run as read and no second
        // `ResultRead` ever comes. A row wearing `✉` over a result its parent has
        // read is what the human would see (§8.39).
        let waited = &asked[3];
        assert!(
            waited.saw("#2 ✓ does the thing (already read — no new run since)")
                && waited.saw("#3 ✓ does the thing (already read — no new run since)"),
            "the wait hands both results over instead of parking: {:?}",
            shape(waited)
        );
        assert!(
            !books.saw("✉"),
            "…and the books say the parent has read both results: {:?}",
            shape(books)
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A stored session with one child, for the two tests below: the child that
    /// comes back is the one whose completion has to reach its parent.
    fn stored_with_one_child() -> Session {
        Session {
            model: "test-model".into(),
            provider: "custom".into(),
            base_url: "http://127.0.0.1:1".into(),
            context: None,
            messages: vec![Message::user("the root task")],
            agents: vec![session::AgentSession {
                id: 2,
                parent: Some(0),
                depth: 1,
                brief: "port the parser".into(),
                title: None,
                branch: None,
                status: session::StoredStatus::Done,
                landed: None,
                leftover: false,
                summary: Some("finished it".into()),
                result_unread: false,
                messages: vec![Message::user("port the parser"), Message::assistant("done")],
            }],
            notices: Vec::new(),
        }
    }

    /// A restored child's completion wakes the root. This is the end-to-end
    /// shape the suite was missing: every existing wake test supplied the wiring
    /// production lacked — `a_starting_run_tells_its_parent` builds the child
    /// with a parent mailbox by hand, `test_actor` builds one with no parent at
    /// all — and the restore tests assert only the *inbound* mailbox
    /// (`a_stored_conversation_restores_its_agents_with_a_live_mailbox`).
    ///
    /// The bug (§8.39): a restored child's `parent_tx` was a dead channel by
    /// construction (`App::restore_agents` passed `parent: None`), and nothing
    /// could ever repair it — the road that *would* wire a parent
    /// (`App::deliver_to_actor`) is reached only after a send into the child's
    /// mailbox fails, and a restored child's actor is alive. So the human's nudge
    /// landed, the row turned `✓`, and the root never ran again: the bar kept
    /// saying "waiting on 1 subagent(s) — the root resumes as they finish", its
    /// books kept the child running, and a `wait` burned its whole 600 s cap.
    ///
    /// What the root's request says is the whole fix, and both halves of it:
    /// the completion has to reach a live actor *and* that actor has to have a
    /// conversation for the fold to land in — a run woken onto an empty
    /// transcript is a request whose only message is `#2 done: …`, with no
    /// system prompt and no history.
    ///
    /// Nothing here drains the UI's own channel, so the completion can only
    /// arrive through the mailbox the restore itself wired: the UI's re-delivery
    /// is for a mailbox with no actor behind it, and that road has its own two
    /// tests (`a_report_with_no_actor_behind_the_parent_goes_to_the_ui` for the
    /// actor's half, `a_childs_report_reaches_the_parent_the_tree_names` for the
    /// app's).
    #[test]
    fn a_restored_childs_completion_wakes_the_root() {
        let root = repo("restored-wake");
        let port = say_endpoint("ported the parser");
        let scripted = Arc::new(Scripted::new().says("noted"));
        let (mut app, _rx) = app_with_scripted_root_at(
            &root,
            Some(stored_with_one_child()),
            scripted.clone(),
            &format!("http://127.0.0.1:{port}"),
        );
        // The human nudges the child the way the box does: through `deliver`,
        // aimed at the pane they are reading.
        assert!(app.tree.focus(AgentId(2)), "the restored child is a row");
        app.deliver("one more thing".into(), Vec::new())
            .expect("a restored child's actor takes the human's words");

        // Its run ends, its completion reaches the root's live mailbox, and the
        // root's run is the request that says so.
        let deadline = Instant::now() + Duration::from_secs(10);
        while scripted.asked().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let asked = scripted.asked();
        let wake = asked.first().expect(
            "the child's `✓` must wake the root: a completion that reaches nobody is the \
             ghost the row and the books disagree about (§8.39)",
        );
        assert_eq!(
            wake.messages.first().map(|message| message.role.as_str()),
            Some("system"),
            "the run the completion starts is a conversation, not a bare completion line: {:?}",
            wake.messages.iter().map(Message::text).collect::<Vec<_>>()
        );
        assert!(
            wake.saw("the root task"),
            "…the history it woke into is in it: {:?}",
            wake.messages.iter().map(Message::text).collect::<Vec<_>>()
        );
        assert!(
            wake.saw("#2 done: ported the parser"),
            "…and the child's result is folded in as a line the model reads: {:?}",
            wake.messages.iter().map(Message::text).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The revive order, pinned, because the wiring at the restore door reads
    /// `tree.agent_tx` *while* the loop is still building it: the file is in
    /// spawn order (`App::session_snapshot` walks the tree, and a parent's row is
    /// pushed before any child it spawns) and every revived actor's mailbox is
    /// registered the moment it is built (`AgentTree::register`), so a nested
    /// child's parent is there to be read — the whole tree is wired, not just the
    /// root's own children.
    ///
    /// The chain is what says so, and it is two hops that only exist because
    /// each one was woken: the grandchild's completion starts its parent's run,
    /// and *that* completion starts the root's. A grandchild reporting into a
    /// dead mailbox would land here as `#3 done: …` folded into the root's own
    /// request — a result skipping the parent it belongs to, which is the one
    /// thing the tree must never do (§8.39).
    #[test]
    fn a_restored_grandchilds_completion_walks_the_tree() {
        let root = repo("restored-nested");
        let port = say_endpoint("does the thing");
        let scripted = Arc::new(Scripted::new().says("noted"));
        let mut stored = stored_with_one_child();
        stored.agents.push(session::AgentSession {
            id: 3,
            parent: Some(2),
            depth: 2,
            brief: "port the lexer".into(),
            title: None,
            branch: None,
            status: session::StoredStatus::Done,
            landed: None,
            leftover: false,
            summary: Some("lexed it".into()),
            result_unread: false,
            messages: vec![Message::user("port the lexer"), Message::assistant("lexed")],
        });
        let (mut app, _rx) = app_with_scripted_root_at(
            &root,
            Some(stored),
            scripted.clone(),
            &format!("http://127.0.0.1:{port}"),
        );
        assert!(
            app.tree.focus(AgentId(3)),
            "the restored grandchild is a row"
        );
        app.deliver("one more thing".into(), Vec::new()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        let woke = || {
            scripted
                .asked()
                .iter()
                .any(|ask| ask.saw("#2 done: does the thing"))
        };
        while !woke() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let asked = scripted.asked();
        assert!(
            woke(),
            "the grandchild's result must reach its parent, and its parent's the root: {:?}",
            asked
                .last()
                .map(|ask| ask.messages.iter().map(Message::text).collect::<Vec<_>>())
        );
        assert!(
            !asked.iter().any(|ask| ask.saw("#3 done:")),
            "and the grandchild's own report stops at its parent, never the root"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The parent's books, which is what the lost completion cost: the human's
    /// nudge marked the child running (`App::tell_parent_running`, sent by the
    /// UI) and nothing ever cleared it, so a `wait` blocked on a child whose row
    /// said `✓` until its 600 s cap — and the one-shared-child guard refused the
    /// next shared spawn.
    ///
    /// The books live in the actor, so the way to read them is the way the
    /// audit read them: ask. The root's first run is the one the completion
    /// starts, and it calls `wait`; the answer arrives in the request *after*
    /// that call, because a wait still blocked on a finished child is exactly
    /// the failure — that request never comes.
    #[test]
    fn a_wait_on_a_restored_child_answers_the_result_not_the_deadline() {
        let root = repo("restored-books");
        let port = say_endpoint("ported the parser");
        let scripted = Arc::new(
            Scripted::new()
                .calls(vec![tool_call("c0", "wait", serde_json::json!({}))])
                .says("done"),
        );
        let (mut app, _rx) = app_with_scripted_root_at(
            &root,
            Some(stored_with_one_child()),
            scripted.clone(),
            &format!("http://127.0.0.1:{port}"),
        );
        assert!(app.tree.focus(AgentId(2)), "the restored child is a row");
        app.deliver("one more thing".into(), Vec::new()).unwrap();

        // The tool result of the `wait` is what the next request carries.
        let deadline = Instant::now() + Duration::from_secs(10);
        let digested = || {
            scripted
                .asked()
                .iter()
                .any(|ask| ask.saw("#2 ✓ ported the parser"))
        };
        while !digested() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let asked = scripted.asked();
        assert!(
            digested(),
            "the wait answers the child's result: {:?}",
            asked
                .last()
                .map(|ask| ask.messages.last().map(Message::text))
        );
        assert!(
            !asked.iter().any(|ask| ask.saw("wait timed out")),
            "and not the deadline: a child at rest with a read result is not running"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The wake path, not just the restore path: a parent whose actor was parked
    /// (a mailbox with no receiver behind it) is revived when a message arrives,
    /// and `agent::revive` starts it with an empty `ActorState` like any other.
    /// The rows its children have on screen have to be handed over *before* the
    /// words that wake it, or the run those words start answers "no children and
    /// no jobs" about a child the human can see (finding H25).
    ///
    /// The revived actor's model is the cell's, not a scripted one, so the
    /// endpoint above is what makes its first turn call `status` — and that tool
    /// result is where the books' words arrive.
    #[test]
    fn a_woken_parent_can_name_the_child_the_tree_shows() {
        let root = repo("wake-books");
        let port = status_endpoint();
        let (mut app, rx) = app_root_at(
            &root,
            None,
            session_save::fake::Recorder::new(),
            &format!("http://127.0.0.1:{port}"),
        );
        // The parent, parked: the tree holds its mailbox and nobody holds the
        // receiver, which is what reclaiming a finished child's thread leaves.
        let (tx, parked) = crossbeam_channel::unbounded::<AgentMsg>();
        drop(parked);
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "a parent".to_string(),
            depth: 1,
            branch: None,
            // Parked and hand-made: no worktree, so no fork revision.
            fork: None,
            cmd: tx,
        });
        // The child whose row is on screen, and whose name the books have to
        // carry: finished, its result read, which is exactly what `status`
        // lists as `#2 ✓ did 2`.
        let _child = finished_child_of(&mut app, 2, AgentId(1));

        assert!(
            app.deliver_to_actor(
                AgentId(1),
                AgentMsg::Nudge("what is agent #2 doing?".into())
            ),
            "the parked parent's mailbox takes the words that wake it"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut listing = String::new();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(msg) => {
                    if let Msg::Agent {
                        id,
                        event: AgentEvent::Message(message),
                        ..
                    } = &msg
                    {
                        if *id == AgentId(1) && message.role == "tool" {
                            listing = message.text().to_string();
                        }
                    }
                    app.update(msg);
                    if !listing.is_empty() {
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }

        assert!(
            listing.contains("#2 ✓ did 2"),
            "the child on the screen is in the woken parent's books: {listing:?}"
        );
        assert!(
            !listing.contains("no children and no jobs"),
            "and the books are not the empty ones a revival starts with"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A stored conversation with one agent, for the restore path.
    fn stored_with_agent(status: session::StoredStatus, messages: Vec<Message>) -> Session {
        Session {
            model: "test-model".into(),
            provider: "custom".into(),
            base_url: "http://127.0.0.1:1".into(),
            context: None,
            messages: vec![Message::user("the root task")],
            agents: vec![session::AgentSession {
                id: 2,
                parent: Some(0),
                depth: 1,
                brief: "port the parser".into(),
                title: None,
                branch: None,
                status,
                landed: None,
                leftover: false,
                summary: None,
                result_unread: false,
                messages,
            }],
            notices: Vec::new(),
        }
    }

    /// A landed agent's nudge is refused in the landing's own word: the past
    /// tense is [`Landed::past`]'s, so the refusal and the row cannot tell the
    /// same story two ways (refactor R12). The third landing is the case a past
    /// tense cannot carry — "was nothing committed" is not a sentence — so its
    /// refusal is worded around the same fact, and still claims no merge.
    #[test]
    fn a_nudge_to_a_landed_agent_names_how_it_landed() {
        for (landed, want) in [
            (
                session::StoredLanded::Merged,
                format!(
                    "agent #2 was {} — its worktree is gone; \
                     spawn a fresh agent or work in the root",
                    Landed::Merged.past()
                ),
            ),
            (
                session::StoredLanded::Discarded,
                format!(
                    "agent #2 was {} — its worktree is gone; \
                     spawn a fresh agent or work in the root",
                    Landed::Discarded.past()
                ),
            ),
            (
                session::StoredLanded::NothingCommitted,
                "agent #2 committed nothing — its worktree is gone; \
                 spawn a fresh agent or work in the root"
                    .to_string(),
            ),
        ] {
            let root = dir("landed-refusal");
            let mut stored = stored_with_agent(
                session::StoredStatus::Done,
                vec![Message::user("port the parser")],
            );
            stored.agents[0].landed = Some(landed);
            let (app, _rx) = app_root(&root, Some(stored), session_save::fake::Recorder::new());

            assert_eq!(
                app.worktree_gone(AgentId(2)).as_deref(),
                Some(want.as_str())
            );
            if matches!(landed, session::StoredLanded::NothingCommitted) {
                assert!(
                    !want.contains("merged"),
                    "a run that never committed must not claim a merge: {want}"
                );
            }
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// One decision at restore: a stored branch whose worktree is gone is not
    /// the agent's any more (finding U13). The row must not offer a diff
    /// for a reclaimed directory, must not paint a dead path in its
    /// footer, and must not refuse a nudge the actor would happily run in the
    /// main checkout.
    #[test]
    fn a_restored_branch_whose_worktree_is_gone_is_dropped() {
        let root = dir("restore-dead-branch");
        let mut stored = stored_with_agent(
            session::StoredStatus::Idle,
            vec![Message::user("port the parser")],
        );
        stored.agents[0].branch = Some("mush/2".to_string());
        let (app, _rx) = app_root(&root, Some(stored), session_save::fake::Recorder::new());

        let node = app.tree.node(AgentId(2)).expect("the agent is restored");
        assert!(
            node.branch.is_none(),
            "the branch went with its worktree: {:?}",
            node.branch
        );
        assert!(
            app.worktree_gone(AgentId(2)).is_none(),
            "a nudge must not be refused for a branch the agent no longer has"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same branch, but the worktree is really there and carries nothing:
    /// the startup sweep takes it (zero commits is `Merged` to a run with no
    /// stored fork revision — finding H21) and the *order* is what the actor
    /// sees. Swept before the revive, the child is built in the root it names
    /// in its system prompt; swept after, the actor's workspace is a directory
    /// the same breath deleted, and the row is labelled `merged` for work that
    /// never existed.
    #[test]
    fn a_restored_actor_is_built_after_the_sweep_that_takes_its_worktree() {
        let root = repo("restore-swept-worktree");
        // A real worktree on a branch standing on HEAD and nothing else: what a
        // restored run left behind when the process died before its first
        // commit.
        git(
            &root,
            &["worktree", "add", "-q", "-b", "mush/2", ".mush/wt/2"],
        );
        let worktree = root.join(".mush/wt/2");
        let mut stored = stored_with_agent(
            session::StoredStatus::Idle,
            vec![Message::user("port the parser")],
        );
        stored.agents[0].branch = Some("mush/2".to_string());

        let (port, asked) = recording_endpoint();
        let (mut app, _rx) = app_root_at(
            &root,
            Some(stored),
            session_save::fake::Recorder::new(),
            &format!("http://127.0.0.1:{port}"),
        );

        let node = app.tree.node(AgentId(2)).expect("the agent is restored");
        assert_eq!(
            node.landed, None,
            "a branch with no commit of its own is not a merge to claim"
        );
        assert_eq!(node.branch, None, "and the branch went with its worktree");
        assert!(
            !worktree.exists(),
            "the sweep took it before any actor was built on it"
        );
        assert!(
            app.worktree_gone(AgentId(2)).is_none(),
            "so nothing refuses a nudge"
        );

        // The actor the restore built runs in the root: its first request
        // names the workspace the model is told to work in, and that is the one
        // the child can still see — not the worktree the sweep took.
        app.tree.focus(AgentId(2));
        app.chat.insert("carry on");
        app.send_message();
        let body = asked
            .recv_timeout(Duration::from_secs(5))
            .expect("the restored child runs");
        assert!(
            body.contains(root.to_str().unwrap()),
            "the actor's workspace is the root: {body}"
        );
        assert!(
            !body.contains(".mush/wt/2"),
            "not the worktree the sweep took: {body}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Everything the restore heard from one agent, as text, for a test that
    /// has to prove what did *not* happen.
    fn events_from(rx: &Receiver<Msg>, id: AgentId, within: Duration) -> Vec<String> {
        let deadline = Instant::now() + within;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(Msg::Agent {
                    id: from, event, ..
                }) if from == id => {
                    seen.push(format!("{event:?}"));
                }
                Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        seen
    }

    /// Opening mush is not a request. A restored agent comes back with its
    /// transcript and its mailbox and does nothing until the human asks — the
    /// one thing it says at startup is the prompt its own history opens with
    /// ([`AgentEvent::SystemPrompt`]), which is a fact about the history the
    /// meter weighs, not a run.
    ///
    /// Reviving an agent by *running* it replayed every stored task against the
    /// endpoint the moment mush opened — thirteen agents, thirteen requests
    /// nobody asked for — and a thinking endpoint refusing a replayed turn left
    /// the whole tree marked `✗` over work that had finished.
    #[test]
    fn a_restored_agent_comes_back_at_rest() {
        let root = repo("restore-at-rest");
        let stored = stored_with_agent(
            session::StoredStatus::Done,
            vec![Message::user("port the parser"), Message::assistant("done")],
        );

        // Nothing answers here: an agent that ran would fail, loudly, in the
        // events this test reads.
        let (app, rx) = app_root(&root, Some(stored), session_save::fake::Recorder::new());

        let seen = events_from(&rx, AgentId(2), Duration::from_millis(300));
        let ran: Vec<&String> = seen
            .iter()
            .filter(|event| !event.starts_with("SystemPrompt("))
            .collect();
        assert!(
            ran.is_empty(),
            "a restored agent must not run at startup: {ran:?}"
        );
        assert_eq!(
            seen.len(),
            1,
            "and its own prompt is the one thing it says: {seen:?}"
        );
        assert_eq!(
            app.tree.node(AgentId(2)).map(|node| node.phase.clone()),
            Some(Phase::Done),
            "and its row still says what it did last"
        );
        // The mailbox is live, so the *human's* next message is what starts it.
        assert!(
            app.tree.agent_tx[&AgentId(2)]
                .send(AgentMsg::Nudge("carry on".into()))
                .is_ok(),
            "a restored agent is idle, not dead"
        );
        let started = events_from(&rx, AgentId(2), Duration::from_millis(500));
        assert!(
            started.iter().any(|event| event.contains("Running")),
            "the message it was sent starts the run: {started:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The stored transcript is the evidence of what an agent produced; the
    /// stored status is only the last thing that happened. An attempt the
    /// endpoint refused before it could touch the transcript (a resumed agent's
    /// first request) must not turn a finished agent into a `✗`, and the
    /// refusal belongs on the row either way.
    #[test]
    fn a_refused_attempt_does_not_turn_a_finished_agent_into_a_failure() {
        let root = repo("restore-refused");
        let refusal = "model returned HTTP 400: The `reasoning_content` in the thinking \
                       mode must be passed back to the API.";
        let stored = stored_with_agent(
            session::StoredStatus::Failed(refusal.to_string()),
            vec![Message::user("port the parser"), Message::assistant("done")],
        );

        let (app, _rx) = app_root(&root, Some(stored), session_save::fake::Recorder::new());

        let node = app.tree.node(AgentId(2)).expect("the agent is restored");
        assert_eq!(
            node.phase,
            Phase::Done,
            "the transcript ends on the answer the agent produced"
        );
        assert_eq!(
            node.summary.as_deref(),
            Some(format!("last attempt refused: {refusal}").as_str()),
            "the refusal is still on the row"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other half of that rule: an agent whose stored transcript stops
    /// mid-task has no answer to fall back on, so a stored failure stays one.
    #[test]
    fn a_failure_that_left_the_transcript_mid_task_is_still_a_failure() {
        let root = repo("restore-mid-task");
        let stored = stored_with_agent(
            session::StoredStatus::Failed("the endpoint stopped responding".into()),
            vec![
                Message::user("port the parser"),
                Message::assistant("starting now"),
                Message::user("and make it fast"),
            ],
        );

        let (app, _rx) = app_root(&root, Some(stored), session_save::fake::Recorder::new());

        assert_eq!(
            app.tree.node(AgentId(2)).map(|node| node.phase.clone()),
            Some(Phase::Failed("the endpoint stopped responding".into())),
            "an unfinished transcript keeps its failure"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A stored session file with one child of the root, written the way a
    /// human would hand-edit it: only the fields this test is about.
    fn stored_agent_file(root: &std::path::Path, status: &str) -> String {
        format!(
            r#"{{
              "root": "{}",
              "model": "test-model",
              "provider": "custom",
              "base_url": "http://127.0.0.1:1",
              "updated": 0,
              "messages": [{{"role": "user", "content": "the root task"}}],
              "agents": [
                {{
                  "id": 2,
                  "parent": 0,
                  "depth": 1,
                  "brief": "port the parser",
                  "status": "{status}",
                  "messages": []
                }}
              ]
            }}
"#,
            root.display()
        )
    }

    /// The clearest cut-off a restart can prove: the file's own status still
    /// said `running`, which no run ever writes as its own ending — so the run
    /// was in flight when the harness (or the terminal, or the process) went
    /// away, and nothing was committed by it.
    ///
    /// It came back `Idle`, which on screen and in the file is exactly what an
    /// agent nobody had asked to do anything looks like, and its work sat
    /// uncommitted until a human went looking (finding H2).
    #[test]
    fn a_restored_run_that_never_ended_comes_back_cut_off() {
        let root = dir("restore-cut-off");
        session::ensure_mush_dir(&root).unwrap();
        std::fs::write(
            session::session_path(&root),
            stored_agent_file(&root, "running"),
        )
        .unwrap();

        let mut app = reopened(&root);

        assert!(
            app.tree.has(AgentId(2)),
            "a status the file carries must not lose the agent"
        );
        let rows = screen(&mut app, 120, 24);
        assert!(
            rows.iter().any(|row| row.contains("⚠ #2")),
            "the row says the run never ended, and never says ✓: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("✓ #2")),
            "a run nobody finished is not a finished one: {rows:?}"
        );
        // The row is the mark; the pane says what follows from it, because
        // "recoverable or lost" is the question the human has.
        let notice = app
            .chat
            .notices_for(AgentId(2))
            .map(|notice| notice.text.clone())
            .collect::<Vec<_>>()
            .join(" · ");
        assert!(notice.contains("nothing was committed"), "{notice}");
        // And the parent is told in the words a completion would have used:
        // the model reads this on its next turn, which is the only way it can
        // know a child's work is uncommitted rather than finished.
        let root_text = app
            .chat
            .transcript(AgentId::ROOT)
            .iter()
            .map(|message| message.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(root_text.contains("#2 cut off"), "{root_text}");
        assert!(root_text.contains("nothing was committed"), "{root_text}");
        assert!(
            !root_text.contains("#2 done:"),
            "a cut-off run must never read as a result: {root_text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other half of the same fact: a cut-off run stays cut off. The status
    /// is written back on every save, so a `⚠` is not a mark that lasts one
    /// launch — and a second quit without touching the agent does not quietly
    /// turn it into `idle`.
    #[test]
    fn a_cut_off_run_stays_cut_off_across_a_second_launch() {
        let root = dir("cut-off-twice");
        session::ensure_mush_dir(&root).unwrap();
        std::fs::write(
            session::session_path(&root),
            stored_agent_file(&root, "running"),
        )
        .unwrap();

        let first = reopened(&root);
        let after = serde_json::to_string(&first.session_snapshot()).unwrap();
        assert!(
            after.contains(r#""status":"cut_off""#),
            "the file says what the row says: {after}"
        );
        std::fs::write(session::session_path(&root), after).unwrap();

        let mut second = reopened(&root);
        let rows = screen(&mut second, 120, 24);
        assert!(
            rows.iter().any(|row| row.contains("⚠ #2")),
            "a launch that changes nothing keeps the fact: {rows:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// What the file says about a run *in flight*. `Idle` was the old answer,
    /// and it is the one thing a busy agent is not: it made a killed run — the
    /// harness SIGTERM'd, the terminal closed — indistinguishable from an agent
    /// nobody had ever asked to do anything (finding H2).
    #[test]
    fn the_session_records_a_run_in_flight_as_running() {
        let root = dir("stored-running");
        let mut app = app_at(root.clone());
        crowd(&mut app, 1);
        assert_eq!(
            app.tree.node(AgentId(1)).map(|node| node.phase.clone()),
            Some(Phase::Thinking),
            "the child is mid-run"
        );

        let busy = serde_json::to_string(&app.session_snapshot()).unwrap();
        assert!(busy.contains(r#""status":"running""#), "{busy}");

        // A run that *ended* still stores its own ending: `running` is a state,
        // never a result.
        app.tree.finish(AgentId(1), Some("all done".to_string()));
        let done = serde_json::to_string(&app.session_snapshot()).unwrap();
        assert!(done.contains(r#""status":"done""#), "{done}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other half of the same writer: a run that never happened is not a
    /// run in flight. The wildcard that stores a busy phase is the *phases of a
    /// run in flight*, and an agent nobody has asked to do anything is not one
    /// of them — writing it as `running` brings it back `⚠ cut off` on the next
    /// launch, which is the flattened lie this change exists to stop, in the
    /// other direction (finding H2).
    #[test]
    fn an_agent_at_rest_is_stored_at_rest() {
        let root = dir("stored-idle");
        session::ensure_mush_dir(&root).unwrap();
        std::fs::write(
            session::session_path(&root),
            stored_agent_file(&root, "idle"),
        )
        .unwrap();

        let mut app = reopened(&root);
        assert_eq!(
            app.tree.node(AgentId(2)).map(|node| node.phase.clone()),
            Some(Phase::Idle),
            "an agent whose run never started comes back idle"
        );
        let stored = serde_json::to_string(&app.session_snapshot()).unwrap();
        assert!(stored.contains(r#""status":"idle""#), "{stored}");

        // And the row a restart paints is the same one: a `⚠` here would
        // claim a run was cut off that nobody had ever started.
        let rows = screen(&mut app, 120, 24);
        assert!(
            rows.iter().any(|row| row.contains("· #2")),
            "an agent at rest wears `·`: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("⚠ #2")),
            "and never the mark of a run that never ended: {rows:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A count a human has to count digits in says nothing at a glance, and the
    /// endpoint's real numbers are large ones.
    #[test]
    fn token_counts_read_at_a_glance() {
        assert_eq!(tokens_label(842), "842");
        assert_eq!(tokens_label(3_100), "3.1k");
        assert_eq!(tokens_label(32_000), "32k", "a whole number keeps no `.0`");
        assert_eq!(tokens_label(500_000), "500k");
        assert_eq!(tokens_label(999_900), "999.9k");
        assert_eq!(
            tokens_label(999_999),
            "1M",
            "the last stretch before a million is not `1000k`"
        );
        assert_eq!(tokens_label(1_213_866), "1.2M");
    }

    /// A paste lands in the message box as one insert and is *not* sent: mush
    /// must never decide for the human that what they pasted was a message.
    #[test]
    fn a_paste_fills_the_box_without_sending() {
        let (mut app, _rx) = test_app("paste");
        app.focus = Focus::Chat;
        app.update(Msg::Paste("line one\r\nline two\n".into()));
        // Line endings are normalised, and the whole paste arrives at once.
        assert_eq!(app.chat.input().text(), "line one\nline two\n");
        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "a paste is not a send"
        );
    }

    /// The bytes of a png, as far as `image_mime` is concerned: the magic
    /// number is the whole of what it reads, and the padding lets a test size a
    /// picture to whatever a budget needs.
    fn png(padding: usize) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.resize(8 + padding, 0);
        bytes
    }

    /// A saved image, as a test attaches one without going through a paste.
    /// Its header names no size, so it weighs its bytes — the fallback the
    /// budget tests below exercise.
    fn image(path: &str) -> Image {
        Image {
            path: path.to_string(),
            mime: "image/png".to_string(),
            bytes: png(0),
            pixels: None,
        }
    }

    /// Configure the one model the provider table documents as accepting image
    /// parts, so a test of "an attachment can ride" asks the real gate.
    fn let_the_model_see(app: &mut App) {
        app.cell.edit(|cfg| cfg.set_model("deepseek-flash"));
    }

    /// Spawn child `id` as an *isolated* agent: a `mush/<id>` branch, and the
    /// worktree on disk the branch names, so the child's own tools resolve
    /// paths in a workspace of their own. The spawn goes through the real
    /// `Spawned` event, so the brief opens the child's transcript. The mailbox
    /// is live and nobody reads it: a nudge to it lands without reviving an
    /// actor. Returns the child's workspace root and the mailbox to hold.
    fn isolate_child(app: &mut App, id: u64) -> (std::path::PathBuf, Receiver<AgentMsg>) {
        let (cmd, rx) = crossbeam_channel::unbounded();
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Spawned {
                child: id,
                parent: 0,
                brief: format!("task {id}"),
                depth: 1,
                branch: Some(format!("mush/{id}")),
                fork: None,
                title: None,
                cmd,
            },
        });
        let root = mush_core::git::worktree_path(app.ws.root(), id);
        std::fs::create_dir_all(&root).unwrap();
        (root, rx)
    }

    /// A bracketed paste that is nothing but an image's path attaches the image
    /// — no text is inserted — because that is what drag-and-drop and a file
    /// manager's "copy" produce. The gate is the real one, and the line the bar
    /// says is the one the human reads.
    #[test]
    fn a_pasted_image_path_attaches_and_inserts_nothing() {
        let (mut app, _rx) = test_app("paste-image");
        let_the_model_see(&mut app);
        std::fs::create_dir_all(app.ws.root().join("shots")).unwrap();
        std::fs::write(app.ws.root().join("shots/a.png"), png(4)).unwrap();

        app.update(Msg::Paste("shots/a.png".into()));

        assert_eq!(app.chat.input().text(), "", "the path is not text");
        let attached = app.chat.attachments();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].path, "shots/a.png");
        assert_eq!(attached[0].mime, "image/png");
        assert_eq!(attached[0].bytes, png(4), "the file's own bytes ride");
        let (line, kind) = app.status_line().expect("a line about the attach");
        assert_eq!(kind, StatusKind::Info);
        assert!(line.contains("shots/a.png"), "{line}");
        assert!(line.contains("png"), "the format is named: {line}");
        assert!(line.contains("Enter sends"), "and how to send it: {line}");
        assert!(app.chat.transcript(AgentId::ROOT).is_empty());
    }

    /// The human's report: four paths pasted at once are four pictures, not
    /// text. One gesture — nothing lands in the box — the four attach in the
    /// order pasted, the box paints the rows it allows with the last counting
    /// the rest, and the title counts all four.
    #[test]
    fn a_paste_of_four_image_paths_attaches_four_images() {
        let (mut app, _rx) = test_app("paste-four");
        let_the_model_see(&mut app);
        std::fs::create_dir_all(app.ws.root().join("shots")).unwrap();
        for i in 0..4 {
            std::fs::write(app.ws.root().join(format!("shots/{i}.png")), png(i)).unwrap();
        }
        let paste = "shots/0.png shots/1.png shots/2.png shots/3.png";

        app.update(Msg::Paste(paste.into()));

        assert_eq!(app.chat.input().text(), "", "four pictures, no text");
        let attached = app.chat.attachments();
        assert_eq!(attached.len(), 4, "one paste, four images");
        for (i, image) in attached.iter().enumerate() {
            assert_eq!(image.path, format!("shots/{i}.png"), "paste order");
            assert_eq!(image.bytes, png(i), "each picture's own bytes");
        }
        // One line for the gesture: the count, and how to send it.
        let (line, kind) = app.status_line().expect("one line for the gesture");
        assert_eq!(kind, StatusKind::Info);
        assert!(line.contains("attached 4 images"), "{line}");
        assert!(line.contains("Enter sends"), "{line}");
        assert!(!line.contains(".png"), "no per-picture line: {line}");

        // The box paints the rows it allows — the first two name their
        // pictures, the third counts the rest — and the title carries the
        // whole count.
        let Screen::Panes(panes) = app.screen(Rect::new(0, 0, 120, 32)) else {
            panic!("a terminal with room for the panes");
        };
        let input = panes.chat.input.as_ref().expect("the message box");
        assert_eq!(input.attachment_count, 4, "the title's count");
        assert_eq!(input.attachments.len(), 3, "the rows the box allows");
        assert!(
            input.attachments[0].contains("shots/0.png"),
            "{:?}",
            input.attachments
        );
        assert!(
            input.attachments[1].contains("shots/1.png"),
            "{:?}",
            input.attachments
        );
        assert!(
            input.attachments[2].contains("+2 more"),
            "{:?}",
            input.attachments
        );
        let text = shot(&mut app, 120, 32).text();
        assert!(text.contains("message · 4 images"), "{text}");
        assert!(text.contains("▣ +2 more"), "{text}");
    }

    /// A picture attached while a child is focused is carried to the child: the
    /// path the `Image` carries is one the child's *own* workspace reads back
    /// as its bytes, and the message the child receives carries the same path.
    ///
    /// The paste road read the human's file outside the workspace and copied it
    /// into the shared checkout, where the human's own view resolves it; a child
    /// with a worktree of its own resolves the same relative path against that
    /// worktree, where the copy is not. Before, the gate attached the shared
    /// path whatever agent was focused, so the placeholder the model was left
    /// with named a file it could not read (audit A2/B3: no test focused a child
    /// before attaching).
    #[test]
    fn an_image_attached_to_a_child_is_readable_from_the_childs_own_workspace() {
        let (mut app, _rx) = test_app("child-carry");
        let_the_model_see(&mut app);
        let (child_root, _mailbox) = isolate_child(&mut app, 1);
        // The human's file, outside the workspace mush was opened on.
        let outside =
            std::env::temp_dir().join(format!("mush-child-carry-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        let shot = outside.join("shot.png");
        std::fs::write(&shot, png(64)).unwrap();

        app.tree.focus(AgentId(1));
        app.update(Msg::Paste(shot.display().to_string()));

        // The box holds the picture under a path the child's own tools resolve.
        let attached = app.chat.attachments();
        assert_eq!(attached.len(), 1, "the paste attached it");
        let child = Workspace::new(&child_root).unwrap();
        let seen = child
            .read_image(&attached[0].path)
            .unwrap_or_else(|e| panic!("the child cannot read {}: {e}", attached[0].path))
            .expect("the path names an image in the child's workspace");
        assert_eq!(
            seen.bytes, attached[0].bytes,
            "byte-identical to what rides"
        );

        // And the message the child is sent carries the same path.
        app.chat.insert("look");
        app.send_message();
        let sent = app
            .chat
            .transcript(AgentId(1))
            .last()
            .expect("the message the child received");
        assert_eq!(sent.role, "user");
        let seen = child
            .read_image(&sent.images[0].path)
            .unwrap_or_else(|e| panic!("after the send, {}: {e}", sent.images[0].path))
            .expect("the sent path names an image in the child's workspace");
        assert_eq!(seen.bytes, sent.images[0].bytes);
    }

    /// A FIFO named like the picture must not freeze the pane. The carry asks
    /// whether the receiving workspace already holds the image's bytes, and a
    /// raw `fs::read` answers that by *opening* the path: on a FIFO with no
    /// writer that open blocks until one appears — on the UI thread, at every
    /// attach and every send. The carry goes through the workspace's own
    /// reader, which stats before it opens (finding #15's rule, and its
    /// watchdog test); the old read never answers, and a hang must fail here
    /// rather than hang the suite.
    #[test]
    fn the_carry_never_opens_a_name_that_is_not_a_regular_file() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut app, _rx) = test_app("carry-fifo");
            let_the_model_see(&mut app);
            let fifo = app.ws.root().join("x.png");
            let made = std::process::Command::new("mkfifo").arg(&fifo).status();
            let made = made.is_ok_and(|status| status.success());
            let attached = made && app.attach_image(image("x.png"));
            let path = app.chat.attachments().first().map(|i| i.path.clone());
            let _ = tx.send((made, attached, path));
        });
        let (made, attached, path) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a FIFO must answer, not hold an open until a writer appears");
        assert!(made, "this test needs `mkfifo` to build the shape it pins");
        assert!(attached, "the picture is carried into .mush/paste");
        let path = path.expect("the box holds it");
        assert!(
            path.starts_with(".mush/paste/"),
            "the FIFO was not the picture; the copy is: {path}"
        );
    }

    /// A paste of several paths is atomic the way one path is: one word that
    /// is not an image makes the whole paste the words it is — nothing
    /// attaches, nothing is swallowed.
    #[test]
    fn a_paste_with_a_word_that_is_not_an_image_lands_whole_as_text() {
        let (mut app, _rx) = test_app("paste-four-text");
        let_the_model_see(&mut app);
        std::fs::create_dir_all(app.ws.root().join("shots")).unwrap();
        std::fs::write(app.ws.root().join("shots/a.png"), png(0)).unwrap();
        std::fs::write(app.ws.root().join("shots/b.png"), png(4)).unwrap();
        let paste = "shots/a.png notes.txt shots/b.png";

        app.update(Msg::Paste(paste.into()));

        assert_eq!(app.chat.input().text(), paste, "the whole paste, as pasted");
        assert!(
            app.chat.attachments().is_empty(),
            "not even the image words"
        );
        assert!(app.chat.transcript(AgentId::ROOT).is_empty());
    }

    /// A batch the model may not see is refused as one gesture: the single
    /// picture's refusal, said once for the whole paste, nothing attached, and
    /// every word landing in the box as text.
    #[test]
    fn a_paste_of_images_that_cannot_ride_lands_whole_as_text() {
        let (mut app, _rx) = test_app("paste-four-blind");
        std::fs::write(app.ws.root().join("shot-a.png"), png(0)).unwrap();
        std::fs::write(app.ws.root().join("shot-b.png"), png(4)).unwrap();
        let paste = "shot-a.png shot-b.png";

        app.update(Msg::Paste(paste.into()));

        assert_eq!(app.chat.input().text(), paste);
        assert!(app.chat.attachments().is_empty());
        let (line, kind) = app.status_line().expect("the refusal stays on the bar");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("test-model"), "the model is named: {line}");
        assert!(line.contains("Ctrl-P"), "and the road is: {line}");
        assert_eq!(line.matches("Ctrl-P").count(), 1, "said once: {line}");
    }

    /// One member over the cap is a refusal, not the text fallback, and the
    /// batch is atomic the way one name is: every word — the small path
    /// included — lands in the box as text, nothing attaches, and one line
    /// says why.
    #[test]
    fn a_paste_with_a_member_past_the_cap_lands_whole_as_text() {
        let (mut app, _rx) = test_app("paste-four-big");
        let_the_model_see(&mut app);
        std::fs::write(app.ws.root().join("small.png"), png(0)).unwrap();
        let big = mush_core::workspace::IMAGE_FILE_CAP as usize;
        std::fs::write(app.ws.root().join("big.png"), png(big)).unwrap();
        let paste = "small.png big.png";

        app.update(Msg::Paste(paste.into()));

        assert_eq!(
            app.chat.input().text(),
            paste,
            "both words, not only the one that could not ride"
        );
        assert!(app.chat.attachments().is_empty());
        let (line, kind) = app.status_line().expect("a refusal");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("2 MB cap"), "{line}");
        assert!(line.contains("convert"), "the downscale road: {line}");
    }

    /// The room warning for a batch is one line, and it carries the count a
    /// single picture's line cannot: how many of the pictures are at stake.
    /// The conversation already fills a third of the budget and each picture is
    /// a quarter of it, so the third picture is the first past the room and the
    /// second is at stake — and all four are still attached, because the human
    /// decides what to send. The fourth lands on the budget exactly, which the
    /// window's bound accepts (it refuses only *past* it), so this is the
    /// warning and not a refusal.
    #[test]
    fn the_room_warning_for_a_batch_names_the_count_at_stake() {
        let (mut app, _rx) = test_app("paste-four-room");
        let_the_model_see(&mut app);
        app.cell.edit(|cfg| cfg.set_context(500_000));
        let budget = app.cfg().history_budget();
        app.chat
            .push_message(AgentId::ROOT, Message::user("x".repeat(budget / 3)));
        let room = budget - app.chat.used_weight_for(AgentId::ROOT);
        // Each picture weighs a quarter of the budget: bytes + path (11) +
        // mime (9).
        let each = budget / 4;
        assert!(
            2 * each <= room && 3 * each > room,
            "the third is the first past it"
        );
        std::fs::create_dir_all(app.ws.root().join("shots")).unwrap();
        for i in 0..4 {
            std::fs::write(app.ws.root().join(format!("shots/{i}.png")), png(each - 28)).unwrap();
        }
        let paste = "shots/0.png shots/1.png shots/2.png shots/3.png";

        app.update(Msg::Paste(paste.into()));

        assert_eq!(app.chat.attachments().len(), 4, "the human decides");
        let (line, kind) = app.status_line().expect("the fact is said");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("room left for history"), "{line}");
        assert!(line.contains("2 of 4 images"), "the count at stake: {line}");
        assert!(
            line.contains("oldest turns"),
            "what attaching them costs: {line}"
        );
        assert!(
            !line.contains("shed"),
            "a trim drops the turn a picture arrived in, not its bytes: {line}"
        );
        assert!(line.contains("/compact"), "one road to make room: {line}");
        assert!(line.contains("convert"), "and the downscale: {line}");
        assert_eq!(line.matches("room left").count(), 1, "said once: {line}");
    }

    /// Everything else a paste can be is still text: prose, and the path of a
    /// file that is not an image — which the model can still be asked to read.
    /// A paste is never swallowed.
    #[test]
    fn a_paste_that_is_not_an_image_is_still_text() {
        let (mut app, _rx) = test_app("paste-text");
        let_the_model_see(&mut app);
        std::fs::write(app.ws.root().join("notes.txt"), "hello").unwrap();

        app.update(Msg::Paste("look at this".into()));
        assert_eq!(app.chat.input().text(), "look at this");
        assert!(app.chat.attachments().is_empty());

        app.update(Msg::Paste("notes.txt".into()));
        assert_eq!(app.chat.input().text(), "look at thisnotes.txt");
        assert!(
            app.chat.attachments().is_empty(),
            "a text file is not an image"
        );
    }

    /// When the model is not documented to see, the path lands as text and the
    /// refusal says why: the model, what mush does not know about it, and the
    /// one key that changes the fact.
    #[test]
    fn a_paste_that_cannot_ride_lands_as_text_with_the_reason() {
        let (mut app, _rx) = test_app("paste-blind");
        std::fs::write(app.ws.root().join("shot.png"), png(0)).unwrap();

        app.update(Msg::Paste("shot.png".into()));

        assert_eq!(
            app.chat.input().text(),
            "shot.png",
            "a paste is never swallowed"
        );
        assert!(app.chat.attachments().is_empty());
        let (line, kind) = app.status_line().expect("the refusal stays on the bar");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("test-model"), "the model is named: {line}");
        assert!(line.contains("Ctrl-P"), "and the road is: {line}");
    }

    /// The gate itself, on its two refusing arms: no model at all, and a model
    /// the provider table does not document as seeing. Neither attaches, and
    /// the line names what the second one needs — `Ctrl-P`, the one key that
    /// changes the fact.
    #[test]
    fn the_attach_gate_refuses_a_model_that_may_not_see() {
        let (mut app, _rx) = test_app("attach-gate");
        app.cell.edit(|cfg| cfg.model.clear());

        assert!(!app.attach_image(image("shot.png")), "no model, no attach");
        assert!(app.chat.attachments().is_empty());
        let (line, kind) = app.status_line().expect("the refusal");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("no model"), "{line}");

        app.cell.edit(|cfg| cfg.set_model("test-model"));
        assert!(!app.attach_image(image("shot.png")));
        assert!(app.chat.attachments().is_empty(), "still not attached");
        let (line, kind) = app.status_line().expect("the refusal");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("test-model"), "the model is named: {line}");
        assert!(line.contains("Ctrl-P"), "and the road is: {line}");
    }

    /// The gate's third arm attaches *and* refuses a line: an image with no
    /// room left in the budget rides — the human decides what to send — but the
    /// fact they cannot see is said: attaching it costs the oldest turns of the
    /// conversation, which the trimmer drops at the next request to make room.
    /// The line names `/compact`, which folds those turns into a summary
    /// instead, and the downscale. The room is the budget minus what the
    /// conversation already weighs, not the whole budget.
    #[test]
    fn an_image_with_no_room_left_attaches_with_the_fact_said() {
        let (mut app, _rx) = test_app("attach-over-budget");
        let_the_model_see(&mut app);
        app.cell.edit(|cfg| cfg.set_context(1_024));
        let budget = app.cfg().history_budget();
        // A transcript that fills the budget: whatever the picture weighs, the
        // room left for it is nothing.
        app.chat
            .push_message(AgentId::ROOT, Message::user("x".repeat(budget)));
        let big = Image {
            bytes: png(4),
            ..image("shot.png")
        };

        assert!(app.attach_image(big), "attached anyway");

        assert_eq!(app.chat.attachments().len(), 1, "the human decides");
        let (line, kind) = app.status_line().expect("the fact is said");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("room left for history"), "{line}");
        assert!(line.contains("oldest turns"), "what it costs: {line}");
        assert!(line.contains("/compact"), "one road to spare them: {line}");
        assert!(line.contains("convert"), "and the downscale: {line}");
    }

    /// The room runs out whatever order the pictures arrived in: the images
    /// already waiting in the box are not in the transcript yet, so the gate
    /// counts them by hand — two pictures that each fit can still not fit
    /// together.
    #[test]
    fn the_images_already_in_the_box_count_against_the_room_left() {
        let (mut app, _rx) = test_app("attach-pending");
        let_the_model_see(&mut app);
        app.cell.edit(|cfg| cfg.set_context(120_000));
        let room = app.cfg().history_budget() - app.chat.used_weight_for(AgentId::ROOT);
        // A hair over half the room each, with the path and mime on top: one
        // fits alone, the second makes the pair too heavy.
        let each = room / 2 + 64;
        for path in ["shots/one.png", "shots/two.png"] {
            let pending = Image {
                bytes: png(each),
                ..image(path)
            };
            assert!(app.attach_image(pending), "{path} is attached either way");
        }

        assert_eq!(app.chat.attachments().len(), 2, "both are in the box");
        let (line, kind) = app.status_line().expect("the second says why");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("room left for history"), "{line}");
    }

    /// The room the gate weighs is the *focused* agent's: the same picture, in
    /// the same box, is warned about when the child's conversation has no room
    /// for it and attaches with the plain line when the child's is empty but
    /// the root's is full.
    ///
    /// Before, the gate asked `used_weight_for(AgentId::ROOT)` whichever agent
    /// was focused, so a child's own window was pinned by no test at all (audit
    /// A2: no test focused a child before attaching).
    #[test]
    fn the_attach_gate_weighs_the_focused_agents_room() {
        // The child's conversation is nearly full: a picture one byte past the
        // child's room, and nowhere near the root's.
        let (mut app, _rx) = test_app("child-room");
        let_the_model_see(&mut app);
        app.cell.edit(|cfg| cfg.set_context(500_000));
        let (child_root, _mailbox) = isolate_child(&mut app, 1);
        let room = app.cfg().history_budget() - app.chat.used_weight_for(AgentId(1));
        let path = "shots/big.png";
        let bytes = png(room + 1 - 8 - path.len() - "image/png".len());
        // The file lies in both workspaces with these bytes, so the picture is
        // carried unchanged and the weight is exactly the room plus one.
        for root in [app.ws.root().to_path_buf(), child_root.clone()] {
            std::fs::create_dir_all(root.join("shots")).unwrap();
            std::fs::write(root.join(path), &bytes).unwrap();
        }
        let picture = Image {
            path: path.to_string(),
            mime: "image/png".to_string(),
            bytes,
            pixels: None,
        };
        assert_eq!(picture.weight(), room + 1, "one byte past the child's room");

        app.tree.focus(AgentId(1));
        assert!(app.attach_image(picture.clone()), "the human decides");
        let (line, kind) = app
            .status_line()
            .expect("the child's room is the fact that warning is about");
        assert_eq!(kind, StatusKind::Error, "{line}");
        assert!(line.contains("room left for history"), "{line}");

        // The other way around, in its own app (an attachment changes the room
        // the next call sees): the root's conversation is full, the child's is
        // empty, and the same picture attaches with the plain line.
        let (mut app, _rx) = test_app("root-room");
        let_the_model_see(&mut app);
        app.cell.edit(|cfg| cfg.set_context(500_000));
        let (child_root, _mailbox) = isolate_child(&mut app, 1);
        let room = app.cfg().history_budget() - app.chat.used_weight_for(AgentId(1));
        let bytes = png(room / 2 - 8 - path.len() - "image/png".len());
        for root in [app.ws.root().to_path_buf(), child_root] {
            std::fs::create_dir_all(root.join("shots")).unwrap();
            std::fs::write(root.join(path), &bytes).unwrap();
        }
        let budget = app.cfg().history_budget();
        app.chat
            .push_message(AgentId::ROOT, Message::user("x".repeat(budget)));
        let picture = Image {
            path: path.to_string(),
            mime: "image/png".to_string(),
            bytes,
            pixels: None,
        };
        assert_eq!(picture.weight(), room / 2, "half the child's room");

        app.tree.focus(AgentId(1));
        assert!(app.attach_image(picture), "the human decides");
        let (line, kind) = app.status_line().expect("the plain line");
        assert_eq!(kind, StatusKind::Info, "the child has the room: {line}");
        assert!(line.contains("attached"), "{line}");
    }

    /// The human's own case, at the gate: a 724 KiB 1920×1080 screenshot
    /// dropped into a ~300k-token conversation on a 500k-token window attaches
    /// with the plain line. The room left is ~550 KB of weight and the picture
    /// costs ~2.8k tokens — not the ~247k its bytes used to read as, which is
    /// what once made the picture the first thing a trim shed.
    #[test]
    fn the_humans_screenshot_attaches_with_the_plain_line() {
        let (mut app, _rx) = test_app("attach-screenshot");
        let_the_model_see(&mut app);
        app.cell.edit(|cfg| cfg.set_context(500_000));
        app.chat
            .push_message(AgentId::ROOT, Message::user("x".repeat(900_000)));
        let shot = Image {
            bytes: png(741_388),
            pixels: Some((1_920, 1_080)),
            ..image("shots/screen.png")
        };
        // The picture names a file holding its own bytes, as a paste leaves it:
        // the gate keeps a path the receiving workspace reads back identically.
        std::fs::create_dir_all(app.ws.root().join("shots")).unwrap();
        std::fs::write(app.ws.root().join("shots/screen.png"), &shot.bytes).unwrap();

        assert!(app.attach_image(shot), "it fits the room left");

        assert_eq!(app.chat.attachments().len(), 1);
        let (line, kind) = app.status_line().expect("the plain line");
        assert_eq!(kind, StatusKind::Info);
        assert!(line.contains("shots/screen.png"), "{line}");
        assert!(line.contains("Enter sends"), "{line}");
        assert!(!line.contains("/compact"), "no warning: {line}");
    }

    /// The boundary is the window's, not a second one: a picture that weighs
    /// *exactly* the room left fits (the gate warns only above
    /// `cost + pending > room`, the inclusive comparison the trimmer makes at
    /// the ceiling), and one byte more warns. Two apps, because an attachment
    /// changes the room the next call sees.
    #[test]
    fn a_picture_exactly_at_the_room_left_fits_and_one_byte_more_does_not() {
        for (label, extra) in [("fits", 0usize), ("one-over", 1)] {
            let (mut app, _rx) = test_app(&format!("attach-boundary-{label}"));
            let_the_model_see(&mut app);
            app.cell.edit(|cfg| cfg.set_context(500_000));
            let room = app.cfg().history_budget() - app.chat.used_weight_for(AgentId::ROOT);
            // A headerless png weighs its bytes (8 + padding) plus its path
            // (8) and mime (9); size it to land on the boundary or a byte past
            // — and write the file, so the gate has the path it weighs.
            let bytes = png(room - 8 - 8 - 9 + extra);
            std::fs::write(app.ws.root().join("shot.png"), &bytes).unwrap();
            let image = Image {
                bytes,
                ..image("shot.png")
            };
            assert_eq!(image.weight(), room + extra);

            assert!(app.attach_image(image), "the human decides");
            let (line, kind) = app.status_line().expect("a line about the attach");
            if extra == 0 {
                assert_eq!(kind, StatusKind::Info, "exactly at the boundary: {line}");
                assert!(line.contains("attached"), "{line}");
            } else {
                assert_eq!(kind, StatusKind::Error, "one byte past: {line}");
                assert!(line.contains("room left for history"), "{line}");
            }
        }
    }

    /// The gate measures what the endpoint charges, not what the file weighs: a
    /// file at the transport's 2 MB cap whose 8×8 pixels are nowhere near the
    /// room it has, and a small file claiming 20,000×20,000 pixels past the
    /// whole window. File bytes are the transport's ruler (the cap on a read);
    /// pixels are the budget's.
    ///
    /// The big-file side is a file at the transport's own 2 MB cap — the
    /// largest a picture mush can carry — because the merge that made the
    /// reader stat before it opens also made a past-cap file unreadable to the
    /// workspace it lies in, and the gate will not hand an agent a path its own
    /// tools refuse (`carry_images`). The cap-sized file still weighs a few
    /// tokens: the contrast the test is about is between the file and the
    /// picture, not between two file sizes.
    #[test]
    fn the_attach_gate_counts_pixels_not_file_bytes() {
        let (mut app, _rx) = test_app("attach-measures");
        let_the_model_see(&mut app);
        app.cell.edit(|cfg| cfg.set_context(500_000));
        let room = app.cfg().history_budget() - app.chat.used_weight_for(AgentId::ROOT);

        let cap = mush_core::workspace::IMAGE_FILE_CAP as usize;
        let bytes_heavy = Image {
            bytes: png(cap - 8),
            pixels: Some((8, 8)),
            ..image("huge.png")
        };
        std::fs::write(app.ws.root().join("huge.png"), &bytes_heavy.bytes).unwrap();
        assert_eq!(bytes_heavy.bytes.len(), cap, "the transport's whole cap");
        assert!(
            bytes_heavy.weight() < room,
            "a full cap of file, a few tokens of picture"
        );
        assert!(app.attach_image(bytes_heavy), "it fits");
        assert_eq!(
            app.status_line().map(|(_, kind)| kind),
            Some(StatusKind::Info)
        );

        let pixel_heavy = Image {
            bytes: png(4),
            pixels: Some((20_000, 20_000)),
            ..image("big.png")
        };
        std::fs::write(app.ws.root().join("big.png"), &pixel_heavy.bytes).unwrap();
        assert!(
            pixel_heavy.weight() > room,
            "a small file whose pixels do not fit the room left"
        );
        assert!(
            pixel_heavy.weight() > app.cfg().history_budget(),
            "and they outweigh the whole window too"
        );
        assert!(!app.attach_image(pixel_heavy), "refused, not attached");
        assert_eq!(
            app.chat.attachments().len(),
            1,
            "the box holds only the picture that fit"
        );
        let (line, kind) = app.status_line().expect("the fact is said");
        assert_eq!(kind, StatusKind::Error);
        assert!(
            line.contains("whole history budget"),
            "the window cannot hold it: {line}"
        );
        assert!(
            !line.contains("/compact"),
            "and no fold shrinks a picture: {line}"
        );
        assert!(
            line.contains("convert"),
            "the downscale is the road: {line}"
        );
    }

    /// Nothing in the accounting panics on a header that claims the biggest
    /// picture there is: the picture outweighs the whole window, so the gate
    /// refuses it — and names the downscale — rather than wrapping around to
    /// "it fits".
    #[test]
    fn the_attach_gate_survives_a_header_that_claims_every_pixel() {
        let (mut app, _rx) = test_app("attach-overflow");
        let_the_model_see(&mut app);
        app.cell.edit(|cfg| cfg.set_context(500_000));
        let impossible = Image {
            bytes: png(4),
            pixels: Some((u32::MAX, u32::MAX)),
            ..image("huge.png")
        };
        std::fs::write(app.ws.root().join("huge.png"), &impossible.bytes).unwrap();

        assert!(!app.attach_image(impossible), "refused, not attached");
        assert!(app.chat.attachments().is_empty(), "nothing is in the box");

        let (line, kind) = app.status_line().expect("the fact is said");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("whole history budget"), "{line}");
        assert!(
            line.contains("convert"),
            "the downscale is the road: {line}"
        );
    }

    /// The window's bound, at the single door: a picture that fits the whole
    /// history budget on its own, but not with the pictures already in the box,
    /// is refused rather than attached with a warning — no trim of the
    /// conversation shrinks a picture, so a request carrying them would go out
    /// over the window and the endpoint would refuse it, and attaching would
    /// only spend a turn discovering that. `/compact` is named for what it is,
    /// a fold of the conversation: the weight over this bound is the pictures'.
    #[test]
    fn a_picture_that_would_push_the_box_past_the_budget_is_refused() {
        let (mut app, _rx) = test_app("attach-window-sum");
        let_the_model_see(&mut app);
        app.cell.edit(|cfg| cfg.set_context(500_000));
        let budget = app.cfg().history_budget();
        std::fs::create_dir_all(app.ws.root().join("shots")).unwrap();
        let first = Image {
            bytes: png(3 * budget / 4 - 28),
            ..image("shots/first.png")
        };
        std::fs::write(app.ws.root().join("shots/first.png"), &first.bytes).unwrap();
        assert!(first.weight() <= budget, "it fits the budget alone");
        assert!(app.attach_image(first), "and the box was empty");
        assert_eq!(
            app.status_line().map(|(_, kind)| kind),
            Some(StatusKind::Info)
        );

        let second = Image {
            bytes: png(budget / 2 - 28),
            ..image("shots/second.png")
        };
        std::fs::write(app.ws.root().join("shots/second.png"), &second.bytes).unwrap();
        assert!(second.weight() <= budget, "it also fits the budget alone");

        assert!(
            !app.attach_image(second),
            "the sum is past the budget: refused"
        );
        assert_eq!(
            app.chat.attachments().len(),
            1,
            "the first is all the box holds"
        );
        let (line, kind) = app.status_line().expect("the refusal");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("whole history budget"), "{line}");
        assert!(
            line.contains("pictures already in the box"),
            "what it is too heavy with: {line}"
        );
        assert!(
            line.contains("send the box's pictures first"),
            "a road that acts on the sum: {line}"
        );
        assert!(
            line.contains("`/compact` folds the conversation, not the pictures"),
            "the fold is named for what it is: {line}"
        );
    }

    /// The window's bound, at the batch door: a paste whose pictures, with the
    /// box's, would pass the whole history budget attaches *nothing* — the
    /// gesture is atomic, so half a batch is not a thing this door makes — and
    /// one line says how many of how many are at stake. The words land in the
    /// box as text, which is also the road to the downscale the line names.
    #[test]
    fn a_batch_that_would_push_the_box_past_the_budget_attaches_nothing() {
        let (mut app, _rx) = test_app("paste-window");
        let_the_model_see(&mut app);
        app.cell.edit(|cfg| cfg.set_context(500_000));
        let budget = app.cfg().history_budget();
        // Four pictures at two fifths of the budget each: two fit (4/5), the
        // third crosses (6/5) and so does the fourth.
        let each = 2 * budget / 5;
        std::fs::create_dir_all(app.ws.root().join("shots")).unwrap();
        for i in 0..4 {
            std::fs::write(app.ws.root().join(format!("shots/{i}.png")), png(each - 28)).unwrap();
        }
        let paste = "shots/0.png shots/1.png shots/2.png shots/3.png";

        app.update(Msg::Paste(paste.into()));

        assert_eq!(app.chat.input().text(), paste, "the words land as text");
        assert!(app.chat.attachments().is_empty(), "nothing attaches");
        let (line, kind) = app.status_line().expect("the refusal");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("whole history budget"), "{line}");
        assert!(line.contains("2 of 4 images"), "the count at stake: {line}");
        assert!(
            line.contains("send the box's pictures first"),
            "a road that acts on the sum: {line}"
        );
        assert!(
            line.contains("`/compact` folds the conversation, not the pictures"),
            "the fold is named for what it is: {line}"
        );
        assert!(line.contains("convert"), "the downscale: {line}");
    }

    /// The box's own bound is bytes, and it is not the window's: a picture of
    /// 8×8 pixels weighs a few tokens however large its file is. Eight files at
    /// the transport's 2 MB cap fill the box's [`BOX_IMAGE_BYTES`] exactly; the
    /// ninth is refused, and the line names the bound it was.
    #[test]
    fn a_picture_past_the_boxes_own_byte_bound_is_refused() {
        let (mut app, _rx) = test_app("attach-box-bytes");
        let_the_model_see(&mut app);
        app.cell.edit(|cfg| cfg.set_context(500_000));
        let cap = mush_core::workspace::IMAGE_FILE_CAP as usize;
        assert_eq!(BOX_IMAGE_BYTES, cap * 8, "eight of the transport's files");
        std::fs::create_dir_all(app.ws.root().join("shots")).unwrap();
        for i in 0..8 {
            let path = format!("shots/{i}.png");
            let bytes = png(cap - 8);
            std::fs::write(app.ws.root().join(&path), &bytes).unwrap();
            let picture = Image {
                path,
                bytes,
                pixels: Some((8, 8)),
                ..image("unused")
            };
            assert!(app.attach_image(picture), "picture {i} fits the box");
        }
        assert_eq!(app.chat.attachments().len(), 8);
        // The ninth is a file the transport allows and the box does not: its
        // pixels are as cheap as any of the eight, and its bytes are what the
        // box refuses.
        let ninth = Image {
            path: "shots/ninth.png".to_string(),
            bytes: png(cap - 8),
            pixels: Some((8, 8)),
            ..image("unused")
        };
        std::fs::write(app.ws.root().join("shots/ninth.png"), &ninth.bytes).unwrap();
        assert!(ninth.weight() < 64, "a few tokens of picture");

        assert!(!app.attach_image(ninth), "past the box's bound: refused");
        assert_eq!(app.chat.attachments().len(), 8, "the box is unchanged");
        let (line, kind) = app.status_line().expect("the refusal");
        assert_eq!(kind, StatusKind::Error);
        assert!(
            line.contains("picture bytes"),
            "the box's bound is named: {line}"
        );
        assert!(
            line.contains(&size_label(BOX_IMAGE_BYTES)),
            "and its number: {line}"
        );
        assert!(
            line.contains("token bound cannot bound bytes"),
            "why the window cannot be it: {line}"
        );
        assert!(line.contains("convert"), "the downscale: {line}");
    }

    /// The box's byte bound, at the batch door: two more files at the transport
    /// cap do not fit beside the eight the box already holds, whatever their
    /// pixels weigh. The batch is refused whole — nothing attaches — and the
    /// one line names the box's bound.
    #[test]
    fn a_batch_past_the_boxes_own_byte_bound_attaches_nothing() {
        let (mut app, _rx) = test_app("paste-box-bytes");
        let_the_model_see(&mut app);
        app.cell.edit(|cfg| cfg.set_context(500_000));
        let cap = mush_core::workspace::IMAGE_FILE_CAP as usize;
        // The box already holds eight cap-sized pictures; their pixels are 8×8,
        // so it is the bytes and not the window that is nearly full.
        for i in 0..8 {
            app.chat.attach(Image {
                path: format!("shots/held{i}.png"),
                bytes: png(cap - 8),
                pixels: Some((8, 8)),
                ..image("unused")
            });
        }
        // Two more, each a file the transport allows.
        let mut batch = Vec::new();
        std::fs::create_dir_all(app.ws.root().join("shots")).unwrap();
        for i in 0..2 {
            let path = format!("shots/extra{i}.png");
            let bytes = png(cap - 8);
            std::fs::write(app.ws.root().join(&path), &bytes).unwrap();
            batch.push(Image {
                path,
                bytes,
                pixels: Some((8, 8)),
                ..image("unused")
            });
        }

        assert!(!app.attach_images(batch), "past the box's bound: refused");
        assert_eq!(
            app.chat.attachments().len(),
            8,
            "nothing of the batch attaches"
        );
        let (line, kind) = app.status_line().expect("the refusal");
        assert_eq!(kind, StatusKind::Error);
        assert!(
            line.contains("picture bytes"),
            "the box's bound is named: {line}"
        );
        assert!(
            line.contains("2 images weighing"),
            "what the batch would add: {line}"
        );
        assert!(
            line.contains("token bound cannot bound bytes"),
            "why the window cannot be it: {line}"
        );
        assert!(
            line.contains("send the box's pictures first"),
            "a road that acts on the box: {line}"
        );
    }

    /// An image that *is* an image and cannot ride — past the cap — says so and
    /// still inserts the path: it is never swallowed, and the path is the road
    /// to the downscale the sentence names.
    #[test]
    fn a_pasted_image_past_the_cap_says_why_and_still_inserts_the_path() {
        let (mut app, _rx) = test_app("paste-big");
        let_the_model_see(&mut app);
        let big = mush_core::workspace::IMAGE_FILE_CAP as usize;
        std::fs::write(app.ws.root().join("big.png"), png(big)).unwrap();

        app.update(Msg::Paste("big.png".into()));

        assert_eq!(app.chat.input().text(), "big.png");
        assert!(app.chat.attachments().is_empty());
        let (line, kind) = app.status_line().expect("a refusal");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("2 MB cap"), "{line}");
        assert!(line.contains("convert"), "the downscale road: {line}");
    }

    /// `Ctrl-V`'s answer, in the message box: an image attaches, no image says
    /// so and names the other road, a failure is a failure — and an answer
    /// stamped with a conversation that is gone is dropped, like any stale
    /// event (`Ctrl-N` is the case the stamp exists for).
    #[test]
    fn a_clipboard_answer_attaches_or_says_there_is_none() {
        let (mut app, _rx) = test_app("clipboard");
        let_the_model_see(&mut app);
        let conversation = app.tree.conversation();
        // The clipboard road always writes its bytes into the workspace before
        // the gate sees them; the file is what makes the fixture one the gate
        // can keep the path of (a picture whose file the workspace reads back
        // with its own bytes is carried as it is).
        std::fs::write(app.ws.root().join("shot.png"), png(0)).unwrap();

        app.update(Msg::Clipboard {
            conversation,
            result: Ok(None),
        });
        assert!(app.chat.attachments().is_empty());
        let (line, kind) = app.status_line().expect("a line");
        assert_eq!(kind, StatusKind::Info);
        assert!(line.contains("holds no image"), "{line}");
        assert!(line.contains("path"), "and names the other road: {line}");

        app.update(Msg::Clipboard {
            conversation,
            result: Ok(Some(image("shot.png"))),
        });
        assert_eq!(app.chat.attachments().len(), 1);
        assert_eq!(app.chat.attachments()[0].path, "shot.png");

        app.update(Msg::Clipboard {
            conversation,
            result: Err("no clipboard reader on PATH".to_string()),
        });
        let (line, kind) = app.status_line().expect("the refusal");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("no clipboard reader"), "{line}");

        // A new chat: the answer the old one asked for is not news here.
        ctrl(&mut app, 'n');
        let before = app.chat.attachments().len();
        app.update(Msg::Clipboard {
            conversation,
            result: Ok(Some(image("stale.png"))),
        });
        assert_eq!(
            app.chat.attachments().len(),
            before,
            "a stale clipboard answer is dropped"
        );
    }

    /// `Ctrl-Y` opens the select mode on the conversation the pane shows: a
    /// pane with no words says so instead of opening a cursor over nothing, a
    /// second `Ctrl-Y` does not move the cursor the human is standing on, a
    /// letter while the mode is on is not typing, and `Tab` — the app-wide pane
    /// cycle, which the mode does not own — leaves it behind and moves on.
    #[test]
    fn ctrl_y_opens_the_select_mode_and_a_letter_is_not_typing() {
        let (mut app, _rx) = test_app("select-open");

        ctrl(&mut app, 'y');
        assert!(!app.chat.selecting(), "a pane with no words has no line");
        assert!(
            text_of(&app).contains("nothing to select"),
            "the key says why: {}",
            text_of(&app)
        );

        app.chat
            .push_message(AgentId::ROOT, Message::assistant("first\nsecond"));
        ctrl(&mut app, 'y');
        assert!(app.chat.selecting(), "the mode is on");

        // The mode takes the keyboard: the `x` is the mode's to drop, and an
        // empty box proves the drop — a `Chat(Insert('x'))` would have left it
        // there. `↑` is not the transcript's scroll either: it is the cursor.
        app.update(Msg::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        assert_eq!(
            app.chat.input().text(),
            "",
            "the letter never reached the box"
        );
        ctrl(&mut app, 'y');
        assert!(app.chat.selecting(), "a second Ctrl-Y is a no-op");

        tab(&mut app);
        assert!(!app.chat.selecting(), "Tab leaves the mode");
        assert_eq!(app.focus, Focus::Agents, "and cycles the pane it names");
    }

    /// A fold replaces the transcript under the select mode, and the mode must
    /// not be left pointing at rows that are gone: the automatic fold fires at
    /// nine tenths of the budget, so this is an ordinary long session's road,
    /// and the frame it panicked was the process's last — the UI thread's draw
    /// path takes every actor, every child's worktree and the unsent draft with
    /// it (D1, `chat.rs:1768`). The fold reaches the chat exactly as the actor
    /// sends it, `AgentEvent::Compact`, and either the cursor lands on a line
    /// that exists or the pane paints no cursor and no `Enter copies` clause.
    #[test]
    fn a_fold_while_selecting_does_not_panic_the_frame() {
        let (mut app, _rx) = test_app("probe-fold");
        app.chat
            .push_message(AgentId::ROOT, Message::assistant("first\nsecond"));
        app.chat.push_message(AgentId::ROOT, Message::user("third"));
        ctrl(&mut app, 'y');
        assert!(app.chat.selecting(), "the mode is on");
        app.update(Msg::Agent {
            conversation: app.tree.conversation(),
            id: AgentId::ROOT,
            event: AgentEvent::Compact {
                in_run: false,
                summary: "the summary".to_string(),
            },
        });
        assert!(
            !app.chat.selecting(),
            "the fold takes the mode with the rows"
        );
        let grid = frame_grid(&mut app, 80, 24);
        assert!(
            !grid.iter().any(|row| row.contains("Enter copies")),
            "no clause promises a cursor the pane is not painting"
        );
        assert!(
            grid.iter().any(|row| row.contains("the summary")),
            "the fold's own line is what the pane paints now"
        );
    }

    /// `Enter` in the select mode is the copy: the app hands the text to the
    /// writer it holds (never to a program the machine may not have, and never
    /// to the human's real clipboard), queues the line the copy built for the
    /// bar, and leaves the mode behind.
    #[test]
    fn enter_in_the_select_mode_hands_the_text_to_the_writer_and_says_what_copied() {
        let (mut app, rx) = test_app("select-copy");
        let written: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&written);
        app.write_clipboard = Arc::new(move |text: &str| {
            sink.lock().unwrap().push(text.to_string());
            Ok(())
        });
        app.chat
            .push_message(AgentId::ROOT, Message::assistant("first\nsecond"));

        ctrl(&mut app, 'y');
        // Shift-↑ extends: the cursor is on `first` and `second` is selected
        // with it, so the copy is both source lines.
        app.update(Msg::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)));
        app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(!app.chat.selecting(), "Enter leaves the mode");

        // The write is a thread, and its answer is the message: the line the
        // bar will say, and *what* the writer was handed.
        let answer = loop {
            match rx
                .recv_timeout(Duration::from_secs(30))
                .expect("the write answered")
            {
                msg @ Msg::Copied { .. } => break msg,
                // The idle root's own events are not this test's subject.
                _ => continue,
            }
        };
        let Msg::Copied {
            conversation,
            line,
            result,
        } = answer
        else {
            unreachable!("the loop stopped at a copy's answer");
        };
        assert_eq!(conversation, app.tree.conversation());
        assert_eq!(line, "copied 2 lines from #0's reply — 12 bytes");
        assert!(result.is_ok(), "the writer the test handed in took it");
        assert_eq!(
            written.lock().unwrap().as_slice(),
            ["first\nsecond"],
            "the source lines, joined with the transcript's own newline"
        );

        // The answer with the live conversation is the bar's line.
        app.update(Msg::Copied {
            conversation,
            line,
            result: Ok(()),
        });
        assert_eq!(text_of(&app), "copied 2 lines from #0's reply — 12 bytes");
    }

    /// The copy's answer carries the conversation that asked for it, so one
    /// that outlived a `Ctrl-N` says nothing into the new chat — and a write
    /// that failed is a failure, which does not fade.
    #[test]
    fn a_copy_answer_from_a_chat_that_is_gone_is_dropped() {
        let (mut app, _rx) = test_app("select-stale");
        let conversation = app.tree.conversation();
        ctrl(&mut app, 'n');
        let before = text_of(&app).to_string();
        app.update(Msg::Copied {
            conversation,
            line: "copied 2 lines from #0's reply — 12 bytes".to_string(),
            result: Ok(()),
        });
        assert_eq!(text_of(&app), before, "a stale copy is not news here");

        app.update(Msg::Copied {
            conversation: app.tree.conversation(),
            line: "copied 2 lines from #0's reply — 12 bytes".to_string(),
            result: Err("no clipboard writer on PATH".to_string()),
        });
        let (line, kind) = app.status_line().expect("the writer's failure");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("no clipboard writer"), "{line}");
    }

    /// Backspace takes the thing immediately before the cursor, and at the very
    /// start of the box that is the newest attachment — the pictures are painted
    /// above the words. Esc clears both halves of the box.
    #[test]
    fn backspace_at_the_boxes_start_pops_the_newest_attachment_and_esc_clears_both() {
        let (mut app, _rx) = test_app("box-keys");
        app.focus = Focus::Chat;
        app.chat.attach(image("a.png"));
        app.chat.attach(image("b.png"));
        app.chat.insert("hi");

        // With the cursor at the very start and words in the box, the key takes
        // the picture overhead: a plain backspace had nothing to delete there.
        app.update(Msg::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)));
        let bar = text_of(&app).to_string();
        backspace(&mut app);
        assert_eq!(
            text_of(&app),
            bar.as_str(),
            "a pop says nothing: the row vanishing is the feedback"
        );
        assert_eq!(app.chat.input().text(), "hi", "the words are untouched");
        assert_eq!(app.chat.attachments().len(), 1);
        assert_eq!(
            app.chat.attachments()[0].path,
            "a.png",
            "the newest goes first"
        );
        backspace(&mut app);
        assert!(app.chat.attachments().is_empty(), "one press, one image");

        // With no pictures left, Backspace is the text's, wherever it lands.
        backspace(&mut app);
        assert_eq!(app.chat.input().text(), "hi", "index 0 deletes nothing");
        app.update(Msg::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)));
        backspace(&mut app);
        assert_eq!(app.chat.input().text(), "h");

        // Attach again, type, and Esc: both halves of the box go.
        app.chat.attach(image("c.png"));
        app.chat.insert("draft");
        app.update(Msg::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(app.chat.input().text(), "");
        assert!(app.chat.attachments().is_empty());
    }

    /// Esc names what went and the one key that puts it back: the bar is where a
    /// human learns the road exists, at the moment they need it.
    #[test]
    fn esc_says_what_it_cleared_and_the_way_back() {
        let (mut app, _rx) = test_app("esc-line");
        app.focus = Focus::Chat;
        app.chat.attach(image("a.png"));
        app.chat.attach(image("b.png"));
        app.chat.insert("draft");

        app.update(Msg::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));

        let (line, kind) = app.status_line().expect("the line Esc leaves");
        assert_eq!(kind, StatusKind::Info, "a line, not a failure");
        assert_eq!(line, "cleared the box and 2 images · Ctrl-Z puts it back");
        assert!(app.chat.input().is_empty());
        assert!(app.chat.attachments().is_empty());

        // The key the line names is the key that does it.
        ctrl(&mut app, 'z');
        assert_eq!(app.chat.input().text(), "draft");
        assert_eq!(app.chat.attachments().len(), 2, "and both pictures");
    }

    /// A send spends the `Ctrl-Z` slot: the draft left the box, so no keystroke
    /// brings it back — even when the slot was holding an earlier loss.
    #[test]
    fn a_send_spends_the_slot() {
        let (mut app, _rx) = test_app("send-spends");
        let_the_model_see(&mut app);
        app.focus = Focus::Chat;
        app.chat.insert("lost words");
        app.update(Msg::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.chat.insert("sent words");

        app.send_message();

        ctrl(&mut app, 'z');
        assert_eq!(app.chat.input().text(), "", "the sent draft stays sent");
    }

    /// Enter on an empty box sends nothing, so it spends nothing: the slot is
    /// still there for the key that refills the box.
    #[test]
    fn enter_on_an_empty_box_does_not_spend_the_slot() {
        let (mut app, _rx) = test_app("empty-enter");
        app.focus = Focus::Chat;
        app.chat.insert("lost words");
        app.update(Msg::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));

        app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

        ctrl(&mut app, 'z');
        assert_eq!(app.chat.input().text(), "lost words");
    }

    /// The box's painted promises survive every key that moves the draft: the
    /// rows are what is attached, and the title counts the whole message.
    #[test]
    fn the_boxes_rows_and_title_follow_the_edit_keys() {
        let (mut app, _rx) = test_app("paint-keys");
        app.focus = Focus::Chat;
        app.chat.attach(image("a.png"));
        app.chat.attach(image("b.png"));
        app.chat.insert("draft");

        let painted = shot(&mut app, 120, 32).text();
        assert!(painted.contains("▣ a.png"), "{painted}");
        assert!(painted.contains("▣ b.png"), "{painted}");
        assert!(painted.contains("message · 2 images"), "{painted}");

        // A pop takes the newest row, and the title with it.
        app.update(Msg::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)));
        backspace(&mut app);
        let painted = shot(&mut app, 120, 32).text();
        assert!(!painted.contains("▣ b.png"), "the row went too: {painted}");
        assert!(painted.contains("▣ a.png"), "{painted}");
        assert!(painted.contains("message · 1 image"), "{painted}");

        // Ctrl-U clears the words and leaves the rows and the count alone.
        ctrl(&mut app, 'u');
        let painted = shot(&mut app, 120, 32).text();
        assert!(!painted.contains("draft"), "the words are gone: {painted}");
        assert!(painted.contains("▣ a.png"), "{painted}");
        assert!(painted.contains("message · 1 image"), "{painted}");

        // Ctrl-Z puts the words back; the rows never moved.
        ctrl(&mut app, 'z');
        let painted = shot(&mut app, 120, 32).text();
        assert!(painted.contains("draft"), "the words are back: {painted}");
        assert!(painted.contains("▣ a.png"), "{painted}");
        assert!(painted.contains("message · 1 image"), "{painted}");

        // Esc empties the box, rows and title included. The count is what the
        // title promises, so that is what is read — the bar's own line about
        // the clear names an image too, and it is a different surface.
        app.update(Msg::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        let painted = shot(&mut app, 120, 32).text();
        assert!(!painted.contains("▣"), "the rows went: {painted}");
        assert!(
            !painted.contains("message · "),
            "and the title stops counting: {painted}"
        );
        assert!(painted.contains("┌ message ─"), "{painted}");
    }

    /// An image-only message is a legal send — the box is empty and Enter still
    /// sends, because the attachment is the message — and it is still marked as
    /// the human's line in the pane: the `▣` row is what was said, the mark is
    /// who said it.
    #[test]
    fn an_empty_box_with_an_attachment_still_sends() {
        let (mut app, _rx) = test_app("image-only");
        let_the_model_see(&mut app);
        app.focus = Focus::Chat;
        std::fs::write(app.ws.root().join("shot.png"), png(0)).unwrap();
        app.chat.attach(image("shot.png"));

        app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

        let sent = app
            .chat
            .transcript(AgentId::ROOT)
            .last()
            .expect("the message");
        assert_eq!(sent.role, "user");
        assert_eq!(sent.text(), "", "no words, just the picture");
        assert_eq!(sent.images.len(), 1);
        assert_eq!(sent.images[0].path, "shot.png");
        assert!(
            app.chat.attachments().is_empty(),
            "the attachment went with the send"
        );
        assert!(app.busy(), "and the root is running it");

        let text = shot(&mut app, 120, 32).text();
        assert!(text.contains("you ›"), "the speaker mark: {text}");
        assert!(text.contains("▣ shot.png (png · 8 B)"), "{text}");
    }

    /// The vision gate holds at the wire, not only at the box: a model can be
    /// switched (`Ctrl-P`) between the attachment and the `Enter` that sends
    /// it, and a model mush does not know to see must not be handed image
    /// parts for the endpoint to reject — a turn and the human's money. The
    /// refusal changes nothing, so the words and the picture are still there.
    #[test]
    fn a_model_switched_after_the_attach_is_refused_at_the_wire() {
        let (mut app, _rx) = test_app("switch-model");
        let_the_model_see(&mut app);
        app.chat.attach(image("shot.png"));
        app.chat.insert("look at this");

        app.cell.edit(|cfg| cfg.set_model("test-model"));
        app.send_message();

        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "nothing that did not run is in the conversation"
        );
        assert_eq!(
            app.chat.input().text(),
            "look at this",
            "the words went back"
        );
        assert_eq!(app.chat.attachments().len(), 1, "and so did the picture");
        let (line, kind) = app.status_line().expect("the refusal");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("test-model"), "the model is named: {line}");
        assert!(line.contains("Ctrl-P"), "and the road is: {line}");
    }

    /// The delivered message carries its images, on both roads: a root run
    /// carries them in the conversation it is sent, and a nudge — the human
    /// steering a running agent with a screenshot — is the same user message in
    /// the actor's mailbox.
    #[test]
    fn a_delivered_message_carries_its_images() {
        let (mut app, _rx) = test_app("deliver-images");
        let_the_model_see(&mut app);
        // The pictures name real files holding their own bytes: a picture whose
        // file the receiving workspace reads back byte-identically is carried
        // as the human named it, a copy is only for one it cannot read.
        std::fs::write(app.ws.root().join("root.png"), png(0)).unwrap();
        std::fs::write(app.ws.root().join("child.png"), png(0)).unwrap();

        app.chat.attach(image("root.png"));
        app.chat.insert("look");
        app.send_message();
        let sent = app.chat.transcript(AgentId::ROOT).last().unwrap();
        assert_eq!(sent.text(), "look");
        assert_eq!(sent.images.len(), 1, "the root's run carries the picture");
        assert_eq!(sent.images[0].path, "root.png");

        let (cmd, mailbox) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd,
        });
        app.tree.focus(AgentId(1));
        app.chat.attach(image("child.png"));
        app.chat.insert("steer");
        app.send_message();
        match mailbox.try_recv() {
            Ok(AgentMsg::Nudge(message)) => {
                assert_eq!(message.text(), "steer");
                assert_eq!(message.images.len(), 1, "a nudge is a user message too");
                assert_eq!(message.images[0].path, "child.png");
            }
            other => panic!("a nudge with an image: {other:?}"),
        }
        assert_eq!(
            app.chat
                .transcript(AgentId(1))
                .last()
                .map(|message| message.images.len()),
            Some(1),
            "and the pane shows the same message"
        );
    }

    /// A send that does not land puts the words *and* the attachments back:
    /// the unit of "the human's message" is both, and neither should have to be
    /// pasted again.
    #[test]
    fn a_refused_send_restores_the_words_and_the_attachments() {
        let (mut app, _rx) = test_app("refused-send");
        app.cell.edit(|cfg| cfg.model.clear());
        app.chat.attach(image("shot.png"));
        app.chat.insert("look at this");
        app.send_message();

        assert_eq!(
            app.chat.input().text(),
            "look at this",
            "the words went back"
        );
        assert_eq!(app.chat.attachments().len(), 1, "and so did the picture");
        assert_eq!(app.chat.attachments()[0].path, "shot.png");
        let (line, kind) = app.status_line().expect("the refusal");
        assert_eq!(kind, StatusKind::Error);
        assert!(line.contains("no model"), "{line}");
        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "nothing that did not run is in the conversation"
        );
    }

    /// The painted rows name the image in both surfaces and the same way: the
    /// box's attachment rows and the transcript's row under the words are the
    /// one label builder, so a picture reads alike before and after it is sent.
    #[test]
    fn the_box_and_the_transcript_name_an_attached_image() {
        let (mut app, _rx) = test_app("paint-image");
        let_the_model_see(&mut app);
        std::fs::create_dir_all(app.ws.root().join("shots")).unwrap();
        std::fs::write(app.ws.root().join("shots/a.png"), png(340_000)).unwrap();
        app.update(Msg::Paste("shots/a.png".into()));

        let label = "shots/a.png (png · 340 KB)";
        let Screen::Panes(panes) = app.screen(Rect::new(0, 0, 120, 32)) else {
            panic!("a terminal with room for the panes");
        };
        let input = panes.chat.input.as_ref().expect("the message box");
        assert_eq!(input.attachments, vec![format!("▣ {label}")]);
        assert_eq!(input.attachment_count, 1);
        assert_eq!(input.cursor_row, 0, "the cursor's line is below the row");

        // And the frame really paints it there, with the title counting it.
        let text = shot(&mut app, 120, 32).text();
        assert!(
            text.contains(&format!("▣ {label}")),
            "the box paints the attachment row: {text}"
        );
        assert!(text.contains("message · 1 image"), "{text}");

        // Sent, the transcript names it the same way.
        app.chat.insert("look");
        app.send_message();
        let text = shot(&mut app, 120, 32).text();
        assert!(text.contains("you › look"), "{text}");
        assert!(
            text.contains(&format!("▣ {label}")),
            "the transcript names it too: {text}"
        );
    }

    /// Press Backspace the way the app does: through the pure key table and
    /// the arms.
    fn backspace(app: &mut App) {
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));
    }

    /// Enter sends; Shift+Enter and Alt+Enter start a new line instead, so a
    /// multi-line message can be written before it goes.
    #[test]
    fn a_modified_enter_starts_a_new_line_a_plain_one_sends() {
        let (mut app, _rx) = test_app("multiline-key");
        app.focus = Focus::Chat;
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, modifiers)));
        }
        assert_eq!(app.chat.input().text(), "\n\n");
        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "a modified Enter must not send"
        );
    }

    /// A streamed message is the message path, and the message path must build
    /// nothing: the debounce is what turns a burst of them into one write.
    #[test]
    fn a_streamed_message_hands_no_snapshot_to_the_writer() {
        let (mut app, recorder) = app_recording("streamed");
        for i in 0..5 {
            streamed(&mut app, &format!("m{i}"));
        }
        assert_eq!(
            recorder.len(),
            0,
            "the handler rebuilt nothing: a tool result costs no snapshot"
        );
        assert!(
            app.session_dirty_at.is_some(),
            "it did mark the conversation dirty, though"
        );

        // The debounce elapsed, the tick pays for all five at once — with the last of
        // them, which is the state a restart must resume from.
        age_session(&mut app, SESSION_DEBOUNCE);
        app.tick();
        let saved = recorder.saved();
        assert_eq!(saved.len(), 1, "one snapshot for the burst");
        assert_eq!(saved[0].messages.len(), 5);
        assert_eq!(saved[0].messages[4].text(), "m4");
        assert!(
            app.session_dirty_at.is_none(),
            "and the file is current again"
        );
    }

    /// The debounce is a minute, and the number is the decision: a long run
    /// pays one whole-snapshot rebuild per minute instead of one per second,
    /// and a crash costs at most that minute of machine-generated chat. Pinned
    /// here so a later tweak cannot quietly move the boundary the doc describes.
    #[test]
    fn the_session_debounce_is_a_minute() {
        assert_eq!(SESSION_DEBOUNCE, Duration::from_secs(60));
    }

    /// The whole point of the debounce, end to end: a burst of root messages
    /// costs one write, and the file it leaves holds the last of them.
    #[test]
    fn a_burst_of_messages_ends_in_one_write_holding_the_last_of_them() {
        let root = dir("burst");
        let (mut app, writer) = app_writing(&root);
        for i in 0..5 {
            streamed(&mut app, &format!("m{i}"));
        }
        assert!(
            !session::session_path(&root).exists(),
            "no message wrote a file"
        );

        age_session(&mut app, SESSION_DEBOUNCE);
        app.tick();
        // Wait for the writer rather than for a clock: the hand-over is the
        // UI thread's, and the write is the writer's.
        writer.flush();
        assert_eq!(writer.writes(), 1, "five messages, one write");
        let stored = Session::load(&root).expect("the burst reached the disk");
        assert_eq!(stored.messages.len(), 5);
        assert_eq!(stored.messages.last().unwrap().text(), "m4");
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The human's own message is on disk before the send returns: the run it
    /// starts may take minutes, and a crash in it must not lose the request.
    /// This is the boundary the debounce is *not* allowed to cover.
    #[test]
    fn a_sent_message_is_on_disk_before_the_send_returns() {
        let root = dir("send");
        let (mut app, _writer) = app_writing(&root);
        app.chat.insert("please port the parser");
        app.send_message();

        let stored = Session::load(&root).expect("the send flushed it");
        assert_eq!(
            stored.messages.last().map(|message| message.text()),
            Some("please port the parser")
        );
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A turn typed at a child is the human's own words too, so it crosses the
    /// same boundary the root's does: written before the send returns, not left
    /// to the debounce. A crash a minute later must not bring the child back
    /// having never been told the thing it was asked for.
    #[test]
    fn a_message_to_a_child_is_on_disk_before_the_send_returns() {
        let root = dir("send-child");
        let (mut app, _writer) = app_writing(&root);
        let (child_tx, _child_rx) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd: child_tx,
        });
        app.tree.focus(AgentId(1));
        app.chat.insert("carry on with the lexer");
        app.send_message();

        let stored = Session::load(&root).expect("the send flushed it");
        let child = stored
            .agents
            .iter()
            .find(|agent| agent.id == 1)
            .expect("the child is in the file");
        assert_eq!(
            child.messages.last().map(|message| message.text()),
            Some("carry on with the lexer")
        );
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The shipped default window is not the human's: a session budgeted on it
    /// must store `context: null`, or the next launch would read a default
    /// nobody stated back as a statement — and no endpoint could ever teach
    /// mush a smaller window for that workspace again. Only an explicit window
    /// is worth remembering.
    #[test]
    fn a_default_window_is_never_stored_as_if_it_were_stated() {
        let root = dir("default-context");
        let (mut app, _writer) = app_writing(&root);
        app.cell.edit(|cfg| {
            cfg.provider = Provider::DeepSeek;
            cfg.rederive_context();
        });
        assert_eq!(app.cfg().context_tokens, 120_000, "the shipped default");
        assert!(!app.cfg().context_explicit);

        app.chat.insert("hello");
        app.send_message();

        let stored = Session::load(&root).expect("the send flushed it");
        assert_eq!(stored.context, None, "a guess is not a statement");
        assert_eq!(stored.model, app.cfg().model);
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A quit kills the agents' process groups (finding H9), so the human who
    /// quits is told what that costs before it happens: two runs were lost to a
    /// `Ctrl-Q` that said nothing. The first press arms and names them; the
    /// second, while the line is still there, is the quit.
    #[test]
    fn a_live_run_arms_the_quit_and_names_what_dies() {
        let (mut app, _rx) = test_app("quit-armed");
        app.chat.insert("do the thing");
        app.send_message();
        assert_eq!(app.tree.agents[0].phase, Phase::Thinking, "the root runs");

        ctrl(&mut app, 'q');

        assert!(!app.should_quit, "the first Ctrl-Q warns, it does not quit");
        assert_eq!(
            text_of(&app),
            "Ctrl-Q again quits · kills #0 thinking",
            "the line names the agent and what it is doing"
        );

        ctrl(&mut app, 'q');

        assert!(app.should_quit, "the second one is the quit");
    }

    /// A signal is not a key: it cannot be pressed twice, so it does not wait
    /// for a second press. The handler sets the flag, the loop's step reads it
    /// and quits — with live work, and no arm in between (finding E1).
    ///
    /// This is the one test in the binary that raises one of the three signals,
    /// deliberately: once the flag is set, a second signal would take the
    /// conditional default road and kill the test process (see `signals`).
    #[test]
    fn a_signal_takes_the_quit_road_without_the_arming_press() {
        let _signals = crate::signals::install().expect("the handlers install");
        let (mut app, _rx) = test_app("quit-signal");
        app.chat.insert("do the thing");
        app.send_message();
        assert!(app.busy(), "there is work a quit would kill");

        ctrl(&mut app, 'q');
        assert!(!app.should_quit, "Ctrl-Q warns first");
        assert!(app.quit_armed(), "and the warning is on the line");

        signal_hook::low_level::raise(signal_hook::consts::SIGTERM).expect("the signal is raised");
        assert!(
            crate::take_signal_quit(&mut app),
            "the handler set the flag, and the loop's step reads it"
        );
        assert!(app.should_quit, "and the signal quits at once");
    }

    /// A detached job is work in flight too, and it is a process group the old
    /// silent quit left running (finding S4/H9): the line counts it beside the
    /// agent whose jobs they are — an agent at rest whose `cargo build` is not.
    #[test]
    fn a_detached_job_is_counted_in_the_quit_warning() {
        use crate::jobs::Launch;
        use crate::machine::fake::{Script, Scripted as ScriptedMachine};
        use crate::machine::{Machine, ShellCommand};

        let (mut app, _rx) = test_app("quit-job");
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let job = machine
            .spawn(&ShellCommand {
                command: "cargo build",
                root: std::path::Path::new("/tmp"),
            })
            .unwrap();
        let (tx, job_rx) = crossbeam_channel::unbounded();
        app.tree
            .handles()
            .jobs
            .launch(Launch::started(
                0,
                "cargo build".to_string(),
                false,
                tx,
                job,
            ))
            .unwrap();
        assert!(app.busy(), "a job is work in flight");

        ctrl(&mut app, 'q');

        assert!(
            !app.should_quit,
            "a running job is not nothing to warn about"
        );
        assert_eq!(
            text_of(&app),
            "Ctrl-Q again quits · kills #0 idle + 1 job",
            "the agent is at rest, its job is not"
        );

        ctrl(&mut app, 'q');
        assert!(app.should_quit, "and the second press still quits");

        // The quit is the one that kills, as it always was: `Drop` is the only
        // killer and the job's own thread reports the stop.
        drop(app);
        assert!(matches!(
            job_rx.recv_timeout(Duration::from_secs(5)),
            Ok(AgentMsg::CommandDone { .. })
        ));
    }

    /// A stopped agent that still owns a running job is named as stopped, not
    /// as idle: the agent's own state and the job beside it are two facts, and
    /// the line that warns what a quit kills must not deny the first one
    /// (findings §6, refactor R22).
    #[test]
    fn a_stopped_agent_over_a_live_job_is_named_as_stopped() {
        use crate::jobs::Launch;
        use crate::machine::fake::{Script, Scripted as ScriptedMachine};
        use crate::machine::{Machine, ShellCommand};

        let (mut app, _rx) = test_app("quit-stopped-job");
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let job = machine
            .spawn(&ShellCommand {
                command: "cargo build",
                root: std::path::Path::new("/tmp"),
            })
            .unwrap();
        let (tx, _job_rx) = crossbeam_channel::unbounded();
        app.tree
            .handles()
            .jobs
            .launch(Launch::started(
                0,
                "cargo build".to_string(),
                false,
                tx,
                job,
            ))
            .unwrap();
        // The run was stopped; the command it left behind is not.
        app.tree.stopped(AgentId::ROOT);

        ctrl(&mut app, 'q');

        assert_eq!(
            text_of(&app),
            "Ctrl-Q again quits · kills #0 stopped + 1 job",
            "a stopped agent is not an idle one"
        );
    }

    /// The common quit kills nothing, so it stays one keystroke and stays
    /// silent: a warning every quit must read is a warning nobody reads.
    #[test]
    fn a_quit_with_nothing_live_is_still_one_key() {
        let (mut app, _rx) = test_app("quit-idle");
        assert!(!app.busy());

        ctrl(&mut app, 'q');

        assert!(app.should_quit, "one key, no confirmation step");
        assert_eq!(text_of(&app), "", "and nothing said about it");
        assert!(!app.quit_armed());
    }

    /// `/quit` is the same two-step quit in words. The letters that spell it
    /// are typing, not a change of mind, so they do not disarm what they are on
    /// their way to run — a command that armed itself off on the way in could
    /// never quit. `Ctrl-C` and the tree's own keys do disarm it.
    #[test]
    fn slash_quit_is_the_same_two_step_quit() {
        let (mut app, _rx) = test_app("quit-said");
        app.chat.insert("do the thing");
        app.send_message();
        assert!(app.busy());

        let typed = |app: &mut App, line: &str| {
            for letter in line.chars() {
                app.on_key(KeyEvent::new(KeyCode::Char(letter), KeyModifiers::NONE));
            }
            app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        };

        typed(&mut app, "/quit");
        assert!(!app.should_quit, "the typed quit warns first");
        assert!(app.quit_armed());

        typed(&mut app, "/quit");
        assert!(app.should_quit, "the second /quit is the quit");
    }

    /// `Ctrl-C` is the opposite of quitting, so it takes the arming back — and
    /// mush is not wedged by having been warned at: the stop lands, the next
    /// message reaches the agent, and the exit still flushes what the debounce
    /// had not.
    #[test]
    fn ctrl_c_disarms_the_quit_and_the_app_keeps_working() {
        let root = dir("quit-disarmed");
        let (mut app, _writer) = app_writing(&root);
        app.chat.insert("do the thing");
        app.send_message();
        assert_eq!(app.tree.agents[0].phase, Phase::Thinking);

        ctrl(&mut app, 'q');
        assert!(!app.should_quit);
        assert!(app.quit_armed());

        ctrl(&mut app, 'c');

        assert!(!app.should_quit, "Ctrl-C is not a quit");
        assert!(!app.quit_armed(), "and it takes the warning back");
        assert_eq!(text_of(&app), "", "the line goes with the arming");

        // The stop lands, and the next message starts a run the row shows.
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Stopped,
        });
        app.chat.insert("still here");
        app.send_message();
        assert_eq!(
            app.tree.agents[0].phase,
            Phase::Thinking,
            "the message reached the agent"
        );

        drop(app);

        let stored = Session::load(&root).expect("the exit flush wrote it");
        assert_eq!(
            stored.messages.last().map(|message| message.text()),
            Some("still here")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A line that cannot name everything *counts* what is left, and it fits one
    /// bar row at 80×24. A clipped list would read as a short one, and the human
    /// would quit believing they knew what it cost (finding H9).
    #[test]
    fn the_quit_warning_fits_the_bar_and_counts_the_rest() {
        let (mut app, _rx) = test_app("quit-many");
        let mut _mailboxes = Vec::new();
        for id in 1..=5u64 {
            let (cmd, rx) = crossbeam_channel::unbounded();
            _mailboxes.push(rx);
            app.tree.insert(Spawn {
                id: AgentId(id),
                parent: AgentId::ROOT,
                brief: format!("child {id}"),
                depth: 1,
                branch: None,
                fork: None,
                cmd,
            });
        }

        ctrl(&mut app, 'q');

        let warning = text_of(&app).to_string();
        assert!(
            warning.starts_with("Ctrl-Q again quits · kills #1 thinking, #2 thinking"),
            "{warning}"
        );
        assert!(
            warning.ends_with("+3 more"),
            "the rest are counted: {warning}"
        );
        assert!(warning.chars().count() <= 73, "one bar row: {warning}");

        // And the frame paints it whole, on the bar's own row: the row is 80
        // columns wide and the badge takes seven of them. Read through the
        // `Shot` harness, so the assertion is about a frame the app derived and
        // the frame's own shape is checked with it (refactor B17).
        let frame = shot(&mut app, 80, 24);
        frame.assert_shape("the quit warning", 80, 24);
        assert!(frame.line(22).contains(&warning), "{:?}", frame.line(22));

        ctrl(&mut app, 'q');
        assert!(app.should_quit, "the second press quits");
    }

    /// The line a stop key answers with when nothing runs names the half of
    /// `Ctrl-N` a human cannot undo. "starts a new chat" alone read as if only a
    /// beginning were at stake, while the key stops every agent, kills what they
    /// left running and drops every transcript — root and children (finding
    /// H26).
    ///
    /// It has to fit the bar's one row at 80×24, badge and the space after it
    /// included: the bar paints its line whole and does no width arithmetic of
    /// its own, so a longer warning would have its tail clipped — and the tail
    /// is where the transcripts are.
    #[test]
    fn the_nothing_running_line_names_what_the_new_chat_key_drops() {
        let (mut app, _rx) = test_app("nothing-running");
        ctrl(&mut app, 'x');

        let line = text_of(&app).to_string();
        assert_eq!(line, NOTHING_RUNNING);
        assert!(
            line.contains("Ctrl-N drops every transcript"),
            "the cost of the key, not just the new beginning: {line}"
        );
        assert!(
            line.chars().count() <= 73,
            "one bar row once the badge takes its seven columns: {line}"
        );

        // And the frame paints it whole on the bar's own row, which is where a
        // clipped tail would show.
        let frame = shot(&mut app, 80, 24);
        frame.assert_shape("nothing running", 80, 24);
        assert!(frame.line(22).contains(&line), "{:?}", frame.line(22));
    }

    /// The warning is composed from the tree, not remembered, so it follows it:
    /// work that ends while the human is deciding leaves the line, and a quit
    /// with nothing left to warn about stops being armed.
    #[test]
    fn the_armed_warning_follows_the_tree() {
        let (mut app, _rx) = test_app("quit-follows");
        let (cmd, _child_rx) = crossbeam_channel::unbounded();
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd,
        });
        app.chat.insert("do the thing");
        app.send_message();
        assert_eq!(app.working_agents().len(), 2, "the root and its child");

        ctrl(&mut app, 'q');
        assert_eq!(
            text_of(&app),
            "Ctrl-Q again quits · kills #0 thinking, #1 thinking"
        );

        // The child finishes while the human is deciding: the line names what
        // is running now, not what was.
        app.tree
            .finish(AgentId(1), Some("did the work".to_string()));
        app.tick();
        assert_eq!(text_of(&app), "Ctrl-Q again quits · kills #0 thinking");

        // Nothing is live any more, so there is nothing to warn about: the
        // warning goes, the arming goes, and one key quits again.
        app.tree.finish(AgentId::ROOT, Some("done".to_string()));
        app.tick();
        assert_eq!(text_of(&app), "", "the line went with the work");
        assert!(!app.quit_armed());

        ctrl(&mut app, 'q');
        assert!(app.should_quit);
    }

    /// A warning whose five seconds are up is gone, and with it the arming: the
    /// next `Ctrl-Q` is a fresh question (with a fresh warning), not the second
    /// half of a pair the human made six seconds ago.
    #[test]
    fn a_faded_warning_arms_again_rather_than_quitting() {
        let (mut app, _rx) = test_app("quit-faded");
        app.chat.insert("do the thing");
        app.send_message();
        assert!(app.busy());

        ctrl(&mut app, 'q');
        assert!(app.quit_armed());

        // Five seconds on: the line fades, and the arming fades with it.
        age_status(&mut app, 6);
        app.tick();
        assert_eq!(text_of(&app), "", "the warning is gone");
        assert!(!app.quit_armed());

        ctrl(&mut app, 'q');
        assert!(!app.should_quit, "a fresh warning, not a silent quit");
        assert!(app.quit_armed(), "with five seconds of its own");

        ctrl(&mut app, 'q');
        assert!(app.should_quit, "and the second press still quits");
    }

    /// Quitting writes what the debounce had not: the exit flush is what makes
    /// a crash cost one debounce of chat rather than everything since the last
    /// boundary.
    #[test]
    fn quitting_writes_the_messages_the_debounce_had_not() {
        let root = dir("quit");
        let (mut app, _writer) = app_writing(&root);
        streamed(&mut app, "the last thing said");
        assert!(
            !session::session_path(&root).exists(),
            "the debounce has not elapsed"
        );

        drop(app);

        let stored = Session::load(&root).expect("the exit flush wrote it");
        assert_eq!(
            stored.messages.last().map(|message| message.text()),
            Some("the last thing said")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Quitting kills every job, wherever it is in the tree. The `Drop` is the
    /// one way out of the event loop, and a build an agent started must not
    /// outlive a clean quit — the human's next `cargo build` would otherwise
    /// fight a ghost for the target directory.
    #[test]
    fn quitting_kills_the_jobs_the_agents_started() {
        use crate::jobs::Launch;
        use crate::machine::fake::{Script, Scripted as ScriptedMachine};
        use crate::machine::{Machine, ShellCommand};

        let (app, _rx) = test_app("jobs-die-on-quit");
        let machine = Arc::new(
            ScriptedMachine::new()
                .runs(Script::hangs())
                .runs(Script::hangs()),
        );
        let registry = app.tree.handles().jobs;
        let mut job_rx = Vec::new();
        // One job for the root, one for an agent that is not the root: the kill
        // is the registry's, so it is not the root's jobs that die but every
        // job in the tree.
        for owner in [0u64, 3] {
            let job = machine
                .spawn(&ShellCommand {
                    command: "cargo build",
                    root: std::path::Path::new("/tmp"),
                })
                .unwrap();
            let (tx, rx) = crossbeam_channel::unbounded();
            registry
                .launch(Launch::started(
                    owner,
                    "cargo build".to_string(),
                    false,
                    tx,
                    job,
                ))
                .unwrap();
            job_rx.push(rx);
        }
        assert_eq!(registry.running(), 2, "both jobs are live before the quit");

        drop(app);

        // The kill is synchronous: the process groups are gone before `Drop`
        // returns, not ten milliseconds later.
        assert_eq!(machine.kills(), 2, "quitting killed every job");
        for rx in &job_rx {
            assert!(
                matches!(
                    rx.recv_timeout(Duration::from_secs(5)),
                    Ok(AgentMsg::CommandDone { .. })
                ),
                "each job's own thread reported its stop"
            );
        }
        assert_eq!(registry.running(), 0, "nothing is left running");
    }

    /// Quitting kills the command an agent is *waiting on*, not only a detached
    /// job. A `run_command` without `detach` is spawned for the length of a tool
    /// call and used to be registered nowhere, so `kill_all` — the last thing
    /// `App::drop` does — could not see it: a plain `sleep 10; touch marker`
    /// survived a clean `Ctrl-Q`, in its own process group, and did its work
    /// after mush was gone (finding S4). The hold below is the same one the
    /// agent takes, and the quit is the same one the human's `Ctrl-Q` runs.
    #[test]
    fn quitting_kills_a_running_foreground_command() {
        use crate::machine::fake::{Script, Scripted as ScriptedMachine};
        use crate::machine::{Machine, ShellCommand};

        let (app, _rx) = test_app("foreground-dies-on-quit");
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let registry = app.tree.handles().jobs;
        let job = machine
            .spawn(&ShellCommand {
                command: "sleep 10; touch marker",
                root: std::path::Path::new("/tmp"),
            })
            .unwrap();
        let held = registry.hold(0, job);
        assert!(
            registry.holding_foreground(0),
            "the call holds it while the model waits on it"
        );
        assert_eq!(machine.kills(), 0, "and nothing has stopped it yet");

        drop(app);

        assert_eq!(
            machine.kills(),
            1,
            "quitting killed the command the agent was waiting on"
        );
        // The call is over, so its slot is gone: the registry is left holding
        // nothing, and this test keeps it alive precisely to check that.
        drop(held);
        assert!(
            !registry.holding_foreground(0),
            "a finished call leaves no slot behind"
        );
        registry.kill_all();
        assert_eq!(
            machine.kills(),
            1,
            "so the next quit cannot kill the same command twice"
        );
    }

    /// `c` on a row is aimed at the work, and a detached job is work in flight:
    /// an agent whose run ended while its `cargo bench` still runs is not idle
    /// on the machine. The row used to answer "not running" and do nothing,
    /// while Ctrl-C — which asks `working_agents`, the same question spelled
    /// once — stopped that very job. Two paths, one answer.
    #[test]
    fn c_on_an_idle_agent_still_stops_its_job() {
        use crate::jobs::Launch;
        use crate::machine::fake::{Script, Scripted as ScriptedMachine};
        use crate::machine::{Machine, ShellCommand};

        let (mut app, _rx) = test_app("cancel-job-row");
        let machine = Arc::new(ScriptedMachine::new().runs(Script::hangs()));
        let registry = app.tree.handles().jobs;
        let job = machine
            .spawn(&ShellCommand {
                command: "cargo bench",
                root: std::path::Path::new("/tmp"),
            })
            .unwrap();
        let (tx, job_rx) = crossbeam_channel::unbounded();
        registry
            .launch(Launch::started(
                0,
                "cargo bench".to_string(),
                false,
                tx,
                job,
            ))
            .unwrap();
        // The root is idle — and the row must not say so as if the machine were.
        assert!(!app.tree.agents[0].phase.is_busy());
        assert_eq!(app.live_jobs(AgentId::ROOT).len(), 1);
        assert!(app.busy(), "the tree has work in flight on the machine");

        app.tree.cursor_top();
        app.focus = Focus::Agents;
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
        )));

        assert!(
            !text_of(&app).contains("is not running"),
            "there is something to stop: {}",
            text_of(&app)
        );
        // Stopped for real: the Stop goes to the owner's actor, which kills what
        // it started, and the job's own thread reports it.
        match job_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(AgentMsg::CommandDone { line, .. }) => {
                assert!(line.contains("stopped after"), "{line}")
            }
            other => panic!("the job must be stopped: {:?}", other.is_ok()),
        }
        assert!(!app.busy(), "and the machine is free again");
    }

    /// Ctrl-N clears the stored conversation too, not just the visible one: the
    /// old chat coming back at the next start is exactly what the flush stops.
    #[test]
    fn a_new_chat_clears_the_stored_conversation() {
        let root = dir("new-chat");
        let (mut app, _writer) = app_writing(&root);
        app.chat.insert("something worth remembering");
        app.send_message();
        assert_eq!(Session::load(&root).unwrap().messages.len(), 1);

        new_chat(&mut app);

        let stored = Session::load(&root).expect("the command flushed it");
        assert!(stored.messages.is_empty(), "the old chat is not resumed");
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A session file mush cannot read is *said*, not silently dropped
    /// (finding S3). The human comes back to an empty screen; "empty" must not
    /// be the whole story, so the line names the file, the reason and where the
    /// only copy went — in the pane's foot, in `/notes`, and on the bar without
    /// opening anything. A workspace with nothing stored says none of it.
    #[test]
    fn an_unreadable_session_is_said_rather_than_dropped() {
        let (mut app, _rx) = test_app("unreadable-session");

        // Nothing stored: no line about a session, and the bar keeps its hint.
        assert!(
            app.chat.notices_for(AgentId::ROOT).next().is_none(),
            "an absent session is not news"
        );
        let rows = screen(&mut app, 200, 40);
        assert!(
            !rows.join("\n").contains("could not read"),
            "and nothing is said about one: {rows:?}"
        );

        let notice = "could not read .mush/session.json — expected value at line 1 column 2; \
                      kept as .mush/session.json.bak · starting a new conversation";
        app.session_unreadable(notice);

        // The bar says it without anything being opened: line one, in red,
        // after the focus badge — and the pane's foot carries it whole.
        let rows = screen(&mut app, 200, 40);
        assert!(
            rows.iter().any(|row| row.starts_with(" chat ")
                && row.contains("could not read .mush/session.json")),
            "the bar carries it: {rows:?}"
        );
        let painted = rows.join("\n");
        assert!(
            painted.contains("expected value at line 1 column 2"),
            "the reason is on screen: {painted}"
        );
        assert!(
            painted.contains("kept as .mush/session.json.bak"),
            "and where the only copy went: {painted}"
        );
        // `/notes` reads it back, so a line that wrapped or scrolled is still
        // readable in full.
        run(&mut app, "/notes");
        assert!(
            screen(&mut app, 200, 40)
                .join("\n")
                .contains("could not read .mush/session.json"),
            "the report holds it"
        );
        // It is news, not chatter: the human's next send does not take it away,
        // and the session is marked dirty so the next save carries it.
        assert!(
            !app.chat.dismiss_said(),
            "a send must not end what the workspace said about itself"
        );
        assert_eq!(
            app.chat
                .notices_for(AgentId::ROOT)
                .filter(|notice| notice.rank() == crate::app::chat::Rank::Alert)
                .count(),
            1,
            "and it ranks as a failure, so the foot and the bar cannot hide it"
        );
        assert_eq!(app.chat.stored_notices().len(), 1);
        let _ = std::fs::remove_dir_all(app.ws.root());
    }

    /// A compaction is what a restart resumes from, so the folded transcript is
    /// written before the event returns rather than waiting out the debounce.
    #[test]
    fn a_compaction_is_written_before_the_event_returns() {
        let root = dir("compact");
        let (mut app, _writer) = app_writing(&root);
        streamed(&mut app, "a long conversation");
        let conversation = app.tree.conversation();

        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Compact {
                in_run: false,
                summary: "porting the parser".to_string(),
            },
        });

        let stored = Session::load(&root).expect("the fold flushed it");
        assert_eq!(stored.messages.len(), 1, "the conversation is the summary");
        assert!(stored.messages[0].text().contains("porting the parser"));
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A background write's failure has no caller to return to, so it waits for
    /// a tick and is reported the way the flush path reports it: a workspace
    /// mush cannot write to must not look saved.
    #[test]
    fn a_failed_background_write_is_reported_on_a_tick() {
        use session_save::fake::Recorder;

        let recorder = Recorder::new().fails("no space left on device");
        let root = dir("failed-write");
        let mut app = app_root(&root, None, recorder).0;
        streamed(&mut app, "lost");

        // The debounce hands it over; the scripted write fails; the next tick is
        // where the UI can hear about it.
        age_session(&mut app, SESSION_DEBOUNCE);
        app.tick();
        app.tick();

        assert_eq!(
            app.status.as_ref().map(|status| status.kind),
            Some(StatusKind::Error)
        );
        assert!(
            text_of(&app).contains("could not save session"),
            "it says what failed: {}",
            text_of(&app)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// How long one frame costs on a session the size of a real one.
    ///
    /// `#[ignore]`d on purpose: the 16 ms budget is a property of an *idle* box,
    /// and this suite runs while sibling agents build on the same machine, so a
    /// bar this tight flakes and a gate that is green "usually" is not a gate
    /// (finding H6). Run it deliberately, alone, with
    /// `cargo test -- --ignored a_frame_fits`.
    ///
    /// When it is run, a frame that does not fit a 60 fps budget is felt as lag,
    /// so it is a regression guard as much as a measurement.
    #[test]
    #[ignore = "the 16 ms budget needs an idle box; run it alone with --ignored"]
    fn a_frame_fits_in_a_60fps_budget_on_a_long_transcript() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let (mut app, _rx) = test_app("perf");
        // Roughly what a long session looks like: hundreds of messages, with
        // multi-kilobyte tool results in among them.
        for i in 0..300 {
            app.chat
                .push_message(AgentId::ROOT, Message::user(format!("message {i}")));
            app.chat.push_message(
                AgentId::ROOT,
                Message::assistant("a reply a few words long"),
            );
            app.chat.push_message(
                AgentId::ROOT,
                Message::tool(format!("c{i}"), "x".repeat(2000)),
            );
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        // A frame is now two halves — `App` derives the `Screen` and the
        // painter paints it — and the budget is for *both*: a screen built in
        // 55 ms and painted in one is still 55 ms of lag.
        let area = Rect::new(0, 0, 120, 40);
        terminal
            .draw(|f| {
                let screen = app.screen(area);
                crate::ui::draw(f, &screen, &crate::theme::Theme::default())
            })
            .unwrap();
        const FRAMES: u32 = 30;
        let start = Instant::now();
        for _ in 0..FRAMES {
            let screen = app.screen(area);
            terminal
                .draw(|f| crate::ui::draw(f, &screen, &crate::theme::Theme::default()))
                .unwrap();
        }
        let per_frame = start.elapsed() / FRAMES;
        eprintln!("measured: {per_frame:?} per frame");
        assert!(
            per_frame < Duration::from_millis(16),
            "a frame must fit a 60 fps budget, took {per_frame:?}"
        );
    }

    /// The two lifetimes, read off the file. A run's failure belongs to the run
    /// and to the workspace it broke, so it is on disk and comes back at the
    /// next start; a line that answered a command answered a moment that is
    /// over by then, and restoring it out of context would say "git diff
    /// HEAD...mush/2" over a tree nobody was looking at.
    #[test]
    fn a_failure_is_stored_and_a_command_answer_is_not() {
        let root = dir("notes-file");
        let (mut app, _writer) = app_writing(&root);
        app.chat.note("git diff HEAD...mush/2");
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Error("no route to host".into()),
        });
        app.flush_session();
        drop(app);

        let stored = Session::load(&root).expect("the command flushed it");
        assert_eq!(stored.notices.len(), 1, "only the failure is stored");
        assert_eq!(stored.notices[0].agent, 0);
        assert_eq!(stored.notices[0].text, "no route to host");
        assert!(
            stored.notices[0].at > 0,
            "a stored line says when it happened, not only what happened"
        );

        let app = reopened(&root);
        let notes: Vec<&str> = app
            .chat
            .notices_for(AgentId::ROOT)
            .map(|notice| notice.text.as_str())
            .collect();
        assert_eq!(
            notes,
            vec!["no route to host"],
            "the restart kept the failure and dropped the diff line"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A typo'd command answers the moment the human typed it, so it is nothing
    /// more than a note for that moment: not an alert that outlives the moment,
    /// and not a line the session file writes down and a relaunch reads back. A
    /// *run's* failure is the opposite and stays stored — see
    /// [`a_failure_is_stored_and_a_command_answer_is_not`].
    ///
    /// The line names the command that lists the ones that do exist: a human
    /// who typed a name that is not one needs the list, and "unknown command"
    /// on its own left them to guess at it (finding D3).
    #[test]
    fn a_typoed_command_is_a_moment_not_a_stored_failure() {
        let root = dir("typo");
        let (mut app, _writer) = app_writing(&root);
        app.chat.insert("/hlep");
        app.send_message();

        let notice = app
            .chat
            .notices_for(AgentId::ROOT)
            .next()
            .expect("the typo is answered");
        assert_eq!(notice.text, "unknown command: /hlep — /help lists them");
        assert_eq!(
            notice.rank(),
            Rank::Said,
            "an informational line, not an alert the cap may never yield"
        );
        assert!(
            app.chat.stored_notices().is_empty(),
            "nothing about a typo is written to the session"
        );

        app.flush_session();
        let stored = Session::load(&root).expect("the flush wrote the file");
        assert!(
            stored.notices.is_empty(),
            "so the file records no such line: {:?}",
            stored.notices
        );
        drop(app);

        let app = reopened(&root);
        assert!(
            app.chat.notices_for(AgentId::ROOT).next().is_none(),
            "and a relaunch does not bring the typo back"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A typed name is unbounded — a paste reaches the box whole — and the bar
    /// paints its line whole, so the name is what the row's budget cuts: the
    /// half of the sentence that must survive is the one that says where the
    /// commands are (finding D3).
    #[test]
    fn an_over_long_typo_still_points_at_the_help() {
        let (mut app, _rx) = test_app("typo-paste");
        app.chat.insert(&format!("/{}", "x".repeat(400)));
        app.send_message();

        let notice = app
            .chat
            .notices_for(AgentId::ROOT)
            .next()
            .expect("the typo is answered");
        assert!(
            notice.text.ends_with(" — /help lists them"),
            "the way out is not what the row clips: {}",
            notice.text
        );
        assert!(
            notice.text.contains('…'),
            "the cut is visible: {}",
            notice.text
        );
        // The name got its whole budget and no more: `unknown command: ` and
        // the tail are 17 and 19 columns of the row, so the line is exactly
        // what the two named numbers say it is.
        assert_eq!(notice.text.chars().count(), UNKNOWN_NAME_COLUMNS + 36);
    }

    /// A fresh run of an agent supersedes its stale failure: the row is derived
    /// from the phase and cannot lie, so the line that disagrees with it is the
    /// one that goes.
    #[test]
    fn a_completed_run_supersedes_an_older_failure() {
        let (mut app, _rx) = test_app("supersede");
        let conversation = app.tree.conversation();
        let send = |app: &mut App, id: AgentId, event: AgentEvent| {
            app.update(Msg::Agent {
                conversation,
                id,
                event,
            })
        };
        let notes = |app: &App| -> Vec<String> {
            app.chat
                .notices_for(AgentId::ROOT)
                .map(|notice| notice.text.clone())
                .collect()
        };

        send(
            &mut app,
            AgentId::ROOT,
            AgentEvent::Error("no route to host".into()),
        );
        assert_eq!(notes(&app), vec!["no route to host"]);

        // Failing twice in one run is still one failure: two lines would
        // disagree about which of them is current.
        send(
            &mut app,
            AgentId::ROOT,
            AgentEvent::Error("still no route".into()),
        );
        assert_eq!(notes(&app), vec!["still no route"]);

        // A child's run is not the root's business: the line is tagged with the
        // agent it concerns (finding B19).
        send(
            &mut app,
            AgentId(1),
            AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        assert_eq!(
            notes(&app),
            vec!["still no route"],
            "an agent that ran clears its own line and nobody else's"
        );

        // The run that supersedes it: the failure belonged to the attempt this
        // one replaced, and once it has started and finished there is nothing
        // left claiming the agent is broken.
        send(
            &mut app,
            AgentId::ROOT,
            AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        send(&mut app, AgentId::ROOT, AgentEvent::Done);
        assert!(
            notes(&app).is_empty(),
            "a completed run leaves no failure line behind: {:?}",
            notes(&app)
        );
    }

    /// The foot at the two sizes the audit names. 40×10 leaves the transcript
    /// pane one row, so the pane keeps that row and says the count in its
    /// title; 60×17 has room for two note rows and the count above them. Either
    /// way the number is the number of lines not painted, which is what makes
    /// it a decision and not a discovery.
    #[test]
    fn the_foot_caps_at_two_rows_and_counts_the_rest() {
        let (mut app, _rx) = test_app("foot-sizes");
        // The line is pushed straight in, without the box: a user message that
        // is not the echo of what the human sent is somebody else's — the
        // voice is `chat.rs`'s business and this test is about the foot.
        app.chat
            .push_message(AgentId::ROOT, Message::user("port the parser"));
        for index in 0..6 {
            app.chat.note_for(AgentId::ROOT, format!("note {index}"));
        }

        // 40×10: the pane is one row tall inside its border. The transcript
        // keeps it — the foot takes none, and the title says what is missing.
        let small = screen(&mut app, 40, 10);
        assert_eq!(small.len(), 10);
        assert!(
            small[4].contains("› port the parser"),
            "the one transcript row is the conversation, not a foot: {:?}",
            &small[3..6]
        );
        assert!(
            small[3].contains("+6 more lines") && small[3].contains("/notes"),
            "six note lines are hidden, the pane says so and names the way to read \
             them even though it has no count row to spend: {:?}",
            small[3]
        );
        assert!(
            !small.iter().any(|row| row.contains("  +6 more lines")),
            "but the count row itself needs a row the pane does not have: {small:?}"
        );

        // 60×17: two rows of notes, then one that says four lines are not
        // there. Six lines were written, two are painted, four are counted.
        let roomy = screen(&mut app, 60, 17);
        assert!(roomy[4].contains("› port the parser"), "{:?}", &roomy[3..9]);
        assert_eq!(roomy[5], "  +4 more lines · /notes", "{:?}", &roomy[3..9]);
        assert_eq!(roomy[6], "· note 4", "{:?}", &roomy[3..9]);
        assert_eq!(roomy[7], "· note 5", "{:?}", &roomy[3..9]);
        assert!(
            !roomy.iter().any(|row| row.contains("note 3")),
            "the cap is two rows, and the count is the rest: {:?}",
            &roomy[3..9]
        );
    }

    /// A tree bigger than its pane, which is the case the size tiers exist for:
    /// the compact strip wants two rows more than it has agents.
    fn crowd(app: &mut App, agents: u64) {
        let conversation = app.tree.conversation();
        for id in 1..=agents {
            app.update(Msg::Agent {
                conversation,
                id: AgentId::ROOT,
                event: AgentEvent::Spawned {
                    child: id,
                    parent: 0,
                    brief: format!("task {id}"),
                    depth: 1,
                    branch: None,
                    fork: None,
                    title: None,
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }
    }

    /// The bar keeps a row whatever else is on screen. In compact mode it was
    /// the trailing constraint behind a `Min(6)` chat, and at 40×10 the panes
    /// above it took the row it was owed: the frame painted the tree, the
    /// transcript and the message box, and the ` chat ` row — the focus badge,
    /// the key hint, and the only home a failure has — was simply not there.
    #[test]
    fn the_bar_keeps_a_row_on_the_shortest_terminals() {
        let (mut app, _rx) = test_app("bar-floor");
        crowd(&mut app, 3);
        // Failing to reach an agent's mailbox is a failure with no other home:
        // it is not a message, so it is never in the transcript. `crowd`'s
        // children are spawned with a receiver nobody holds, which is what a
        // *parked* actor looks like — and parking is the UI's own doing, so the
        // request would simply wake it (§8.21). A node the tree has no mailbox
        // for at all — a leftover worktree, an agent whose actor was never
        // started — is the one nothing can wake, and that is the failure this
        // reads for.
        app.tree.agent_tx.remove(&AgentId(1));
        app.tree.focus(AgentId(1));
        run(&mut app, "/compact");
        assert!(
            text_of(&app).contains("agent #1 is gone"),
            "the line the bar is supposed to carry: {}",
            text_of(&app)
        );

        for (width, height) in [(40u16, 10u16), (40, 11), (40, 12), (60, 12), (120, 12)] {
            let rows = screen(&mut app, width, height);
            assert_eq!(rows.len(), height as usize, "{width}x{height}");
            let bar = rows.last().unwrap();
            assert!(
                bar.contains(" chat ") && bar.contains("agent #1 is gone"),
                "the bar is missing its only row at {width}x{height}: {rows:?}"
            );
        }
    }

    /// Two children whose briefs open identically are two agents on the screen,
    /// named by what they were asked to make — without opening either (finding
    /// U6).
    #[test]
    fn the_row_names_an_agent_by_its_derived_title() {
        let (mut app, _rx) = test_app("agent-titles");
        let conversation = app.tree.conversation();
        for (id, path) in [(1u64, "deep.txt"), (2, "wide.txt")] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId::ROOT,
                event: AgentEvent::Spawned {
                    child: id,
                    parent: 0,
                    depth: 1,
                    brief: format!("create a file called {path} containing exactly: work"),
                    branch: None,
                    fork: None,
                    title: None,
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }
        let rows = screen(&mut app, 120, 32);
        assert!(
            rows.iter().any(|row| row.contains("#1 deep.txt")),
            "the row names the agent by what it was asked to make: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("#2 wide.txt")),
            "and its sibling by its own: {rows:?}"
        );
    }

    /// A title the caller gave wins over the handle derived from the brief; a
    /// spawn that was given none keeps the derived handle (finding U14).
    #[test]
    fn the_row_prefers_the_title_its_caller_gave() {
        let (mut app, _rx) = test_app("agent-given-titles");
        let conversation = app.tree.conversation();
        for (id, title) in [(1u64, Some("parser port")), (2, None)] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId::ROOT,
                event: AgentEvent::Spawned {
                    child: id,
                    parent: 0,
                    depth: 1,
                    brief: "create a file called deep.txt containing exactly: work".to_string(),
                    branch: None,
                    fork: None,
                    title: title.map(str::to_string),
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }
        let rows = screen(&mut app, 120, 32);
        assert!(
            rows.iter().any(|row| row.contains("#1 parser port")),
            "a named agent is exactly what its caller called it: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("#2 deep.txt")),
            "and a nameless one still gets its handle from the brief: {rows:?}"
        );
    }

    /// A job's handle is its own command on one line: a `command` can be a
    /// thirty-line heredoc with a `cd` in front of it, and the footer and the
    /// bar each have one row to name it in.
    #[test]
    fn a_job_is_named_by_its_command_on_one_line() {
        assert_eq!(job_title("cargo build --release"), "cargo build --release");
        assert_eq!(job_title("cd /w && cargo test -q"), "cargo test -q");
        assert_eq!(
            job_title("python3 - <<'PY'\nimport io\nprint('x')\nPY"),
            "python3 - <<'PY'"
        );
        assert!(job_title("").is_empty());
        assert!(job_title(&"x".repeat(80)).chars().count() <= JOB_TITLE_COLUMNS);
    }

    /// A failure is the third way a run can end, and it reaches the bar the way
    /// a stop does. `Stopped` said what happened and `Failed` did not, so the
    /// newest thing on screen could be a crash (`✗ #0` on the row, `! cannot
    /// reach …` in the foot) under a line advertising Ctrl-P. A loop-stop is
    /// this same event — the loop guard's complaint is the run's error — so one
    /// arm covers both endings.
    #[test]
    fn a_failure_reaches_the_bar_like_a_stop_does() {
        let (mut app, _rx) = test_app("failure-bar");
        let conversation = app.tree.conversation();
        let loop_stop = "the run was stopped as a loop: the same tool call repeated 5 times \
                         with nothing changed in between";
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Error(loop_stop.to_string()),
        });
        assert_eq!(
            app.tree.agents[0].phase,
            Phase::Failed(loop_stop.to_string())
        );
        let rows = screen(&mut app, 40, 10);
        let bar = rows.last().expect("the bar is painted");
        assert!(
            bar.contains("agent #0 failed"),
            "the smallest terminal still says what happened: {rows:?}"
        );

        // The stop that already worked, for comparison: the two endings are
        // told the same way.
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Stopped,
        });
        let rows = screen(&mut app, 40, 10);
        let bar = rows.last().expect("the bar is painted");
        assert!(
            bar.contains("agent #0 stopped"),
            "a stop says so too: {rows:?}"
        );
    }

    /// A run parked in a wait is not a model call, and it is not silent either:
    /// the transcript foot says what the wait is on, in the row's own words
    /// (`waiting on results.`), and never claims `working` while it is parked.
    ///
    /// Finding U7's ruling — `working` may not claim a model call that is not
    /// happening — stands. Its mechanism (paint nothing) is superseded: a line
    /// that says `waiting on results.` cannot be mistaken for a model call, and
    /// the old silence made the pane tell a human less than the row beside it,
    /// which has always named what it waits on. The `wait` noun is the same
    /// derivation on all three surfaces (finding U14's glyph rides it too).
    #[test]
    fn a_waiting_agent_is_not_drawn_working() {
        let (mut app, _rx) = test_app("waiting-foot");
        app.tree.begin(AgentId::ROOT, None);
        // The label the actor emits for `wait` with no arguments.
        app.tree.activity(AgentId::ROOT, "wait ");
        app.tree.age(AgentId::ROOT, Duration::from_secs(5));

        let rows = screen(&mut app, 120, 32);
        let painted = rows.join("\n");
        assert!(
            painted.contains("waiting on results."),
            "the foot says what the run waits on, in the row's own words: {rows:?}"
        );
        assert!(
            !painted.contains("working."),
            "and nothing claims a model call is in flight: {rows:?}"
        );
        let waiting = rows
            .iter()
            .find(|row| row.contains("waiting on results 5s"))
            .unwrap_or_else(|| panic!("the row says what it is waiting for: {rows:?}"));
        // The icon is the surface a glance reads, so it says the same thing the
        // words do: an hourglass, never the working `◐` (finding U14).
        assert!(waiting.contains("⧗ #0"), "{waiting:?}");
        assert!(!waiting.contains('◐'), "{waiting:?}");
        // And the count above it agrees: waiting is not working.
        assert!(rows[0].contains("1 waiting"), "{}", rows[0]);
        assert!(!rows[0].contains("working"), "{}", rows[0]);

        // A model that really has not answered still says so: the point is the
        // distinction, not the silence.
        app.tree.begin(AgentId::ROOT, None);
        app.tree.age(AgentId::ROOT, Duration::from_secs(2));
        let rows = screen(&mut app, 120, 32);
        let painted = rows.join("\n");
        assert!(
            painted.contains("thinking 2s"),
            "the row says the model has not answered: {rows:?}"
        );
        assert!(
            painted.contains("thinking."),
            "and the foot names that call the same way: {rows:?}"
        );
        assert!(rows[0].contains("1 working"), "{}", rows[0]);
    }

    /// `/notes` is the other half of the cap: the lines the foot ceded are read
    /// in full, oldest first, with the cursor on the newest.
    #[test]
    fn the_notes_command_lists_what_the_foot_could_not_show() {
        let (mut app, _rx) = test_app("notes-command");
        assert!(
            app.chat
                .notes_report(AgentId::ROOT, session::now_secs(), 74)
                .rows
                .is_empty(),
            "nothing has been written about a fresh conversation"
        );
        run(&mut app, "/notes");
        assert!(
            app.picker.is_none(),
            "and the command says so instead of opening an empty list"
        );
        assert_eq!(text_of(&app), "nothing written about #0 yet");

        for index in 0..5 {
            app.chat.note_for(AgentId::ROOT, format!("note {index}"));
        }
        run(&mut app, "/notes");
        let picker = app.picker.as_ref().expect("the lines mush wrote");
        assert!(
            matches!(picker.kind, PickerKind::Notes),
            "the popup is a reading, not a choice"
        );
        assert_eq!(
            picker.items.len(),
            5,
            "every note, not only the held-back ones"
        );
        assert_eq!(
            picker.items[0].label, "0s · note 0",
            "oldest first, like the pane"
        );
        assert_eq!(picker.cursor, 4, "the cursor opens on the newest");
    }

    /// A note longer than the popup must not drop the human mid-sentence: the
    /// list opens on the head of the newest note — the row that carries when it
    /// happened and how it started — not on its last row (finding T7).
    #[test]
    fn the_notes_popup_opens_on_the_head_of_the_newest_note() {
        let (mut app, _rx) = test_app("notes-head");
        // Old enough to have a stamp, short enough to be one row.
        app.chat.note_for(AgentId::ROOT, "opened notes.txt");
        // The newest note, long enough to wrap into several rows at the width
        // the popup is painted at.
        app.chat.note_for(
            AgentId::ROOT,
            "the run failed while folding the transcript: the endpoint returned 503 \
             for the third summarisation attempt, and the fold was abandoned with the \
             conversation left half-written, so read the tail of the transcript before \
             trusting anything above it",
        );

        let _ = screen(&mut app, 80, 24);
        run(&mut app, "/notes");
        let picker = app.picker.as_ref().expect("the lines mush wrote");
        assert!(
            picker.items.len() > 3,
            "the newest note wraps, which is the case this is about: {:?}",
            labels(&picker.items)
        );
        assert!(
            picker.cursor < picker.items.len() - 1,
            "the cursor is not on the last row, which is mid-sentence: {:?}",
            labels(&picker.items)
        );
        assert_eq!(
            picker.items[picker.cursor].label, "0s · the run failed while folding the",
            "it opens on the head of the newest note, stamp included"
        );
        assert!(
            picker.items[..picker.cursor]
                .iter()
                .any(|row| row.label.contains("opened notes.txt")),
            "and the older note is above it, not scrolled away: {:?}",
            labels(&picker.items)
        );
        let title = picker.title();
        assert!(
            title.contains(&format!(
                "line {}/{}",
                picker.cursor + 1,
                picker.items.len()
            )),
            "the title says where in the list the cursor is: {title}"
        );
    }

    /// `/notes` is the escape hatch for the lines the foot ceded, so it has to
    /// be readable at every size the popup is painted at. The report used to be
    /// wrapped at a fixed 74 — the popup's content width only at the widest size
    /// — so every row was clipped below 80 columns, and the first row clipped
    /// even on a 200-column terminal. The width has to come from the popup the
    /// frame actually paints, which is what `picker_text_width` says.
    #[test]
    fn the_notes_popup_wraps_to_the_width_it_is_painted_at() {
        let (mut app, _rx) = test_app("notes-wrap");
        app.chat.note(
            "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo \
             lima mike november oscar papa quebec romeo sierra tango",
        );

        // 60 columns: the popup is 40 wide and its list 34. A paint records the
        // width the way `main` does, then the command wraps its report to it.
        let _ = screen(&mut app, 60, 17);
        run(&mut app, "/notes");
        let narrow = screen(&mut app, 60, 17).join("\n");
        assert!(
            narrow.contains("tango"),
            "the whole note is read at 60 columns, not clipped: {narrow}"
        );

        // 200 columns: the widest popup, 74 content columns. The first row used
        // to overrun it by the age and marker that lead it.
        let _ = screen(&mut app, 200, 50);
        run(&mut app, "/notes");
        let wide = screen(&mut app, 200, 50).join("\n");
        assert!(wide.contains("tango"), "and at 200 columns: {wide}");
    }

    /// The empty state is a row like any other: wrapped to the pane and windowed
    /// to its height. Returned raw it was cut mid-word on a narrow pane, so the
    /// end of the instruction never appeared at all.
    #[test]
    fn the_empty_state_is_readable_on_a_narrow_pane() {
        let (mut app, _rx) = test_app("empty-state");
        let rows = screen(&mut app, 60, 17).join("\n");
        assert!(
            rows.contains("directly."),
            "the instruction wraps whole instead of clipping mid-word: {rows}"
        );
        assert!(
            rows.contains("Tab cycles panes · Enter sends"),
            "and the hints under it still get their rows: {rows}"
        );
    }

    /// V4: the bar's fallback is the painter's, not the derivation's. The test
    /// that moved into `screen.rs` stops at `bar_word(None, None)` being `None`,
    /// so nothing pinned the words a human reads when there is nothing to say —
    /// and the `/help` in them is how a human finds the commands at all.
    #[test]
    fn an_empty_bar_paints_the_hint() {
        let (mut app, _rx) = test_app("bar-hint");
        let frame = screen(&mut app, 120, 32).join("\n");
        assert!(
            frame.contains("/help"),
            "the bar's fallback names where the commands are: {frame}"
        );
    }

    /// V2: the size tiers are a fact about the layout, and nothing pinned the
    /// boundaries — a column or a row moved and every test stayed green. Below
    /// 80 columns or below 20 rows the panes stack; at 80×20 they sit side by
    /// side. The titles' rows are how the two layouts read from outside.
    #[test]
    fn the_size_tiers_paint_the_two_layouts() {
        let (mut app, _rx) = test_app("sweep-tiers");
        spawn_agent(&mut app, 1, 0, 1, "a task", None);
        let title_rows = |app: &mut App, width: u16, height: u16| {
            let rows = screen(app, width, height);
            let agents = rows.iter().position(|row| row.contains(" agents "));
            let chat = rows.iter().position(|row| row.contains(" mush "));
            (agents, chat)
        };

        // The width boundary: 79 columns stacks the tree over the chat, and 80
        // is the first width that holds both side by side.
        let (agents, chat) = title_rows(&mut app, 79, 24);
        assert!(
            agents.is_some() && chat.is_some() && agents != chat,
            "79 columns stacks the tree over the chat: {agents:?} vs {chat:?}"
        );
        let (agents, chat) = title_rows(&mut app, 80, 24);
        assert_eq!(
            agents, chat,
            "80×24 is the first width with room for both panes side by side"
        );
        // The height boundary, at a width wide enough to hold both: 19 rows
        // stacks, 20 is the first side-by-side height.
        let (agents, chat) = title_rows(&mut app, 80, 19);
        assert!(
            agents != chat,
            "19 rows stacks them: {agents:?} vs {chat:?}"
        );
        let (agents, chat) = title_rows(&mut app, 80, 20);
        assert_eq!(
            agents, chat,
            "80×20 is the first height with room for both panes side by side"
        );
        // A narrow terminal stays stacked however tall it grows — the width
        // edge dominates, so 60×20 must not flip on height alone.
        let (agents, chat) = title_rows(&mut app, 60, 19);
        assert!(agents != chat, "60×19 stacks them: {agents:?} vs {chat:?}");
        let (agents, chat) = title_rows(&mut app, 60, 20);
        assert!(
            agents != chat,
            "a narrow terminal stacks them at any height: {agents:?} vs {chat:?}"
        );
        let (agents, chat) = title_rows(&mut app, 100, 30);
        assert_eq!(agents, chat, "and wider too");
    }

    /// A send ends the chatter the last command left in the foot: a command's
    /// answer belongs to the moment the human typed *before* this message, and
    /// leaving it there spends the pane's rows on a question nobody is asking
    /// any more (finding U8). A failure is not a moment and survives the send.
    #[test]
    fn sending_the_next_message_ends_the_last_command_answer() {
        let (mut app, _rx) = test_app("chatter-send");
        app.chat.note_for(AgentId::ROOT, "opened notes.txt");
        app.chat.note_error_for(AgentId::ROOT, "first failure");
        assert_eq!(
            app.chat.notices_for(AgentId::ROOT).count(),
            2,
            "the bar line and the pane's list both exist"
        );

        app.chat.insert("carry on");
        app.send_message();

        let left: Vec<&str> = app
            .chat
            .notices_for(AgentId::ROOT)
            .map(|notice| notice.text.as_str())
            .collect();
        assert_eq!(
            left,
            vec!["first failure"],
            "the chatter went and the failure stayed: {left:?}"
        );
        assert!(
            app.chat
                .transcript(AgentId::ROOT)
                .iter()
                .any(|message| message.role == "user"),
            "and the message itself was still sent"
        );
    }

    /// `/notes` reads the chatter instead of superseding it: dismissing on the
    /// way in would make the pane's own `+N more · /notes` point at nothing,
    /// which is the half of finding U8 the command exists to answer.
    #[test]
    fn asking_for_the_notes_does_not_throw_them_away() {
        let (mut app, _rx) = test_app("chatter-notes");
        app.chat.note_for(AgentId::ROOT, "opened notes.txt");

        app.chat.insert("/notes");
        app.send_message();

        assert!(app.picker.is_some(), "the list opened");
        assert_eq!(
            app.chat.notices_for(AgentId::ROOT).count(),
            1,
            "and it had the line it came to read"
        );
    }

    /// The clock, on the tick that already expires the bar's own transient line:
    /// a hint read once and then left on the screen must not outlive its moment,
    /// because the agent it answers may never run again (finding U8).
    #[test]
    fn a_transient_line_leaves_the_foot_without_a_run() {
        let (mut app, _rx) = test_app("chatter-clock");
        app.chat.note_for(AgentId::ROOT, "opened notes.txt");
        app.chat.note_error_for(AgentId::ROOT, "no route to host");

        app.tick();
        assert_eq!(
            app.chat.notices_for(AgentId::ROOT).count(),
            2,
            "a line inside its moment is still there"
        );

        app.chat.age_notices(600);
        app.tick();

        let left: Vec<&str> = app
            .chat
            .notices_for(AgentId::ROOT)
            .map(|notice| notice.text.as_str())
            .collect();
        assert_eq!(
            left,
            vec!["no route to host"],
            "the tick took the chatter and left the failure: {left:?}"
        );
    }

    fn test_app(label: &str) -> (App, Receiver<Msg>) {
        let root = std::env::temp_dir().join(format!("mush-app-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        app_root(&root, None, session_save::fake::Recorder::new())
    }

    /// Point the home-config *write* at a throwaway file, once per test binary.
    ///
    /// `pick` persists what the human picked (`persist_user_config`), and the
    /// two picker tests below are the only places in this suite that press
    /// `Enter` on a row: without this they would rewrite the human's own
    /// `~/.config/mush/config.json` with a fixture's endpoint and model.
    /// `MUSH_CONFIG` is the override `mush_core::userconfig` documents for
    /// exactly this, and nothing else here reads the home config.
    fn isolate_user_config() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let path = std::env::temp_dir().join(format!(
                "mush-user-config-{}/config.json",
                std::process::id()
            ));
            std::env::set_var("MUSH_CONFIG", path);
        });
    }

    /// Ctrl-N must do what Ctrl-N does: a cleared chat with a live root, not
    /// an empty pane over a stale conversation.
    #[test]
    fn ctrl_n_restarts_the_root_and_clears_the_conversation() {
        let (mut app, _rx) = test_app("new");
        app.chat
            .push_message(AgentId::ROOT, Message::user("an old task"));
        app.chat.note_for(AgentId::ROOT, "old noise");
        let before = app.cell.handle();

        new_chat(&mut app);

        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "the conversation is gone"
        );
        assert!(
            app.chat.notices_for(AgentId::ROOT).next().is_none(),
            "notices are gone"
        );
        assert_eq!(app.tree.agents.len(), 1, "the tree is reset to the root");
        assert!(!app.busy());
        // The respawned root owns a fresh config cell and the UI adopted its
        // handle; without that, a later /model would write the old tree's cell
        // and the live actor would never hear about it.
        assert!(
            !before.same_cell(&app.cell.handle()),
            "the UI moved to the new tree's cell"
        );
        app.cell.edit(|cfg| cfg.set_model("after-the-restart"));
        assert_ne!(
            before.config().unwrap().model,
            "after-the-restart",
            "and the old tree's cell is no longer the one it writes"
        );

        // Its mailbox is alive, so the next message starts a run instead of
        // reporting that the root is gone.
        app.chat.insert("hello");
        app.send_message();
        assert!(app.busy());
        assert_eq!(app.tree.agents[0].phase, Phase::Thinking);
    }

    /// The audit's C4: `Ctrl-N` used to replace the store with an empty one from
    /// one keystroke — the key unarmed, no copy kept, and the only warning
    /// arriving *after* the loss. The first press now says what would go and
    /// where it is kept; only the second clears, and it keeps the conversation
    /// beside the store first.
    #[test]
    fn a_new_chat_keeps_the_old_conversation_and_arms_the_key() {
        let root = dir("new-chat-keeps");
        let (mut app, _writer) = app_writing(&root);
        app.chat
            .push_message(AgentId::ROOT, Message::user("a conversation worth keeping"));
        app.flush_session();
        let previous = root.join(".mush/session.json.previous");

        ctrl(&mut app, 'n');

        // The first press costs nothing: the message is still in the store,
        // nothing has been copied yet, and the bar asks again — naming what
        // would go and where the copy would be.
        assert_eq!(
            Session::load(&root)
                .expect("the store is still there")
                .messages
                .len(),
            1,
            "the first press drops nothing"
        );
        assert!(!previous.exists(), "and copies nothing");
        assert!(
            text_of(&app).contains("Ctrl-N again"),
            "the bar is waiting for the second key: {}",
            text_of(&app)
        );
        assert!(
            text_of(&app).contains(".mush/session.json.previous"),
            "the line says where the copy is kept: {}",
            text_of(&app)
        );

        ctrl(&mut app, 'n');

        // The second press is the one that costs, and it costs a reclamation:
        // the copy holds what the live store held a keystroke ago.
        let kept = std::fs::read_to_string(&previous).expect("the copy is on disk");
        assert!(
            kept.contains("a conversation worth keeping"),
            "the copy is the conversation: {kept}"
        );
        let stored = Session::load(&root).expect("the empty chat is what is live now");
        assert!(stored.messages.is_empty(), "the live store is the new chat");
        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "and the pane agrees with it"
        );
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The audit's C4 copy is the store's file, not the box's: `Ctrl-N` keeps
    /// the cleared conversation at `.mush/session.json.previous` through the
    /// same `atomic_write` the store uses, told the same
    /// [`mush_core::workspace::Fresh::Private`], so the copy comes out `0600`
    /// whatever the human's umask says about new files. The contents and the
    /// arm are `a_new_chat_keeps_the_old_conversation_and_arms_the_key`'s
    /// fact; this names the mode, so the next merge that touches the call
    /// cannot make the conversation group- or world-readable again.
    #[test]
    fn the_new_chat_copy_is_private_like_the_store() {
        use std::os::unix::fs::PermissionsExt;

        let root = dir("new-chat-private");
        let (mut app, _writer) = app_writing(&root);
        app.chat
            .push_message(AgentId::ROOT, Message::user("a conversation worth keeping"));
        app.flush_session();

        new_chat(&mut app);

        let previous = root.join(".mush/session.json.previous");
        let mode = std::fs::metadata(&previous)
            .expect("the copy is on disk")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            mode, 0o600,
            "the copy is the human's alone, like the store it mirrors"
        );
        drop(app);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The twin: a conversation with nothing in it is cleared by one press —
    /// the key must not become slower for the state it is for.
    #[test]
    fn an_empty_chat_is_cleared_by_one_press_unarmed() {
        let root = dir("new-chat-empty");
        let (mut app, _writer) = app_writing(&root);

        ctrl(&mut app, 'n');

        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "one press was the whole key"
        );
        assert!(
            !text_of(&app).contains("Ctrl-N again"),
            "nothing was armed for a conversation with nothing to lose: {}",
            text_of(&app)
        );
        assert_eq!(text_of(&app), "new chat — agents stopped, root restarted");
        assert!(
            !root.join(".mush/session.json.previous").exists(),
            "and nothing was copied"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Another key takes the new chat's arm back, the way it takes the quit's
    /// (finding C4): the warning belongs to the moment it was said, and the
    /// next `Ctrl-N` is a fresh first press. If the arm outlived the key that
    /// moved on, the second `Ctrl-N` would clear the conversation here.
    #[test]
    fn another_key_takes_the_new_chat_arm_back() {
        let (mut app, _rx) = test_app("new-chat-disarm");
        app.chat
            .push_message(AgentId::ROOT, Message::user("worth keeping"));

        ctrl(&mut app, 'n');
        assert!(
            text_of(&app).contains("Ctrl-N again"),
            "the first press arms: {}",
            text_of(&app)
        );

        ctrl(&mut app, 't'); // a key that moves on
        assert!(
            !text_of(&app).contains("Ctrl-N again"),
            "and the next key takes the arm back: {}",
            text_of(&app)
        );
        ctrl(&mut app, 'n');
        assert_eq!(
            app.chat.transcript(AgentId::ROOT).len(),
            1,
            "a fresh first press does not clear the conversation"
        );
    }

    /// Ctrl-N has to kill what the old tree left running on the machine, and it
    /// cannot rely on the registry's own `Drop` to do it: a running job's watch
    /// thread holds an `Arc<Registry>` (`jobs.rs`'s `Drop` docs), so the
    /// registry, its backstop and the process group all outlive the tree that
    /// started them — an old build fighting the new chat's for the target
    /// directory. `stop_all` is not enough either: it reaches only *live*
    /// actors, and the job's owner here is one whose actor is already gone.
    #[test]
    fn ctrl_n_kills_the_jobs_the_old_tree_left_running() {
        let (mut app, _rx) = test_app("new-chat-job");
        // A node whose actor is not there to hear the Shutdown (the helper
        // drops its receiver), owning a live job.
        spawn_agent(&mut app, 1, 0, 1, "a task", None);
        let machine = running_job_on(&mut app, 1);
        assert_eq!(
            app.tree.handles().jobs.running(),
            1,
            "the old tree's registry knows it"
        );

        new_chat(&mut app);

        assert_eq!(
            machine.kills(),
            1,
            "the old tree's job is killed as the chat is replaced"
        );
        assert_eq!(
            app.tree.handles().jobs.running(),
            0,
            "and the new tree starts with nothing running"
        );
    }

    /// `Ctrl-T` flips what the chat pane paints — the endpoint's own reasoning
    /// above the turn it decided — and `Ctrl-N` leaves the choice as the human
    /// made it: the toggle is a view, not a fact about the conversation.
    #[test]
    fn ctrl_t_shows_and_hides_the_reasoning_and_ctrl_n_keeps_the_choice() {
        let (mut app, _rx) = test_app("reasoning");
        app.chat.push_message(
            AgentId::ROOT,
            Message {
                reasoning_content: Some("weighing the greeting".into()),
                ..Message::assistant("hello")
            },
        );
        assert!(
            chat_rows(&mut app)
                .iter()
                .any(|row| row.contains("⋯ weighing the greeting")),
            "shown by default: {:?}",
            chat_rows(&mut app)
        );

        ctrl(&mut app, 't');
        assert!(!app.chat.shows_reasoning());
        assert!(
            !chat_rows(&mut app)
                .iter()
                .any(|row| row.contains("weighing the greeting")),
            "one toggle hides it in the pane: {:?}",
            chat_rows(&mut app)
        );

        ctrl(&mut app, 't');
        assert!(
            app.chat.shows_reasoning(),
            "and the same key brings it back"
        );

        // Ctrl-N starts a new chat, not a new preference.
        ctrl(&mut app, 't');
        assert!(!app.chat.shows_reasoning());
        ctrl(&mut app, 'n');
        assert!(
            !app.chat.shows_reasoning(),
            "the human's view outlives the chat it was set in"
        );
    }

    /// Zen's whole promise, at the ubiquitous 80×24 and on a wide terminal:
    /// the focused pane takes the columns the two panes shared — the rectangle
    /// a terminal drag can take without a line of the other pane in it (finding
    /// K3) — while the bar and the message box keep the rows they had, so the
    /// view moves the panes' frame and not the conversation in it.
    #[test]
    fn zen_gives_the_focused_pane_the_two_panes_width_at_every_size() {
        let (mut app, _rx) = test_app("zen-layout");
        app.chat
            .push_message(AgentId::ROOT, Message::assistant("hello"));
        for (width, height) in [(80u16, 24u16), (200, 40)] {
            let at = format!("{width}×{height}");
            let two = pane_rects(&mut app, width, height);
            assert!(two.agents.width > 0, "{at}: the two-pane frame to measure");
            assert!(
                two.transcript.width < width,
                "{at}: the chat shares the frame with the tree"
            );

            // Chat focused: the transcript and its box are the whole frame
            // above the bar, and the agents pane is a zero rect — not a hidden
            // pane, so nothing can paint in it.
            ctrl(&mut app, 'f');
            let zen = pane_rects(&mut app, width, height);
            assert_eq!(
                zen.agents,
                Rect::new(0, 0, 0, 0),
                "{at}: the agents pane is not painted"
            );
            assert_eq!(zen.transcript.x, 0, "{at}: the chat starts at the left");
            assert_eq!(
                zen.transcript.width, width,
                "{at}: the chat has the two-pane width"
            );
            assert_eq!(zen.transcript.y, two.transcript.y, "{at}: and its rows");
            assert_eq!(
                zen.transcript.height + zen.input.height,
                two.transcript.height + two.input.height,
                "{at}: the internal split keeps its rows"
            );
            assert_eq!(
                zen.transcript.bottom(),
                zen.input.y,
                "{at}: the transcript sits on its box"
            );
            assert_eq!(zen.input.y, two.input.y, "{at}: the box keeps its rows");
            assert_eq!(
                zen.input.height, two.input.height,
                "{at}: the box keeps its rows"
            );
            assert_eq!(zen.bar, two.bar, "{at}: and so does the bar");
            assert!(zen.transcript_painted, "{at}: the transcript still paints");
            shot(&mut app, width, height).assert_shape("zen chat", width, height);
            ctrl(&mut app, 'f');

            // Agents focused: the tree is the whole width above the box, and
            // the chat is reduced to its message box below it.
            tab(&mut app);
            assert_eq!(app.focus, Focus::Agents, "{at}: Tab moves the keyboard");
            ctrl(&mut app, 'f');
            let tree = pane_rects(&mut app, width, height);
            assert_eq!(tree.agents.x, 0, "{at}: the tree starts at the left");
            assert_eq!(
                tree.agents.width, width,
                "{at}: the tree has the two-pane width"
            );
            assert_eq!(
                tree.agents.height + tree.input.height,
                two.agents.height,
                "{at}: the tree and the box take the rows the tree had"
            );
            assert_eq!(
                tree.agents.bottom(),
                tree.input.y,
                "{at}: the tree sits on the box"
            );
            assert_eq!(
                tree.transcript.height, 0,
                "{at}: the chat's transcript has no rows"
            );
            assert!(
                !tree.transcript_painted,
                "{at}: so `ChatPane::transcript` is `None`"
            );
            assert_eq!(tree.input.y, two.input.y, "{at}: the box keeps its rows");
            assert_eq!(
                tree.input.height, two.input.height,
                "{at}: the box keeps its rows"
            );
            assert_eq!(tree.input.x, 0, "{at}: and takes the width");
            assert_eq!(tree.input.width, width, "{at}: and takes the width");
            assert_eq!(tree.bar, two.bar, "{at}: and the bar keeps its rows");
            shot(&mut app, width, height).assert_shape("zen tree", width, height);
            ctrl(&mut app, 'f');
            tab(&mut app);
            assert_eq!(app.focus, Focus::Chat, "{at}: back where the size began");
        }
    }

    /// With zen on, the layout reads the focus — the fact `Tab` already cycles
    /// — so the pane cycle is the whole of "which pane is full-screen".
    #[test]
    fn zen_tabs_between_the_full_screen_panes() {
        let (mut app, _rx) = test_app("zen-tab");
        ctrl(&mut app, 'f');
        let chat = pane_rects(&mut app, 80, 24);
        assert_eq!(chat.transcript.width, 80, "the chat has the frame");
        assert_eq!(chat.agents.width, 0, "and the tree has nothing");

        tab(&mut app);
        let tree = pane_rects(&mut app, 80, 24);
        assert_eq!(tree.agents.width, 80, "Tab hands the frame to the tree");
        assert_eq!(tree.transcript.width, 80, "the box still spans the frame");
        assert_eq!(tree.transcript.height, 0, "with no transcript above it");

        tab(&mut app);
        assert_eq!(
            pane_rects(&mut app, 80, 24),
            chat,
            "and Tab again hands it back"
        );
    }

    /// Zen is a view, and the same key puts the frame back: the two panes, the
    /// same rects, the same box rows.
    #[test]
    fn toggling_zen_back_restores_the_two_panes() {
        let (mut app, _rx) = test_app("zen-back");
        app.chat
            .push_message(AgentId::ROOT, Message::assistant("hello"));
        let before = pane_rects(&mut app, 120, 32);
        ctrl(&mut app, 'f');
        assert_ne!(
            pane_rects(&mut app, 120, 32),
            before,
            "the view changed the frame"
        );
        ctrl(&mut app, 'f');
        assert_eq!(
            pane_rects(&mut app, 120, 32),
            before,
            "and the same key is the road back"
        );
    }

    /// `Ctrl-F` is a view key, so it rides the app-wide `Ctrl-` block: it works
    /// from the chat and from the tree, it is off until asked for, and `Ctrl-N`
    /// leaves it as the human set it — the view is the human's, not the
    /// conversation's.
    #[test]
    fn ctrl_f_toggles_zen_from_both_panes() {
        let (mut app, _rx) = test_app("zen-key");
        assert!(!app.zen, "the zen view is off until asked for");
        assert_eq!(app.focus, Focus::Chat, "the test starts in the chat");
        ctrl(&mut app, 'f');
        assert!(app.zen, "the chat pane's keyboard reaches the key");
        ctrl(&mut app, 'f');
        assert!(!app.zen, "and the same key takes it back");

        tab(&mut app);
        assert_eq!(app.focus, Focus::Agents);
        ctrl(&mut app, 'f');
        assert!(app.zen, "the agents pane's keyboard reaches it too");
        ctrl(&mut app, 'n');
        assert!(app.zen, "a new chat leaves the view as the human set it");
    }

    /// The counts of who is working are the one fact that lives only in the
    /// agents pane's title, so zen moves them to the title of the pane that is
    /// still on screen: the conversation pane's. The hidden-row counts stay
    /// behind — `▲N`/`▼N` is arithmetic about a list the view does not paint.
    #[test]
    fn zen_keeps_the_agents_counts_in_the_conversation_panes_title() {
        let (mut app, _rx) = test_app("zen-counts");
        // Twenty rows in a pane that shows eighteen: the agents pane's own
        // title drops its tail, and the tail is the `waiting` clause.
        crowd(&mut app, 19);
        let two = screen(&mut app, 80, 24);
        assert!(
            two[0].contains('▼'),
            "the agents pane hides rows and says so: {}",
            two[0]
        );
        assert!(two[0].contains("19 working"), "{}", two[0]);
        assert!(
            !two[0].contains("waiting"),
            "and its own title had to drop the waiting count: {}",
            two[0]
        );

        ctrl(&mut app, 'f');
        let zen = screen(&mut app, 80, 24);
        assert!(
            zen[0].contains(" mush "),
            "the conversation pane's title is painted: {}",
            zen[0]
        );
        assert!(zen[0].contains("19 working"), "{}", zen[0]);
        assert!(
            zen[0].contains("1 waiting"),
            "the clause the hidden title dropped is readable again: {}",
            zen[0]
        );
        assert!(
            !zen[0].contains('▲') && !zen[0].contains('▼'),
            "the hidden rows are not a fact about a pane with no rows: {}",
            zen[0]
        );
        assert!(
            !zen.iter().any(|row| row.contains(" agents ")),
            "the agents pane is not painted: {zen:?}"
        );
    }

    /// An actor Ctrl-N abandoned can still be finishing a request (up to the
    /// HTTP timeout); its events must not land in the new conversation. Ids
    /// collide by design — the new root is #0 too.
    #[test]
    fn events_from_an_abandoned_conversation_are_ignored() {
        let (mut app, _rx) = test_app("stale");
        let abandoned = app.tree.conversation();
        ctrl(&mut app, 'n');
        assert_ne!(app.tree.conversation(), abandoned, "a new conversation tag");
        app.chat
            .push_message(AgentId::ROOT, Message::user("current work"));

        app.update(Msg::Agent {
            conversation: abandoned,
            id: AgentId::ROOT,
            event: AgentEvent::Message(Message::assistant("stale reply")),
        });
        app.update(Msg::Agent {
            conversation: abandoned,
            id: AgentId::ROOT,
            event: AgentEvent::Compact {
                in_run: false,
                summary: "stale summary".to_string(),
            },
        });
        assert_eq!(
            app.chat.transcript(AgentId::ROOT).len(),
            1,
            "the stale reply and summary are dropped"
        );

        app.update(Msg::Agent {
            conversation: app.tree.conversation(),
            id: AgentId::ROOT,
            event: AgentEvent::Message(Message::assistant("fresh reply")),
        });
        assert_eq!(
            app.chat.transcript(AgentId::ROOT).len(),
            2,
            "the live conversation still lands"
        );
    }

    /// A steering message sent while the root is busy is folded into the run,
    /// and it must also be visible: the human has to see what they said.
    #[test]
    fn steering_text_is_echoed_in_the_chat() {
        let (mut app, _rx) = test_app("steer");
        app.tree.begin(AgentId::ROOT, None);
        app.chat.insert("also rename the module");

        app.send_message();

        assert_eq!(
            app.chat.transcript(AgentId::ROOT).len(),
            1,
            "the steering message is echoed"
        );
        assert_eq!(
            app.chat.transcript(AgentId::ROOT)[0].text(),
            "also rename the module"
        );
        assert_eq!(text_of(&app), "noted — folded in as the agent continues");
    }

    /// The meter counts the human's own words as soon as they are sent, not
    /// only once a reply arrives (finding B8).
    #[test]
    fn the_context_meter_counts_the_human_message() {
        let (mut app, _rx) = test_app("meter");
        let before = app.context_used_tokens();
        app.chat.insert("a question long enough to weigh something");
        app.send_message();
        assert!(
            app.context_used_tokens() > before,
            "the meter ignores the human's message"
        );
    }

    /// The meter follows the pane: a subagent's conversation is weighed against
    /// its own transcript, not the root's, because that transcript is what its
    /// next request will send.
    #[test]
    fn the_meter_measures_the_open_conversation() {
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::user("x".repeat(300)));
        let root = chat.used_tokens_for(AgentId::ROOT);
        assert!(root > 0);
        assert_eq!(
            chat.used_tokens_for(AgentId(7)),
            0,
            "nothing has been said to that agent"
        );

        chat.push_message(AgentId(7), Message::user("y".repeat(900)));
        assert!(
            chat.used_tokens_for(AgentId(7)) > root,
            "a longer child conversation weighs more than the root's"
        );
        assert_eq!(
            chat.used_tokens_for(AgentId::ROOT),
            root,
            "the root's own number is unchanged by a child's"
        );
    }

    /// The note a trim hands the model is one the human reads too: the actor
    /// emits it as a [`AgentEvent::Message`], and that road is the pane, the
    /// meter and the file. Before, the sentence lived in the actor's list
    /// alone, so the stored session and the number the human read were a note
    /// short of what the model was told.
    #[test]
    fn the_dropped_turns_note_reaches_the_pane_and_the_session() {
        let (mut app, _rx) = test_app("dropped-note");
        let note = {
            let mut messages = vec![
                Message::system("you are mush"),
                Message::user("first"),
                Message::assistant("x".repeat(500)),
                Message::user("second"),
                Message::assistant("more"),
                Message::user("third"),
            ];
            mush_core::transcript::trim_history(&mut messages, 300).expect("a trim that had to cut")
        };

        app.on_agent(AgentId::ROOT, AgentEvent::Message(note.clone()));

        assert_eq!(
            app.chat.transcript(AgentId::ROOT).last().map(Message::text),
            Some(note.text()),
            "the pane holds the sentence the model was given"
        );
        assert_eq!(
            app.session_snapshot().messages.last().map(Message::text),
            Some(note.text()),
            "and a restart resumes with it"
        );
    }

    /// One arithmetic per agent: the number `used_weight_for` returns is the
    /// agent's **own** system prompt plus its transcript — the history its
    /// actor sends.
    ///
    /// For the root that is the conversation the next `Run` hands the actor:
    /// the root's prompt is the conversation's own. For a child it is the
    /// prompt its actor published (only the actor that built it knows the
    /// workspace it names, its depth and its isolation) plus everything the
    /// child has said. The audit's blind spot: the old sum added the root's
    /// prompt for *every* id — 3,247 B against a shared leaf's 1,634 — so a
    /// focused leaf read 1,613 B ≈ 537 tokens heavier than the run it was
    /// about, and a 4× prompt weight would have passed every meter test.
    #[test]
    fn an_agents_weight_is_its_own_prompt_plus_its_transcript() {
        let (mut app, _rx) = test_app("own-prompt");
        // The root: exactly the conversation the actor is handed.
        app.chat
            .push_message(AgentId::ROOT, Message::user("x".repeat(300)));
        let conversation: usize = app.chat.conversation().iter().map(Message::weight).sum();
        assert_eq!(
            app.chat.used_weight_for(AgentId::ROOT),
            conversation,
            "the root's number is the conversation it sends"
        );

        // A child: what its own actor published, plus the transcript it holds —
        // its parent's brief as the opening line, and everything since.
        let (child_root, _mailbox) = isolate_child(&mut app, 1);
        assert_eq!(
            app.chat.transcript(AgentId(1))[0].text(),
            "task 1",
            "the brief opens the child's transcript, as it opens the actor's history"
        );
        app.chat
            .push_message(AgentId(1), Message::user("do the thing"));
        let prompt = Message::system(prompt::subagent_prompt(
            child_root.to_str().unwrap(),
            1,
            true,
            1 < crate::agent::MAX_DEPTH,
        ));
        assert_ne!(
            prompt.weight(),
            app.chat.system().weight(),
            "the child's prompt is not the root's; the test can tell them apart"
        );
        app.on_agent(AgentId(1), AgentEvent::SystemPrompt(prompt.clone()));

        let transcript: usize = app
            .chat
            .transcript(AgentId(1))
            .iter()
            .map(Message::weight)
            .sum();
        assert_eq!(
            app.chat.used_weight_for(AgentId(1)),
            prompt.weight() + transcript,
            "the child's number is its own prompt plus its transcript"
        );
        let _ = std::fs::remove_dir_all(child_root);
    }

    /// The facts line says how full the conversation is as well as how big the
    /// window is: the window alone cannot tell a human whether the next message
    /// will compact.
    #[test]
    fn the_context_meter_shows_used_over_window() {
        let (mut app, _rx) = test_app("meter-label");
        // The system prompt is part of every request, so the meter starts at its
        // weight — never at zero.
        let empty = app.context_meter();
        assert!(
            empty.starts_with("ctx ")
                && empty.ends_with(&format!("~{}", tokens_label(app.cfg().context_tokens))),
            "{empty}"
        );

        // Long enough that its weight has to show at the label's own
        // granularity: the meter prints a tenth of a thousand tokens, so a
        // short question can weigh less than the smallest step it shows.
        app.chat
            .insert(&"a question long enough to weigh something ".repeat(20));
        app.send_message();
        let meter = app.context_meter();
        assert_ne!(meter, empty, "the human's words are counted: {meter}");

        // A window the human stated is not marked as derived, and is shown as
        // the number it is: 32,768 tokens is `32.8k`, not `32k`.
        app.cell.edit(|cfg| cfg.set_context(32_768));
        assert!(
            app.context_meter().ends_with("32.8k"),
            "{}",
            app.context_meter()
        );
        assert!(
            !app.context_meter().contains('~'),
            "{}",
            app.context_meter()
        );
    }

    /// A window an actor learned reaches the UI's cell, through the event that
    /// announced it — not as a mutex write the UI never hears about (finding
    /// B7). The bar and the tool caps read one number, and the
    /// actors holding a handle from before the event measure against the same
    /// one.
    #[test]
    fn a_window_learned_by_an_actor_reaches_the_ui_cell() {
        let (mut app, _rx) = test_app("learned-window");
        // Taken first, the way the root actor's context holds it for the life
        // of the tree.
        let handle = app.cell.handle();
        let first = app.cfg().context_tokens;
        assert_ne!(first, 4_096, "the test wants a window that changes");

        app.update(Msg::Agent {
            conversation: app.tree.conversation(),
            id: AgentId::ROOT,
            event: AgentEvent::Context {
                tokens: 4_096,
                source: WindowSource::Complaint,
            },
        });

        assert_eq!(
            app.cfg().context_tokens,
            4_096,
            "the bar and the caps read the learned window"
        );
        assert!(
            !app.cfg().context_explicit,
            "and they still read it as learned, not as the human's"
        );
        assert_eq!(
            handle.config().unwrap().context_tokens,
            4_096,
            "the actors read the same one"
        );
    }

    /// A window the human stated is not the endpoint's to overwrite, whichever
    /// side learns the number first (finding B7's other half).
    #[test]
    fn a_stated_window_survives_what_an_actor_learns() {
        let (mut app, _rx) = test_app("stated-window");
        app.cell.edit(|cfg| cfg.set_context(32_768));
        let handle = app.cell.handle();

        app.update(Msg::Agent {
            conversation: app.tree.conversation(),
            id: AgentId::ROOT,
            event: AgentEvent::Context {
                tokens: 4_096,
                source: WindowSource::Complaint,
            },
        });

        assert_eq!(app.cfg().context_tokens, 32_768);
        assert_eq!(handle.config().unwrap().context_tokens, 32_768);
    }

    /// The runtime switches re-derive the window: a window mush only *learned*
    /// belongs to the endpoint that taught it, and a new endpoint or provider
    /// means a new one (finding A5).
    ///
    /// The switch itself, not `/url` and `/provider`: those arms fetch the
    /// model list after switching, which is a socket the default suite does not
    /// take. The arms call exactly these two functions and nothing else, so the
    /// behaviour they have is the behaviour pinned here.
    #[test]
    fn a_runtime_switch_rederives_the_window() {
        let (mut app, _rx) = test_app("rederive");
        let fallback = app.cfg().fallback_context();
        app.cell.learn_context(4_096, WindowSource::Advertised);
        assert_eq!(app.cfg().context_tokens, 4_096, "learned, not stated");

        app.switch_endpoint("http://127.0.0.1:9999");
        assert_eq!(
            app.cfg().context_tokens,
            fallback,
            "a new endpoint re-derives the window"
        );
        assert_eq!(
            app.cell.handle().config().unwrap().context_tokens,
            fallback,
            "and the actors measure against the re-derived one"
        );

        app.switch_provider(Provider::DeepSeek);
        assert_eq!(app.cfg().base_url, "https://api.deepseek.com");
        assert_eq!(app.cfg().model, "deepseek-flash", "the vendor's own model");
        assert_eq!(
            app.cfg().context_tokens,
            500_000,
            "and the window documented for it"
        );
        assert_eq!(
            app.cell.handle().config().unwrap().context_tokens,
            500_000,
            "the actors follow the switch"
        );

        // A window the human stated is theirs: neither switch touches it.
        app.cell.edit(|cfg| cfg.set_context(32_768));
        app.switch_endpoint("http://127.0.0.1:9998");
        app.switch_provider(Provider::Custom);
        assert_eq!(
            app.cfg().context_tokens,
            32_768,
            "what the human said stays"
        );
    }

    /// The model list is discovered on its own thread, so it arrives after the
    /// first frame (finding A9). Its first entry is the guess only when nothing
    /// else named a model, its advertised window comes with it, and a model the
    /// human picked is never overwritten by whatever the endpoint lists first.
    #[test]
    fn a_late_model_list_names_a_model_only_when_nothing_did() {
        let (mut app, _rx) = test_app("late-models");
        let endpoint = app.cfg().base_url.clone();
        // What "no model given and none known yet" looks like after the
        // precedence chain: mush started without one.
        app.cell.edit(|cfg| cfg.model.clear());

        app.update(Msg::Models {
            endpoint: endpoint.clone(),
            models: vec![http::Model {
                id: "found".to_string(),
                context: Some(4_096),
            }],
        });

        assert_eq!(app.cfg().model, "found", "the endpoint's first model");
        assert_eq!(
            app.cfg().context_tokens,
            4_096,
            "and the window it advertised for it"
        );
        assert_eq!(
            app.cell.handle().config().unwrap().model,
            "found",
            "the actors ask for the model the UI shows"
        );
        assert!(
            text_of(&app).contains("found"),
            "a model that appeared on its own is said out loud: {}",
            text_of(&app)
        );

        app.cell.edit(|cfg| cfg.set_model("mine"));
        app.update(Msg::Models {
            endpoint,
            models: vec![http::Model {
                id: "other".to_string(),
                context: None,
            }],
        });
        assert_eq!(app.cfg().model, "mine", "a stated model wins over the list");
    }

    /// A model id is data, not a format: an id that carries the picker's own
    /// `" · "` separator is one id. The row says `{id} · {tokens}` for a human
    /// to read, `Enter` takes what the row stands for, and the bullet marks
    /// that same row — reading the id back out of the label made the pick the
    /// text before the first separator, i.e. `weird` (Tier 3 §7).
    #[test]
    fn a_model_id_carrying_the_separator_is_still_the_model_that_is_picked() {
        isolate_user_config();
        let (mut app, _rx) = test_app("model-id-separator");
        app.models = vec![
            http::Model {
                id: "weird · model".to_string(),
                context: Some(500_000),
            },
            http::Model {
                id: "plain".to_string(),
                context: None,
            },
        ];
        app.open_model_picker();

        // The row a human reads: the whole id, then the window it advertises.
        let rows = screen(&mut app, 80, 24);
        assert!(
            rows.iter().any(|row| row.contains("weird · model · 500k")),
            "the row carries the id and its window: {rows:?}"
        );

        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.picker.is_none(), "the pick closed the list");
        assert_eq!(
            app.cfg().model,
            "weird · model",
            "the separator is part of the id, not the end of it"
        );
        assert!(
            text_of(&app).starts_with("model: weird · model @ "),
            "and the bar names the model that was picked: {}",
            text_of(&app)
        );

        // The bullet follows the same id rather than a parse of the label, and
        // it follows the row rather than the cursor: `plain` is where the
        // cursor is put, and it is still `weird · model` that is marked.
        app.open_model_picker();
        app.move_picker(1);
        let rows = screen(&mut app, 80, 24);
        let marked: Vec<&String> = rows.iter().filter(|row| row.contains('•')).collect();
        assert_eq!(marked.len(), 1, "exactly one row is marked: {rows:?}");
        assert!(
            marked[0].contains("weird · model"),
            "and it is the model in use: {marked:?}"
        );
    }

    /// The provider list is a choice the same way: a row stands for the name it
    /// wears, the list opens on the provider in use (both read the row's id),
    /// and `Enter` applies that name — nothing about the endpoint in use
    /// changes when the row picked is the one already there.
    #[test]
    fn the_provider_picker_picks_the_name_its_row_stands_for() {
        isolate_user_config();
        let (mut app, _rx) = test_app("provider-picker");

        run(&mut app, "/provider");

        let picker = app.picker.as_ref().expect("the provider list opened");
        assert_eq!(picker.kind, PickerKind::Provider);
        assert_eq!(
            picker.items[picker.cursor].id.as_deref(),
            Some(app.cfg().provider.name()),
            "the cursor opens on the provider in use: {:?}",
            labels(&picker.items)
        );

        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.picker.is_none(), "the pick closed the list");
        assert_eq!(app.cfg().provider, Provider::Custom);
        assert!(
            text_of(&app).starts_with("provider: custom · "),
            "and the bar names the provider that was picked: {}",
            text_of(&app)
        );
    }

    /// A list fetched from the endpoint the human has since left must not land:
    /// the picker, and the model it would name, are about the endpoint in use.
    #[test]
    fn a_model_list_from_an_endpoint_mush_left_is_dropped() {
        let (mut app, _rx) = test_app("stale-models");
        app.cell.edit(|cfg| cfg.model.clear());

        app.update(Msg::Models {
            endpoint: "http://127.0.0.1:2".to_string(),
            models: vec![http::Model {
                id: "from-elsewhere".to_string(),
                context: None,
            }],
        });

        assert!(app.models.is_empty(), "nothing was adopted");
        assert!(app.cfg().model.is_empty(), "and no model was named");
    }

    /// A request with no model is a guaranteed refusal, and discovery now runs
    /// after the first frame, so this state is reachable for as long as a fetch
    /// takes: the human is told, rather than reading the endpoint's complaint
    /// about an empty model id (finding A9).
    #[test]
    fn a_message_with_no_model_says_so_instead_of_asking() {
        let (mut app, _rx) = test_app("no-model");
        app.cell.edit(|cfg| cfg.model.clear());

        app.chat.insert("hello");
        app.send_message();

        assert!(!app.busy(), "no run was started");
        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "and nothing was put in the transcript an endpoint would be sent"
        );
        assert!(
            text_of(&app).contains("no model"),
            "the bar says why: {}",
            text_of(&app)
        );
        assert_eq!(
            app.chat.input().text(),
            "hello",
            "and the words are still in the box to retry"
        );
    }

    /// A failed nudge puts the node's phase back exactly as it was (finding
    /// B10); a dead mailbox must not rewrite a `✓` into `· idle`.
    #[test]
    fn a_failed_nudge_restores_the_phase() {
        let (mut app, _rx) = test_app("nudge-restore");
        app.focus = Focus::Agents;
        // A finished child with a summary, whose actor is gone: the nudge below
        // cannot be delivered, and the row must not end up claiming work.
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd: crossbeam_channel::unbounded().0,
        });
        app.tree
            .finish(AgentId(1), Some("did the work".to_string()));
        app.tree.agent_tx.remove(&AgentId(1));
        app.tree.focus(AgentId(1));
        app.chat.insert("one more thing");

        app.send_message();

        assert_eq!(
            app.tree.node(AgentId(1)).unwrap().phase,
            Phase::Done,
            "the ✓ is not rewritten"
        );
        assert_eq!(text_of(&app), "agent #1 is gone");
    }

    /// A human's nudge to a child tells the parent's books the child is running
    /// again: the parent's waits and the one-shared-child guard read them, and
    /// a resume is invisible from the parent's side otherwise (audit row 1).
    #[test]
    fn a_human_nudge_tells_the_parent_the_child_is_running() {
        let (mut app, _rx) = test_app("nudge-parent");
        let (child_tx, _child_rx) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd: child_tx,
        });
        let (parent_tx, parent_rx) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.agent_tx.insert(AgentId::ROOT, parent_tx);
        app.tree.focus(AgentId(1));

        app.chat.insert("carry on");
        app.send_message();

        assert!(
            matches!(parent_rx.try_recv(), Ok(AgentMsg::ChildRunning { id: 1 })),
            "the parent is told the child it believed at rest is running"
        );
    }

    /// `Enter` on a tree row shows that agent *and* hands it the keyboard
    /// (finding S2): typing then reaches the agent, the letters are a message
    /// and not tree bindings, and the bar's badge moves with the keyboard.
    #[test]
    fn enter_on_a_row_shows_its_transcript_and_leaves_the_keys_in_the_tree() {
        let (mut app, _rx) = test_app("enter-focus");
        app.focus = Focus::Agents;
        // Two rows, so a leaked `g`/`G` would move the cursor somewhere the
        // message could hide.
        let (cmd, mailbox) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd,
        });
        app.tree.insert(Spawn {
            id: AgentId(2),
            parent: AgentId::ROOT,
            brief: "parser".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd: crossbeam_channel::unbounded().0,
        });
        app.tree.cursor_top();
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.tree.cursor_id(), Some(AgentId(1)));

        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.tree.focused, AgentId(1), "the pane shows #1");
        assert_eq!(app.focus, Focus::Agents, "and the tree kept the keyboard");
        let cursor = app.tree.cursor();
        assert!(
            screen(&mut app, 80, 24)
                .iter()
                .any(|row| row.contains(" agents ")),
            "the bar's badge agrees with the key table"
        );

        // The letters are the tree's: `g` moves the cursor, and nothing is
        // typed at the agent.
        app.on_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));

        assert_ne!(app.tree.cursor(), cursor, "`g` is still the first-row key");
        assert!(
            app.chat.input().text().is_empty(),
            "nothing went into a box"
        );
        assert!(mailbox.try_recv().is_err(), "so nothing reached #1");
    }

    /// The other half of the same rule: while the chat owns the keyboard, `c`
    /// is a letter, not a tree binding (finding S2).
    #[test]
    fn a_c_in_the_chat_types_a_c_and_cancels_nothing() {
        let (mut app, _rx) = test_app("chat-c");
        app.focus = Focus::Chat;
        let (cmd, mailbox) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd,
        });
        app.tree.begin(AgentId(1), None);
        app.tree.focus(AgentId(1));

        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));

        assert_eq!(app.chat.input().text(), "c", "the letter went into the box");
        assert!(mailbox.try_recv().is_err(), "no Stop was sent");
        assert!(
            app.tree.node(AgentId(1)).unwrap().phase.is_busy(),
            "the agent is still running"
        );
    }

    /// `/compact` asks the *focused* agent to fold its conversation, and says
    /// so on the bar. Nothing about the row's phase changes: the fold is the
    /// actor's job, and its `Compact` event is what replaces the transcript.
    #[test]
    fn compact_asks_the_focused_agent() {
        let (mut app, _rx) = test_app("compact-focus");
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd: crossbeam_channel::unbounded().0,
        });
        let (mailbox, asked) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.agent_tx.insert(AgentId(1), mailbox);
        app.tree.focus(AgentId(1));
        let before = app.tree.node(AgentId(1)).unwrap().phase.clone();

        run(&mut app, "/compact");

        assert!(
            matches!(asked.try_recv(), Ok(AgentMsg::Compact(_))),
            "the request goes to the agent whose pane is focused"
        );
        assert_eq!(text_of(&app), "compacting #1…");
        assert_eq!(
            app.tree.node(AgentId(1)).unwrap().phase,
            before,
            "a fold is not a run, so the row is not put to work"
        );
        // The root is a different agent: its mailbox is not what was written to.
        assert!(asked.try_recv().is_err());
    }

    /// A `/compact` that cannot be delivered says so, the way a nudge does —
    /// and leaves the row exactly as it was (finding B10).
    #[test]
    fn compact_on_a_dead_mailbox_does_not_lie_on_the_row() {
        let (mut app, _rx) = test_app("compact-dead");
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd: crossbeam_channel::unbounded().0,
        });
        app.tree
            .finish(AgentId(1), Some("did the work".to_string()));
        app.tree.agent_tx.remove(&AgentId(1));
        app.tree.focus(AgentId(1));

        run(&mut app, "/compact");

        assert_eq!(text_of(&app), "agent #1 is gone");
        assert_eq!(
            app.tree.node(AgentId(1)).unwrap().phase,
            Phase::Done,
            "no phase is claimed for work nobody is doing"
        );
    }

    /// `/help` is the list a human reads to find out what exists.
    #[test]
    fn help_advertises_compact() {
        let (mut app, _rx) = test_app("compact-help");
        run(&mut app, "/help");
        let picker = app.picker.as_ref().expect("help opens a list");
        let help = labels(&picker.items).join("\n");
        assert!(help.contains("/compact"), "{help}");
    }

    /// The help is a readable list, not a two-row teaser in the foot: it opens
    /// the popup `/notes` uses, at the top, and the popup paints it (finding
    /// U15).
    #[test]
    fn help_opens_a_readable_list() {
        let (mut app, _rx) = test_app("help-list");

        run(&mut app, "/help");

        let picker = app.picker.as_ref().expect("help opens a list");
        assert_eq!(picker.kind, PickerKind::Help);
        assert_eq!(picker.cursor, 0, "help opens at its head");
        assert!(
            picker
                .items
                .iter()
                .any(|row| row.label.contains("mush keys")),
            "the list carries the keys: {}",
            labels(&picker.items).join("\n")
        );
        let rows = screen(&mut app, 120, 32);
        assert!(
            rows.iter().any(|row| row.contains(" help · line 1/")),
            "the popup paints the list and where in it the reader is: {rows:?}"
        );
    }

    /// `/key` with no key says where a key *would* go. It promised "memory
    /// only", while the arm beside it writes the secret into the home config in
    /// plain text and names the file it wrote: the promise was wrong in the one
    /// direction that costs a secret, so the sentence names the file (finding
    /// A1).
    #[test]
    fn the_key_report_names_where_a_key_would_be_saved() {
        let (mut app, _rx) = test_app("key-report");
        run(&mut app, "/key");
        assert_eq!(
            text_of(&app),
            format!(
                "no api key — /key <secret> sets one (saved to {})",
                userconfig::config_path().display()
            )
        );
    }

    /// `Enter` on a row says whose transcript the pane shows, and the brief it
    /// says it with is bounded like every other bar line: a brief is a sentence
    /// written *for a model* and the bar paints its row whole, so the line took
    /// whatever the brief gave it — for a long one, the rest of the terminal
    /// (finding B7). The head survives, because it is what says whose brief it
    /// is.
    #[test]
    fn the_focus_line_bounds_the_brief_it_carries() {
        let (mut app, _rx) = test_app("focus-line");
        let brief = "please create a file called deep.txt and fill it with everything \
                     the task needs, at length, in prose";
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: brief.to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd: crossbeam_channel::unbounded().0,
        });
        app.tree.move_cursor(1);
        assert_eq!(app.tree.cursor_id(), Some(AgentId(1)));

        app.focus_cursor_row();

        let line = text_of(&app);
        assert!(line.starts_with("agent #1: "), "whose brief it is: {line}");
        assert!(line.contains('…'), "the cut is visible: {line}");
        assert!(
            line.chars().count() <= CURSOR_LINE_COLUMNS,
            "one bar row at 80×24: {line}"
        );
    }

    /// A fold the human asked for is on the screen while it runs, at both sizes
    /// the audit photographs: the row wears its own glyph and words, the bar
    /// says what happens to the words they are about to type — in the same verb
    /// the row uses, whatever kind of fold it is — and the transcript's foot
    /// repeats the fold rather than `working` (finding U11, refactor R8).
    #[test]
    fn a_fold_in_flight_is_painted_on_every_surface() {
        let (mut app, _rx) = test_app("compact-painted");
        app.chat
            .push_message(AgentId::ROOT, Message::user("fold this".to_string()));
        let flag = Arc::new(AtomicBool::new(false));

        for kind in [
            Compacting::Requested,
            Compacting::Parked,
            Compacting::NearlyFull,
        ] {
            app.on_agent(
                AgentId::ROOT,
                AgentEvent::Compacting {
                    why: kind,
                    cancel: Some(flag.clone()),
                },
            );

            assert_eq!(
                app.tree_line().as_deref(),
                Some(
                    format!(
                        "{} #0 · keep typing — your message is answered after the fold",
                        kind.verb()
                    )
                    .as_str()
                ),
                "the bar names the fold with the row's own verb: {kind:?}"
            );

            // The row's words, where there are columns for them: at 40 a tail
            // cell that does not fit is dropped whole, not cut.
            let wide = screen(&mut app, 80, 24).join("\n");
            assert!(
                wide.contains(kind.words().trim_end_matches('…')),
                "the row says {kind:?} in its own words: {wide}"
            );

            for (width, height) in [(80u16, 24u16), (40, 10)] {
                let rows = screen(&mut app, width, height);
                let painted = rows.join("\n");
                assert!(
                    painted.contains("≡ #0"),
                    "the row says a fold, not a run, at {width}×{height}: {rows:?}"
                );
                assert!(
                    painted.contains(kind.verb()),
                    "and the bar's line carries the same verb at {width}×{height}: {rows:?}"
                );
                assert!(
                    painted.contains("keep typing"),
                    "and the human's question answered at {width}×{height}: {rows:?}"
                );
                assert!(
                    !painted.contains("working."),
                    "a fold is not the run's own model call at {width}×{height}: {rows:?}"
                );
            }
        }

        // The box still takes a line while the fold runs: mush never blocks
        // input, and the sentence the bar prints says what happens to the
        // words. What proves it is the painted box, not the buffer: a line can
        // be in the box and nowhere on the screen.
        app.focus = Focus::Chat;
        for letter in "noted".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(letter), KeyModifiers::NONE));
        }
        let rows = screen(&mut app, 40, 10).join("\n");
        assert!(rows.contains("noted"), "the box took the line: {rows}");
        assert_eq!(app.chat.input().text(), "noted");
    }

    /// When the fold lands the state leaves the screen, and the transcript says
    /// what happened to the conversation instead (finding U11).
    #[test]
    fn a_landed_fold_takes_its_state_off_the_screen() {
        let (mut app, _rx) = test_app("compact-landed");
        app.chat
            .push_message(AgentId::ROOT, Message::user("fold this".to_string()));
        app.on_agent(
            AgentId::ROOT,
            AgentEvent::Compacting {
                why: Compacting::Requested,
                cancel: None,
            },
        );
        assert!(screen(&mut app, 80, 24).join("\n").contains("compacting"));

        app.on_agent(
            AgentId::ROOT,
            AgentEvent::Compact {
                summary: "the task, and where it got to".to_string(),
                in_run: false,
            },
        );
        for (width, height) in [(80u16, 24u16), (40, 10)] {
            let painted = screen(&mut app, width, height).join("\n");
            assert!(
                !painted.contains("compacting") && !painted.contains('≡'),
                "nothing still claims to be folding at {width}×{height}: {painted}"
            );
        }
        assert_eq!(app.tree_line(), None, "and the bar stops saying so");
        assert_eq!(
            app.tree.node(AgentId::ROOT).unwrap().phase,
            Phase::Done,
            "the fold that landed is a thing that finished"
        );
    }

    /// A fold that fails or is stopped takes its state off the row too: the
    /// notice owns what went wrong, and the `≡` does not outlive the request.
    #[test]
    fn a_fold_that_ends_without_landing_leaves_a_quiet_row() {
        let (mut app, _rx) = test_app("compact-ended");
        app.on_agent(
            AgentId::ROOT,
            AgentEvent::Compacting {
                why: Compacting::NearlyFull,
                cancel: None,
            },
        );
        app.on_agent(AgentId::ROOT, AgentEvent::CompactingEnded { in_run: false });
        let painted = screen(&mut app, 80, 24).join("\n");
        assert!(!painted.contains("compacting"), "{painted}");
        assert_eq!(app.tree.node(AgentId::ROOT).unwrap().phase, Phase::Idle);
    }

    /// The bar's derived sentence answers "may I keep typing?" while the fold
    /// runs, and it is the *same* fact and the same verb the row draws — one
    /// derivation, so the two cannot disagree about whether anything is folding
    /// or about what to call it (finding U11, refactor R8).
    #[test]
    fn the_bar_answers_may_i_keep_typing_while_a_fold_runs() {
        let (mut app, _rx) = test_app("compact-bar");
        assert_eq!(app.tree_line(), None, "nothing to report at rest");

        app.tree.compacting(AgentId::ROOT, Compacting::Parked, None);
        assert_eq!(
            app.tree_line().as_deref(),
            Some("folding #0 · keep typing — your message is answered after the fold"),
            "a parked fold is what the human is waiting for, and it is folding"
        );
        let rows = screen(&mut app, 80, 24).join("\n");
        assert!(rows.contains("keep typing"), "{rows}");

        app.tree.compacted(AgentId::ROOT, false);
        assert_eq!(app.tree_line(), None, "and it goes when the fold does");
    }

    /// Ctrl-C reaches a fold from rest: the fold's `Compacting` event carries
    /// the only handle there is to the summarize call, and the UI's Stop looks
    /// exactly where that handle is put (findings U11 and B6).
    #[test]
    fn a_stop_reaches_the_summarize_call_of_a_fold_from_rest() {
        let (mut app, _rx) = test_app("compact-stop");
        let flag = Arc::new(AtomicBool::new(false));
        app.on_agent(
            AgentId::ROOT,
            AgentEvent::Compacting {
                why: Compacting::Requested,
                cancel: Some(flag.clone()),
            },
        );

        app.interrupt_all();

        assert!(
            flag.load(Ordering::SeqCst),
            "the Stop the human pressed reached the summarize request"
        );
        assert_eq!(
            app.tree.node(AgentId::ROOT).unwrap().phase,
            Phase::Cancelling,
            "and the row says the stop is on its way"
        );
    }

    /// `/help` renders the same key table `mush --help` does, so a human who
    /// learns the keyboard from the notice can discover every binding instead
    /// of the six the old hand-written line named — and discover the keys that
    /// really scroll (finding K3), not a wheel mush never takes.
    #[test]
    fn help_names_the_whole_key_table() {
        let (mut app, _rx) = test_app("keys-help");
        // A real terminal size, the way `main` reports it: the list wraps to the
        // popup this screen paints, and a phrase split by a narrow terminal is
        // not a missing binding.
        app.set_term_size(200, 50);
        run(&mut app, "/help");
        let picker = app.picker.as_ref().expect("help opens a list");
        let help = labels(&picker.items).join("\n");
        for want in [
            "j / k, ↑ / ↓",
            "show the selected agent's transcript",
            "cancel the selected agent",
            "back to the root agent",
            "←",
            "→",
            "Ctrl-Q",
            "↑ / ↓, PgUp / PgDn",
            "page up / down the rows",
        ] {
            assert!(
                help.contains(want),
                "`{want}` is missing from /help:\n{help}"
            );
        }
        assert!(
            !help.contains("wheel"),
            "a wheel it does not scroll:\n{help}"
        );
    }

    /// `←`/`→` in the agents pane walk the tree by its parent links, over the
    /// *painted* pre-order rows, not the storage order (finding U10): from a
    /// great-grandchild `←` reaches the root one ancestor per press, `→` walks
    /// back down the same chain, and `←` follows the parent link rather than the
    /// row above it.
    #[test]
    fn left_and_right_walk_the_tree_by_its_parent_links() {
        let (mut app, _rx) = test_app("tree-walk");
        app.focus = Focus::Agents;
        // root(0) → #1 → #2 → #3, plus #4 as the root's second child, spawned
        // *before* #2 and #3. Storage order is therefore 0,1,4,2,3, while the
        // painted pre-order is 0,1,2,3,4 — the two orders differ, so the test
        // can tell which one the cursor walks.
        let mut _mailboxes = Vec::new();
        for (id, parent, depth) in [(1u64, 0u64, 1usize), (4, 0, 1), (2, 1, 2), (3, 2, 3)] {
            let (cmd, rx) = crossbeam_channel::unbounded::<AgentMsg>();
            _mailboxes.push(rx);
            app.tree.insert(Spawn {
                id: AgentId(id),
                parent: AgentId(parent),
                brief: format!("child {id}"),
                depth,
                branch: None,
                fork: None,
                cmd,
            });
        }
        let stored: Vec<u64> = app.tree.agents.iter().map(|node| node.id.0).collect();
        let painted: Vec<u64> = app.tree.rows().iter().map(|node| node.id.0).collect();
        assert_eq!(stored, vec![0, 1, 4, 2, 3], "storage is spawn order");
        assert_eq!(
            painted,
            vec![0, 1, 2, 3, 4],
            "rows are the painted pre-order"
        );
        // `G` walks the painted rows: the bottom row is #4, not the storage
        // vector's last (#3).
        app.tree.cursor_bottom();
        assert_eq!(app.tree.cursor_id(), Some(AgentId(4)));

        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);

        // Select the great-grandchild #3 (painted index 3) and walk up. (The
        // tree's `move_cursor` moves one row per call, so three calls.)
        app.tree.cursor_top();
        for _ in 0..3 {
            app.tree.move_cursor(1);
        }
        assert_eq!(app.tree.cursor_id(), Some(AgentId(3)));
        app.on_key(left);
        assert_eq!(app.tree.cursor_id(), Some(AgentId(2)), "#3's parent");
        app.on_key(left);
        assert_eq!(app.tree.cursor_id(), Some(AgentId(1)), "its grandparent");
        app.on_key(left);
        assert_eq!(app.tree.cursor_id(), Some(AgentId::ROOT), "three levels up");
        // The root has no parent: the cursor stays where it is, an honest no-op.
        app.on_key(left);
        assert_eq!(app.tree.cursor_id(), Some(AgentId::ROOT));
        // `→` is the companion: the root's first child, then down the chain.
        app.on_key(right);
        assert_eq!(
            app.tree.cursor_id(),
            Some(AgentId(1)),
            "the root's first child"
        );
        app.on_key(right);
        assert_eq!(app.tree.cursor_id(), Some(AgentId(2)));
        app.on_key(right);
        assert_eq!(
            app.tree.cursor_id(),
            Some(AgentId(3)),
            "down the same chain"
        );
        // A leaf has no child: the cursor stays.
        app.on_key(right);
        assert_eq!(app.tree.cursor_id(), Some(AgentId(3)));

        // #4's painted row is directly below #3's, but its parent is the root:
        // `←` follows the link (root), not row-minus-one (#3).
        app.tree.cursor_bottom();
        assert_eq!(app.tree.cursor_id(), Some(AgentId(4)));
        app.on_key(left);
        assert_eq!(
            app.tree.cursor_id(),
            Some(AgentId::ROOT),
            "the parent link, not the row above"
        );
    }

    /// The workspace's hue reaches a real frame and *only* the chrome:
    /// comparing a frame painted with a themed `Theme` against the same frame
    /// painted with the fixed palette, every cell keeps its text, and every
    /// cell whose colour changed is a cell that wore the accent before. The
    /// threading cannot quietly fall back to the fixed palette at one site,
    /// and the hue cannot bleed into the reds, yellows and grays that are
    /// content.
    #[test]
    fn a_themed_frame_repaints_only_the_chrome() {
        use ratatui::style::Color;

        let (mut app, _rx) = test_app("theme-chrome");
        app.focus = Focus::Chat;
        let env = crate::theme::EnvText {
            theme: None,
            colorterm: Some("truecolor".to_string()),
            term: None,
        };
        let theme = crate::theme::Theme::resolve(&env, std::path::Path::new("/work")).unwrap();
        let hue = theme.hue().expect("a truecolor terminal gets a hue");
        let accent = Color::Rgb(hue.rgb.0, hue.rgb.1, hue.rgb.2);

        let (width, height) = (100u16, 30u16);
        let (_, plain) = painted_with(&mut app, width, height, &crate::theme::Theme::default());
        let (_, themed) = painted_with(&mut app, width, height, &theme);
        let mut repainted = 0;
        let mut badge = false;
        for y in 0..height {
            for x in 0..width {
                let before = plain[(x, y)].style();
                let after = themed[(x, y)].style();
                assert_eq!(
                    plain[(x, y)].symbol(),
                    themed[(x, y)].symbol(),
                    "({x}, {y}) text moved with the colour"
                );
                if before == after {
                    continue;
                }
                repainted += 1;
                assert!(
                    [before.fg, before.bg].contains(&Some(Color::Cyan)),
                    "({x}, {y}) changed without wearing the accent: {before:?} -> {after:?}"
                );
                assert!(
                    [after.fg, after.bg].contains(&Some(accent)),
                    "({x}, {y}) lost the accent: {before:?} -> {after:?}"
                );
                if y >= height - super::screen::bar_rows(height) {
                    badge = true;
                }
            }
        }
        assert!(repainted > 0, "the hue reached nothing");
        assert!(badge, "the bar's badge did not wear the hue");
    }

    /// `PgUp`/`PgDn` in the agents pane move the cursor a whole page, stop at
    /// both ends of the painted rows, and leave the selection on the row the
    /// pane paints as selected — a run with twenty-four children is paged, not
    /// walked a row at a time.
    #[test]
    fn page_keys_move_the_tree_cursor_a_page_and_clamp_at_both_ends() {
        let (mut app, _rx) = test_app("tree-page");
        app.focus = Focus::Agents;
        // The receivers are kept for the test's life, so the children's
        // mailboxes stay open: a child whose actor has gone is a row the pane
        // still paints, but this is the tree a run builds.
        let mut _mailboxes = Vec::new();
        for id in 1..=24u64 {
            let (cmd, mailbox) = crossbeam_channel::unbounded::<AgentMsg>();
            _mailboxes.push(mailbox);
            app.tree.insert(Spawn {
                id: AgentId(id),
                parent: AgentId::ROOT,
                brief: format!("child {id}"),
                depth: 1,
                branch: None,
                fork: None,
                cmd,
            });
        }
        let painted: Vec<AgentId> = app.tree.rows().iter().map(|node| node.id).collect();
        assert_eq!(painted.len(), 25, "the root and twenty-four children");
        assert!(
            painted.len() > 18,
            "more rows than an 80x24 pane shows, so the bottom is reached by paging"
        );
        let bottom = *painted.last().unwrap();

        let down = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        // The page is whatever the key table says it is, so this test cannot
        // disagree with `keys.rs` about the distance.
        let page = match keys::key(Focus::Agents, false, false, down) {
            Intent::TreeMove(step) => step,
            other => panic!("PgDn in the agents pane is not a page: {other:?}"),
        };
        assert!(page > 1, "a page is more than a row");
        let page = page as usize;

        app.tree.cursor_top();
        app.on_key(down);
        assert_eq!(
            app.tree.cursor_id(),
            Some(painted[page]),
            "one page down the painted rows"
        );
        app.on_key(down);
        assert_eq!(app.tree.cursor_id(), Some(painted[page * 2]), "two pages");
        // Only five rows are left: the third page clamps on the last painted
        // row instead of wrapping to the top or running off the end.
        app.on_key(down);
        assert_eq!(app.tree.cursor_id(), Some(bottom), "clamped at the bottom");
        app.on_key(down);
        assert_eq!(
            app.tree.cursor_id(),
            Some(bottom),
            "and the end stays the end"
        );

        app.on_key(up);
        assert_eq!(
            app.tree.cursor_id(),
            Some(painted[page + 4]),
            "a page back up"
        );
        app.on_key(up);
        app.on_key(up);
        assert_eq!(app.tree.cursor_id(), Some(painted[0]), "clamped at the top");
        app.on_key(up);
        assert_eq!(
            app.tree.cursor_id(),
            Some(painted[0]),
            "and the top stays the top"
        );

        // The selection is the row that becomes visible: the pane scrolls to
        // follow the cursor, and the last row sits below an eighteen-row
        // window. At the top of the list it is not on the pane at all — the
        // control that says the check below is about following the cursor and
        // not about a row that happened to be painted.
        app.tree.cursor_top();
        let top_view = screen(&mut app, 80, 24);
        assert!(
            !top_view.iter().any(|row| row.contains(&bottom.to_string())),
            "the bottom row is outside the pane's window at the top of the list:\n{}",
            top_view.join("\n")
        );

        app.on_key(down);
        app.on_key(down);
        app.on_key(down);
        assert_eq!(app.tree.cursor_id(), Some(bottom));
        let rows = screen(&mut app, 80, 24);
        // The row and the cursor row's footer both name the agent; the row is
        // the one the pane paints as selected, and it says so with the
        // highlight alone — no marker beside it.
        let named: Vec<&String> = rows
            .iter()
            .filter(|row| row.contains(&bottom.to_string()))
            .collect();
        assert!(
            !named.is_empty(),
            "the page's row is on the pane now — the list followed the cursor:\n{}",
            rows.join("\n")
        );
        let selected = selected_rows(&mut app, 80, 24);
        assert_eq!(
            selected.len(),
            1,
            "exactly one row wears the pane's selection highlight: {selected:?}\n{}",
            rows.join("\n")
        );
        assert!(
            rows[selected[0]].contains(&bottom.to_string()),
            "and it is the row the pane paints as selected:\n{}",
            rows.join("\n")
        );
    }

    /// A session can hold two rows of one id — the restore's refusal of a
    /// duplicate is the secrets audit's C9, and this pane must survive whatever
    /// state it is handed. Two lengths used to decide one question: `rows()`
    /// de-duplicated by id while the cursor was bounded by the storage length,
    /// so a session with two rows of id 2 was `agents=3 rows=2` and `G` indexed
    /// `rows[2]` — an index panic on a real keypress, in release builds too, with
    /// the hidden twin an agent the human could not see before it (D2,
    /// `screen.rs:472`). The cursor is now bounded by the rows that were
    /// painted, and the pane indexes those rows with something that cannot
    /// panic.
    #[test]
    fn the_pane_never_indexes_past_the_rows_it_painted() {
        let (mut app, _rx) = test_app("pane-twin-rows");
        for _ in 0..2 {
            app.tree.register(Existing {
                id: AgentId(2),
                parent: None,
                depth: 1,
                brief: "twin".to_string(),
                title: None,
                phase: Phase::Done,
                branch: None,
                fork: None,
                summary: None,
                leftover: false,
                landed: None,
                result_unread: false,
                tx: None,
            });
        }
        assert_eq!(app.tree.agents.len(), 3, "the root and both twins");
        assert_eq!(
            app.tree.rows().len(),
            3,
            "every node is a row the pane paints"
        );

        app.focus = Focus::Agents;
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('G'),
            KeyModifiers::NONE,
        )));
        assert_eq!(
            app.tree.cursor(),
            app.tree.rows().len() - 1,
            "G names the last painted row"
        );
        assert_eq!(
            app.tree.rows()[app.tree.cursor()].id,
            AgentId(2),
            "and the last painted row is the twin's"
        );

        let (screen, _) = painted(&mut app, 80, 24);
        let Screen::Panes(panes) = screen else {
            panic!("80x24 is above the floor")
        };
        assert_eq!(panes.agents.rows.len(), 3, "both twins are painted");
        assert_eq!(
            panes.agents.cursor, 2,
            "and the cursor names the last painted row"
        );
        assert!(
            !panes.agents.footer.is_empty(),
            "the row under the cursor has its footer, not a skipped index"
        );
    }

    /// `←` in the chat pane is the message box's cursor and moves nothing in the
    /// tree: the two panes keep their own meaning for the same key.
    #[test]
    fn left_in_the_chat_pane_leaves_the_tree_alone() {
        let (mut app, _rx) = test_app("chat-left");
        app.focus = Focus::Chat;
        app.tree.cursor_bottom();
        let before = app.tree.cursor_id();
        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(
            app.tree.cursor_id(),
            before,
            "the chat does not move the tree"
        );
    }

    /// A tree Ctrl-N abandoned can still spawn children, and their `Spawned`
    /// events are dropped — so this is the only moment the UI can tell such a
    /// child to go away. Without it the child runs unseen forever.
    #[test]
    fn a_child_spawned_by_an_abandoned_tree_is_shut_down() {
        let (mut app, _rx) = test_app("stale-child");
        let abandoned = app.tree.conversation();
        ctrl(&mut app, 'n');
        let (child_tx, child_rx) = crossbeam_channel::unbounded::<AgentMsg>();

        app.update(Msg::Agent {
            conversation: abandoned,
            id: AgentId(1),
            event: AgentEvent::Spawned {
                child: 1,
                parent: 0,
                brief: "sneaky".to_string(),
                depth: 1,
                branch: None,
                fork: None,
                title: None,
                cmd: child_tx,
            },
        });

        assert!(
            matches!(child_rx.recv().unwrap(), AgentMsg::Shutdown),
            "the stray child is told to end"
        );
        assert_eq!(app.tree.agents.len(), 1, "and it never joins the new tree");
    }

    /// Ctrl-C cancels work; it must not end an idle root, which only comes
    /// back with Ctrl-N.
    #[test]
    fn ctrl_c_stops_running_agents_only() {
        let (mut app, _rx) = test_app("interrupt");
        app.interrupt();
        assert_eq!(text_of(&app), NOTHING_RUNNING);

        app.chat.insert("hello");
        app.send_message();
        assert_eq!(
            app.tree.agents[0].phase,
            Phase::Thinking,
            "the idle root is usable"
        );

        let (mut app, _rx) = test_app("interrupt-running");
        app.tree.begin(AgentId::ROOT, None);
        app.interrupt();
        assert_eq!(
            app.tree.agents[0].phase,
            Phase::Cancelling,
            "the row must show that a cancel is in flight"
        );
        let rows = screen(&mut app, 120, 32);
        assert!(
            rows.iter()
                .any(|row| row.contains("⊘ #0") && row.contains("cancelling")),
            "the row is where a cancel in flight is drawn: {rows:?}"
        );
        assert!(
            rows.join("\n").contains("cancelling."),
            "and the foot says the same word, dots and all: {rows:?}"
        );
    }

    /// The cancel mark lasts exactly as long as the cancel does: the actor
    /// acknowledges by ending the run, and a fresh run clears it too.
    #[test]
    fn a_cancel_mark_clears_when_the_actor_yields() {
        let (mut app, _rx) = test_app("cancel-mark");
        let conversation = app.tree.conversation();
        app.tree.begin(AgentId::ROOT, None);
        app.interrupt();
        assert_eq!(app.tree.agents[0].phase, Phase::Cancelling);

        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Stopped,
        });
        assert_eq!(
            app.tree.agents[0].phase,
            Phase::Stopped,
            "a stopped run is its own state, not Idle and not Done"
        );

        app.tree.begin(AgentId::ROOT, None);
        app.interrupt();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        });
        assert_eq!(
            app.tree.agents[0].phase,
            Phase::Thinking,
            "a new run is running"
        );
    }

    /// An agent that has not run is not done, and it is not busy: the row must
    /// say so rather than claim a `✓` it never earned.
    #[test]
    fn an_idle_agent_is_neither_done_nor_busy() {
        let (app, _rx) = test_app("idle-phase");
        assert_eq!(app.tree.agents[0].phase, Phase::Idle);
        assert!(!app.busy());
        assert_eq!(app.tree_line(), None, "nothing to report");
    }

    /// The stale-status defect: a finished run must leave nothing behind. Every
    /// line about work in progress is derived from the phase, so when the run
    /// ends they all stop existing at once.
    #[test]
    fn a_finished_run_leaves_nothing_behind() {
        let (mut app, _rx) = test_app("finished-phase");
        let conversation = app.tree.conversation();
        app.tree.begin(AgentId::ROOT, None);
        app.tree.age(AgentId::ROOT, Duration::from_secs(70));
        assert!(
            screen(&mut app, 120, 32)
                .iter()
                .any(|row| row.contains("◐ #0") && row.contains("thinking 1m10s")),
            "a run in flight names its age"
        );

        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Done,
        });
        assert_eq!(app.tree.agents[0].phase, Phase::Done);
        assert!(!app.busy());
        let rows = screen(&mut app, 120, 32).join("\n");
        assert!(!rows.contains("thinking"), "no `thinking` survives: {rows}");
    }

    /// The human's own report, at the surface they read it on: a tool finishes,
    /// the model is asked again, and the row and the foot still said
    /// `run_command …` — the label of work that was over — for the whole model
    /// call. The run here is real and its events travel the UI's channel, so
    /// this reads the phase the actor's own `Thinking` event leaves: the second
    /// reply is held, which is what proves the moment is a request in flight.
    #[test]
    fn a_finished_tool_does_not_hold_the_row_while_the_model_is_asked_again() {
        let root = repo("thinking-between-tools");
        let held = Arc::new(Gate::new());
        let scripted = Arc::new(
            Scripted::new()
                .calls(vec![tool_call(
                    "c0",
                    "run_command",
                    serde_json::json!({ "command": "printf hello > note.txt" }),
                )])
                .held(held.clone())
                .says("wrote note.txt"),
        );
        let (mut app, rx) = app_with_live_scripted_root(&root, scripted.clone());
        app.chat.insert("write note.txt");
        app.send_message();
        assert!(
            held.wait_until_asked(Duration::from_secs(5)),
            "the request after the tool must reach the model"
        );
        // Apply everything the actor said before that request went out — the
        // road the window takes, one `Msg::Agent` at a time.
        while let Ok(msg) = rx.try_recv() {
            app.update(msg);
        }

        let node = app.tree.node(AgentId::ROOT).expect("the root has a row");
        assert_eq!(
            node.phase,
            Phase::Thinking,
            "the finished tool's label is not what the row says while the model is asked again"
        );
        assert_eq!(node.phase.words().as_deref(), Some("thinking"));

        // The held reply ends the run, so the tree is left the way the run
        // left it.
        held.release();
        assert!(
            pump(&mut app, &rx, &scripted, |app, _| app
                .tree
                .node(AgentId::ROOT)
                .is_some_and(|node| node.phase == Phase::Done)),
            "the run finishes"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The cursor row's age is derived once per frame: the footer reads the
    /// activity off the row already built for the list, not the clock again, so
    /// the two surfaces cannot paint two ages for one frame (finding R26).
    #[test]
    fn the_row_and_the_footer_paint_one_age_for_the_cursor() {
        let (mut app, _rx) = test_app("one-age");
        app.tree.begin(AgentId::ROOT, None);
        app.tree.age(AgentId::ROOT, Duration::from_secs(70));
        let frame = screen(&mut app, 120, 32).join("\n");
        assert_eq!(
            frame.matches("thinking 1m10s").count(),
            2,
            "the row and the footer both name the same age: {frame}"
        );
    }

    /// The pane title counts the phases it names, and no agent is in two of its
    /// counts (finding U2).
    ///
    /// It used to say `N running` over every busy phase, so an agent napping on
    /// its children — a row wearing `⏸` — was counted as work the title could
    /// not show. The counts now come from the phases as two disjoint buckets,
    /// and each clause says which one it is.
    #[test]
    fn the_title_counts_working_and_waiting_agents_separately() {
        let (mut app, _rx) = test_app("title-counts");
        let conversation = app.tree.conversation();
        // The root naps on two children, and one of those has a child of its
        // own: three agents work, one waits, and nobody is both.
        for (id, parent, depth) in [(1u64, 0u64, 1usize), (2, 0, 1), (3, 2, 2)] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId(parent),
                event: AgentEvent::Spawned {
                    child: id,
                    parent,
                    brief: format!("child {id}"),
                    depth,
                    branch: None,
                    fork: None,
                    title: None,
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Done,
        });
        // A branch with work on it, so the totals have something to say and
        // must yield the line to the counts rather than be cut in half.
        app.tree.agent_stats.insert(
            AgentId(1),
            mush_core::git::Stat {
                files: 1,
                added: 324,
                removed: 40,
            },
        );
        assert_eq!(
            app.tree.roster(),
            crate::app::tree::Roster {
                working: 3,
                waiting: 1
            }
        );

        let rows = screen(&mut app, 200, 50);
        assert!(
            rows[0].contains(" agents · 3 working · 1 waiting"),
            "the title counts what it names: {}",
            rows[0]
        );
        assert!(
            !rows[0].contains("Σ +324 −") || rows[0].contains("Σ +324 −40"),
            "a total is painted whole or not at all: {}",
            rows[0]
        );
    }

    /// The machine's load is a fact on the pane title: a row's `⚙N` is one
    /// agent's share, and the whole tree's count is what says why a box full of
    /// parallel worktrees is slow (finding H8).
    #[test]
    fn the_title_names_the_machines_load() {
        let (mut app, _rx) = test_app("title-load");
        assert!(
            !screen(&mut app, 120, 32)[0].contains("job"),
            "an idle machine says nothing about jobs"
        );
        running_job(&mut app, 0);
        running_job(&mut app, 0);
        let rows = screen(&mut app, 120, 32);
        assert!(rows[0].contains(" agents · 2 jobs"), "{}", rows[0]);
    }

    /// A clause that does not fit is dropped whole, and the totals are the last
    /// to go: the pane is at most 46 columns wide, and ` agents · 1 working · 1
    /// waiting · Σ +324 −40` is 44 of them — the half of it that would fit
    /// (`Σ +324 −`) is a total that is not the total.
    #[test]
    fn the_title_elides_clauses_instead_of_cutting_numbers() {
        let (mut app, _rx) = test_app("title-elides");
        app.tree.agent_stats.insert(
            AgentId::ROOT,
            mush_core::git::Stat {
                files: 1,
                added: 324,
                removed: 40,
            },
        );
        // A running child: one agent working, and a root that is waiting on it.
        crowd(&mut app, 1);

        // Widest pane `draw` ever gives this pane: every clause fits, totals
        // included, and each one is whole.
        let wide = screen(&mut app, 200, 50);
        assert!(
            wide[0].contains(" agents · 1 working · 1 waiting · Σ +324 −40"),
            "{}",
            wide[0]
        );

        // The narrow pane (80 columns, where the tree keeps its thirty): the
        // totals go first, then the waiting count, and never mid-number.
        let narrow = screen(&mut app, 80, 24);
        assert!(!narrow[0].contains("Σ"), "{}", narrow[0]);
        assert!(!narrow[0].contains("waiting"), "{}", narrow[0]);
        assert!(narrow[0].contains(" agents · 1 working"), "{}", narrow[0]);
    }

    /// The pane paints tree order, and every key that moves or reads the cursor
    /// follows what it paints: `j`/`k` walk the rows, `g`/`G` land on the first
    /// and last of them, `Enter` focuses the agent under the highlight, and the
    /// footer under the list names that same agent (finding U4).
    #[test]
    fn the_cursor_walks_the_rows_the_pane_paints() {
        let (mut app, _rx) = test_app("row-order");
        let conversation = app.tree.conversation();
        // Spawn order that is not tree order: the root's second child is spawned
        // before the first child's own child, so `#3` belongs under `#1` and
        // above `#2`.
        for (id, parent, depth) in [(1u64, 0u64, 1usize), (2, 0, 1), (3, 1, 2)] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId(parent),
                event: AgentEvent::Spawned {
                    child: id,
                    parent,
                    brief: format!("agent {id}"),
                    depth,
                    branch: None,
                    fork: None,
                    title: None,
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }
        app.focus = Focus::Agents;
        // The pane's own columns only: the bar under it names agents too.
        let pane = |app: &mut App| -> Vec<String> {
            screen(app, 120, 32)
                .into_iter()
                .map(|row| row.chars().take(31).collect())
                .collect()
        };
        let key = |app: &mut App, c: char| {
            app.update(Msg::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        };

        // Painted order: the root, #1, #3 (its child), then #2 — while the
        // storage order is the spawn order 0, 1, 2, 3.
        let rows = pane(&mut app);
        for (row, id) in rows[1..=4].iter().zip(["#0", "#1", "#3", "#2"]) {
            assert!(row.contains(id), "the row for {id} is not there: {row:?}");
        }

        // `j` twice lands on the third painted row, which is #3: storage order
        // would have put its sibling #2 there.
        key(&mut app, 'j');
        key(&mut app, 'j');
        assert_eq!(app.tree.cursor_id(), Some(AgentId(3)));
        assert_eq!(app.tree.agents[2].id, AgentId(2), "storage order is not it");

        // `Enter` focuses the row the highlight is on, and the footer under the
        // list is that same row's facts.
        app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(app.tree.focused, AgentId(3));
        assert_eq!(text_of(&app), "agent #3: agent 3");
        let rows = pane(&mut app);
        assert!(
            rows.iter().any(|row| row.contains("#3 agent 3")),
            "the footer names the cursor row: {rows:?}"
        );

        // `Enter` hands the keyboard to the agent it focused (finding S2), so
        // the rest of this walk — which is about the *rows* — takes it back the
        // way a human does.
        app.focus = Focus::Agents;

        // `k` back up one row is #1, and `G` is the last painted row — the
        // root's second child, whose storage index is 2 of 3.
        key(&mut app, 'k');
        assert_eq!(app.tree.cursor_id(), Some(AgentId(1)));
        key(&mut app, 'G');
        assert_eq!(app.tree.cursor_id(), Some(AgentId(2)));
        key(&mut app, 'g');
        assert_eq!(app.tree.cursor_id(), Some(AgentId::ROOT));
    }

    /// A napping root with live children is derived from the tree: nobody has
    /// to remember to write it, so it cannot be forgotten either.
    #[test]
    fn a_napping_root_reports_its_children() {
        let (mut app, _rx) = test_app("napping-root");
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId(1),
            event: AgentEvent::Spawned {
                child: 1,
                parent: 0,
                brief: "lexer".to_string(),
                depth: 1,
                branch: None,
                fork: None,
                title: None,
                cmd: crossbeam_channel::unbounded().0,
            },
        });
        app.update(Msg::Agent {
            conversation,
            id: AgentId(1),
            event: AgentEvent::Status("edit_file src/lex.rs".to_string()),
        });
        assert_eq!(app.tree.agents[0].phase, Phase::Idle, "the root napped");
        assert_eq!(
            app.tree_line().as_deref(),
            Some("waiting on 1 subagent(s) — the root resumes as they finish")
        );
    }

    /// A working agent with working children is drawn working, and the children
    /// are a second mark rather than a replacement glyph (finding U1).
    ///
    /// The row used to derive its glyph from "has live children", so an agent
    /// mid-turn with children running wore `⏸` — "paused" about the one agent
    /// the human was watching work. The glyph is now a function of the agent's
    /// own phase and `⏸N` carries the children, so neither fact hides the other.
    #[test]
    fn a_working_agent_with_working_children_is_not_drawn_paused() {
        let (mut app, _rx) = test_app("waiting-glyph");
        let conversation = app.tree.conversation();
        // The root is mid-turn, and two of its children are working.
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        });
        for id in [1u64, 2u64] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId::ROOT,
                event: AgentEvent::Spawned {
                    child: id,
                    parent: 0,
                    brief: format!("child {id}"),
                    depth: 1,
                    branch: None,
                    fork: None,
                    title: None,
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }

        let rows = screen(&mut app, 120, 32);
        let root_row = rows
            .iter()
            .find(|row| row.contains("#0"))
            .expect("the root has a row")
            .clone();
        assert!(
            root_row.contains("◐ #0"),
            "a working agent is `◐`, never `⏸`: {root_row}"
        );
        assert!(
            root_row.contains("⏸2"),
            "and its two working children are still on the row: {root_row}"
        );

        // A child with no children of its own wears the plain running glyph.
        let child_row = rows
            .iter()
            .find(|row| row.contains("#1"))
            .expect("the child has a row")
            .clone();
        assert!(child_row.contains("◐ #1"), "{child_row}");
        assert!(!child_row.contains("⏸"), "{child_row}");
    }

    /// A pane's position is the human's: another agent's line cannot move it,
    /// and neither can the pane's own line while they are away from the bottom
    /// (finding U3).
    #[test]
    fn news_moves_only_the_pane_it_is_about_and_only_from_the_bottom() {
        let (mut app, _rx) = test_app("scroll-pin");
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Spawned {
                child: 1,
                parent: 0,
                brief: "lexer".to_string(),
                depth: 1,
                branch: None,
                fork: None,
                title: None,
                cmd: crossbeam_channel::unbounded().0,
            },
        });
        // A transcript long enough to scroll, in the pane the human is reading.
        for index in 0..30 {
            app.chat
                .push_message(AgentId(1), Message::assistant(format!("line {index}")));
        }
        app.tree.focus(AgentId(1));
        app.chat.scroll_by(AgentId(1), 4);
        let held = chat_rows(&mut app);
        assert!(
            !held.join("\n").contains("line 29"),
            "the pane is away from the newest line: {held:?}"
        );
        // The bar is not the pane: its context meter legitimately follows the
        // transcript the news just grew, and this test is about the reading
        // position. At 120×32 the bar is the last two rows.
        let pane = |rows: &[String]| rows[..rows.len().saturating_sub(2)].to_vec();

        // Another agent's news: the root says something of its own.
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Message(Message::assistant("the root's line")),
        });
        assert_eq!(
            pane(&chat_rows(&mut app)),
            pane(&held),
            "another agent's line must not move this pane"
        );

        // And the pane's own agent speaks while the human is away from the
        // bottom: they are still where they put themselves.
        app.update(Msg::Agent {
            conversation,
            id: AgentId(1),
            event: AgentEvent::Message(Message::assistant("its own line")),
        });
        assert_eq!(
            pane(&chat_rows(&mut app)),
            pane(&held),
            "a held pane is the human's"
        );

        // At the bottom the same news is exactly what the pane shows: that is
        // what following means, and it needs nothing to be told to it.
        app.chat.scroll_by(AgentId(1), -4);
        assert!(
            chat_rows(&mut app).join("\n").contains("its own line"),
            "a pane at the bottom follows the newest line"
        );
    }

    /// Busy agents are named with their age, on their own row: a model that
    /// has thought for two minutes should look different from one that has
    /// thought for a second, and the row is where that is read.
    #[test]
    fn busy_agents_are_named_with_their_age() {
        let (mut app, _rx) = test_app("activity-age");
        app.tree.begin(AgentId::ROOT, None);
        app.tree.activity(AgentId::ROOT, "edit_file src/lib.rs");
        app.tree.age(AgentId::ROOT, Duration::from_secs(12));
        let rows = screen(&mut app, 120, 32);
        assert!(
            rows.iter()
                .any(|row| row.contains("◐ #0") && row.contains("edit_file src/lib.rs 12s")),
            "the row names the work and its age: {rows:?}"
        );
        // And the first line of the bar says something else: the activity has
        // two homes already, and the bar has only one line (finding U5).
        let bar = rows.last().expect("the bar is painted");
        assert!(
            !bar.contains("edit_file"),
            "the bar does not repeat the row: {bar:?}"
        );
    }

    /// An informational line is worth reading for a moment; an error is worth
    /// reading until it is fixed or replaced.
    #[test]
    fn info_fades_and_errors_stay() {
        let (mut app, _rx) = test_app("status-life");
        // Nothing is said at startup: the bar's facts line already carries the
        // model and the window, so a status line would only repeat it.
        assert_eq!(text_of(&app), "");

        app.say("saved src/main.rs");
        assert_eq!(text_of(&app), "saved src/main.rs");
        age_status(&mut app, 6);
        assert_eq!(text_of(&app), "", "an info line fades");
        app.tick();
        assert!(app.status.is_none(), "and is dropped, not just hidden");

        app.fail("cannot write /etc/passwd");
        age_status(&mut app, 600);
        assert_eq!(text_of(&app), "cannot write /etc/passwd");
        assert_eq!(
            app.status_line().map(|(_, kind)| kind),
            Some(StatusKind::Error)
        );
        assert_eq!(
            app.status_line().map(|(_, kind)| kind),
            Some(StatusKind::Error)
        );
    }

    /// A child's run dies while the human is reading the root. The failure has
    /// to be visible *when it happens* — the row's `✗`, the bar, and the child's
    /// own foot — not only when something later asks the child for its state
    /// (finding B27's live shape: an agent's run died with a framing error and
    /// the human learned what broke from a line that blamed the endpoint).
    #[test]
    fn a_childs_failure_is_visible_when_it_happens() {
        let (mut app, _rx) = test_app("child-failure-visible");
        let _child = finished_child(&mut app, 1);
        begin_run(&mut app, AgentId::ROOT);
        let error = "the reply from http://127.0.0.1:1 broke before it could be \
                     read: malformed chunk size: \"\"";

        app.on_agent(AgentId(1), AgentEvent::Error(error.to_string()));

        // The row: `✗`, derived the moment the event lands.
        assert_eq!(
            app.tree.node(AgentId(1)).map(|node| &node.phase),
            Some(&Phase::Failed(error.to_string())),
            "the failed child's row wears `✗`"
        );
        // The bar: named, even though the human is reading the root — a row can
        // be scrolled out of the history window, and the bar is the one line
        // that is always in front of them.
        let (line, kind) = app.status_line().expect("the bar says what happened");
        assert_eq!(kind, StatusKind::Error, "and it stays until replaced");
        assert!(line.contains("agent #1 failed"), "{line}");
        assert!(line.contains("malformed chunk size"), "{line}");
        // The child's own pane: the durable `!` line under its row.
        let notes = app.chat.notes_report(AgentId(1), 0, 200).rows.join("\n");
        assert!(
            notes.contains("malformed chunk size"),
            "the pane it happened to says so too: {notes}"
        );
        // And nothing of it landed in the root's pane: a child's failure is the
        // child's line (finding B19).
        let root_notes = app.chat.notes_report(AgentId::ROOT, 0, 200).rows.join("\n");
        assert!(
            !root_notes.contains("malformed chunk size"),
            "a child's failure is not the root's notice: {root_notes}"
        );
    }

    /// Ages are read at a glance, so they must not be raw seconds.
    #[test]
    fn ages_read_like_clocks() {
        assert_eq!(short_age(Duration::from_secs(3)), "3s");
        assert_eq!(short_age(Duration::from_secs(70)), "1m10s");
        assert_eq!(short_age(Duration::from_secs(3600 + 120)), "1h02m");
    }

    /// The commands an untrusted word can carry: an OSC title rename, a carriage
    /// return, a bidi isolate. Every one of them has leaked onto some one-line
    /// surface of this screen at least once (findings U9/N3).
    const HOSTILE: &str = "\x1b]0;PWNED\x07\r\u{2066}escaped";

    /// Every size the draw sweep paints at: the seven the §4.5 audit
    /// photographs, plus the tiers' own edges — the compact cut at 80 wide and
    /// 20 tall, the bar's second row at 24 — and the degenerate 1×1 a resize can
    /// reach mid-frame.
    const SWEEP_SIZES: &[(u16, u16)] = &[
        (200, 50),
        (160, 26),
        (120, 32),
        (100, 25),
        (80, 24),
        (79, 24),
        (60, 20),
        (60, 17),
        (50, 12),
        (40, 10),
        (39, 10),
        (39, 9),
        (30, 8),
        (20, 5),
        (1, 1),
    ];

    /// One state of the draw sweep: an app, and the words its frame must have
    /// painted.
    ///
    /// `words` are painted at *every* size with a floor to paint in: the facts
    /// that must survive the smallest screen, and the ones a bug in the size
    /// tiers would take away. `roomy` are painted at the two presentation sizes
    /// the audit photographs (200×50 and 120×32), where the transcript's foot,
    /// the selected row's footer and the facts line all have the room they were
    /// built for — and, per state, with both focus states, because a focused
    /// pane's border, its highlight and the chat cursor are painted differently
    /// and the rewrite had dropped that half of the old sweep (finding V3).
    /// `reopen` is for the popups whose
    /// item wrapping is derived from the terminal's width *when they open*
    /// (`/notes`): the sweep opens them again for each size, which is what a
    /// human resizing the terminal with the popup up would get.
    ///
    /// `absent` is the other half of `words`: facts the frame must *not* carry
    /// at any size with a floor to paint in, for a state whose point is an
    /// absence — a pane with no foot row must not claim hidden lines (finding
    /// V7).
    struct Sweep {
        name: &'static str,
        app: App,
        words: Vec<&'static str>,
        roomy: Vec<&'static str>,
        absent: Vec<&'static str>,
        reopen: Option<fn(&mut App)>,
    }

    /// One agent, as its parent reports it to the UI.
    fn spawn_agent(
        app: &mut App,
        id: u64,
        parent: u64,
        depth: usize,
        brief: &str,
        branch: Option<&str>,
    ) {
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId(parent),
            event: AgentEvent::Spawned {
                child: id,
                parent,
                depth,
                brief: brief.to_string(),
                branch: branch.map(str::to_string),
                fork: None,
                title: None,
                cmd: crossbeam_channel::unbounded().0,
            },
        });
    }

    /// A run in flight on one agent: what its actor reports when it starts.
    fn begin_run(app: &mut App, id: AgentId) {
        app.on_agent(
            id,
            AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
    }

    /// A job running on this machine, launched through the one registry the
    /// rows, the selected row's footer and the bar all read.
    fn running_job(app: &mut App, owner: u64) {
        running_job_on(app, owner);
    }

    /// The same, with the machine handed back so a test can ask it what was
    /// killed; the *command* half of "is this job really gone" is not visible
    /// through the registry (its record survives the kill, as a stopped job).
    fn running_job_on(app: &mut App, owner: u64) -> Arc<crate::machine::fake::Scripted> {
        use crate::jobs::Launch;
        use crate::machine::fake::{Script, Scripted};
        use crate::machine::{Machine, ShellCommand};

        let machine = Arc::new(Scripted::new().runs(Script::hangs()));
        let job = machine
            .spawn(&ShellCommand {
                command: "cargo build --release",
                root: std::path::Path::new("/tmp"),
            })
            .unwrap();
        let (mailbox, _rx) = crossbeam_channel::unbounded();
        app.tree
            .handles()
            .jobs
            .launch(Launch::started(
                owner,
                "cargo build --release".to_string(),
                false,
                mailbox,
                job,
            ))
            .unwrap();
        machine
    }

    /// Twenty agents three levels deep, with a branch on one of them, a run in
    /// flight, a failure and a dirty repository: the shape the audit's worst
    /// screen was.
    fn a_twenty_agent_tree(label: &str) -> (App, Receiver<Msg>) {
        let (mut app, rx) = test_app(label);
        for id in 1..=12u64 {
            // One isolated child, one working, one failed: the three states a
            // row's tail can carry.
            let branch = (id == 3).then(|| format!("mush/{id}"));
            spawn_agent(
                &mut app,
                id,
                0,
                1,
                &format!("task {id} for the sweep"),
                branch.as_deref(),
            );
        }
        for id in 13..=19u64 {
            spawn_agent(&mut app, id, 1, 2, &format!("grandchild {id}"), None);
        }
        // A mixed tree: the states a row can wear, so the title's counts count
        // something. Everything `insert` made thinking is put back at rest
        // except the children that stay in flight.
        app.tree.idle(AgentId(1));
        app.tree.activity(AgentId(2), "edit_file src/lexer.rs");
        app.tree
            .finish(AgentId(3), Some("the parser is ported".to_string()));
        app.tree.fail(AgentId(4), "no route to host".to_string());
        app.tree.stopped(AgentId(5));
        for id in 13..=19u64 {
            app.tree.idle(AgentId(id));
        }
        app.tree.agent_stats.insert(
            AgentId(3),
            git::Stat {
                files: 2,
                added: 12,
                removed: 4,
            },
        );
        // A dirty repository read a moment ago, so the facts line has its
        // branch, its dirty count and its delta.
        app.git = Some(git::RepoStatus {
            branch: "main".to_string(),
            dirty: 3,
            stat: git::Stat {
                files: 1,
                added: 9,
                removed: 2,
            },
        });
        app.git_at = Some(Instant::now());
        (app, rx)
    }

    /// The states the sweep paints: one per fact a frame can carry, each built
    /// the way its actor, its file or its keyboard would build it.
    ///
    /// The receivers are handed back with the states so the UI channel each app
    /// was built with stays alive for as long as the app does — a dropped
    /// receiver is a message that never lands.
    fn sweep_states() -> (Vec<Sweep>, Vec<Receiver<Msg>>) {
        let mut states: Vec<Sweep> = Vec::new();
        let mut keep = Vec::new();

        // Nothing has happened yet.
        let (fresh, rx) = test_app("sweep-fresh");
        states.push(Sweep {
            name: "fresh",
            app: fresh,
            words: vec!["· #0", " agents ", " chat ", "Tab cycles panes"],
            roomy: vec!["you", " message ", "Ask for a change"],
            absent: Vec::new(),
            reopen: None,
        });
        keep.push(rx);

        // A run in flight, naming the tool it is running — on the row and in the
        // foot, in the same words, and never the old generic `working.` (the
        // foot reads the same `Phase::words` the row does).
        let (mut run, rx) = test_app("sweep-run");
        begin_run(&mut run, AgentId::ROOT);
        run.on_agent(
            AgentId::ROOT,
            AgentEvent::Status("edit_file src/lib.rs".into()),
        );
        states.push(Sweep {
            name: "a run in flight",
            app: run,
            words: vec![
                "◐ #0",
                "edit_file src/lib.rs",
                "edit_file src/lib.rs.",
                " chat ",
            ],
            roomy: vec![" agents · 1 working"],
            absent: vec!["working."],
            reopen: None,
        });
        keep.push(rx);

        // A run parked on somebody else's result: the hourglass, never the
        // working icon, and a foot that *says* what the wait is on in the row's
        // own words (finding U14; finding U7's silence superseded). Painted at
        // every size, because the glyph is a column the rows are fitted with.
        let (mut waiting, rx) = test_app("sweep-waiting");
        begin_run(&mut waiting, AgentId::ROOT);
        waiting.on_agent(AgentId::ROOT, AgentEvent::Status("wait ".into()));
        states.push(Sweep {
            name: "a run parked in a wait",
            app: waiting,
            words: vec!["⧗ #0", "waiting on results", "waiting on results."],
            roomy: vec![" agents · 1 waiting"],
            absent: vec!["◐ #0", "working."],
            reopen: None,
        });
        keep.push(rx);

        // A root parked on a child's result — and a grandchild of its own in
        // flight under that child, because the count the bar prints, the `⏸N`
        // on the row and the title's `M waiting` all have to be about the
        // root's *own* children (finding U2).
        let (mut parked, rx) = test_app("sweep-parked");
        spawn_agent(
            &mut parked,
            1,
            0,
            1,
            "build the lexer for the config format",
            None,
        );
        spawn_agent(&mut parked, 2, 1, 2, "tokenize the samples", None);
        states.push(Sweep {
            name: "parked on children",
            app: parked,
            words: vec!["⏸1", "2 working", "waiting on 1 subagent"],
            roomy: vec!["◐ #1", "◐ #2", "lexer", " agents · 2 working · 1 waiting"],
            absent: Vec::new(),
            reopen: None,
        });
        keep.push(rx);

        // The three folds the tree can be doing, each in its own words on the
        // row and in the foot (the foot's dots land behind the fold's sentence,
        // never `working` over it). The transcript is left empty on purpose: at
        // 40×10 a pane with messages protects its last row, and the foot that
        // carries the fold's longer spelling is the row it protects it *from* —
        // the existing fold test keeps the message case.
        for (label, why, words, foot) in [
            (
                "sweep-fold-requested",
                Compacting::Requested,
                "compacting",
                "compacting.",
            ),
            (
                "sweep-fold-parked",
                Compacting::Parked,
                "folding at the next step",
                "folding at the next step.",
            ),
            (
                "sweep-fold-nearly-full",
                Compacting::NearlyFull,
                "context nearly full",
                "context nearly full — compacting.",
            ),
        ] {
            let (mut fold, rx) = test_app(label);
            fold.on_agent(AgentId::ROOT, AgentEvent::Compacting { why, cancel: None });
            states.push(Sweep {
                name: "a fold",
                app: fold,
                words: vec!["≡ #0", words, foot, "keep typing"],
                roomy: vec!["your message is answered after the fold"],
                absent: vec!["working."],
                reopen: None,
            });
            keep.push(rx);
        }

        // A failed run: the model's own error words, on the row, the foot and
        // the bar.
        let (mut failed, rx) = test_app("sweep-failed");
        begin_run(&mut failed, AgentId::ROOT);
        failed.on_agent(AgentId::ROOT, AgentEvent::Error("no route to host".into()));
        states.push(Sweep {
            name: "a failed run",
            app: failed,
            words: vec!["✗ #0", "no route to host", "agent #0 failed"],
            roomy: vec!["! no route to host"],
            absent: Vec::new(),
            reopen: None,
        });
        keep.push(rx);

        // A stopped run is not a failure and must not borrow `✓`.
        let (mut stopped, rx) = test_app("sweep-stopped");
        begin_run(&mut stopped, AgentId::ROOT);
        stopped.on_agent(AgentId::ROOT, AgentEvent::Stopped);
        states.push(Sweep {
            name: "a stopped run",
            app: stopped,
            words: vec!["⊘ #0", "stopped"],
            roomy: vec!["re-send to resume"],
            absent: Vec::new(),
            reopen: None,
        });
        keep.push(rx);

        // A session restored from a hand-written file: a failed child and the
        // failure mush wrote about the root's last run.
        states.push(Sweep {
            name: "a restored session",
            app: restored_session("sweep-restored"),
            words: vec!["· #0", " agents ", " chat ", "mush › "],
            roomy: vec!["✗ #1", "mush/1", "! the endpoint returned 503"],
            absent: Vec::new(),
            reopen: None,
        });

        // A pane was scrolled away from the bottom: the title says so.
        let (mut scrolled, rx) = test_app("sweep-scrolled");
        for i in 1..=30 {
            scrolled
                .chat
                .push_message(AgentId::ROOT, Message::user(format!("message {i}")));
            scrolled
                .chat
                .push_message(AgentId::ROOT, Message::assistant(format!("reply {i}")));
        }
        scrolled.chat.scroll_by(AgentId::ROOT, 5);
        states.push(Sweep {
            name: "a transcript scrolled back",
            app: scrolled,
            words: vec!["scrolled ↑5 rows", "PgDn"],
            roomy: vec![],
            absent: Vec::new(),
            reopen: None,
        });
        keep.push(rx);

        // More notes than the foot has rows: the pane says how many it is
        // hiding, and where to read them.
        let (mut noted, rx) = test_app("sweep-notes-foot");
        for i in 1..=7 {
            noted
                .chat
                .note_for(AgentId::ROOT, format!("mush wrote this: note {i}"));
        }
        states.push(Sweep {
            name: "a foot with hidden lines",
            app: noted,
            words: vec!["more lines", "/notes"],
            roomy: vec!["note 7", "note 6", "+5 more lines"],
            absent: Vec::new(),
            reopen: None,
        });
        keep.push(rx);

        // A job on this machine: the row's badge, the footer's line and the
        // bar's report, all from the one registry.
        let (mut jobbed, rx) = test_app("sweep-job");
        running_job(&mut jobbed, 0);
        jobbed.on_agent(
            AgentId::ROOT,
            AgentEvent::JobStarted {
                job: JobId(1),
                command: "cargo build --release".to_string(),
            },
        );
        states.push(Sweep {
            name: "a job",
            app: jobbed,
            words: vec!["⚙1", "#c1"],
            roomy: vec![" 1 jobs · #c1"],
            absent: Vec::new(),
            reopen: None,
        });
        keep.push(rx);

        // Twenty agents, three levels deep, with a dirty repository. The
        // cursor sits mid-tree so the window has rows hidden on *both* sides:
        // the sweep reads the `▲`/`▼` split back against the rows painted
        // between them, where a top cursor would only ever exercise `▼`
        // (finding V7).
        let (mut twenty, twenty_rx) = a_twenty_agent_tree("sweep-twenty");
        twenty.tree.move_cursor(5);
        states.push(Sweep {
            name: "twenty agents",
            app: twenty,
            words: vec![" agents · ", " chat "],
            roomy: vec![
                "8 working",
                "1 waiting",
                "grandchild",
                "mush/3",
                "main ±3 +9−2",
            ],
            absent: Vec::new(),
            reopen: None,
        });
        keep.push(twenty_rx);

        // The `/model` picker.
        let (mut models, rx) = test_app("sweep-model-picker");
        models.models = vec![
            http::Model {
                id: "test-model".to_string(),
                context: Some(500_000),
            },
            http::Model {
                id: "deepseek-chat".to_string(),
                context: Some(128_000),
            },
            // An endpoint names its own models, and the picker paints the name
            // whole: this one is a command, and the sweep's cell check is what
            // reads it.
            http::Model {
                id: format!("{HOSTILE}model"),
                context: None,
            },
        ];
        models.open_model_picker();
        states.push(Sweep {
            name: "an open /model picker",
            app: models,
            words: vec![
                " models · Enter picks ",
                "• test-model · 500k",
                "j/k or PgUp/PgDn",
            ],
            roomy: vec!["deepseek-chat · 128k"],
            absent: Vec::new(),
            reopen: None,
        });
        keep.push(rx);

        // The `/notes` popup, whose wrapping is derived from the terminal's
        // width when it opens — so it is opened again for every size.
        let (mut notes, rx) = test_app("sweep-notes-picker");
        notes.chat.note_for(
            AgentId::ROOT,
            "the endpoint returned 503 while the run was folding the transcript",
        );
        notes.open_notes_picker();
        states.push(Sweep {
            name: "an open /notes popup",
            app: notes,
            words: vec![
                "notes · newest last",
                "the endpoint returned 503",
                "j/k or PgUp/PgDn",
            ],
            roomy: vec![],
            absent: Vec::new(),
            reopen: None,
        });
        keep.push(rx);

        // A busy root whose one message the pane protects at the smallest
        // sizes: the foot has no row there, so the frame must not claim hidden
        // lines anywhere — a derived activity line is not a line `/notes` can
        // answer (finding V7). The line the foot *would* paint is the phase's
        // own word, not the old generic `working.`.
        let (mut spinner, rx) = test_app("sweep-spinner");
        spinner
            .chat
            .push_message(AgentId::ROOT, Message::user("what is happening"));
        begin_run(&mut spinner, AgentId::ROOT);
        states.push(Sweep {
            name: "a working line with no foot row",
            app: spinner,
            words: vec!["◐ #0"],
            roomy: vec!["thinking."],
            absent: vec!["more lines"],
            reopen: None,
        });
        keep.push(rx);

        // Untrusted words on every one-line surface at once: a model reply, a
        // run's failure, a brief, a job command and a model id, each carrying
        // the commands the verifier used.
        let (mut hostile, rx) = test_app("sweep-hostile");
        spawn_agent(&mut hostile, 1, 0, 1, HOSTILE, Some("mush/1"));
        // The cursor is on the child, whose brief is the hostile text: the
        // selected row's *footer* is the one-line surface that carries a brief
        // through `truncate`, and the root's own failure is on the bar.
        hostile.tree.move_cursor(1);
        hostile
            .chat
            .push_message(AgentId::ROOT, Message::assistant(HOSTILE));
        hostile.on_agent(AgentId::ROOT, AgentEvent::Error(HOSTILE.to_string()));
        hostile.chat.note_for(AgentId::ROOT, HOSTILE);
        states.push(Sweep {
            name: "hostile words",
            app: hostile,
            words: vec!["escaped", "␍", " chat "],
            roomy: vec!["escaped", "␍"],
            absent: Vec::new(),
            reopen: None,
        });
        keep.push(rx);

        (states, keep)
    }

    /// A workspace whose `.mush/session.json` was written by hand: two root
    /// messages, a child whose last run failed, and the failure notice mush
    /// stored about the root. The app that adopts it is what a restart paints.
    fn restored_session(label: &str) -> App {
        let root = dir(label);
        let session = root.join(".mush/session.json");
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        // The stored worktree is really on disk: a restored isolated agent
        // keeps its branch, and the sweep photographs a row with one. A branch
        // whose worktree is gone is the *other* case — it is dropped, and
        // `a_restored_branch_whose_worktree_is_gone_is_dropped` is where that
        // is pinned (finding U13).
        std::fs::create_dir_all(root.join(".mush/wt/1")).unwrap();
        // Written as text, not built from `Session`: a hand-edited file is the
        // input this path has to survive, and a round trip through the writer
        // would test the writer instead.
        std::fs::write(
            &session,
            r#"{
  "root": "/tmp/sweep-restored",
  "model": "test-model",
  "updated": 1,
  "messages": [
    {"role": "user", "content": "port the parser module"},
    {"role": "assistant", "content": "done, mostly"}
  ],
  "agents": [
    {
      "id": 1,
      "parent": 0,
      "depth": 1,
      "brief": "port the parser module",
      "branch": "mush/1",
      "status": {"failed": "no route to host"},
      "messages": []
    }
  ],
  "notices": [
    {
      "agent": 0,
      "at": 1,
      "text": "the endpoint returned 503 while the run was folding the transcript"
    }
  ]
}
"#,
        )
        .unwrap();
        reopened(&root)
    }

    /// The draw sweep: every state, at every size, read as *text*.
    ///
    /// What it replaced asserted "does not panic" — which is what a layout
    /// arithmetic bug looks like from the outside and nothing more. This one
    /// asserts what a frame *says*: the words each state is about, that no pane
    /// painted over its own border or outside its rect, that the bar keeps its
    /// row, that every line a pane was handed fits the pane, that no cell is a
    /// control byte, and that the frame is exactly the terminal's size. The
    /// words are the evidence; a layout that silently drops a fact is the bug
    /// class the audit's ten defects all belonged to (refactor B17).
    #[test]
    fn the_draw_sweep_asserts_painted_text_not_that_it_did_not_panic() {
        let paint = shot;
        let (mut states, _keep) = sweep_states();
        for state in &mut states {
            let Sweep {
                name,
                app,
                words,
                roomy,
                absent,
                reopen,
            } = state;
            for &(width, height) in SWEEP_SIZES {
                // A popup whose contents are wrapped to the terminal's width is
                // opened again for each size, the way a resize would.
                if let Some(reopen) = reopen {
                    app.set_term_size(width, height);
                    reopen(app);
                }
                let shot = shot(app, width, height);
                let at = format!("{name} at {width}×{height}");
                // The frame is the terminal: every row, every column, and not
                // one cell that commands the display instead of being read.
                assert_eq!(shot.cells.len(), height as usize, "{at}: the rows");
                assert!(
                    shot.cells.iter().all(|row| row.len() == width as usize),
                    "{at}: a row is not {width} columns wide"
                );
                assert_no_command(&at, &shot);

                if is_below_floor(width, height) {
                    // Below the floor: one notice, centred on both axes, and
                    // *nothing else* painted — the panes are not built at all,
                    // so nothing can leak through the floor (R3, finding P11).
                    assert!(
                        matches!(shot.screen, Screen::Floor { .. }),
                        "{at}: the floor notice"
                    );
                    let painted: Vec<usize> = (0..height as usize)
                        .filter(|&y| !shot.line(y as u16).trim().is_empty())
                        .collect();
                    assert_eq!(
                        painted,
                        vec![(height as usize).saturating_sub(1) / 2],
                        "{at}: one row, at the middle: {painted:?}"
                    );
                    if width >= 5 {
                        let floor = format!("{MIN_WIDTH}×{MIN_HEIGHT}");
                        assert!(
                            shot.line(painted[0] as u16).contains(&floor),
                            "{at}: the notice must name the floor"
                        );
                    }
                    continue;
                }

                let text = shot.text();
                for word in words.iter() {
                    assert!(text.contains(word), "{at}: must paint `{word}`:\n{text}");
                }
                for word in absent.iter() {
                    assert!(
                        !text.contains(word),
                        "{at}: must not paint `{word}`:\n{text}"
                    );
                }
                // The title's `▲`/`▼` must be the window the painter really
                // left, and the window must hold the rows it claims — read back
                // from the cells, not trusted from the derivation. A popup is
                // skipped: its `Clear` erases the list cells this counts
                // (findings V1/V7).
                if matches!(&shot.screen, Screen::Panes(panes) if panes.picker.is_none()) {
                    assert_window_counts(&at, &shot);
                }
                // The audit's two presentation sizes: the widest terminal mush
                // is photographed on and the one a working session sits at.
                // Every pane — the two-column tree, a foot with a count row, the
                // bar's facts line — has the room it was built for there, which
                // is what makes a word missing here a word that exists nowhere.
                if (width, height) == (200, 50) || (width, height) == (120, 32) {
                    for word in roomy.iter() {
                        assert!(text.contains(word), "{at}: must paint `{word}`:\n{text}");
                    }
                    // The other focus state, at the same size: the words must
                    // not depend on which pane holds the keyboard, and the
                    // focused border and the cursor are the parts a one-focus
                    // sweep never painted (finding V3).
                    let was = app.focus;
                    app.focus = match was {
                        Focus::Chat => Focus::Agents,
                        Focus::Agents => Focus::Chat,
                    };
                    let other = paint(app, width, height);
                    let text = other.text();
                    for word in roomy.iter() {
                        assert!(
                            text.contains(word),
                            "{at}: must paint `{word}` with {:?} focused:\n{text}",
                            app.focus
                        );
                    }
                    other.assert_shape(name, width, height);
                    app.focus = was;
                }
                shot.assert_shape(name, width, height);
            }
        }
    }

    /// A wide glyph is two columns of the pane, not one, and every pane paints
    /// it inside its own border: the row's fields, the transcript's wrap, the
    /// message box and the footer are all column arithmetic, and a width taken
    /// for granted is how a CJK row ends up a column over its border (finding
    /// B9).
    #[test]
    fn the_sweep_fits_wide_glyphs_in_their_panes() {
        let (mut app, _rx) = test_app("sweep-wide-glyphs");
        spawn_agent(&mut app, 1, 0, 1, "移植解析器与词法分析", Some("mush/1"));
        app.chat.push_message(
            AgentId::ROOT,
            Message::assistant("请把解析器移植过来，然后运行测试"),
        );
        app.on_agent(AgentId(1), AgentEvent::Error("端点的回应无法解析".into()));
        for &(width, height) in SWEEP_SIZES {
            if is_below_floor(width, height) {
                continue;
            }
            let shot = shot(&mut app, width, height);
            shot.assert_shape("wide glyphs", width, height);
            assert_no_command(&format!("wide glyphs at {width}×{height}"), &shot);
        }
        let text = shot(&mut app, 200, 50).shown().join("\n");
        assert!(
            text.contains("移植解析"),
            "the row names the agent in its own script: {text}"
        );
        assert!(
            text.contains("✉"),
            "the unread mark rides the row it is about (H4); it is state, so the title yields \
             to it by R1 — which is the one column this test gave up: {text}"
        );
        assert!(
            text.contains("请把解析器移植过来"),
            "and the transcript keeps the message: {text}"
        );
    }

    /// The hidden-row counts and the arrows that name the side, read off the
    /// painted title at the size where the pane is shortest.
    ///
    /// Nineteen rows hidden under a one-row window is the defect P12 named, and
    /// a bare `+19` cannot say which way they went — at the bottom of the pane
    /// every hidden row is *above* the window, which is the other half of the
    /// same defect.
    #[test]
    fn the_sweep_counts_the_rows_the_pane_cannot_show() {
        let (mut app, _rx) = a_twenty_agent_tree("sweep-hidden");
        // The compact strip gives the tree one inner row at 40×10: the root is
        // the window, and the other nineteen rows are below it.
        let text = shot(&mut app, 40, 10).text();
        assert!(text.contains("▼19"), "the rows below are named: {text}");
        assert!(!text.contains('▲'), "nothing is above the top row: {text}");

        app.tree.cursor_bottom();
        let text = shot(&mut app, 40, 10).text();
        assert!(
            text.contains("▲19"),
            "at the bottom they are all above: {text}"
        );
        assert!(!text.contains('▼'), "nothing is below: {text}");

        // At the presentation size nothing is hidden, so nothing says it is.
        app.tree.cursor_top();
        let text = shot(&mut app, 200, 50).text();
        assert!(!text.contains('▲') && !text.contains('▼'), "{text}");
    }

    /// The title's `▲`/`▼` counts name exactly the rows the pane's window
    /// leaves off screen, and the rows the window paints are the rows between
    /// them. The counts are arithmetic over `AgentsPane::list_area`, and this
    /// reads them back against the painted cells instead of trusting the
    /// derivation — the one way the count, the scroll offset and the window can
    /// be caught drifting apart (findings V1/V7).
    fn assert_window_counts(at: &str, shot: &Shot) {
        let Screen::Panes(panes) = &shot.screen else {
            return;
        };
        let pane = &panes.agents;
        let window = pane.list_area.height as usize;
        let rows = pane.rows.len();
        let mut above = 0usize;
        let mut below = 0usize;
        for cell in pane.title.split(" · ") {
            if let Some(count) = cell
                .strip_prefix('▲')
                .and_then(|digits| digits.parse().ok())
            {
                above = count;
            }
            if let Some(count) = cell
                .strip_prefix('▼')
                .and_then(|digits| digits.parse().ok())
            {
                below = count;
            }
        }
        assert_eq!(
            above + below,
            rows.saturating_sub(window),
            "{at}: ▲/▼ must name exactly what the window hides"
        );
        // The ids painted inside the window, in order: the counts must be of
        // the rows really between them, not of an offset the painter ignored.
        let painted: Vec<u64> = (pane.list_area.y..pane.list_area.bottom())
            .filter_map(|y| {
                let line: String = (pane.list_area.x..pane.list_area.right())
                    .map(|x| shot.cells[y as usize][x as usize].clone())
                    .collect();
                line.split_once('#')?
                    .1
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .ok()
            })
            .collect();
        let expected: Vec<u64> = pane
            .rows
            .iter()
            .skip(above)
            .take(window)
            .map(|row| row.id.0)
            .collect();
        assert_eq!(
            painted, expected,
            "{at}: the window must paint the {window} rows the counts leave in it"
        );
    }

    /// V1: the window the counts name is the window the painter paints. The
    /// counts are arithmetic over `AgentsPane::list_area`, and the painter now
    /// reads that same value instead of deriving the geometry again — so this
    /// reads both back at the sizes the tree outgrows its pane.
    #[test]
    fn the_window_the_counts_name_is_the_window_the_painter_paints() {
        let (mut app, _rx) = a_twenty_agent_tree("sweep-window");
        for &(width, height) in &[(40u16, 10u16), (80u16, 24u16), (120u16, 32u16)] {
            let shot = shot(&mut app, width, height);
            assert_window_counts(&format!("{width}×{height}"), &shot);
        }
    }

    /// The pane's title wears the counts that fit it and drops the rest whole:
    /// a clause cut mid-number (`Σ +324 −`, `2 waitin`) is a count that is not
    /// the count (finding U2).
    #[test]
    fn the_sweep_paints_the_title_clauses_that_fit() {
        let (mut app, _rx) = test_app("sweep-title");
        spawn_agent(
            &mut app,
            1,
            0,
            1,
            "build the lexer for the config format",
            None,
        );
        // At 40×10 the compact strip gives the tree the whole width, and both
        // counts fit.
        let text = shot(&mut app, 40, 10).text();
        assert!(text.contains("1 working"), "{text}");
        assert!(text.contains("1 waiting"), "{text}");
        // At 80×24 the tree gets thirty columns, and the widest clause yields.
        let text = shot(&mut app, 80, 24).text();
        assert!(text.contains(" agents · 1 working"), "{text}");
        assert!(
            !text.contains("1 waiting"),
            "a clause that does not fit is dropped whole, never cut: {text}"
        );
    }

    /// The dots move once a second, and the poll is not the animation: a frame's
    /// worth of ticks leaves both the beat and the painted line alone, where the
    /// braille spinner this replaced turned over on every one of them.
    #[test]
    fn the_working_dots_move_once_a_second() {
        let (mut app, _rx) = test_app("dot-beat");
        begin_run(&mut app, AgentId::ROOT);
        app.spin = 0;
        app.spin_at = Instant::now();
        // A fresh git read would schedule a repaint of its own, and this test is
        // about what the beat does.
        app.git_at = Some(Instant::now());

        app.dirty_screen = false;
        for _ in 0..30 {
            app.tick();
        }
        assert_eq!(app.spin, 0, "thirty passes of a 30 ms poll are not a beat");
        assert!(!app.dirty_screen, "nothing moved, so nothing is repainted");

        app.spin_at = Instant::now() - DOT_PERIOD;
        app.tick();
        assert_eq!(app.spin, 1, "a second on, the dot lands");
        assert!(app.dirty_screen, "and the frame is owed one");
    }

    /// A derived line is not a hidden line: a busy agent with nothing written
    /// about it must not claim `+1 more lines` for its own activity line — a
    /// count `/notes` cannot answer.
    #[test]
    fn the_sweep_never_counts_a_derived_line_as_hidden() {
        let (mut app, _rx) = test_app("sweep-spinner-count");
        app.chat
            .push_message(AgentId::ROOT, Message::user("what is happening"));
        begin_run(&mut app, AgentId::ROOT);
        // At 40×10 the pane has one row of transcript, it protects it for the
        // message, and the foot therefore has no row at all: the activity line
        // is not painted, and a pane that counted it would say so in its title.
        let text = shot(&mut app, 40, 10).text();
        assert!(!text.contains("thinking."), "no row for it here: {text}");
        assert!(
            !text.contains("more lines"),
            "a derived line is not a hidden line: {text}"
        );
        // Where the foot has a row, the activity line is painted and still
        // nothing is counted as hidden.
        for &(width, height) in SWEEP_SIZES {
            if is_below_floor(width, height) || (width, height) == (40, 10) {
                continue;
            }
            let text = shot(&mut app, width, height).text();
            assert!(
                text.contains("thinking."),
                "the activity line is the pane's own words at {width}×{height}: {text}"
            );
            assert!(
                !text.contains("more lines"),
                "the activity line is not a hidden line at {width}×{height}: {text}"
            );
        }
    }

    /// The stored failure survives the restart into the *painted* frame: a row
    /// where the pane has one, and the title — which is the pane's way of
    /// saying what it is hiding — where it does not.
    #[test]
    fn the_sweep_paints_a_restored_failure_at_every_size() {
        let mut app = restored_session("sweep-restored-notice");
        let text = shot(&mut app, 80, 24).text();
        assert!(
            text.contains("! the endpoint returned 503"),
            "the stored failure is a row of the foot: {text}"
        );
        let text = shot(&mut app, 40, 10).text();
        assert!(
            text.contains("more lines") && text.contains("/notes"),
            "a pane with no row for the line says how many it is hiding: {text}"
        );
    }

    /// A wrapped message keeps its tail: the pane is anchored at the bottom, so
    /// the last row of the newest message is the row that must be there at
    /// every size, and the head is there when the pane has the rows for it.
    #[test]
    fn the_sweep_keeps_the_tail_of_a_wrapped_message() {
        let (mut app, _rx) = test_app("sweep-wrapped");
        let long = format!("HEADWORD {} TAILWORD", "wide ".repeat(40));
        app.chat
            .push_message(AgentId::ROOT, Message::assistant(&long));
        for &(width, height) in SWEEP_SIZES {
            if is_below_floor(width, height) {
                continue;
            }
            let shot = shot(&mut app, width, height);
            assert!(
                shot.text().contains("TAILWORD"),
                "the tail of the newest message at {width}×{height}:\n{}",
                shot.text()
            );
            shot.assert_shape("a wrapped message", width, height);
        }
        let text = shot(&mut app, 200, 50).text();
        assert!(text.contains("HEADWORD"), "the head, where it fits: {text}");
    }

    /// Scrolling holds the window: the pane says so, and what it shows is the
    /// rows above the bottom rather than the ones following it.
    #[test]
    fn the_sweep_shows_the_rows_a_scrolled_pane_is_holding() {
        let (mut app, _rx) = test_app("sweep-held");
        for i in 1..=40 {
            app.chat
                .push_message(AgentId::ROOT, Message::user(format!("message {i}")));
        }
        let bottom = shot(&mut app, 120, 32).text();
        app.chat.scroll_by(AgentId::ROOT, 20);
        let held = shot(&mut app, 120, 32).text();
        assert!(held.contains("scrolled ↑20 rows"), "{held}");
        let older = (1..=40)
            .map(|i| format!("message {i}"))
            .find(|word| held.contains(word.as_str()) && !bottom.contains(word.as_str()))
            .unwrap_or_else(|| {
                panic!("the held window shows rows the bottom did not:\n{held}\n---\n{bottom}")
            });
        assert!(
            !bottom.contains(&older),
            "{older} is only in the held frame"
        );
    }

    /// A popup's own text is read at every size too: the wrapped note, the
    /// bullet on the current model and the hint are all painted, and the popup
    /// leaves the panes it covers alone.
    #[test]
    fn the_sweep_paints_what_a_popup_says() {
        let (mut app, _rx) = test_app("sweep-popup");
        app.models = vec![
            http::Model {
                id: "test-model".to_string(),
                context: Some(500_000),
            },
            http::Model {
                id: "deepseek-chat".to_string(),
                context: None,
            },
        ];
        app.open_model_picker();
        for &(width, height) in SWEEP_SIZES {
            if is_below_floor(width, height) {
                continue;
            }
            let shot = shot(&mut app, width, height);
            let text = shot.text();
            assert!(
                text.contains(" models · Enter picks "),
                "the popup's title at {width}×{height}: {text}"
            );
            assert!(
                text.contains("• test-model"),
                "the current model is marked at {width}×{height}: {text}"
            );
            assert!(
                text.contains("j/k or PgUp/PgDn"),
                "and the keys are named at {width}×{height}: {text}"
            );
            shot.assert_shape("an open /model picker", width, height);
        }

        // A note can carry a failure's own words, and a model id comes from the
        // endpoint: both are painted whole in the popup, and both must be
        // defanged where they are painted.
        let (mut notes, _rx) = test_app("sweep-popup-hostile");
        notes.chat.note_error_for(AgentId::ROOT, HOSTILE);
        notes.open_notes_picker();
        for &(width, height) in SWEEP_SIZES {
            if is_below_floor(width, height) {
                continue;
            }
            notes.set_term_size(width, height);
            notes.open_notes_picker();
            let shot = shot(&mut notes, width, height);
            let at = format!("a hostile /notes popup at {width}×{height}");
            assert_no_command(&at, &shot);
            shot.assert_shape(&at, width, height);
            assert!(
                shot.shown().join("\n").contains("escaped"),
                "{at}: the words survive the commands around them"
            );
        }
    }

    /// Deliver one hostile payload the way its actor does: a model reply (the
    /// run then ends, so the reply becomes the row's and the footer's summary),
    /// a run's failure (the endpoint's own words), and a tool result.
    fn feed_hostile(app: &mut App, case: &str, text: &str) {
        let conversation = app.tree.conversation();
        let event = match case {
            "reply" => {
                app.update(Msg::Agent {
                    conversation,
                    id: AgentId::ROOT,
                    event: AgentEvent::Message(Message::assistant(text)),
                });
                AgentEvent::Done
            }
            "error" => AgentEvent::Error(text.to_string()),
            "result" => AgentEvent::Message(Message::tool("call_1", text)),
            other => unreachable!("no case {other}"),
        };
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event,
        });
    }

    /// A pane is a terminal, and a terminal acts on what it is given — so the
    /// one-line surfaces are fed the same bytes the verifier used, and what is
    /// asserted is the *painted frame*, not the source string.
    ///
    /// The wrapped transcript was already defanged when this leaked: a reply of
    /// `…\rREPLACED…` returned the cursor over the agents pane's own border from
    /// the *footer*, an error body's `ESC ]0;PWNED BEL` renamed the window from
    /// the *bar*, and a CSI in either wiped the frame. A test that reads the
    /// transcript is exactly what let the other four surfaces through, so this
    /// one reads the cells, at the audit's 80×24 and at the compact 40×10.
    #[test]
    fn a_one_line_surface_cannot_be_commanded_by_the_text_it_paints() {
        const CR: &str = "\r";
        const OSC: &str = "\x1b]0;PWNED\x07";
        const CSI: &str = "\x1b[2J\x1b[H";
        const BIDI: &str = "\u{2066}";

        // Each case, with the commands in it and with plain words in the same
        // places: the second is what the frame is measured against, so a pane
        // that a leak moved is a pane whose border is somewhere else.
        let payload = |case: &str, hostile: bool| -> String {
            match (case, hostile) {
                ("reply", true) => {
                    format!("final reply{CR}STEP1 {OSC}{CSI}{BIDI}middle")
                }
                ("reply", false) => "final reply STEP1 middle".to_string(),
                ("error", true) => {
                    format!("model returned HTTP 500: boom{CR}REST {OSC} {CSI}{BIDI}after")
                }
                ("error", false) => "model returned HTTP 500: boom REST  after".to_string(),
                ("result", true) => format!("boom{CR}REST {OSC} {CSI}{BIDI}after"),
                ("result", false) => "boom REST  after".to_string(),
                other => unreachable!("no case {other:?}"),
            }
        };
        // What the surface still says once the commands are gone: the CR is
        // marked, never taken away, and the words around it are the words.
        let says = |case: &str| match case {
            "reply" => "final reply␍STEP1",
            _ => "boom␍REST",
        };

        for (width, height) in [(80u16, 24u16), (40, 10)] {
            for case in ["reply", "error", "result"] {
                let (mut app, _rx) = test_app("hostile-one-line");
                feed_hostile(&mut app, case, &payload(case, true));
                let frame = frame_grid(&mut app, width, height);

                let (mut clean, _rx) = test_app("hostile-one-line-plain");
                feed_hostile(&mut clean, case, &payload(case, false));
                let pristine = frame_grid(&mut clean, width, height);

                let painted = frame.join("\n");
                for bad in ['\x1b', '\r'] {
                    assert!(
                        !painted.contains(bad),
                        "{case} at {width}×{height} painted {bad:?}: {painted:?}"
                    );
                }
                // Cell by cell, and never the newline this test joined them with:
                // every other control byte, and the bidi isolates, are a command
                // to a display rather than something to read.
                for row in &frame {
                    for ch in row.chars() {
                        assert!(
                            !ch.is_control()
                                && !matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'),
                            "{case} at {width}×{height} painted {ch:?}, which commands the display: {row:?}"
                        );
                    }
                }
                assert!(
                    painted.contains(says(case)),
                    "{case} at {width}×{height} lost its words: {painted:?}"
                );

                // The box skeleton belongs to the frame, not to the text: a
                // border that moved or vanished is a pane that was overwritten.
                for (y, (row, base)) in frame.iter().zip(pristine.iter()).enumerate() {
                    for (x, (cell, want)) in row.chars().zip(base.chars()).enumerate() {
                        if "─│┌┐└┘├┤┬┴┼".contains(want) {
                            assert_eq!(
                                cell, want,
                                "{case} at {width}×{height} overwrote the border at {x},{y}: {row:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A long unbroken token — the model reply that is one 20 000-character
    /// word — must be cut at a one-line surface's edge, with the `…` the rest of
    /// the UI uses, and never have its middle painted into a pane.
    #[test]
    fn a_one_line_surface_cuts_a_long_token_at_its_edge() {
        let token = "S".repeat(20_000);
        let reply = format!("final model reply\rSTEP1 middle {token}");

        for (width, height) in [(80u16, 24u16), (40, 10)] {
            let (mut app, _rx) = test_app("long-token");
            feed_hostile(&mut app, "reply", &reply);
            let frame = frame_grid(&mut app, width, height);
            let painted = frame.join("\n");

            assert!(!painted.contains('\r'), "{painted:?}");
            // The agents pane paints none of the token: at 80×24 it is the
            // thirty columns on the left, at 40×10 the three rows on top. (The
            // transcript is *meant* to wrap the token — that is the one surface
            // that may show its text.)
            let pane: Vec<&String> = if width >= 80 {
                frame.iter().collect()
            } else {
                frame.iter().take(3).collect()
            };
            for row in pane {
                let cells: String = row
                    .chars()
                    .take(if width >= 80 { 30 } else { 40 })
                    .collect();
                assert!(
                    !cells.contains("SSSS"),
                    "the agents pane painted the token at {width}×{height}: {row:?}"
                );
            }
            if width >= 80 {
                assert!(
                    painted.contains('…'),
                    "the footer must say it cut the token: {painted:?}"
                );
            }
        }
    }

    /// One frame with a nested tree: the root is at rest with work out, its own
    /// child #1 is parked in `wait`, and the grandchild #2 is working.
    ///
    /// The bar's sentence is a promise about when the root resumes — "the root
    /// resumes as they finish" — and the root resumes when *its* children
    /// finish, which is the derivation the row's `⏸N` and the title's `M
    /// waiting` read. The bar counted every busy node in the tree instead, so
    /// with a grandchild at work it promised a resume that the grandchild's
    /// finish does not cause, in the same frame where the pane title named one
    /// (finding U2's second owner).
    #[test]
    fn the_bar_and_the_title_agree_on_who_the_root_waits_for() {
        let (mut app, _rx) = test_app("nested-wait");
        let conversation = app.tree.conversation();
        for (child, parent, depth) in [(1u64, 0u64, 1usize), (2, 1, 2)] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId(parent),
                event: AgentEvent::Spawned {
                    child,
                    parent,
                    brief: "a task".to_string(),
                    depth,
                    branch: None,
                    fork: None,
                    title: None,
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
            app.update(Msg::Agent {
                conversation,
                id: AgentId(child),
                event: AgentEvent::Running {
                    cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                },
            });
        }
        // The root ended its turn with child #1 still out; #1 is parked on its
        // own child, and #2 is the one actually working.
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Done,
        });
        app.tree.activity(AgentId(1), "wait 3s");
        app.tree.activity(AgentId(2), "read_file deep.txt 2s");
        // Two facts, deliberately different: `busy_counts` is "a run is in
        // flight" — child #1's is, it is parked in a `wait` and will finish and
        // report — while the title counts who is *computing*: the root is
        // napping, #1 is parked on its own child, and only #2 is working
        // (findings U2, U14).
        let roster = app.tree.roster();
        assert_eq!(
            (
                app.tree
                    .busy_counts()
                    .get(&AgentId::ROOT)
                    .copied()
                    .unwrap_or(0),
                roster.working,
                roster.waiting,
            ),
            (1, 1, 2),
            "the tree this is about: the root waits on one of two agents with a run in flight"
        );

        let rows = screen(&mut app, 200, 50);
        let title = rows.first().expect("the pane title is painted");
        // The bar's message row, above the facts row it shares the foot with.
        let bar = &rows[rows.len() - 2];

        assert!(title.contains("1 working"), "{title:?}");
        assert!(title.contains("2 waiting"), "{title:?}");
        assert!(
            bar.contains("waiting on 1 subagent(s)"),
            "the bar counted a grandchild the root does not resume on: {bar:?}"
        );
        assert!(
            !bar.contains("waiting on 2"),
            "the bar disagrees with the title in the same frame: {bar:?}"
        );

        // The same fact with a *stopped* root (finding U12): a stopped or failed
        // agent's mailbox is just as alive — the child's completion folds in and
        // starts a run — so the title must still count it waiting. The title
        // said `0 waiting` while the bar promised `the root resumes`, which is
        // one fact derived two ways.
        app.tree.stopped(AgentId::ROOT);
        assert!(
            app.tree.roster().waiting >= 1,
            "a stopped root over a working child is still waiting on it"
        );
        let rows = screen(&mut app, 200, 50);
        let title = rows.first().expect("the pane title is painted");
        let bar = &rows[rows.len() - 2];
        assert!(title.contains("2 waiting"), "{title:?}");
        assert!(
            bar.contains("waiting on 1 subagent(s)"),
            "the bar and the title agree about a stopped root: {bar:?}"
        );
    }

    /// A status that arrives after the run ended must not put a finished agent
    /// back to work (finding B5).
    #[test]
    fn a_late_status_does_not_restart_a_finished_agent() {
        let (mut app, _rx) = test_app("late-status");
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Done,
        });
        assert_eq!(app.tree.agents[0].phase, Phase::Done);
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Status("committed abc123 on mush/1".to_string()),
        });
        assert_eq!(app.tree.agents[0].phase, Phase::Done, "the ✓ must survive");
        assert!(!app.busy());
    }

    /// A `⊘` whose acknowledgement can never arrive goes quiet instead of
    /// spinning forever (finding B6).
    #[test]
    fn a_stale_cancel_falls_back_to_idle() {
        let (mut app, _rx) = test_app("stale-cancel");
        app.tree.cancel_requested(AgentId::ROOT);
        app.tree.age(AgentId::ROOT, Duration::from_secs(11));
        app.tick();
        assert_eq!(app.tree.agents[0].phase, Phase::Idle);
        assert!(!app.busy(), "the bar must stop claiming work");
    }

    /// The one ending no actor can report, because the actor that would have
    /// reported it is the thing that vanished: a run in flight whose mailbox is
    /// dead. The human's Ctrl-C finds nobody to ask, and what the row says is
    /// `⚠` — the run died where it stood and committed nothing — not `⊘`,
    /// whose whole meaning is an agent a message resumes (finding H2).
    ///
    /// The parent is told through the message a completion travels in, because
    /// the UI is the only observer left that can send one.
    #[test]
    fn a_run_whose_actor_vanished_is_reported_to_its_parent_as_cut_off() {
        let (mut app, _rx) = test_app("live-cut-off");
        let conversation = app.tree.conversation();
        // A live parent (#1) whose mailbox this test holds, and a child (#2)
        // whose receiver is dropped: the actor is gone, the row does not know it
        // yet.
        let (parent_tx, parent_rx) = crossbeam_channel::unbounded::<AgentMsg>();
        for (child, parent, depth, cmd) in [
            (1, 0, 1, parent_tx),
            (2, 1, 2, crossbeam_channel::unbounded().0),
        ] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId::ROOT,
                event: AgentEvent::Spawned {
                    child,
                    parent,
                    brief: format!("task {child}"),
                    depth,
                    branch: None,
                    fork: None,
                    title: None,
                    cmd,
                },
            });
        }
        app.tree.begin(AgentId(2), None);
        app.tree.focus(AgentId(2));

        app.stop_one(AgentId(2));

        assert_eq!(
            app.tree.node(AgentId(2)).map(|node| node.phase.clone()),
            Some(Phase::CutOff),
            "a stop nobody can hear is not a stop"
        );
        match parent_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(AgentMsg::ChildDone { id, run, outcome }) => {
                assert_eq!(id, 2, "the child that was cut off");
                assert_eq!(
                    outcome,
                    agent::Outcome::CutOff,
                    "not `done`, not `stopped`, not `failed`"
                );
                // A run that never ended has no number of its own, so the UI
                // files it under one no real report can carry: the parent folds
                // it once, keeps it, and no later run can read as the same one.
                assert_eq!(run, agent::CUT_OFF_RUN, "the run that never got a number");
            }
            Ok(_) => panic!("the parent heard something other than a completion"),
            Err(_) => panic!("the parent was never told"),
        }
        let notice = app
            .chat
            .notices_for(AgentId(2))
            .map(|notice| notice.text.clone())
            .collect::<Vec<_>>()
            .join(" · ");
        assert!(notice.contains("nothing was committed"), "{notice}");
        let rows = screen(&mut app, 120, 24);
        assert!(
            rows.iter().any(|row| row.contains("⚠ #2")),
            "the row says which ending this was: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("cut off")),
            "and the pane of the agent it happened to says why: {rows:?}"
        );
        // The line the human's own key earns is a status, and a status yields
        // to the derived facts above it (R2): what *this* agent is doing is
        // still on the row, while the root's nap is on the bar.
        assert!(
            app.status_line()
                .is_some_and(|(text, _)| text.contains("cut off")),
            "the key that did nothing says so: {:?}",
            app.status_line()
        );
    }

    /// The same report when the parent is *parked*: its mailbox is the dead one
    /// `park_history` leaves, so a raw send drops the one report a run that
    /// never ended has — and the message's whole reason for existing is that it
    /// wakes a napping parent. Through `deliver_to_actor` the wake happens and
    /// the tree ends up holding the newer mailbox it built.
    #[test]
    fn a_cut_off_child_wakes_a_parked_parent_through_the_ui() {
        let (mut app, rx) = test_app("cut-off-parked-parent");
        let conversation = app.tree.conversation();
        // A parent (#1) whose receiver is dropped — a parked actor, exactly
        // what reclaiming a finished child's thread leaves — and a child (#2)
        // whose actor is gone too, mid-run.
        let (parent_tx, parked) = crossbeam_channel::unbounded::<AgentMsg>();
        drop(parked);
        for (child, parent, depth, cmd) in [
            (1, 0, 1, parent_tx),
            (2, 1, 2, crossbeam_channel::unbounded().0),
        ] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId::ROOT,
                event: AgentEvent::Spawned {
                    child,
                    parent,
                    brief: format!("task {child}"),
                    depth,
                    branch: None,
                    fork: None,
                    title: None,
                    cmd,
                },
            });
        }
        app.tree.begin(AgentId(2), None);

        app.stop_one(AgentId(2));

        // The mailbox the tree holds for #1 is a newer one, with an actor
        // behind it: the wake the report itself asked for.
        assert!(
            app.tree.agent_tx[&AgentId(1)]
                .send(AgentMsg::Stop(agent::Stop::Human))
                .is_ok(),
            "the parked parent has an actor again"
        );
        // And it is this report that woke it: a parent with empty books folds
        // a cut-off as news, so the completion begins the run that tells its
        // model — which is the only evidence that the report reached it.
        assert!(
            runs(&mut app, &rx, AgentId(1)),
            "the completion reached the parent the report woke"
        );
    }

    /// A cut-off owner's job is killed rather than orphaned. The actor that
    /// started it is the one that vanished, so `Registry::stop` (owner-only) and
    /// the owner's own `Stop`/`Shutdown` handlers can never reach it, and the UI
    /// — the only observer left — is where the kill has to be made. Without it
    /// the build runs until quit, owned by a row that says the work was cut off.
    #[test]
    fn a_cut_off_owners_job_is_killed() {
        let (mut app, _rx) = test_app("cut-off-job");
        let machine = running_job_on(&mut app, 1);
        // Agent #1 mid-run with a dead mailbox: the exact shape a cut-off owner
        // has. `spawn_agent` drops the receiver, so the Stop cannot land.
        spawn_agent(&mut app, 1, 0, 1, "a task", None);
        begin_run(&mut app, AgentId(1));

        app.stop_one(AgentId(1));

        assert_eq!(
            app.tree.node(AgentId(1)).map(|node| node.phase.clone()),
            Some(Phase::CutOff),
            "the row says the run died where it stood"
        );
        assert_eq!(
            machine.kills(),
            1,
            "and the job the vanished actor left behind is killed"
        );
    }

    /// The other half of what a vanished actor leaves: the machine it claimed.
    /// A panic skips the call's release ([`crate::jobs::Foreground::drop`]), and
    /// the road that kills the orphaned job is the only one that can free the
    /// claim — a sibling queued on a lock held by a row that is gone never gets
    /// in (finding E4).
    #[test]
    fn a_cut_off_owner_frees_the_machine_it_held() {
        use crate::jobs::Launch;
        use crate::machine::fake::{Script, Scripted};
        use crate::machine::{Machine, ShellCommand};

        let (mut app, _rx) = test_app("cut-off-holder");
        let jobs = app.tree.handles().jobs.clone();
        let machine = Arc::new(Scripted::new().runs(Script::hangs()));
        let job = machine
            .spawn(&ShellCommand {
                command: "cargo bench",
                root: std::path::Path::new("/tmp"),
            })
            .unwrap();
        let (mailbox, _rx) = crossbeam_channel::unbounded();
        jobs.launch(Launch::started(
            1,
            "cargo bench".to_string(),
            true,
            mailbox,
            job,
        ))
        .unwrap();
        spawn_agent(&mut app, 1, 0, 1, "a task", None);
        begin_run(&mut app, AgentId(1));
        assert!(jobs.held().is_some(), "the exclusive job holds the machine");

        app.stop_one(AgentId(1));

        assert_eq!(jobs.held(), None, "the cut-off road frees the machine");
        assert!(
            jobs.take_machine(9, "cargo bench").is_ok(),
            "and a sibling can take it"
        );
    }

    /// A child's result is unread until its *parent's actor* has read it, and
    /// the screen says so from both ends of the relationship: the child's row
    /// wears `✉`, and the parent's row counts what it owes a read (finding H4).
    ///
    /// It is the one fact a child's own phase cannot give: a `✓` says a run
    /// finished, not that anybody was told.
    #[test]
    fn a_result_its_parent_has_not_read_is_marked_on_the_row() {
        let (mut app, _rx) = test_app("unread-result");
        let conversation = app.tree.conversation();
        for (child, parent, depth) in [(1, 0, 1), (2, 1, 2)] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId::ROOT,
                event: AgentEvent::Spawned {
                    child,
                    parent,
                    brief: format!("task {child}"),
                    depth,
                    branch: None,
                    fork: None,
                    title: None,
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
        }

        // #2 finishes. Nobody has read that yet — #1 was not even running.
        app.update(Msg::Agent {
            conversation,
            id: AgentId(2),
            event: AgentEvent::Done,
        });
        let rows = screen(&mut app, 120, 24);
        assert!(
            rows.iter().any(|row| row.contains("✓ #2 ✉")),
            "the child's row says its result is unread: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("#1 ✉1")),
            "and its parent's row says how many it owes: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("#0 ✉")),
            "the root has no parent, so nothing of its is unread: {rows:?}"
        );
        // The selected row spells the mark out, so `✉` is not a glyph a human
        // has to guess at: which result, and who owes a read on it.
        app.tree.cursor_bottom();
        let rows = screen(&mut app, 120, 24);
        assert!(
            rows.iter().any(|row| row.contains("✉ result unread by #1")),
            "the child's own footer says who has not read it: {rows:?}"
        );
        app.tree.move_cursor(-1);
        let rows = screen(&mut app, 120, 24);
        assert!(
            rows.iter().any(|row| row.contains("✉1 unread from #2")),
            "and its parent's footer names what it owes: {rows:?}"
        );
        app.tree.cursor_top();

        // #1's actor folds the line in: the result is read, and both marks go.
        app.update(Msg::Agent {
            conversation,
            id: AgentId(1),
            event: AgentEvent::ResultRead { child: 2 },
        });
        let rows = screen(&mut app, 120, 24);
        assert!(
            !rows.iter().any(|row| row.contains('✉')),
            "a result that has been read wears no mark: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("✓ #2")),
            "while the result itself is still there: {rows:?}"
        );

        // A new run supersedes the old result the way the actor's own
        // `delivered` set does, so a mark cannot outlive what it is about.
        app.update(Msg::Agent {
            conversation,
            id: AgentId(2),
            event: AgentEvent::Running {
                cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        });
        app.update(Msg::Agent {
            conversation,
            id: AgentId(2),
            event: AgentEvent::Done,
        });
        assert!(
            screen(&mut app, 120, 24)
                .iter()
                .any(|row| row.contains("✓ #2 ✉")),
            "a second result is unread again"
        );
    }

    /// The child's brief is the first thing its transcript shows, exactly as
    /// the model received it (finding B13).
    #[test]
    fn a_childs_brief_is_the_start_of_its_transcript() {
        let (mut app, _rx) = test_app("brief");
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Spawned {
                child: 1,
                parent: 0,
                brief: "count the lexer tokens".to_string(),
                depth: 1,
                branch: None,
                fork: None,
                title: None,
                cmd: crossbeam_channel::unbounded().0,
            },
        });
        let messages = app.chat.transcript(AgentId(1));
        assert_eq!(messages.len(), 1, "the brief opens the transcript");
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].text(), "count the lexer tokens");
    }

    /// Every Done replaces the row's summary, and a new run clears it: the row
    /// describes the current run, not the first one forever (finding B14).
    #[test]
    fn a_rows_summary_follows_the_latest_run() {
        let (mut app, _rx) = test_app("summary");
        let conversation = app.tree.conversation();
        app.chat
            .push_message(AgentId::ROOT, Message::assistant("first result"));
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Done,
        });
        assert_eq!(app.tree.agents[0].summary.as_deref(), Some("first result"));
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        });
        assert_eq!(
            app.tree.agents[0].summary, None,
            "a new run clears the old one"
        );
        app.chat
            .push_message(AgentId::ROOT, Message::assistant("second result"));
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Done,
        });
        assert_eq!(app.tree.agents[0].summary.as_deref(), Some("second result"));
    }

    /// Leftover worktrees keep their ids, and the tree's counter is raised
    /// above them, so the next spawned child cannot collide (finding B1); a
    /// reaped leftover releases the focus (finding B11).
    ///
    /// The leftover's work is unmerged on purpose: a `mush/<id>` whose branch is
    /// already in HEAD is reclaimed by the same pass, so it is a row that never
    /// gets registered and a number that is free again (finding H10) —
    /// `a_merged_residue_is_reclaimed_by_the_startup_pass` is that half.
    #[test]
    fn leftover_worktrees_raise_the_id_floor_and_release_the_focus() {
        use std::fs;
        use std::process::Command;

        let root = std::env::temp_dir().join(format!("mush-app-wt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        git(&["worktree", "add", "-q", "-b", "mush/7", ".mush/wt/7"]);
        // The leftover carries work nobody merged: one in HEAD would be residue
        // the startup pass reclaims, not a leftover (finding H10).
        fs::write(root.join(".mush/wt/7/work.txt"), "the leftover's work\n").unwrap();
        git::run(&root.join(".mush/wt/7"), &["add", "-A"]).unwrap();
        git::run(
            &root.join(".mush/wt/7"),
            &["commit", "-qm", "leftover work"],
        )
        .unwrap();

        let (mut app, _rx) = app_root(&root, None, session_save::fake::Recorder::new());

        assert!(
            app.tree.agents.iter().any(|node| node.id == AgentId(7)),
            "the leftover is registered"
        );
        assert!(
            app.tree.handles().ids.agents_floor() >= 8,
            "the next spawn must not reuse #7"
        );

        app.tree.focus(AgentId(7));
        git(&["worktree", "remove", "--force", ".mush/wt/7"]);
        app.discover_worktrees();
        assert!(
            !app.tree.agents.iter().any(|node| node.id == AgentId(7)),
            "the reaped leftover is gone"
        );
        assert_eq!(
            app.tree.focused,
            AgentId::ROOT,
            "focus cannot point at a ghost"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A `mush/<id>` branch whose checkout is gone is not work on disk — no row,
    /// as `the_roster_does_not_report_a_dead_worktree` says — but while its work
    /// is unmerged it is still fatal to the next `git worktree add -b mush/<id>`,
    /// because the branch ref outlives the directory git registered it against.
    /// So the residue raises the id floor: the number is spent, the row would be
    /// a lie. (The *merged* residue — H10's specimen — is reclaimed by the same
    /// pass, and `a_merged_residue_is_reclaimed_by_the_startup_pass` is that
    /// half.)
    #[test]
    fn a_dir_less_branch_raises_the_id_floor_without_a_row() {
        use std::fs;
        use std::process::Command;

        let root = std::env::temp_dir().join(format!("mush-app-residue-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        git(&["worktree", "add", "-q", "-b", "mush/7", ".mush/wt/7"]);
        // Unmerged work, so the residue the startup pass keeps is the *kept*
        // half of the rule: a `mush/<id>` in HEAD is reclaimed and its number
        // handed back (finding H10).
        fs::write(root.join(".mush/wt/7/work.txt"), "unmerged residue\n").unwrap();
        git::run(&root.join(".mush/wt/7"), &["add", "-A"]).unwrap();
        git::run(&root.join(".mush/wt/7"), &["commit", "-qm", "residue"]).unwrap();
        // The directory goes by hand, as `rm -rf .mush` leaves it: git's entry
        // and the branch stay, and the next `worktree add -b mush/7` refuses.
        fs::remove_dir_all(root.join(".mush/wt/7")).unwrap();
        let named = git::worktrees(&root).expect("git answers");
        let residue = named
            .iter()
            .find(|worktree| worktree.id == Some(7))
            .expect("discovery still sees the entry and its branch");
        assert!(!residue.on_disk(), "but the checkout is what is gone");
        assert!(
            git::run(&root, &["rev-parse", "--verify", "refs/heads/mush/7"]).is_ok(),
            "and the branch is still there, which is what the next add collides with"
        );

        let (app, _rx) = app_root(&root, None, session_save::fake::Recorder::new());

        assert!(
            !app.tree.agents.iter().any(|node| node.id == AgentId(7)),
            "the residue gets no row"
        );
        assert!(
            app.tree.handles().ids.agents_floor() >= 8,
            "but its id is not handed to the next child"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// An id at the top of the `u64` space has no `id + 1`, and a branch can
    /// name one: `git::worktree_id` parses any `mush/<digits>` branch, and an
    /// unmerged commit on `mush/18446744073709551615` reached the reservation as
    /// `id + 1`. Reproduced in this worktree before the fix — a real repository
    /// under `/tmp` with an unmerged commit on that branch, started through
    /// `App::new`:
    ///
    /// ```text
    /// thread 'app::tests::audit_probe_a_branch_naming_the_largest_agent_id'
    /// panicked at crates/mush/src/app/mod.rs:1209:46:
    /// attempt to add with overflow
    /// ```
    ///
    /// In a release build the add wraps to 0 instead, and the floor is silently
    /// left unset. The name is now refused *by name* — a name mush cannot hold
    /// as a child is not a child — and the floor stays usable for the next
    /// spawn. The file half of the audit's test (`mod.rs:743`) is C9's door and
    /// is not this wave's.
    #[test]
    fn no_agent_id_can_overflow_the_floor() {
        use std::fs;

        let root = repo("max-agent-id");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "mush/18446744073709551615",
                ".mush/wt/max",
            ],
        );
        // Unmerged work: a branch already in HEAD is reclaimed before any
        // reservation, and the overflow lives on the reservation road.
        let worktree = root.join(".mush/wt/max");
        fs::write(worktree.join("work.txt"), "unmerged\n").unwrap();
        git(&worktree, &["add", "-A"]);
        git(&worktree, &["commit", "-qm", "work"]);

        let (mut app, _rx) = app_root(&root, None, session_save::fake::Recorder::new());
        // `App::new` already discovered the repository; this is the same pass,
        // read back through the bar, so the sentence asserted below is the
        // one this road wrote and not something a later refresh replaced.
        app.discover_worktrees();

        assert!(
            !app.tree.agents.iter().any(|node| node.id.0 == u64::MAX),
            "the name is refused, not registered as a child"
        );
        assert!(
            app.tree.handles().ids.agents_floor() < u64::MAX,
            "and the floor is left usable: {}",
            app.tree.handles().ids.agents_floor()
        );
        assert!(
            git::resolve(&root, "mush/18446744073709551615").is_some(),
            "the branch mush cannot hold as a child is left where it is"
        );
        assert!(
            text_of(&app).contains("mush/18446744073709551615"),
            "and the refusal names it: {:?}",
            text_of(&app)
        );
        drop(app);
        let _ = fs::remove_dir_all(&root);
    }

    /// A branch in mush's namespace that names no id at all — `mush/x`, a
    /// hand-made name — is refused by name with a sentence too: no row, no
    /// reservation, and the human who wrote it is told rather than left to
    /// wonder why their branch was passed over (D3, "a name mush cannot read is
    /// not a child").
    #[test]
    fn a_branch_that_names_no_agent_is_refused_by_name() {
        use std::fs;

        let root = repo("unreadable-branch");
        git(
            &root,
            &["worktree", "add", "-q", "-b", "mush/x", ".mush/wt/x"],
        );

        let (mut app, _rx) = app_root(&root, None, session_save::fake::Recorder::new());
        app.discover_worktrees();

        assert!(
            !app.tree
                .agents
                .iter()
                .any(|node| node.branch.as_deref() == Some("mush/x")),
            "a name mush cannot read is not a child"
        );
        assert_eq!(
            app.tree.handles().ids.agents_floor(),
            1,
            "the next child still takes the first number"
        );
        assert!(
            text_of(&app).contains("mush/x") && text_of(&app).contains("left alone"),
            "and the refusal names the branch: {:?}",
            text_of(&app)
        );
        drop(app);
        let _ = fs::remove_dir_all(&root);
    }

    /// H10's specimen, replayed. `mush/7` was merged by a human and its checkout
    /// is gone: git still names the branch, and before this patch the next
    /// isolated spawn died on `a branch named 'mush/7' already exists`. The
    /// startup pass deletes it, and the name it was holding is usable again —
    /// which is the whole point, so the test's last word is that spawn's own
    /// first command.
    #[test]
    fn a_merged_residue_is_reclaimed_by_the_startup_pass() {
        use std::fs;

        let root = repo("reclaim-residue");
        git(
            &root,
            &["worktree", "add", "-q", "-b", "mush/7", ".mush/wt/7"],
        );
        let worktree = root.join(".mush/wt/7");
        fs::write(worktree.join("work.txt"), "merged work\n").unwrap();
        git(&worktree, &["add", "-A"]);
        git(&worktree, &["commit", "-qm", "mush #7: work"]);
        git(&root, &["merge", "--no-edit", "mush/7"]);
        // The checkout goes by hand, as it did for the specimen: git keeps the
        // registry entry and the branch.
        fs::remove_dir_all(&worktree).unwrap();
        assert!(
            git_of(&root, &["rev-parse", "--verify", "refs/heads/mush/7"]).is_ok(),
            "the branch is what git still names"
        );

        let (app, _rx) = app_root(&root, None, session_save::fake::Recorder::new());

        assert!(
            !app.tree.agents.iter().any(|node| node.id == AgentId(7)),
            "merged work is not a leftover row: it is in HEAD"
        );
        assert!(
            git_of(&root, &["rev-parse", "--verify", "refs/heads/mush/7"]).is_err(),
            "and the branch is gone"
        );
        assert!(
            app.tree.handles().ids.agents_floor() < 8,
            "the number went with it, so nothing is reserved for #7 any more"
        );
        // The failure this reclamation exists to end: the name is free again.
        let spawned = git_of(
            &root,
            &["worktree", "add", "-q", "-b", "mush/7", ".mush/wt/7"],
        );
        assert!(
            spawned.is_ok(),
            "the name the next isolated spawn needs must be free: {spawned:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The periodic read is what notices a hand merge without a restart: within
    /// one git read of the merge, the row says the work landed, the checkout is
    /// gone, and — because mush took the branch with it — nothing offers a
    /// `git diff` against either any more (finding H10). Before the merge the
    /// same read says *why* the worktree is still there, which is the other half
    /// of the rule.
    #[test]
    fn a_hand_merge_is_marked_landed_by_the_next_git_read() {
        use std::fs;

        let root = repo("reclaim-refresh");
        let (mut app, rx) = app_and_rx(root.clone());
        wait_git(&mut app, &rx);
        // The revision the worktree below is forked at, read the way the spawn
        // path reads it: the reply's `at <sha>` and the sweep's question 2.
        let fork = git_of(&root, &["rev-parse", "HEAD"]).expect("HEAD");
        // An isolated child of the root, at rest: its worktree exists, its work
        // is on a branch nobody has merged.
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Spawned {
                child: 1,
                parent: 0,
                brief: "build the thing".to_string(),
                depth: 1,
                branch: Some("mush/1".to_string()),
                fork: Some(fork),
                title: None,
                cmd: crossbeam_channel::unbounded().0,
            },
        });
        app.update(Msg::Agent {
            conversation,
            id: AgentId(1),
            event: AgentEvent::Done,
        });
        // `Done` asks the tree for a git read, and that read owns a `git`
        // process against this repository. Let it finish *before* the test
        // drives git itself: two git processes on one repository race the
        // index lock, and a read that snapshots the worktree between the
        // write and the commit puts the wrong `kept` reason on the row — the
        // test used to fail either way, depending on which thread won.
        wait_git(&mut app, &rx);
        git(
            &root,
            &["worktree", "add", "-q", "-b", "mush/1", ".mush/wt/1"],
        );
        let worktree = root.join(".mush/wt/1");
        fs::write(worktree.join("work.txt"), "the child's work\n").unwrap();
        git(&worktree, &["add", "-A"]);
        git(&worktree, &["commit", "-qm", "mush #1: work"]);

        app.refresh_git();
        wait_git(&mut app, &rx);
        let child = app.tree.node(AgentId(1)).expect("the child is in the tree");
        assert!(
            child
                .kept
                .as_deref()
                .is_some_and(|why| why.contains("mush/1")),
            "an unmerged worktree is kept, and the row says why: {:?}",
            child.kept
        );
        assert!(
            worktree.exists(),
            "and it is still on disk — the sweep kept its hands off"
        );

        // The human merges by hand: the work is in HEAD, and the next read is
        // the only thing that has to notice.
        git(&root, &["merge", "--no-edit", "mush/1"]);
        app.refresh_git();
        wait_git(&mut app, &rx);

        let child = app.tree.node(AgentId(1)).expect("the child is still here");
        assert_eq!(
            child.landed,
            Some(Landed::Merged),
            "the row says where the work went"
        );
        assert_eq!(
            child.branch, None,
            "and the branch it named is gone with the checkout"
        );
        assert_eq!(child.kept, None, "nothing is left to say about it");
        assert!(!worktree.exists(), "the checkout is reclaimed");
        assert!(
            git_of(&root, &["rev-parse", "--verify", "refs/heads/mush/1"]).is_err(),
            "and so is the branch, so no row can offer a diff against it"
        );
        // A landed agent is not one to send work to: the path it was spawned
        // with is gone, and a run there would recreate it as a plain directory
        // no surface can see (finding S1).
        assert!(app.worktree_gone(AgentId(1)).is_some());
        let _ = fs::remove_dir_all(&root);
    }

    /// The UI's sweep is the second removal path, and it must reach the same
    /// answer the actor's run end does: a child whose run committed nothing,
    /// with the base moved on since its spawn, is "nothing committed" — never
    /// "merged", which is the lie the row told about an ordinary read-only
    /// child (the defect this patch is).
    #[test]
    fn a_child_that_committed_nothing_is_swept_as_nothing_committed() {
        use std::fs;

        let root = repo("reclaim-nothing-committed");
        let (mut app, rx) = app_and_rx(root.clone());
        wait_git(&mut app, &rx);
        let fork = git_of(&root, &["rev-parse", "HEAD"]).expect("HEAD");
        let conversation = app.tree.conversation();
        app.update(Msg::Agent {
            conversation,
            id: AgentId::ROOT,
            event: AgentEvent::Spawned {
                child: 1,
                parent: 0,
                brief: "look at the repository".to_string(),
                depth: 1,
                branch: Some("mush/1".to_string()),
                fork: Some(fork.clone()),
                title: None,
                cmd: crossbeam_channel::unbounded().0,
            },
        });
        app.update(Msg::Agent {
            conversation,
            id: AgentId(1),
            event: AgentEvent::Done,
        });
        wait_git(&mut app, &rx);
        // The run's worktree, standing on its fork revision and nothing else:
        // the state every read-only child leaves.
        git(
            &root,
            &["worktree", "add", "-q", "-b", "mush/1", ".mush/wt/1"],
        );
        assert_eq!(
            git_of(&root, &["rev-parse", "refs/heads/mush/1"]).unwrap(),
            fork,
            "the branch really is its fork revision"
        );
        // The base moves on the way HEAD does under a session, so the branch is
        // an ancestor of it without anybody merging anything — the shape that
        // used to read "merged into HEAD".
        fs::write(root.join("human.txt"), "the human's own work\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "the human's own work"]);

        app.refresh_git();
        wait_git(&mut app, &rx);

        let child = app.tree.node(AgentId(1)).expect("the child is in the tree");
        assert_eq!(
            child.landed,
            Some(Landed::NothingCommitted),
            "the row says what happened: the run never committed"
        );
        assert_eq!(child.branch, None, "and the branch it named is gone");
        assert!(
            !root.join(".mush/wt/1").exists(),
            "the checkout is reclaimed, even though the base moved on"
        );
        assert!(
            git_of(&root, &["rev-parse", "--verify", "refs/heads/mush/1"]).is_err(),
            "and `branch -d` deletes it: it is an ancestor of the new HEAD"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A nested child's merge lands in its *parent's* branch, and the row says
    /// "merged" — never "into HEAD", which is a ref the work was never in. The
    /// two nodes are real: a real worktree on `mush/1`, a real child on `mush/2`
    /// forked from it, and a real merge back into it.
    #[test]
    fn a_nested_merge_reads_merged_and_never_claims_head() {
        use std::fs;

        let root = repo("reclaim-nested-landing");
        let (mut app, rx) = app_and_rx(root.clone());
        wait_git(&mut app, &rx);
        // The parent's worktree, forked from HEAD, with a commit of its own —
        // the base a nested child forks from.
        let parent_fork = git_of(&root, &["rev-parse", "HEAD"]).expect("HEAD");
        git(
            &root,
            &["worktree", "add", "-q", "-b", "mush/1", ".mush/wt/1"],
        );
        let parent = root.join(".mush/wt/1");
        fs::write(parent.join("parent.txt"), "parent\n").unwrap();
        git(&parent, &["add", "-A"]);
        git(&parent, &["commit", "-qm", "mush #1: parent work"]);
        // The child, forked from the parent's branch...
        let child_fork = git_of(&root, &["rev-parse", "refs/heads/mush/1"]).expect("mush/1");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "mush/2",
                ".mush/wt/2",
                "mush/1",
            ],
        );
        let child = root.join(".mush/wt/2");
        fs::write(child.join("child.txt"), "child\n").unwrap();
        git(&child, &["add", "-A"]);
        git(&child, &["commit", "-qm", "mush #2: child work"]);
        // ...merged back into it, which is where a nested landing happens.
        git(&parent, &["merge", "--no-edit", "mush/2"]);

        let conversation = app.tree.conversation();
        for (child, parent, brief, branch, fork) in [
            (
                1u64,
                0u64,
                "parent work".to_string(),
                "mush/1".to_string(),
                parent_fork,
            ),
            (
                2,
                1,
                "child work".to_string(),
                "mush/2".to_string(),
                child_fork,
            ),
        ] {
            app.update(Msg::Agent {
                conversation,
                id: AgentId(parent),
                event: AgentEvent::Spawned {
                    child,
                    parent,
                    brief,
                    depth: 1,
                    branch: Some(branch),
                    fork: Some(fork),
                    title: (child == 2).then(|| "kid".to_string()),
                    cmd: crossbeam_channel::unbounded().0,
                },
            });
            app.update(Msg::Agent {
                conversation,
                id: AgentId(child),
                event: AgentEvent::Done,
            });
        }
        wait_git(&mut app, &rx);

        app.refresh_git();
        wait_git(&mut app, &rx);

        let child = app.tree.node(AgentId(2)).expect("the child is in the tree");
        assert_eq!(
            child.landed,
            Some(Landed::Merged),
            "the child's work is in the base it was forked from"
        );
        assert_eq!(child.branch, None, "so its branch is dropped with it");
        assert!(
            !root.join(".mush/wt/2").exists(),
            "and the checkout is reclaimed"
        );
        // The row's detail is read in the footer under the list, for the row
        // the cursor is on: point it at the child before the frame is painted.
        assert!(
            app.tree.point_cursor_at(AgentId(2)),
            "the child must have a row to select"
        );
        let rows = screen(&mut app, 100, 30);
        let detail = rows
            .iter()
            .find(|row| row.contains("merged"))
            .unwrap_or_else(|| panic!("the child's landing must be painted: {rows:?}"));
        assert!(
            !detail.contains("HEAD"),
            "a nested landing is not a merge into HEAD: {detail:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// `git::run`'s answer, as the shape a test asserts on rather than an
    /// `unwrap` that hides git's own words.
    fn git_of(root: &std::path::Path, args: &[&str]) -> Result<String, String> {
        mush_core::git::run(root, args)
    }

    // ------------------------------------------------------------- attach (M3)
    //
    // The socket's own half — framing, a bad line, a kept connection — lives in
    // `crate::attach`'s tests. These are the `App` half: the ops, over the tree
    // and the chat, on the same `handle_attach` the message loop calls.

    fn attach_request(id: u64, op: attach::Op) -> attach::Request {
        attach::Request {
            id: serde_json::json!(id),
            op,
        }
    }

    fn attach_ok(response: attach::Response) -> serde_json::Value {
        match response.reply {
            attach::Reply::Ok(body) => body,
            attach::Reply::Err(error) => panic!("expected ok, got {error:?}"),
        }
    }

    fn attach_err(response: attach::Response) -> attach::ReplyError {
        match response.reply {
            attach::Reply::Err(error) => error,
            attach::Reply::Ok(body) => panic!("expected an error, got {body}"),
        }
    }

    /// `read` hands back the transcript lines with the revision a client hands
    /// to `edit`, and `since` skips the lines it has already seen.
    #[test]
    fn attach_read_returns_the_lines_and_a_revision() {
        let (mut app, _rx) = test_app("attach-read");
        app.chat.push_message(AgentId::ROOT, Message::user("first"));
        app.chat
            .push_message(AgentId::ROOT, Message::assistant("second"));

        let body = attach_ok(app.handle_attach(
            "a client",
            &attach_request(1, attach::Op::Read { agent: 0, since: 0 }),
        ));
        assert_eq!(body["agent"], serde_json::json!(0));
        assert_eq!(
            body["revision"].as_u64(),
            Some(app.chat.revision(AgentId::ROOT))
        );
        let lines = body["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["line"], serde_json::json!(0));
        assert_eq!(lines[0]["role"], "user");
        assert_eq!(lines[0]["text"], "first");
        assert_eq!(lines[1]["text"], "second");

        // `since` is the first line to read, so the second read is the tail.
        let body = attach_ok(app.handle_attach(
            "a client",
            &attach_request(2, attach::Op::Read { agent: 0, since: 1 }),
        ));
        let lines = body["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["line"], serde_json::json!(1));
        assert_eq!(lines[0]["text"], "second");
    }

    /// An id the tree does not have is a bad request, not an empty transcript.
    #[test]
    fn attach_read_of_an_unknown_agent_is_a_bad_request() {
        let (mut app, _rx) = test_app("attach-read-missing");
        let error = attach_err(app.handle_attach(
            "a client",
            &attach_request(1, attach::Op::Read { agent: 9, since: 0 }),
        ));
        assert_eq!(error.kind, "bad_request");
        assert!(error.message.unwrap().contains('9'));
    }

    /// The roster is read from the tree: parents, phases, the focused flag, and
    /// the working-children count the row paints as `⏸N` (M3 / H1).
    #[test]
    fn attach_agents_reflects_the_tree() {
        let (mut app, _rx) = test_app("attach-agents");
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: Some("mush/1".to_string()),
            fork: None,
            cmd: crossbeam_channel::unbounded().0,
        });

        let body = attach_ok(app.handle_attach("a client", &attach_request(2, attach::Op::Agents)));
        let agents = body["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 2, "the root and its child");

        assert_eq!(agents[0]["id"], serde_json::json!(0));
        assert_eq!(agents[0]["parent"], serde_json::Value::Null);
        assert_eq!(agents[0]["phase"], "idle");
        assert_eq!(agents[0]["focused"], serde_json::json!(true));
        assert_eq!(
            agents[0]["children_working"],
            serde_json::json!(1),
            "the root has one child thinking"
        );

        assert_eq!(agents[1]["id"], serde_json::json!(1));
        assert_eq!(agents[1]["parent"], serde_json::json!(0));
        assert_eq!(agents[1]["phase"], "thinking");
        // The activity is the painted row's own words, age and all: one
        // derivation for the pane and the wire (finding R21).
        assert!(
            agents[1]["activity"]
                .as_str()
                .unwrap_or_default()
                .starts_with("thinking"),
            "{}",
            agents[1]["activity"]
        );
        assert_eq!(agents[1]["branch"], "mush/1");
        assert_eq!(agents[1]["focused"], serde_json::json!(false));
        assert_eq!(
            agents[1]["revision"].as_u64(),
            Some(app.chat.revision(AgentId(1)))
        );

        // A finished-but-unread result travels the wire from both ends: the
        // child's own mark and the count its parent owes, the same fact the row
        // paints as `✉`/`✉N` and the thing `.mush/session.json` could not say
        // (finding H1).
        app.tree.finish(AgentId(1), Some("lexer done".into()));
        let body = attach_ok(app.handle_attach("a client", &attach_request(3, attach::Op::Agents)));
        let agents = body["agents"].as_array().unwrap();
        assert_eq!(agents[1]["result_unread"], serde_json::json!(true));
        assert_eq!(agents[1]["unread_children"], serde_json::json!(0));
        assert_eq!(agents[0]["result_unread"], serde_json::json!(false));
        assert_eq!(agents[0]["unread_children"], serde_json::json!(1));
    }

    /// `focus` moves the pane, the keyboard and the tree cursor exactly as
    /// `Enter` on the row does — the same `focus_cursor_row` path.
    #[test]
    fn attach_focus_behaves_like_enter_on_the_row() {
        let (mut app, _rx) = test_app("attach-focus");
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd: crossbeam_channel::unbounded().0,
        });
        app.focus = Focus::Agents;
        app.tree.cursor_top();

        let body = attach_ok(app.handle_attach(
            "a client",
            &attach_request(3, attach::Op::Focus { agent: 1 }),
        ));
        assert_eq!(body, serde_json::json!({}));
        assert_eq!(app.tree.focused, AgentId(1), "the pane shows #1");
        assert_eq!(app.focus, Focus::Agents, "and the tree kept the keyboard");
        assert_eq!(
            app.tree.cursor_id(),
            Some(AgentId(1)),
            "the row is selected"
        );

        let error = attach_err(app.handle_attach(
            "a client",
            &attach_request(4, attach::Op::Focus { agent: 9 }),
        ));
        assert_eq!(error.kind, "bad_request");
    }

    /// A client's op is not the human's own key. The warning a `Ctrl-Q` armed
    /// *is* the arm (finding H9), so an op that says its own line — `focus`'s
    /// agent line, `edit`'s draft line — must not take the human's confirmation
    /// out from under them; the second press is still theirs (findings §6).
    #[test]
    fn an_attach_op_does_not_disarm_the_humans_quit() {
        let (mut app, _rx) = test_app("attach-quit-arm");
        spawn_agent(&mut app, 1, 0, 1, "port the parser", None);
        begin_run(&mut app, AgentId(1));

        ctrl(&mut app, 'q');
        assert!(app.quit_armed(), "a live agent's run armed the quit");
        let warning = text_of(&app).to_string();

        let body = attach_ok(app.handle_attach(
            "a client",
            &attach_request(1, attach::Op::Focus { agent: 1 }),
        ));
        assert_eq!(body, serde_json::json!({}), "the op still did its work");
        assert_eq!(app.tree.focused, AgentId(1), "the client moved the pane");
        assert!(app.quit_armed(), "and the human's warning is still there");
        assert_eq!(text_of(&app), warning, "word for word, and with its clock");

        attach_ok(app.handle_attach(
            "a client",
            &attach_request(
                2,
                attach::Op::Edit {
                    agent: 1,
                    base: app.chat.revision(AgentId(1)),
                    text: "half typed".to_string(),
                    send: false,
                },
            ),
        ));
        assert!(app.quit_armed(), "a draft does not take it either");
        assert_eq!(text_of(&app), warning);

        ctrl(&mut app, 'q');
        assert!(
            app.should_quit,
            "the press the human was holding still ends it"
        );
    }

    /// The one line an op may take the warning with: a failure of its own.
    /// `Rank::Alert` is the rank the warning holds too, and the bar has one row
    /// for the two — so the client's failure reaches the human, and the arm
    /// goes with the line it lives on, as any other failure does (findings §6).
    ///
    /// And it is told *to the client*: a message that landed in no mailbox is
    /// not an `Ok` revision. The client used to be handed one, so the words
    /// that never arrived read to the sender as delivered while the human's own
    /// bar said the agent was gone — the one reader who could not see the
    /// failure was the one who sent it.
    #[test]
    fn an_attach_failure_lands_over_the_humans_warning() {
        let (mut app, _rx) = test_app("attach-quit-failure");
        // A node the tree has no mailbox for at all: the send this op is about
        // to take has nowhere to go. A *dead* mailbox is not this case any more
        // — that is a parked child, and the words wake it (§8.21).
        spawn_agent(&mut app, 1, 0, 1, "port the parser", None);
        begin_run(&mut app, AgentId(1));
        app.tree.agent_tx.remove(&AgentId(1));

        ctrl(&mut app, 'q');
        assert!(app.quit_armed());

        let error = attach_err(app.handle_attach(
            "a client",
            &attach_request(
                3,
                attach::Op::Edit {
                    agent: 1,
                    base: app.chat.revision(AgentId(1)),
                    text: "are you there?".to_string(),
                    send: true,
                },
            ),
        ));
        assert_eq!(error.kind, "bad_request", "the sender is told: {error:?}");
        assert_eq!(error.message, Some(agent::gone(AgentId(1))));

        assert_eq!(
            text_of(&app),
            "agent #1 is gone",
            "the op's own failure is what the human reads"
        );
        assert!(!app.quit_armed(), "and the line it lives on went with it");
    }

    /// A read's revision is only meaningful inside one conversation. Ctrl-N
    /// moves it forward and the payload names the new conversation, so a
    /// client's stale token conflicts instead of landing a draft in the wrong
    /// chat (finding A1).
    #[test]
    fn a_read_revision_is_scoped_to_its_conversation() {
        let (mut app, _rx) = test_app("attach-epoch");
        let before = attach_ok(app.handle_attach(
            "a client",
            &attach_request(1, attach::Op::Read { agent: 0, since: 0 }),
        ));
        let revision = before["revision"].as_u64().expect("a revision");
        let conversation = before["conversation"].as_u64().expect("an epoch");

        ctrl(&mut app, 'n');

        let after = attach_ok(app.handle_attach(
            "a client",
            &attach_request(2, attach::Op::Read { agent: 0, since: 0 }),
        ));
        assert!(
            after["revision"].as_u64().unwrap() > revision,
            "the counter moved on, it did not restart: {after}"
        );
        assert_ne!(
            after["conversation"].as_u64().unwrap(),
            conversation,
            "and the epoch names the conversation the revision belongs to"
        );

        let error = attach_err(app.handle_attach(
            "a client",
            &attach_request(
                3,
                attach::Op::Edit {
                    agent: 0,
                    base: revision,
                    text: "a stale draft".to_string(),
                    send: false,
                },
            ),
        ));
        assert_eq!(
            error.kind, "conflict",
            "a token from before Ctrl-N cannot land in the new chat"
        );
        assert_eq!(app.chat.input().text(), "", "and the box is untouched");
    }

    /// A client's words are a message, never a command (the keyboard is where
    /// commands live), and an empty one is refused rather than sent as a blank
    /// user turn that starts a run (finding A4).
    #[test]
    fn an_empty_attach_send_is_refused_and_changes_nothing() {
        let (mut app, _rx) = test_app("attach-empty-send");
        let revision = app.chat.revision(AgentId::ROOT);
        let error = attach_err(app.handle_attach(
            "a client",
            &attach_request(
                9,
                attach::Op::Edit {
                    agent: 0,
                    base: revision,
                    text: "   \n".to_string(),
                    send: true,
                },
            ),
        ));
        assert_eq!(error.kind, "bad_request");
        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "nothing was appended"
        );
        assert_eq!(
            app.chat.revision(AgentId::ROOT),
            revision,
            "and nothing moved"
        );
        assert!(
            !app.tree.node(AgentId::ROOT).unwrap().phase.is_busy(),
            "no run was started"
        );
    }

    /// A refusal changes nothing: a client's message that could not run is not
    /// committed to the root's transcript, and the human's box is not handed a
    /// draft nobody wrote. The refusal used to be answered *after* `deliver`
    /// had pushed the message and flushed the session, so words that never ran
    /// were in the conversation the next run would read (Tier 3 §2).
    #[test]
    fn a_refused_attach_send_leaves_the_root_transcript_unchanged() {
        let (mut app, _rx) = test_app("attach-refused-root");
        // The root's mailbox is gone, which is the one way `deliver` can refuse
        // a root message: `Ctrl-N` restarts it.
        app.tree.agent_tx.remove(&AgentId::ROOT);
        let revision = app.chat.revision(AgentId::ROOT);
        let error = attach_err(app.handle_attach(
            "a client",
            &attach_request(
                4,
                attach::Op::Edit {
                    agent: 0,
                    base: revision,
                    text: "anyone home?".to_string(),
                    send: true,
                },
            ),
        ));
        assert_eq!(error.kind, "bad_request");
        assert_eq!(
            error.message.as_deref(),
            Some("root agent is gone — Ctrl-N restarts it"),
            "the client is told why"
        );
        assert!(
            app.chat.transcript(AgentId::ROOT).is_empty(),
            "the refused words are not in the transcript"
        );
        assert_eq!(
            app.chat.input().text(),
            "",
            "and not in the human's box either"
        );
        assert_eq!(app.chat.revision(AgentId::ROOT), revision, "nothing moved");
    }

    /// A branch whose worktree is gone is not a place a client can read files
    /// or run commands: the roster reports the main checkout, where the work
    /// actually is (finding A8). Reporting the dead path sent a client's next
    /// `read_file` into a phantom directory.
    #[test]
    fn the_roster_does_not_report_a_dead_worktree() {
        let (mut app, _rx) = test_app("attach-dead-worktree");
        spawn_agent(&mut app, 1, 0, 1, "a task", Some("mush/1"));
        // Nothing created `.mush/wt/1`, which is what a merged or discarded
        // agent looks like from here.

        let body = attach_ok(app.handle_attach("a client", &attach_request(1, attach::Op::Agents)));
        let agents = body["agents"].as_array().expect("a roster");
        let row = agents
            .iter()
            .find(|node| node["id"] == 1)
            .expect("the child's row");
        assert_eq!(
            row["worktree"],
            serde_json::json!(app.ws.root().display().to_string()),
            "the worktree is where the work is"
        );
    }

    /// A stale base is a `conflict` that names the revision that moved, and it
    /// changes nothing — not the box, not the transcript.
    #[test]
    fn attach_edit_with_a_stale_base_conflicts_and_changes_nothing() {
        let (mut app, _rx) = test_app("attach-conflict");
        let stale = app.chat.revision(AgentId::ROOT);
        // Something moved the transcript after the client last read.
        app.chat
            .push_message(AgentId::ROOT, Message::assistant("a reply"));

        let response = app.handle_attach(
            "a client",
            &attach_request(
                5,
                attach::Op::Edit {
                    agent: 0,
                    base: stale,
                    text: "a draft".to_string(),
                    send: false,
                },
            ),
        );
        let error = attach_err(response);
        assert_eq!(error.kind, "conflict");
        assert_eq!(
            error.revision,
            Some(app.chat.revision(AgentId::ROOT)),
            "the client is told where the transcript went"
        );
        assert_eq!(app.chat.input().text(), "", "the draft was not written");
        assert_eq!(
            app.chat.transcript(AgentId::ROOT).len(),
            1,
            "and nothing was appended"
        );
    }

    /// A fresh base lands a draft in the box and moves the revision, so the
    /// same base cannot be spent twice.
    #[test]
    fn attach_edit_with_a_fresh_base_lands_a_draft() {
        let (mut app, _rx) = test_app("attach-draft");
        let base = app.chat.revision(AgentId::ROOT);
        let body = attach_ok(app.handle_attach(
            "a client",
            &attach_request(
                6,
                attach::Op::Edit {
                    agent: 0,
                    base,
                    text: "a draft from the agent".to_string(),
                    send: false,
                },
            ),
        ));
        assert_eq!(body["revision"].as_u64(), Some(base + 1));
        assert_eq!(app.chat.input().text(), "a draft from the agent");

        // The revision the first edit returned is the base a second one needs;
        // the old base is refused.
        let error = attach_err(app.handle_attach(
            "a client",
            &attach_request(
                7,
                attach::Op::Edit {
                    agent: 0,
                    base,
                    text: "again".to_string(),
                    send: false,
                },
            ),
        ));
        assert_eq!(error.kind, "conflict");
    }

    /// `send: true` speaks to the agent the way a typed message does: the line
    /// lands in its transcript as the human's and its mailbox gets a nudge —
    /// without moving the human's own focus off the agent they were watching.
    #[test]
    fn attach_edit_that_sends_reaches_the_mailbox() {
        let (mut app, _rx) = test_app("attach-send");
        let (cmd, mailbox) = crossbeam_channel::unbounded::<AgentMsg>();
        app.tree.insert(Spawn {
            id: AgentId(1),
            parent: AgentId::ROOT,
            brief: "lexer".to_string(),
            depth: 1,
            branch: None,
            fork: None,
            cmd,
        });
        let base = app.chat.revision(AgentId(1));

        let body = attach_ok(app.handle_attach(
            "a client",
            &attach_request(
                8,
                attach::Op::Edit {
                    agent: 1,
                    base,
                    text: "also rename the module".to_string(),
                    send: true,
                },
            ),
        ));
        assert_eq!(body["revision"].as_u64(), Some(base + 1));
        assert_eq!(
            app.chat.transcript(AgentId(1)).last().map(Message::text),
            Some("also rename the module"),
            "the words are in the agent's transcript"
        );
        assert_eq!(
            app.tree.focused,
            AgentId::ROOT,
            "the human's pane did not move"
        );
        assert!(
            matches!(
                mailbox.try_recv(),
                Ok(AgentMsg::Nudge(message)) if message.text() == "also rename the module"
            ),
            "the message reaches the agent's mailbox the way a typed one does"
        );
    }

    /// The CLI's printers read a body with `attach::Roster`/`attach::Transcript`
    /// — one shape per answer — instead of fishing each key out of a `Value` and
    /// defaulting what is missing. Pinned against a *real* `handle_attach` body,
    /// the only place the producer's keys and the client's shape meet: a key
    /// renamed on the producer's side is now the client's error, where before it
    /// painted an empty column forever with nothing failing (finding R23).
    #[test]
    fn the_cli_shapes_read_the_roster_the_producer_writes() {
        let (mut app, _rx) = test_app("attach-shape-roster");
        let body = attach_ok(app.handle_attach("a client", &attach_request(1, attach::Op::Agents)));

        let roster = attach::Roster::read(&body).expect("the producer's roster reads back");
        assert_eq!(roster.agents.len(), 1, "the root alone");
        let root = &roster.agents[0];
        assert_eq!(root.id, 0);
        assert_eq!(root.phase, "idle");
        assert_eq!(root.parent, None, "the root hangs under nothing");
        assert_eq!(root.activity, None, "an idle agent says nothing");
        assert_eq!(root.children_working, 0);

        let mut broken = body;
        broken["agents"][0].as_object_mut().unwrap().remove("phase");
        let error = attach::Roster::read(&broken).expect_err("a body missing `phase` is refused");
        assert!(error.contains("phase"), "the missing key is named: {error}");
    }

    /// The same for a `read` answer: the transcript's lines and their indices,
    /// and a line missing its `text` refused by name (finding R23).
    #[test]
    fn the_cli_shapes_read_the_transcript_the_producer_writes() {
        let (mut app, _rx) = test_app("attach-shape-transcript");
        app.chat.push_message(AgentId::ROOT, Message::user("hi"));
        let body = attach_ok(app.handle_attach(
            "a client",
            &attach_request(2, attach::Op::Read { agent: 0, since: 0 }),
        ));

        let transcript = attach::Transcript::read(&body).expect("the producer's lines read back");
        assert_eq!(transcript.lines.len(), 1);
        assert_eq!(transcript.lines[0].line, 0);
        assert_eq!(transcript.lines[0].text, "hi");

        let mut broken = body;
        broken["lines"][0].as_object_mut().unwrap().remove("text");
        let error =
            attach::Transcript::read(&broken).expect_err("a line missing `text` is refused");
        assert!(error.contains("text"), "the missing key is named: {error}");
    }
}
