//! Width and truncation arithmetic — one home for what a "column" means.
//!
//! Wrapping, truncation, row budgets and secret masks all count *display
//! columns*, so they live together and cannot drift (finding B9). Nothing here
//! touches a terminal: this is string arithmetic over `unicode-width`.

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
/// - every other **C0/C1 control** and `DEL` is dropped, along with the bidi
///   embedding and isolate characters: they exist to command a display rather
///   than to be read, and the one U+200D a ZWJ emoji needs is not among them.
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
fn invisible(ch: char) -> bool {
    ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
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
            loop {
                if current_width + char_width <= width || current.is_empty() {
                    break;
                }
                if let Some(space) = last_space {
                    let rest = current.split_off(space);
                    out.push(std::mem::take(&mut current));
                    current = rest.trim_start().to_string();
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
    /// A list's own marker (`-`, `*`, `+`, `1.`), kept rather than hidden.
    Bullet,
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
/// renderer. Tables, block quotes, setext headings, reference links, HTML,
/// task-list checkboxes, nested lists and indented code blocks are all *not*
/// rules; a line that uses one is simply the text it is.
///
/// It is **additive** too. The only text a rule removes is scaffolding a human
/// does not read in a view — the `#`s of a heading and the two fence lines of a
/// code block. Every word is kept; a list keeps its marker and only styles it,
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
/// - one, two or three `#`s and a space → [`RunStyle::Heading`]. The `#`s and
///   that one space are not painted: the heading's style says what they said.
///   Four or more `#`s, or a `#` with no space after it, are text.
/// - `- `, `* `, `+ `, or `1. `–`99. ` → the marker keeps its place and is
///   styled [`RunStyle::Bullet`]. A marker with no space after it is not one,
///   and an ordered marker is at most two digits, because `1998. It was a good
///   year` opens a sentence, not a list.
/// - a line whose first non-space text is three backticks opens a fenced block,
///   and the next such line closes it. The fence lines are not painted and
///   everything between them is [`RunStyle::Fence`], one style with no inline
///   parsing, so `**` in code stays code. A fence that never closes runs to the
///   end of the message: an unterminated block is still a block, and the code
///   in it is still code.
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
/// narrow. The rows this returns are the rows the plain wrapper would have
/// made for the same text, with the styles attached.
pub fn markdown_rows(text: &str, width: usize) -> Vec<Vec<Run>> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut fence = false;
    for raw in text.split('\n') {
        let line = sanitize(raw);
        if fence_line(&line) {
            // The fence is scaffolding, not content: it is a block boundary,
            // and a row of backticks is not something a human reads. The code
            // inside is untouched — see the never-closing fence above.
            fence = !fence;
            continue;
        }
        let runs = if fence {
            vec![Run {
                text: line,
                style: RunStyle::Fence,
            }]
        } else {
            block(&line)
        };
        out.extend(wrap_runs(&runs, width));
    }
    out
}

/// Whether a line is a fence, opening or closing one. The run of backticks is
/// not counted: three or more at the start of the line toggle the block, and an
/// info string after them is part of the fence, not of the code.
fn fence_line(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// One source line, parsed before it is wrapped: a heading, a list item, or
/// whatever the inline rules make of it.
fn block(line: &str) -> Vec<Run> {
    if let Some((level, text)) = heading(line) {
        // The heading's style is the whole heading: a marker inside it is read
        // (so `## **Title**` does not paint its asterisks) but the runs all
        // come out as the heading, because that is the only style it wears.
        let mut runs = inline(text);
        for run in &mut runs {
            run.style = RunStyle::Heading(level);
        }
        return runs;
    }
    if let Some((marker, text)) = list_marker(line) {
        let mut runs = vec![Run {
            text: marker.to_string(),
            style: RunStyle::Bullet,
        }];
        runs.extend(inline(text));
        return runs;
    }
    inline(line)
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
    /// said. A marker needs its space, an ordered one is at most two digits,
    /// and an indented one is not a marker at all, because there is no nested
    /// list layout here.
    #[test]
    fn a_list_marker_stays_as_its_text_and_only_a_marker_is_styled() {
        for (text, marker) in [("- item", "-"), ("+ item", "+"), ("* item", "*")] {
            assert_eq!(
                runs(text),
                vec![
                    (marker.to_string(), RunStyle::Bullet),
                    (" item".to_string(), RunStyle::Plain),
                ],
                "{text:?} is a bullet"
            );
        }
        for (text, marker) in [("1. first", "1."), ("99. last", "99.")] {
            assert_eq!(
                runs(text),
                vec![
                    (marker.to_string(), RunStyle::Bullet),
                    (text[marker.len()..].to_string(), RunStyle::Plain),
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

    /// A fence is a block, and the fence lines are not painted: everything
    /// between them is code, one style, with no inline parsing — so the markers
    /// a model writes in code stay the characters they are. A fence that never
    /// closes runs to the end of the message, because an unterminated block is
    /// still a block and the code in it is still code.
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

        // The fence line itself is not a row: a message that is only a fence
        // paints nothing at all.
        assert_eq!(markdown_rows("```", 40), Vec::<Vec<Run>>::new());
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
        // run of nothing but markers, at the start or the end of a row.
        for text in [
            "** a**", "**a **", "~~a ~~", "***a ***", "***", "****", "******",
        ] {
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
