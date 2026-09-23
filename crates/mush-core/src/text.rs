//! Width and truncation arithmetic — one home for what a "column" means.
//!
//! Wrapping, truncation, row budgets and secret masks all count *display
//! columns*, so they live together and cannot drift (finding B9). Nothing here
//! touches a terminal: this is string arithmetic over `unicode-width`.
//!
//! The line arithmetic is here for the other side of the same question: what a
//! *file's* line is, as bytes, versus what a display makes of it
//! ([`file_lines`]). The model's roads read a file and must hand back its own
//! bytes; the pane's painter is the one road that rewrites them ([`sanitize`]).

use unicode_truncate::UnicodeTruncateStr;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Untrusted text made safe to paint: a model's reply, a tool result, a file
/// name a model chose, a line a server wrote.
///
/// A pane is a terminal, and a terminal acts on what it is given: a bare `\r`
/// returns the cursor to column 1 (a reply of `…and then\rREPLACED…` erased the
/// pane's own border), `ESC ]0;PWNED BEL` sets the window title, and a CSI `2J`
/// wipes the frame — the conversation is not allowed to repaint the screen it is
/// shown on. So:
///
/// - an **escape sequence is removed whole** — CSI, OSC, and every other
///   `ESC`-introduced form. Dropping the escape byte alone would leave the rest
///   of the sequence's bytes to be painted as text, which is a different lie.
/// - a **carriage return** becomes `␍`: it is a *visible* intent (the row was
///   meant to be overwritten) that no row can honour, and it is rare enough that
///   marking it is honest where dropping it would silently join two words. The
///   `\r` of a `\r\n` is a line ending, so it goes with nothing shown.
/// - every other **C0/C1 control** and `DEL` is dropped, along with the
///   characters that command the order a line is painted in: the bidi
///   embeddings and isolates (U+202A–202E, U+2066–2069) and the bidi *marks*
///   (U+200E LRM, U+200F RLM, U+061C ALM). A mark is the same command spelled
///   invisibly — inside `src/main.rs` it can make the painted name read as a
///   different path — and the whole family goes (finding B15).
/// - the zero-width characters that command no order **stay**, and are named
///   so the rule above cannot be read as wider than it is: ZWJ (U+200D) and
///   ZWNJ (U+200C) are orthography — an emoji sequence, a Persian word — and
///   ZWSP (U+200B), BOM (U+FEFF) and SHY (U+00AD) are invisible but reorder
///   nothing and take no column. Dropping them would be this function editing
///   the text it was asked to make safe.
/// - a **tab** is kept. It is layout, not a command, and [`wrap_text`] renders
///   it as four columns — the pane's tab stop, never the terminal's.
///
/// Nothing here touches a terminal: this is the same width-and-text arithmetic
/// as the rest of the module, and it is what the wrappers apply before a row is
/// built, so no caller can forget it.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => skip_escape(&mut chars),
            // The `\r` of a `\r\n` is a line ending; so is one at the very end
            // of the text, because the wrapper splits on `\n` before it ever
            // gets here.
            '\r' if chars.peek() == Some(&'\n') || chars.peek().is_none() => {}
            '\r' => out.push('␍'),
            '\t' | '\n' => out.push(ch),
            ch if invisible(ch) => {}
            ch => out.push(ch),
        }
    }
    out
}

/// A character that commands a display instead of appearing on it.
///
/// The bidi *marks* are here beside the embeddings and isolates: LRM, RLM and
/// ALM do not paint, but they pick the order a neutral run is laid out in, so a
/// line that holds one can be painted as a different line than it is — the
/// same command the embeddings spell, in one invisible character (finding
/// B15). The zero-width characters that command no order are not here, and
/// [`sanitize`]'s doc names them so this rule cannot be read as wider than it
/// is.
fn invisible(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

/// Whether `text` holds a `\n` that is not the second byte of a `\r\n`.
fn has_bare_lf(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes
        .iter()
        .enumerate()
        .any(|(index, &byte)| byte == b'\n' && (index == 0 || bytes[index - 1] != b'\r'))
}

/// Whether `text`'s lines all end with CRLF and there is at least one: a CRLF
/// file.
///
/// A *mixed* file (one bare LF anywhere) is not one: an edit into it can be
/// byte-exact, and calling it a CRLF file would refuse work the bytes allow.
/// The two roads that must not guess are the window, which says a CRLF file's
/// ending out loud, and the edit, which refuses an edit that would insert an
/// ending the file does not use (finding B7).
pub fn is_crlf(text: &str) -> bool {
    text.contains("\r\n") && !has_bare_lf(text)
}

/// The lines `text` holds as the file's own bytes: split at every `\n`, with
/// the `\r` of a `\r\n` kept at the end of the line it ends.
///
/// [`str::lines`] is a *reader's* split — it drops the `\r` of a CRLF ending —
/// and this is the splitter a *model's* road reads a file through: a searched
/// line must be the line the file holds, so its trailing spaces, its escape
/// sequences and the `\r` of its ending all come back (finding B8). What the
/// pane paints is the painter's own copy, [`sanitize`]d; what a read window
/// shows is a line's text and says so when an ending is missing (finding B7).
pub fn file_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive('\n')
        .map(|line| line.strip_suffix('\n').unwrap_or(line))
}

/// Consume one escape sequence, whole.
///
/// `ESC` introduces CSI (`ESC [ parameters intermediates final`), OSC (`ESC ] …
/// BEL` or `… ESC \`), and a family of short forms (`ESC 7`, `ESC (B`). A
/// sequence that never terminates — a truncated reply, a line cut by the
/// wrapper — takes the rest of the text with it: the bytes of a command are not
/// words, and painting half of one is how the sequence's own digits end up on
/// screen.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        // CSI: parameters and intermediates, then one byte in `@`–`~`.
        Some('[') => {
            for ch in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&ch) || ch.is_control() {
                    break;
                }
            }
        }
        // OSC: everything up to BEL, or to the string terminator `ESC \`.
        Some(']') => {
            for ch in chars.by_ref() {
                if ch == '\x07' {
                    break;
                }
                if ch == '\x1b' {
                    chars.next();
                    break;
                }
            }
        }
        // The short forms that take one more character: a character-set
        // designation (`ESC (`), a line attribute (`ESC #`), a charset (`ESC %`).
        Some('(' | ')' | '#' | '%') => {
            chars.next();
        }
        // `ESC` plus a single byte (`ESC 7`, `ESC =`), and a lone `ESC` at the
        // end of the text: nothing more to consume.
        _ => {}
    }
}

/// The first line of `text`, with every run of whitespace collapsed to one
/// space — a brief, a command or a summary read as one row.
///
/// The one home of "the brief's first line" (refactor R11): the commit subject,
/// an agent's title, a job's handle and a tool call's label each began with this
/// arithmetic, and the copies had already drifted — one kept a first line's
/// inner runs of spaces because it only trimmed the ends. What a caller does
/// with the line (cut it to columns, pick the words out of it, drop everything
/// before its last `&&`) is that caller's decision and stays there.
pub fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Word-aware wrapping that preserves explicit newlines and never splits a
/// grapheme's display width arithmetic.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    wrap_capped(text, width, None)
}

/// [`wrap_text`], but it stops after `max_lines` lines.
///
/// For a caller that discards the rest — a tool result shows its first eight
/// lines — wrapping the whole thing is work thrown away, and at one frame per
/// keystroke it was megabytes per second: 300 multi-kilobyte results wrapped in
/// full cost 55 ms a frame, 18 fps, to paint about forty lines.
pub fn wrap_text_capped(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    wrap_capped(text, width, Some(max_lines))
}

/// The least a description column may be and still be read: below this the
/// two-column shape stops paying for itself, and a description hangs under the
/// cell it describes instead of into a column of fragments.
///
/// At the `/help` popup's 40-column floor the command table's descriptions
/// wrapped to one character per row — and to nine columns at 80 — because the
/// usage column was never allowed to give a column back (finding D23). Sixteen
/// is about a word and a half of prose: a readability judgement, not an
/// arithmetic one, and one number because both help tables read it.
pub const MIN_DESCRIPTION_COLUMNS: usize = 16;

/// One row of a two-column help table, rendered for a surface `width` columns
/// wide: `left` (its column sized by the caller's `left_width`, the longest
/// left cell's) starts at the table's four-space indent and `description`
/// starts in the column `4 + left_width + 2` reserves it, with every wrapped
/// continuation hanging under the description rather than under the left cell.
///
/// When that description column would be narrower than
/// [`MIN_DESCRIPTION_COLUMNS`], the two-column shape is abandoned: the left
/// cell gets its own row and the description hangs *under* it, wrapped to the
/// same four-space indent and the rest of `width`. A column of one- and
/// two-word fragments costs more rows than it saves and reads as broken text,
/// and the row it describes is what the human came for (finding D23).
///
/// Both help surfaces render through this one function — `mush --help`'s keys
/// and commands blocks and the `/help` popup's — so the popup and the CLI
/// cannot disagree about the shape, and the continuation indent is spelled
/// once (the column arithmetic refactor R60 names).
pub fn columns(left: &str, left_width: usize, description: &str, width: usize) -> String {
    let description_column = 4 + left_width + 2;
    let mut out = String::new();
    if width.saturating_sub(description_column) < MIN_DESCRIPTION_COLUMNS {
        out.push_str(&format!("    {left}\n"));
        for line in wrap_text(description, width.saturating_sub(4).max(1)) {
            out.push_str(&format!("    {line}\n"));
        }
        return out;
    }
    let lead = format!("    {left:<left_width$}  ");
    let mut wrapped = wrap_text(description, width - description_column).into_iter();
    if let Some(first) = wrapped.next() {
        out.push_str(&lead);
        out.push_str(&first);
        out.push('\n');
    }
    for continuation in wrapped {
        out.push_str(&format!("{:description_column$}{continuation}\n", ""));
    }
    out
}

fn wrap_capped(text: &str, width: usize, max_lines: Option<usize>) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        if max_lines.is_some_and(|max| out.len() >= max) {
            break;
        }
        // Per line, before it is measured: one pass over the lines this caller
        // will actually paint, and a wide glyph or an escape sequence cannot
        // make the wrapping and the terminal disagree about where the row ends.
        let raw = sanitize(raw);
        let mut current = String::new();
        let mut current_width = 0usize;
        let mut last_space: Option<usize> = None;

        for ch in raw.chars() {
            let (rendered, char_width) = if ch == '\t' {
                ("    ".to_string(), 4)
            } else {
                (
                    ch.to_string(),
                    UnicodeWidthChar::width(ch).unwrap_or(1).max(1),
                )
            };

            // A row ends at its last space, and the character that did not fit
            // goes after the tail the space left behind. That tail can itself
            // be too full for the character — a row that ended exactly at the
            // width, then a CJK glyph or a tab (four columns at once) — and the
            // old single break appended it anyway: `wrap_text(" bcd日", 4)`
            // painted `bcd日`, five columns, and the terminal cut the glyph the
            // pane had no column for. So the break is a loop: while the tail
            // does not fit either, the tail is a row of its own.
            //
            // A break trims the **spaces it broke at**, and only those: `rest`
            // begins at the space the row ended on, so every other character of
            // the tail is the text's own. A no-break space, an ideographic space
            // or a line separator is not a space to break at, so it is not one
            // to delete either — `trim_start` ate the whole whitespace class
            // and dropped characters the plain wrapper kept, which put the
            // view's rows and the wrapper's rows on two different rules
            // (finding B14). The tail rule is one rule now, spelled here and in
            // `wrap_runs`: no character is dropped, and the two wrappers cannot
            // disagree about where a row ends or what it holds.
            loop {
                if current_width + char_width <= width || current.is_empty() {
                    break;
                }
                if let Some(space) = last_space {
                    let rest = current.split_off(space);
                    out.push(std::mem::take(&mut current));
                    current = rest.trim_start_matches(' ').to_string();
                } else {
                    out.push(std::mem::take(&mut current));
                }
                // Stop mid-line too, so one enormous wrapped line cannot cost
                // more than the lines the caller will keep.
                if max_lines.is_some_and(|max| out.len() >= max) {
                    return out;
                }
                current_width = UnicodeWidthStr::width(current.as_str());
                last_space = None;
            }

            current.push_str(&rendered);
            current_width += char_width;
            if ch == ' ' {
                last_space = Some(current.len() - 1);
            }
        }
        out.push(current);
    }
    out
}

/// One styled run of one rendered markdown row.
///
/// Runs are plain data: a piece of the source's text and one name from
/// [`RunStyle`]'s vocabulary. No colour and no terminal lives here — the app
/// maps the vocabulary onto its own palette in one place, so the parser never
/// learns what an accent is and a new surface never learns the parser's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// The characters, exactly as the source wrote them. A run never rewrites
    /// text; it only says how the text was marked.
    pub text: String,
    pub style: RunStyle,
}

/// What a run is, in the markdown view's whole vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStyle {
    /// Untouched text.
    Plain,
    /// `**strong**`.
    Strong,
    /// `*emphasis*` or `_emphasis_`.
    Emphasis,
    /// `~~strike~~`.
    Strike,
    /// `` `code` `` — an inline span.
    Code,
    /// The body of a fenced block.
    Fence,
    /// `#`/`##`/`###`, with the level.
    Heading(u8),
    /// A list's own marker (`-`, `*`, `+`, `1.`) and the one space after it,
    /// kept rather than hidden: the marker is where the item sits, and the
    /// space is the column every wrapped continuation of the item hangs under.
    Bullet,
    /// A horizontal rule's row: `─` across the pane, painted where the source
    /// line's `---` was. The one run whose text the view writes rather than
    /// keeps — a rule is a line across the *pane*, and the pane's own width is
    /// the only width it can be drawn at.
    Rule,
    /// The `│ ` a block quote's `>` became. The quoted words keep their own
    /// runs; this is the bar beside them.
    Quote,
    /// The text of `[text](url)`.
    Link,
    /// The ` (url)` beside it. A URL is never dropped: this is a coding tool,
    /// and the human may have to copy it out of the pane.
    Url,
}

/// The model's prose read as a view: markdown parsed into styled runs, wrapped
/// to a width. The pane paints the rows; nothing here writes anything back.
///
/// A model writes markdown — headings, bullets, `**emphasis**`, code fences —
/// and the pane painted the markers as if they were the sentence. This is the
/// answer, and it is deliberately the small one. The parse is **line-local**:
/// every source line is read on its own, so nothing here can reflow a
/// paragraph, join two lines, re-indent a list or turn `- a\n- b` into a layout
/// the source did not have. That boundary is the point — the human called the
/// full version a rabbit hole, and a chat reply needs a reading, not a document
/// renderer. Tables, setext headings, reference links, HTML, nested lists and
/// indented code blocks are all *not* rules; a line that uses one is simply the
/// text it is.
///
/// It is **additive** too. The only text a rule removes is scaffolding a human
/// does not read in a view — the `#`s of a heading, the `>` a quote's bar
/// replaced, and the two fence lines of a code block. Every word is kept; a list keeps its marker and only styles it,
/// because the marker is information; and a link always shows its URL beside
/// its text, because a dropped URL is data loss. Above all this is a *view*:
/// what the human copies out of the pane is still the model's own bytes,
/// because nothing here rewrites the transcript — it only decides how a frame
/// paints it.
///
/// # What is a rule
///
/// Inline, within one line:
///
/// - `**strong**` → [`RunStyle::Strong`]. Two or more asterisks are the strong
///   form and the whole run is spent: `***bold***` is a strong `bold`, not a
///   pair of markers and a stray.
/// - `*emphasis*` and `_emphasis_` → [`RunStyle::Emphasis`]. An `_` inside or
///   beside a word is not one, so `snake_case_name` survives; `__strong__` has
///   no rule here and stays text rather than being half-read. A lone `*` or `_`
///   pairs only with another lone one, so `*a**` is text: the emphasis rule
///   cannot spend the closer's two.
/// - `` `code` `` → [`RunStyle::Code`]. A run of backticks is one span, opened
///   and closed whole, so `` ``code`` `` is a code `code`.
/// - `~~strike~~` → [`RunStyle::Strike`]. Worth one rule: a model uses it to
///   mark its own correction, and on a terminal that cannot strike it out the
///   words still read. A run of two or more is spent whole, as strong is.
/// - `[text](url)` → the text in [`RunStyle::Link`] and ` (url)` in
///   [`RunStyle::Url`]. The URL's own parentheses are counted, so a wiki link's
///   tail is not cut off.
///
/// A **run** of a marker is all-or-nothing: a rule spends every marker in the
/// run it opens or closes with, and a run no rule can spend as a pair is text
/// exactly as it was typed. So no row ever paints a marker left over from a run
/// half-read.
///
/// Block, at the start of a line:
///
/// - three or more `-`, `*` or `_` and nothing else on the line, spaces
///   between them allowed → a row of `─` across the pane in
///   [`RunStyle::Rule`]. The line's own characters are scaffolding, like a
///   heading's `#`s: what the line says is "a break", and the characters it
///   says it with are not words. `---` directly under a paragraph is the case
///   worth writing down: CommonMark reads it as a *setext heading*, and this
///   view deliberately reads it as a rule — a setext heading is still not a
///   rule here — so `Title` above a `---` is a paragraph and the `---` is a
///   rule across the pane. The row is exactly `width` columns, so a rule can
///   never paint past the narrowest pane.
/// - one, two or three `#`s and a space → [`RunStyle::Heading`]. The `#`s and
///   that one space are not painted: the heading's style says what they said.
///   Four or more `#`s, or a `#` with no space after it, are text.
/// - a line whose first non-space text is `>` → the `>` and the one space that
///   may follow it become `│ ` in [`RunStyle::Quote`], and the quoted words
///   follow it as they were typed. The bar is not the `>`: in a coding tool a
///   `>` at the head of a line reads as a shell redirect, and a quote is not a
///   command — the same reason the words are kept, since the quote is
///   something a human wrote and this is a view of it, not an edit. `>text`
///   with no space is the same quote as `> text`: the space is the marker's
///   separator, not a word. The source's own indentation is kept, because an
///   indented `> ` is a quote inside a list item and moving it to column 0
///   would move it out of the item it belongs to. A second `>` is text
///   exactly as it is — `> > x` is `│ > x` — because this view reads one
///   marker per line, and a bar for the inner one would be a nesting there is
///   no layout for.
/// - `- `, `* `, `+ `, or `1. `–`99. ` → the marker keeps its place and is
///   styled [`RunStyle::Bullet`]. A marker with no space after it is not one,
///   and an ordered marker is at most two digits, because `1998. It was a good
///   year` opens a sentence, not a list.
/// - a `- `, `* ` or `+ ` item whose own text begins with `[ ]`, `[x]` or
///   `[X]` and a space → the checkbox paints as `☐` or `☑` in
///   [`RunStyle::Bullet`], the marker's own style. Two brackets and the space
///   between them say what one box says, so the box says "checkbox" in one
///   column where the source spelled it in three; a box is a fact about the
///   item, and it is painted rather than dropped for the same reason the
///   marker is. Everything that is not a checkbox is text exactly as typed:
///   `[]` has no state, `[y]` is not a state this view knows, `[ ]` with no
///   words after it would be a box nobody wrote, and a `[ ]` with no list
///   marker is a line of text. `1. [ ] task` is not one either: this view's
///   boxes belong to the unordered markers, because the ordered ones are
///   where an item sits and the box is what it is.
/// - a line whose first non-space text is three backticks opens a fenced block,
///   and the next such line closes it. The fence lines of a block with a body
///   are not painted and everything between them is [`RunStyle::Fence`], one
///   style with no inline parsing, so `**` in code stays code. A block with no
///   words in it is the exception — there is nothing for the fence to hide, so
///   its fence lines are painted as text, and a reply of nothing but a fence
///   is a turn the human can see (finding D14). A fence that never closes runs
///   to the end of the message: an unterminated block is still a block, and the
///   code in it is still code.
///
/// An mark that never closes is **text**: `**bold` is `**bold`, a lone `*` is a
/// lone `*`, and a `[link](` with no `)` is the characters it is. Nothing is
/// guessed, and nothing is dropped on the way.
///
/// # The wrap
///
/// [`wrap_text`]'s rules over runs instead of characters: each source line
/// wraps on its own, the source's own newlines are honoured, a tab is four
/// columns, every line is [`sanitize`]d, and a word is broken only when it
/// cannot fit a row by itself. A rendered row therefore does not outgrow
/// `width` — one glyph (or one tab, four columns at once) wider than the whole
/// width is the only thing a row cannot honour, and a pane's body is never that
/// narrow.
///
/// The one thing a block marker changes is where a wrapped row *starts*: a line
/// a rule opened with a marker — an item's `- ` or `1. `, a task item's `- ☐ `,
/// a quote's `│ ` — wraps the line's own *text* in the columns the marker
/// leaves, and every row after the first leads with the marker's own width of
/// blank. So the continuation of
///
/// ```text
/// - a bullet whose text is long enough to wrap
/// over here
/// ```
///
/// is
///
/// ```text
/// - a bullet whose text is long enough to
///   wrap over here
/// ```
///
/// and not a row that begins in column 0 under nothing. The margin is the
/// marker's own width, so `1. ` hangs three columns and `10. ` four, each
/// under the text the marker introduced.
///
/// The wrap is still the plain wrapper's wrap, exactly — the same break
/// points, the same tab stop, and the same tail on every break (finding B14) —
/// with two amendments, both of which are the same rule read where the marker
/// is: the text wrapped is the text the rules above left *after* the marker,
/// at the columns the marker leaves, and the marker is a margin on every
/// continuation row. The margin is a column the text was given, not a second
/// wrapping rule: the break points, the tab stop and the tail are
/// [`wrap_text`]'s, one margin over. A marker that leaves no column for the
/// text — as wide as the pane, or wider — and a glyph wider than the columns a
/// margin left are not margins: both fall back to the plain wrapper's wrap of
/// the marker and the text together, so every character is still painted and
/// no row outgrows the width.
pub fn markdown_rows(text: &str, width: usize) -> Vec<Vec<Run>> {
    markdown_walk(text, width).0
}

/// How many rows each source line of `text` paints under [`markdown_rows`], in
/// source order — one entry per `text.split('\n')` line, the lines a rule
/// paints nothing for included as zeroes.
///
/// The map a caller that tags painted rows with their source line needs: the
/// pane's stop map cannot count a fence line's rows off the source, because a
/// fence line paints a row only when its block has no body, and only the walk
/// that paints the rows knows that. Asking the same walk for both answers is
/// what keeps the map from drifting off the screen — a second count with a
/// restated rule is the drift this exists to prevent (finding D14).
///
/// One entry per line, always: a line whose rule paints nothing is a `0`, not
/// a missing entry, so a caller can index by source line.
pub fn markdown_row_counts(text: &str, width: usize) -> Vec<usize> {
    markdown_walk(text, width).1
}

/// [`markdown_rows`]' walk, with the per-source-line row count beside it: the
/// two answers are one pass, because the count *is* the walk's per-line result.
fn markdown_walk(text: &str, width: usize) -> (Vec<Vec<Run>>, Vec<usize>) {
    let width = width.max(1);
    let lines: Vec<String> = text.split('\n').map(sanitize).collect();
    let mut out = Vec::new();
    let mut counts = Vec::with_capacity(lines.len());
    let mut at = 0;
    while at < lines.len() {
        if !fence_line(&lines[at]) {
            let rows = wrap_block(&block(&lines[at], width), width);
            counts.push(rows.len());
            out.extend(rows);
            at += 1;
            continue;
        }
        // The fence's block: the lines up to the next fence line, or the
        // message's end — an unterminated block is still a block, and the code
        // in it is still code.
        let close = lines[at + 1..]
            .iter()
            .position(|line| fence_line(line))
            .map(|skip| at + 1 + skip);
        let end = close.unwrap_or(lines.len());
        // Does anything between the fences paint a word? A block that says
        // nothing is not scaffolding to hide: the fence lines are the whole of
        // what the model wrote, so they are painted as text. A reply of
        // nothing but a fence was a turn with no row at all — a `mush › ` mark
        // over nothing, and a select mode advertising `Enter copies` with no
        // cursor anywhere on screen (finding D14).
        let words = lines[at + 1..end]
            .iter()
            .any(|line| !line.trim().is_empty());
        for (index, line) in lines[at..end].iter().enumerate() {
            // The opening fence of a block with words is scaffolding and paints
            // nothing; every other line is the block's own text — the code
            // inside it, one style with no inline parsing.
            let style = match (words, index) {
                (true, 0) => {
                    counts.push(0);
                    continue;
                }
                (true, _) => RunStyle::Fence,
                (false, _) => RunStyle::Plain,
            };
            let rows = wrap_runs(
                &[Run {
                    text: line.clone(),
                    style,
                }],
                width,
            );
            counts.push(rows.len());
            out.extend(rows);
        }
        // The closing fence: scaffolding like the opening one when the block
        // had words, and one more line of what the model wrote when it did not.
        if let Some(close) = close {
            if words {
                counts.push(0);
            } else {
                let rows = wrap_runs(
                    &[Run {
                        text: lines[close].clone(),
                        style: RunStyle::Plain,
                    }],
                    width,
                );
                counts.push(rows.len());
                out.extend(rows);
            }
            at = close + 1;
        } else {
            at = end;
        }
    }
    debug_assert_eq!(counts.len(), lines.len(), "one count per source line");
    (out, counts)
}

/// Whether a line is a fence, opening or closing one. The run of backticks is
/// not counted: three or more at the start of the line toggle the block, and an
/// info string after them is part of the fence, not of the code.
fn fence_line(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// One source line, parsed before it is wrapped: a rule, a heading, a list
/// item, or whatever the inline rules make of it.
fn block(line: &str, width: usize) -> Block {
    if rule_line(line) {
        // The one run the view writes itself. A rule is a line across the
        // pane, not the characters a model typed to ask for one: `-`, `*` and
        // `_` are how markdown spells it, and painting them would show the
        // spelling instead of the break. The pane's width is the only width
        // the row can honour, so it is the width the row is built at — and it
        // is then wrapped like any other run, which is one row of exactly
        // `width` columns because `─` is one column.
        return Block {
            marker: Vec::new(),
            content: vec![Run {
                text: "─".repeat(width),
                style: RunStyle::Rule,
            }],
        };
    }
    if let Some((indent, text)) = quote(line) {
        let bar = if text.is_empty() { "│" } else { "│ " };
        let mut marker = Vec::with_capacity(2);
        if !indent.is_empty() {
            marker.push(Run {
                text: indent.to_string(),
                style: RunStyle::Plain,
            });
        }
        marker.push(Run {
            text: bar.to_string(),
            style: RunStyle::Quote,
        });
        return Block {
            marker,
            content: inline(text),
        };
    }
    if let Some((level, text)) = heading(line) {
        // The heading's style is the whole heading: a marker inside it is read
        // (so `## **Title**` does not paint its asterisks) but the runs all
        // come out as the heading, because that is the only style it wears.
        let mut runs = inline(text);
        for run in &mut runs {
            run.style = RunStyle::Heading(level);
        }
        return Block {
            marker: Vec::new(),
            content: runs,
        };
    }
    if let Some((marker, text)) = list_marker(line) {
        // The marker and the one space after it lead the first row; the space
        // is part of the marker because it is what a wrapped row hangs under.
        let mut leading = vec![Run {
            text: format!("{marker} "),
            style: RunStyle::Bullet,
        }];
        let rest = &text[1..];
        if let Some((box_char, rest)) = checkbox(marker, text) {
            // The brackets and the space between them are scaffolding: the box
            // is the same fact in one column, and the box's own space is what
            // the item's words hang under.
            leading.push(Run {
                text: format!("{box_char} "),
                style: RunStyle::Bullet,
            });
            return Block {
                marker: leading,
                content: inline(rest),
            };
        }
        return Block {
            marker: leading,
            content: inline(rest),
        };
    }
    Block {
        marker: Vec::new(),
        content: inline(line),
    }
}

/// Whether a line is a horizontal rule: three or more `-`, `*` or `_` and
/// nothing else, spaces between them allowed.
///
/// The same character all the way across, which is CommonMark's rule and the
/// only reading that cannot be confused with text: `- * -` is prose about
/// bullets, not a break. Two markers are not a rule — `--` is a longer
/// hyphen, `**` an unclosed strong — and neither is a marker run with anything
/// beside it. A rule line is checked before every other block rule because it
/// is the one that would otherwise be read as something else: `* * *` is a
/// bullet whose item is `* *` by the list rule, and CommonMark reads it as a
/// break.
fn rule_line(line: &str) -> bool {
    let rest = line.trim();
    let mut chars = rest.chars();
    let Some(marker) = chars.next() else {
        return false;
    };
    if !matches!(marker, '-' | '*' | '_') {
        return false;
    }
    chars.all(|ch| ch == marker || ch == ' ')
        && rest.chars().filter(|ch| *ch == marker).count() >= 3
}

/// `> quoted`, `>quoted`: the source's own indentation and the text after the
/// marker — the marker's one optional space is not part of the text, since a
/// quote's words start at the first character that is not the marker.
fn quote(line: &str) -> Option<(&str, &str)> {
    let indent = line.len() - line.trim_start().len();
    let body = line[indent..].strip_prefix('>')?;
    Some((&line[..indent], body.strip_prefix(' ').unwrap_or(body)))
}

/// `# Title`, `## Title`, `### Title`: the level and the text after one space.
fn heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.chars().take_while(|ch| *ch == '#').count();
    if !(1..=3).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..];
    if rest.is_empty() {
        return Some((hashes as u8, rest));
    }
    rest.strip_prefix(' ').map(|text| (hashes as u8, text))
}

/// `- item`, `* item`, `+ item`, `1. item`: the marker and the text after it.
fn list_marker(line: &str) -> Option<(&str, &str)> {
    let first = line.chars().next()?;
    if matches!(first, '-' | '*' | '+') && line[1..].starts_with(' ') {
        return Some((&line[..1], &line[1..]));
    }
    if first.is_ascii_digit() {
        let digits = line.chars().take_while(char::is_ascii_digit).count();
        if digits <= 2 && line[digits..].starts_with(". ") {
            return Some((&line[..digits + 1], &line[digits + 1..]));
        }
    }
    None
}

/// The box a task-list item's `[ ]`/`[x]`/`[X]` names, and the item's own
/// words after it — `None` for everything that is not a checkbox.
///
/// `marker` is the marker the item opened with: a box belongs to the unordered
/// markers, because an ordered item already says where it sits and the box is
/// what it is.
///
/// `text` is what [`list_marker`] left: the one space that separates the marker
/// from the item's text, then the text. So the form read here is ` [ ] words`:
/// `[`, one state character, `]`, one space, and at least one word after it. A
/// box with nothing after it is not one — there is nothing for it to be about —
/// and the state must be one a box can say, so `[]`, `[y]` and ` [ ]` are the
/// characters they are.
fn checkbox<'a>(marker: &str, text: &'a str) -> Option<(char, &'a str)> {
    if !matches!(marker, "-" | "*" | "+") {
        return None;
    }
    let rest = text.strip_prefix(" [")?;
    let state = rest.chars().next()?;
    let rest = &rest[state.len_utf8()..];
    let box_char = match state {
        ' ' => '☐',
        'x' | 'X' => '☑',
        _ => return None,
    };
    let rest = rest.strip_prefix("] ")?;
    if rest.is_empty() {
        return None;
    }
    Some((box_char, rest))
}

/// One source line's inline markers: plain text, spans, and links, in order.
///
/// No nesting and no escapes: inside a span the text is the text, so
/// `**a *b**` is strong text that happens to hold a star. That is the small
/// version on purpose — a recursive parser is where a chat reply stops being a
/// reading.
fn inline(line: &str) -> Vec<Run> {
    let chars: Vec<char> = line.chars().collect();
    let mut runs: Vec<Run> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        match span(&chars, i) {
            Some((next, span)) => {
                if !plain.is_empty() {
                    runs.push(Run {
                        text: std::mem::take(&mut plain),
                        style: RunStyle::Plain,
                    });
                }
                runs.extend(span);
                i = next;
            }
            None => {
                plain.push(chars[i]);
                i += 1;
            }
        }
    }
    if !plain.is_empty() {
        runs.push(Run {
            text: plain,
            style: RunStyle::Plain,
        });
    }
    runs
}

/// The marker run at `chars[i]`, if there is one: the runs it paints and the
/// index the scanner goes on at. `None` means the character is text, which is
/// what an unterminated marker, and a run no rule can spend whole, get.
///
/// A **run** of a marker is the unit: the scanner only ever asks a rule at the
/// first character of one, a rule spends every marker in the run it names, and
/// a run that is not a matched pair is text in full — so no row can carry a
/// marker left over from a run half-read as an opener or a closer.
/// `***bold***` is a strong `bold`; `*a**` is the four characters it is,
/// because the one-`*` rule cannot spend the closer's two.
fn span(chars: &[char], i: usize) -> Option<(usize, Vec<Run>)> {
    let marker = chars[i];
    // Only the first character of a run is ever a marker: the rest are the
    // text of the run the first one was read as, so a second `*` of a `***`
    // is not an opener of its own.
    if matches!(marker, '`' | '*' | '~' | '_') && i > 0 && chars[i - 1] == marker {
        return None;
    }
    match marker {
        '`' => {
            let open = run_len(chars, i, '`');
            close(chars, i + open, '`', Closer::Any).map(|(end, len)| {
                (
                    end + len,
                    vec![Run {
                        text: chars[i + open..end].iter().collect(),
                        style: RunStyle::Code,
                    }],
                )
            })
        }
        '*' => {
            let open = run_len(chars, i, '*');
            // One asterisk is emphasis, two or more are strong: the closer
            // must be a run of the same form, and that run is spent whole.
            let (style, closer) = if open == 1 {
                (RunStyle::Emphasis, Closer::Single)
            } else {
                (RunStyle::Strong, Closer::Run)
            };
            close(chars, i + open, '*', closer).map(|(end, len)| {
                (
                    end + len,
                    vec![Run {
                        text: chars[i + open..end].iter().collect(),
                        style,
                    }],
                )
            })
        }
        '~' => {
            let open = run_len(chars, i, '~');
            if open < 2 {
                return None;
            }
            close(chars, i + open, '~', Closer::Run).map(|(end, len)| {
                (
                    end + len,
                    vec![Run {
                        text: chars[i + open..end].iter().collect(),
                        style: RunStyle::Strike,
                    }],
                )
            })
        }
        '_' if underscore_opens(chars, i) => {
            close(chars, i + 1, '_', Closer::Single).and_then(|(end, len)| {
                // The closing `_` must end a word too, or an `_` inside an
                // identifier could close a span it never opened.
                if matches!(chars.get(end + len), Some(ch) if *ch == '_' || ch.is_alphanumeric()) {
                    return None;
                }
                Some((
                    end + len,
                    vec![Run {
                        text: chars[i + 1..end].iter().collect(),
                        style: RunStyle::Emphasis,
                    }],
                ))
            })
        }
        '[' => link(chars, i).map(|(end, text, url)| {
            (
                end + 1,
                vec![
                    Run {
                        text,
                        style: RunStyle::Link,
                    },
                    Run {
                        text: format!(" ({url})"),
                        style: RunStyle::Url,
                    },
                ],
            )
        }),
        _ => None,
    }
}

/// Whether the `_` at `i` opens emphasis: at a word boundary, and alone. That
/// is the whole defence of `snake_case_name` and of `__strong__`, which is not
/// a rule here.
fn underscore_opens(chars: &[char], i: usize) -> bool {
    let before = if i == 0 { None } else { Some(chars[i - 1]) };
    match before {
        // Beside a word or beside another `_` it is the text's own character.
        Some(ch) if ch == '_' || ch.is_alphanumeric() => false,
        _ => chars.get(i + 1) != Some(&'_'),
    }
}

/// The length of the maximal run of `marker` at `chars[i]` — zero when
/// `chars[i]` is some other character. A run is the unit an inline marker is
/// read in ([`span`]).
fn run_len(chars: &[char], i: usize, marker: char) -> usize {
    chars[i..].iter().take_while(|ch| **ch == marker).count()
}

/// Which runs of a marker may close a span, by the rule that opened it: the
/// one-marker rules pair with one marker, a run of two or more pairs with a run
/// of two or more, and a code span's backticks with any run of backticks at
/// all. A run of another form is skipped whole by [`close`] — it is the text
/// inside the span, as the lone `*` of `**a *b**` is — never half-read.
#[derive(Clone, Copy)]
enum Closer {
    /// Exactly one marker: the `**` of `*a**` cannot close the one-`*` rule,
    /// so that line is the characters it is, not an emphasis and a stray.
    Single,
    /// Two or more, every one of them: `**a***` is a strong `a` with the
    /// opening run and the closing run both spent.
    Run,
    /// Any run at all: `` ``code`` `` is one code span, not a code span holding
    /// a backtick and a stray.
    Any,
}

impl Closer {
    /// Whether a run of this length closes the span.
    fn fits(self, len: usize) -> bool {
        match self {
            Closer::Single => len == 1,
            Closer::Run => len >= 2,
            Closer::Any => true,
        }
    }
}

/// The next run of `marker` after `from` that closes a span, as its index and
/// its length — or `None` if there is none. The run must fit the opener's form
/// ([`Closer`]); a run that does not is skipped whole, never half-read. The
/// content must be non-empty and must not begin or end in whitespace, which is
/// what keeps `a * b * c` and `** **` as the text they are rather than a span
/// of spaces.
fn close(chars: &[char], from: usize, marker: char, closer: Closer) -> Option<(usize, usize)> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == marker {
            let len = run_len(chars, i, marker);
            if closer.fits(len)
                && i > from
                && !chars[from].is_whitespace()
                && !chars[i - 1].is_whitespace()
            {
                return Some((i, len));
            }
            i += len;
        } else {
            i += 1;
        }
    }
    None
}

/// `[text](url)`, or `None` for anything that is not one: the index of the
/// closing `)`, the text and the URL. The URL's own parentheses are counted, so
/// `[a](https://en.wikipedia.org/wiki/Foo_(bar))` keeps its tail, and a URL is
/// never empty — a link with nothing to show is just its text.
fn link(chars: &[char], open: usize) -> Option<(usize, String, String)> {
    let end_text = open + 1 + chars[open + 1..].iter().position(|ch| *ch == ']')?;
    let text: String = chars[open + 1..end_text].iter().collect();
    if text.trim().is_empty() || chars.get(end_text + 1) != Some(&'(') {
        return None;
    }
    let mut depth = 1usize;
    let mut i = end_text + 2;
    while i < chars.len() {
        match chars[i] {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let url: String = chars[end_text + 2..i].iter().collect();
                    if url.trim().is_empty() {
                        return None;
                    }
                    return Some((i, text, url));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// One source line, parsed before it is wrapped: the runs that lead the line
/// and the runs the line is about.
///
/// The split exists for one reason: a wrapped row must not start in column 0
/// when the line above it started under a marker. `marker` is what the first
/// row leads with — a list's `- `, a task item's `- ☐ `, a quote's `│ `, a
/// chosen block's indentation — and its own width is the margin every row
/// after the first hangs under ([`wrap_block`]). A line without a marker has
/// an empty `marker` and wraps exactly as it always did.
struct Block {
    marker: Vec<Run>,
    content: Vec<Run>,
}

/// [`wrap_runs`] over one parsed line: the content wraps in the columns the
/// marker leaves, the marker leads the first row, and every row after it leads
/// with the marker's own width of blank — so a wrapped item's continuation
/// sits under the item's text and not under the marker.
///
/// A margin is only a margin when it leaves a column for the words. A marker
/// as wide as the pane (or wider) would leave none, and a single glyph wider
/// than the columns the margin left — a two-column glyph in a
/// one-column budget — cannot be honoured by any break. Both cases fall back
/// to the plain wrapper's wrap of the marker and the content together: nothing
/// is dropped, the marker is still painted, and every row stays inside the
/// width.
fn wrap_block(block: &Block, width: usize) -> Vec<Vec<Run>> {
    let width = width.max(1);
    let margin = runs_width(&block.marker);
    if margin > 0 && margin < width {
        let budget = width - margin;
        let rows = wrap_runs(&block.content, budget);
        if rows.iter().all(|row| runs_width(row) <= budget) {
            return rows
                .into_iter()
                .enumerate()
                .map(|(index, row)| {
                    let mut out = if index == 0 {
                        block.marker.clone()
                    } else {
                        vec![Run {
                            text: " ".repeat(margin),
                            style: RunStyle::Plain,
                        }]
                    };
                    out.extend(row);
                    out
                })
                .collect();
        }
    }
    let mut runs = block.marker.clone();
    runs.extend(block.content.iter().cloned());
    wrap_runs(&runs, width)
}

/// The columns a run sequence takes, by [`wrap_runs`]' own arithmetic: a tab
/// is four columns, a glyph is its own width, and a character the width is
/// unknown for is one column. One spelling of the arithmetic beside the
/// wrapper's, because the marker's margin and a table's padding both measure
/// the runs the wrapper will paint.
fn runs_width(runs: &[Run]) -> usize {
    runs.iter()
        .flat_map(|run| run.text.chars())
        .map(|ch| {
            if ch == '\t' {
                4
            } else {
                UnicodeWidthChar::width(ch).unwrap_or(1).max(1)
            }
        })
        .sum()
}

/// [`wrap_text`] over styled runs: the same rows, each row split into runs of
/// one style. The arithmetic is `wrap_capped`'s, tab expansion included, and a
/// test pins the two against each other — one rule, two spellings, and no drift
/// between the view and the text beside it.
fn wrap_runs(runs: &[Run], width: usize) -> Vec<Vec<Run>> {
    let width = width.max(1);
    let chars: Vec<(char, RunStyle)> = runs
        .iter()
        .flat_map(|run| run.text.chars().map(|ch| (ch, run.style)))
        .collect();

    let mut out: Vec<Vec<(char, RunStyle)>> = Vec::new();
    let mut current: Vec<(char, RunStyle)> = Vec::new();
    let mut current_width = 0usize;
    let mut last_space: Option<usize> = None;

    for (ch, style) in chars {
        let (char_width, tab) = if ch == '\t' {
            (4, true)
        } else {
            (UnicodeWidthChar::width(ch).unwrap_or(1).max(1), false)
        };
        // The break is a loop, exactly as in `wrap_capped`: the tail a space
        // break leaves can itself be too full for this character, and a row
        // must not outgrow the width it was given.
        loop {
            if current_width + char_width <= width || current.is_empty() {
                break;
            }
            if let Some(space) = last_space {
                let rest = current.split_off(space);
                out.push(std::mem::take(&mut current));
                current = rest;
                while matches!(current.first(), Some((' ', _))) {
                    current.remove(0);
                }
            } else {
                out.push(std::mem::take(&mut current));
            }
            current_width = current
                .iter()
                .map(|(ch, _)| UnicodeWidthChar::width(*ch).unwrap_or(1).max(1))
                .sum();
            last_space = None;
        }
        if tab {
            // Four columns of layout, `wrap_text`'s tab stop, in the style of
            // the character that asked for them. A tab is not a break point:
            // the spaces it becomes never set `last_space`.
            current.extend([(' ', style); 4]);
        } else {
            current.push((ch, style));
        }
        current_width += char_width;
        if ch == ' ' {
            last_space = Some(current.len() - 1);
        }
    }
    out.push(current);

    out.into_iter().map(runs_of).collect()
}

/// One row's characters, gathered back into runs of one style.
fn runs_of(chars: Vec<(char, RunStyle)>) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for (ch, style) in chars {
        match runs.last_mut() {
            Some(run) if run.style == style => run.text.push(ch),
            _ => runs.push(Run {
                text: ch.to_string(),
                style,
            }),
        }
    }
    runs
}

/// Shorten to at most `max` display columns *including* the ellipsis, so a
/// caller budgeting columns gets text that really fits (finding B9: counting
/// characters made a CJK row twice as wide as its budget).
///
/// The text is [`sanitize`]d first. This is what a one-line painter calls to
/// fit a fact into its columns, and the facts it is handed are the same
/// untrusted ones the transcript wraps — a model's summary, an endpoint's error
/// body, a job's command. The wrapper sanitized and this did not, so the rule
/// reached the transcript and missed every one-line surface beside it: a reply
/// of `…and then\rREPLACED…` returned the cursor over the pane's own border, and
/// an error body's `ESC ]0;PWNED BEL` renamed the window. Sanitizing here closes
/// all of those at once, for callers nobody has written yet, and it cannot turn
/// into work per frame: the text is about to be walked to `max` columns anyway.
pub fn truncate(text: &str, max: usize) -> String {
    truncate_flag(text, max).0
}

/// [`truncate`], and whether it dropped anything: `(text, cut_short)`.
///
/// A caller cannot read whether the text came back whole off the string, so the
/// arithmetic says so.
pub fn truncate_flag(text: &str, max: usize) -> (String, bool) {
    cut(&sanitize_upto(text, max), max)
}

/// [`sanitize`], over no more than what `max` columns can be made from.
///
/// A one-line painter is handed `max` columns and whatever text the actor had:
/// an endpoint's whole error body (up to `http::MAX_BODY_BYTES`), a model's
/// 20000-character summary on a row the frame repaints sixty times a second.
/// Every painted column comes from at least one character, so a prefix of
/// `4 * max + 64` characters holds every column that can be shown, with room
/// for the escape sequences that are removed whole; and because a sequence cut
/// at the bound is dropped whole by [`skip_escape`], what comes back is always a
/// *prefix* of the safe line — never more of it than there is. The cost of a
/// row is therefore the row's width, not the length of the text behind it.
fn sanitize_upto(text: &str, max: usize) -> String {
    let limit = max.saturating_mul(4).saturating_add(64);
    let mut chars = text.chars();
    let cut: usize = chars.by_ref().take(limit).map(char::len_utf8).sum();
    sanitize(&text[..cut])
}

/// [`truncate`] over text that is already safe to paint: the column arithmetic
/// alone, so a caller that has sanitized its own field does not pay for it
/// twice.
fn cut(text: &str, max: usize) -> (String, bool) {
    if max == 0 {
        return (String::new(), !text.is_empty());
    }
    let (out, _) = text.unicode_truncate(max);
    if out.len() == text.len() {
        return (text.to_string(), false);
    }
    let (body, _) = text.unicode_truncate(max - 1);
    (format!("{body}…"), true)
}

/// Lay out one row in the width it has.
///
/// The row answers "what is happening": the state (glyph, id, and the marks
/// beside it) is never sacrificed, then the branch and the line delta — facts
/// that exist nowhere else on the screen — then the activity, then the brief.
///
/// The activity is spent *before* the brief, which is the whole point of this
/// function: a row that fits its brief and its branch but not the sentence
/// saying what the agent is doing has spent its last columns on the one field
/// the screen can find elsewhere — the brief is one row below in the cursor
/// row's footer, and again as the transcript's opening line. Truncating the
/// brief to fit instead of reserving room for the activity is how a busy
/// agent's row came to read `◐ #1 delegate …  mush/1` with its current tool
/// call and its age gone (§4.5's first question, unanswered at 200×50).
///
/// A field is dropped whole rather than cut to a letter or two: a brief of
/// three columns is not a brief, and the footer carries the real one.
///
/// Every field is [`sanitize`]d before it is measured, because a row is a
/// terminal too: the head carries the branch a model chose, the tail carries
/// its own words about what it is doing, and the tail is never truncated, so
/// the rule cannot ride on [`truncate`] alone (see its doc).
pub fn fit_row(
    head: &str,
    brief: &str,
    branch_stat: &str,
    tail: &[String],
    width: usize,
) -> String {
    /// The least a field is worth: under this a long brief is dropped rather
    /// than cut (`cre…`), because the row would be spending its last columns on
    /// a word that is not one.
    const MIN_FIELD: usize = 7;

    let head = sanitize_upto(head, width);
    let brief = sanitize_upto(brief, width);
    let branch_stat = sanitize_upto(branch_stat, width);

    let head_width = UnicodeWidthStr::width(head.as_str());
    if width <= head_width + 2 {
        return head;
    }
    let budget = width - head_width - 1;
    let branch_width = UnicodeWidthStr::width(branch_stat.as_str());
    let show_branch = branch_width > 0 && branch_width + 2 <= budget.saturating_sub(4);
    let after_branch = budget.saturating_sub(if show_branch { branch_width + 2 } else { 0 });

    // The tail, reserved first and each cell whole.
    let mut cells = Vec::new();
    let mut remaining = after_branch;
    for cell in tail {
        let cell = sanitize_upto(cell, remaining);
        let cell_width = UnicodeWidthStr::width(cell.as_str());
        if remaining < cell_width + 2 {
            break;
        }
        cells.push(cell);
        remaining -= cell_width + 2;
    }

    let mut line = head;
    if !brief.is_empty() {
        // Whole, if it fits — a short title costs nothing — and otherwise only
        // when the columns left are enough to say something: a brief cut to
        // `cre…` is not a brief, and the row spends those columns on nothing
        // instead.
        let room = remaining.saturating_sub(1);
        if UnicodeWidthStr::width(brief.as_str()) <= room || room >= MIN_FIELD {
            line.push(' ');
            line.push_str(&cut(&brief, room).0);
        }
    }
    if show_branch {
        line.push_str("  ");
        line.push_str(&branch_stat);
    }
    for cell in cells {
        line.push_str("  ");
        line.push_str(&cell);
    }
    line.trim_end().to_string()
}

/// The nearest character boundary at or before `at`, never past the end of
/// `text`: `at` past the end is the end, so a caller passing a size and a cap in
/// any order cannot panic.
pub fn boundary_at_or_before(text: &str, at: usize) -> usize {
    let mut cut = at.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

/// The nearest character boundary at or after `at`, never past the end of
/// `text`: [`boundary_at_or_before`]'s twin, for the tail of a long result.
pub fn boundary_at_or_after(text: &str, at: usize) -> usize {
    let mut cut = at.min(text.len());
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    cut
}

/// Show only the edges of a secret for confirmation without leaking it.
pub fn mask_key(key: &str) -> String {
    // Four *characters*, not four bytes: `/key aéééé` must not panic on a
    // multi-byte boundary (finding B2).
    let chars: Vec<char> = key.trim().chars().collect();
    if chars.len() <= 8 {
        return "••••".to_string();
    }
    let first: String = chars[..4].iter().collect();
    let last: String = chars[chars.len() - 4..].iter().collect();
    format!("{first}…{last}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row spends its activity before its brief, and its state never goes.
    ///
    /// "What is each agent doing?" is the first question the screen exists to
    /// answer, and the activity is the only field that answers it — the brief's
    /// full text is one row below in the cursor row's footer. Truncating the
    /// brief to fit, which is what spending it first means, is how a busy
    /// agent's row came to read `◐ #1 delegate …  mush/1` with its current tool
    /// call gone.
    #[test]
    fn a_row_spends_its_activity_before_its_brief() {
        let head = "▶◐ #2";
        let activity = ["write deep.txt 3s".to_string()];

        // Roomy: state, brief, branch and activity together.
        assert_eq!(
            fit_row(head, "create a file", "mush/2 +8−0", &activity, 70),
            "▶◐ #2 create a file  mush/2 +8−0  write deep.txt 3s"
        );

        // 40 columns: the branch and the activity fit and the brief does not,
        // so the brief is what yields — cut to two columns it would be neither
        // a word nor here, so it goes.
        assert_eq!(
            fit_row(head, "create a file", "mush/2 +8−0", &activity, 40),
            "▶◐ #2  mush/2 +8−0  write deep.txt 3s"
        );

        // Narrower: the activity no longer fits whole, so it is dropped and the
        // brief spends what it can — a field goes whole.
        let narrow = fit_row(head, "create a file", "mush/2 +8−0", &activity, 34);
        assert_eq!(narrow, "▶◐ #2 create a file  mush/2 +8−0", "{narrow}");

        // Narrower: the branch is all that is left whole. Six columns of a
        // long brief would be `creat…`, which is not a word.
        assert_eq!(
            fit_row(head, "create a file", "mush/2 +8−0", &activity, 26),
            "▶◐ #2  mush/2 +8−0"
        );

        // A brief that fits whole is placed however little room is left for
        // it: `lexer` is a handle, not a sentence.
        assert_eq!(
            fit_row(head, "lexer", "mush/2 +8−0", &activity, 26),
            "▶◐ #2 lexer  mush/2 +8−0"
        );

        // Narrowest: the state alone, which is never dropped.
        assert_eq!(
            fit_row(head, "create a file", "mush/2 +8−0", &activity, 10),
            head
        );

        // And at no width does the row outgrow the columns it was given.
        for width in 8..=120usize {
            let row = fit_row(head, "create a file", "mush/2 +8−0", &activity, width);
            assert!(
                UnicodeWidthStr::width(row.as_str()) <= width,
                "{row:?} is wider than {width}"
            );
        }
    }

    /// The one-line path carries the rule too, so a caller that formats a fact
    /// into columns cannot leak what the wrapper would have removed. The rule
    /// used to ride on `wrap_capped` alone, and every surface that truncated
    /// instead of wrapping painted the raw bytes: an error body's `\r` returned
    /// the cursor over the pane's own border and its OSC renamed the window.
    #[test]
    fn a_fitted_line_is_defanged_before_it_is_cut() {
        assert_eq!(truncate("and then\rREPLACED", 40), "and then␍REPLACED");
        assert_eq!(truncate("\x1b]0;PWNED\x07done", 40), "done");
        assert_eq!(truncate("\x1b[2J\x1b[Hwiped", 5), "wiped");
        assert_eq!(truncate("saf\u{2066}e", 40), "safe");
        // The cut still counts columns of what is left, so a sequence that was
        // dropped cannot widen the result past its budget.
        let cut = truncate("safe \x1b]0;PWNED\x07 tail", 8);
        assert!(UnicodeWidthStr::width(cut.as_str()) <= 8, "{cut:?}");

        // Every field of a row, not only the brief that goes through
        // `truncate`: the head is built by the painter and the tail cells are
        // placed whole, with no cut to ride on.
        let row = fit_row(
            "▶◐ #2",
            "lexer",
            "mush/2 +1−0",
            &["boom\rREST \x1b[2J\x1b[Hwiped \x1b]0;PWNED\x07\u{2066}now".to_string()],
            60,
        );
        assert!(!row.contains('\x1b'), "{row:?}");
        assert!(!row.contains('\r'), "{row:?}");
        assert!(row.ends_with("boom␍REST wiped now"), "{row:?}");
        // And a row whose *title* carries the bytes: the brief is measured after
        // it is made safe, so the row still fits the columns it was given.
        let row = fit_row("▶ #1", &("x".repeat(30) + "\x1b]0;PWNED\x07"), "", &[], 20);
        assert!(!row.contains('\x1b'), "{row:?}");
        assert!(UnicodeWidthStr::width(row.as_str()) <= 20, "{row:?}");
    }

    /// The cut says whether it cut, which is the one thing a caller cannot read
    /// off the string: a text that already ends in `…` is not a text that was
    /// truncated, and a cut whose `…` replaced a dropped wide glyph has as many
    /// characters as the body it came from. Two callers used to guess, from the
    /// ellipsis and from the counts, and each guessed wrong on one of the two.
    #[test]
    fn the_cut_says_whether_it_cut() {
        let (same, cut) = truncate_flag("lexer", 40);
        assert_eq!(same, "lexer");
        assert!(!cut, "nothing was dropped");

        // The ellipsis belongs to the text, and the flag is not fooled.
        let (kept, cut) = truncate_flag("fix the …", 40);
        assert_eq!(kept, "fix the …");
        assert!(!cut);

        let (long, cut) = truncate_flag("abcdefghijkl", 8);
        assert_eq!(long, "abcdefg…");
        assert!(cut, "a cut text says so");

        // The cut whose ellipsis stands in for a wide glyph: two columns for
        // one character, so the result has the body's character count.
        let wide = format!("{}你", "x".repeat(9));
        let (cut_text, cut) = truncate_flag(&wide, 10);
        assert_eq!(cut_text.chars().count(), wide.chars().count());
        assert!(cut_text.ends_with('…'));
        assert!(cut, "columns were spent, whatever the counts say");

        // Nothing fits in nothing.
        assert_eq!(truncate_flag("text", 0), (String::new(), true));
        assert_eq!(truncate_flag("", 0), (String::new(), false));
    }

    /// A byte budget lands on a character boundary from either side: the walk
    /// is one rule, and the two ends it has to respect are the test.
    #[test]
    fn a_byte_cut_lands_on_a_character_boundary() {
        // a|é|中|b — five bytes of characters, seven bytes of text.
        let text = "aé中b";
        assert_eq!(boundary_at_or_before(text, 2), 1, "é starts at 1");
        assert_eq!(boundary_at_or_before(text, 3), 3);
        assert_eq!(boundary_at_or_before(text, 4), 3);
        assert_eq!(
            boundary_at_or_before(text, 0),
            0,
            "nothing before the start"
        );
        assert_eq!(
            boundary_at_or_before(text, 99),
            text.len(),
            "a cap past the end is the end, not a panic"
        );

        assert_eq!(boundary_at_or_after(text, 2), 3);
        assert_eq!(boundary_at_or_after(text, 4), 6);
        assert_eq!(boundary_at_or_after(text, 1), 1, "already a boundary");
        assert_eq!(boundary_at_or_after(text, 99), text.len());

        // Both halves of the same cut are strings the caller may slice with.
        for cut in 0..=text.len() {
            let head = boundary_at_or_before(text, cut);
            let tail = boundary_at_or_after(text, cut);
            assert!(text.is_char_boundary(head) && head <= cut, "{cut}");
            assert!(text.is_char_boundary(tail) && tail >= cut, "{cut}");
        }
    }

    /// The brief's first line, collapsed onto one row: the arithmetic the commit
    /// subject, an agent's title, a job's handle and a tool label all begin with
    /// (refactor R11).
    #[test]
    fn the_first_line_is_the_first_line_collapsed() {
        // A second line is not part of it, and neither is the whitespace around
        // the first: the run of blank lines between them collapses away.
        assert_eq!(first_line("  a\n\n  b  c \n"), "a");
        // Inner runs collapse too, so the four callers cannot disagree about a
        // brief written with two spaces after a sentence.
        assert_eq!(first_line("create  a   file\nand more"), "create a file");
        assert_eq!(first_line("one line"), "one line");
        assert_eq!(first_line("trailing   "), "trailing");
        assert_eq!(first_line(""), "");
        assert_eq!(first_line("\nsecond"), "");
    }

    #[test]
    fn wraps_on_word_boundaries() {
        let lines = wrap_text("the quick brown fox jumps", 10);
        assert!(lines.iter().all(|line| line.chars().count() <= 10));
        assert_eq!(lines.concat().replace(' ', ""), "thequickbrownfoxjumps");
    }

    /// A model's reply, a tool result or a file name can carry the bytes that
    /// command a terminal. None of them may leave a pane: a `\r` moved the
    /// cursor back over the border, an OSC set the window title, and a CSI wiped
    /// the frame.
    #[test]
    fn untrusted_text_cannot_command_the_terminal() {
        // An escape sequence goes whole: not its letter alone, not its digits.
        assert_eq!(sanitize("\x1b]0;PWNED\x07done"), "done");
        assert_eq!(sanitize("\x1b[2J\x1b[Hwiped"), "wiped");
        assert_eq!(sanitize("\x1b[1;31mred\x1b[0m"), "red");
        assert_eq!(sanitize("\x1b(Bascii"), "ascii");
        assert_eq!(sanitize("\x1b7saved"), "saved");
        // An unterminated sequence takes the rest of the text with it.
        assert_eq!(sanitize("before\x1b[38;5"), "before");
        // A carriage return is visible, because two words joined is a lie; the
        // `\r` of a CRLF is a line ending and shows nothing.
        assert_eq!(sanitize("and then\rREPLACED"), "and then␍REPLACED");
        assert_eq!(sanitize("line\r\nnext"), "line\nnext");
        assert_eq!(sanitize("line\r"), "line");
        // The controls that only ever commanded a display go, and a tab stays:
        // it is layout, and the wrapper is what gives it columns.
        assert_eq!(sanitize("a\x07b\x00c\x7fd"), "abcd");
        assert_eq!(sanitize("a\tb"), "a\tb");
        assert_eq!(sanitize("safe\u{202e}drowssap"), "safedrowssap");
        // And the ordinary text a transcript is made of is untouched.
        assert_eq!(sanitize("w00 w01 · #1 done: ✓"), "w00 w01 · #1 done: ✓");
    }

    /// The doc says what goes and what stays, and this pins both halves. Every
    /// character that can command the *order* a line is painted in leaves no
    /// trace — the bidi marks beside the embeddings and isolates — while the
    /// zero-width characters that command no order stay, because removing them
    /// would be the sanitizer editing the text it was asked to make safe: a ZWJ
    /// emoji and a ZWNJ word are the proof (finding B15).
    #[test]
    fn sanitize_strips_what_its_doc_says() {
        for ch in [
            '\u{061c}', // ALM
            '\u{200e}', // LRM
            '\u{200f}', // RLM
            '\u{202a}', // LRE
            '\u{202e}', // RLO
            '\u{2066}', // LRI
            '\u{2069}', // PDI
        ] {
            assert_eq!(
                sanitize(&format!("safe{ch}drowssap")),
                "safedrowssap",
                "{ch:?} commands the display"
            );
            // The one-line cut is a painter too, and must not put it back.
            assert_eq!(
                truncate(&format!("src{ch}/main.rs"), 40),
                "src/main.rs",
                "the cut sanitizes as it truncates"
            );
        }

        // Kept, and named in the doc: orthography, and zero-width characters
        // with no order to command.
        for ch in [
            '\u{200b}', // ZWSP
            '\u{200c}', // ZWNJ
            '\u{200d}', // ZWJ
            '\u{feff}', // BOM
            '\u{00ad}', // SHY
        ] {
            assert_eq!(
                sanitize(&format!("a{ch}b")),
                format!("a{ch}b"),
                "{ch:?} is not a command and stays"
            );
        }
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        assert_eq!(sanitize(family), family, "a ZWJ emoji survives whole");
    }

    /// A wrapped row is what the terminal will paint: no escape survives the
    /// wrapper, so a caller that forgets to sanitize cannot leak one either.
    #[test]
    fn wrapping_defangs_as_it_wraps() {
        let rows = wrap_text("start \x1b[2Jmiddle\rREPLACED tail", 40);
        assert_eq!(rows.join(" "), "start middle␍REPLACED tail");
        assert!(!rows.join("").contains('\x1b'));

        // A tab is still four columns of layout, and a line's end is still a row.
        assert_eq!(wrap_text("a\tb", 40), vec!["a    b".to_string()]);
        assert_eq!(
            wrap_text("one\r\ntwo", 40),
            vec!["one".to_string(), "two".to_string()]
        );
    }

    #[test]
    fn preserves_newlines() {
        assert_eq!(
            wrap_text("a\nb", 10),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn hard_splits_long_words() {
        let lines = wrap_text("abcdefghijklmnop", 5);
        assert!(lines.iter().all(|line| line.chars().count() <= 5));
        assert_eq!(lines.join(""), "abcdefghijklmnop");
    }

    /// A truncation budget is in columns, so a wide glyph must not overshoot it
    /// (finding B9).
    #[test]
    fn truncation_counts_columns_not_characters() {
        let wide = "日本語日本語";
        let cut = truncate(wide, 5);
        assert!(
            UnicodeWidthStr::width(cut.as_str()) <= 5,
            "{cut} is {} columns",
            UnicodeWidthStr::width(cut.as_str())
        );
        assert!(cut.ends_with('…'), "{cut}");
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("anything", 0), "");
        assert!(UnicodeWidthStr::width(truncate(wide, 1).as_str()) <= 1);
    }

    /// A wrapped row never outgrows the width it was given, whatever lands on
    /// the tail a space break left behind: a CJK glyph is two columns and a tab
    /// is four, and the terminal cuts whatever a row paints past its edge. Four
    /// columns is where a row starts to have room for either — the body of a
    /// pane is never narrower (the app's `MIN_BODY` is 4).
    #[test]
    fn a_wrapped_row_never_outgrows_its_width() {
        for text in [
            "ab c\t",
            " bcd日",
            "abc日",
            "日本語 日本語",
            "a\tb\tc",
            "\t先 后\t",
            "one two three 日本 four",
        ] {
            for width in 4..=12usize {
                for row in wrap_text(text, width) {
                    assert!(
                        UnicodeWidthStr::width(row.as_str()) <= width,
                        "{text:?} @ {width} painted {row:?}, {} columns",
                        UnicodeWidthStr::width(row.as_str())
                    );
                }
            }
        }
    }

    /// Showing the edges of an API key must count characters: slicing four
    /// bytes of a multi-byte key panicked (finding B2).
    #[test]
    fn masking_a_key_never_splits_a_character() {
        assert_eq!(mask_key("short"), "••••");
        assert_eq!(mask_key("aéééééééé"), "aééé…éééé");
        assert_eq!(mask_key(&"é".repeat(9)), "éééé…éééé");
    }

    /// The view's rows flattened to `(text, style)` pairs: for the tests that
    /// read the parse rather than the wrap.
    fn runs(text: &str) -> Vec<(String, RunStyle)> {
        markdown_rows(text, 80)
            .into_iter()
            .flatten()
            .map(|run| (run.text, run.style))
            .collect()
    }

    /// The view's rows as they are painted, styles dropped.
    fn rows(text: &str, width: usize) -> Vec<String> {
        markdown_rows(text, width)
            .into_iter()
            .map(|row| row.into_iter().map(|run| run.text).collect())
            .collect()
    }

    /// Assert a row never paints past `width`. The one exception the module
    /// states is a glyph wider than the whole width — a two-column glyph at a
    /// one-column pane — which no break can honour.
    fn assert_row_fits(row: &[Run], width: usize, what: &str) {
        let columns = runs_width(row);
        if columns <= width {
            return;
        }
        let widest = row
            .iter()
            .flat_map(|run| run.text.chars())
            .map(|ch| {
                if ch == '\t' {
                    4
                } else {
                    UnicodeWidthChar::width(ch).unwrap_or(1).max(1)
                }
            })
            .max()
            .unwrap_or(1);
        assert!(
            widest > width,
            "{what} @ {width}: {row:?} is {columns} columns"
        );
    }

    /// Every string of length 1..=`max_len` over `alphabet`, in order. The
    /// equality test's fuzz is generated rather than typed out, so its
    /// alphabet is readable and its reach is exact (finding B14).
    fn strings_over(alphabet: &[char], max_len: usize) -> Vec<String> {
        fn walk(alphabet: &[char], remaining: usize, current: &mut String, out: &mut Vec<String>) {
            if remaining == 0 {
                return;
            }
            for ch in alphabet {
                current.push(*ch);
                out.push(current.clone());
                walk(alphabet, remaining - 1, current, out);
                current.pop();
            }
        }
        let mut out = Vec::new();
        walk(alphabet, max_len, &mut String::new(), &mut out);
        out
    }

    /// A marker marks, and a marker that never closes is text — the characters
    /// as they were written, not half a span and not a dropped character. The
    /// same guard keeps a `*` used for arithmetic and a span of spaces as the
    /// text they are.
    #[test]
    fn a_span_is_read_and_an_unterminated_marker_is_text() {
        assert_eq!(
            runs("a **strong** word"),
            vec![
                ("a ".to_string(), RunStyle::Plain),
                ("strong".to_string(), RunStyle::Strong),
                (" word".to_string(), RunStyle::Plain),
            ]
        );
        assert_eq!(
            runs("a *light* word"),
            vec![
                ("a ".to_string(), RunStyle::Plain),
                ("light".to_string(), RunStyle::Emphasis),
                (" word".to_string(), RunStyle::Plain),
            ]
        );
        assert_eq!(
            runs("a _light_ word"),
            vec![
                ("a ".to_string(), RunStyle::Plain),
                ("light".to_string(), RunStyle::Emphasis),
                (" word".to_string(), RunStyle::Plain),
            ]
        );
        assert_eq!(
            runs("call `wrap_text` now"),
            vec![
                ("call ".to_string(), RunStyle::Plain),
                ("wrap_text".to_string(), RunStyle::Code),
                (" now".to_string(), RunStyle::Plain),
            ]
        );
        assert_eq!(
            runs("~~old~~ new"),
            vec![
                ("old".to_string(), RunStyle::Strike),
                (" new".to_string(), RunStyle::Plain),
            ]
        );

        // Unterminated: every character is the text it was.
        for text in [
            "a **strong word",
            "a *light word",
            "a _light word",
            "a `code word",
            "a ~~struck word",
            "2 * 3 * 4",
            "a ** ** b",
        ] {
            assert_eq!(
                runs(text),
                vec![(text.to_string(), RunStyle::Plain)],
                "{text:?} was read as a marker"
            );
        }
    }

    /// An underscore is a marker only at a word boundary and only alone:
    /// `snake_case_name` is one word, and `__strong__` — which is not a rule
    /// here — stays text rather than being read as half of one.
    #[test]
    fn an_underscore_inside_a_word_is_not_emphasis() {
        for text in ["snake_case_name", "a_x_", "__strong__", "x__y"] {
            assert_eq!(
                runs(text),
                vec![(text.to_string(), RunStyle::Plain)],
                "{text:?} was read as emphasis"
            );
        }
        assert_eq!(
            runs("_a_ and _(b)_"),
            vec![
                ("a".to_string(), RunStyle::Emphasis),
                (" and ".to_string(), RunStyle::Plain),
                ("(b)".to_string(), RunStyle::Emphasis),
            ]
        );
    }

    /// A heading is its text in the heading's style: the `#`s and the one space
    /// after them are not painted, because the style says what they said. One
    /// to three `#`s are a heading; a fourth is text, and so is a `#` with no
    /// space after it.
    #[test]
    fn a_heading_is_its_text_and_only_one_to_three_hashes_are_headings() {
        assert_eq!(
            runs("# Title"),
            vec![("Title".to_string(), RunStyle::Heading(1))]
        );
        assert_eq!(
            runs("### Deep"),
            vec![("Deep".to_string(), RunStyle::Heading(3))]
        );
        // A marker inside a heading is read, but the heading's style is the
        // only one it wears.
        assert_eq!(
            runs("## **Title**"),
            vec![("Title".to_string(), RunStyle::Heading(2))]
        );
        for text in ["#### Not a heading", "#nospace"] {
            assert_eq!(
                runs(text),
                vec![(text.to_string(), RunStyle::Plain)],
                "{text:?} was read as a heading"
            );
        }
        // A heading with no words is still a line: one empty row, not none.
        assert_eq!(rows("#", 20), vec![String::new()]);
    }

    /// A list keeps its marker and only styles it: the marker is information —
    /// `1.` is where the item sits — so hiding it would lose what the line
    /// said. The marker's run carries the one space after it, because that
    /// space is what a wrapped row hangs under ([`wrap_block`]). A marker
    /// needs its space, an ordered one is at most two digits, and an indented
    /// one is not a marker at all, because there is no nested list layout
    /// here.
    #[test]
    fn a_list_marker_stays_as_its_text_and_only_a_marker_is_styled() {
        for (text, marker) in [("- item", "-"), ("+ item", "+"), ("* item", "*")] {
            assert_eq!(
                runs(text),
                vec![
                    (format!("{marker} "), RunStyle::Bullet),
                    ("item".to_string(), RunStyle::Plain),
                ],
                "{text:?} is a bullet"
            );
        }
        for (text, marker) in [("1. first", "1."), ("99. last", "99.")] {
            assert_eq!(
                runs(text),
                vec![
                    (format!("{marker} "), RunStyle::Bullet),
                    (text[marker.len() + 1..].to_string(), RunStyle::Plain),
                ],
                "{text:?} is an item"
            );
        }
        // `1998.` opens a sentence, `-item` is a hyphenated word, and two
        // columns of indent are not a nesting this view has a layout for.
        for text in [
            "1998. It was a good year",
            "-item",
            "1.item",
            "  - item",
            "+not a bullet",
        ] {
            assert_eq!(
                runs(text),
                vec![(text.to_string(), RunStyle::Plain)],
                "{text:?} was read as a list"
            );
        }
    }

    /// A line of three or more `-`, `*` or `_` — nothing else on it, spaces
    /// between them allowed — is a horizontal rule: the pane draws `─` across
    /// its own width, and the characters the line was written with are
    /// scaffolding, like a heading's `#`s.
    ///
    /// `---` directly under a paragraph is the case the doc writes down:
    /// CommonMark reads it as a *setext heading*, and this view deliberately
    /// reads it as a rule, because a setext heading is not a rule here. The
    /// row is the pane's width and never a column more, which is what the
    /// narrowest-pane half of this test is for.
    #[test]
    fn a_rule_is_a_row_across_the_pane_and_its_characters_are_scaffolding() {
        for text in ["---", "- - -", "***", "___", "  ----  ", "- -- -"] {
            assert_eq!(rows(text, 12), vec!["─".repeat(12)], "{text:?} is a rule");
            assert_eq!(
                markdown_rows(text, 12),
                vec![vec![Run {
                    text: "─".repeat(12),
                    style: RunStyle::Rule,
                }]],
                "{text:?} is a rule"
            );
        }
        // Two markers are not a rule: `--` is a longer hyphen, `**` an
        // unclosed strong, `- -` a bullet whose item is a dash — and `- * -`
        // is prose about two markers, not a break.
        for text in ["--", "**", "__", "- -", "- * -", "a ---", "--- x", "~~~"] {
            assert!(
                rows(text, 12).iter().all(|row| !row.contains('─')),
                "{text:?} was read as a rule"
            );
        }
        // A rule under a paragraph is still a rule, and a rule is a row the
        // human can see — one entry in the map, not a source line that paints
        // nothing.
        assert_eq!(rows("Title\n---", 6), vec!["Title", "──────"]);
        assert_eq!(markdown_row_counts("a\n---\nb", 40), vec![1, 1, 1]);
        // The row is the pane's own width at every pane: a rule can never
        // paint past the edge it was drawn on.
        for width in 1..=8usize {
            assert_eq!(markdown_row_counts("---", width), vec![1]);
            assert_eq!(
                markdown_rows("---", width),
                vec![vec![Run {
                    text: "─".repeat(width),
                    style: RunStyle::Rule,
                }]],
                "a rule at {width} columns"
            );
        }
    }

    /// A quote's `>` becomes a bar: in a coding tool a `>` at the head of a
    /// line reads as a shell redirect, and the quote is not a command. The
    /// quoted words are kept as they were typed — additive, nothing of the
    /// quote is dropped — `>text` is the same quote as `> text`, the source's
    /// own indentation is kept (it is a `> ` inside a list), and a second `>`
    /// stays the character it is.
    #[test]
    fn a_quote_is_a_bar_and_the_words_are_untouched() {
        assert_eq!(
            runs("> quoted words"),
            vec![
                ("│ ".to_string(), RunStyle::Quote),
                ("quoted words".to_string(), RunStyle::Plain),
            ]
        );
        // No space after the `>`: the same marker, so the same row.
        assert_eq!(runs(">text"), runs("> text"));
        // The words inside a quote are read like any other words.
        assert_eq!(
            runs("> **bold** words"),
            vec![
                ("│ ".to_string(), RunStyle::Quote),
                ("bold".to_string(), RunStyle::Strong),
                (" words".to_string(), RunStyle::Plain),
            ]
        );
        // An indented quote is a quote, and it keeps the indent it was
        // written with: re-indenting it would move it out of its list item.
        assert_eq!(rows("  > indented", 40), vec!["  │ indented"]);
        // One marker per line: the inner `>` is the quoter's own character.
        assert_eq!(rows("> > nested", 40), vec!["│ > nested"]);
        // A `>` with no words is a row with the bar and nothing after it.
        assert_eq!(rows(">", 40), vec!["│"]);
        assert_eq!(rows("> ", 40), vec!["│"]);
        // The bar is a block rule: a `>` inside a line is the text it is, and
        // a quote line is one row in the map like any other line.
        for text in ["a > b", "=> arrow", "# > not a quote"] {
            assert!(
                !rows(text, 40)[0].contains('│'),
                "{text:?} was read as a quote"
            );
        }
        assert_eq!(markdown_row_counts("> a\n> b", 40), vec![1, 1]);
    }

    /// A task list's checkbox is a box: `[ ]` and `[x]`/`[X]` say one thing in
    /// three columns, and `☐`/`☑` say it in one, in the bullet's own style.
    /// The brackets and the space between them are scaffolding. Everything
    /// that is not a checkbox is text exactly as typed, and a box is one
    /// column wide because the pane's width arithmetic counts columns.
    #[test]
    fn a_task_list_is_a_box_in_the_bullets_style() {
        for (text, box_char) in [
            ("- [ ] todo", '☐'),
            ("- [x] done", '☑'),
            ("- [X] done", '☑'),
            ("* [ ] star", '☐'),
            ("+ [x] plus", '☑'),
        ] {
            let words = &text[6..];
            assert_eq!(
                runs(text),
                vec![
                    (format!("{} ", &text[..1]), RunStyle::Bullet),
                    (format!("{box_char} "), RunStyle::Bullet),
                    (words.to_string(), RunStyle::Plain),
                ],
                "{text:?} is a task item"
            );
        }
        // The words after the box are read like any other item's.
        assert_eq!(
            runs("- [ ] **bold** word"),
            vec![
                ("- ".to_string(), RunStyle::Bullet),
                ("☐ ".to_string(), RunStyle::Bullet),
                ("bold".to_string(), RunStyle::Strong),
                (" word".to_string(), RunStyle::Plain),
            ]
        );
        assert_eq!(rows("- [ ] todo", 40), vec!["- ☐ todo"]);
        // A box is one column: the row it is painted in counts it as one.
        assert_eq!(UnicodeWidthStr::width("☐"), 1);
        assert_eq!(UnicodeWidthStr::width("☑"), 1);
        // And what is not a checkbox is the characters it is: the brackets
        // are the item's own text, and the item keeps the marker it had.
        for (text, marker) in [
            ("- []", "-"),
            ("- [y]", "-"),
            ("- [ ]", "-"),
            ("- [ ] ", "-"),
            ("- [x]", "-"),
            ("- [ ]x", "-"),
            ("1. [ ] ordered", "1."),
            ("99. [x] ordered", "99."),
        ] {
            assert_eq!(
                runs(text),
                vec![
                    (format!("{marker} "), RunStyle::Bullet),
                    (text[marker.len() + 1..].to_string(), RunStyle::Plain),
                ],
                "{text:?} was read as a task item"
            );
        }
        assert_eq!(
            runs("[ ] no marker"),
            vec![("[ ] no marker".to_string(), RunStyle::Plain)]
        );
    }

    /// A wrapped item hangs under its own text: the marker is a margin on every
    /// row after the first, so the continuation of an item starts where the
    /// item's words start — two columns under `- `, three under `1. `, four
    /// under `10. ` and under a task item's `- ☐ `, and two under a quote's
    /// `│ `.
    ///
    /// This is the half of finding B14 that moved: the item's wrap is still the
    /// plain wrapper's wrap — [`wrap_text`] of the item's *text* at the columns
    /// the marker leaves, the same break points, tab stop and tail — with the
    /// marker's own width as a left margin on every continuation row. The other
    /// half is the pin that no row ever outgrows the width: a marker that
    /// leaves no column for the words is not a margin, and then the line is the
    /// plain wrapper's wrap of marker and text together.
    #[test]
    fn a_wrapped_item_hangs_under_its_own_text() {
        let text = "a bullet whose text is long enough to wrap";
        for width in 6..=40usize {
            let mut want = Vec::new();
            for (index, row) in wrap_text(text, width - 2).into_iter().enumerate() {
                want.push(if index == 0 {
                    format!("- {row}")
                } else {
                    format!("  {row}")
                });
            }
            assert_eq!(rows(&format!("- {text}"), width), want, "at {width}");
        }
        // A marker's own width is the margin, so what hangs under a number is
        // the text the number introduced.
        assert_eq!(
            rows("1. a first item that wraps", 12),
            vec!["1. a first", "   item", "   that", "   wraps"]
        );
        assert_eq!(
            rows("10. a tenth item that wraps", 12),
            vec!["10. a tenth", "    item", "    that", "    wraps"]
        );
        assert_eq!(
            rows("- [ ] a task that wraps", 12),
            vec!["- ☐ a task", "    that", "    wraps"]
        );
        assert_eq!(
            rows("> a quote that wraps", 12),
            vec!["│ a quote", "  that wraps"]
        );
        // The indentation of a quote inside a list is still the source's, and
        // it is part of the margin: `  ` then `│ ` is four columns.
        assert_eq!(
            rows("  > a quote that wraps", 12),
            vec!["  │ a quote", "    that", "    wraps"]
        );
        // A marker wider than the pane, or a glyph the margin leaves no column
        // for, is not a margin: the line is the plain wrapper's wrap of marker
        // and text together — every character painted, no row past the edge.
        for line in ["99. words", "- 日本語の項目"] {
            for width in 1..=6usize {
                for row in markdown_rows(line, width) {
                    assert_row_fits(&row, width, line);
                }
            }
        }
        assert_eq!(rows("99. words", 4), wrap_text("99. words", 4));
        // A marker whose room is real still hangs: at one column more than the
        // marker, the item's words get that column and the rows line up.
        assert_eq!(
            rows("99. words", 5),
            vec!["99. w", "    o", "    r", "    d", "    s"]
        );
        // Every kind of marker this view paints, at every pane: no row outgrows
        // the width, `width` columns included.
        for width in 1..=12usize {
            for line in [
                "- a long item here",
                "99. a long item here",
                "> a long quote here",
                "  > a long quote here",
                "- [ ] a long task here",
                "- 日本語の長い項目です",
            ] {
                for row in markdown_rows(line, width) {
                    assert_row_fits(&row, width, line);
                }
            }
        }
    }

    /// A fence is a block, and the fence lines are not painted: everything
    /// between them is code, one style, with no inline parsing — so the markers
    /// a model writes in code stay the characters they are. A fence that never
    /// closes runs to the end of the message, because an unterminated block is
    /// still a block and the code in it is still code. The one block whose
    /// fence lines *are* painted is the block that says nothing: with no body
    /// to hide there is no scaffolding, and a reply of nothing but a fence
    /// must be a turn the human can see (finding D14).
    #[test]
    fn a_fence_hides_its_lines_and_marks_the_code_between_them() {
        let text = "before\n```rust\nlet x = **1**;\t// tab\n```\nafter";
        assert_eq!(
            rows(text, 40),
            vec!["before", "let x = **1**;    // tab", "after"]
        );
        assert_eq!(
            markdown_rows(text, 40),
            vec![
                vec![Run {
                    text: "before".to_string(),
                    style: RunStyle::Plain,
                }],
                vec![Run {
                    text: "let x = **1**;    // tab".to_string(),
                    style: RunStyle::Fence,
                }],
                vec![Run {
                    text: "after".to_string(),
                    style: RunStyle::Plain,
                }],
            ]
        );

        let never_closed = "before\n```\nlet x = 1;\nstill code";
        assert_eq!(
            rows(never_closed, 40),
            vec!["before", "let x = 1;", "still code"]
        );
        assert_eq!(
            markdown_rows(never_closed, 40)
                .into_iter()
                .flatten()
                .map(|run| run.style)
                .collect::<Vec<_>>(),
            vec![RunStyle::Plain, RunStyle::Fence, RunStyle::Fence]
        );

        // A body that says nothing is not a body: the fence lines are the
        // whole of what was written, so they are rows of their own. Two fences
        // in a row are an empty block, not a fence that hides its own line.
        assert_eq!(rows("```", 40), vec!["```"]);
        assert_eq!(rows("```\n```", 40), vec!["```", "```"]);
        assert_eq!(rows("```\n\n```", 40), vec!["```", "", "```"]);
        assert_eq!(
            rows("a\n```\n\n```\nb", 40),
            vec!["a", "```", "", "```", "b"]
        );
        assert_eq!(rows("```\n \n```", 40), vec!["```", " ", "```"]);

        // And the row counts are the walk's own map, one entry per source
        // line: zeroes for the scaffolding a body hides, and the painted rows
        // for the lines that are text.
        assert_eq!(
            markdown_row_counts("```\nlet x = 1;\n```", 40),
            vec![0, 1, 0]
        );
        assert_eq!(markdown_row_counts("```\n```", 40), vec![1, 1]);
        assert_eq!(markdown_row_counts("a\n```\n```\nb", 40), vec![1, 1, 1, 1]);
        assert_eq!(
            markdown_row_counts("a\n```\nlet x = 1;\nb", 40),
            vec![1, 0, 1, 1]
        );
    }

    /// A link renders as its text and its URL, both: this is a coding tool, and
    /// a URL a model gave is data the human may have to copy out of the pane.
    /// The URL's own parentheses are counted, so a wiki link keeps its tail.
    #[test]
    fn a_link_keeps_its_text_and_its_url() {
        assert_eq!(
            runs("see [the docs](https://example.com/a) now"),
            vec![
                ("see ".to_string(), RunStyle::Plain),
                ("the docs".to_string(), RunStyle::Link),
                (" (https://example.com/a)".to_string(), RunStyle::Url),
                (" now".to_string(), RunStyle::Plain),
            ]
        );
        assert_eq!(
            runs("[wiki](https://en.wikipedia.org/wiki/Foo_(bar))"),
            vec![
                ("wiki".to_string(), RunStyle::Link),
                (
                    " (https://en.wikipedia.org/wiki/Foo_(bar))".to_string(),
                    RunStyle::Url
                ),
            ]
        );
        // A link that never closes, one with no `(`, and one with no URL to
        // show are all the text they are.
        for text in [
            "[docs](https://example.com/a",
            "[docs] https://example.com/a",
            "[docs]()",
        ] {
            assert_eq!(
                runs(text),
                vec![(text.to_string(), RunStyle::Plain)],
                "{text:?} was read as a link"
            );
        }
    }

    /// A line of nothing but markers is a line of text: the parser guesses
    /// nothing, so a lone `*` and a bare `~~` paint exactly those characters.
    #[test]
    fn a_line_of_only_markers_is_text() {
        for text in ["**", "*", "~~", "`", "_"] {
            assert_eq!(
                runs(text),
                vec![(text.to_string(), RunStyle::Plain)],
                "{text:?} was read as a marker"
            );
        }
    }

    /// A block line is still its own line: a heading, a bullet and a blank line
    /// never merge, split or re-indent the lines around them. The view is a
    /// reading of the source's own lines, never a reflow of them.
    #[test]
    fn a_block_line_never_reflows_the_lines_around_it() {
        let text = "# Title\n\n- one\n- two\n\n### Deep\nplain";
        assert_eq!(
            rows(text, 40),
            vec!["Title", "", "- one", "- two", "", "Deep", "plain"]
        );
    }

    /// An empty message is one empty row, exactly as `wrap_text` reads it: the
    /// view says the same thing the plain wrapper said rather than inventing or
    /// losing a row.
    #[test]
    fn an_empty_message_is_one_empty_row() {
        assert_eq!(markdown_rows("", 40), vec![Vec::<Run>::new()]);
        assert_eq!(
            markdown_rows("\n", 40),
            vec![Vec::<Run>::new(), Vec::<Run>::new()]
        );
    }

    /// The view's wrap is `wrap_text`'s wrap: for text that is not markdown the
    /// two make the same rows, character for character, at every width — one
    /// rule with two spellings, and this is the test that says they cannot
    /// drift.
    ///
    /// The second half is the fuzz that found the last divergence (finding
    /// B14): a no-break space is not a space, so a break that lands before one
    /// must leave it in the row on both roads. Every string up to length 5 over
    /// that alphabet, at widths 1..=12, is that fuzz kept as the pin.
    #[test]
    fn a_plain_message_wraps_exactly_like_wrap_text() {
        let texts = [
            "the quick brown fox jumps over the lazy dog",
            "one\n\ntwo three\nfour",
            "a\tb\tc and some words",
            "日本語のテキストと english words",
            "and then\rREPLACED \x1b]0;PWNED\x07 tail",
            "w00 w01 w02 w03 w04 w05",
            "  leading and trailing  ",
            "spaces  between  words",
            "bcd日 after a full row",
            "",
        ];
        for text in texts {
            for width in 1..=24usize {
                assert_eq!(
                    rows(text, width),
                    wrap_text(text, width),
                    "{text:?} @ {width}"
                );
            }
        }
        let alphabet = ['a', 'b', ' ', '\u{a0}', '\u{3000}', '\u{2028}', '\t'];
        for text in strings_over(&alphabet, 5) {
            for width in 1..=12usize {
                assert_eq!(
                    rows(&text, width),
                    wrap_text(&text, width),
                    "{text:?} @ {width}"
                );
            }
        }
    }

    /// A span that wraps keeps its style on both rows: the emphasis is a fact
    /// about the words, not about a row.
    #[test]
    fn a_span_that_wraps_keeps_its_style() {
        assert_eq!(
            markdown_rows("**alpha beta** gamma", 6),
            vec![
                vec![Run {
                    text: "alpha".to_string(),
                    style: RunStyle::Strong,
                }],
                vec![Run {
                    text: "beta".to_string(),
                    style: RunStyle::Strong,
                }],
                vec![Run {
                    text: "gamma".to_string(),
                    style: RunStyle::Plain,
                }],
            ]
        );
    }

    /// A row of the view never outgrows the width it was given: a wide glyph is
    /// two columns, a tab four, and an unbroken URL or path has no spaces to
    /// break at — it is cut at the width like any long word, and every piece of
    /// it is still painted.
    #[test]
    fn every_row_of_the_view_fits_its_width() {
        let text = "# 見出し\n\n- 项目 one two\n\nsee [the docs](https://example.com/a/very/long/path/that/never/breaks/anywhere) and 日本語\n\n```\nlet x = 1;\t// tab\n```\n\n**strong 日本語** tail";
        for width in 4..=40usize {
            for row in markdown_rows(text, width) {
                let painted: String = row.iter().map(|run| run.text.as_str()).collect();
                assert!(
                    UnicodeWidthStr::width(painted.as_str()) <= width,
                    "{painted:?} @ {width} is {} columns",
                    UnicodeWidthStr::width(painted.as_str())
                );
            }
        }

        // And the URL survives the cut whole, at every width.
        let url = "https://example.com/a/very/long/path/that/never/breaks/anywhere";
        let text = format!("see [the docs]({url})");
        for width in 4..=24usize {
            let flat: String = rows(&text, width).concat();
            assert!(
                flat.contains(url),
                "the URL lost its tail at {width}: {flat:?}"
            );
        }
    }

    /// A run of a marker is all-or-nothing: a rule spends every marker in the
    /// run it opens or closes with, and a run that is not a matched pair is
    /// text exactly as it was typed — so no row ever carries a marker left over
    /// from a run half-read as an opener or a closer. `***bold***` is a strong
    /// `bold` with both three-marker runs spent, the reading a model means; a
    /// one-marker rule pairs only with one marker, so `*a**` is the four
    /// characters it is; an underscore run of two or more is not a rule here
    /// (`__strong__` is text), so `___x___` is too; and a run beside a space,
    /// at a row's edge, inside code or a fence is text where it stands.
    #[test]
    fn a_run_of_a_marker_is_all_or_nothing() {
        // Three or more: the whole run opens and the whole run closes.
        assert_eq!(
            runs("***bold***"),
            vec![("bold".to_string(), RunStyle::Strong)]
        );
        assert_eq!(runs("****x****"), vec![("x".to_string(), RunStyle::Strong)]);
        // A run of two opens strong, and a run of two or more — however long —
        // closes it whole.
        assert_eq!(runs("**a***"), vec![("a".to_string(), RunStyle::Strong)]);
        assert_eq!(runs("***a**"), vec![("a".to_string(), RunStyle::Strong)]);
        // A one-marker rule closes only on one marker: these are the characters
        // they are, not half a span and a stray.
        for text in ["*a**", "**a*", "***x*", "*a***"] {
            assert_eq!(
                runs(text),
                vec![(text.to_string(), RunStyle::Plain)],
                "{text:?} was read as a span"
            );
        }
        // A run beside a space is text, whichever side the space is on; so is a
        // run of nothing but markers at the *end* of a row. At the start of a
        // row three or more of one marker are a rule instead — the rule test
        // pins that reading — so those lines are not in this list.
        for text in ["** a**", "**a **", "~~a ~~", "***a ***"] {
            assert_eq!(
                runs(text),
                vec![(text.to_string(), RunStyle::Plain)],
                "{text:?} was read as a span"
            );
        }
        // A run at the edge of a row that does open leaves no marker behind.
        assert_eq!(
            runs("a ***x*** b"),
            vec![
                ("a ".to_string(), RunStyle::Plain),
                ("x".to_string(), RunStyle::Strong),
                (" b".to_string(), RunStyle::Plain),
            ]
        );
        // A run that closes mid-line spends itself: `b***` is text, not a stray.
        assert_eq!(
            runs("**a**b***"),
            vec![
                ("a".to_string(), RunStyle::Strong),
                ("b***".to_string(), RunStyle::Plain),
            ]
        );
        // An underscore run of two or more is not a rule — `__strong__` stays
        // text — and a run of two opening on a run of one is the same refusal,
        // so `___x___` and `___x__` are the characters they are.
        for text in ["___x___", "___x__"] {
            assert_eq!(
                runs(text),
                vec![(text.to_string(), RunStyle::Plain)],
                "{text:?} was read as a span"
            );
        }
        // Strike spends the whole run too.
        assert_eq!(runs("~~x~~~"), vec![("x".to_string(), RunStyle::Strike)]);
        assert_eq!(runs("~~~x~~~"), vec![("x".to_string(), RunStyle::Strike)]);
        assert_eq!(runs("~~~x~~"), vec![("x".to_string(), RunStyle::Strike)]);
        // A run of backticks is one code span, opened and closed whole.
        for text in ["``x``", "``x`", "`x``"] {
            assert_eq!(
                runs(text),
                vec![("x".to_string(), RunStyle::Code)],
                "{text:?}"
            );
        }
        // Inside a code span, or a fence's body, inline parsing is off: a run of
        // asterisks there is the code's own characters.
        assert_eq!(
            runs("say `***x***` now"),
            vec![
                ("say ".to_string(), RunStyle::Plain),
                ("***x***".to_string(), RunStyle::Code),
                (" now".to_string(), RunStyle::Plain),
            ]
        );
        assert_eq!(
            markdown_rows("```\n***x***\n```", 40),
            vec![vec![Run {
                text: "***x***".to_string(),
                style: RunStyle::Fence,
            }]]
        );
        // A run split across a wrap was spent before the wrap: both rows carry
        // the whole strong span, and no marker comes back inside it.
        assert_eq!(
            markdown_rows("***alpha beta***", 6),
            vec![
                vec![Run {
                    text: "alpha".to_string(),
                    style: RunStyle::Strong,
                }],
                vec![Run {
                    text: "beta".to_string(),
                    style: RunStyle::Strong,
                }],
            ]
        );
    }
}
