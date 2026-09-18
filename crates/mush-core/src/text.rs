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

            if current_width + char_width > width && !current.is_empty() {
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
/// A caller that has to *say* that it cut — a subject, a digest — is asking a
/// question only the arithmetic can answer, and two callers used to answer it by
/// looking at the string instead: `subject_brief` read a trailing `…`, and
/// `Outcome::digest` compared character counts. Both were wrong about the same
/// edge, in opposite directions: a brief that really ends in `…` is not a brief
/// that was cut (the word before it was swallowed), and a cut whose `…` stands in
/// for a dropped wide glyph has the same number of characters as the body (so the
/// digest fell silent about the very text it hid).
///
/// The flag is “the returned text is not the whole text”, so `truncate(text, 0)`
/// on a non-empty text is a cut and on an empty text is not.
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
///
/// The flag says whether the text came back whole, which is the one thing a
/// caller cannot read off the string (see [`truncate_flag`]).
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

    /// Showing the edges of an API key must count characters: slicing four
    /// bytes of a multi-byte key panicked (finding B2).
    #[test]
    fn masking_a_key_never_splits_a_character() {
        assert_eq!(mask_key("short"), "••••");
        assert_eq!(mask_key("aéééééééé"), "aééé…éééé");
        assert_eq!(mask_key(&"é".repeat(9)), "éééé…éééé");
    }
}
