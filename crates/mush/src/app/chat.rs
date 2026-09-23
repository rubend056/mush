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
//! A line said is also *who* said it ([`Voice`]). A transcript holds four kinds
//! of user line and only one of them is the human's: the brief a parent spawned
//! an agent with, a parent's steering, mush's own report about a child or a job,
//! and the words typed into this terminal. The box is the human's voice, so the
//! line that answers a send is theirs and a line that arrives unasked is
//! somebody else's — the fact that keeps `you › ` meaning the human.
//!
//! A notice is a line *about* a conversation, and it has a lifetime here rather
//! than a life of its own. It carries when it happened and which agent it
//! concerns; it is one of two kinds, and they age differently. **Chatter** — a
//! hint, a command's answer, a diff, a usage line, anything a run did not fail
//! at — belongs to the moment it answers: the agent's next run ends it, the
//! human's next send ends it, and `SAID_TTL` ends it if neither happens. A
//! repeated chatter line collapses into one with a count, so an empty-reply
//! loop cannot spend the foot row by row. **News** — a failure, a run mush
//! stopped — belongs to its run: a new one replaces the agent's old one, it is
//! written to the session so a restart still says what broke, and no clock
//! takes it away. Before this, every notice ever written stayed until Ctrl-N, a
//! failure from twenty runs ago was painted under the newest message as if it
//! were the newest thing said, none of it survived a restart, and a line about
//! one moment spent the foot for the life of the session.
//!
//! What a pane paints is built here too (`painted`), because which rows it shows
//! is a fact about the conversation, its scrollback and its notes — not about the
//! terminal: width and height are arguments, the blank separator that closes a
//! message is trimmed before the window is cut, every block that is not the
//! human's own words or the model's reply is folded to its kind's number of rows
//! ([`Fold`], with the `…` row that says what is hidden), and the foot is capped
//! and counted. `ui.rs` keeps the frame around it — the border, the prompt and the
//! cursor — and paints what this returns, title included, because a pane one row
//! tall has no row to spend on saying what it is hiding, or that the human has
//! scrolled away from the bottom.

use std::cell::Cell;
use std::collections::HashMap;
use std::time::Duration;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use mush_core::message::{Image, Message};
use mush_core::session;
use mush_core::text::{markdown_rows, sanitize, truncate, wrap_text, wrap_text_capped};
use mush_core::transcript;

use crate::agent::summarize_args;
use crate::app::image_label;
use crate::app::keys::ChatKey;
use crate::app::short_age;
use crate::ids::AgentId;
use crate::input::Input;
use crate::ui::{dim, image_count};

/// The dots behind a pane's activity line: `thinking.`, `thinking..`,
/// `thinking...`, one a second, looping.
///
/// The dots are the whole animation. They replaced a ten-frame braille spinner
/// (`⠋⠙⠹…`) that `App::tick` advanced once per event-loop pass: a 30 ms poll
/// turned that glyph over thirty times a second, which is a flicker rather than
/// a pulse — and a decoration that repaints a frame is not free. The word in
/// front of them is the phase's own ([`Pane::words`]), so the foot and the row
/// spell one phase once; the beat comes from the caller ([`Pane::spin`])
/// rather than from each tool call, so a run that changes tools does not
/// restart mid-dot.
fn dotted(words: &str, beats: u64) -> String {
    format!("{words}{}", ".".repeat(1 + (beats % 3) as usize))
}

/// The most rows the foot may take from the transcript: two rows of notes and
/// the one that says how many lines are not shown. The transcript is the point
/// of the pane, so a foot of twenty lines is a transcript of four.
const FOOT_ROWS: usize = 3;
/// Of those rows, how many the notes themselves may have before the rest are
/// arithmetical. Two, because the third is what says they are an excerpt.
const FOOT_NOTE_ROWS: usize = 2;

/// How long a chatter line is worth a row of the foot, in seconds.
///
/// The bar fades an `Info` line after five seconds (`INFO_TTL`) because the bar
/// is glanced at as it changes; the foot is read after the fact — a human looks
/// up from the model to see what mush said — so the same five seconds would
/// erase `/help` before it was read. Two minutes is long enough for that glance
/// and short enough that a line about one moment cannot become furniture, which
/// is the half of finding U8 a run's end and the human's next send do not
/// cover: an agent that never runs again leaves nothing behind but this.
const SAID_TTL: u64 = 120;

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
    /// How many times in a row this exact line was said. One line said twice is
    /// one line — an empty-reply loop is one fact, not five — but a human
    /// reading it is owed the number (see [`Self::line`]).
    pub count: u32,
    pub text: String,
}

/// Only failures are red. Hints — `/help`, the git command to merge a branch —
/// are information, and colouring them like errors is how a screen cries wolf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Error,
    /// A run mush itself stopped — the loop guard ending a model that kept
    /// repeating one call. Nothing the model did *failed*: R4 reserves `!` for
    /// the things that did, and the stop is the honest mark — `⊘`, the same
    /// reading `status` gives a stopped child.
    Stopped,
    /// A run that never ended: the process went away with it in flight, or the
    /// agent's actor vanished. Not a failure — nothing the model did broke —
    /// and not a stop — the human did not ask for it, and there is no actor
    /// left to resume. `⚠`, because what it leaves behind is a worktree nobody
    /// should trust before looking at it (finding H2).
    CutOff,
}

impl NoticeKind {
    /// The mark a line of this kind leads with, and how it is painted: `·` for a
    /// line mush wrote, `⊘` for a run it stopped, `⚠` for a run that never
    /// ended, `!` in red for one it failed to do.
    fn mark(self) -> (&'static str, Style) {
        match self {
            NoticeKind::Info => ("· ", dim()),
            NoticeKind::Stopped => ("⊘ ", Style::default().fg(Color::Yellow)),
            NoticeKind::CutOff => ("⚠ ", Style::default().fg(Color::Yellow)),
            NoticeKind::Error => ("! ", Style::default().fg(Color::Red)),
        }
    }
}

/// Who said one line of a transcript.
///
/// A conversation is not only the human's words: a child's pane opens with the
/// brief its parent spawned it with, a folded completion (`#1 done: …`) is
/// mush's own report of another agent, and a `control` message puts a
/// parent's words in a child's transcript. All three used to render as
/// `you › ` — the human's own voice, in their own mouth, for words they never
/// said. Each of them is now a voice of its own, so `you › ` means the human
/// again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Voice {
    /// The human at this terminal: what they typed into the message box.
    #[default]
    Human,
    /// The brief a parent spawned this agent with, which opens its transcript.
    Brief,
    /// A parent's words to this agent (a `control` message). `control`
    /// only reaches the sender's own children, so the speaker is its parent.
    Parent,
    /// A line mush itself wrote into the conversation: a child's or a job's
    /// report, a fold's carried summary. Nobody said it, so it has no voice —
    /// it is marked like the other lines mush writes.
    Mush,
}

impl Voice {
    /// The mark this voice leads with, and the style it is painted in.
    ///
    /// Every voice is here, [`Voice::Mush`] included: it is not a speaker, so
    /// its mark carries no speaker's colour — the `· ` that names mush's own
    /// words, in the dim style the pane paints those rows in. It used to be
    /// spelled twice, once here and once in an arm of `render_message`, with
    /// two styles and the copy here unreachable (finding C1).
    fn mark(self) -> (&'static str, Style) {
        match self {
            Voice::Human => ("you › ", Style::default().fg(Color::Cyan)),
            Voice::Brief => ("brief › ", Style::default().fg(Color::Cyan)),
            Voice::Parent => ("parent › ", Style::default().fg(Color::Magenta)),
            Voice::Mush => ("· ", dim()),
        }
    }
}

/// What wins when more than one line wants to be a pane's last (finding B12):
/// a failure first, then derived activity, then what mush merely said.
///
/// This is the one precedence table. The bar picks the line it shows through
/// [`Rank::last_word`]; a notice says where it sits through [`Notice::rank`];
/// and the foot keeps the alert over the run in flight over a hint by the same
/// order — so no two surfaces can disagree about which of two things the human
/// needs to see first, which is how an `Error` status came to lose to a
/// `thinking…` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// Where this line sits in the precedence table. A failure, a run mush
    /// stopped and a run that never ended are all the thing the human has to
    /// read; only a line mush merely wrote yields.
    pub fn rank(&self) -> Rank {
        match self.kind {
            NoticeKind::Error | NoticeKind::Stopped | NoticeKind::CutOff => Rank::Alert,
            NoticeKind::Info => Rank::Said,
        }
    }

    /// The line as it is read: the text, and how often it was said when it was
    /// said more than once. Collapsing repeats must not hide that the thing
    /// happened five times — the count is the difference between "the reply was
    /// empty" and "the reply was empty every turn until the run gave up".
    fn line(&self) -> String {
        if self.count > 1 {
            format!("{} ×{}", self.text, self.count)
        } else {
            self.text.clone()
        }
    }

    /// Whether this line is chatter: a line about one moment, said at one
    /// moment. Everything a run did *not* fail at is chatter — a hint, a
    /// command's answer, a diff, a usage line — and it ends with the moment
    /// (see [`Chat::dismiss_said`] and [`Chat::expire_said`]). A failure or a
    /// stop is news: it belongs to its run, is written to the session, and only
    /// the next run replaces it.
    ///
    /// A cut-off line is news in the same sense — it is a fact about a run, not
    /// about a moment — but not in the second one: nothing writes it to the
    /// session, because the stored *status* already carries it in the row's own
    /// vocabulary and a restored `⚠` must not come back as a red `!`.
    fn is_chatter(&self) -> bool {
        self.kind == NoticeKind::Info
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
    /// Where the select mode is painted, when it is on this pane's
    /// conversation. `None` is the ordinary reading.
    pub select: Option<SelectRows>,
}

/// The rows of a painted pane the select mode marks: indices into
/// [`Painted::lines`].
///
/// Positions, not styles: the colour a cursor wears is a painting decision and
/// lives in `ui.rs` with every other colour, so a frame says only which rows
/// wear it — and a test can read which rows the mode is on without a terminal.
pub struct SelectRows {
    /// Every painted row of the cursor's own source line.
    pub cursor: Vec<usize>,
    /// Every painted row of the source lines the selection covers.
    pub selected: Vec<usize>,
}

/// The lines mush wrote about one agent, as `/notes` reads them: the rows of
/// the list, oldest first, and the row the newest note starts at.
///
/// The second half is the answer to finding T7: the report used to be a bare
/// list of rows, so the popup could only open on the last one — and a long note
/// wraps into many rows, so the row it opened on was the middle of a sentence,
/// with the stamp and the start of the note above the fold. Where a note begins
/// is a fact about the notes, so it is derived here rather than guessed from the
/// rows by whoever opens the list.
pub struct Notes {
    pub rows: Vec<String>,
    /// The index of the newest note's first row. `0` for an empty list.
    pub newest: usize,
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
/// what that agent's run is doing, which beat its dots are on, and the
/// endpoint/model line the empty state names.
#[derive(Clone)]
pub struct Pane<'a> {
    pub agent: AgentId,
    /// What the run is doing, in the row's own words
    /// ([`Phase::words`](crate::app::tree::Phase::words)), or `None` when there
    /// is nothing in flight. Carried rather than derived here, so the foot
    /// cannot spell a phase differently from the row beside it — and a run
    /// parked in a `wait` says what it waits on (`waiting on results.`) instead
    /// of the silence that made the pane tell a human less than the row
    /// (finding U7, superseded).
    pub words: Option<String>,
    /// The beat [`dotted`] counts from: one a second while anything is in
    /// flight (`DOT_PERIOD`), not one a frame.
    pub spin: u64,
    pub label: &'a str,
}

/// Where one pane is reading from: a position the human owns.
///
/// The bottom is a *state*, not an offset. While a pane is at the bottom it
/// follows the newest line; the moment the human scrolls away it is **holding**
/// a window — the last `offset` rows of a transcript that was `up_to` messages
/// long — so lines that arrive afterwards land *below* the window instead of
/// pushing the text the human is reading up the pane (finding U3). Nothing in
/// this program may reset it: the only thing that knows they want the bottom is
/// them, and the next key they press says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reading {
    /// At the bottom: the newest line is the last row, and stays that way.
    Following,
    /// Holding a window `offset` rows above the bottom of `messages[..up_to]`.
    Holding { offset: usize, up_to: usize },
}

impl Reading {
    /// The held window's `(offset, up_to)`, if the transcript still has it: a
    /// fold or a shorter restored session can leave a reading pointing past the
    /// end, and a position the transcript no longer has is not a position — the
    /// pane is at the bottom again. One rule, so the pane's title and its body
    /// cannot disagree about which transcripts this reading may be read from.
    fn held(self, messages: usize) -> Option<(usize, usize)> {
        match self {
            Reading::Holding { offset, up_to } if up_to <= messages => Some((offset, up_to)),
            _ => None,
        }
    }
}

/// What the box last lost, waiting for `Ctrl-Z`.
///
/// It holds *what went*, not a copy of the whole box: a pop takes one image and
/// leaves the words, a `Ctrl-U` takes the words and leaves the images — so the
/// slot is the half that is gone, and a restore puts it back without touching
/// the half that never left. That is also what keeps it cheap: a snapshot of
/// the box would copy every image's bytes on a keystroke, and a restore from one
/// would silently overwrite a draft typed after the loss.
#[derive(Debug, Default)]
struct Lost {
    /// The words the loss took, if the loss took any.
    words: String,
    /// The images the loss took: the newest one for a `Backspace` pop, every
    /// one for Esc, none for a `Ctrl-U`.
    images: Vec<Image>,
}

/// The select mode: a cursor over the transcript's *own source lines*, and the
/// window a pane shows it through.
///
/// `Ctrl-Y` starts it and `Enter` copies, so this is the one road from a pane to
/// the clipboard. What it copies is the transcript, not the rows: a painted row
/// is a wrap of a source line at one terminal's width, and a drag over the
/// screen is the terminal's rectangle — neither is text another program can be
/// handed. `Message::text()` is, and every source line is one line of it, so
/// the cursor moves over those lines and the copy is them joined with the
/// newlines the transcript has — a soft wrap never becomes one.
///
/// The mode is *modal*: while it is on the keys belong to it (`keys::key`
/// routes them before the panes, the way a picker does), so a letter is not
/// typing and `Esc` is not the box's clear.
#[derive(Debug)]
struct Selecting {
    /// The agent whose pane this cursor is over: the mode belongs to one
    /// conversation, and a pane showing another one paints no cursor.
    agent: AgentId,
    /// The cursor: an index into that agent's transcript, and an index into
    /// `Message::text().split('\n')` — one *source* line, which is one or more
    /// painted rows.
    cursor: (usize, usize),
    /// The other end of a selection while `Shift-↑`/`Shift-↓` extends one.
    /// `None` is a bare cursor, and `Enter` then copies its own line.
    anchor: Option<(usize, usize)>,
    /// The pane's window: which message row `top.0`'s chunk starts at, and how
    /// many rows of it the window drops (`top.1`).
    ///
    /// A `Cell` because only the frame knows the pane's measure. The cursor is
    /// the state the keys own; where the pane can show it depends on a width and
    /// a height the keys never see, so the frame places the window as it paints
    /// and leaves the cursor alone. The placement is idempotent, and one a
    /// resize left behind is put right by the next frame or key.
    top: Cell<(usize, usize)>,
}

/// What the select mode's keys do.
///
/// `keys::key` decides *which* key is which (the one keymap, [`ChatKey`]'s
/// reason), and [`Chat::select_apply`] is the one place they run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectKey {
    /// `Ctrl-Y`: open the mode, with the cursor on the newest line. The key
    /// that opens the mode belongs with the keys that work it, and it is the
    /// one [`Chat::start_select`] answers; the others go through
    /// [`Chat::select_apply`].
    Start,
    /// `↑`/`↓` and `PgUp`/`PgDn`: move the cursor `step` source lines, positive
    /// toward the newest.
    Move(i64),
    /// `Shift-↑`/`Shift-↓`: move the cursor and keep the selection reaching back
    /// to where it started.
    Extend(i64),
    /// `Home`: the oldest source line in the pane.
    First,
    /// `End`: the newest.
    Last,
    /// `Enter`: put the selection (or the cursor's own line) on the clipboard
    /// and leave the mode.
    Copy,
    /// `Esc`: leave without copying.
    Cancel,
}

/// What `Enter` hands the app: the text to put on the clipboard, and the line
/// mush says once it is there.
///
/// The line is built with the copy because the count of lines, the byte count
/// and *what* was copied are facts about the selection; the clipboard decides
/// only whether the copy worked ([`crate::clipboard::write_text`]).
pub struct Copied {
    /// The transcript's own text, exactly as it is: the selected source lines
    /// joined with the `\n`s they have between them.
    pub text: String,
    /// `copied 12 lines from #1's reply — 1,284 bytes`.
    pub line: String,
}

/// The line the bar reads after Esc cleared the box: what went, and the one key
/// that puts it back. `None` when the box had nothing to lose — Esc on an empty
/// box clears nothing, so there is nothing to name and no road to offer — which
/// is also the caller's guard against taking down a draft that is not there.
fn cleared_line(had_words: bool, images: usize) -> Option<String> {
    let what = match (had_words, images) {
        (false, 0) => return None,
        (true, 0) => "the box".to_string(),
        (true, n) => format!("the box and {}", image_count(n)),
        (false, n) => image_count(n),
    };
    Some(format!("cleared {what} · Ctrl-Z puts it back"))
}

/// The rows one message paints, and the source line each row is the reading of.
///
/// The map is what lets a *source line* be found among painted rows at all: a
/// wrapped row is not a line of the text, and a block the fold paints
/// ([`folded_marked`]) shows fewer rows than its text has lines. `rows` runs
/// parallel to `lines` and is `None` for a row that is not the message's own
/// words — the reasoning, a tool call, a picture label, the blank that closes a
/// message.
#[derive(Default)]
struct Chunk {
    lines: Vec<Line<'static>>,
    rows: Vec<Option<(usize, usize)>>,
}

impl Chunk {
    /// The first `rows` rows, for the one caller that wants the part of a
    /// message above a row it already found.
    fn cut(mut self, rows: usize) -> Self {
        self.lines.truncate(rows);
        self.rows.truncate(rows);
        self
    }

    /// Append the rows from `skip` on, at most `room` of them.
    fn take_into(self, body: &mut Body, skip: usize, room: usize) {
        for (line, row) in self.lines.into_iter().zip(self.rows).skip(skip).take(room) {
            body.lines.push(line);
            body.rows.push(row);
        }
    }
}

/// A pane's rows, and where each row's own text came from.
#[derive(Default)]
struct Body {
    lines: Vec<Line<'static>>,
    /// Parallel to `lines`: `(message index, source line)`, the provenance a
    /// [`Chunk`] carries once the message is known.
    rows: Vec<Option<(usize, usize)>>,
}

impl Body {
    /// Drop the blank rows that close the transcript, if it closes here: at one
    /// row of pane that blank would be the only visible line (finding B4).
    fn trim_trailing_blanks(&mut self) {
        let mut kept = self.lines.len();
        while self.lines[..kept].last().map(|line| line.width()) == Some(0) {
            kept -= 1;
        }
        self.lines.truncate(kept);
        self.rows.truncate(kept);
    }
}

/// The source lines of one message the pane paints text rows for, or `None` for
/// a message whose text has no row of its own.
///
/// The predicate is the `render_message` arms' own: a user line is always
/// painted (the mark is, even for a message that is only a picture), a reply
/// with no words paints nothing, and a folded block — a tool result, a report,
/// a brief — is painted from its first row: the fold decides *which* rows,
/// never whether the line exists, so the cursor can still walk the lines a long
/// block hides behind its `…` and copy them whole.
fn lines_of(message: &Message) -> Option<Vec<&str>> {
    match message.role.as_str() {
        "user" | "tool" => Some(message.text().split('\n').collect()),
        "assistant" if !message.text().trim().is_empty() => {
            Some(message.text().split('\n').collect())
        }
        _ => None,
    }
}

/// The first row of a message that is the reading of source line `line`, or —
/// for a line the fold hid — the last row that is the reading of a line at
/// or before it: the result's `…`, which stands for the rows that did not fit.
fn first_row(rows: &[Option<(usize, usize)>], message: usize, line: usize) -> Option<usize> {
    rows.iter()
        .position(|row| *row == Some((message, line)))
        .or_else(|| last_row(rows, message, line))
}

/// The last row that is the reading of `line`: a wrapped line is several rows,
/// and a window that ends on the cursor's line wants its end, not its start.
fn last_row(rows: &[Option<(usize, usize)>], message: usize, line: usize) -> Option<usize> {
    rows.iter()
        .rposition(|row| *row == Some((message, line)))
        .or_else(|| {
            rows.iter().rposition(
                |row| matches!(row, Some((at, row_line)) if *at == message && *row_line <= line),
            )
        })
}

/// One past the last row of a chunk that is the reading of the message's own
/// text. A window the pane's height cut *before* this row is hiding text; one
/// cut at or after it has all the words on screen, whatever else it left below
/// (the picture labels, the blank that closes a message).
fn last_text(chunk: &Chunk) -> usize {
    chunk
        .rows
        .iter()
        .rposition(|row| row.is_some())
        .map_or(0, |at| at + 1)
}

/// Which row of a window the cursor is painted on: the cursor's own line's
/// first row, or — for a line the cap hid — the row [`first_row`]'s fallback
/// names. `None` is "this window does not show the cursor", which is what makes
/// the frame place the window again.
///
/// `cut` is the message whose rows the window's height cut short of its text: a
/// cut message cannot answer for a hidden line, because the cursor's line may be
/// under the cut rather than behind the cap.
fn cursor_row(
    rows: &[Option<(usize, usize)>],
    cursor: (usize, usize),
    cut: Option<usize>,
) -> Option<usize> {
    if let Some(at) = rows.iter().position(|row| *row == Some(cursor)) {
        return Some(at);
    }
    if cut == Some(cursor.0) {
        return None;
    }
    rows.iter().rposition(
        |row| matches!(row, Some((message, line)) if *message == cursor.0 && *line <= cursor.1),
    )
}

/// Whether the cursor sits above everything a window shows. A window with no
/// text row in it at all has nothing to be above, and reads as below — the
/// bottom anchoring is the one that ends up showing the cursor.
fn cursor_above(body: &Body, cursor: (usize, usize)) -> bool {
    match body.rows.iter().flatten().next() {
        Some(&first) => cursor < first,
        None => false,
    }
}

/// The rows a window paints the mode on: the cursor's own line, and every
/// source line the selection covers.
///
/// `cursor` is the caller's clamp of the mode's own, not `select.cursor`: the
/// row the pane paints is the row the window was placed for.
fn select_rows(
    rows: &[Option<(usize, usize)>],
    select: &Selecting,
    cursor: (usize, usize),
    cut: Option<usize>,
) -> Option<SelectRows> {
    let at = cursor_row(rows, cursor, cut)?;
    // Every row of the cursor's own line, not just the one the lookup landed
    // on: a wrapped line is one line, and every row of it is the cursor.
    let tag = rows[at];
    let painted: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| **row == tag)
        .map(|(at, _)| at)
        .collect();
    let selected = match select.anchor {
        Some(anchor) if anchor != cursor => {
            let (from, to) = if anchor < cursor {
                (anchor, cursor)
            } else {
                (cursor, anchor)
            };
            rows.iter()
                .enumerate()
                .filter(|(_, row)| row.is_some_and(|tag| from <= tag && tag <= to))
                .map(|(at, _)| at)
                .collect()
        }
        // A bare cursor is not a selection: the one line it covers would wear
        // both styles and say nothing the cursor does not.
        _ => Vec::new(),
    };
    Some(SelectRows {
        cursor: painted,
        selected,
    })
}

/// One conversation: what has been said, what mush added to it, and what the
/// human is typing.
pub struct Chat {
    /// The system prompt the root conversation runs with. It is not stored in
    /// the session — it names a workspace that may have moved — so it lives
    /// here, next to the transcript it opens.
    system: Message,
    /// Each *subagent's* own system prompt, keyed by id: the prompt its actor
    /// was built with, published once per actor ([`AgentEvent::SystemPrompt`]).
    ///
    /// The root's is `system` above. A child's cannot be rebuilt here: it names
    /// the workspace the child's own tools resolve paths in, its depth, and
    /// whether it is isolated — facts decided where the child is built, and a
    /// second derivation here would be a second answer to "which prompt does
    /// this agent send". The audit measured what that cost while the root's
    /// prompt stood in for every agent: 3,247 B against a shared leaf's 1,634,
    /// so a focused leaf's meter read 1,613 B ≈ 537 tokens heavier than the
    /// history its actor sends. An agent whose actor has not published one
    /// weighs no prompt: the number is the actor's fact, not this table's
    /// guess.
    systems: HashMap<AgentId, Message>,
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
    /// The images attached to the next message, in the order they were
    /// attached. They live beside the box and not in it because they are not
    /// text: the box edits graphemes, and a picture has none. They travel with
    /// the send — [`Chat::take_attachments`] is the send's half — and a
    /// refused send hands them back the way it hands the words back.
    attachments: Vec<Image>,
    /// The draft the box last lost — Esc's clear, a `Backspace` pop, or a
    /// `Ctrl-U` — for `Ctrl-Z` to put back. One slot, not a stack: the key
    /// answers the loss that just happened or nothing at all, and the slot is
    /// spent by the restore, by the send, and by a new chat, so nothing the
    /// human has let go of comes back on a keystroke later.
    lost: Option<Lost>,
    /// Where each conversation's pane is reading from, keyed by the agent whose
    /// transcript it shows. Per conversation because the position is the
    /// human's *reading of one pane*: news about another agent must not move
    /// it, and scrolling one pane must not carry its offset into another's
    /// (finding U3). A pane nobody has scrolled is absent, which is exactly
    /// `Reading::Following` — following costs no state at all.
    reading: HashMap<AgentId, Reading>,
    /// The select mode: the cursor `Ctrl-Y` put over a pane's transcript, or
    /// `None` when the keys are the box's again. One mode at a time, over one
    /// conversation — a pane showing another agent paints no cursor and its
    /// keys are the box's.
    ///
    /// A cursor into a transcript that no longer exists is not a cursor, so
    /// `replace_transcript` drops the mode when it is over the conversation it
    /// replaces, as `clear` does for a new chat. The paint road clamps the
    /// cursor the way the key road does, so a mode left pointing past the rows
    /// by any other road paints the pane rather than taking the frame down
    /// with it.
    select: Option<Selecting>,
    /// The user lines that are *not* the human's, keyed by the index they sit at
    /// in their conversation. Absent is the norm — most of a transcript is the
    /// human's own words — so the default costs nothing and there is no second
    /// copy of the transcript to keep in step with this one. `replace_transcript`
    /// drops an agent's map with it: a restored transcript arrives without its
    /// provenance, and the pane then reads what it can from the lines themselves
    /// ([`unrecorded`]).
    spoken: HashMap<AgentId, HashMap<usize, Voice>>,
    /// Each agent's transcript revision: a monotone counter of the changes the
    /// UI's copy of that agent has taken — one per appended line, one per
    /// wholesale replacement, and one per draft an attach client set. `read`
    /// returns it and `edit` compares it, so an edit written against a
    /// transcript (or draft) that has moved on is refused, not guessed at
    /// (M3). It is not the line count: a fold replaces a long transcript with
    /// a short one, and a revision that stepped back would let a stale edit
    /// land.
    revisions: HashMap<AgentId, u64>,
    /// The words the human just sent, waiting for their echo.
    ///
    /// The box is the human's voice and `take_input` is the send; `app::mod`
    /// hands exactly those words back through [`Self::push_message`] on the same
    /// turn, with no event able to arrive in between. So the first user line that
    /// matches them is the human's, and a user line that does not is somebody
    /// else's — the fact that tells a parent's steering apart from the human's
    /// nudge, which is the only thing two such lines differ by.
    pending: Option<String>,
    /// Whether the model's own reasoning is painted above the turn it decided.
    ///
    /// A *view*, shown by default: the reasoning is already stored with the
    /// turn in the session file and replayed to the endpoint (a thinking
    /// endpoint refuses a replayed turn without it), so hiding it costs the
    /// conversation nothing — which is why `Ctrl-T` writes no notice and
    /// touches no stored line. It lives here, beside the transcript it hides,
    /// rather than in `App`, because every pane paints through this one
    /// transcript and the choice is about the reading, not about the frame.
    reasoning: bool,
    /// How much of each kind of block this conversation's panes paint: the one
    /// [`Fold`] behind every pane, so two panes cannot fold the same kind to
    /// two numbers — and so a view that sets one has one place to set it.
    fold: Fold,
}

impl Chat {
    pub fn new(system: Message, root: Vec<Message>) -> Self {
        Self {
            system,
            systems: HashMap::new(),
            root,
            agents: HashMap::new(),
            notices: Vec::new(),
            input: Input::default(),
            attachments: Vec::new(),
            lost: None,
            reading: HashMap::new(),
            select: None,
            spoken: HashMap::new(),
            revisions: HashMap::new(),
            pending: None,
            reasoning: true,
            fold: Fold::DEFAULT,
        }
    }

    /// Whether a pane paints the model's reasoning above the turn it decided.
    pub fn shows_reasoning(&self) -> bool {
        self.reasoning
    }

    /// `Ctrl-T`: show or hide the model's reasoning. A view, so it is not a
    /// change to the conversation and not a thing to say. `clear` deliberately
    /// leaves it alone: the human's choice outlives the chat it was made in.
    pub fn set_reasoning(&mut self, on: bool) {
        self.reasoning = on;
    }

    /// The revision of the UI's copy of `agent`'s transcript. A conversation
    /// nobody has changed reads as its own line count, so the first `read` of
    /// a restored one still hands back a token that means something.
    pub fn revision(&self, agent: AgentId) -> u64 {
        self.revisions
            .get(&agent)
            .copied()
            .unwrap_or_else(|| self.transcript(agent).len() as u64)
    }

    /// Advance a revision past `prior`, and never below the number of lines the
    /// copy now holds: monotone across a fold that shrinks the transcript, and
    /// clear about having moved even when the count did not (M3).
    fn advance(&mut self, agent: AgentId, prior: u64) {
        let lines = self.transcript(agent).len() as u64;
        self.revisions.insert(agent, (prior + 1).max(lines));
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

    /// `id`'s actor says what its own history opens with: the prompt it was
    /// built with, and the one every request it sends starts from.
    ///
    /// One writer, one reader: the actor emits it ([`AgentEvent::SystemPrompt`]
    /// when its thread is built) and [`Self::used_weight_for`] weighs it. It is
    /// not stored in the session — a prompt names a workspace that may have
    /// moved, so `session_snapshot` leaves a child's system message out of what
    /// it stores and the actor builds a fresh one on the way back in
    /// (`agent::revive`).
    pub fn learn_system(&mut self, id: AgentId, prompt: Message) {
        self.systems.insert(id, prompt);
    }

    /// The system prompt `id`'s history opens with: the root's own, or the one
    /// the agent's actor published. `None` is "no actor has said yet" — the
    /// root never has to (its prompt is the conversation's) and a child's actor
    /// says it before its first run.
    fn system_for(&self, id: AgentId) -> Option<&Message> {
        if id == AgentId::ROOT {
            Some(&self.system)
        } else {
            self.systems.get(&id)
        }
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
    ///
    /// This is the one place a line's *voice* is decided while it is known: a
    /// user line that is the echo of the words the box just sent is the human's,
    /// and every other user line was written by another agent or by mush (see
    /// [`unrecorded`]).
    pub fn push_message(&mut self, agent: AgentId, message: Message) {
        let prior = self.revision(agent);
        if message.role == "user" {
            let index = self.transcript(agent).len();
            let voice = match self.pending.take() {
                Some(words) if words == message.text().trim() => Voice::Human,
                _ => elsewhere(agent, index, &message),
            };
            if voice != Voice::Human {
                self.spoken.entry(agent).or_default().insert(index, voice);
            }
        }
        if agent == AgentId::ROOT {
            self.root.push(message);
        } else {
            self.agents.entry(agent).or_default().push(message);
        }
        self.advance(agent, prior);
    }

    /// Replace an agent's transcript: the root's is compacted to
    /// `[system, user(summary)]`, a restored one arrives whole.
    ///
    /// The voices this conversation knew go with it: the indices they were keyed
    /// by describe the transcript that is gone, and a stale one would paint
    /// somebody else's line in the wrong voice. What a restored transcript still
    /// says for itself is read back at paint time.
    ///
    /// The select mode goes with it when it is over this conversation: a cursor
    /// into a transcript that no longer exists is not a cursor, and a frame
    /// asked to paint one would index a row the replacement took away. This is
    /// the one road that replaces a transcript, so this is where the mode is
    /// dropped — clamped at paint time as well, for every road that cannot know
    /// it took rows away ([`Chat::painted`]).
    pub fn replace_transcript(&mut self, agent: AgentId, messages: Vec<Message>) {
        let prior = self.revision(agent);
        self.spoken.remove(&agent);
        self.pending = None;
        if self
            .select
            .as_ref()
            .is_some_and(|select| select.agent == agent)
        {
            self.select = None;
        }
        if agent == AgentId::ROOT {
            self.root = messages;
        } else {
            self.agents.insert(agent, messages);
        }
        self.advance(agent, prior);
    }

    /// Who said the line at `index` of `agent`'s transcript, if a speaker is what
    /// it has. A user line with nothing recorded about it is the human's: that is
    /// what most of a transcript is, and the alternative — recusing the pane from
    /// its own conversation after a restart — is the louder lie.
    fn voice_at(&self, agent: AgentId, index: usize, message: &Message) -> Option<Voice> {
        if message.role != "user" {
            return None;
        }
        Some(
            self.spoken
                .get(&agent)
                .and_then(|voices| voices.get(&index))
                .copied()
                .unwrap_or_else(|| unrecorded(agent, index, message)),
        )
    }

    /// How big one conversation is, in tokens, roughly — the same
    /// three-bytes-per-token heuristic the trimmer uses.
    ///
    /// Derived on read, per agent, and never counted beside the transcript:
    /// there is no push site left to forget, and the human's own words weigh as
    /// soon as they are in the transcript they are in (finding B8). The meter
    /// itself reads the bytes ([`Self::used_weight_for`], against the history
    /// budget, whose boundary is one byte); this is the same sum in the unit a
    /// window is stated in, for a reader that wants that number on its own.
    #[cfg(test)]
    pub fn used_tokens_for(&self, id: AgentId) -> usize {
        self.used_weight_for(id) / mush_core::config::BYTES_PER_TOKEN
    }

    /// The same sum in the budget's own currency: one agent's **own** system
    /// prompt plus its transcript, weighed the one way
    /// [`mush_core::transcript::trim_history`] weighs them. Split from the
    /// token spelling of the same sum so a caller that needs the number in
    /// bytes — the attach gate, asking how much room a picture has left, and
    /// the meter, comparing against the byte boundary every decision uses —
    /// reads the one sum instead of adding the parts up again (two spellings
    /// of one arithmetic is how the budget and the meter drift apart).
    ///
    /// The prompt is the agent's own: the root's is the conversation's
    /// ([`Self::system`], which the root actor is handed with every run), and a
    /// child's is the one its actor published ([`Self::learn_system`]) — a
    /// child's prompt names the child's own workspace, so only the actor that
    /// built it can say what it weighs. An agent whose prompt has not been
    /// published, or that has no transcript at all, weighs nothing.
    ///
    /// The pane's copy can be heavier than the actor's list, and that is
    /// deliberate: a trim drops turns the pane keeps — the pane is the human's
    /// record of the conversation, and the dropped-turns note is in both — and
    /// a shed replaces a result in the actor's list while the pane still shows
    /// what the tool produced. The difference is one-directional: the actor's
    /// list is never the heavier of the two, so the room the attach gate
    /// computes is never larger than the room the next request has.
    pub fn used_weight_for(&self, id: AgentId) -> usize {
        let transcript = if id == AgentId::ROOT {
            &self.root
        } else {
            match self.agents.get(&id) {
                Some(messages) => messages,
                None => return 0,
            }
        };
        let prompt = self.system_for(id).map_or(0, Message::weight);
        prompt.saturating_add(
            transcript
                .iter()
                .map(Message::weight)
                .fold(0, usize::saturating_add),
        )
    }

    /// How many lines [`Self::clear`] would drop: the root transcript and every
    /// child's, because a new chat empties all of them.
    ///
    /// What the armed `Ctrl-N`'s warning counts, derived on read so the line
    /// cannot disagree with the chat it is about. Notices are not lines: a
    /// failure is the workspace's, not the conversation's, and a count of what
    /// the copy keeps must not claim one.
    pub fn lines_to_drop(&self) -> usize {
        self.root.len() + self.agents.values().map(Vec::len).sum::<usize>()
    }

    /// Ctrl-N: the conversation is gone, the box and the scrollback with it.
    ///
    /// The reasoning toggle is *not* reset: it is a view the human chose, not a
    /// fact about the conversation, and a new chat they cannot read the way
    /// they just asked for is a preference the UI forgot.
    pub fn clear(&mut self) {
        self.root.clear();
        self.agents.clear();
        self.systems.clear();
        self.notices.clear();
        self.reading.clear();
        // The mode is a cursor over a transcript that no longer exists; the
        // mode that survives Ctrl-N would be a cursor over nothing.
        self.select = None;
        self.spoken.clear();
        self.pending = None;
        // The road back goes with the conversation the loss was in: a Ctrl-N
        // that handed a keystroke a draft from the chat that just went would be
        // the same surprise as a sent message coming back.
        self.lost = None;
        // Every counter steps forward rather than resetting to nothing: a
        // client that read a revision before Ctrl-N must not see the same
        // number come back for a different conversation, where its next edit
        // would be accepted as if the transcript had stood still (finding A1).
        // The root is always bumped — its revision is the one a client holds
        // first — and so is every conversation the tree knew, whether or not a
        // change had put an entry in the map (`revision`'s fallback is the line
        // count, which is a token like any other).
        let mut known: Vec<AgentId> = vec![AgentId::ROOT];
        known.extend(self.agents.keys().copied());
        known.extend(self.revisions.keys().copied());
        known.sort_unstable();
        known.dedup();
        for id in known {
            let prior = self.revision(id);
            self.advance(id, prior);
        }
    }

    /// Forget one agent's conversation: the child the history window reaped
    /// (`App::reap_history`), whose node, transcript and stored row all go
    /// together.
    ///
    /// **No archive** (§8.21): the transcript is dropped, not written anywhere,
    /// because an archive is one more lifetime to reason about and the file is
    /// already bounded without it: a conversation is folded at nine tenths of
    /// its history budget, a *finished* child's is never folded again (it sits
    /// frozen at whatever it reached), and the file's bound is `CHILD_HISTORY ×
    /// that fold trigger + the root`. So dropping the row drops one child's
    /// frozen transcript from the next save.
    ///
    /// Every map in here is keyed by the agent's id, so every one of them goes
    /// with it: the transcript itself, the voices keyed by line index, the
    /// revision an attach client edits against, and the pane's reading
    /// position. The notices go too — they are tagged with the agent they were
    /// written about, and only failures are written to the session, so leaving
    /// them behind would keep the file growing with `!` lines about an agent
    /// nothing can open (`Chat::stored_notices`).
    ///
    /// The revision is *dropped* rather than stepped forward the way
    /// [`Self::clear`] steps it, and that is safe only because the id is spent:
    /// a reaped node's number is one `Ids::next_agent` has already passed, so
    /// the `0` a missing entry reads as can never be a token for a live
    /// conversation — and both attach doors refuse an agent the tree does not
    /// have (`App::attach_read`/`attach_edit`) before they compare a revision
    /// at all. Never the root: the window keeps it, and the pane the human
    /// reads is not this method's to empty.
    pub fn forget(&mut self, agent: AgentId) {
        self.agents.remove(&agent);
        self.systems.remove(&agent);
        self.spoken.remove(&agent);
        self.revisions.remove(&agent);
        self.reading.remove(&agent);
        self.notices.retain(|notice| notice.agent != agent);
    }

    /// A line for the transcript that is not a message: a hint, or a failure.
    /// It concerns the root conversation unless tagged otherwise.
    ///
    /// Test-only now: a line that answers a command goes to the pane the human
    /// is looking at (`/help`'s own arm), so production callers name their
    /// agent through [`Self::note_for`].
    #[cfg(test)]
    pub fn note(&mut self, text: impl Into<String>) {
        self.note_for(AgentId::ROOT, text);
    }

    /// An information line — what a command just did, what a run just hit.
    ///
    /// It belongs to the moment it answers, so it is *chatter*: the agent's next
    /// run ends it ([`Self::clear_notes_for`]), the human's next send ends it
    /// ([`Self::dismiss_said`]), and `SAID_TTL` ends it if neither happens.
    /// Nothing about it is written to the session, because a restart has no
    /// moment to answer. A failure is the opposite and is not written here —
    /// see [`Self::note_error_for`].
    pub fn note_for(&mut self, agent: AgentId, text: impl Into<String>) {
        self.push_notice(agent, NoticeKind::Info, text);
    }

    /// A failure in the root conversation. Test-only now: every production
    /// failure is tagged with the agent it concerns, so the root's goes through
    /// [`Self::note_error_for`].
    #[cfg(test)]
    pub fn note_error(&mut self, text: impl Into<String>) {
        self.note_error_for(AgentId::ROOT, text);
    }

    /// A run failed. This is the line that outlives the run: it is the agent's
    /// own record of its last failure, so a new one replaces the old rather
    /// than piling up beside it — two failures for one agent would disagree
    /// about which is current, and the pane would print both.
    ///
    /// A run the loop guard stopped arrives here too, because that is the shape
    /// an ended run has on the wire. It is not a failure — mush stopped it, and
    /// nothing the model did broke — so it lands as a stop, and it takes the
    /// notice the guard wrote about the same event with it: one event, one line,
    /// and the line that survives is the one that says what happened to the run.
    pub fn note_error_for(&mut self, agent: AgentId, text: impl Into<String>) {
        let text = text.into();
        let stopped = text.starts_with(LOOP_STOP);
        let kind = if stopped {
            NoticeKind::Stopped
        } else {
            NoticeKind::Error
        };
        self.notices.retain(|notice| {
            if notice.agent != agent {
                return true;
            }
            // One failure per agent, and one stop: the newest.
            if notice.kind != NoticeKind::Info {
                return false;
            }
            !(stopped && notice.text.starts_with(LOOP_NOTICE))
        });
        self.push_notice(agent, kind, text);
    }

    /// A run that never ended, said where the human reads it: the agent's row
    /// wears `⚠`, and this is the line under it that says what that means.
    ///
    /// It is the counterpart of [`Self::note_error_for`] for the one ending that
    /// has no event of its own — nothing reported it, because the thing that
    /// would have reported it is the thing that vanished (finding H2). Only the
    /// newest line about an agent is current, so this replaces an earlier
    /// failure, stop or cut-off exactly as a new failure would.
    pub fn note_cut_off_for(&mut self, agent: AgentId, text: impl Into<String>) {
        self.notices
            .retain(|notice| notice.agent != agent || notice.kind == NoticeKind::Info);
        self.push_notice(agent, NoticeKind::CutOff, text);
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

    /// End every chatter line, wherever it is: the human just did the next
    /// thing, and a line that answered the thing before it is over.
    ///
    /// This is the other half of the lifetime a command's answer has. Its agent
    /// may never run again — a `/help` read once, a diff of work already merged
    /// by hand — and until this existed the line sat in the foot until Ctrl-N,
    /// spending the transcript's rows on a moment nobody was in any more
    /// (finding U8). Every agent's chatter goes, not only the focused one's:
    /// the moment ended for the human, and which pane happened to show the line
    /// is not what makes it stale.
    ///
    /// Failures and stops are deliberately untouched: they are the run's record
    /// of itself, and they age out on their own terms.
    pub fn dismiss_said(&mut self) -> bool {
        let before = self.notices.len();
        self.notices.retain(|notice| !notice.is_chatter());
        self.notices.len() != before
    }

    /// End every chatter line that has outlived `SAID_TTL`, as of `now`.
    ///
    /// The clock is the fallback for a human who does nothing at all: `/help`
    /// read once and then left on a screen for an hour was the finding. Called
    /// from the tick, the same place the bar's own `Info` line expires, so both
    /// of mush's transient lines have one home for their lifetime.
    pub fn expire_said(&mut self, now: u64) -> bool {
        let before = self.notices.len();
        self.notices
            .retain(|notice| !(notice.is_chatter() && now.saturating_sub(notice.at) >= SAID_TTL));
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
                // A restored failure is one line, whatever it said before it was
                // written: the count is a fact about this process's turns, and
                // the file has no turns to count.
                count: 1,
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
    ///
    /// The list also carries where the newest note *begins*, because a long
    /// note wraps into many rows and the row that says when it happened and how
    /// it started is the head: a reader dropped at the bottom lands mid-sentence
    /// with the age above the fold (finding T7). See [`Notes::newest`].
    pub fn notes_report(&self, agent: AgentId, now: u64, width: usize) -> Notes {
        let mut rows = Vec::new();
        let mut newest = 0;
        for notice in self.notices_for(agent) {
            // The head of the newest note is the last one this loop starts: a
            // later notice, if there is one, moves it down. A note that wraps
            // to no rows at all (an empty text another version stored) leaves
            // the head where it was rather than pointing past the list.
            let head = rows.len();
            // The glyph has one home, beside the kind it marks: this list and
            // the pane's foot paint the same mark for the same kind of line.
            let (mark, _) = notice.kind.mark();
            let age = short_age(Duration::from_secs(now.saturating_sub(notice.at)));
            let lead = format!("{age} {mark}");
            // The lead is part of the first row, so the text is wrapped *inside*
            // what the lead leaves — a continuation row carries the same indent.
            // Wrapping at `width` and then prepending the lead made the very
            // first row `lead` wider than the popup, which is exactly the row
            // that was clipped even on an 80-column popup. Like every other
            // width on this screen, the lead is measured in columns.
            let lead_width = UnicodeWidthStr::width(lead.as_str());
            for (index, line) in wrap_text(&notice.line(), width.saturating_sub(lead_width))
                .into_iter()
                .enumerate()
            {
                if index == 0 {
                    rows.push(format!("{lead}{line}"));
                } else {
                    rows.push(format!("{}{line}", " ".repeat(lead_width)));
                }
            }
            if rows.len() > head {
                newest = head;
            }
        }
        Notes { rows, newest }
    }

    fn push_notice(&mut self, agent: AgentId, kind: NoticeKind, text: impl Into<String>) {
        let text = text.into();
        // One line said twice in a row is one line: an empty reply on five
        // consecutive turns is one fact about the run, and printing it five
        // times spent the foot — the rows the transcript was supposed to have —
        // on one sentence (finding U8). The newest stamp and the count keep the
        // collapsed line honest about how often it happened; a different line
        // in between starts a new one, because then it *is* two moments.
        if let Some(last) = self
            .notices
            .iter_mut()
            .rev()
            .find(|notice| notice.agent == agent)
        {
            if last.kind == kind && last.text == text {
                last.count += 1;
                last.at = session::now_secs();
                return;
            }
        }
        self.notices.push(Notice {
            agent,
            kind,
            at: session::now_secs(),
            count: 1,
            text,
        });
    }

    /// Backdate every line by `seconds`, so a test can reach the far side of
    /// `SAID_TTL` without sleeping. Test-only: nothing in production rewrites a
    /// stamp, and the age a restored failure reports is the age the file says.
    #[cfg(test)]
    pub fn age_notices(&mut self, seconds: u64) {
        for notice in &mut self.notices {
            notice.at = notice.at.saturating_sub(seconds);
        }
    }

    /// Whether the select mode is on, in whichever pane it was started in.
    ///
    /// The keymap's question: while it is on the mode takes the keyboard from
    /// both panes, the way a picker does (finding B2's one home for that
    /// decision).
    pub fn selecting(&self) -> bool {
        self.select.is_some()
    }

    /// Leave the select mode without copying anything. The other road out is
    /// `Esc` (which every caller can reach through [`Chat::select_apply`]), and
    /// this one is for a key that is not the mode's: `Tab` moves the focus, and
    /// a mode that kept the keyboard after the human moved on would be the one
    /// modal mush could not get out of with `Tab`.
    pub fn cancel_select(&mut self) {
        self.select = None;
    }

    /// `Ctrl-Y`: start selecting in the pane `on` shows, with the cursor on the
    /// newest source line — where the pane already is, because it follows the
    /// bottom.
    ///
    /// `Some(line)` is what the bar says when there is nothing to stand on: a
    /// pane with no words yet has no source line, and a mode whose cursor has
    /// nowhere to be is a mode the human cannot copy their way out of.
    pub fn start_select(&mut self, on: AgentId) -> Option<String> {
        match self.last_line(on) {
            Some(cursor) => {
                self.select = Some(Selecting {
                    agent: on,
                    cursor,
                    anchor: None,
                    // Placed by the first frame, which is what knows the
                    // pane's measure; anywhere above the cursor is the same
                    // answer here.
                    top: Cell::new((0, 0)),
                });
                None
            }
            None => Some("nothing to select in this pane".to_string()),
        }
    }

    /// The newest source line the pane paints, if it paints one.
    fn last_line(&self, on: AgentId) -> Option<(usize, usize)> {
        let transcript = self.transcript(on);
        (0..transcript.len())
            .rev()
            .find_map(|index| lines_of(&transcript[index]).map(|lines| (index, lines.len() - 1)))
    }

    /// The oldest source line the pane paints, if it paints one.
    fn first_line(&self, on: AgentId) -> Option<(usize, usize)> {
        let transcript = self.transcript(on);
        (0..transcript.len()).find_map(|index| lines_of(&transcript[index]).map(|_| (index, 0)))
    }

    /// What the select mode's keys do — the one place they run.
    ///
    /// `Some(copied)` is `Enter`: the copy the caller hands the clipboard, and
    /// the mode left behind with it.
    pub fn select_apply(&mut self, on: AgentId, key: SelectKey) -> Option<Copied> {
        let Some(cursor) = self.clamped_cursor(on) else {
            // The transcript under the mode has no line left to stand on.
            // `replace_transcript` drops the mode with the rows it replaces, so
            // this is the guard for every other road — an agent reaped out from
            // under the pane, or a state a test built — and leaving is the only
            // honest answer.
            self.select = None;
            return None;
        };
        self.set_cursor(cursor);
        match key {
            // The key that opens the mode is [`Chat::start_select`]'s: the
            // caller never routes it here, and one that arrives anyway has
            // nothing to do — the cursor is already where the mode was started.
            SelectKey::Start => None,
            SelectKey::Cancel => {
                self.select = None;
                None
            }
            SelectKey::Copy => {
                let copied = self.copy(on, cursor);
                self.select = None;
                Some(copied)
            }
            SelectKey::Move(step) => {
                let next = self.step_line(on, cursor, step);
                self.set_cursor(next);
                None
            }
            SelectKey::Extend(step) => {
                let next = self.step_line(on, cursor, step);
                if let Some(select) = self.select.as_mut() {
                    // The anchor is where the selection started: the first
                    // extended step plants it on the line the cursor was on,
                    // and every one after keeps it. A plain move never drops
                    // it, so the selection follows the cursor's end.
                    if select.anchor.is_none() {
                        select.anchor = Some(cursor);
                    }
                    select.cursor = next;
                }
                None
            }
            SelectKey::First => {
                if let Some(first) = self.first_line(on) {
                    self.set_cursor(first);
                }
                None
            }
            SelectKey::Last => {
                if let Some(last) = self.last_line(on) {
                    self.set_cursor(last);
                }
                None
            }
        }
    }

    fn set_cursor(&mut self, cursor: (usize, usize)) {
        if let Some(select) = self.select.as_mut() {
            select.cursor = cursor;
        }
    }

    /// The cursor as the transcript is *now*: a transcript can shrink under a
    /// state that still points into it — a reaped conversation, or a state a
    /// caller built — and neither a key nor a frame may index past the end. The
    /// nearest line that still exists is the honest clamp — and `None` when
    /// there is no source line left at all, which drops the mode rather than
    /// leaving a cursor over nothing.
    fn clamped_cursor(&self, on: AgentId) -> Option<(usize, usize)> {
        let select = self.select.as_ref().filter(|select| select.agent == on)?;
        let transcript = self.transcript(on);
        let mut index = select.cursor.0.min(transcript.len().checked_sub(1)?);
        loop {
            if let Some(lines) = lines_of(&transcript[index]) {
                return Some((index, select.cursor.1.min(lines.len() - 1)));
            }
            index = index.checked_sub(1)?;
        }
    }

    /// The source line one step older or newer than `cursor`, or `None` at an
    /// end of the transcript.
    fn adjacent(
        &self,
        on: AgentId,
        cursor: (usize, usize),
        forward: bool,
    ) -> Option<(usize, usize)> {
        let transcript = self.transcript(on);
        let lines = lines_of(transcript.get(cursor.0)?)?;
        if forward {
            if cursor.1 + 1 < lines.len() {
                return Some((cursor.0, cursor.1 + 1));
            }
            ((cursor.0 + 1)..transcript.len())
                .find_map(|index| lines_of(&transcript[index]).map(|_| (index, 0)))
        } else {
            if cursor.1 > 0 && cursor.1 < lines.len() {
                return Some((cursor.0, cursor.1 - 1));
            }
            (0..cursor.0).rev().find_map(|index| {
                lines_of(&transcript[index]).map(|lines| (index, lines.len() - 1))
            })
        }
    }

    /// The cursor moved `step` source lines, positive toward the newest,
    /// stopping at either end: the oldest and the newest lines are ends of the
    /// transcript, not walls to crash into.
    fn step_line(&self, on: AgentId, cursor: (usize, usize), step: i64) -> (usize, usize) {
        let mut cursor = cursor;
        let forward = step > 0;
        for _ in 0..step.unsigned_abs() {
            match self.adjacent(on, cursor, forward) {
                Some(next) => cursor = next,
                None => break,
            }
        }
        cursor
    }

    /// `Enter`: the transcript's own text for the selection, or for the cursor's
    /// own line when there is no selection, plus the line mush says once the
    /// clipboard has taken it.
    ///
    /// The text is the *source lines* joined with `\n` — the separator the
    /// transcript has between them — so a whole message is `Message::text()`
    /// byte for byte, a soft wrap at this pane's width is not a newline, and a
    /// tab is a tab. A folded block — a tool result, a report, a brief — is
    /// copied whole even where the pane's cap hides its tail.
    fn copy(&self, on: AgentId, cursor: (usize, usize)) -> Copied {
        let select = self.select.as_ref().expect("the mode is on");
        let (from, to) = match select.anchor {
            Some(anchor) if anchor <= cursor => (anchor, cursor),
            Some(anchor) => (cursor, anchor),
            None => (cursor, cursor),
        };
        let transcript = self.transcript(on);
        let mut parts: Vec<&str> = Vec::new();
        let mut covered: Vec<usize> = Vec::new();
        for index in from.0..=to.0 {
            let Some(lines) = transcript.get(index).and_then(lines_of) else {
                continue;
            };
            let first = if index == from.0 {
                from.1.min(lines.len() - 1)
            } else {
                0
            };
            let last = if index == to.0 {
                to.1.min(lines.len() - 1)
            } else {
                lines.len() - 1
            };
            if first > last {
                continue;
            }
            parts.extend_from_slice(&lines[first..=last]);
            covered.push(index);
        }
        let text = parts.join("\n");
        let what = match covered.as_slice() {
            [one] => match transcript[*one].role.as_str() {
                "user" => "your message".to_string(),
                "tool" => format!("{on}'s tool result"),
                _ => format!("{on}'s reply"),
            },
            many => format!("{} messages", many.len()),
        };
        let count = parts.len();
        let noun = if count == 1 { "line" } else { "lines" };
        let line = format!(
            "copied {count} {noun} from {what} — {} bytes",
            grouped(text.len())
        );
        Copied { text, line }
    }

    /// Where the pane showing `agent` is reading from. A conversation nobody
    /// has scrolled is at the bottom.
    fn reading(&self, agent: AgentId) -> Reading {
        self.reading
            .get(&agent)
            .copied()
            .unwrap_or(Reading::Following)
    }

    /// The human moved the pane showing `agent` by `rows`: positive is older,
    /// and the pane the key acts on is the conversation it shows (finding U3).
    ///
    /// Scrolling away from the bottom *holds* the window the pane now shows, so
    /// the lines that arrive while the human reads are not what the pane
    /// follows. Scrolling back down rejoins the newest line: the rows that
    /// arrived meanwhile are one step further down, and the step that leaves the
    /// held window is the step that takes them.
    pub fn scroll_by(&mut self, agent: AgentId, rows: i64) {
        let messages = self.transcript(agent).len();
        // Nothing said yet: there is no window to hold, and an empty pane stays
        // a pane at the bottom.
        if messages == 0 {
            return;
        }
        let held = self.reading(agent).held(messages);
        let next = match (held, rows) {
            // Already at the bottom, and asked for something below it: there is
            // nothing under the bottom, and the pane still follows.
            (None, rows) if rows <= 0 => return,
            (None, rows) => Reading::Holding {
                offset: rows as usize,
                up_to: messages,
            },
            (Some((offset, up_to)), rows) => {
                // Past the bottom of what the pane was holding: the rows that
                // arrived in the meantime are one step further down, so the step
                // that leaves the held window is the step that rejoins the
                // newest line.
                let moved = (offset as i64 + rows).max(0) as usize;
                if moved == 0 {
                    Reading::Following
                } else {
                    Reading::Holding {
                        offset: moved,
                        up_to,
                    }
                }
            }
        };
        self.reading.insert(agent, next);
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
        let transcript = self.transcript(pane.agent);
        let protected = usize::from(!transcript.is_empty());
        let room = FOOT_ROWS.min(height.saturating_sub(protected));
        let foot = self.foot(pane, width, room);
        // The window: the select mode's own while it is on this pane, the
        // human's reading otherwise. The mode counts as on only while the
        // transcript still has a line under its cursor: `clamped_cursor` is the
        // key road's clamp, and the frame takes the same one because no frame
        // may index a row that is not there. `None` here — another pane's mode,
        // no mode, or a cursor whose rows a replacement took away — paints the
        // ordinary body, with no cursor and no `Enter copies` clause promising
        // one, which is what keeps a stale state from taking the process down
        // with it (D1).
        let select = self
            .select
            .as_ref()
            .filter(|select| select.agent == pane.agent);
        let cursor = self.clamped_cursor(pane.agent);
        let mode = select.zip(cursor);
        let (mut body, cut) = match mode {
            Some((select, cursor)) => self.select_body(
                select,
                cursor,
                pane.agent,
                width,
                height.saturating_sub(foot.lines.len()),
            ),
            None => (
                self.body(pane, width, height.saturating_sub(foot.lines.len())),
                None,
            ),
        };
        // Read before the foot is appended: the mode's rows are transcript
        // rows, and the foot never carries the cursor.
        let select_rows =
            mode.and_then(|(select, cursor)| select_rows(&body.rows, select, cursor, cut));
        body.lines.extend(foot.lines);
        let lines = body.lines;

        let mut title = if pane.agent == AgentId::ROOT {
            " mush ".to_string()
        } else {
            format!(" agent {} ", pane.agent)
        };
        // The mode's own line, first because it is the newest thing about the
        // pane: `Ctrl-Y` put the cursor here and the keys that finish the job
        // are not the ones the hint under the pane advertises. The pair named
        // is the one that leaves the mode — nothing else on this screen says
        // which of the two copies.
        if mode.is_some() {
            title.push_str("· Enter copies · Esc leaves ");
        }
        // A pane with no row to spare for the foot's own count line is the case
        // the title exists for: wherever the human looks, the pane says how
        // many lines it is hiding — and names the way to read them, because the
        // count row that carries `· /notes` is exactly the row this pane has no
        // room for.
        if foot.hidden > 0 && !foot.counted {
            title.push_str(&format!("· {} · /notes ", more_label(foot.hidden)));
        }
        // A pane that is not at the bottom says so. The foot staying put is
        // what makes it a foot, but a window holding rows above the newest line
        // looks exactly like one following it, and the human who scrolled away
        // is the only one who knows they did (finding T10). The rows are the
        // held window's own offset, and the key named is the chat pane's way
        // back down to the newest line. While the mode is on, this is not the
        // reading the pane shows — the mode has its own window — so the clause
        // would be a lie about the rows on screen.
        if mode.is_none() {
            if let Some((offset, _)) = self.reading(pane.agent).held(transcript.len()) {
                title.push_str(&format!("· scrolled ↑{offset} rows · PgDn "));
            }
        }
        Painted {
            lines,
            title,
            select: select_rows,
        }
    }

    /// The window the select mode's cursor is shown through, placed so the
    /// cursor is on screen.
    ///
    /// The pane's own reading ([`Reading`]) is not touched: the mode is a
    /// reading of its own, and leaving it puts the pane back where the human
    /// was, not where the cursor ended.
    ///
    /// `cursor` is the caller's clamp of the mode's own ([`Chat::clamped_cursor`]),
    /// never `select.cursor`: the window is placed for the line the transcript
    /// still has, so every index the walk makes names a row that is there.
    fn select_body(
        &self,
        select: &Selecting,
        cursor: (usize, usize),
        on: AgentId,
        width: usize,
        height: usize,
    ) -> (Body, Option<usize>) {
        let transcript = self.transcript(on);
        if transcript.is_empty() || height == 0 {
            return (Body::default(), None);
        }
        let start = select.top.get();
        let start = (start.0.min(transcript.len() - 1), start.1);
        let (body, cut) = self.window_from(on, width, height, start);
        if cursor_row(&body.rows, cursor, cut).is_some() {
            return (body, cut);
        }
        // The window the state carries no longer shows the cursor: the terminal
        // was resized, the transcript moved under it, or the cursor's
        // own line is behind a block's fold. Put it where the pane can hold
        // it — the cursor's line at the top when it is above the window, at the
        // bottom when it is below — and leave the placement where the next
        // frame finds it.
        let start = if cursor_above(&body, cursor) {
            self.top_at_cursor(on, width, cursor)
        } else {
            self.top_at_bottom(on, width, height, cursor)
        };
        select.top.set(start);
        self.window_from(on, width, height, start)
    }

    /// The transcript's rows in a window: from `start` — a message, and how many
    /// rows of that message's own chunk to skip — forward, until `height` rows
    /// are filled or the transcript ends.
    ///
    /// The message the height cut is reported, so a caller can tell a window
    /// that stopped at the pane's bottom from one that stopped at the
    /// transcript's end.
    fn window_from(
        &self,
        on: AgentId,
        width: usize,
        height: usize,
        start: (usize, usize),
    ) -> (Body, Option<usize>) {
        let transcript = self.transcript(on);
        let mut body = Body::default();
        let mut cut = None;
        let mut index = start.0;
        let mut skip = start.1;
        while body.lines.len() < height && index < transcript.len() {
            let chunk = self.chunk(on, index, width);
            let room = height - body.lines.len();
            if chunk.lines.len().saturating_sub(skip) > room && skip + room < last_text(&chunk) {
                cut = Some(index);
            }
            chunk.take_into(&mut body, skip, room);
            skip = 0;
            index += 1;
        }
        // A window that reached the transcript's own end trims the blank that
        // closes it, exactly as the following view does (finding B4); a window
        // the pane's height cut has no closing blank to trim.
        if cut.is_none() {
            body.trim_trailing_blanks();
        }
        (body, cut)
    }

    /// One message's rows, with the message and source line each came from.
    fn chunk(&self, on: AgentId, index: usize, width: usize) -> Chunk {
        let message = &self.transcript(on)[index];
        let voice = self.voice_at(on, index, message);
        let mut lines = Vec::new();
        let rows = render_message(&mut lines, message, voice, width, self.reasoning, self.fold);
        debug_assert_eq!(lines.len(), rows.len(), "one map entry per painted row");
        Chunk {
            lines,
            rows: rows
                .into_iter()
                .map(|line| line.map(|line| (index, line)))
                .collect(),
        }
    }

    /// The window's top with the cursor's own line as its first row: where the
    /// line begins in its message. A line the fold hid begins at the `…`, which
    /// is the row that stands for it.
    fn top_at_cursor(&self, on: AgentId, width: usize, cursor: (usize, usize)) -> (usize, usize) {
        let chunk = self.chunk(on, cursor.0, width);
        (
            cursor.0,
            first_row(&chunk.rows, cursor.0, cursor.1).unwrap_or(0),
        )
    }

    /// The window's top with the cursor's own line as the pane's last row: the
    /// rows above it, walked back until the pane is full.
    ///
    /// When the transcript's own beginning is closer than the pane's top there
    /// is nothing above the start to fill a pane with, and the window is the
    /// transcript's first rows instead — a half-empty pane under the cursor
    /// would read as a gap in the conversation that is not there.
    fn top_at_bottom(
        &self,
        on: AgentId,
        width: usize,
        height: usize,
        cursor: (usize, usize),
    ) -> (usize, usize) {
        let want = height.saturating_sub(1);
        let head = self.chunk(on, cursor.0, width);
        // The cursor's line's last row is the window's last row, so the rows
        // above the window are everything before it.
        let mut above = last_row(&head.rows, cursor.0, cursor.1).unwrap_or(0);
        let mut blocks: Vec<(usize, Chunk)> = vec![(cursor.0, head.cut(above))];
        let mut index = cursor.0;
        while above < want && index > 0 {
            index -= 1;
            let chunk = self.chunk(on, index, width);
            above += chunk.lines.len();
            blocks.push((index, chunk));
        }
        if above < want {
            return (0, 0);
        }
        let mut skip = above - want;
        for (index, chunk) in blocks.iter().rev() {
            if skip < chunk.lines.len() {
                return (*index, skip);
            }
            skip -= chunk.lines.len();
        }
        (0, 0)
    }

    /// The transcript itself, without the foot: the rows the conversation fills
    /// in `height`, newest at the bottom — or, while the human is holding a
    /// window, the rows they are holding, with everything that arrived since
    /// still below them (finding U3).
    fn body(&self, pane: &Pane<'_>, width: usize, height: usize) -> Body {
        let transcript = self.transcript(pane.agent);
        // Which conversation this window is made of, and how far above its
        // bottom it starts. Holding is a fact about the human's reading, so it
        // is read here rather than guessed from the rows.
        let (messages, scroll) = match self.reading(pane.agent).held(transcript.len()) {
            Some((offset, up_to)) => (&transcript[..up_to], offset),
            // The transcript the pane was holding is gone: a fold replaced it,
            // or the session was restored with less. Falling back to the bottom
            // is the only position that still means something.
            None => (transcript, 0),
        };

        // A pane with nothing in it says what it is waiting for rather than
        // being blank.
        if messages.is_empty() && self.notices_for(pane.agent).next().is_none() {
            let hint: Vec<String> = if pane.agent == AgentId::ROOT {
                vec![
                    "Ask for a change — the agent reads and edits this workspace directly."
                        .to_string(),
                    String::new(),
                    pane.label.to_string(),
                    "Tab cycles panes · Enter sends · /help lists commands".to_string(),
                ]
            } else {
                vec![format!(
                    "Agent {} has no messages yet — typing here sends it a nudge.",
                    pane.agent
                )]
            };
            // Wrapped to the pane and windowed to its height, like every other
            // row: returned raw, the hint was cut mid-word on a narrow pane
            // ("the agent reads and") and the lines under it never appeared.
            let lines: Vec<Line<'static>> = hint
                .iter()
                .flat_map(|line| wrap_text(line, width))
                .take(height)
                .map(|line| Line::from(Span::styled(line, dim())))
                .collect();
            return Body {
                rows: vec![None; lines.len()],
                lines,
            };
        }

        // Built back to front and then reversed: each chunk is one message's
        // rows in their own order, and the pane is anchored at the bottom, so
        // the newest line is the one that must be there.
        let want = height + scroll;
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut count = 0usize;

        for index in (0..messages.len()).rev() {
            if count >= want {
                break;
            }
            let chunk = self.chunk(pane.agent, index, width);
            count += chunk.lines.len();
            chunks.push(chunk);
        }

        let mut body = Body::default();
        for chunk in chunks.into_iter().rev() {
            chunk.take_into(&mut body, 0, usize::MAX);
        }
        body.trim_trailing_blanks();

        // Anchor the window at the bottom: the newest `height` rows, with
        // `scroll` rows of older ones above them. Building backwards means the
        // last chunk can overshoot `want`, so the window cannot be assumed to
        // be exactly `want` rows — deriving the start from what was built is
        // the only arithmetic that is right in both cases. Taking `0` when it
        // overshot painted the *oldest* rows of the window, which made a
        // message taller than the pane freeze the view and hide its own end.
        let start = body.lines.len().saturating_sub(height + scroll);
        body.lines.drain(..start);
        body.rows.drain(..start);
        body.lines.truncate(height);
        body.rows.truncate(height);
        body
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
        // lower is less hideable, and whether the block is a note (something
        // `/notes` could read back) or the derived activity line.
        let mut blocks: Vec<Vec<Line<'static>>> = Vec::new();
        let mut worth: Vec<usize> = Vec::new();
        let mut is_note: Vec<bool> = Vec::new();
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
            is_note.push(true);
        }
        if let Some(words) = &pane.words {
            worth.push(1);
            // The foot says what the run is doing, in the words the row beside
            // it already shows ([`Pane::words`], the phase's own derivation) —
            // `thinking.` for a model call, the tool's own label for a call in
            // flight, `waiting on results.` for a run parked in a `wait`, the
            // fold's sentence for a fold — with the beat's dots behind them.
            //
            // Finding U7 ruled that `working` may not claim a model call that
            // is not happening, and its fix was to paint *nothing* over a
            // parked `wait`. That mechanism is superseded: silence made the
            // pane tell a human less than the row beside it, and `waiting on
            // results.` cannot be mistaken for a model call either.
            blocks.push(vec![Line::from(Span::styled(
                dotted(words, pane.spin),
                Style::default().fg(Color::Cyan),
            ))]);
            // Derived from a phase, so not a note: hiding it promises nothing
            // `/notes` can answer, and counting it made a busy agent with no
            // notices at all claim lines that do not exist.
            is_note.push(false);
        }
        if let Some(at) = alert {
            worth.push(0);
            blocks.push(footnote_lines(notices[at], width));
            is_note.push(true);
        }

        let rows: Vec<usize> = blocks.iter().map(Vec::len).collect();
        let total: usize = rows.iter().sum();
        // More lines than a foot may show: one row of what is left is spent on
        // saying so. The transcript keeps the row that arithmetic costs it, and
        // a pane with a single row to spare gets the line itself — the count is
        // worth less to a human than the note it counts, and the title can say
        // it instead.
        let mut budget = if total <= FOOT_NOTE_ROWS {
            total.min(room)
        } else if room >= 2 {
            FOOT_NOTE_ROWS.min(room - 1)
        } else {
            room
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
        // The count is the notes that are not painted, not every foot row: the
        // activity line is derived and always rebuildable, so a count that
        // included it would name lines `/notes` cannot show.
        let hidden: usize = (0..blocks.len())
            .filter(|index| is_note[*index])
            .map(|index| rows[index] - keep[index])
            .sum();
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

    /// Attach one image to the next message. The gate that decides whether an
    /// image may ride at all is [`crate::app::App::attach_image`]'s, not this
    /// one's: the box holds what the human asked for, and the app says what
    /// cannot be carried.
    pub fn attach(&mut self, image: Image) {
        self.attachments.push(image);
    }

    /// The images waiting to be sent, oldest first.
    pub fn attachments(&self) -> &[Image] {
        &self.attachments
    }

    /// Take the attachments out, as a send does. Like [`Self::take_input`], a
    /// send that does not land gives them back
    /// ([`Self::restore_attachments`]).
    pub fn take_attachments(&mut self) -> Vec<Image> {
        std::mem::take(&mut self.attachments)
    }

    /// Put attachments back in the box after a send that did not land: the
    /// human's pictures are not something they should have to paste again.
    pub fn restore_attachments(&mut self, images: Vec<Image>) {
        self.attachments = images;
    }

    /// Take the box's text out, as a send does. The words are remembered until
    /// their echo arrives, so the line that comes back is painted as the human's
    /// ([`Self::push_message`]).
    pub fn take_input(&mut self) -> String {
        let text = self.input.take();
        self.expect_human(&text);
        text
    }

    /// Forget what `Ctrl-Z` would put back, because the draft has been sent: a
    /// message that has left the box must not come back on a keystroke.
    ///
    /// [`crate::app::App::send_message`] calls this once the send has something
    /// to send and nothing to do with the words it took — so Enter on an empty
    /// box, which sends nothing, is not a loss of its own.
    pub fn forget_lost(&mut self) {
        self.lost = None;
    }

    /// Queue the words the human just sent, so the line that echoes them is
    /// painted as theirs — the mark a typed message gets. `take_input` does this
    /// on the typing path; an attach client's `edit send` has no box to take, so
    /// it states the words here through the same one place.
    ///
    /// A message with no words at all — one that is only an attachment — is
    /// still the human's line, so an empty sentence is queued when an image is
    /// attached: without that, the echo would fall to [`elsewhere`] and the
    /// picture would open as `parent › `, somebody else's words.
    pub fn expect_human(&mut self, text: &str) {
        let words = text.trim();
        if !words.is_empty() || !self.attachments.is_empty() {
            self.pending = Some(words.to_string());
        }
    }

    /// Replace the message box with an attach client's draft for `agent`. The
    /// box holds one draft at a time — it is the human's box — and the agent's
    /// revision moves, so a second client that edits from a stale base is
    /// refused rather than silently overwriting what is there (M3).
    pub fn set_draft(&mut self, agent: AgentId, text: &str) {
        self.input.clear();
        self.input.insert(text);
        let prior = self.revision(agent);
        self.advance(agent, prior);
    }

    /// Do one key the keymap handed to the chat: editing the message box, or
    /// scrolling the transcript — `on` is the agent whose pane the chat is
    /// showing, because a scrollback belongs to a conversation (finding U3).
    ///
    /// Which keys those are is [`crate::app::keys`]'s decision, not this one's
    /// — this is only where they happen, so the box cannot have a second,
    /// private key table that drifts from the app's. `<Enter>` never arrives
    /// here: sending is the agents' business, and the keymap asks the pane
    /// that question first.
    ///
    /// The `Some` a few arms return is a line for the bar — the one thing a key
    /// here says to the human rather than to the box ([`cleared_line`]); every
    /// other arm is box state and returns `None`.
    pub fn apply(&mut self, on: AgentId, key: ChatKey) -> Option<String> {
        match key {
            // A new line instead of sending. Only terminals that report the
            // modifier can deliver Shift+Enter (kitty, WezTerm, foot, Ghostty,
            // recent Alacritty); elsewhere it arrives as a plain Enter, which
            // is why Alt+Enter does the same thing and is the reliable one.
            ChatKey::Newline => {
                self.input.insert("\n");
                None
            }
            // Backspace takes the thing immediately before the cursor. The
            // pictures are painted above the words, so at the very start of the
            // box that thing is the newest attachment, and a plain backspace
            // had nothing to delete there while the box held text. Several
            // images go newest first, one per press; anywhere else in the box
            // the key is the text's, as it always was. An empty box is the same
            // rule with no text: the newest picture is what it means.
            ChatKey::Backspace => {
                if self.input.is_at_start() && !self.attachments.is_empty() {
                    self.lost = self.attachments.pop().map(|newest| Lost {
                        images: vec![newest],
                        ..Lost::default()
                    });
                } else {
                    self.input.backspace();
                }
                None
            }
            ChatKey::Delete => {
                self.input.delete_forward();
                None
            }
            ChatKey::Left => {
                self.input.move_left();
                None
            }
            ChatKey::Right => {
                self.input.move_right();
                None
            }
            ChatKey::Home => {
                self.input.move_home();
                None
            }
            ChatKey::End => {
                self.input.move_end();
                None
            }
            ChatKey::Insert(c) => {
                self.input.insert(&c.to_string());
                None
            }
            ChatKey::Scroll(rows) => {
                self.scroll_by(on, rows);
                None
            }
            // Esc empties the box, and everything waiting to be sent with it:
            // what the human asked to clear is the message they were writing,
            // and half of that message left behind would be a picture they
            // thought they had let go of. The draft goes into the `Ctrl-Z` slot
            // on its way out, so this one-key loss has a road back, and Esc
            // with nothing to lose is not a loss: it sets nothing and says
            // nothing.
            ChatKey::Clear => {
                let said = cleared_line(!self.input.is_empty(), self.attachments.len())?;
                self.lost = Some(Lost {
                    words: self.input.take(),
                    images: std::mem::take(&mut self.attachments),
                });
                Some(said)
            }
            // Ctrl-U: readline's `unix-line-discard`, which is the habit a
            // terminal input is allowed to have. It clears the whole draft, not
            // "the cursor's line" — the box soft-wraps, so the line a human
            // sees is not a line the text has. The images stay: they are not
            // what the key is about.
            ChatKey::ClearWords => {
                if !self.input.is_empty() {
                    self.lost = Some(Lost {
                        words: self.input.take(),
                        ..Lost::default()
                    });
                }
                None
            }
            // Ctrl-Z puts back what the box last lost — the words, the images,
            // or both, whichever the loss took ([`Lost`]). The slot is spent by
            // the restore: the key answers the loss that just happened, not a
            // history of them.
            ChatKey::Undo => {
                if let Some(lost) = self.lost.take() {
                    self.input.insert(&lost.words);
                    self.attachments.extend(lost.images);
                }
                None
            }
        }
    }
}

/// The fewest columns a line's own words get before the pane gives up on its
/// mark. A mark wider than the pane is a row of label with no words after it, so
/// below this the mark goes and the words stay.
const MIN_BODY: usize = 4;

/// The most of a tool call's arguments a label ever shows. A tool call is a
/// heading for its result, not a transcript of the call: `edit_file src/lex.rs`,
/// not forty lines of JSON (`docs/mush.md` §4.5 R4).
const LABEL_ARGS: usize = 60;

/// How a tool result that never happened is spelled. `agent.rs` prefixes every
/// refused or failed call's result with exactly this (`format!("error: {error}")`),
/// so a result either is one or merely starts like one.
const FAILED: &str = "error:";

/// One tool call's row: `  ⚙ name summarized-args`, budgeted to the pane.
///
/// The arguments are what a path or a command is read from, so the columns they
/// are given are the pane's less the `  ⚙ name ` head's — the number `truncate`
/// was handed used to be a flat 60 that ignored the head, so on a narrow pane a
/// path was cut mid-word with the `…` that says so falling outside the border.
/// The name is never the part that goes: a row too narrow for both keeps the
/// name.
fn tool_label(call: &mush_core::ToolCall, width: usize) -> String {
    // `agent::summarize_args` is the same reading the tree shows.
    let head = format!("  ⚙ {} ", call.function.name);
    let budget = LABEL_ARGS.min(width.saturating_sub(head.width()));
    if budget < MIN_BODY {
        return head.trim_end().to_string();
    }
    format!(
        "{head}{}",
        truncate(&summarize_args(&call.function.arguments), budget)
    )
    .trim_end()
    .to_string()
}

/// The rows of one turn's `reasoning_content`, or none at all.
///
/// `None` and whitespace-only reasonings paint *nothing* — not a bare mark row.
/// A thinking endpoint really does return `""` for a reply that did no thinking
/// (and `reasoning_content` is `None` for every line that never had one), so a
/// mark with no words under it would cost a row of the pane on most turns in a
/// long session, and say nothing with it.
///
/// The shape is the `"tool"` arm's, which already solved "a mark on the first
/// row and an indent under it": the block indents by the same two columns the
/// tool arm does, the mark sits on the first row, and every row is wrapped
/// *inside* the indent plus the mark's own columns so nothing is clipped from
/// the right edge. Dim, because it is what the model thought and not what it
/// told the human, and marked `⋯ ` — a thought trails off into the reply under
/// it. It is [`Kind::Reasoning`] to the fold, whose number for it is
/// `usize::MAX`: shown whole today, because this is the text the human pressed
/// `Ctrl-T` to read — and folded like any other block the day a setting lowers
/// the number, because the fold's number, not this function, is where that
/// decision lives.
///
/// The map is dropped: a thought is not a source line the select mode walks to
/// — the cursor moves over what the model *said* and what a tool returned — and
/// `render_message`'s caller marks every row of this block `None` afterwards.
fn reasoning_rows(out: &mut Vec<Line<'static>>, message: &Message, width: usize, fold: Fold) {
    let Some(reasoning) = message.reasoning_content.as_deref() else {
        return;
    };
    if reasoning.trim().is_empty() {
        return;
    }
    let mut map = Vec::new();
    folded_marked(
        out,
        &mut map,
        Head::solid("  ⋯ ", dim()),
        reasoning,
        width,
        Kind::Reasoning,
        fold,
    );
}

/// The rows a message's pictures get, one dim row each: `▣ path (format ·
/// size)`, named exactly the way the box names an attachment.
///
/// Every arm that paints message rows calls this, and there is one copy of it
/// because the reading must not depend on the role: a picture the human sent
/// and a picture the model read — `read_file` hands a png back *inside the tool
/// result*, so the turn that read it carries it — are one fact about a message.
/// These rows used to live in the `"user"` arm alone, so a turn where the model
/// looked at a screenshot read exactly like one where it did not, and the human
/// had no way to tell the difference.
///
/// The row is the reading of the bytes the message still holds. A picture whose
/// bytes are gone — the session writer turns them into one placeholder line in
/// the text (`Message::drop_images`), and a transcript restored from
/// `.mush/session.json` carries that line — is read there; this paints the ones
/// that still have them.
fn image_rows(out: &mut Vec<Line<'static>>, message: &Message) {
    for image in &message.images {
        out.push(Line::from(Span::styled(
            format!("  ▣ {}", image_label(image)),
            dim(),
        )));
    }
}

/// The rows of one marked line: the mark on the first row, its own width of
/// blank under it, and the words wrapped *inside* the columns the mark leaves.
///
/// Wrapping at the pane's whole width and prepending the mark afterwards made
/// every row `mark` columns too wide for the pane, and ratatui clipped the
/// overflow from the right: an assistant's `w00 … w59` at 80 columns lost
/// `w12 w13` off its first row, `w26 w27` off its second, and one more pair off
/// every row after that — each wrapped line quietly lost its end, for as long as
/// the message was. The tool-call rows and the message box already budgeted
/// their own indent this way; the marks are where the arithmetic was missing.
fn marked(out: &mut Vec<Line<'static>>, mark: &str, style: Style, text: &str, width: usize) {
    // The parser's vocabulary reaches the app here and nowhere else: this
    // function and `reply_style` beside it are the only code that knows what a
    // `Strong` or a `Heading(2)` is, and the module above never learns either.
    use mush_core::text::markdown_rows;

    // The model's reply is the pane's one piece of prose, and the only text
    // whose markdown is read as a view: `render_message`'s assistant arm is the
    // one caller that hands this function `mush › ` in the reply's green, and
    // those are the words a model wrote *for the human*. Every other caller is
    // a line somebody said, and keeps the plain path byte for byte: the human's
    // own message (`you › `), a brief, a parent's steering, mush's notices and
    // footnotes. Three more kinds of text never reach this function at all, and
    // their boundary is the same one: a tool result and a `run_command`
    // transcript are data the human copies verbatim — a diff, a test log, a
    // shell session, where a `#` is a comment and an `*` is a glob — a `Ctrl-T`
    // reasoning row is a working note and not prose, and a tool-call label is
    // the call's own JSON. Only a reply is a document.
    const REPLY_MARK: &str = "mush › ";
    let reply = mark == REPLY_MARK;
    let lead = mark.width();
    // A pane too narrow for the mark and a few words: the mark is what the row
    // cannot afford, because a mark the pane clips is a row that says who spoke
    // and nothing about what was said.
    let (mark, lead) = if width >= lead + MIN_BODY {
        (mark, lead)
    } else {
        ("", 0)
    };
    if reply {
        // The view is a view: `markdown_rows` reads the reply's own bytes and
        // writes nothing back, so what the human copies is still the model's
        // text, markers and all. It wraps inside the columns the mark leaves,
        // exactly as the plain path below does, so no row of the view can
        // outgrow the pane it is painted in.
        for (index, runs) in markdown_rows(text, width.saturating_sub(lead))
            .into_iter()
            .enumerate()
        {
            let head = if index == 0 {
                Span::styled(mark.to_string(), style)
            } else {
                Span::raw(" ".repeat(lead))
            };
            let spans = std::iter::once(head)
                .chain(
                    runs.into_iter()
                        .map(|run| Span::styled(run.text, reply_style(run.style))),
                )
                .collect::<Vec<Span<'static>>>();
            out.push(Line::from(spans));
        }
        return;
    }
    for (index, line) in wrap_text(text, width.saturating_sub(lead))
        .into_iter()
        .enumerate()
    {
        let head = if index == 0 {
            Span::styled(mark.to_string(), style)
        } else {
            Span::raw(" ".repeat(lead))
        };
        out.push(Line::from(vec![head, Span::raw(line)]));
    }
}

/// The markdown view's styles, in one place: the parser's vocabulary ends here,
/// so nothing else in the app learns what a `Strong` or a `Heading(2)` is and a
/// new surface cannot invent its own reading of one.
///
/// The palette is the pane's own. Bold, italic and strike are the modifiers a
/// terminal already has; code, a fence and a link's URL are the dim grey the
/// pane paints its secondary facts in; a heading is the reply's accent, in
/// bold, because the heading is the reply's; a link is underlined, and a
/// bullet's marker — the one part of a row that is layout rather than words —
/// is the accent too.
fn reply_style(style: mush_core::text::RunStyle) -> Style {
    use mush_core::text::RunStyle;
    use ratatui::style::Modifier;

    match style {
        RunStyle::Plain => Style::default(),
        RunStyle::Strong => Style::default().add_modifier(Modifier::BOLD),
        RunStyle::Emphasis => Style::default().add_modifier(Modifier::ITALIC),
        RunStyle::Strike => Style::default().add_modifier(Modifier::CROSSED_OUT),
        RunStyle::Code | RunStyle::Fence | RunStyle::Url => Style::default().fg(Color::DarkGray),
        RunStyle::Heading(_) => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        RunStyle::Bullet => Style::default().fg(Color::Green),
        RunStyle::Link => Style::default().add_modifier(Modifier::UNDERLINED),
    }
}

/// The rows of one notice, wrapped at the pane's width and marked by kind: only
/// a failure shouts. The mark leads the first row only — a wrapped line is one
/// line, and a column of `!` reads as several failures.
fn footnote_lines(notice: &Notice, width: usize) -> Vec<Line<'static>> {
    let (mark, style) = notice.kind.mark();
    let mut rows = Vec::new();
    marked(&mut rows, mark, style, &notice.line(), width);
    rows
}

/// How a pane says it is showing an excerpt. One wording, because the foot's
/// count row, the pane's title and a block's `…` row ([`elision`]) are places
/// saying the same number.
fn more_label(hidden: usize) -> String {
    format!("+{hidden} more lines")
}

/// `1,284`: a count with its thousands marked, the way a number a human reads
/// rather than counts is written.
fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// The head of the one user message a fold leaves behind
/// (`prompt::compaction_message`): the model's own summary, carried as the next
/// conversation's first message. It is mush's line, not the human's words.
const FOLDED: &str = "Context compacted";

/// The two lines one loop guard writes, in its own vocabulary: the notice it
/// emits as it stops the run, and the failure the run then ends with
/// (`agent.rs`'s guard, which the run has no other way to report). They are one
/// event, so the pane paints one line for it — see [`Chat::note_error_for`].
const LOOP_NOTICE: &str = "the run repeated the same tool call";
const LOOP_STOP: &str = "the run was stopped as a loop";

/// Who said a user line when nothing recorded it: a transcript restored from the
/// session file, or the one a fold just replaced. Everything mush writes into a
/// conversation has a shape — a child's `#1 done: …` / `#1 stopped: …` /
/// `#1 failed: …`, a job's `#c2 done: …`, a fold's carried summary, the line
/// that says the oldest turns were dropped ([`transcript::is_dropped_note`]) —
/// and a child's transcript opens with the brief its parent spawned it with.
/// What is left is the human's, because that is what most of a transcript is.
///
/// The one line this cannot place is a parent's steering after a restart: the
/// words look exactly like the human's own nudge, and nothing in the file says
/// which they were. It reads as the human's until the process is new again —
/// the alternative would be painting the human's question as somebody else's.
fn unrecorded(agent: AgentId, index: usize, message: &Message) -> Voice {
    let text = message.text();
    if report(text) || text.starts_with(FOLDED) || transcript::is_dropped_note(message) {
        return Voice::Mush;
    }
    if agent != AgentId::ROOT && index == 0 {
        return Voice::Brief;
    }
    Voice::Human
}

/// Who said a line that is known *not* to be the human's: [`unrecorded`] read at
/// the one moment the answer is certain, so what it cannot place is another
/// agent — a parent's steering, the only other speaker a transcript has.
fn elsewhere(agent: AgentId, index: usize, message: &Message) -> Voice {
    match unrecorded(agent, index, message) {
        Voice::Human => Voice::Parent,
        voice => voice,
    }
}

/// Whether a line is one of mush's reports — `#1 done: …`, `#c2 stopped: …`,
/// `#3 cut off: …` — written by the run loop, the job registry and the UI's own
/// last-resort report with exactly this vocabulary.
fn report(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('#') else {
        return false;
    };
    let rest = rest.strip_prefix('c').unwrap_or(rest);
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0
        && [" done:", " stopped:", " failed:", " cut off:"]
            .iter()
            .any(|tail| rest[digits..].starts_with(tail))
}

/// The kinds of multi-line block a conversation paints, one number each in
/// [`Fold`].
///
/// The list is closed on purpose. [`Kind::slot`] is a `match` with no wildcard,
/// and [`Fold`]'s table is exactly [`Kind::COUNT`] long, so a block that
/// arrives as a new variant cannot inherit a default number by accident: the
/// compiler names every place the new kind has to be given one before the crate
/// builds again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A tool call's result: a file dump — a diff, a test log, a shell
    /// transcript — read from its head and copied whole behind its `…`.
    Result,
    /// The model's own reasoning — the `Ctrl-T` block [`reasoning_rows`]
    /// paints.
    Reasoning,
}

impl Kind {
    /// How many kinds there are: the length of [`Fold`]'s table, so a new
    /// variant that is given a slot but not a table entry is a compile error,
    /// and one given a table entry but not a slot is too.
    const COUNT: usize = 2;

    /// This kind's own number in [`Fold`]'s table. No wildcard arm: a new kind
    /// cannot compile until it is given a slot here, and the slot is where its
    /// number is read from.
    fn slot(self) -> usize {
        match self {
            Kind::Result => 0,
            Kind::Reasoning => 1,
        }
    }
}

/// The one place a conversation's "how much of this thing does the human see"
/// decision lives: how many rows, per [`Kind`] of block, a pane paints before
/// the `…` row that stands for the rest.
///
/// It is a *value* and not a `const` per arm. A view can hold one and set it
/// ([`Fold::with`]) — the `Ctrl-O` child is the first setting, a `0`-rows
/// number for one kind — and a setting will later read the numbers from
/// configuration, which is why they are here and not spelled at a paint site.
/// `Chat` holds the one a conversation paints through.
///
/// The numbers are **per kind** because the kinds are read differently. A tool
/// result is a dump: the human reads its head and copies the rest, and the pane
/// exists to keep a long transcript scrollable — eight rows, the number the
/// `"tool"` arm used to keep as a `const` of its own. Wrapping a block only as
/// far as the fold is also why a long result stopped being most of a frame's
/// cost on a long session (see [`wrap_text_capped`]). A reasoning block is the
/// text the human pressed `Ctrl-T` to read, so its number is `usize::MAX`:
/// shown whole today, with the slot in place because the setting the human
/// already asked for is "one for child/tool calls and another for thinking rows
/// shown".
///
/// A block that reports a **failure** is kept even where the fold would hide
/// it: the failure is never what the fold gives up. The rule lives here, not in
/// an arm and not in the handler of a key that changes a number, so a `0`-rows
/// setting still paints a failed result's own `! error: …` row — the same rule
/// the foot's cap already holds ("the failure is never the line the cap gives
/// up").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fold {
    /// One number per kind, indexed by [`Kind::slot`].
    rows: [usize; Kind::COUNT],
}

impl Fold {
    /// The numbers a conversation opens with: eight rows for a result, and no
    /// bound for the reasoning.
    pub const DEFAULT: Fold = Fold {
        rows: [8, usize::MAX],
    };

    /// How many of `text`'s rows this fold lets a pane paint for `kind` — the
    /// kind's number, and the one exception at a `0`-rows setting: a block that
    /// reports a failure keeps its failure row (see the type's doc).
    pub fn shown(&self, kind: Kind, text: &str) -> usize {
        let rows = self.rows[kind.slot()];
        if rows == 0 && fails(kind, text) {
            1
        } else {
            rows
        }
    }

    /// The same fold with one kind's number changed — how a view sets it: a
    /// setting that will read a number from configuration, and the key that
    /// lowers one at runtime (the `Ctrl-O` child), both hand the result down
    /// through here. The setter is test-only until that child lands, which is
    /// why the attribute is here: the value is settable, and this is the door.
    #[cfg(test)]
    pub fn with(mut self, kind: Kind, rows: usize) -> Self {
        self.rows[kind.slot()] = rows;
        self
    }
}

/// Whether a block of this kind reports a failure — the one thing the fold
/// never gives up (see [`Fold`]'s own doc). The vocabulary is the kinds' own: a
/// tool result fails when it opens with mush's `error:` prefix. A thought is
/// nobody's failure to report.
fn fails(kind: Kind, text: &str) -> bool {
    match kind {
        Kind::Result => text.trim_start().starts_with(FAILED),
        Kind::Reasoning => false,
    }
}

/// The one row a fold spends on saying what it hid: the `…` and the tree's own
/// excerpt count ([`more_label`]) — the same words the foot's count row and the
/// pane's title use, so no surface invents a second.
fn elision(hidden: usize) -> String {
    format!("… {}", more_label(hidden))
}

/// The head of a folded block: the mark that leads its first row, and the
/// styles its mark and its words are painted in.
///
/// A tool result and a working note paint the whole row in one colour, while a
/// voice colours only its mark and leaves the words plain ([`marked`]); the two
/// styles are bundled here, so the painter takes them as one argument.
#[derive(Clone, Copy)]
struct Head<'a> {
    mark: &'a str,
    mark_style: Style,
    body: Style,
}

impl<'a> Head<'a> {
    /// A row painted in one style throughout: a tool result, a reasoning row.
    fn solid(mark: &'a str, style: Style) -> Self {
        Self {
            mark,
            mark_style: style,
            body: style,
        }
    }
}

/// The rows of one folded block, and the source line each is the reading of:
/// [`marked`]'s plain path, stopped at the kind's number.
///
/// The mark leads the first row and its own width of blank stands under it,
/// exactly as [`marked`] paints a voice's rows; what is added is the fold — at
/// most [`Fold::shown`] wrapped rows, then the one `…` row [`elision`] spells,
/// whenever the text runs on. The block is always a plain line: a folded block
/// is a dump, a report or a working note, and never the reply the markdown view
/// is for.
///
/// The `…` row carries the source line the first hidden row came from, so the
/// select cursor can stand on the line the fold hid — and, through the fallback
/// in [`first_row`], on the lines after it. `hidden` is the number of source
/// lines the `…` stands for (that line and every line after it), counted
/// without wrapping them: counting painted rows would wrap the very text the
/// fold exists not to wrap, and a line is what a reader counts in a dump
/// anyway.
fn folded_marked(
    out: &mut Vec<Line<'static>>,
    rows: &mut Vec<Option<usize>>,
    head: Head<'_>,
    text: &str,
    width: usize,
    kind: Kind,
    fold: Fold,
) {
    let start = out.len();
    let base = rows.len();
    // A pane too narrow for the mark and a few words drops the mark, exactly as
    // [`marked`] does for a voice's rows: a mark the pane clips is a row that
    // says who spoke and nothing about what was said.
    let lead = head.mark.width();
    let (mark, lead) = if width >= lead + MIN_BODY {
        (head.mark, lead)
    } else {
        ("", 0)
    };
    let mark_style = head.mark_style;
    let body_style = head.body;
    let wrap = width.saturating_sub(lead);
    let shown = fold.shown(kind, text);
    // Wrapped only as far as the fold: one row past the number is what tells
    // the fold it has more to stand for.
    let wrapped = wrap_text_capped(text, wrap, shown.saturating_add(1));
    let clipped = wrapped.len() > shown;
    // Which source line each painted row is the reading of, and how many lines
    // the block has: the walk wraps only the lines the fold may paint and scans
    // the rest, so the count cannot cost what the cap exists to avoid.
    let mut tags: Vec<usize> = Vec::new();
    let mut total = 0usize;
    let mut left = shown.saturating_add(1);
    for (line, raw) in text.split('\n').enumerate() {
        total = line + 1;
        if left == 0 {
            continue;
        }
        let count = wrap_text_capped(raw, wrap, left).len();
        tags.extend(std::iter::repeat(line).take(count));
        left -= count;
    }
    debug_assert_eq!(tags.len(), wrapped.len(), "one source per wrapped row");
    for (index, line) in wrapped.iter().take(shown).enumerate() {
        let head = if index == 0 {
            Span::styled(mark.to_string(), mark_style)
        } else {
            Span::styled(" ".repeat(lead), body_style)
        };
        out.push(Line::from(vec![
            head,
            Span::styled(line.clone(), body_style),
        ]));
        rows.push(tags.get(index).copied());
    }
    if clipped {
        // The `…` stands for the first wrapped row the fold did not paint, so
        // the cursor can stand on the line it hides — and, through the fallback
        // in [`first_row`], on the lines after it.
        let hidden = total - tags[shown];
        out.push(Line::from(Span::styled(
            format!("{}{}", " ".repeat(lead), elision(hidden)),
            mark_style,
        )));
        rows.push(tags.get(shown).copied());
    }
    debug_assert_eq!(out.len() - start, rows.len() - base, "one entry per row");
}

/// One message's rows: who said it, wrapped at the pane's width — and, beside
/// them, the source line of the message's own text each row is the reading of.
///
/// The map is *returned* rather than kept by the painter because a source line
/// is one or more painted rows, and only the pass that paints a row knows
/// whether the row is a soft wrap of the line above it, a markdown view of it,
/// or the `…` a fold hid. A second pass that counted them could disagree with
/// the rows on screen, and the cursor would then sit on the wrong one. The
/// caller adds the message's index.
///
/// `reasoning` is the pane's `Ctrl-T` choice and `fold` the pane's [`Fold`] —
/// how much of each kind of block it paints. Both are threaded in rather than
/// read off a `Chat` this free function has no handle on.
fn render_message(
    out: &mut Vec<Line<'static>>,
    message: &Message,
    voice: Option<Voice>,
    width: usize,
    reasoning: bool,
    fold: Fold,
) -> Vec<Option<usize>> {
    let start = out.len();
    let mut rows: Vec<Option<usize>> = Vec::new();
    match message.role.as_str() {
        "user" => {
            // Mush's own line in the conversation is marked like the other
            // lines mush writes into a pane, and `mark()` is the one spelling
            // of that mark as it is of every speaker's.
            let (mark, style) = voice.unwrap_or(Voice::Human).mark();
            // The mark is painted even for a message that is only an
            // attachment: the `▣` rows below are *what* was said, whoever said
            // it, and the mark is *who* said it. Without it, a picture the
            // human sent would read exactly like a dim line of mush's own.
            mark_rows(
                out,
                &mut rows,
                mark,
                style,
                message.text(),
                width,
                View::Plain,
            );
            image_rows(out, message);
            rows.resize(out.len() - start, None);
            out.push(Line::from(""));
            rows.push(None);
        }
        "assistant" => {
            // The reasoning comes first because that is the order it decided
            // the turn in: the human reading down the pane sees what the model
            // thought, then what it said. It is a block of its own rather than
            // a third colour on the reply, and it is[`Kind::Reasoning`] to the
            // fold, whose number for it is `usize::MAX`: what the human pressed
            // `Ctrl-T` to read is shown whole, and only a setting that lowers
            // the number makes it fold like any other block.
            if reasoning {
                reasoning_rows(out, message, width, fold);
                rows.resize(out.len() - start, None);
            }
            let text = message.text();
            if !text.trim().is_empty() {
                // The reply is the conversation's own text: the one block the
                // fold never touches ([`voice_kind`]'s other exemption), so it
                // goes to the plain painter, not through the fold.
                mark_rows(
                    out,
                    &mut rows,
                    "mush › ",
                    Style::default().fg(Color::Green),
                    text,
                    width,
                    View::Markdown,
                );
            }
            for call in message.tool_calls() {
                out.push(Line::from(Span::styled(
                    tool_label(call, width),
                    Style::default().fg(Color::Yellow),
                )));
                rows.push(None);
            }
            image_rows(out, message);
            rows.resize(out.len() - start, None);
            out.push(Line::from(""));
            rows.push(None);
        }
        "tool" => {
            // A result is a file dump, so it arrives folded: the number of rows
            // it gets, and the reason it is eight, are [`Fold`]'s
            // ([`Kind::Result`]) — the one place a conversation's "how much of
            // this thing does the human see" lives. A result that came back
            // `error: …` — mush's own spelling for a call that was refused or
            // that failed — is not a result, and it was painted exactly like
            // one, with only the word at the front to tell them apart. The mark
            // is the difference now, and it is red, because this is the one
            // kind of line in the transcript that reports something did not
            // happen. Its failure row is the one the fold never gives up, at
            // any number, because that rule lives on [`Fold`] too.
            let failed = message.text().trim_start().starts_with(FAILED);
            let (mark, style) = if failed {
                ("  ! ", Style::default().fg(Color::Red))
            } else {
                ("  ", dim())
            };
            folded_marked(
                out,
                &mut rows,
                Head::solid(mark, style),
                message.text(),
                width,
                Kind::Result,
                fold,
            );
            image_rows(out, message);
            rows.resize(out.len() - start, None);
            out.push(Line::from(""));
            rows.push(None);
        }
        _ => {}
    }
    debug_assert_eq!(
        out.len() - start,
        rows.len(),
        "one map entry per painted row"
    );
    rows
}

/// Which view makes a text's rows: the reply's markdown, or the wrapper every
/// other line goes through.
///
/// [`marked`] decides the view from the mark it is handed — the reply's
/// `mush › ` is the one that reaches the parser — and this names that same
/// choice for the caller that has to follow the rows back to their source
/// lines: the two views split rows over a line differently (the view does not
/// paint a heading's `#`s or a fence's own lines), so a map counted off the
/// source alone would point at the wrong row.
#[derive(Clone, Copy)]
enum View {
    Plain,
    Markdown,
}

/// The rows of one marked line, and the source line each is the reading of:
/// [`marked`] paints them and this tags them.
///
/// `marked` wraps each source line on its own (`wrap_text` splits on `\n`
/// first, and the markdown parser is line-local for the same reason), so its
/// rows come out one source line at a time; the head it put on the first row is
/// the width the line was wrapped inside, read back off the row rather than
/// recomputed, so the map cannot disagree with the mark the pane could afford.
///
/// The markdown walk restates the view's one cross-line rule — a fence line
/// paints no row, and the lines inside a fence are one plain block — because the
/// parser's own `fence_line` is not public and asking it for a prefix of the
/// reply per source line would parse the message once per line. The
/// `debug_assert` below is what keeps the two row counts from drifting.
fn mark_rows(
    out: &mut Vec<Line<'static>>,
    rows: &mut Vec<Option<usize>>,
    mark: &str,
    style: Style,
    text: &str,
    width: usize,
    view: View,
) {
    let start = out.len();
    let base = rows.len();
    debug_assert_eq!(out.len(), rows.len(), "the map is one entry per row");
    marked(out, mark, style, text, width);
    let lead = out
        .get(start)
        .and_then(|row| row.spans.first())
        .map_or(0, |head| UnicodeWidthStr::width(head.content.as_ref()));
    let wrap = width.saturating_sub(lead);
    let mut fence = false;
    for (line, raw) in text.split('\n').enumerate() {
        let count = match view {
            View::Plain => wrap_text(raw, wrap).len(),
            View::Markdown => {
                let source = sanitize(raw);
                if source.trim_start().starts_with("```") {
                    fence = !fence;
                    0
                } else if fence {
                    wrap_text(&source, wrap).len()
                } else {
                    markdown_rows(&source, wrap).len()
                }
            }
        };
        rows.extend(std::iter::repeat(Some(line)).take(count));
    }
    // The buffers are parallel, not merely both filled: `marked` pushed the
    // rows and this pushed one entry for each of them, and it is that pairing —
    // the map's index into `out` — that the whole selection reads.
    debug_assert_eq!(out.len() - start, rows.len() - base, "one entry per row");
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::app::keys::{self, Intent};
    use crate::app::{Compacting, Focus, Phase};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A `Ctrl-` key as a terminal delivers it.
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// An attached image, as `Ctrl-V` or a pasted path produces one. Its bytes
    /// are what a pop and a restore move around; nothing here looks at them,
    /// and it names no pixel size, so it weighs the bytes.
    fn image(path: &str) -> Image {
        Image {
            path: path.to_string(),
            mime: "image/png".to_string(),
            bytes: vec![1, 2, 3],
            pixels: None,
        }
    }

    /// Press a key the way the app does: the pure keymap decides which pane
    /// owns it, and the chat runs what it is handed. The editing keys are
    /// tested through the real table rather than a private entry point, so a
    /// key that stopped reaching the box fails here.
    fn press(chat: &mut Chat, key: KeyEvent) -> bool {
        // Not selecting: this helper is how the box's own keys are tested, and
        // the mode's keys are pinned in the tests that open the mode.
        match keys::key(Focus::Chat, false, false, key) {
            Intent::Chat(intent) => {
                // The pane the key is about is the one the chat is showing.
                chat.apply(AgentId::ROOT, intent);
                true
            }
            _ => false,
        }
    }

    fn pane(agent: AgentId) -> Pane<'static> {
        Pane {
            agent,
            words: None,
            spin: 0,
            label: "test-model · ctx ~500k",
        }
    }

    /// An assistant turn carrying the endpoint's own reasoning, the way a
    /// thinking model returns one.
    fn thinking(text: &str, reasoning: &str) -> Message {
        Message {
            reasoning_content: Some(reasoning.into()),
            ..Message::assistant(text)
        }
    }

    /// The human speaks, through the box they type in: what `App::deliver` does,
    /// in the order it does it. A test that pushes a user message *without* this
    /// is testing somebody else's line — which is the whole point of the voice.
    fn say(chat: &mut Chat, agent: AgentId, text: &str) {
        chat.insert(text);
        assert_eq!(chat.take_input().trim(), text);
        chat.push_message(agent, Message::user(text));
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

    /// One message's rows, as `render_message` paints them: the map beside them
    /// is the select mode's own, and these tests read the words.
    fn message_rows(
        message: &Message,
        voice: Option<Voice>,
        width: usize,
        reasoning: bool,
    ) -> Vec<Line<'static>> {
        message_rows_under(message, voice, width, reasoning, Fold::DEFAULT)
    }

    /// [`message_rows`] through a fold of the caller's choosing: the pane's
    /// number under the test's control, where the conversation's own is a
    /// `Chat` field.
    fn message_rows_under(
        message: &Message,
        voice: Option<Voice>,
        width: usize,
        reasoning: bool,
        fold: Fold,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        render_message(&mut lines, message, voice, width, reasoning, fold);
        lines
    }

    /// A 40×10 terminal leaves the transcript pane one row tall, and every
    /// message ends with a blank separator: that blank was the only row shown,
    /// so the reply was invisible (finding B4). The trim happens before the
    /// window is cut, for the same reason at every height.
    #[test]
    fn a_one_row_pane_shows_a_message_and_not_the_blank_after_it() {
        let mut chat = Chat::bare();
        say(&mut chat, AgentId::ROOT, "make the lexer faster");
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

    /// The window follows the scrollback, and the bottom is where the newest
    /// line is: a pane that is at the bottom needs nothing done to it.
    #[test]
    fn the_window_follows_the_scrollback() {
        let mut chat = Chat::bare();
        for i in 0..5 {
            say(&mut chat, AgentId::ROOT, &format!("line {i}"));
        }
        let pane = pane(AgentId::ROOT);
        let bottom = shown(&pane_rows(&chat, &pane, 20, 2));
        assert_eq!(bottom, vec!["you › line 4"], "anchored at the newest");

        // Up and down are the pane's own keys.
        assert!(press(&mut chat, key(KeyCode::Up)));
        let up = shown(&pane_rows(&chat, &pane, 20, 2));
        assert_ne!(up, bottom, "scrolling shows what was above the fold");
        assert!(up.iter().any(|row| row.contains("line 3")), "{up:?}");

        assert!(press(&mut chat, key(KeyCode::Down)));
        assert_eq!(shown(&pane_rows(&chat, &pane, 20, 2)), bottom);
        // And a new line arrives at the bottom, where the pane already is.
        say(&mut chat, AgentId::ROOT, "line 5");
        assert_eq!(shown(&pane_rows(&chat, &pane, 20, 2)), vec!["you › line 5"]);
    }

    /// A pane the human has scrolled away from holds the window it is showing:
    /// lines that arrive afterwards land below it instead of pushing the text
    /// they are reading up the pane — and scrolling back down is what rejoins
    /// the newest line (finding U3).
    #[test]
    fn a_held_window_does_not_follow_the_lines_that_arrive() {
        let mut chat = Chat::bare();
        for index in 0..6 {
            say(&mut chat, AgentId::ROOT, &format!("line {index}"));
        }
        let pane = pane(AgentId::ROOT);
        chat.scroll_by(AgentId::ROOT, 4);
        let held = shown(&pane_rows(&chat, &pane, 20, 2));
        assert!(
            !held.join("\n").contains("line 5"),
            "the pane is away from the newest line: {held:?}"
        );

        say(&mut chat, AgentId::ROOT, "arrived");
        assert_eq!(
            shown(&pane_rows(&chat, &pane, 20, 2)),
            held,
            "the rows the human was reading stayed where they were"
        );

        // The step that leaves the held window rejoins the newest line.
        chat.scroll_by(AgentId::ROOT, -4);
        let bottom = shown(&pane_rows(&chat, &pane, 20, 2));
        assert!(
            bottom.iter().any(|row| row.contains("arrived")),
            "{bottom:?}"
        );
    }

    /// A pane holding rows above the newest line says so in its title: the foot
    /// staying put is what makes it a foot, but a held window and a following
    /// one look identical, and the pane is the only place that fact can live at
    /// every size (finding T10). The marker is derived from the reading, so it
    /// is there the instant they scroll and gone the instant they come back.
    #[test]
    fn a_pane_away_from_the_bottom_says_so_in_its_title() {
        let mut chat = Chat::bare();
        for index in 0..12 {
            say(&mut chat, AgentId::ROOT, &format!("line {index}"));
        }
        assert_eq!(
            chat.painted(&pane(AgentId::ROOT), 60, 6).title,
            " mush ",
            "a pane at the bottom has nothing to say about it"
        );

        chat.scroll_by(AgentId::ROOT, 3);
        assert_eq!(
            chat.painted(&pane(AgentId::ROOT), 60, 6).title,
            " mush · scrolled ↑3 rows · PgDn "
        );

        // Both of the title's facts fit side by side: the foot's own count —
        // which the title carries only when the pane has no row to spend on the
        // count line — and the reading position.
        chat.note_for(AgentId::ROOT, "a long note ".repeat(20));
        let painted = chat.painted(&pane(AgentId::ROOT), 60, 2);
        assert!(
            painted.title.contains("/notes") && painted.title.contains("scrolled ↑3 rows"),
            "both facts fit: {}",
            painted.title
        );
        assert_eq!(
            chat.painted(&pane(AgentId(1)), 60, 6).title,
            " agent #1 ",
            "another pane is still at the bottom"
        );

        chat.scroll_by(AgentId::ROOT, -3);
        assert_eq!(
            chat.painted(&pane(AgentId::ROOT), 60, 6).title,
            " mush ",
            "and coming back to the newest line takes the marker with it"
        );
    }

    /// Scrolling is per conversation: one pane's position is not another's, so
    /// reading a child's scrollback cannot move the root's (finding U3).
    #[test]
    fn one_panes_position_is_not_anothers() {
        let mut chat = Chat::bare();
        for index in 0..6 {
            say(&mut chat, AgentId::ROOT, &format!("root {index}"));
            say(&mut chat, AgentId(1), &format!("child {index}"));
        }
        let root = pane(AgentId::ROOT);
        let child = pane(AgentId(1));
        let child_rows = shown(&pane_rows(&chat, &child, 20, 2));

        chat.scroll_by(AgentId::ROOT, 3);
        assert_ne!(
            shown(&pane_rows(&chat, &root, 20, 2)),
            shown(&pane_rows(&chat, &child, 20, 2)),
            "the root scrolled"
        );
        assert_eq!(
            shown(&pane_rows(&chat, &child, 20, 2)),
            child_rows,
            "the child's pane did not move"
        );
    }

    /// What `Enter` copies is the message's own text, byte for byte: a pane's
    /// soft wrap of a paragraph is a fact about the screen, and a paragraph's
    /// own newlines are facts about the text. The pane must wrap more rows than
    /// the text has lines for this to say anything, so the painted rows are
    /// read beside the copy.
    #[test]
    fn a_reply_is_copied_as_the_message_wrote_it() {
        let reply = "the first paragraph, long enough that a narrow pane wraps it more than once\n\nand a second paragraph\n\nand a third";
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::assistant(reply));
        assert!(chat.start_select(AgentId::ROOT).is_none(), "the mode is on");
        chat.select_apply(AgentId::ROOT, SelectKey::First);
        chat.select_apply(AgentId::ROOT, SelectKey::Extend(keys::PAGE));
        let copied = chat
            .select_apply(AgentId::ROOT, SelectKey::Copy)
            .expect("Enter copies");
        assert_eq!(copied.text, reply, "the message's own bytes");
        assert_eq!(
            copied.line,
            format!(
                "copied {} lines from #0's reply — {} bytes",
                reply.split('\n').count(),
                reply.len()
            )
        );
        assert!(!chat.selecting(), "Enter leaves the mode");

        let pane = pane(AgentId::ROOT);
        let painted = chat.painted(&pane, 20, 12);
        assert!(
            painted.lines.len() > reply.split('\n').count(),
            "the pane wrapped the paragraphs: {}",
            shown(&painted.lines).join(" / ")
        );
    }

    /// A tool result is copied whole, byte for byte, including the lines the
    /// pane's cap hides: the cap bounds the frame, not the transcript — and a
    /// soft wrap would have eaten the tab or the indent.
    #[test]
    fn a_tool_result_is_copied_byte_exact() {
        let result = (0..20)
            .map(|n| format!("line {n}: a\tb"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::tool("call_1", &result));
        chat.start_select(AgentId::ROOT);
        // The cursor starts on the newest line, so a shift-page back past the
        // oldest one is a selection of the whole result — the 11 lines a page
        // covers are not the transcript, and the copy is not the pane.
        chat.select_apply(AgentId::ROOT, SelectKey::Extend(-2 * keys::PAGE));
        let copied = chat
            .select_apply(AgentId::ROOT, SelectKey::Copy)
            .expect("Enter copies");
        assert_eq!(copied.text, result, "the whole result, cap and all");
        assert_eq!(
            copied.line,
            format!(
                "copied 20 lines from #0's tool result — {} bytes",
                result.len()
            )
        );
    }

    /// The human's own message is the text they typed, newlines and all.
    #[test]
    fn the_humans_own_message_is_copied_as_it_was_typed() {
        let mut chat = Chat::bare();
        say(&mut chat, AgentId::ROOT, "one\ntwo");
        chat.start_select(AgentId::ROOT);
        chat.select_apply(AgentId::ROOT, SelectKey::First);
        chat.select_apply(AgentId::ROOT, SelectKey::Extend(keys::PAGE));
        let copied = chat
            .select_apply(AgentId::ROOT, SelectKey::Copy)
            .expect("Enter copies");
        assert_eq!(copied.text, "one\ntwo");
        assert_eq!(copied.line, "copied 2 lines from your message — 7 bytes");
    }

    /// A message whose bytes are gone carries its placeholder, and that is what
    /// the copy is: a selection over the transcript is the transcript's text,
    /// not the picture that once was there.
    #[test]
    fn a_message_that_dropped_its_images_carries_its_placeholder() {
        let mut message = Message::user_with_images("look at this", vec![image("shot.png")]);
        message.drop_images();
        let text = message.text().to_string();
        assert!(text.contains("[image: shot.png (png)"), "{text}");
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, message);
        chat.start_select(AgentId::ROOT);
        chat.select_apply(AgentId::ROOT, SelectKey::First);
        chat.select_apply(AgentId::ROOT, SelectKey::Extend(keys::PAGE));
        let copied = chat
            .select_apply(AgentId::ROOT, SelectKey::Copy)
            .expect("Enter copies");
        assert_eq!(copied.text, text);
    }

    /// A selection that crosses a message boundary joins the messages at the
    /// lines the selection starts and ends on, and says how many messages it
    /// came from.
    #[test]
    fn a_selection_spanning_two_messages_joins_them_at_their_own_lines() {
        let mut chat = Chat::bare();
        say(&mut chat, AgentId::ROOT, "one\ntwo");
        chat.push_message(AgentId::ROOT, Message::assistant("three\nfour"));
        chat.start_select(AgentId::ROOT);
        chat.select_apply(AgentId::ROOT, SelectKey::First);
        chat.select_apply(AgentId::ROOT, SelectKey::Extend(keys::PAGE));
        let copied = chat
            .select_apply(AgentId::ROOT, SelectKey::Copy)
            .expect("Enter copies");
        assert_eq!(copied.text, "one\ntwo\nthree\nfour");
        assert_eq!(copied.line, "copied 4 lines from 2 messages — 18 bytes");
    }

    /// A count a human reads rather than counts: the bar says `1,284`, not
    /// `1284`.
    #[test]
    fn the_copied_line_marks_the_thousands_of_a_big_number() {
        let text = "x".repeat(1234);
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::user(&text));
        chat.start_select(AgentId::ROOT);
        let copied = chat
            .select_apply(AgentId::ROOT, SelectKey::Copy)
            .expect("Enter copies");
        assert_eq!(
            copied.line,
            format!(
                "copied 1 line from your message — {text_len} bytes",
                text_len = "1,234"
            )
        );
    }

    /// The oldest and newest lines are ends of the transcript, not walls: a key
    /// held down past either one stays where it is, and the copy still works.
    #[test]
    fn moving_the_cursor_past_either_end_does_not_panic() {
        let mut chat = Chat::bare();
        say(&mut chat, AgentId::ROOT, "the only line");
        chat.start_select(AgentId::ROOT);
        for step in [1, 100, -100, -1, 0] {
            chat.select_apply(AgentId::ROOT, SelectKey::Move(step));
            chat.select_apply(AgentId::ROOT, SelectKey::Extend(step));
        }
        let copied = chat
            .select_apply(AgentId::ROOT, SelectKey::Copy)
            .expect("Enter copies");
        assert_eq!(copied.text, "the only line");

        // And a pane with nothing said yet is not a mode with nowhere to be.
        let mut empty = Chat::bare();
        assert!(empty.start_select(AgentId::ROOT).is_some(), "it says so");
        assert!(!empty.selecting(), "and does not enter");
    }

    /// `Esc` leaves the mode without copying and without touching the box: the
    /// keys are the mode's, so the pane's own clear is not one of them.
    #[test]
    fn esc_leaves_the_mode_without_copying_and_the_box_alone() {
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::assistant("something"));
        chat.insert("a draft");
        chat.start_select(AgentId::ROOT);
        assert!(
            chat.select_apply(AgentId::ROOT, SelectKey::Cancel)
                .is_none(),
            "Esc copies nothing"
        );
        assert!(!chat.selecting());
        assert_eq!(chat.input().text(), "a draft");
    }

    /// The cursor and the selection are painted on the transcript's own lines,
    /// whatever the window is showing: a line the pane wraps is one line of the
    /// cursor.
    #[test]
    fn the_cursor_and_the_selection_are_painted_on_their_own_lines() {
        let mut chat = Chat::bare();
        say(&mut chat, AgentId::ROOT, "one\ntwo");
        chat.push_message(AgentId::ROOT, Message::assistant("three\nfour"));
        chat.start_select(AgentId::ROOT);
        let pane = pane(AgentId::ROOT);
        let painted = chat.painted(&pane, 20, 6);
        let select = painted.select.as_ref().expect("the mode paints");
        assert!(
            select.selected.is_empty(),
            "a bare cursor is not a selection yet"
        );
        let rows = shown(&painted.lines);
        assert!(
            select.cursor.iter().any(|at| rows[*at].contains("four")),
            "the cursor stands on the newest line: {:?} {rows:?}",
            select.cursor
        );
        chat.select_apply(AgentId::ROOT, SelectKey::First);
        let painted = chat.painted(&pane, 20, 6);
        let select = painted.select.as_ref().expect("the mode paints");
        let rows = shown(&painted.lines);
        assert!(
            select.cursor.iter().any(|at| rows[*at].contains("one")),
            "the cursor followed the jump to the oldest line: {:?} {rows:?}",
            select.cursor
        );

        chat.select_apply(AgentId::ROOT, SelectKey::Extend(keys::PAGE));
        let painted = chat.painted(&pane, 20, 6);
        let select = painted.select.as_ref().expect("the mode paints");
        let covered: Vec<String> = select
            .selected
            .iter()
            .map(|at| shown(&painted.lines[*at..=*at])[0].clone())
            .collect();
        for word in ["one", "two", "three", "four"] {
            assert!(
                covered.iter().any(|row| row.contains(word)),
                "{word} is selected: {covered:?}"
            );
        }
        let rows = shown(&painted.lines);
        assert!(
            select.cursor.iter().any(|at| rows[*at].contains("four")),
            "the extended cursor is the selection's new end: {:?} {rows:?}",
            select.cursor
        );
    }

    /// A pane smaller than the transcript shows the line the cursor is on, not
    /// the bottom: the mode's window follows the cursor, which is the whole
    /// point of a cursor.
    #[test]
    fn the_panes_window_follows_the_cursor_and_not_the_bottom() {
        let mut chat = Chat::bare();
        for n in 0..8 {
            chat.push_message(AgentId::ROOT, Message::assistant(format!("reply {n}")));
        }
        chat.start_select(AgentId::ROOT);
        let pane = pane(AgentId::ROOT);
        let painted = chat.painted(&pane, 40, 5);
        let cursor = painted.select.as_ref().expect("painted").cursor[0];
        assert!(
            shown(&painted.lines[cursor..=cursor])[0].contains("reply 7"),
            "the newest line to start with"
        );
        chat.select_apply(AgentId::ROOT, SelectKey::First);
        let painted = chat.painted(&pane, 40, 5);
        let cursor = painted.select.as_ref().expect("painted").cursor[0];
        assert!(
            shown(&painted.lines[cursor..=cursor])[0].contains("reply 0"),
            "and the oldest after Home"
        );
    }

    /// A line behind a tool result's cap still has a row to stand on — the `…`
    /// that hides it — and the copy takes the line whole: the cap is the pane's,
    /// not the transcript's.
    #[test]
    fn a_line_behind_a_tool_results_cap_stands_on_the_ellipsis() {
        let result = (0..12)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::tool("call_1", &result));
        chat.start_select(AgentId::ROOT);
        chat.select_apply(AgentId::ROOT, SelectKey::First);
        chat.select_apply(AgentId::ROOT, SelectKey::Move(11));
        let pane = pane(AgentId::ROOT);
        let painted = chat.painted(&pane, 40, 8);
        let cursor = painted.select.as_ref().expect("painted").cursor.clone();
        assert_eq!(cursor.len(), 1, "the hidden line has one row to stand on");
        assert!(
            shown(&painted.lines[cursor[0]..=cursor[0]])[0].contains('…'),
            "and it is the ellipsis"
        );
        let copied = chat
            .select_apply(AgentId::ROOT, SelectKey::Copy)
            .expect("Enter copies");
        assert_eq!(copied.text, "line 11");
    }

    /// Ctrl-N leaves the mode behind: a cursor over a transcript that is gone is
    /// not a cursor.
    #[test]
    fn a_new_chat_leaves_the_select_mode_behind() {
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::assistant("hi"));
        chat.start_select(AgentId::ROOT);
        chat.clear();
        assert!(!chat.selecting());
    }

    /// A fold is the other road that takes the transcript the mode stands on:
    /// `replace_transcript` drops a mode over the conversation it replaces,
    /// exactly as `clear` drops it for a new chat — a cursor into a transcript
    /// that no longer exists is not a cursor.
    #[test]
    fn a_fold_leaves_the_select_mode_behind() {
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::assistant("first\nsecond"));
        chat.start_select(AgentId::ROOT);
        chat.replace_transcript(AgentId::ROOT, vec![Message::user("a summary")]);
        assert!(!chat.selecting());

        // Another pane's mode is not that fold's to drop: the child's
        // transcript moved, the root's rows did not.
        chat.push_message(AgentId::ROOT, Message::assistant("third"));
        chat.start_select(AgentId::ROOT);
        chat.replace_transcript(AgentId(1), vec![Message::user("the child's line")]);
        assert!(chat.selecting(), "the mode is over the root, not the child");
    }

    /// The frame clamps the mode's cursor exactly as the key road does: a state
    /// left pointing past the transcript — a road that took rows away without
    /// dropping the mode — paints the cursor on a line that exists, and a
    /// transcript with no line left to stand on paints no cursor and no `Enter
    /// copies` clause. No frame may index a row that is not there (D1,
    /// `chat.rs:1768`).
    #[test]
    fn the_frame_clamps_a_cursor_left_past_the_transcript() {
        let mut chat = Chat::bare();
        say(&mut chat, AgentId::ROOT, "first\nsecond");
        assert!(chat.start_select(AgentId::ROOT).is_none());
        // The state the paint road must survive, reached without a key: the
        // cursor names a message and a line the transcript does not have.
        chat.select.as_mut().expect("the mode is on").cursor = (9, 4);

        let pane = pane(AgentId::ROOT);
        let painted = chat.painted(&pane, 40, 8);
        let select = painted
            .select
            .as_ref()
            .expect("the cursor lands on the newest line that still exists");
        assert!(!select.cursor.is_empty());
        assert!(
            select.cursor.iter().all(|at| *at < painted.lines.len()),
            "every painted cursor row is a row the pane has"
        );
        assert!(
            shown(&painted.lines)[select.cursor[0]].contains("second"),
            "and it is the newest line that still exists: {:?}",
            shown(&painted.lines)
        );

        // A transcript with nothing left to stand on: the mode paints as off —
        // no cursor, and no clause promising the copy key.
        chat.root.clear();
        let painted = chat.painted(&pane, 40, 8);
        assert!(painted.select.is_none(), "no cursor over nothing");
        assert!(
            !painted.title.contains("Enter copies"),
            "and no clause promising one: {}",
            painted.title
        );
    }

    /// A transcript belongs to one agent: what a child was told is in the
    /// child's pane, and the root's conversation is not shown to it.
    #[test]
    fn a_transcript_belongs_to_one_agent() {
        let mut chat = Chat::bare();
        say(&mut chat, AgentId::ROOT, "the human's question");
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

        assert!(press(&mut chat, shift), "a modified Enter is an edit");
        assert_eq!(chat.input().text(), "\n");
        assert!(
            !press(&mut chat, key(KeyCode::Enter)),
            "a plain Enter must reach the agents"
        );
        assert!(
            !press(&mut chat, key(KeyCode::Tab)),
            "and so must the pane keys"
        );
    }

    /// Every row of a wrapped message fits the pane, and every word of it is
    /// still there. The voice used to be prepended *after* the text was wrapped
    /// at the pane's whole width, so every row was six (or seven) columns too
    /// wide and ratatui clipped the overflow from the right edge: `w12 w13` gone
    /// from the first row at 80 columns, `w26 w27` from the second, and one more
    /// pair off every row for the life of the message.
    #[test]
    fn a_wrapped_message_keeps_its_tail_at_every_width() {
        let words: Vec<String> = (0..60).map(|index| format!("w{index:02}")).collect();
        let text = words.join(" ");
        for width in [30usize, 40, 60, 80, 120] {
            for message in [Message::user(&text), Message::assistant(&text)] {
                let rows = message_rows(&message, Some(Voice::Human), width, true);
                let painted = shown(&rows);
                for row in &painted {
                    assert!(
                        UnicodeWidthStr::width(row.as_str()) <= width,
                        "a {width}-column pane painted {}: {row:?}",
                        UnicodeWidthStr::width(row.as_str())
                    );
                }
                let flat = painted.join(" ");
                for word in &words {
                    assert!(
                        flat.contains(word.as_str()),
                        "{word} was clipped at {width}: {painted:?}"
                    );
                }
            }
        }
    }

    /// A truncated label says it was truncated. The arguments are budgeted the
    /// columns the `  ⚙ name ` head leaves, so the `…` lands *inside* the pane;
    /// the flat 60 it used to be ignored the head, so on a narrow pane a path
    /// was cut mid-word and the mark that says something was dropped fell past
    /// the border.
    #[test]
    fn a_tool_call_label_truncates_inside_the_pane() {
        let path = "crates/mush/src/app/chat.rs/deeply/nested/module/some/more/directories/and/more/file.rs";
        for width in [24usize, 40, 60, 80, 120] {
            let mut chat = Chat::bare();
            let call = mush_core::ToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: mush_core::FunctionCall {
                    name: "read_file".into(),
                    arguments: format!(r#"{{"path":"{path}"}}"#),
                },
            };
            chat.push_message(
                AgentId::ROOT,
                Message {
                    role: "assistant".into(),
                    tool_calls: Some(vec![call]),
                    ..Default::default()
                },
            );

            let rows = shown(&pane_rows(&chat, &pane(AgentId::ROOT), width, 4));
            let label = rows
                .iter()
                .find(|row| row.contains("⚙"))
                .unwrap_or_else(|| panic!("no label at {width}: {rows:?}"));
            assert!(
                UnicodeWidthStr::width(label.as_str()) <= width,
                "a {width}-column pane painted {}: {label:?}",
                UnicodeWidthStr::width(label.as_str())
            );
            assert!(label.contains("read_file"), "the name stays: {label:?}");
            assert!(
                label.ends_with('…'),
                "a cut path says so: {label:?} at {width}"
            );
        }

        // A path that fits is painted whole, with no mark to explain.
        let mut chat = Chat::bare();
        let call = mush_core::ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: mush_core::FunctionCall {
                name: "read_file".into(),
                arguments: r#"{"path":"src/a.rs"}"#.into(),
            },
        };
        chat.push_message(
            AgentId::ROOT,
            Message {
                role: "assistant".into(),
                tool_calls: Some(vec![call]),
                ..Default::default()
            },
        );
        let rows = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 40, 4));
        assert!(
            rows.iter().any(|row| row == "  ⚙ read_file src/a.rs"),
            "{rows:?}"
        );
    }

    /// A message's pictures are named in the pane whatever said it. A `read_file`
    /// that answers with a png carries it inside the tool *result*, and that
    /// result painted its text and nothing about the picture — so a turn where
    /// the model looked at a screenshot read exactly like one where it did not.
    /// The row is the one the human's own attachment already got: a picture is a
    /// picture whichever side of the turn read it.
    #[test]
    fn a_tool_result_carrying_a_picture_paints_its_row() {
        let mut chat = Chat::bare();
        let read = image("shots/a.png");
        chat.push_message(
            AgentId::ROOT,
            Message::tool_with_images("call_1", "read shots/a.png", vec![read.clone()]),
        );
        let rows = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 60, 8));
        assert!(
            rows.iter()
                .any(|row| *row == format!("  ▣ {}", image_label(&read))),
            "the model's own reading is named: {rows:?}"
        );

        // And the human's picture is still the row it was: one reading, not one
        // per role.
        let sent = image("shots/sent.png");
        chat.push_message(
            AgentId::ROOT,
            Message::user_with_images("look", vec![sent.clone()]),
        );
        let rows = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 60, 10));
        assert!(
            rows.iter()
                .any(|row| *row == format!("  ▣ {}", image_label(&sent))),
            "the human's message is unchanged: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| *row == format!("  ▣ {}", image_label(&read))),
            "and both pictures are in one pane: {rows:?}"
        );
    }

    /// A pane narrower than the voice: the label is what the row cannot afford.
    /// Painting `you › ` into a five-column pane is a row that says who spoke
    /// and nothing else, and a message the human cannot read.
    #[test]
    fn a_pane_narrower_than_the_voice_still_shows_the_words() {
        let rows = message_rows(&Message::user("aaaa bbbb"), Some(Voice::Human), 5, true);
        assert_eq!(
            shown(&rows),
            vec!["aaaa".to_string(), "bbbb".to_string(), String::new()]
        );
    }

    /// The model's own reasoning is painted above the turn it decided, dim, with
    /// the mark on the first row only: the endpoint's `reasoning_content` was
    /// already captured, stored and replayed, and no pane ever showed it.
    #[test]
    fn the_reasoning_is_shown_by_default_above_the_turn_it_decided() {
        let mut chat = Chat::bare();
        assert!(chat.shows_reasoning(), "shown until the human hides it");
        say(&mut chat, AgentId::ROOT, "make it faster");
        chat.push_message(
            AgentId::ROOT,
            thinking("done", "one two three four five six"),
        );

        let rows = pane_rows(&chat, &pane(AgentId::ROOT), 24, 5);
        let painted = shown(&rows);
        assert_eq!(
            painted,
            vec![
                "you › make it faster",
                "",
                "  ⋯ one two three four",
                "    five six",
                "mush › done",
            ]
        );
        // Above the reply it decided, and every row of the block is dim — the
        // continuation row carries the indent and no second mark.
        let mark = rows
            .iter()
            .position(|row| row.to_string().contains('⋯'))
            .unwrap();
        let reply = rows
            .iter()
            .position(|row| row.to_string().contains("mush ›"))
            .unwrap();
        assert!(mark < reply, "the reasoning comes first: {painted:?}");
        for row in &rows[mark..reply] {
            assert!(
                row.spans
                    .iter()
                    .all(|span| span.style.fg == Some(Color::DarkGray)),
                "the block is dim: {row:?}"
            );
        }
        assert_eq!(rows[mark].to_string().matches('⋯').count(), 1, "one mark");
    }

    /// `Ctrl-T` off is the whole block gone, and `Ctrl-N` keeps the choice: a
    /// view the human set is not a fact about the conversation, and a new chat
    /// they cannot read the way they just asked for is a preference the UI
    /// forgot.
    #[test]
    fn hiding_the_reasoning_paints_no_row_and_a_new_chat_keeps_the_choice() {
        let mut chat = Chat::bare();
        say(&mut chat, AgentId::ROOT, "make it faster");
        chat.push_message(AgentId::ROOT, thinking("done", "weighing the words"));
        chat.set_reasoning(false);

        assert!(!chat.shows_reasoning());
        assert_eq!(
            shown(&pane_rows(&chat, &pane(AgentId::ROOT), 24, 5)),
            vec!["you › make it faster", "", "mush › done"]
        );

        chat.clear();
        assert!(
            !chat.shows_reasoning(),
            "the human's view outlives the chat it was set in"
        );
        chat.set_reasoning(true);
        assert!(chat.shows_reasoning());
    }

    /// A reasoning that trims to nothing costs no row at all: DeepSeek really
    /// returns `""` for a reply that did no thinking, and `None` is every other
    /// model's line — a bare `⋯ ` row would spend a row of the pane on most
    /// turns of a long session and say nothing with it.
    #[test]
    fn an_empty_reasoning_paints_no_row_at_all() {
        for reasoning in [None, Some(""), Some("   "), Some("\n\t ")] {
            let mut chat = Chat::bare();
            say(&mut chat, AgentId::ROOT, "make it faster");
            chat.push_message(
                AgentId::ROOT,
                Message {
                    reasoning_content: reasoning.map(str::to_string),
                    ..Message::assistant("done")
                },
            );
            assert_eq!(
                shown(&pane_rows(&chat, &pane(AgentId::ROOT), 24, 3)),
                vec!["you › make it faster", "", "mush › done"],
                "{reasoning:?} painted a row"
            );
        }
    }

    /// A tool-call turn is a turn too, and its reasoning is the only place the
    /// model said why it is calling: the block is painted above the `⚙` rows,
    /// and the turn's empty `content` paints no `mush › ` row above them.
    #[test]
    fn a_tool_call_turn_shows_its_reasoning_above_the_calls() {
        let mut chat = Chat::bare();
        chat.push_message(
            AgentId::ROOT,
            Message {
                role: "assistant".into(),
                reasoning_content: Some("read the file first".into()),
                tool_calls: Some(vec![mush_core::ToolCall {
                    id: "call_1".into(),
                    kind: "function".into(),
                    function: mush_core::FunctionCall {
                        name: "read_file".into(),
                        arguments: r#"{"path":"src/a.rs"}"#.into(),
                    },
                }]),
                ..Default::default()
            },
        );
        assert_eq!(
            shown(&pane_rows(&chat, &pane(AgentId::ROOT), 40, 2)),
            vec!["  ⋯ read the file first", "  ⚙ read_file src/a.rs"]
        );
    }

    /// A message taller than the pane must show its *end*, not its start: the
    /// pane is anchored at the bottom (scroll 0), so the newest rows are the
    /// ones a human is looking for — and with scroll 0 there is no other way to
    /// reach them.
    #[test]
    fn a_message_taller_than_the_pane_shows_its_end() {
        let mut chat = Chat::bare();
        say(&mut chat, AgentId::ROOT, "aaaa bbbb cccc dddd");
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
            say(&mut chat, AgentId::ROOT, &format!("line {index}"));
        }
        let pane = pane(AgentId::ROOT);
        let bottom = pane_rows(&chat, &pane, 40, 3);
        let text: Vec<String> = bottom.iter().map(|l| l.to_string()).collect();
        assert!(text.last().unwrap().contains("line 5"), "{text:?}");

        chat.scroll_by(AgentId::ROOT, 2);
        let scrolled = pane_rows(&chat, &pane, 40, 3);
        assert_eq!(scrolled.len(), 3, "the window is the pane's height");
        assert!(
            scrolled[0].to_string() != text[0],
            "scrolling showed older rows"
        );
    }

    /// A model can write an escape into its reply and a tool result can carry
    /// one out of a log file. Neither reaches the terminal that paints the pane:
    /// the reply that erased the border, the OSC that set the window title and
    /// the CSI that wiped the frame are all just text now — and text that lost
    /// its commands, not text that kept them.
    #[test]
    fn a_pane_paints_no_escape_sequence_and_no_carriage_return() {
        let mut chat = Chat::bare();
        say(&mut chat, AgentId::ROOT, "look at this");
        chat.push_message(
            AgentId::ROOT,
            Message::assistant("and then\rREPLACED \x1b]0;PWNED\x07"),
        );
        chat.push_message(
            AgentId::ROOT,
            Message::tool("call_1", "\x1b[2J\x1b[Hwiped\ttabbed\nsecond row"),
        );
        chat.note_for(AgentId::ROOT, "a note \x1b[31min red\x1b[0m");

        let rows = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 60, 8)).join("\n");
        assert!(!rows.contains('\x1b'), "{rows:?}");
        assert!(!rows.contains('\r'), "{rows:?}");
        assert!(rows.contains("and then␍REPLACED"), "{rows:?}");
        assert!(rows.contains("wiped    tabbed"), "{rows:?}");
        assert!(rows.contains("second row"), "{rows:?}");
        assert!(rows.contains("· a note in red"), "{rows:?}");
    }

    /// The tool-call label is the model's own arguments, and it is painted as one
    /// span rather than wrapped, so it is defanged where it is read: both the
    /// transcript and the tree's row paint `summarize_args`.
    #[test]
    fn a_tool_call_label_carries_no_escape_from_its_arguments() {
        let mut chat = Chat::bare();
        let call = mush_core::ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: mush_core::FunctionCall {
                name: "read_file".into(),
                arguments: r#"{"path":"src/\u001b]0;PWNED\u0007main.rs"}"#.into(),
            },
        };
        chat.push_message(
            AgentId::ROOT,
            Message {
                role: "assistant".into(),
                tool_calls: Some(vec![call]),
                ..Default::default()
            },
        );

        let rows = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 60, 4)).join("\n");
        assert!(!rows.contains('\x1b'), "{rows:?}");
        assert!(rows.contains("⚙ read_file src/"), "{rows:?}");

        // And a command whose argument carries an escape: the label keeps its
        // words and loses the sequence.
        let mut chat = Chat::bare();
        let call = mush_core::ToolCall {
            id: "call_2".into(),
            kind: "function".into(),
            function: mush_core::FunctionCall {
                name: "run_command".into(),
                arguments: r#"{"command":"cat log\u001b[2J\u001b[H"}"#.into(),
            },
        };
        chat.push_message(
            AgentId::ROOT,
            Message {
                role: "assistant".into(),
                tool_calls: Some(vec![call]),
                ..Default::default()
            },
        );
        let rows = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 60, 4)).join("\n");
        assert!(rows.contains("⚙ run_command cat log"), "{rows:?}");
    }

    /// A result that came back `error: …` is not a result. Painting it exactly
    /// like one left the word at the front as the only difference, so a call
    /// that was refused or failed — "this call was not run" — read as work that
    /// happened.
    #[test]
    fn a_failed_tool_result_is_not_painted_as_a_success() {
        let mut chat = Chat::bare();
        chat.push_message(
            AgentId::ROOT,
            Message::tool(
                "call_1",
                "error: this call was not run — the run was stopped as a loop",
            ),
        );
        chat.push_message(AgentId::ROOT, Message::tool("call_2", "wrote 3 lines"));

        let rows = pane_rows(&chat, &pane(AgentId::ROOT), 60, 8);
        let painted = shown(&rows);
        assert!(
            painted.iter().any(|row| row.starts_with("  ! error:")),
            "{painted:?}"
        );
        assert!(
            painted.iter().any(|row| row == "  wrote 3 lines"),
            "{painted:?}"
        );

        let failed = rows
            .iter()
            .find(|line| line.to_string().contains("! error:"))
            .expect("the failed result");
        assert_eq!(
            failed.spans.first().map(|span| span.style.fg),
            Some(Some(Color::Red)),
            "the failure is the red one: {failed:?}"
        );
        let ok = rows
            .iter()
            .find(|line| line.to_string().contains("wrote 3 lines"))
            .expect("the result");
        assert_ne!(
            ok.spans.first().map(|span| span.style.fg),
            Some(Some(Color::Red))
        );
    }

    /// One loop guard writes two lines — the notice it emits as it stops the
    /// run, and the failure the run then ends with — and the pane painted both,
    /// one of them with a red `!`, though nothing the model did failed. One
    /// event is one line, and it is marked as the stop R4 says it is.
    #[test]
    fn a_guard_stop_is_one_line_marked_as_a_stop() {
        let notice = "the run repeated the same tool call 5 times without changing \
                      anything — stopping it as a loop";
        let stop = "the run was stopped as a loop: the same tool call repeated 5 times \
                    with nothing changed in between";
        let mut chat = Chat::bare();
        // The order the two arrive in: the notice as the guard fires, the
        // failure when the run ends a moment later.
        chat.note_for(AgentId::ROOT, notice);
        chat.note_error_for(AgentId::ROOT, stop);

        let texts: Vec<&str> = chat
            .notices_for(AgentId::ROOT)
            .map(|notice| notice.text.as_str())
            .collect();
        assert_eq!(texts, vec![stop], "one event, one line");
        let notice = chat.notices_for(AgentId::ROOT).next().unwrap();
        assert_eq!(notice.kind, NoticeKind::Stopped);
        assert_eq!(
            notice.rank(),
            Rank::Alert,
            "a run that stopped is the thing the human has to read"
        );

        let rows = pane_rows(&chat, &pane(AgentId::ROOT), 70, 7);
        let painted = shown(&rows).join("\n");
        assert!(
            painted.contains("⊘ the run was stopped as a loop"),
            "{painted}"
        );
        let row = rows
            .iter()
            .find(|line| line.to_string().contains("⊘"))
            .expect("the stop line");
        assert_eq!(
            row.spans.first().map(|span| span.style.fg),
            Some(Some(Color::Yellow)),
            "a stop is not a failure and is not painted like one: {row:?}"
        );

        // A real failure is still the red `!` it was, and it takes the agent's
        // older outcome with it: an agent has one *last run*, so it has one line
        // about how that run ended.
        chat.note_error_for(AgentId::ROOT, "no route to host");
        let texts: Vec<&str> = chat
            .notices_for(AgentId::ROOT)
            .map(|notice| notice.text.as_str())
            .collect();
        assert_eq!(texts, vec!["no route to host"]);
    }

    /// A line the human did not say is not painted in the human's voice. Three
    /// kinds of them reach a child's pane: the brief its parent spawned it with,
    /// a parent's steering (a `control` message, the words of which the
    /// human has no other way to see), and mush's own report of a completion it
    /// folded in. All three read `you › …` — the human's words in the human's
    /// mouth — while the human's own nudge to the same agent must keep it.
    #[test]
    fn a_line_the_human_did_not_say_is_not_in_the_human_voice() {
        let mut chat = Chat::bare();
        chat.push_message(AgentId(1), Message::user("create a file called iso.txt"));
        chat.push_message(AgentId(1), Message::user("#1 done: created iso.txt"));
        chat.push_message(AgentId(1), Message::user("keep the steps small"));
        say(&mut chat, AgentId(1), "and add a test");

        let rows = shown(&pane_rows(&chat, &pane(AgentId(1)), 60, 8));
        let painted = rows.join("\n");
        assert!(painted.contains("brief › create a file"), "{rows:?}");
        assert!(painted.contains("· #1 done: created iso.txt"), "{rows:?}");
        assert!(
            painted.contains("parent › keep the steps small"),
            "{rows:?}"
        );
        assert!(painted.contains("you › and add a test"), "{rows:?}");
        assert_eq!(
            rows.iter().filter(|row| row.contains("you ›")).count(),
            1,
            "only the human's own words carry the human's voice: {rows:?}"
        );
    }

    /// The line that says the oldest turns were dropped is mush's, not the
    /// human's: it reaches the pane through the same `Message` road every line
    /// takes, and a human reading `you ›` over it would think they said it —
    /// while the model was told it by mush.
    #[test]
    fn the_dropped_turns_note_reads_as_mushs_line() {
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::user(transcript::DROPPED_TURNS_NOTE));
        say(&mut chat, AgentId::ROOT, "a question of my own");

        let rows = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 60, 12));
        let note_rows: Vec<&String> = rows
            .iter()
            .filter(|row| row.contains("oldest turns"))
            .collect();
        assert!(!note_rows.is_empty(), "the note is painted: {rows:?}");
        assert!(
            note_rows.iter().all(|row| row.starts_with("· ")),
            "in mush's voice, not the human's: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row == "you › a question of my own"),
            "and the human keeps their own: {rows:?}"
        );
    }

    /// The same in the root's pane: mush folds a child's result in as a *user*
    /// message (that is the shape a model reads it in), so the root's transcript
    /// paints `· #1 done: …` — mush's report — and not the human asking
    /// themselves a question.
    #[test]
    fn a_folded_completion_is_mushs_line_in_the_roots_pane() {
        let mut chat = Chat::bare();
        say(&mut chat, AgentId::ROOT, "delegate the parser");
        chat.push_message(AgentId::ROOT, Message::user("#1 done: wrote the parser"));

        let rows = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 60, 6));
        assert!(
            rows.iter().any(|row| row == "· #1 done: wrote the parser"),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row == "you › delegate the parser"),
            "{rows:?}"
        );
    }

    /// A transcript restored from the session file arrives without its voices,
    /// so the pane reads what the lines themselves say: a fold's carried summary
    /// is mush's line, a child's first line is the brief it was spawned with,
    /// and everything else is the human — which is what most of a transcript is,
    /// and the panel recusing itself from the human's own question would be the
    /// louder lie.
    #[test]
    fn a_restored_transcript_reads_its_own_lines() {
        let mut chat = Chat::bare();
        chat.replace_transcript(
            AgentId::ROOT,
            vec![
                Message::user("port the parser"),
                Message::user("Context compacted — continue the task from this summary:\ndid it"),
                Message::user("#c2 done: exit 0 · 3m12s · cargo test"),
                Message::assistant("still here"),
            ],
        );
        chat.replace_transcript(
            AgentId(1),
            vec![
                Message::user("write the lexer tests"),
                Message::user("and make them fast"),
            ],
        );

        let root = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 70, 10)).join("\n");
        assert!(root.contains("you › port the parser"), "{root}");
        assert!(root.contains("· Context compacted"), "{root}");
        assert!(root.contains("· #c2 done: exit 0"), "{root}");

        let child = shown(&pane_rows(&chat, &pane(AgentId(1)), 70, 6)).join("\n");
        assert!(child.contains("brief › write the lexer tests"), "{child}");
        assert!(
            child.contains("you › and make them fast"),
            "a mid-transcript line nothing distinguishes reads as the human's: {child}"
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
            words: Phase::Activity("run_command cargo test".to_string()).words(),
            ..pane(AgentId(1))
        };

        let painted = chat.painted(&busy, 40, 4);
        let rows = shown(&painted.lines);
        assert!(
            rows.iter().any(|row| row.contains("no route to host")),
            "the failure is never the line the cap gives up: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("run_command cargo test.")),
            "and the run in flight is still shown, in its own words, ranked below the failure: \
             {rows:?}"
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

    /// The foot paints the phase's own words plus the beat's dots — the whole
    /// line, at every phase that has something to say and every dot it can
    /// wear: `thinking.`, `thinking..`, `thinking...`, a tool call's own label,
    /// `waiting on results.` for a run parked in a `wait`, the fold's sentence,
    /// `cancelling.`. The word is `Phase::words`, the same one the row paints,
    /// so the two surfaces cannot spell one phase two ways.
    #[test]
    fn the_foot_paints_the_phases_words_and_the_beats_dots() {
        let chat = Chat::bare();
        for (phase, words) in [
            (Phase::Thinking, "thinking"),
            (
                Phase::Activity("run_command cargo test".to_string()),
                "run_command cargo test",
            ),
            (Phase::Activity("wait ".to_string()), "waiting on results"),
            (
                Phase::Compacting(Compacting::Parked),
                "folding at the next step",
            ),
            (Phase::Compacting(Compacting::Requested), "compacting"),
            (
                Phase::Compacting(Compacting::NearlyFull),
                "context nearly full — compacting",
            ),
            (Phase::Cancelling, "cancelling"),
        ] {
            assert_eq!(
                phase.words().as_deref(),
                Some(words),
                "{phase:?} is its own words"
            );
            for (beat, dots) in [(0u64, "."), (1, ".."), (2, "...")] {
                let pane = Pane {
                    words: phase.words(),
                    spin: beat,
                    ..pane(AgentId::ROOT)
                };
                let line = format!("{words}{dots}");
                let rows = shown(&chat.painted(&pane, 40, 4).lines);
                assert!(
                    rows.iter().any(|row| row == &line),
                    "{phase:?} at beat {beat} must paint exactly {line:?}: {rows:?}"
                );
            }
        }
    }

    /// The animation did not move into one phase's line when the line became
    /// the phase's own: the beat is the pane's, the third dot loops back to the
    /// first, and a run that changes tools keeps its dot.
    #[test]
    fn the_dots_count_on_every_word() {
        assert_eq!(dotted("thinking", 0), "thinking.");
        assert_eq!(dotted("thinking", 1), "thinking..");
        assert_eq!(dotted("thinking", 2), "thinking...");
        assert_eq!(dotted("thinking", 3), "thinking.");
        assert_eq!(
            dotted("run_command cargo test", 1),
            "run_command cargo test.."
        );
    }

    /// A phase at rest paints no foot line: `None` is not an empty dot line,
    /// and the pane says nothing about a run that is not in flight. The rows
    /// are the transcript's alone.
    #[test]
    fn the_foot_paints_nothing_at_rest() {
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::assistant("the reply"));
        for phase in [
            Phase::Idle,
            Phase::Done,
            Phase::Failed("no route to host".to_string()),
            Phase::Stopped,
            Phase::CutOff,
        ] {
            assert_eq!(phase.words(), None, "{phase:?} is at rest");
            let pane = Pane {
                words: phase.words(),
                ..pane(AgentId::ROOT)
            };
            assert_eq!(
                shown(&chat.painted(&pane, 40, 4).lines),
                vec!["mush › the reply"],
                "{phase:?} must not paint a foot line"
            );
        }
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

        // One row of pane: the transcript keeps it — the foot takes none — and
        // the title carries the count, because the foot has no row to spend on
        // saying it.
        let painted = chat.painted(&pane, 40, 1);
        assert_eq!(shown(&painted.lines), vec!["mush › the newest reply"]);
        assert_eq!(painted.title, " mush · +5 more lines · /notes ");

        // Two rows: the conversation and one line of mush's own — five note
        // lines were written and the other four are counted in the title,
        // because a foot one row tall is worth more as a line than as a sum of
        // the lines it is not showing.
        let painted = chat.painted(&pane, 40, 2);
        assert_eq!(
            shown(&painted.lines),
            vec!["mush › the newest reply", "· note 4"]
        );
        assert_eq!(painted.title, " mush · +4 more lines · /notes ");

        // Three rows: the count has a row of its own now, so both it and the
        // newest line are painted.
        let painted = chat.painted(&pane, 40, 3);
        assert_eq!(
            shown(&painted.lines),
            vec![
                "mush › the newest reply",
                "  +4 more lines · /notes",
                "· note 4"
            ]
        );
        assert_eq!(painted.title, " mush ");

        // Nothing said yet is the other case: there is no transcript row to
        // protect, and the single row a 40×10 pane has goes to mush's own line
        // rather than to a blank or to arithmetic about it. A fresh pane with a
        // note in it used to paint exactly that blank.
        let mut fresh = Chat::bare();
        for index in 0..5 {
            fresh.note_for(AgentId::ROOT, format!("note {index}"));
        }
        let painted = fresh.painted(&pane, 40, 1);
        assert_eq!(
            shown(&painted.lines),
            vec!["· note 4"],
            "the newest line, not the count of the lines above it"
        );
        assert_eq!(painted.title, " mush · +4 more lines · /notes ");
    }

    /// The derived activity line is not a note: `/notes` cannot show it, so it
    /// must not be counted as one. A run in flight with nothing written about
    /// it used to title the pane `+1 more lines` and then answer "nothing
    /// written about #0 yet" — the screen promising a reading it could not
    /// give.
    #[test]
    fn the_activity_row_never_inflates_the_count() {
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::assistant("the newest reply"));
        let busy = Pane {
            words: Phase::Activity("edit_file src/lib.rs".to_string()).words(),
            ..pane(AgentId::ROOT)
        };

        // One row of pane, and no notes: the transcript keeps the row, the
        // hidden activity line is not a line something wrote, and the title
        // claims nothing.
        let painted = chat.painted(&busy, 40, 1);
        assert_eq!(shown(&painted.lines), vec!["mush › the newest reply"]);
        assert_eq!(painted.title, " mush ", "{:?}", painted.title);
        assert!(
            chat.notes_report(AgentId::ROOT, 0, 34).rows.is_empty(),
            "and there is in fact nothing to read"
        );

        // With one note the count is that note and only that note, whether the
        // activity line is shown beside it or not.
        chat.note_for(AgentId::ROOT, "reading the lexer");
        let painted = chat.painted(&busy, 40, 1);
        assert_eq!(painted.title, " mush · +1 more lines · /notes ");
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
        let rows = chat.notes_report(AgentId(1), at, 20).rows;
        assert!(rows.len() > 2, "the long line wraps: {rows:?}");
        assert!(rows[0].starts_with("0s · could not"), "{:?}", rows[0]);
        assert!(
            rows[1].starts_with("     "),
            "a continuation lines up under the text, not under the stamp: {:?}",
            rows[1]
        );
        assert!(
            rows.iter().any(|row| row.starts_with("0s ! no route to")),
            "the failure is in the same list, marked: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.trim_start().starts_with("host")),
            "and wrapped whole instead of being clipped: {rows:?}"
        );
        assert!(
            chat.notes_report(AgentId(2), at, 20).rows.is_empty(),
            "and it is one agent's list, not every agent's (finding B19)"
        );
    }

    /// The report is wrapped for the width the list is actually painted at, so
    /// a row — the age and marker that lead it included — never overruns the
    /// list and is clipped. The old test wrapped at a fixed 74 and never at the
    /// painted width, which is exactly why the first row clipped on the widest
    /// popup (74 + the lead) and every row clipped below 80 columns.
    #[test]
    fn the_notes_report_fits_the_width_it_is_given() {
        use unicode_width::UnicodeWidthStr;

        let mut chat = Chat::bare();
        chat.note_for(
            AgentId(1),
            "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo \
             lima mike november oscar papa quebec romeo sierra tango",
        );
        let at = chat.notices_for(AgentId(1)).next().expect("the note").at;

        // 34 and 74 are the popup's own content widths at the two ends of
        // `ui::picker_text_width` (a 40-column terminal and an 80-column one),
        // which is what makes this the real painted width and not a stand-in.
        for width in [34usize, 46, 74] {
            let rows = chat.notes_report(AgentId(1), at, width).rows;
            assert!(
                rows.iter().any(|row| row.contains("tango")),
                "the tail of the note survives at {width}: {rows:?}"
            );
            for row in &rows {
                assert!(
                    UnicodeWidthStr::width(row.as_str()) <= width,
                    "a row is wider than the list at {width}: {row:?}"
                );
            }
        }
    }

    /// A revision is process-monotone: it is a token a client holds across
    /// turns, so a new chat moves it *forward* instead of resetting it.
    /// Restarting at 0 made a new conversation collide with a transcript the
    /// client had already read, and its next edit landed as if nothing had
    /// happened (finding A1).
    #[test]
    fn a_revision_never_steps_back_across_new_transcripts() {
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::user("first"));
        chat.replace_transcript(AgentId(2), vec![Message::user("a child line")]);
        let root_before = chat.revision(AgentId::ROOT);
        let child_before = chat.revision(AgentId(2));

        chat.clear();
        assert!(
            chat.revision(AgentId::ROOT) > root_before,
            "a new chat steps the root forward: {}",
            chat.revision(AgentId::ROOT)
        );
        assert!(
            chat.revision(AgentId(2)) > child_before,
            "and every conversation the client could hold"
        );

        // A conversation nobody had changed is a token too: an empty transcript
        // reads as revision 0, and Ctrl-N must not hand that same 0 back for a
        // different (also empty) one — the case a client polls into.
        let mut empty = Chat::bare();
        let zero = empty.revision(AgentId::ROOT);
        empty.clear();
        assert!(
            empty.revision(AgentId::ROOT) > zero,
            "even an empty conversation's token moves on"
        );
    }

    /// Forgetting one agent — the child the history window reaped (§8.21) —
    /// drops every map this conversation keys by its id, and nothing else: the
    /// transcript, the voices keyed by line, the revision an attach client
    /// edits against, the pane's own reading position, and the lines mush wrote
    /// about it.
    #[test]
    fn forgetting_an_agent_drops_only_its_own_entries() {
        let mut chat = Chat::bare();
        say(&mut chat, AgentId(1), "port the parser");
        chat.push_message(AgentId(1), Message::user("#1 done: did it"));
        chat.note_error_for(AgentId(1), "boom");
        chat.scroll_by(AgentId(1), 2);
        say(&mut chat, AgentId(2), "port the lexer");
        chat.note_error_for(AgentId(2), "the lexer's own failure");
        assert!(chat.spoken.contains_key(&AgentId(1)));
        assert!(chat.reading.contains_key(&AgentId(1)));

        chat.forget(AgentId(1));

        assert!(!chat.agents.contains_key(&AgentId(1)));
        assert!(!chat.spoken.contains_key(&AgentId(1)));
        assert!(!chat.revisions.contains_key(&AgentId(1)));
        assert!(!chat.reading.contains_key(&AgentId(1)));
        assert!(
            !chat.notices.iter().any(|notice| notice.agent == AgentId(1)),
            "a `!` line about an agent whose pane cannot be opened is not a line"
        );
        assert!(chat.transcript(AgentId(1)).is_empty());

        // The sibling is untouched, entry for entry.
        assert_eq!(chat.transcript(AgentId(2))[0].text(), "port the lexer");
        assert!(chat.notices.iter().any(|notice| notice.agent == AgentId(2)));
        assert_eq!(chat.revision(AgentId(2)), 1);
    }

    /// Clearing is per agent, because a line about one conversation is not a
    /// line about another (finding B19).
    #[test]
    fn clearing_notes_is_one_agent_at_a_time() {
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
    }

    /// One line said five times is one line with a count: an empty reply on five
    /// consecutive turns is one fact about the run, and printing it five times
    /// spent the foot — the rows the conversation was supposed to have — on one
    /// sentence (finding U8).
    #[test]
    fn a_line_said_again_and_again_collapses_into_one_with_a_count() {
        let mut chat = Chat::bare();
        for _ in 0..5 {
            chat.note_for(AgentId::ROOT, "model produced an empty reply");
        }
        let notices: Vec<&Notice> = chat.notices_for(AgentId::ROOT).collect();
        assert_eq!(notices.len(), 1, "five turns, one line");
        assert_eq!(notices[0].count, 5, "and it says how many");
        // The count is not a field for a test's benefit: it is what the pane
        // and `/notes` read, and it must not hide that it happened five times.
        let at = notices[0].at;
        assert_eq!(
            chat.notes_report(AgentId::ROOT, at, 40).rows,
            vec!["0s · model produced an empty reply ×5".to_string()]
        );
        let lines = chat.painted(&pane(AgentId::ROOT), 40, 4).lines;
        assert!(
            shown(&lines).join("\n").contains("×5"),
            "the foot says it once, with the count: {:?}",
            shown(&lines)
        );

        // A different line in between means two moments, not repeats: the
        // collapse is about the same line *in a row*.
        chat.note_for(AgentId::ROOT, "could not compact");
        chat.note_for(AgentId::ROOT, "model produced an empty reply");
        assert_eq!(
            chat.notices_for(AgentId::ROOT).count(),
            3,
            "the same words after another line are a new line"
        );
    }

    /// A failure is the run's record and outlives the moment; a hint is the
    /// moment, and the human's next act ends it. `/notes` reads the same list,
    /// so a line that has ended is gone from there too — one answer to "what
    /// does this pane still have to say" (finding U8).
    #[test]
    fn the_human_s_next_act_ends_the_chatter_and_keeps_the_failure() {
        let mut chat = Chat::bare();
        chat.note_for(AgentId(1), "opened notes.txt");
        chat.note_for(AgentId::ROOT, "merged mush/1 into HEAD");
        chat.note_error_for(AgentId(1), "no route to host");

        assert!(chat.dismiss_said(), "there was chatter to end");
        assert_eq!(
            chat.notices_for(AgentId(1))
                .map(|notice| notice.text.as_str())
                .collect::<Vec<_>>(),
            vec!["no route to host"],
            "the child's failure is not the child's chatter"
        );
        assert_eq!(
            chat.notices_for(AgentId::ROOT).count(),
            0,
            "and every agent's chatter goes, not only the focused one's"
        );
        assert!(!chat.dismiss_said(), "and there is nothing left to end");
        assert_eq!(
            chat.stored_notices().len(),
            1,
            "the failure is still the line the session keeps"
        );
    }

    /// The clock is the fallback for a human who does nothing: `SAID_TTL` ends
    /// the chatter with no key pressed at all, which is the half of the finding
    /// that neither the next run nor the next send covers. A failure has no
    /// such clock — the bar keeps its red line until something replaces it, and
    /// so does the pane.
    #[test]
    fn chatter_expires_on_its_clock_and_a_failure_does_not() {
        let mut chat = Chat::bare();
        chat.note_for(AgentId::ROOT, "reply cut off at 20480 tokens");
        chat.note_error_for(AgentId::ROOT, "no route to host");
        let now = chat.notices_for(AgentId::ROOT).next().expect("the hint").at;

        assert!(
            !chat.expire_said(now + SAID_TTL - 1),
            "a line still inside its moment is still there"
        );
        assert_eq!(chat.notices_for(AgentId::ROOT).count(), 2);
        assert!(chat.expire_said(now + SAID_TTL), "and then it is not");
        assert_eq!(
            chat.notices_for(AgentId::ROOT)
                .map(|notice| notice.text.as_str())
                .collect::<Vec<_>>(),
            vec!["no route to host"],
            "the failure outlives any clock"
        );
        assert!(
            !chat.expire_said(u64::MAX),
            "and no later moment takes it either"
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
            chat.system().weight() / mush_core::config::BYTES_PER_TOKEN,
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
            (chat.system().weight() + summary.weight()) / mush_core::config::BYTES_PER_TOKEN,
            "the meter reads what is there now"
        );

        // A subagent's transcript is not the root's conversation, so it does
        // not weigh on it.
        chat.push_message(AgentId(1), Message::assistant("x".repeat(1000)));
        assert_eq!(
            chat.used_tokens_for(AgentId::ROOT),
            (chat.system().weight() + summary.weight()) / mush_core::config::BYTES_PER_TOKEN
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

        assert!(press(&mut chat, key(KeyCode::Backspace)));
        assert_eq!(
            chat.input().text(),
            "x\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f466}"
        );
        assert!(press(&mut chat, key(KeyCode::Backspace)));
        assert_eq!(chat.input().text(), "x", "the whole emoji went at once");

        // And the cursor is where the typing goes, not only where it can be
        // deleted from: after moving left, the character is inserted before `x`.
        assert!(press(&mut chat, key(KeyCode::Left)));
        assert!(press(&mut chat, key(KeyCode::Char('A'))));
        assert_eq!(chat.input().text(), "Ax");
    }

    /// Backspace takes the thing immediately before the cursor, and at the very
    /// start of the box that is the newest attachment — the pictures are
    /// painted above the words. The key that did nothing there while the box
    /// held text now takes the picture overhead.
    #[test]
    fn backspace_at_the_boxes_start_pops_the_newest_attachment() {
        let mut chat = Chat::bare();
        chat.attach(image("a.png"));
        chat.attach(image("b.png"));
        chat.insert("words");
        assert!(
            press(&mut chat, key(KeyCode::Home)),
            "Home is the box's start"
        );

        // The newest image goes; the words are untouched.
        press(&mut chat, key(KeyCode::Backspace));
        assert_eq!(chat.attachments().len(), 1);
        assert_eq!(chat.attachments()[0].path, "a.png", "the newest goes first");
        assert_eq!(chat.input().text(), "words");

        // A second press takes the next one; a third, with none left, leaves
        // the box alone — at index 0 plain backspace has nothing to delete.
        press(&mut chat, key(KeyCode::Backspace));
        assert!(chat.attachments().is_empty());
        press(&mut chat, key(KeyCode::Backspace));
        assert_eq!(chat.input().text(), "words", "the words stay");

        // From inside the text the key is the text's, as it always was.
        press(&mut chat, key(KeyCode::End));
        press(&mut chat, key(KeyCode::Backspace));
        assert_eq!(chat.input().text(), "word");
    }

    /// Ctrl-U clears the words and keeps the images: the images are not what the
    /// key is about, and Esc is still the one key that takes both.
    #[test]
    fn ctrl_u_clears_the_words_and_keeps_the_images() {
        let mut chat = Chat::bare();
        chat.attach(image("shot.png"));
        chat.insert("a draft");

        assert!(press(&mut chat, ctrl('u')));
        assert_eq!(chat.input().text(), "");
        assert_eq!(chat.attachments().len(), 1, "the image stays");
        assert_eq!(chat.attachments()[0].path, "shot.png");

        // The images' rows are still the box's to paint, and Esc takes them.
        chat.insert("again");
        press(&mut chat, key(KeyCode::Esc));
        assert_eq!(chat.input().text(), "");
        assert!(chat.attachments().is_empty(), "Esc clears both");
    }

    /// Ctrl-Z puts back what the box last lost, on each of the three roads: Esc
    /// takes the words and the images, a pop takes one image, Ctrl-U takes the
    /// words — and each key restores exactly that.
    #[test]
    fn ctrl_z_puts_back_what_the_box_lost_each_way_it_can_be_lost() {
        // Esc: both halves go and both come back.
        let mut chat = Chat::bare();
        chat.attach(image("a.png"));
        chat.attach(image("b.png"));
        chat.insert("draft");
        press(&mut chat, key(KeyCode::Esc));
        assert!(chat.input().is_empty() && chat.attachments().is_empty());
        press(&mut chat, ctrl('z'));
        assert_eq!(chat.input().text(), "draft");
        assert_eq!(chat.attachments().len(), 2, "both images are back");

        // A pop: the image the key took is back, newest again; the words, which
        // never went, are still there.
        let mut chat = Chat::bare();
        chat.attach(image("a.png"));
        chat.attach(image("b.png"));
        chat.insert("words");
        press(&mut chat, key(KeyCode::Home));
        press(&mut chat, key(KeyCode::Backspace));
        assert_eq!(chat.attachments().len(), 1);
        press(&mut chat, ctrl('z'));
        assert_eq!(chat.attachments().len(), 2);
        assert_eq!(chat.attachments()[1].path, "b.png");
        assert_eq!(chat.input().text(), "words");

        // Ctrl-U: the words go round the trip; the image never leaves the box.
        let mut chat = Chat::bare();
        chat.attach(image("a.png"));
        chat.insert("words");
        press(&mut chat, ctrl('u'));
        press(&mut chat, ctrl('z'));
        assert_eq!(chat.input().text(), "words");
        assert_eq!(chat.attachments().len(), 1, "the image never left");
    }

    /// Ctrl-Z with nothing lost does nothing — and the slot is spent by a
    /// restore: one slot, not a stack.
    #[test]
    fn ctrl_z_with_nothing_to_put_back_does_nothing() {
        let mut chat = Chat::bare();
        press(&mut chat, ctrl('z'));
        assert!(chat.input().is_empty() && chat.attachments().is_empty());

        chat.insert("typed");
        press(&mut chat, key(KeyCode::Esc));
        press(&mut chat, ctrl('z'));
        assert_eq!(chat.input().text(), "typed");
        press(&mut chat, ctrl('z'));
        assert_eq!(
            chat.input().text(),
            "typed",
            "the first restore spent the slot"
        );
    }

    /// A send drops the slot ([`Chat::forget_lost`], which `send_message`
    /// calls): the message the human sent must not come back on a keystroke,
    /// even when an earlier loss is what the slot was holding.
    #[test]
    fn a_sent_draft_does_not_come_back() {
        let mut chat = Chat::bare();
        chat.insert("lost");
        press(&mut chat, key(KeyCode::Esc));
        chat.insert("sent");
        assert_eq!(chat.take_input(), "sent");
        chat.forget_lost();
        press(&mut chat, ctrl('z'));
        assert_eq!(chat.input().text(), "", "the sent words stay sent");
    }

    /// A new chat drops the slot too: a draft from the conversation that just
    /// went does not land in the next one on a keystroke.
    #[test]
    fn a_new_chat_drops_the_slot() {
        let mut chat = Chat::bare();
        chat.insert("before Ctrl-N");
        press(&mut chat, key(KeyCode::Esc));
        chat.clear();
        press(&mut chat, ctrl('z'));
        assert_eq!(chat.input().text(), "");
    }

    /// The words Esc leaves on the bar name what went and the one key that puts
    /// it back — and an empty box is not a loss, so it says nothing at all.
    #[test]
    fn esc_says_what_it_took_and_the_way_back() {
        let cleared = |chat: &mut Chat| chat.apply(AgentId::ROOT, ChatKey::Clear);

        let mut chat = Chat::bare();
        chat.attach(image("a.png"));
        chat.attach(image("b.png"));
        chat.insert("draft");
        assert_eq!(
            cleared(&mut chat).as_deref(),
            Some("cleared the box and 2 images · Ctrl-Z puts it back")
        );

        let mut chat = Chat::bare();
        chat.insert("draft");
        assert_eq!(
            cleared(&mut chat).as_deref(),
            Some("cleared the box · Ctrl-Z puts it back")
        );

        let mut chat = Chat::bare();
        chat.attach(image("a.png"));
        assert_eq!(
            cleared(&mut chat).as_deref(),
            Some("cleared 1 image · Ctrl-Z puts it back")
        );

        let mut chat = Chat::bare();
        assert_eq!(cleared(&mut chat), None, "an empty box is not a loss");
    }

    /// The model's reply is the pane's one document: its markdown is read as a
    /// view — the `#`s, the asterisks and the fence lines are not painted — and
    /// the message's own bytes are left exactly as the model wrote them. What a
    /// human copies out of a reply is the source, so nothing here rewrites the
    /// transcript; the view only decides how one frame paints it.
    #[test]
    fn a_reply_is_read_as_markdown_and_its_bytes_are_left_alone() {
        let source = "# Steps\n\n- **run** `cargo test`\n\nsee [the docs](https://example.com/a)";
        let message = Message::assistant(source);
        let mut rows = Vec::new();
        render_message(&mut rows, &message, None, 60, false, Fold::DEFAULT);
        assert_eq!(
            shown(&rows),
            vec![
                "mush › Steps".to_string(),
                "       ".to_string(),
                "       - run cargo test".to_string(),
                "       ".to_string(),
                "       see the docs (https://example.com/a)".to_string(),
                String::new(),
            ]
        );
        assert_eq!(
            message.text(),
            source,
            "the transcript keeps the source bytes"
        );

        // The view's palette is decided in one place: a heading in the reply's
        // accent and bold, a strong span bold, a code span the dim grey.
        let heading = &rows[0].spans[1];
        assert_eq!(heading.style.fg, Some(Color::Green));
        assert!(heading
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
        let strong = rows[2]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "run")
            .expect("the strong span");
        assert!(strong
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
        let code = rows[2]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "cargo test")
            .expect("the code span");
        assert_eq!(code.style.fg, Some(Color::DarkGray));
    }

    /// A tool result is data, not prose: its bytes are what the human copies
    /// out — a diff, a test log, a shell transcript — so the view does not
    /// touch it. A `#` in such a line is a comment, an `*` is a glob and
    /// backticks are quoting; the rows below are byte for byte the rows the
    /// plain wrapper painted before there was a view.
    #[test]
    fn a_tool_result_is_painted_byte_for_byte_as_data() {
        let text =
            "# not a heading\n\n- **not strong** `not code`\n\n[not a link](https://example.com/a)";
        let mut rows = Vec::new();
        render_message(
            &mut rows,
            &Message::tool("call_1", text),
            None,
            60,
            false,
            Fold::DEFAULT,
        );
        assert_eq!(
            shown(&rows),
            vec![
                "  # not a heading".to_string(),
                "  ".to_string(),
                "  - **not strong** `not code`".to_string(),
                "  ".to_string(),
                "  [not a link](https://example.com/a)".to_string(),
                String::new(),
            ]
        );
    }

    /// Only the model's reply is a document. The human's own message, a child's
    /// brief and mush's own notices keep the plain path byte for byte: a `#` in
    /// them is not a heading and an `*` is not emphasis, because they are lines
    /// somebody said and not prose to be rendered.
    #[test]
    fn only_the_reply_is_read_as_markdown() {
        let source = "# **bold** `code` [link](https://x.dev/a)";

        let mut rows = Vec::new();
        render_message(
            &mut rows,
            &Message::user(source),
            Some(Voice::Human),
            80,
            false,
            Fold::DEFAULT,
        );
        assert_eq!(shown(&rows)[0], format!("you › {source}"));

        let mut chat = Chat::bare();
        chat.push_message(AgentId(1), Message::user(source));
        let painted = shown(&pane_rows(&chat, &pane(AgentId(1)), 80, 3));
        assert_eq!(painted[0], format!("brief › {source}"));

        let notice = Notice {
            agent: AgentId::ROOT,
            kind: NoticeKind::Info,
            at: 0,
            count: 1,
            text: source.to_string(),
        };
        assert_eq!(
            shown(&footnote_lines(&notice, 80))[0],
            format!("· {source}")
        );
    }

    /// A rendered reply is a view and not a layout change: at every width the
    /// pane paints no row wider than itself, and every word of the reply is
    /// still there. A CJK glyph is two columns and a URL has no space to break
    /// at, and both are where a naive render pushes past the edge.
    #[test]
    fn a_markdown_reply_never_paints_past_the_pane() {
        let text = "# 見出し\n\n- **項目** one two\n\nsee [the docs](https://example.com/a/very/long/path/that/never/breaks/anywhere)\n\n```\nlet x = 1; // 日本語\n```\n\n**strong** tail";
        let message = Message::assistant(text);
        for width in [10usize, 12, 14, 20, 33, 40, 80] {
            let mut rows = Vec::new();
            render_message(&mut rows, &message, None, width, false, Fold::DEFAULT);
            let painted = shown(&rows);
            for row in &painted {
                assert!(
                    UnicodeWidthStr::width(row.as_str()) <= width,
                    "a {width}-column pane painted {}: {row:?}",
                    UnicodeWidthStr::width(row.as_str())
                );
            }
            // The view's margin on a wrapped row is the mark's own columns;
            // strip it so the check reads the reply's words and not the
            // layout the pane wraps every voice in.
            let lead = if width >= "mush › ".width() + MIN_BODY {
                "mush › ".width()
            } else {
                0
            };
            let flat: String = painted
                .iter()
                .map(|row| row.strip_prefix(&" ".repeat(lead)).unwrap_or(row))
                .collect::<Vec<_>>()
                .concat();
            assert!(
                flat.contains("見出し"),
                "the heading's words stay at {width}: {painted:?}"
            );
            assert!(
                flat.contains("項目"),
                "the item's words stay at {width}: {painted:?}"
            );
            assert!(
                flat.contains("https://example.com/a/very/long/path/that/never/breaks/anywhere"),
                "the URL is never dropped at {width}: {painted:?}"
            );
            assert!(
                !painted
                    .iter()
                    .any(|row| row.contains("**") || row.contains("```")),
                "no marker survives the view at {width}: {painted:?}"
            );
        }
    }

    /// Every multi-line block a pane writes goes through the fold, each kind
    /// with its own number: a tool result to eight rows, the reasoning to its
    /// own slot, which is `usize::MAX` — shown whole, because that is the text
    /// the human pressed `Ctrl-T` to read, and folded the day a setting lowers
    /// the number. The report and the brief the `user` arm paints join this
    /// list with that arm's own commit.
    ///
    /// A new kind is a compile-time question, not a silent omission: [`Kind`]'s
    /// table is exactly [`Kind::COUNT`] long and [`Kind::slot`] is a `match`
    /// with no wildcard, so a variant added without a number stops the crate
    /// from building.
    #[test]
    fn every_multi_line_block_the_pane_writes_goes_through_the_fold() {
        let many = (0..25)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let fold = Fold::DEFAULT;
        assert_eq!(fold.shown(Kind::Result, &many), 8);
        assert_eq!(
            fold.shown(Kind::Reasoning, &many),
            usize::MAX,
            "a thought is shown whole until a setting says otherwise"
        );

        // A result paints eight rows and the `…` — ten rows with the blank.
        let result = message_rows(&Message::tool("call_1", &many), None, 60, true);
        let rows = shown(&result);
        assert_eq!(rows.len(), 10, "eight rows and the `…`: {rows:?}");
        assert!(
            rows[8].ends_with("… +17 more lines"),
            "the ninth row names what is hidden: {rows:?}"
        );
        assert_eq!(rows[9], "", "and the blank closes the message");
        assert_eq!(rows[0], "  line 0", "the result's own indent");

        // The reasoning is whole today — every row, no `…`...
        let thought = thinking("", &many);
        let whole = message_rows(&thought, None, 60, true);
        assert_eq!(shown(&whole).len(), 26, "25 rows and the blank");
        assert!(
            !shown(&whole).iter().any(|row| row.contains('…')),
            "nothing is elided: {:?}",
            shown(&whole)
        );
        // ...and the slot is real: a lower number folds it like any other block.
        let folded = message_rows_under(&thought, None, 60, true, fold.with(Kind::Reasoning, 3));
        assert_eq!(
            shown(&folded),
            vec![
                "  ⋯ line 0".to_string(),
                "    line 1".to_string(),
                "    line 2".to_string(),
                "    … +22 more lines".to_string(),
                String::new(),
            ]
        );
    }

    /// A block that reports a failure is kept even where the fold would hide
    /// it: the foot's cap already refuses to drop the failure row ("the failure
    /// is never the line the cap gives up"), and the fold holds the same rule —
    /// it lives on [`Fold`], not in an arm, so a `0`-rows setting still paints
    /// the `! error: …` result's own row.
    #[test]
    fn a_failure_is_never_what_the_fold_gives_up() {
        let zero = Fold::DEFAULT.with(Kind::Result, 0);

        // A failed result: the failure row stays, and the log it came with is
        // what the fold gives up.
        let failed = "error: the call was refused\nthe log line one\nthe log line two";
        let rows = shown(&message_rows_under(
            &Message::tool("call_1", failed),
            None,
            60,
            true,
            zero,
        ));
        assert_eq!(
            rows,
            vec![
                "  ! error: the call was refused".to_string(),
                "    … +2 more lines".to_string(),
                String::new(),
            ]
        );

        // The same text as a *success* vanishes into its `…` at 0: the
        // exemption is the block's, not the setting's.
        let ok = "wrote three lines\nthe log line one\nthe log line two";
        let rows = shown(&message_rows_under(
            &Message::tool("call_1", ok),
            None,
            60,
            true,
            zero,
        ));
        assert_eq!(rows, vec!["  … +3 more lines".to_string(), String::new()]);
    }

    /// The two blocks the fold never touches, whatever it says: the human's own
    /// lines, because their words are theirs however long, and the model's
    /// reply, because it is the conversation's own text. A `0`-rows setting for
    /// every kind the fold owns leaves both whole.
    #[test]
    fn the_humans_lines_and_the_reply_are_never_folded() {
        let many = (0..25)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let zero = Fold::DEFAULT.with(Kind::Result, 0).with(Kind::Reasoning, 0);

        let human = message_rows_under(&Message::user(&many), Some(Voice::Human), 60, true, zero);
        assert_eq!(
            shown(&human).len(),
            26,
            "the human's 25 lines and the blank, in full"
        );
        assert_eq!(shown(&human)[0], "you › line 0");

        let reply = message_rows_under(&Message::assistant(&many), None, 60, true, zero);
        assert_eq!(
            shown(&reply).len(),
            26,
            "the reply's 25 lines and the blank, in full"
        );
        assert_eq!(shown(&reply)[0], "mush › line 0");
    }
}
