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
//! stopped — belongs to its run: a new one replaces the agent's old one, and no
//! clock takes either away. Only the failure is the half written to the
//! session, because that is the line a restart owes; a stop is news on the run
//! and stays on the row, and what a restart reads is the *status* the run
//! stored — a stop the human asked for comes back as the row's `Phase::Stopped`,
//! and one the loop guard ended comes back as that run's error, the guard's own
//! words. Neither returns as a stored `!` line: a run where nothing broke must
//! not come back wearing a failure's red mark. Before this, every notice ever
//! written stayed until Ctrl-N, a failure from twenty runs ago was painted under the
//! newest message as if it were the newest thing said, none of it survived a
//! restart, and a line about one moment spent the foot for the life of the
//! session.
//!
//! What a pane paints is built here too (`painted`), because which rows it shows
//! is a fact about the conversation, its scrollback and its notes — not about the
//! terminal: width and height are arguments, the blank separator that closes a
//! message is trimmed before the window is cut, every block that is not the
//! human's own words or the model's reply is folded to its kind's number of rows
//! ([`Fold`], with the `…` row that says what is hidden — or to none at all,
//! while `Ctrl-O` hides the output), and the foot is capped and counted. `ui.rs` keeps the frame around it — the border, the prompt and the
//! cursor — and paints what this returns, title included, because a pane one row
//! tall has no row to spend on saying what it is hiding, or that the human has
//! scrolled away from the bottom.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::Duration;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use mush_core::message::{Image, Message};
use mush_core::session;
use mush_core::text::{markdown_row_counts, truncate, wrap_text, wrap_text_capped};
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
    /// The pane's title, already elided to the pane's own measure — its
    /// clauses whole or not at all, taken by the same rule the agents pane's
    /// title and the facts line are (finding D11). The painter paints it; it
    /// does not choose its words.
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
    /// Every painted row of the cursor's own stop — a line's rows, or the `…`
    /// the elided tail is.
    pub cursor: Vec<usize>,
    /// Every painted row whose stop the selection covers.
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
    /// The held window's `(offset, up_to)`, if the transcript still has it.
    ///
    /// [`Chat::replace_transcript`] drops the reading with the transcript it
    /// was taken in, so a hold cannot outlive that transcript — a fold used to
    /// leave one behind, and a growth back past the old `up_to` resurrected a
    /// window belonging to a conversation that no longer existed (finding
    /// D15). This bound is the backstop under that rule: a transcript shorter
    /// than the one the window was taken in — a road nobody has written, or a
    /// state a test built — is not a position, and the pane reads as at the
    /// bottom again. One rule, so the pane's title and its body cannot
    /// disagree about which transcripts this reading may be read from.
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

/// One stop of the select cursor: a source line of the message it stands in,
/// or the tail a folded block's cap hid — the rows a pane paints as one `…`.
///
/// The order is the transcript's: `Line(n)` before `Line(n+1)`, and a message's
/// `Tail` after every one of its lines. The copy reads that order too: a
/// selection's text is the source lines its stops cover ([`Stops::span`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Stop {
    /// An index into `Message::text().split('\n')`.
    Line(usize),
    /// Every source line the fold painted no row for. It names no line because
    /// *which* line that is is the pane's measure and the block's kind
    /// ([`Fold::shown`] rows), not the transcript's: this is the one spelling of
    /// "the hidden tail", wherever the fold falls.
    Tail,
}

/// The select mode: a cursor over the transcript's *stops*, and the window a
/// pane shows it through.
///
/// `Ctrl-Y` starts it and `Enter` copies, so this is the one road from a pane to
/// the clipboard. What it copies is the transcript, not the rows: a painted row
/// is a wrap of a source line at one terminal's width, and a drag over the
/// screen is the terminal's rectangle — neither is text another program can be
/// handed. `Message::text()` is, and every source line is one line of it, so
/// the copy is whole source lines joined with the newlines the transcript has —
/// a soft wrap never becomes one. A *stop* is a painted row, though: a source
/// line is one stop whether it wraps over one row or five, and the `…` a
/// folded block's cap paints is one stop for every line the fold hid — so
/// stepping through a long result steps through what the pane shows, and a
/// selection that reaches the `…` takes the whole hidden tail.
///
/// The mode is *modal*: while it is on the keys belong to it (`keys::key`
/// routes them before the panes, the way a picker does), so a letter is not
/// typing and `Esc` is not the box's clear.
#[derive(Debug)]
struct Selecting {
    /// The agent whose pane this cursor is over: the mode belongs to one
    /// conversation, and a pane showing another one paints no cursor.
    agent: AgentId,
    /// The cursor: an index into that agent's transcript, and the [`Stop`] it
    /// stands on.
    ///
    /// The second component is a stop and not a bare line index because a stop
    /// is a *painted row*: a block past its kind's number paints one `…` row
    /// for every source line the fold hid, and a cursor that named those lines
    /// one at a time sat on that same row for a press each — the human's "the
    /// selector sits there a while instead of treating the `…` as one line"
    /// (measured: a 30-line result, `Ctrl-Y`, then 22 `↓` presses that all
    /// landed on the `…`). With the hidden tail as one stop, a single `↓` from
    /// the last line the pane painted is the `…`, the next is the first line
    /// of the message after the result, and `↑` walks back the same way.
    ///
    /// Which lines the fold hides is the *pane's* measure, so [`Self::measure`]
    /// is what turns a `Line` into a `Tail`: a hidden line is not a stop of its
    /// own, it is the tail's. Until a pane has painted the mode's rows, no line
    /// is known hidden and every source line is a stop.
    cursor: (usize, Stop),
    /// The other end of a selection while `Shift-↑`/`Shift-↓` extends one.
    /// `None` is a bare cursor, and `Enter` then copies the stop it stands on —
    /// a line, or the whole tail when it is the `…`.
    anchor: Option<(usize, Stop)>,
    /// The width the pane last painted the mode's rows at, so the key road can
    /// tell a line the pane painted from one the fold hid.
    ///
    /// A `Cell` because only the frame knows the pane's measure, exactly as
    /// `top` is: the frame publishes it as it paints, and the keys read it. It
    /// is the pane's width, not the terminal's (the painter caps it at
    /// `MAX_TRANSCRIPT`), so the boundary this measures is the boundary the
    /// painter painted. `None` is a mode no pane has painted yet: no line is
    /// then known hidden, and every source line is a stop — the reading that
    /// cannot lose a line the pane would have shown.
    measure: Cell<Option<usize>>,
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
    /// `↑`/`↓` and `PgUp`/`PgDn`: move the cursor `step` stops, positive toward
    /// the newest — one source line the pane painted, or the one `…` a capped
    /// result's hidden tail stands as.
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

/// The rows one message paints, and the stop each row is the reading of.
///
/// The map is what lets a *stop* be found among painted rows at all: a wrapped
/// row is not a line of the text, and a block the fold paints
/// ([`folded_marked`]) shows fewer rows than its text has lines. `rows` runs
/// parallel to `lines` and is `None` for a row that is not the message's own
/// words — the reasoning, a tool call, a picture label, the blank that closes a
/// message.
#[derive(Default)]
struct Chunk {
    lines: Vec<Line<'static>>,
    rows: Vec<Option<(usize, Stop)>>,
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
    /// Parallel to `lines`: `(message index, stop)`, the provenance a [`Chunk`]
    /// carries once the message is known.
    rows: Vec<Option<(usize, Stop)>>,
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
/// with no words paints nothing — except a reply of fence lines, which a fence
/// with no body paints as text ([`mush_core::text::markdown_rows`], finding
/// D14) — and a folded block — a tool result, a report, a brief — is painted
/// from its first row: the fold decides *which* rows it paints, so the lines a
/// long block hides behind its `…` are still source lines the copy can take
/// whole. A kind the fold gives no rows at all is the one block whose lines are
/// not stops ([`Stops::of`]). They are one stop, though, not one each:
/// [`Stops`] is where the fold's boundary turns them into [`Stop::Tail`].
fn lines_of(message: &Message) -> Option<Vec<&str>> {
    match message.role.as_str() {
        "user" | "tool" => Some(message.text().split('\n').collect()),
        "assistant" if !message.text().trim().is_empty() => {
            Some(message.text().split('\n').collect())
        }
        _ => None,
    }
}

/// One message's cursor stops at a pane's measure: the source lines the pane
/// painted a row for, and whether its fold hid a tail after them.
///
/// Built from the painter's own wrap walk ([`folded_rows`]), so the boundary
/// the cursor steps over is the boundary the pane painted — never a second wrap
/// with arithmetic of its own that could disagree with the rows on screen.
#[derive(Clone, Copy)]
struct Stops {
    /// The message's source lines.
    lines: usize,
    /// How many of them have a painted row, counted from the first: every line
    /// up to the last painted row's line has a row. The rest are the one
    /// [`Stop::Tail`] when `tail` is set — and this is also the first line that
    /// tail covers, because wrapped rows run in source order. Without a tail
    /// the rest have no stop at all: that is a block the fold gave no rows (the
    /// failure row a hidden one keeps is line one), and the cursor never lands
    /// on a line the pane did not paint.
    visible: usize,
    /// Whether the fold hid rows after the last painted one.
    tail: bool,
}

impl Stops {
    /// The stops of one message at the pane's last measure, or `None` for a
    /// message whose text has no row of its own ([`lines_of`]).
    ///
    /// The message's voice decides which block the fold paints and how its rows
    /// are indented ([`folded_block`]); the boundary is then [`folded_rows`]'s,
    /// the painter's own walk, so the cursor steps over the rows the pane
    /// painted. A message the fold never touches — the human's own line, the
    /// reply, the reasoning — has every source line as a stop, which is what
    /// `folded_block`'s `None` says.
    ///
    /// `measure` is `None` for a mode no pane has painted yet: no line is then
    /// known hidden, and every source line is a stop — the reading that cannot
    /// lose a line the pane would have shown. A kind the fold gives no rows at
    /// all is the exception, and it does not wait for a width: the nothing is
    /// the fold's, not the wrap's, so such a block has no stop at any measure.
    fn of(
        message: &Message,
        voice: Option<Voice>,
        measure: Option<usize>,
        fold: Fold,
    ) -> Option<Stops> {
        let lines = lines_of(message)?.len();
        let mut stops = Stops {
            lines,
            visible: lines,
            tail: false,
        };
        let Some((kind, head)) = folded_block(message, voice) else {
            return Some(stops);
        };
        // The hidden half of `Ctrl-O`: a kind the fold gives no rows paints no
        // row of its own, and the cursor walks the rows the pane painted — so
        // the message is not a stop ([`lines_of`]'s `None`, for the same
        // reason: a block with no row has nothing to stand on). The one
        // exception is the failure row the fold never gives up
        // ([`Fold::shown`]): it is the block's first source line, and there is
        // no tail behind it — a `…` is part of showing a block, and the hidden
        // state has no head for it to stand behind ([`folded_rows`]). The
        // lines are still in the transcript and come back with the toggle.
        if fold.hides(kind) {
            return fails(kind, message.text()).then_some(Stops {
                lines,
                visible: 1,
                tail: false,
            });
        }
        let Some(width) = measure else {
            return Some(stops);
        };
        let (_, rows) = folded_rows(head, message.text(), width, kind, fold);
        if rows.last().is_some_and(|(_, stop)| *stop == Stop::Tail) {
            stops.visible = rows
                .iter()
                .filter_map(|(_, stop)| match stop {
                    Stop::Line(line) => Some(line + 1),
                    Stop::Tail => None,
                })
                .max()
                .unwrap_or(0);
            stops.tail = true;
        }
        Some(stops)
    }

    /// The stop a cursor's line names at this measure: the line itself where
    /// the pane painted a row for it, and the one tail where the fold hid it.
    fn clamp(self, stop: Stop) -> Stop {
        match (stop, self.tail) {
            (Stop::Tail, true) => Stop::Tail,
            (Stop::Tail, false) => Stop::Line(self.visible - 1),
            (Stop::Line(line), true) if line >= self.visible => Stop::Tail,
            (Stop::Line(line), _) => Stop::Line(line.min(self.visible - 1)),
        }
    }

    /// The newest stop of the message.
    fn last(self) -> Stop {
        if self.tail {
            Stop::Tail
        } else {
            Stop::Line(self.visible - 1)
        }
    }

    /// The source lines `stop` covers: one line for [`Stop::Line`], and every
    /// line the fold hid for [`Stop::Tail`]. Clamp first ([`Self::clamp`]): a
    /// `Tail` on a message the pane did not clip is not a stop at all.
    fn span(self, stop: Stop) -> (usize, usize) {
        match stop {
            Stop::Line(line) => (line, line),
            Stop::Tail => (self.visible, self.lines - 1),
        }
    }
}

/// The first row of a message that is the reading of `stop`, or — for a stop the
/// pane's fold hid — the last row that is the reading of a stop at or before it:
/// the result's `…`, which stands for the tail that did not fit.
fn first_row(rows: &[Option<(usize, Stop)>], message: usize, stop: Stop) -> Option<usize> {
    rows.iter()
        .position(|row| *row == Some((message, stop)))
        .or_else(|| last_row(rows, message, stop))
}

/// The last row that is the reading of `stop`: a wrapped line is several rows,
/// and a window that ends on the cursor's stop wants its end, not its start.
fn last_row(rows: &[Option<(usize, Stop)>], message: usize, stop: Stop) -> Option<usize> {
    rows.iter()
        .rposition(|row| *row == Some((message, stop)))
        .or_else(|| {
            rows.iter().rposition(
                |row| matches!(row, Some((at, row_stop)) if *at == message && *row_stop <= stop),
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

/// Which row of a window the cursor is painted on: the cursor's own stop's
/// first row, or — for a line the fold hid — the tail's `…`, the row the pane
/// paints for it. `None` is "this window does not show the cursor", which is
/// what makes the frame place the window again.
///
/// The nearest painted row of the cursor's own message is the last answer: a
/// source line the view paints no row for — a reply's fence line, inside a
/// block that has a body — is still a stop the key road can stand on, and a
/// stop with no row must paint *somewhere* or the frame reads "the cursor is
/// here" as "there is no cursor": the mode's title promised `Enter copies`
/// over a pane with no cursor in it (finding D14). The copy is unaffected — it
/// reads the stop, not the row — so a cursor on a fence line copies the fence
/// line while sitting on the message's nearest words.
///
/// `cut` is the message whose rows the window's height cut short of its text: a
/// cut message cannot answer for a hidden line, because the cursor's stop may be
/// under the cut rather than behind the fold.
fn cursor_row(
    rows: &[Option<(usize, Stop)>],
    cursor: (usize, Stop),
    cut: Option<usize>,
) -> Option<usize> {
    if let Some(at) = rows.iter().position(|row| *row == Some(cursor)) {
        return Some(at);
    }
    if cut == Some(cursor.0) {
        return None;
    }
    rows.iter()
        .rposition(
            |row| matches!(row, Some((message, stop)) if *message == cursor.0 && *stop <= cursor.1),
        )
        .or_else(|| {
            rows.iter().position(
                |row| matches!(row, Some((message, stop)) if *message == cursor.0 && *stop >= cursor.1),
            )
        })
}

/// Whether the cursor sits above everything a window shows. A window with no
/// text row in it at all has nothing to be above, and reads as below — the
/// bottom anchoring is the one that ends up showing the cursor.
fn cursor_above(body: &Body, cursor: (usize, Stop)) -> bool {
    match body.rows.iter().flatten().next() {
        Some(&first) => cursor < first,
        None => false,
    }
}

/// The rows a window paints the mode on: the cursor's own stop, and every
/// painted row whose stop the selection covers.
///
/// `cursor` is the caller's clamp of the mode's own, not `select.cursor`: the
/// row the pane paints is the row the window was placed for.
fn select_rows(
    rows: &[Option<(usize, Stop)>],
    select: &Selecting,
    cursor: (usize, Stop),
    cut: Option<usize>,
) -> Option<SelectRows> {
    let at = cursor_row(rows, cursor, cut)?;
    // Every row of the cursor's own stop, not just the one the lookup landed
    // on: a wrapped line is one stop, and every row of it is the cursor.
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
        // A bare cursor is not a selection: the one stop it covers would wear
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
    /// was built with, published once per actor
    /// ([`AgentEvent::SystemPrompt`](crate::agent::AgentEvent::SystemPrompt)).
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
    /// Each painted tool call's one-word reading, keyed like [`Self::spoken`]:
    /// the agent, then the index of the line it sits in, then the call's place
    /// in that line's batch.
    ///
    /// A label shows at most [`LABEL_ARGS`] columns, but reading it is a full
    /// parse of the model's argument JSON (`agent::summarize_args`), and that
    /// JSON is unbounded — H45/H46 removed the caps on a `write_file`'s
    /// `content` — so one 2 MB call cost 104.9 ms *per frame*, in a debug
    /// build (finding A13). The argument text cannot change once the call is
    /// recorded, so the reading is stored where the transcript it belongs to
    /// lives, and dropped on the three roads [`Self::spoken`] is: a
    /// replacement, a forget, a clear. An append moves no index and keeps it.
    /// The value is already cut to [`LABEL_ARGS`] columns, the most any pane
    /// can show, because a second cut to the pane's own budget is the same cut
    /// the whole value would have taken.
    summaries: RefCell<HashMap<AgentId, HashMap<usize, Vec<String>>>>,
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
    /// Whether a pane paints the *output* kinds — a tool's result, mush's own
    /// report about a child or a job, the brief a child's pane opens with.
    ///
    /// A *view*, in the family of [`Self::reasoning`] and `App`'s zen
    /// (`Ctrl-F`), and shown by default: `Ctrl-O` changes only what the panes
    /// paint, so it is not said into the conversation and not stored — the rows
    /// are still in the transcript and come back on the next press, and a
    /// restart paints them again. It lives here, beside the transcript it
    /// hides, for the same reason the reasoning view does: every pane paints
    /// through this one `Chat`, so one flip is every pane's. The fold's own
    /// rule — a failure is never what the fold gives up ([`Fold`]) — is what
    /// keeps a hidden failure visible; this flag knows nothing about it.
    output: bool,
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
            summaries: RefCell::new(HashMap::new()),
            revisions: HashMap::new(),
            pending: None,
            reasoning: true,
            output: true,
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

    /// Whether a pane paints the tool output, mush's reports and the briefs.
    pub fn shows_output(&self) -> bool {
        self.output
    }

    /// `Ctrl-O`: show or hide the output kinds — a tool's result, mush's own
    /// report about a child or a job, the brief a child's pane opens with. The
    /// two states are the folded numbers and none, so the same key brings the
    /// rows back exactly as they were.
    ///
    /// A view, like [`Self::set_reasoning`]: not a change to the conversation
    /// and not a thing to say, and `clear` deliberately leaves it alone — the
    /// human's choice outlives the chat it was made in. A failure is still
    /// painted in both states, because that rule lives on [`Fold`] and not
    /// here: a hidden failure would be a lie about what happened.
    pub fn set_output(&mut self, on: bool) {
        self.output = on;
    }

    /// The fold this conversation's panes paint through: the conversation's own
    /// numbers, with the output kinds at none while the human has hidden them
    /// ([`Self::set_output`]).
    ///
    /// The one read for both roads that measure a block — the painter
    /// ([`Self::chunk`]) and the select mode's stop walk ([`Self::stops_at`]) —
    /// so a cursor cannot step over a row the pane did not paint.
    fn painted_fold(&self) -> Fold {
        if self.output {
            self.fold
        } else {
            self.fold.without_output()
        }
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
    /// One writer, one reader: the actor emits it
    /// ([`AgentEvent::SystemPrompt`](crate::agent::AgentEvent::SystemPrompt)
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
    ///
    /// It is also where a dropped-turns note is *placed*: the note reaches the
    /// pane through the same append road every line takes, while its place is
    /// the transcript's front ([`transcript::place_dropped_note`], the one
    /// spelling of that place, shared with the request). The messages it passes
    /// keep their text and their voices; only their indices change, so the
    /// caches keyed by index move with them ([`Self::shift_indices`]).
    pub fn push_message(&mut self, agent: AgentId, message: Message) {
        let prior = self.revision(agent);
        let note = transcript::is_dropped_note(&message);
        if message.role == "user" {
            let index = self.transcript(agent).len();
            let voice = match self.pending.take() {
                Some(words) if words == message.text().trim() => Voice::Human,
                _ => elsewhere(agent, index, &message),
            };
            // A note's voice is read off its flag at paint time
            // ([`unrecorded`]) — and it does not keep the index it arrived at
            // (below), so nothing is recorded for it here.
            if voice != Voice::Human && !note {
                self.spoken.entry(agent).or_default().insert(index, voice);
            }
        }
        let messages = if agent == AgentId::ROOT {
            &mut self.root
        } else {
            self.agents.entry(agent).or_default()
        };
        let arrived = messages.len();
        if !note {
            messages.push(message);
        } else if let Some(at) = messages.iter().position(transcript::is_dropped_note) {
            // A copy that already carries one keeps one, in the note's own
            // place: the sentence is the same one, and nothing moves.
            messages[at] = message;
        } else {
            messages.push(message);
            transcript::place_dropped_note(messages);
            let at = messages
                .iter()
                .position(transcript::is_dropped_note)
                .unwrap_or(arrived);
            self.shift_indices(agent, at, arrived);
        }
        self.advance(agent, prior);
    }

    /// Move every index at or past `from` one step later, because a line that
    /// belongs at `from` arrived at the end of the transcript: the messages it
    /// passed keep their text and their voices and only their indices change.
    ///
    /// Everything keyed by index moves with them — the voices recorded for the
    /// lines the note jumped over, and the select cursor — while a held
    /// reading's `up_to` is a count of messages and moves only when the window
    /// reaches past the insertion point. The summary cache is dropped rather
    /// than shifted: it is derived from the messages themselves (finding A13's
    /// parse), so the next paint reads each call from the index it now sits at.
    fn shift_indices(&mut self, agent: AgentId, from: usize, arrived: usize) {
        if let Some(voices) = self.spoken.get_mut(&agent) {
            let moved: Vec<(usize, Voice)> = voices
                .iter()
                .filter(|(index, _)| (from..arrived).contains(index))
                .map(|(index, voice)| (*index, *voice))
                .collect();
            for (index, voice) in moved {
                voices.remove(&index);
                voices.insert(index + 1, voice);
            }
        }
        self.summaries.borrow_mut().remove(&agent);
        if let Some(Reading::Holding { up_to, .. }) = self.reading.get_mut(&agent) {
            if *up_to > from {
                *up_to += 1;
            }
        }
        if let Some(select) = self.select.as_mut().filter(|select| select.agent == agent) {
            if select.cursor.0 >= from {
                select.cursor.0 += 1;
            }
            if let Some(anchor) = select.anchor.as_mut() {
                if anchor.0 >= from {
                    anchor.0 += 1;
                }
            }
            let (index, row) = select.top.get();
            if index >= from {
                select.top.set((index + 1, row));
            }
        }
    }

    /// Replace an agent's transcript: the root's is compacted to
    /// `[system, user(summary)]`, a restored one arrives whole. A copy that
    /// carries the dropped-turns note anywhere is placed where the request keeps
    /// it before it is stored ([`transcript::place_dropped_note`]): the note's
    /// provenance is the flag (finding F3) and its place is the transcript's
    /// front (finding A18), so a restored pane and the next request read the
    /// same order.
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
    ///
    /// The reading goes too, for the same reason one step out: a held window
    /// is a position in *this* transcript, and its `up_to` is a count of the
    /// messages that transcript had when the human scrolled away. Left behind,
    /// it came back from the dead the moment the replacement grew back past
    /// that count — a window belonging to a conversation that no longer exists,
    /// which the pane's title then read as a live position (finding D15). A
    /// fold *is* a new transcript, and a pane reads a new transcript from the
    /// bottom.
    pub fn replace_transcript(&mut self, agent: AgentId, messages: Vec<Message>) {
        let prior = self.revision(agent);
        self.spoken.remove(&agent);
        // The readings are keyed by the indices of the transcript that just
        // went, exactly as the voices are: a stale one would label a message
        // with another call's arguments (see the field).
        self.summaries.borrow_mut().remove(&agent);
        // A hold is a position in the transcript that just went, and a fold is
        // a new transcript: the pane reads it from the bottom (finding D15).
        self.reading.remove(&agent);
        self.pending = None;
        if self
            .select
            .as_ref()
            .is_some_and(|select| select.agent == agent)
        {
            self.select = None;
        }
        // The copy may hold the dropped-turns note anywhere the hand that wrote
        // it left it — a session file holds it where its own trim put it, and
        // the flag is what tells it (finding F3). It is placed where the request
        // keeps it before the pane paints it (finding A18), so a restored pane
        // and the next request read the same order.
        let mut messages = messages;
        transcript::place_dropped_note(&mut messages);
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
    pub fn used_tokens_for(&self, id: AgentId, budget: usize) -> usize {
        self.used_weight_for(id, budget) / mush_core::config::BYTES_PER_TOKEN
    }

    /// The same sum in the budget's own currency: one agent's **own** system
    /// prompt plus its transcript, trimmed to `budget` the way the actor's own
    /// list is ([`Self::bounded_transcript`]) and weighed the one way
    /// [`mush_core::transcript::trim_history`] weighs them. Split from the
    /// token spelling of the same sum so a caller that needs the number in
    /// bytes — the attach gate, asking how much room a picture has left, and
    /// the meter, comparing against the byte boundary every decision uses —
    /// reads the one sum instead of adding the parts up again (two spellings
    /// of one arithmetic is how the budget and the meter drift apart).
    ///
    /// The number is a question about the *next request*: the pane's record is
    /// not trimmed and the next request is not built from it untrimmed, so
    /// weighing the record made the meter say `over` forever on a window whose
    /// fold cannot fit while every request that went out still fitted (finding
    /// A8). The bound is the one the actor itself trims to, so the attach
    /// gate's room is the room the run will find, and the meter's fraction is
    /// the fraction of the request that follows.
    ///
    /// An agent whose prompt has not been published weighs nothing: the number
    /// is the actor's fact, not this table's guess. One whose prompt *has* been
    /// published and that has said nothing yet weighs exactly that prompt — the
    /// window between `AgentEvent::SystemPrompt` and the first `Message` is one
    /// frame wide, and a meter or attach gate that read 0 there would be a
    /// whole prompt short of the request it is pricing (finding D16; the early
    /// return this replaced is why the audit measured 0 against a 3,006-byte
    /// prompt). The prompt counted is the agent's own: the
    /// root's is the conversation's ([`Self::system`], which the root actor is
    /// handed with every run), and a child's is the one its actor published
    /// ([`Self::learn_system`]) — a child's prompt names the child's own
    /// workspace, so only the actor that built it can say what it weighs.
    pub fn used_weight_for(&self, id: AgentId, budget: usize) -> usize {
        self.bounded_transcript(id, budget)
            .iter()
            .map(Message::weight)
            .fold(0, usize::saturating_add)
    }

    /// `id`'s conversation as a request would carry it at the next message
    /// boundary: the system prompt the history opens with, the transcript, and
    /// a trim to `budget` in the shape an actor's own list has (system first,
    /// the opening task after it), with the dropped-turns note put back where
    /// the dropped turns were ([`mush_core::transcript::place_dropped_note`]'s
    /// rule, applied by the trim itself).
    ///
    /// This is the **bounded view**, and it is what [`Self::used_weight_for`]
    /// weighs and what `App::session_snapshot` stores. The pane's own record is
    /// deliberately *not* trimmed and is allowed to be heavier: a cut drops
    /// turns the human still wants to read, and scrolling the conversation they
    /// had is what the pane is for. The two are allowed to differ because they
    /// answer different questions — the pane keeps what was said, this keeps
    /// what the next request pays for — and the pane is the one that can be
    /// reconstructed from nothing, since it is on screen.
    pub fn bounded_transcript(&self, id: AgentId, budget: usize) -> Vec<Message> {
        let mut messages: Vec<Message> = self.system_for(id).into_iter().cloned().collect();
        messages.extend(self.transcript(id).iter().cloned());
        // The note — the one the trim adds, or the one the copy already carried
        // and the trim moves back into place — is part of the view: a transcript
        // that lost turns says so once, and the stored row and the meter count
        // that line like any other.
        let _ = transcript::trim_history(&mut messages, budget);
        messages
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
        self.summaries.borrow_mut().clear();
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
    /// already bounded without it: what a save stores for an agent is the
    /// **bounded view** of its conversation ([`Self::bounded_transcript`]), at
    /// most one trim away from the history budget the run measures against —
    /// the pane's record can be heavier without the store following it (finding
    /// A8), so the file holds `CHILD_HISTORY` such rows plus the root. Dropping
    /// the row drops one child's bounded transcript from the next save.
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
        self.summaries.borrow_mut().remove(&agent);
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

    /// The agent whose pane the mode stands over, if it is on — the one read of
    /// the mode's subject, for a caller that has to tell "the mode already
    /// names the pane the human reads" from "the pane moved under it"
    /// (finding D18's focus change).
    pub fn selecting_agent(&self) -> Option<AgentId> {
        self.select.as_ref().map(|select| select.agent)
    }

    /// Leave the select mode without copying anything. The other road out is
    /// `Esc` (which every caller can reach through [`Chat::select_apply`]), and
    /// this one is for a change the mode does not own: `Tab` moves the focus
    /// and an attach client's `focus` moves the pane, and a mode that kept the
    /// keyboard — or a cursor — over a pane nobody is reading would be a modal
    /// with no way out a human can see (finding D18).
    pub fn cancel_select(&mut self) {
        self.select = None;
    }

    /// `Ctrl-Y`: start selecting in the pane `on` shows, with the cursor on the
    /// newest source line — where the pane already is, because it follows the
    /// bottom. For a block the fold clipped, that line is a hidden one and
    /// the frame paints it on the `…`: the tail is the newest stop there, and
    /// the first read of the cursor clamps it onto that stop.
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
                    // The first frame publishes the width it paints the mode's
                    // rows at; until then no line is known hidden.
                    measure: Cell::new(None),
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

    /// The newest source line of the newest message that has rows of its own,
    /// if there is one. The fold may hide that line from the pane; the frame then
    /// paints the cursor on the `…`, the tail's own stop.
    fn last_line(&self, on: AgentId) -> Option<(usize, Stop)> {
        let transcript = self.transcript(on);
        (0..transcript.len()).rev().find_map(|index| {
            lines_of(&transcript[index]).map(|lines| (index, Stop::Line(lines.len() - 1)))
        })
    }

    /// The oldest source line of the first message that has rows of its own, if
    /// there is one.
    fn first_line(&self, on: AgentId) -> Option<(usize, Stop)> {
        let transcript = self.transcript(on);
        (0..transcript.len())
            .find_map(|index| lines_of(&transcript[index]).map(|_| (index, Stop::Line(0))))
    }

    /// What the select mode's keys do — the one place they run.
    ///
    /// `Some(copied)` is `Enter`: the copy the caller hands the clipboard, and
    /// the mode left behind with it. `None` is a key that changed the state and
    /// nothing else — and, for `Copy`, a copy that did not happen: the mode is
    /// left because the pane it names has no line left to stand on, so the
    /// caller says so rather than letting the selection vanish in silence
    /// (`App::select_key` owns the bar's line, finding D18).
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
                let next = self.step_stop(on, cursor, step);
                self.set_cursor(next);
                None
            }
            SelectKey::Extend(step) => {
                let next = self.step_stop(on, cursor, step);
                if let Some(select) = self.select.as_mut() {
                    // The anchor is where the selection started: the first
                    // extended step plants it on the stop the cursor was on,
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

    fn set_cursor(&mut self, cursor: (usize, Stop)) {
        if let Some(select) = self.select.as_mut() {
            select.cursor = cursor;
        }
    }

    /// The width the pane last painted the mode's rows at, if it has
    /// ([`Selecting::measure`]). The one read of the published measure, so the
    /// clamp, the step and the copy all draw the fold's boundary the same way.
    fn measure(&self) -> Option<usize> {
        self.select.as_ref().and_then(|select| select.measure.get())
    }

    /// One message's stops at a measure, with the voice the pane paints it
    /// under: the fold a block wears is the message's own kind ([`folded_block`]),
    /// and the block's boundary is the painter's walk ([`folded_rows`]), so the
    /// key road measures the rows the paint road painted — one road, not two.
    fn stops_at(&self, on: AgentId, index: usize, measure: Option<usize>) -> Option<Stops> {
        let message = self.transcript(on).get(index)?;
        let voice = self.voice_at(on, index, message);
        Stops::of(message, voice, measure, self.painted_fold())
    }

    /// The cursor as the transcript *and the pane's last paint* are now: a
    /// transcript can shrink under a state that still points into it — a reaped
    /// conversation, or a state a caller built — and neither a key nor a frame
    /// may index past the end; and a line the fold hid is not a stop of its
    /// own, so it becomes the one [`Stop::Tail`] the `…` paints. The nearest
    /// line that still exists is the honest clamp — and `None` when there is no
    /// source line left at all, which drops the mode rather than leaving a
    /// cursor over nothing.
    fn clamped_cursor(&self, on: AgentId) -> Option<(usize, Stop)> {
        let select = self.select.as_ref().filter(|select| select.agent == on)?;
        let measure = select.measure.get();
        let transcript = self.transcript(on);
        let mut index = select.cursor.0.min(transcript.len().checked_sub(1)?);
        loop {
            if let Some(stops) = self.stops_at(on, index, measure) {
                return Some((index, stops.clamp(select.cursor.1)));
            }
            index = index.checked_sub(1)?;
        }
    }

    /// The stop one step older or newer than `cursor`, or `None` at an end of
    /// the transcript.
    ///
    /// The transcript's own lines are not the stops: a line the fold hid is one
    /// of the lines the `…` stands for, and the whole hidden tail is the `…`'s
    /// own stop ([`Stops`]). So forward from the last line the pane painted is
    /// the tail, forward from the tail is the next message's first line, and
    /// backward is the same road reversed.
    fn adjacent(
        &self,
        on: AgentId,
        cursor: (usize, Stop),
        forward: bool,
        measure: Option<usize>,
    ) -> Option<(usize, Stop)> {
        let transcript = self.transcript(on);
        let stops = self.stops_at(on, cursor.0, measure)?;
        if forward {
            if let Stop::Line(line) = cursor.1 {
                if line + 1 < stops.visible {
                    return Some((cursor.0, Stop::Line(line + 1)));
                }
            }
            if stops.tail && cursor.1 != Stop::Tail {
                return Some((cursor.0, Stop::Tail));
            }
            ((cursor.0 + 1)..transcript.len()).find_map(|index| {
                self.stops_at(on, index, measure)
                    .map(|_| (index, Stop::Line(0)))
            })
        } else {
            match cursor.1 {
                Stop::Tail => Some((cursor.0, Stop::Line(stops.visible - 1))),
                Stop::Line(0) => (0..cursor.0).rev().find_map(|index| {
                    self.stops_at(on, index, measure)
                        .map(|stops| (index, stops.last()))
                }),
                Stop::Line(line) => Some((cursor.0, Stop::Line(line - 1))),
            }
        }
    }

    /// The cursor moved `step` stops, positive toward the newest, stopping at
    /// either end: the oldest and the newest stop are ends of the transcript,
    /// not walls to crash into.
    fn step_stop(&self, on: AgentId, cursor: (usize, Stop), step: i64) -> (usize, Stop) {
        let measure = self.measure();
        let mut cursor = cursor;
        let forward = step > 0;
        for _ in 0..step.unsigned_abs() {
            match self.adjacent(on, cursor, forward, measure) {
                Some(next) => cursor = next,
                None => break,
            }
        }
        cursor
    }

    /// `Enter`: the transcript's own text for the selection, or for the stop the
    /// cursor stands on when there is no selection, plus the line mush says once
    /// the clipboard has taken it.
    ///
    /// The text is the *source lines* joined with `\n` — the separator the
    /// transcript has between them — so a whole message is `Message::text()`
    /// byte for byte, a soft wrap at this pane's width is not a newline, and a
    /// tab is a tab. Every stop's own source is taken: a line is itself, and the
    /// elided tail is every line the fold hid, so a selection that reaches the
    /// `…` gets the whole block however many presses the tail spans — the fold
    /// bounds the frame, never the copy. A folded block — a tool result, a
    /// report, a brief — is copied whole however little of it the pane painted.
    fn copy(&self, on: AgentId, cursor: (usize, Stop)) -> Copied {
        let select = self.select.as_ref().expect("the mode is on");
        let measure = select.measure.get();
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
            let Some(stops) = self.stops_at(on, index, measure) else {
                continue;
            };
            // A stop covers a span: one line, or every line the fold hid. The
            // endpoints are clamped at this measure first, so a line a resize
            // has hidden since stands as the tail it now is, and the two ends'
            // spans meet without a gap (a stop's span always continues the one
            // before it).
            let first = if index == from.0 {
                stops.span(stops.clamp(from.1)).0
            } else {
                0
            };
            let last = if index == to.0 {
                stops.span(stops.clamp(to.1)).1
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
    ///
    /// A pane painting the select mode publishes the width its rows are made at
    /// ([`Selecting::measure`]) before it clamps the cursor: only the frame
    /// knows the pane's measure, and the key road reads it to tell a line the
    /// pane painted from one the fold hid.
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
        // The width these rows are painted at, published before the clamp and
        // the body so both read the measure the map below was built at.
        if let Some(select) = select {
            select.measure.set(Some(width));
        }
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

        let mut clauses: Vec<String> = Vec::new();
        // The mode's own line, first because it is the newest thing about the
        // pane: `Ctrl-Y` put the cursor here and the keys that finish the job
        // are not the ones the hint under the pane advertises. The pair named
        // is the one that leaves the mode — nothing else on this screen says
        // which of the two copies.
        if mode.is_some() {
            clauses.push("Enter copies · Esc leaves ".to_string());
        }
        // A pane with no row to spare for the foot's own count line is the case
        // the title exists for: wherever the human looks, the pane says how
        // many lines it is hiding — and names the way to read them, because the
        // count row that carries `· /notes` is exactly the row this pane has no
        // room for.
        if foot.hidden > 0 && !foot.counted {
            clauses.push(format!("{} · /notes ", more_label(foot.hidden)));
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
                clauses.push(format!("scrolled ↑{offset} rows · PgDn "));
            }
        }
        // The clauses are ranked, and a pane that runs out of columns drops
        // them whole from the right — the same rule, and the same function, the
        // agents pane's title and the bar's facts line already use. A title cut
        // mid-word by the border is a count that is not the count: with `Ctrl-Y`
        // open at 40 columns the chat's title ran 64 columns wide and the pane
        // painted ` agent #12 · Enter copies · Esc leaves` — the `+6 more lines`
        // and `/notes` clauses, the pane's own way of saying what it hides, were
        // simply gone (finding D11). Each clause ends in the space that joins it
        // to the next, so the joined line reads as one sentence of clauses.
        //
        // The floor is the pane's own name: a terminal too narrow for one clause
        // still says which conversation the pane is showing. The budget is the
        // measure the rows were laid out at, so a title never outgrows the pane
        // it names.
        let name = if pane.agent == AgentId::ROOT {
            " mush ".to_string()
        } else {
            format!(" agent {} ", pane.agent)
        };
        let title = super::screen::elide(&clauses, "· ", &format!("{name}· "), &name, width);
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
    /// never `select.cursor`: the window is placed for the stop the transcript
    /// and the pane's last paint leave, so every index the walk makes names a
    /// row that is there.
    fn select_body(
        &self,
        select: &Selecting,
        cursor: (usize, Stop),
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
        // own stop is behind a block's fold. Put it where the pane can hold it
        // — the cursor's stop at the top when it is above the window, at the
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

    /// The one-word reading of every tool call in one message, computed once
    /// per call and kept for the life of the transcript entry.
    ///
    /// This is the reading [`tool_label`] paints, and it is a full parse of the
    /// argument JSON: the parse and the cut to the pane's columns are what cost
    /// a frame, and the parse is by far the larger half (finding A13). The
    /// argument text cannot change once the call is recorded, so the reading is
    /// taken here, on the first frame that paints the call, and never again —
    /// the field's own doc has the invalidation rules.
    fn call_summaries(&self, on: AgentId, index: usize, message: &Message) -> Vec<String> {
        let mut cached = self.summaries.borrow_mut();
        let by_index = cached.entry(on).or_default();
        if let Some(read) = by_index.get(&index) {
            return read.clone();
        }
        let read: Vec<String> = message
            .tool_calls()
            .iter()
            .map(|call| truncate(&summarize_args(&call.function.arguments), LABEL_ARGS))
            .collect();
        by_index.insert(index, read.clone());
        read
    }

    /// One message's rows, with the message and stop each came from.
    fn chunk(&self, on: AgentId, index: usize, width: usize) -> Chunk {
        let message = &self.transcript(on)[index];
        let voice = self.voice_at(on, index, message);
        let summaries = self.call_summaries(on, index, message);
        let mut lines = Vec::new();
        let rows = render_message(
            &mut lines,
            message,
            voice,
            width,
            self.reasoning,
            self.painted_fold(),
            &summaries,
        );
        debug_assert_eq!(lines.len(), rows.len(), "one map entry per painted row");
        Chunk {
            lines,
            rows: rows
                .into_iter()
                .map(|stop| stop.map(|stop| (index, stop)))
                .collect(),
        }
    }

    /// The window's top with the cursor's own stop as its first row: where the
    /// stop begins in its message. A tail the fold hid begins at the `…`, which
    /// is the row that stands for it.
    fn top_at_cursor(&self, on: AgentId, width: usize, cursor: (usize, Stop)) -> (usize, usize) {
        let chunk = self.chunk(on, cursor.0, width);
        (
            cursor.0,
            first_row(&chunk.rows, cursor.0, cursor.1).unwrap_or(0),
        )
    }

    /// The window's top with the cursor's own stop as the pane's last row: the
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
        cursor: (usize, Stop),
    ) -> (usize, usize) {
        let want = height.saturating_sub(1);
        let head = self.chunk(on, cursor.0, width);
        // The cursor's stop's last row is the window's last row, so the rows
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
///
/// `summary` is the reading [`Chat::call_summaries`] remembers, and `None` — a
/// caller painting a message without a `Chat` in hand — takes the same reading
/// itself. Either way the reading is what `agent::summarize_args` gives, cut to
/// [`LABEL_ARGS`] first, because the cached copy is stored at that width and a
/// second cut to the pane's budget is the same cut (finding A13).
fn tool_label(call: &mush_core::ToolCall, width: usize, summary: Option<&str>) -> String {
    // `agent::summarize_args` is the same reading the tree shows.
    let head = format!("  ⚙ {} ", call.function.name);
    let budget = LABEL_ARGS.min(width.saturating_sub(head.width()));
    if budget < MIN_BODY {
        return head.trim_end().to_string();
    }
    let summary = match summary {
        Some(summary) => summary.to_string(),
        None => truncate(&summarize_args(&call.function.arguments), LABEL_ARGS),
    };
    format!("{head}{}", truncate(&summary, budget))
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
/// conversation is marked or shaped — a child's `#1 done: …` / `#1 stopped: …` /
/// `#1 failed: …`, a job's `#c2 done: …`, a fold's carried summary, the line
/// that says the oldest turns were dropped (marked by `Message::note`'s flag and
/// read by [`transcript::is_dropped_note`]), and the lines mush writes *to* a
/// run — the loop guard's warning, the instructions a cut-off or unreadable
/// reply is answered with, the report a failed commit leaves (marked by
/// [`Message::mush`], the one constructor that sets the flag) — and a child's
/// transcript opens with the brief its parent spawned it with. What is left is
/// the human's, because that is what most of a transcript is.
///
/// The marked lines are read by the flag alone, never by their words: every one
/// of them reaches the pane as the `user` message the model must keep reading
/// (its place in the request is the point of finding F17), and every one of them
/// is a sentence a human could type word for word. Reading a sentence's own head
/// would make those words the provenance, which is the rule finding F3 removed;
/// the flag is what the session file stores ([`Message::mush`]), so the same
/// read serves the live transcript and the one restored from the session file.
/// One mark serves every writer — the loop guard, both instructions, the failed
/// commit — instead of a prefix per sentence to keep in step with the pane.
///
/// The one line this cannot place is a parent's steering after a restart: the
/// words look exactly like the human's own nudge, and nothing in the file says
/// which they were. It reads as the human's until the process is new again —
/// the alternative would be painting the human's question as somebody else's.
/// The note is not in that class: the flag that tells it
/// ([`transcript::is_dropped_note`]) is stored with the file
/// ([`Message::note`]), so a restored transcript's note is read as mush's here,
/// exactly as it was before the restart. Matching its sentence, by contrast, is
/// the one rule finding F3 removed: a line that is word for word the note but
/// carries no flag stays the human's.
fn unrecorded(agent: AgentId, index: usize, message: &Message) -> Voice {
    let text = message.text();
    if report(text)
        || text.starts_with(FOLDED)
        || message.mush
        || transcript::is_dropped_note(message)
    {
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

/// What a line of mush's report grammar says after its `#N`/`#cN` head —
/// `#1 done: wrote the parser` → `Some(" done: wrote the parser")` — or `None`
/// for a line that is not one of mush's reports.
fn report_tail(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('#')?;
    let rest = rest.strip_prefix('c').unwrap_or(rest);
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let tail = &rest[digits..];
    [" done:", " stopped:", " failed:", " cut off:"]
        .iter()
        .any(|head| tail.starts_with(head))
        .then_some(tail)
}

/// Whether a line is one of mush's reports — `#1 done: …`, `#c2 stopped: …`,
/// `#3 cut off: …` — written by the run loop, the job registry and the UI's own
/// last-resort report with exactly this vocabulary.
fn report(text: &str) -> bool {
    report_tail(text).is_some()
}

/// Whether a report line is the one that says a run *failed* (`#3 failed: …`)
/// — the report whose first row the fold may never give up.
fn report_failed(text: &str) -> bool {
    report_tail(text).is_some_and(|tail| tail.starts_with(" failed:"))
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
    /// Mush's own line written into a conversation ([`Voice::Mush`]): a child's
    /// or a job's report (`#1 done: …`, `#c2 done: …`), a fold's carried
    /// summary, the line that says the oldest turns were dropped.
    Mush,
    /// The words another agent addressed to this pane: the brief a child's
    /// pane opens with ([`Voice::Brief`]) and a parent's steering after it
    /// ([`Voice::Parent`]), which is the same words arriving later.
    Brief,
    /// The model's own reasoning — the `Ctrl-T` block [`reasoning_rows`]
    /// paints.
    Reasoning,
}

impl Kind {
    /// How many kinds there are: the length of [`Fold`]'s table, so a new
    /// variant that is given a slot but not a table entry is a compile error,
    /// and one given a table entry but not a slot is too.
    const COUNT: usize = 4;

    /// This kind's own number in [`Fold`]'s table. No wildcard arm: a new kind
    /// cannot compile until it is given a slot here, and the slot is where its
    /// number is read from.
    fn slot(self) -> usize {
        match self {
            Kind::Result => 0,
            Kind::Mush => 1,
            Kind::Brief => 2,
            Kind::Reasoning => 3,
        }
    }
}

/// The one place a conversation's "how much of this thing does the human see"
/// decision lives: how many rows, per [`Kind`] of block, a pane paints before
/// the `…` row that stands for the rest.
///
/// It is a *value* and not a `const` per arm. The `Ctrl-O` view holds one and
/// sets it ([`Fold::without_output`]) — the output kinds go to no rows at all,
/// and the same numbers come back on the next press — and a setting will later
/// read the numbers from configuration, which is why they are here and not
/// spelled at a paint site. `Chat` holds the one a conversation paints through
/// ([`Chat::painted_fold`]).
///
/// The numbers are **per kind** because the kinds are read differently. A tool
/// result, a report and a brief are dumps: the human reads their head and
/// copies the rest, and the pane exists to keep a long transcript scrollable —
/// eight rows, the number the `"tool"` arm used to keep as a `const` of its
/// own. Wrapping a block only as far as the fold is also why a long result
/// stopped being most of a frame's cost on a long session (see
/// [`wrap_text_capped`]). A reasoning block is the text the human pressed
/// `Ctrl-T` to read, so its number is `usize::MAX`: shown whole today, with the
/// slot in place because the setting the human already asked for is "one for
/// child/tool calls and another for thinking rows shown". `Ctrl-O` leaves this
/// slot exactly where it is: a thought is not output, and `Ctrl-T` owns it.
///
/// A block that reports a **failure** is kept even where the fold would hide
/// it: the failure is never what the fold gives up. The rule lives here, not in
/// an arm and not in the handler of a key that changes a number, so the
/// `Ctrl-O` view's no-rows state still paints a failed result's own
/// `! error: …` row and a `#1 failed: …` report's first row — the same rule the
/// foot's cap already holds ("the failure is never the line the cap gives up").
/// And it paints *only* that row: a `…` ([`elision`]) is part of showing a
/// block, so the hidden state has no head for one to stand behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fold {
    /// One number per kind, indexed by [`Kind::slot`].
    rows: [usize; Kind::COUNT],
}

impl Fold {
    /// The numbers a conversation opens with: eight rows for a result, a
    /// report or a brief, and no bound for the reasoning.
    pub const DEFAULT: Fold = Fold {
        rows: [8, 8, 8, usize::MAX],
    };

    /// How many of `text`'s rows this fold lets a pane paint for `kind` — the
    /// kind's number, and the one exception at a `0`-rows setting: a block that
    /// reports a failure keeps its failure row (see the type's doc).
    pub fn shown(&self, kind: Kind, text: &str) -> usize {
        let rows = self.number(kind);
        if rows == 0 && fails(kind, text) {
            1
        } else {
            rows
        }
    }

    /// The same fold with the output kinds at no rows at all — what a pane
    /// paints through while the human has asked for the main model's words
    /// alone (`Ctrl-O`): a tool's result, mush's own report about a child or a
    /// job, and the brief a child's pane opens with. The reasoning is
    /// deliberately untouched: it is not output, and `Ctrl-T` owns that block.
    pub fn without_output(self) -> Self {
        self.with(Kind::Result, 0)
            .with(Kind::Mush, 0)
            .with(Kind::Brief, 0)
    }

    /// This fold's own number for `kind`, before [`Fold::shown`] adds the one
    /// exception a `0` carries.
    fn number(self, kind: Kind) -> usize {
        self.rows[kind.slot()]
    }

    /// Whether this fold gives `kind` no rows at all — the hidden half of the
    /// `Ctrl-O` view ([`Fold::without_output`]). A block of such a kind paints
    /// no `…` row either ([`folded_rows`]); its one exception is the failure
    /// row [`Fold::shown`] never gives up.
    fn hides(self, kind: Kind) -> bool {
        self.number(kind) == 0
    }

    /// The same fold with one kind's number changed — how a view sets one: the
    /// `Ctrl-O` key zeroes the output kinds through it at runtime
    /// ([`Fold::without_output`]), and a setting will later read a number from
    /// configuration. The door is not test-only any more: the key is its first
    /// runtime caller, which is why the gate came off.
    pub fn with(mut self, kind: Kind, rows: usize) -> Self {
        self.rows[kind.slot()] = rows;
        self
    }
}

/// Whether a block of this kind reports a failure — the one thing the fold
/// never gives up (see [`Fold`]'s own doc). The vocabulary is the kinds' own: a
/// tool result fails when it opens with mush's `error:` prefix, and a mush line
/// when it is a `#3 failed: …` report. A brief and a thought are nobody's
/// failure to report.
fn fails(kind: Kind, text: &str) -> bool {
    match kind {
        Kind::Result => text.trim_start().starts_with(FAILED),
        Kind::Mush => report_failed(text),
        Kind::Brief | Kind::Reasoning => false,
    }
}

/// Which kind of block a voice's lines are, or `None` for the human's own
/// words.
///
/// The two exemptions from the fold are named here: the human's lines are
/// theirs, however long, and the model's reply is the conversation's own text.
/// Everything else a pane writes into a transcript is a block with a number.
/// No wildcard arm: a new voice has to be classified here before the crate
/// builds again.
fn voice_kind(voice: Voice) -> Option<Kind> {
    match voice {
        Voice::Human => None,
        Voice::Brief | Voice::Parent => Some(Kind::Brief),
        Voice::Mush => Some(Kind::Mush),
    }
}

/// The folded block a message's own words are, if the fold paints them: the
/// kind whose number bounds the block, and the head its rows are painted with.
///
/// The one classification the painter and the select mode's stop walk share
/// ([`render_message`], [`Stops::of`]): the block the cursor measures is the
/// block the pane painted. `None` for [`voice_kind`]'s two exemptions — the
/// human's own lines, whose words are theirs however long, and the model's
/// reply, which is the conversation's own text — and for every role with no
/// block. The reasoning is outside this too: [`reasoning_rows`] paints it under
/// its own kind, and its rows are no source line the cursor walks.
fn folded_block(message: &Message, voice: Option<Voice>) -> Option<(Kind, Head<'static>)> {
    match message.role.as_str() {
        "tool" => {
            // A result is a file dump, so it arrives folded ([`Kind::Result`]).
            // A result that came back `error: …` — mush's own spelling for a
            // call that was refused or that failed — is not a result, and it was
            // painted exactly like one, with only the word at the front to tell
            // them apart. The mark is the difference now, and it is red, because
            // this is the one kind of line in the transcript that reports
            // something did not happen; its row is the one the fold never gives
            // up, at any number ([`Fold`]).
            let (mark, style) = if message.text().trim_start().starts_with(FAILED) {
                ("  ! ", Style::default().fg(Color::Red))
            } else {
                ("  ", dim())
            };
            Some((Kind::Result, Head::solid(mark, style)))
        }
        "user" => {
            // Mush's own line about a child or a job, and the words another
            // agent addressed to this pane — the brief a child's pane opens
            // with, a parent's steering — are read the same way, each through
            // its own kind of the fold.
            let voice = voice.unwrap_or(Voice::Human);
            let (mark, style) = voice.mark();
            Some((voice_kind(voice)?, Head::spoken(mark, style)))
        }
        _ => None,
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
/// One value because the two styles can differ: a tool result and a working
/// note paint the whole row in one colour, while a voice colours only its mark
/// and leaves the words plain ([`marked`]). Bundled, they are one argument.
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

    /// A mark in its speaker's own style, and the words in the pane's plain
    /// one: the shape [`marked`] paints a voice's rows in.
    fn spoken(mark: &'a str, style: Style) -> Self {
        Self {
            mark,
            mark_style: style,
            body: Style::default(),
        }
    }
}

/// One folded block's rows at the pane's width, and the stop each is the
/// reading of: at most [`Fold::shown`] wrapped rows, and, where the text ran
/// on, the one `…` row [`elision`] spells — whose stop is [`Stop::Tail`].
///
/// A kind the fold gives no rows at all paints none — not even the `…` — and
/// the one exception is the failure row [`Fold::shown`] keeps: a block that
/// reports a failure paints its first row and nothing else ([`Fold`]).
///
/// One walk, shared by the painter ([`folded_marked`]) and the select mode's
/// stop boundary ([`Stops::of`]): the rows the cursor steps over are the rows
/// the pane painted, never a second wrap with arithmetic of its own that could
/// disagree with them. The returned head is the one the rows were wrapped
/// under: a pane too narrow for the mark and a few words drops it, exactly as
/// [`marked`] does for a voice's rows.
///
/// `hidden` is the number of source lines the `…` stands for (the first line no
/// painted row is the reading of, and every line after it), counted without
/// wrapping them: counting painted rows would wrap the very text the fold
/// exists not to wrap, and a line is what a reader counts in a dump anyway.
fn folded_rows<'a>(
    head: Head<'a>,
    text: &str,
    width: usize,
    kind: Kind,
    fold: Fold,
) -> (Head<'a>, Vec<(String, Stop)>) {
    // A mark the pane clips is a row that says who spoke and nothing about what
    // was said, so the words get the whole width instead.
    let head = if width >= head.mark.width() + MIN_BODY {
        head
    } else {
        Head { mark: "", ..head }
    };
    let lead = head.mark.width();
    let wrap = width.saturating_sub(lead);
    let shown = fold.shown(kind, text);
    // Wrapped only as far as the fold: one row past the number is what tells
    // the fold it has more to stand for.
    let wrapped = wrap_text_capped(text, wrap, shown.saturating_add(1));
    let clipped = wrapped.len() > shown;
    // Which source line each painted row is the reading of, and how many lines
    // the block has: the walk wraps only the lines the fold may paint and scans
    // the rest, so the count cannot cost what the fold exists to avoid.
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
    let mut rows: Vec<(String, Stop)> = wrapped
        .into_iter()
        .take(shown)
        .zip(tags.iter().map(|line| Stop::Line(*line)))
        .collect();
    if clipped && !fold.hides(kind) {
        // The `…` is part of *showing* a block: it stands for the first wrapped
        // row the fold did not paint, and it is one stop for every line from
        // there on: one `↓` steps the whole hidden tail, and a selection that
        // reaches the `…` copies it whole. A kind the fold gives no rows at all
        // has no head for it to stand behind — the hidden half of `Ctrl-O` — so
        // nothing is elided there: a `…` on top of the failure row
        // [`Fold::shown`] keeps would count lines the human asked the pane not
        // to show.
        let hidden = total - tags[shown];
        rows.push((elision(hidden), Stop::Tail));
    }
    (head, rows)
}

/// The rows of one folded block, painted: [`folded_rows`]'s walk, laid out with
/// the mark on the first row and its own width of blank under it, exactly as
/// [`marked`] paints a voice's rows.
///
/// The block is always a plain line: a folded block is a dump, a report or a
/// working note, and never the reply the markdown view is for. The `…` row is
/// part of the fold's own sentence rather than the block's words, so it wears
/// the mark's style and carries [`Stop::Tail`] — one stop for every line the
/// fold hid, so the cursor cannot sit on a row the pane never painted.
fn folded_marked(
    out: &mut Vec<Line<'static>>,
    rows: &mut Vec<Option<Stop>>,
    head: Head<'_>,
    text: &str,
    width: usize,
    kind: Kind,
    fold: Fold,
) {
    let start = out.len();
    let base = rows.len();
    let (head, folded) = folded_rows(head, text, width, kind, fold);
    let lead = head.mark.width();
    for (index, (line, stop)) in folded.into_iter().enumerate() {
        let row = if stop == Stop::Tail {
            Line::from(Span::styled(
                format!("{}{}", " ".repeat(lead), line),
                head.mark_style,
            ))
        } else if index == 0 {
            Line::from(vec![
                Span::styled(head.mark.to_string(), head.mark_style),
                Span::styled(line, head.body),
            ])
        } else {
            Line::from(vec![
                Span::styled(" ".repeat(lead), head.body),
                Span::styled(line, head.body),
            ])
        };
        out.push(row);
        rows.push(Some(stop));
    }
    debug_assert_eq!(out.len() - start, rows.len() - base, "one entry per row");
}

/// One message's rows: who said it, wrapped at the pane's width — and, beside
/// them, the [`Stop`] of the message's own text each row is the reading of.
///
/// The map is *returned* rather than kept by the painter because a stop is one
/// or more painted rows, and only the pass that paints a row knows whether the
/// row is a soft wrap of the line above it, a markdown view of it, or the `…`
/// that stands for the tail the fold hid. A second pass that counted them could
/// disagree with the rows on screen, and the cursor would then sit on the wrong
/// one. The caller adds the message's index.
///
/// `reasoning` is the pane's `Ctrl-T` choice and `fold` the pane's [`Fold`] —
/// how much of each kind of block it paints. Both are threaded in rather than
/// read off a `Chat` this free function has no handle on. `summaries` is the
/// same kind of threading for the tool-call labels: one entry per entry of
/// `message.tool_calls()`, the reading [`Chat::call_summaries`] cached, and
/// empty for a caller that has no cache — the label path then takes the
/// reading itself, which is what the cache would have stored.
fn render_message(
    out: &mut Vec<Line<'static>>,
    message: &Message,
    voice: Option<Voice>,
    width: usize,
    reasoning: bool,
    fold: Fold,
    summaries: &[String],
) -> Vec<Option<Stop>> {
    let start = out.len();
    let mut rows: Vec<Option<Stop>> = Vec::new();
    match message.role.as_str() {
        "user" => {
            // The mark is painted even for a message that is only an
            // attachment: the `▣` rows below are *what* was said, whoever said
            // it, and the mark is *who* said it. Without it, a picture the
            // human sent would read exactly like a dim line of mush's own.
            let voice = voice.unwrap_or(Voice::Human);
            match folded_block(message, Some(voice)) {
                // The human's own words: theirs, however long. The fold has no
                // number for them ([`voice_kind`]).
                None => {
                    let (mark, style) = voice.mark();
                    mark_rows(
                        out,
                        &mut rows,
                        mark,
                        style,
                        message.text(),
                        width,
                        View::Plain,
                    )
                }
                // The blocks the fold exists for, each through its own kind:
                // mush's own line about a child or a job, and the words another
                // agent addressed to this pane — the brief a child's pane opens
                // with, a parent's steering — which are read the same way.
                Some((kind, head)) => {
                    folded_marked(out, &mut rows, head, message.text(), width, kind, fold)
                }
            }
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
            for (at, call) in message.tool_calls().iter().enumerate() {
                out.push(Line::from(Span::styled(
                    tool_label(call, width, summaries.get(at).map(String::as_str)),
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
            // A result is a file dump, so [`folded_block`] paints it through
            // [`Kind::Result`]; the failure row it may never give up is the
            // fold's own rule ([`Fold`]).
            if let Some((kind, head)) = folded_block(message, None) {
                folded_marked(out, &mut rows, head, message.text(), width, kind, fold);
            }
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
/// Which source line painted how many rows is the *view's* own answer, not a
/// second walk with the rule restated: the markdown view's count comes from
/// [`mush_core::text::markdown_row_counts`], the map of the same walk that
/// paints the rows, so a fence line that paints a row because its block has no
/// body counts exactly there (finding D14). The `debug_assert` below is what
/// keeps the two row counts from drifting.
fn mark_rows(
    out: &mut Vec<Line<'static>>,
    rows: &mut Vec<Option<Stop>>,
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
    // The markdown view counts its rows with the view's own walk — one entry per
    // source line, fence lines included — and the plain view with one wrap per
    // line, which is the same arithmetic `wrap_text` does on the whole text.
    let counts = match view {
        View::Plain => None,
        View::Markdown => Some(markdown_row_counts(text, wrap)),
    };
    for (line, raw) in text.split('\n').enumerate() {
        let count = match view {
            View::Plain => wrap_text(raw, wrap).len(),
            View::Markdown => counts.as_ref().expect("the map above")[line],
        };
        rows.extend(std::iter::repeat(Some(Stop::Line(line))).take(count));
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

    /// The chat's half of `Ctrl-O`, as `App::toggle_output` runs it: flip the
    /// view the panes paint through. The key itself is pinned in `keys.rs`'s
    /// own table and in the app's test that presses it.
    fn toggle_output(chat: &mut Chat) {
        chat.set_output(!chat.shows_output());
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
        render_message(&mut lines, message, voice, width, reasoning, fold, &[]);
        lines
    }

    /// The window between the two events an actor's first turn is made of: the
    /// prompt is published (`AgentEvent::SystemPrompt`) and the first message
    /// has not arrived yet. The meter, the bar and every attach gate that reads
    /// the room left must weigh the prompt the actor *will* send — the audit
    /// measured this window at 0 against a 3,006-byte prompt, a whole prompt
    /// short (D16), and the bounded view is where it closed. This is the pin.
    #[test]
    fn an_agent_whose_actor_said_its_prompt_weighs_it_even_with_nothing_said() {
        let budget = 500_000;
        let mut chat = Chat::bare();
        assert_eq!(
            chat.used_weight_for(AgentId(1), budget),
            0,
            "no actor has said anything yet: an unpublished prompt weighs nothing"
        );

        // The actor's own prompt, published before the run it opens.
        let prompt = Message::system("x".repeat(3_000).as_str());
        let prompt_weight = prompt.weight();
        assert_eq!(prompt_weight, 3_006, "the audit's own measurement");
        chat.learn_system(AgentId(1), prompt);
        assert_eq!(
            chat.used_weight_for(AgentId(1), budget),
            prompt_weight,
            "nothing said yet: the prompt is the whole weight"
        );

        // The first line joins the prompt, it does not replace it.
        chat.push_message(AgentId(1), Message::user("hi"));
        assert_eq!(
            chat.used_weight_for(AgentId(1), budget),
            prompt_weight + Message::user("hi").weight(),
            "the audit's after one line: 3,006 + 6"
        );
        assert_eq!(chat.used_weight_for(AgentId(1), budget), 3_012);
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

    /// A reply that is nothing but fence lines is a turn the human must be able
    /// to see: a fence line paints its own row when its block has no body, so
    /// the pane shows the three backticks the model wrote instead of a `mush › `
    /// mark over nothing — and the select mode advertises `Enter copies` only
    /// where there is a cursor to see (finding D14).
    #[test]
    fn a_reply_of_only_a_fence_is_not_an_invisible_turn() {
        for text in ["```", "```\n```", "```\n\n```"] {
            let mut chat = Chat::bare();
            chat.push_message(AgentId::ROOT, Message::assistant(text));
            let pane = pane(AgentId::ROOT);
            let rows = shown(&pane_rows(&chat, &pane, 40, 10));
            assert!(
                rows.iter().any(|row| row.contains("```")),
                "{text:?} paints its fence lines: {rows:?}"
            );
            assert!(chat.start_select(AgentId::ROOT).is_none(), "the mode is on");
            let painted = chat.painted(&pane, 40, 10);
            assert!(
                painted
                    .select
                    .as_ref()
                    .is_some_and(|select| !select.cursor.is_empty()),
                "{text:?} paints the cursor where it says it is: {:?}",
                painted.title
            );
        }

        // The copy is still the source, so the fence bytes the human selects
        // are the model's own — the view only decided how to paint them.
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::assistant("```"));
        chat.start_select(AgentId::ROOT);
        let copied = chat
            .select_apply(AgentId::ROOT, SelectKey::Copy)
            .expect("Enter copies");
        assert_eq!(copied.text, "```");
        assert_eq!(copied.line, "copied 1 line from #0's reply — 3 bytes");
    }

    /// The frame may never confuse "no mode" with "mode with no visible row":
    /// every source line the pane calls selectable paints a cursor somewhere.
    /// A reply's fence line inside a block that *does* have a body is the case
    /// that used to paint none at all while the title promised `Enter copies`
    /// (finding D14); a folded result's hidden tail and a wrapped paragraph are
    /// the same invariant's other shapes.
    #[test]
    fn the_select_cursor_has_a_row_on_every_line_lines_of_names() {
        let long = (0..20)
            .map(|n| format!("line {n}: a\tb"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut chat = Chat::bare();
        say(&mut chat, AgentId::ROOT, "a human line\nand a second");
        chat.push_message(AgentId::ROOT, Message::assistant("```\nlet x = 1;\n```"));
        chat.push_message(
            AgentId::ROOT,
            Message::assistant(
                "a plain reply, long enough to wrap over a few rows at this width and then some",
            ),
        );
        chat.push_message(AgentId::ROOT, Message::tool("call_1", &long));
        let pane = pane(AgentId::ROOT);
        assert!(chat.start_select(AgentId::ROOT).is_none(), "the mode is on");

        let transcript = chat.transcript(AgentId::ROOT).to_vec();
        for (index, message) in transcript.iter().enumerate() {
            let Some(lines) = lines_of(message) else {
                continue;
            };
            for (line, source) in lines.iter().enumerate() {
                chat.select.as_mut().expect("the mode is on").cursor = (index, Stop::Line(line));
                let painted = chat.painted(&pane, 20, 8);
                let select = painted.select.expect("the mode paints a cursor");
                assert!(
                    !select.cursor.is_empty(),
                    "message {index} line {line} ({source:?}) has no cursor row: {}",
                    shown(&painted.lines).join(" / ")
                );
            }
        }
    }

    /// A tool result is copied whole, byte for byte, including the lines the
    /// fold hides: the fold bounds the frame, not the transcript — and a
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

    /// A line behind a folded result's cap still has a row to stand on — the `…`
    /// that hides it, which the frame clamps the cursor onto — and that row is
    /// the whole hidden tail's one stop: the copy takes every line it stands
    /// for, because the fold is the pane's, not the transcript's.
    #[test]
    fn a_line_behind_a_tool_results_cap_stands_on_the_ellipsis() {
        let result = (0..12)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::tool("call_1", &result));
        chat.start_select(AgentId::ROOT);
        // The pane's own measure first: that is where its cap falls, and the
        // hidden lines are one stop only once it has painted one.
        let pane = pane(AgentId::ROOT);
        chat.painted(&pane, 40, 8);
        chat.select_apply(AgentId::ROOT, SelectKey::First);
        chat.select_apply(AgentId::ROOT, SelectKey::Move(11));
        let painted = chat.painted(&pane, 40, 8);
        let cursor = painted.select.as_ref().expect("painted").cursor.clone();
        assert_eq!(cursor.len(), 1, "the hidden tail has one row to stand on");
        assert!(
            shown(&painted.lines[cursor[0]..=cursor[0]])[0].contains('…'),
            "and it is the ellipsis"
        );
        let copied = chat
            .select_apply(AgentId::ROOT, SelectKey::Copy)
            .expect("Enter copies");
        assert_eq!(
            copied.text, "line 8\nline 9\nline 10\nline 11",
            "the whole tail, not the one line the press landed on"
        );
        assert_eq!(
            copied.line,
            "copied 4 lines from #0's tool result — 29 bytes"
        );
    }

    /// The `…` a folded block's cap paints is one stop, not one stop per line it
    /// hides: from the result's last painted line, one `↓` reaches the elided
    /// stop (the cursor's own row is the `…`), the next `↓` is the following
    /// message's first line, and `↑` walks back the same way. Before this, a
    /// 30-line result cost 22 `↓` presses on that same `…` row.
    #[test]
    fn the_cursor_steps_over_an_elided_tail_in_one() {
        let result = (0..30)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::tool("call_1", &result));
        chat.push_message(AgentId::ROOT, Message::assistant("after the result"));
        chat.start_select(AgentId::ROOT);
        let pane = pane(AgentId::ROOT);
        // The pane's measure, published by the paint that shows the cursor on
        // the newest line.
        let row = |chat: &Chat| {
            let painted = chat.painted(&pane, 40, 12);
            let select = painted.select.as_ref().expect("the mode paints");
            shown(&painted.lines)[select.cursor[0]].clone()
        };
        assert!(row(&chat).contains("after the result"), "the newest line");

        // Back over the elided stop in one: the `…` row, then the result's last
        // painted line — not another hidden line under the same `…`.
        chat.select_apply(AgentId::ROOT, SelectKey::Move(-1));
        assert!(
            row(&chat).contains('…'),
            "one step back is the elided stop: {:?}",
            row(&chat)
        );
        chat.select_apply(AgentId::ROOT, SelectKey::Move(-1));
        assert!(
            row(&chat).contains("line 7"),
            "and the next is line 7, the last line the pane painted: {:?}",
            row(&chat)
        );

        // Forward the same way: one `↓` is the elided stop, the next is the
        // message after the result.
        chat.select_apply(AgentId::ROOT, SelectKey::Move(1));
        assert!(row(&chat).contains('…'), "down is the elided stop again");
        chat.select_apply(AgentId::ROOT, SelectKey::Move(1));
        assert!(
            row(&chat).contains("after the result"),
            "and down again is the next message: {:?}",
            row(&chat)
        );

        // And `↑` reverses it exactly.
        chat.select_apply(AgentId::ROOT, SelectKey::Move(-1));
        assert!(row(&chat).contains('…'), "up is the elided stop");
        chat.select_apply(AgentId::ROOT, SelectKey::Move(-1));
        assert!(row(&chat).contains("line 7"), "and up again is line 7");
    }

    /// `Shift-↓` onto the elided stop selects the whole block it stands for:
    /// `Enter` hands the writer every source line behind the `…`, in order and
    /// byte for byte — the message's own text, not the pane's screen — and the
    /// line mush says counts them. Before this, the same gesture copied the one
    /// line the first `…` press landed on.
    #[test]
    fn shift_over_the_ellipsis_copies_the_whole_tail() {
        let result = (0..30)
            .map(|n| format!("line {n}: a\tb"))
            .collect::<Vec<_>>()
            .join("\n");
        let message = Message::tool("call_1", &result);
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, message.clone());
        chat.push_message(AgentId::ROOT, Message::assistant("after the result"));
        chat.start_select(AgentId::ROOT);
        // The pane's measure, then the cursor onto the result's last painted
        // line: the stop above it is the elided one.
        let pane = pane(AgentId::ROOT);
        chat.painted(&pane, 40, 12);
        chat.select_apply(AgentId::ROOT, SelectKey::Move(-1)); // the elided stop
        chat.select_apply(AgentId::ROOT, SelectKey::Move(-1)); // line 7
        chat.select_apply(AgentId::ROOT, SelectKey::Extend(1)); // onto the `…`
        let copied = chat
            .select_apply(AgentId::ROOT, SelectKey::Copy)
            .expect("Enter copies");
        let lines: Vec<&str> = message.text().split('\n').collect();
        let tail = lines[7..].join("\n");
        assert_eq!(copied.text, tail, "lines 7 on, source for source");
        assert!(
            copied.text.ends_with(&lines[8..].join("\n")),
            "every hidden line is in it, in order"
        );
        assert_eq!(
            copied.line,
            format!(
                "copied {} lines from #0's tool result — {} bytes",
                lines.len() - 7,
                tail.len()
            )
        );
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

    /// A fold is a new transcript, and a held window belonged to the old one:
    /// the pane is back at the bottom when the fold lands, and — the half that
    /// used to fail — growth back past the old length does not resurrect the
    /// hold. A position in a conversation that no longer exists is not a
    /// position (finding D15).
    #[test]
    fn a_fold_puts_every_pane_back_at_the_bottom() {
        let mut chat = Chat::bare();
        for i in 0..10 {
            chat.push_message(AgentId::ROOT, Message::assistant(format!("line {i}")));
        }
        // A second conversation, scrolled too: a fold of one is not the other's
        // to move.
        for i in 0..6 {
            chat.push_message(AgentId(1), Message::assistant(format!("child {i}")));
        }
        chat.scroll_by(AgentId::ROOT, 3);
        chat.scroll_by(AgentId(1), 2);
        let root = pane(AgentId::ROOT);
        let child = pane(AgentId(1));
        assert!(
            chat.painted(&root, 40, 6)
                .title
                .contains("scrolled ↑3 rows"),
            "the root is holding a window"
        );

        // The fold: `[user(summary)]` replaces the root's ten lines.
        chat.replace_transcript(AgentId::ROOT, vec![Message::user("a summary")]);
        assert_eq!(
            chat.painted(&root, 40, 6).title,
            " mush ",
            "the pane is at the bottom again"
        );

        // Growth back past the old `up_to` — the resurrection the finding read
        // as `scrolled ↑3 rows · PgDn` after a fold.
        for i in 0..11 {
            chat.push_message(AgentId::ROOT, Message::assistant(format!("new {i}")));
        }
        assert_eq!(
            chat.painted(&root, 40, 6).title,
            " mush ",
            "the hold does not come back with the lines"
        );
        assert!(
            chat.painted(&child, 40, 6)
                .title
                .contains("scrolled ↑2 rows"),
            "and the child's reading is not the root's fold's to drop"
        );
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
        chat.select.as_mut().expect("the mode is on").cursor = (9, Stop::Line(4));

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

    /// A tool call's arguments are read once, not once per frame: the label a
    /// frame paints is the reading the call was recorded with, and a call whose
    /// argument JSON is unbounded — H45/H46 left `write_file`'s `content` at
    /// whatever the model sends — costs the pane the frame that first shows it
    /// and no other (finding A13).
    #[test]
    fn a_tool_calls_arguments_are_read_once_not_once_per_frame() {
        let mut chat = Chat::bare();
        let call = |arguments: &str| mush_core::ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: mush_core::FunctionCall {
                name: "write_file".into(),
                arguments: arguments.into(),
            },
        };
        let big = format!(r#"{{"path":"a.txt","content":"{}"}}"#, "x".repeat(1 << 20));
        chat.push_message(
            AgentId::ROOT,
            Message {
                role: "assistant".into(),
                tool_calls: Some(vec![call(&big)]),
                ..Default::default()
            },
        );
        let first = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 120, 4)).join("\n");
        assert!(first.contains("a.txt"), "the label is the path: {first:?}");

        // The arguments behind the pane change — a state no transcript road
        // writes, and the only way to tell a re-read from a stored reading.
        if let Some(calls) = chat
            .root
            .last_mut()
            .and_then(|message| message.tool_calls.as_mut())
        {
            calls[0].function.arguments = r#"{"path":"somewhere/else.rs"}"#.into();
        }
        let second = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 120, 4)).join("\n");
        assert_eq!(
            first, second,
            "the frame paints the reading the call was recorded with"
        );

        // A replacement is a new transcript, and the readings keyed by the old
        // one's indices go with it: the same index in the new transcript must
        // not answer with the call that used to sit there.
        chat.replace_transcript(
            AgentId::ROOT,
            vec![Message {
                role: "assistant".into(),
                tool_calls: Some(vec![call(r#"{"path":"new.rs"}"#)]),
                ..Default::default()
            }],
        );
        let replaced = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 120, 4)).join("\n");
        assert!(
            replaced.contains("new.rs") && !replaced.contains("a.txt"),
            "a replaced transcript is read on its own: {replaced:?}"
        );
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

    /// The other half of the guard's one line: a stop is news on the run — the
    /// pane paints it until a newer run replaces it — but it is not the line a
    /// restart owes. The session stores the *status* (`Stopped`), and a
    /// restored process paints it as the row's `Phase::Stopped` rather than as
    /// a failure's red `!`. The module doc claimed both halves of News were
    /// written to the session — "a restart still says what broke" — and that
    /// false sentence is what finding D17 was (the probe: `kind=Some(Stopped)
    /// stored_notices=0`).
    #[test]
    fn a_stopped_run_is_either_stored_as_a_stop_or_not_claimed_to_be() {
        let mut chat = Chat::bare();
        // The two lines one loop guard writes, in its order: the notice as it
        // fires, the stop when the run ends a moment later.
        chat.note_for(AgentId::ROOT, LOOP_NOTICE);
        chat.note_error_for(
            AgentId::ROOT,
            "the run was stopped as a loop: the same tool call repeated 5 times",
        );

        // News on the run: the line is still there, and it is marked a stop —
        // the agent's next run ends it, no clock takes it.
        let notice = chat
            .notices_for(AgentId::ROOT)
            .next()
            .expect("the stop is on the run");
        assert_eq!(notice.kind, NoticeKind::Stopped);

        // And the store claims nothing: the failures are the line a restart
        // owes, and this is not a failure. The stop survives as the row's
        // stored status, which is `tree.rs`'s fact (`Phase::Stopped`).
        let stored: Vec<String> = chat
            .stored_notices()
            .into_iter()
            .map(|notice| notice.text)
            .collect();
        assert!(stored.is_empty(), "{stored:?}");
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
        chat.push_message(AgentId::ROOT, Message::note(transcript::DROPPED_TURNS_NOTE));
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

    /// A dropped-turns note lands where the transcript's *front* is, not where
    /// the line arrived (finding A18): the actor emits it at the message
    /// boundary where the trim cut, so the pane hears it after the turns the
    /// request kept — and a note at the end reads as the newest thing said
    /// rather than as a statement about what the front lost. The pane and the
    /// request must read the same transcript, so the pane uses the request's own
    /// rule ([`transcript::place_dropped_note`]), which reads the front off the
    /// list it is given: a request opens with the system prompt and the task,
    /// while the pane's copy carries the task and no prompt.
    #[test]
    fn a_dropped_turns_note_lands_after_the_brief_and_before_the_oldest_kept_turn() {
        let words = |messages: &[Message]| -> Vec<String> {
            messages.iter().map(|m| m.text().to_string()).collect()
        };

        // A child's road: its transcript opens with the brief, and the note
        // arrives at the end, where the append puts what it is told.
        let mut child = Chat::bare();
        child.push_message(AgentId(1), Message::user("the brief"));
        child.push_message(AgentId(1), Message::assistant("reading"));
        child.push_message(AgentId(1), Message::user("more"));
        child.push_message(AgentId(1), Message::note(transcript::DROPPED_TURNS_NOTE));
        let order = words(child.transcript(AgentId(1)));
        assert_eq!(
            order,
            vec![
                "the brief",
                transcript::DROPPED_TURNS_NOTE,
                "reading",
                "more"
            ],
            "after the brief and before the oldest kept turn"
        );
        let rows = shown(&pane_rows(&child, &pane(AgentId(1)), 60, 20));
        let at = |needle: &str| {
            rows.iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("no `{needle}` row: {rows:?}"))
        };
        assert!(
            at("The oldest turns") < at("mush › reading"),
            "the pane paints the note before the turns it explains: {rows:?}"
        );

        // The root's road: the opening task is the front there is no brief for.
        let mut root = Chat::bare();
        say(&mut root, AgentId::ROOT, "do the work");
        root.push_message(AgentId::ROOT, Message::assistant("working"));
        root.push_message(AgentId::ROOT, Message::note(transcript::DROPPED_TURNS_NOTE));
        assert_eq!(
            words(root.transcript(AgentId::ROOT)),
            vec!["do the work", transcript::DROPPED_TURNS_NOTE, "working"],
            "after the opening task, before the oldest kept turn"
        );

        // And the pane's order *is* the request's: the bounded view is the
        // pane's copy with the prompt in front ([`Chat::bounded_transcript`]),
        // so the note sits at the same place in both and nothing is a second
        // derivation of the front.
        let view = root.bounded_transcript(AgentId::ROOT, usize::MAX);
        assert_eq!(view[0].role, "system", "the request opens with the prompt");
        assert_eq!(
            words(&view[1..]),
            words(root.transcript(AgentId::ROOT)),
            "the request's order is the pane's order"
        );

        // The road a stored copy comes by: a file that holds the note after the
        // newest line is normalized by the same placement before it is painted.
        let mut restored = Chat::bare();
        restored.replace_transcript(
            AgentId(1),
            vec![
                Message::user("the brief"),
                Message::assistant("reading"),
                Message::note(transcript::DROPPED_TURNS_NOTE),
            ],
        );
        assert_eq!(
            words(restored.transcript(AgentId(1))),
            vec!["the brief", transcript::DROPPED_TURNS_NOTE, "reading"],
            "a restored copy lands the note after the brief too"
        );

        // The messages the note passed keep their *voice*: a parent's steering
        // was recorded at the index it arrived at, and the note's placement
        // must move that record with the line it describes — an index left
        // behind would paint the note in the parent's voice.
        let mut steered = Chat::bare();
        steered.push_message(AgentId(1), Message::user("the brief"));
        steered.push_message(AgentId(1), Message::user("keep the steps small"));
        steered.push_message(AgentId(1), Message::note(transcript::DROPPED_TURNS_NOTE));
        let rows = shown(&pane_rows(&steered, &pane(AgentId(1)), 60, 20));
        assert!(
            rows.iter()
                .any(|row| row == "parent › keep the steps small"),
            "the steering keeps the voice it was recorded with: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.starts_with("· The oldest turns")),
            "and the note still reads as mush's: {rows:?}"
        );
    }

    /// The note's voice survives the session file: the flag is what tells it
    /// from a human's line ([`Message::note`]), and the file is what carries
    /// the flag across a restart — so a note read back is painted in mush's
    /// voice, while a human's line that is word for word the note, restored
    /// through the same road, is still the human's.
    #[test]
    fn a_restored_note_reads_as_mushs_line_and_a_lookalike_does_not() {
        let stored = mush_core::Session {
            messages: vec![
                Message::user("port the parser"),
                Message::user(transcript::DROPPED_TURNS_NOTE),
                Message::note(transcript::DROPPED_TURNS_NOTE),
            ],
            ..mush_core::Session::default()
        };
        let restored: mush_core::Session =
            serde_json::from_str(&serde_json::to_string(&stored).unwrap()).unwrap();

        let mut chat = Chat::bare();
        chat.replace_transcript(AgentId::ROOT, restored.messages);

        let rows = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 60, 12));
        assert!(
            rows.iter()
                .any(|row| row.starts_with("you › The oldest turns")),
            "the human's lookalike keeps their voice: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.starts_with("· The oldest turns")),
            "and the note is mush's: {rows:?}"
        );
        assert_eq!(
            rows.iter().filter(|row| row.starts_with("you ›")).count(),
            2,
            "the human's two lines and no more: {rows:?}"
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

    /// Mush's words *to* a run are marked at the shape the actor hands over —
    /// [`Message::mush`], the one constructor that sets the flag — and the pane
    /// reads the mark, never the sentence, because every one of these lines is
    /// a sentence a human could type word for word (finding F3). Before the
    /// flag, a child's pane painted all three `parent › ` — the parent the
    /// model was obeying looked like the one that had spoken — and the root's
    /// pane painted them `parent › ` too, for a root that has no parent.
    #[test]
    fn mushs_out_of_band_lines_are_painted_in_mushs_voice() {
        // The three sentences as the actor pushes them: the loop guard's
        // warning before a resumed run, and what the model is told after its
        // last reply could not be read, and after it was cut off at the cap.
        const LINES: [&str; 3] = [
            "Your previous run was stopped as a loop: the same tool call repeated 3 times \
             with nothing changed in between. Do not repeat that call — change what you do \
             (different arguments, a different approach, or a wait for whatever it was \
             blocked on), or finish the run and say what you need.",
            "mush could not read the endpoint's last reply, so nothing of it was recorded. \
             Answer the message before this one again: a tool call with its arguments \
             written as JSON, or the answer as plain text.",
            "Your previous reply was cut off by the endpoint's length limit, so none of it \
             ran. Do the same work in smaller steps: one file or edit per call, a few hundred \
             lines at a time (create a file with a heredoc — `cat > file <<'EOF'` — then \
             extend it with `edit_file`). Do not repeat work you already completed in \
             earlier calls.",
        ];
        for line in LINES {
            let head = &line[..24];

            // A child's transcript: the brief, the model's reply, the line. It
            // reads as mush's, not as the parent's.
            let mut child = Chat::bare();
            child.push_message(AgentId(1), Message::user("the brief"));
            child.push_message(AgentId(1), Message::assistant("reading"));
            child.push_message(AgentId(1), Message::mush(line));
            let rows = shown(&pane_rows(&child, &pane(AgentId(1)), 200, 40));
            assert!(
                rows.iter()
                    .any(|row| row.starts_with("· ") && row.contains(head)),
                "mush's voice on a child's pane: {rows:?}"
            );
            assert!(
                !rows.iter().any(|row| row.contains("parent › ")),
                "and not the parent's, which is what nobody-marked reads as: {rows:?}"
            );

            // The root's pane: there is no parent to be mistaken for, and the
            // line is still mush's rather than the human asking themselves
            // what the endpoint did.
            let mut root = Chat::bare();
            say(&mut root, AgentId::ROOT, "go");
            root.push_message(AgentId::ROOT, Message::mush(line));
            let rows = shown(&pane_rows(&root, &pane(AgentId::ROOT), 200, 40));
            assert!(
                rows.iter()
                    .any(|row| row.starts_with("· ") && row.contains(head)),
                "mush's voice on the root's pane too: {rows:?}"
            );
            assert!(
                rows.iter().any(|row| row == "you › go"),
                "and the human keeps their own words: {rows:?}"
            );
        }

        // The words decide nothing (finding F3): the same sentence pushed as
        // an ordinary user line keeps the fallback voice — the parent's on a
        // child's transcript. This is the read the flag replaced.
        let mut lookalike = Chat::bare();
        lookalike.push_message(AgentId(1), Message::user("the brief"));
        lookalike.push_message(AgentId(1), Message::user(LINES[0]));
        let rows = shown(&pane_rows(&lookalike, &pane(AgentId(1)), 200, 20));
        assert!(
            rows.iter()
                .any(|row| row.starts_with("parent › Your previous run")),
            "an unmarked lookalike stays whoever the pane falls back to: {rows:?}"
        );

        // The mark is read at paint time, so a transcript handed over whole —
        // the restore road — paints the same line the same way.
        let mut restored = Chat::bare();
        restored.replace_transcript(
            AgentId(1),
            vec![
                Message::user("the brief"),
                Message::mush(LINES[1]),
                Message::user("and carry on"),
            ],
        );
        let rows = shown(&pane_rows(&restored, &pane(AgentId(1)), 200, 20));
        assert!(
            rows.iter().any(|row| row.starts_with("· ")),
            "a restored marked line is still mush's: {rows:?}"
        );
    }

    /// A failed commit's line is mush's own report about mush's own act, so the
    /// pane paints it in mush's voice — never as the parent's, which is what an
    /// unmarked line on a *child's* transcript reads as, and never as the
    /// human's on the root's. The line is the sentence `Work::status_line`
    /// writes for `Uncommitted` and the sentence the model reads in the request
    /// (finding F17); the writer marks it [`Message::mush`] ([`report_work`],
    /// pinned by the agent's own tests), so neither the sentence's head nor
    /// git's error behind it is read for provenance.
    #[test]
    fn a_failed_commits_line_is_painted_in_mushs_voice() {
        let line = "could not commit the worktree: /repo/.mush/wt/1 is no longer a worktree";

        // A child's transcript: the brief, the model's reply, then the report
        // the run's end leaves. Before, the report read `parent ›`.
        let mut chat = Chat::bare();
        chat.push_message(AgentId(1), Message::user("the brief"));
        chat.push_message(AgentId(1), Message::assistant("reading"));
        chat.push_message(AgentId(1), Message::mush(line));
        let rows = shown(&pane_rows(&chat, &pane(AgentId(1)), 60, 12));
        let painted: Vec<&String> = rows
            .iter()
            .filter(|row| row.contains("could not commit"))
            .collect();
        assert_eq!(painted.len(), 1, "the line is painted once: {rows:?}");
        assert!(
            painted[0].starts_with("· "),
            "mush's voice, not the parent's: {rows:?}"
        );

        // The root's pane: no parent exists there, and the line is still mush's
        // — not the human asking themselves what happened to the worktree.
        let mut root = Chat::bare();
        say(&mut root, AgentId::ROOT, "commit the work");
        root.push_message(AgentId::ROOT, Message::mush(line));
        let rows = shown(&pane_rows(&root, &pane(AgentId::ROOT), 60, 12));
        assert!(
            rows.iter().any(|row| row.starts_with("· could not commit")),
            "mush's voice on the root's road too: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row == "you › commit the work"),
            "and the human keeps their own words: {rows:?}"
        );

        // The read is the paint-time one, so a transcript restored from the
        // session file — which arrives without the voices a live push recorded
        // — paints the same line the same way: the flag is what the file keeps
        // (`message.rs`'s own test pins the round trip).
        let mut restored = Chat::bare();
        restored.replace_transcript(
            AgentId(1),
            vec![Message::user("the brief"), Message::mush(line)],
        );
        let rows = shown(&pane_rows(&restored, &pane(AgentId(1)), 60, 12));
        assert!(
            rows.iter().any(|row| row.starts_with("· could not commit")),
            "the mark is read from the stored line itself: {rows:?}"
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
        // A window no transcript here fills: these tests are about the
        // arithmetic, not about where the trim lands.
        let budget = 1 << 20;
        let mut chat = Chat::bare();
        let idle = chat.used_tokens_for(AgentId::ROOT, budget);
        assert_eq!(
            idle,
            chat.system().weight() / mush_core::config::BYTES_PER_TOKEN,
            "the system prompt alone, for a conversation with nothing said"
        );

        let asked = Message::user("a question long enough to weigh something");
        chat.push_message(AgentId::ROOT, asked.clone());
        assert!(
            chat.used_tokens_for(AgentId::ROOT, budget) > idle,
            "the meter must count the human's own message"
        );

        // Compaction replaces the transcript with a summary; the meter follows
        // the transcript, because it is the transcript.
        let summary = Message::user("a summary");
        chat.replace_transcript(AgentId::ROOT, vec![summary.clone()]);
        assert_eq!(
            chat.used_tokens_for(AgentId::ROOT, budget),
            (chat.system().weight() + summary.weight()) / mush_core::config::BYTES_PER_TOKEN,
            "the meter reads what is there now"
        );

        // A subagent's transcript is not the root's conversation, so it does
        // not weigh on it.
        chat.push_message(AgentId(1), Message::assistant("x".repeat(1000)));
        assert_eq!(
            chat.used_tokens_for(AgentId::ROOT, budget),
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
        render_message(&mut rows, &message, None, 60, false, Fold::DEFAULT, &[]);
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
            &[],
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
            &[],
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
            render_message(&mut rows, &message, None, width, false, Fold::DEFAULT, &[]);
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

    /// The human's ask: a child's or a job's report arriving in the parent's
    /// conversation folds like a command's result, through the one [`Fold`] —
    /// at most its number of rows, the `…` row saying how much is hidden — and
    /// the select mode still hands out the report's own bytes.
    ///
    /// Before this, a report was a `user`-role line painted by `mark_rows`,
    /// which has no cap at all: the 25-line report below painted 26 rows,
    /// where the same text as a tool result painted ten (eight rows, the `…`,
    /// the blank).
    #[test]
    fn a_childs_report_folds_like_a_commands_result() {
        let report = (0..25)
            .map(|n| format!("#1 done: line {n} of the report"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::user(&report));

        // Eight wrapped rows, the `…` that stands for the rest; the pane trims
        // the blank that closes the message when the transcript ends there.
        let painted = shown(&pane_rows(&chat, &pane(AgentId::ROOT), 60, 20));
        let mut want: Vec<String> = (0..8)
            .map(|n| {
                if n == 0 {
                    format!("· #1 done: line {n} of the report")
                } else {
                    format!("  #1 done: line {n} of the report")
                }
            })
            .collect();
        want.push("  … +17 more lines".to_string());
        assert_eq!(painted, want, "the report folds like a result");

        // The cap is the pane's; the copy's is the text. `Ctrl-Y`'s `Enter`
        // still hands out every byte of the report, the folded lines included.
        chat.start_select(AgentId::ROOT);
        chat.select_apply(AgentId::ROOT, SelectKey::Extend(-3 * keys::PAGE));
        let copied = chat
            .select_apply(AgentId::ROOT, SelectKey::Copy)
            .expect("Enter copies");
        assert_eq!(copied.text, report, "the report's own bytes, folded or not");
        assert_eq!(
            copied.line,
            format!("copied 25 lines from your message — {} bytes", report.len())
        );
    }

    /// Every multi-line block a pane writes goes through the fold, each kind
    /// with its own number: a tool result, mush's own line about a child or a
    /// job, and the brief a child's pane opens with to eight rows; the
    /// reasoning to its own slot, which is `usize::MAX` — shown whole, because
    /// that is the text the human pressed `Ctrl-T` to read, and folded the day
    /// a setting lowers the number.
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
        for kind in [Kind::Result, Kind::Mush, Kind::Brief] {
            assert_eq!(fold.shown(kind, &many), 8, "{kind:?}");
        }
        assert_eq!(
            fold.shown(Kind::Reasoning, &many),
            usize::MAX,
            "a thought is shown whole until a setting says otherwise"
        );

        // The result, the report and the brief each paint eight rows and the
        // `…` — the same ten rows with the blank, whatever their mark.
        let result = message_rows(&Message::tool("call_1", &many), None, 60, true);
        let report = message_rows(&Message::user(&many), Some(Voice::Mush), 60, true);
        let brief = message_rows(&Message::user(&many), Some(Voice::Brief), 60, true);
        for painted in [&result, &report, &brief] {
            let rows = shown(painted);
            assert_eq!(rows.len(), 10, "eight rows and the `…`: {rows:?}");
            assert!(
                rows[8].ends_with("… +17 more lines"),
                "the ninth row names what is hidden: {rows:?}"
            );
            assert_eq!(rows[9], "", "and the blank closes the message");
        }
        assert_eq!(shown(&result)[0], "  line 0", "the result's own indent");
        assert_eq!(shown(&report)[0], "· line 0", "mush's own mark");
        assert_eq!(shown(&brief)[0], "brief › line 0", "the brief's mark");

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
    /// it lives on [`Fold`], not in an arm, so the `Ctrl-O` view's no-rows
    /// state still paints the `! error: …` result's own row and the
    /// `#1 failed: …` report's first row.
    ///
    /// And a `0`-rows kind paints nothing else, not even the `…`: the elision
    /// row is part of *showing* a block — it stands for the tail behind a head
    /// the fold painted — so a `…` there would count the very lines the human
    /// asked the pane not to show.
    #[test]
    fn a_failure_is_never_what_the_fold_gives_up() {
        let zero = Fold::DEFAULT.with(Kind::Result, 0).with(Kind::Mush, 0);

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
            vec!["  ! error: the call was refused".to_string(), String::new(),]
        );

        // The same text as a *success* paints nothing at all at 0: the
        // exemption is the block's, not the setting's.
        let ok = "wrote three lines\nthe log line one\nthe log line two";
        let rows = shown(&message_rows_under(
            &Message::tool("call_1", ok),
            None,
            60,
            true,
            zero,
        ));
        assert_eq!(rows, vec![String::new()]);

        // And a child's failed report, painted through the pane the human
        // reads: the fold rides on `Chat`, so this is the whole road.
        let mut chat = Chat::bare();
        chat.fold = zero;
        chat.push_message(
            AgentId::ROOT,
            Message::user("#1 failed: no route to the endpoint\nthe run's own log"),
        );
        assert_eq!(
            shown(&pane_rows(&chat, &pane(AgentId::ROOT), 60, 8)),
            vec!["· #1 failed: no route to the endpoint".to_string()]
        );
    }

    /// The two blocks the fold never touches, whatever it says: the human's own
    /// lines, because their words are theirs however long, and the model's
    /// reply, because it is the conversation's own text. A `0`-rows setting for
    /// every other kind leaves both whole.
    #[test]
    fn the_humans_lines_and_the_reply_are_never_folded() {
        let many = (0..25)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let zero = Fold::DEFAULT
            .with(Kind::Result, 0)
            .with(Kind::Mush, 0)
            .with(Kind::Brief, 0)
            .with(Kind::Reasoning, 0);

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

    /// The human's ask: a way out if all they want to see is the main model's
    /// output. `Ctrl-O` hides the output rows — a tool's result and mush's own
    /// report about a child — and leaves every other row exactly where it was.
    /// The assistant's turn keeps its `⚙ name args` labels, so the human can
    /// still see that a call happened, and there is no `…` left behind
    /// counting what the pane no longer shows. The same key brings the same
    /// rows back.
    #[test]
    fn ctrl_o_hides_and_shows_command_output() {
        let mut chat = Chat::bare();
        chat.push_message(
            AgentId::ROOT,
            Message {
                tool_calls: Some(vec![mush_core::ToolCall {
                    id: "call_1".into(),
                    kind: "function".into(),
                    function: mush_core::FunctionCall {
                        name: "run_command".into(),
                        arguments: r#"{"command":"cargo test"}"#.into(),
                    },
                }]),
                ..Message::assistant("running the tests")
            },
        );
        chat.push_message(
            AgentId::ROOT,
            Message::tool(
                "call_1",
                "test result: ok. 3 passed\nthe log line one\nthe log line two",
            ),
        );
        chat.push_message(
            AgentId::ROOT,
            Message::user("#1 done: the parser is written"),
        );
        let pane = pane(AgentId::ROOT);

        let before = shown(&pane_rows(&chat, &pane, 60, 20));
        let before_title = chat.painted(&pane, 60, 20).title;
        assert!(
            before.iter().any(|row| row.contains("test result: ok")),
            "the result is shown by default: {before:?}"
        );
        assert!(
            before.iter().any(|row| row.contains("#1 done:")),
            "and so is the child's report: {before:?}"
        );

        toggle_output(&mut chat);
        let hidden = shown(&pane_rows(&chat, &pane, 60, 20));
        assert_eq!(
            hidden,
            vec![
                "mush › running the tests".to_string(),
                "  ⚙ run_command cargo test".to_string(),
            ],
            "the call's own label and the reply, and nothing of the output"
        );
        for row in &hidden {
            assert!(
                before.contains(row),
                "every row that stayed was left as it was: {row:?}"
            );
        }
        assert!(
            !hidden.iter().any(|row| row.contains('…')),
            "nothing counts the rows the human asked not to see: {hidden:?}"
        );
        assert_eq!(
            chat.painted(&pane, 60, 20).title,
            before_title,
            "and the pane's title claims nothing about a fold: no count, no hint"
        );

        toggle_output(&mut chat);
        assert_eq!(
            shown(&pane_rows(&chat, &pane, 60, 20)),
            before,
            "the second press restores exactly the rows that were there"
        );
    }

    /// The failure exemption holds at the view's zero: a failed result's
    /// `! error: …` row and a `#1 failed: …` report's first row are painted in
    /// both states, because a hidden failure would be a lie about what
    /// happened. They are painted *alone*: the log behind them is output, and
    /// the hidden state has no `…` for it either.
    #[test]
    fn ctrl_o_never_hides_a_failure() {
        let mut chat = Chat::bare();
        chat.push_message(
            AgentId::ROOT,
            Message::tool(
                "call_1",
                "error: the call was refused\nthe log line one\nthe log line two",
            ),
        );
        chat.push_message(
            AgentId::ROOT,
            Message::user("#1 failed: no route to the endpoint\nthe run's own log"),
        );
        let pane = pane(AgentId::ROOT);

        let shown_rows = shown(&pane_rows(&chat, &pane, 60, 20));
        assert!(
            shown_rows
                .iter()
                .any(|row| row.contains("! error: the call was refused")),
            "{shown_rows:?}"
        );
        assert!(
            shown_rows
                .iter()
                .any(|row| row.contains("· #1 failed: no route to the endpoint")),
            "{shown_rows:?}"
        );
        assert!(
            shown_rows
                .iter()
                .any(|row| row.contains("the run's own log")),
            "shown whole at the fold's eight: {shown_rows:?}"
        );

        toggle_output(&mut chat);
        assert_eq!(
            shown(&pane_rows(&chat, &pane, 60, 20)),
            vec![
                "  ! error: the call was refused".to_string(),
                String::new(),
                "· #1 failed: no route to the endpoint".to_string(),
            ],
            "the failures stay and nothing else does"
        );
    }

    /// The two states are the folded number and none, and the *shown* one is
    /// exactly the fold's own table: eight wrapped rows and the `…` for a long
    /// result, before and after the toggle — the key does not touch a number,
    /// it only decides which state the pane paints.
    #[test]
    fn ctrl_o_keeps_the_folds_numbers_in_the_shown_state() {
        let many = (0..25)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut chat = Chat::bare();
        chat.push_message(AgentId::ROOT, Message::tool("call_1", &many));
        let pane = pane(AgentId::ROOT);

        let before = shown(&pane_rows(&chat, &pane, 60, 20));
        assert_eq!(before.len(), 9, "eight rows and the `…`: {before:?}");
        assert_eq!(before[0], "  line 0");
        assert_eq!(before[8], "  … +17 more lines");

        toggle_output(&mut chat);
        assert!(
            shown(&pane_rows(&chat, &pane, 60, 20)).is_empty(),
            "at none, a long result paints no row at all"
        );

        toggle_output(&mut chat);
        assert_eq!(
            shown(&pane_rows(&chat, &pane, 60, 20)),
            before,
            "the shown half still folds at eight, with the same `…`"
        );
    }

    /// The selector steps where the pane painted, and a block `Ctrl-O` hides
    /// painted no row of its own — so it has no stop: the cursor walks the rows
    /// around it, and the text is still in the transcript and comes back with
    /// the toggle. The failure row the fold keeps is one stop with no tail,
    /// because the `…` that would stand for the log behind it is not painted
    /// either.
    #[test]
    fn ctrl_o_leaves_no_stop_over_a_hidden_block() {
        let on = AgentId::ROOT;
        let mut chat = Chat::bare();
        say(&mut chat, on, "look at this");
        chat.push_message(
            on,
            Message::tool("call_1", "a diff, one line\nthe rest of the log"),
        );
        toggle_output(&mut chat);
        assert!(chat.start_select(on).is_none(), "the human's line stands");
        assert_eq!(
            chat.clamped_cursor(on),
            Some((0, Stop::Line(0))),
            "the cursor lands on a row the pane painted, not the hidden block"
        );
        assert!(
            chat.stops_at(on, 1, None).is_none(),
            "a block with no row has no stop"
        );
        chat.cancel_select();

        // A failed result is the one row that stays: a stop, and no tail.
        let mut chat = Chat::bare();
        chat.push_message(
            on,
            Message::tool("call_1", "error: the call was refused\nthe log line one"),
        );
        toggle_output(&mut chat);
        let stops = chat
            .stops_at(on, 0, None)
            .expect("the failure row is a stop");
        assert_eq!(
            stops.visible, 1,
            "the failure row is the block's first line"
        );
        assert!(!stops.tail, "no `…` paints, so there is no tail stop");
        assert_eq!(stops.span(Stop::Line(0)), (0, 0), "it covers its own line");
    }
}
