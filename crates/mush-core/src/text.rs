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
pub fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let (out, _) = text.unicode_truncate(max);
    if out.len() == text.len() {
        return text.to_string();
    }
    let (body, _) = text.unicode_truncate(max - 1);
    format!("{body}…")
}

/// Lay out one row in the width it has.
///
/// The row answers "what is happening": the state (glyph, id) is never
/// sacrificed, then the branch and line delta — facts that exist nowhere else on
/// the screen — then the brief, then the activity, which the bar already repeats
/// for the focused agent. Fields are dropped from the right when the pane is
/// narrow, and the cursor row's full facts are one row below in the footer.
pub fn fit_row(
    head: &str,
    brief: &str,
    branch_stat: &str,
    tail: &[String],
    width: usize,
) -> String {
    let head_width = UnicodeWidthStr::width(head);
    if width <= head_width + 2 {
        return head.to_string();
    }
    let budget = width - head_width - 1;
    let branch_width = UnicodeWidthStr::width(branch_stat);
    let show_branch = branch_width > 0 && branch_width + 2 <= budget.saturating_sub(4);
    let after_branch = budget.saturating_sub(if show_branch { branch_width + 2 } else { 0 });

    let mut line = head.to_string();
    let mut remaining = budget;
    if after_branch >= 7 && !brief.is_empty() {
        let text = truncate(brief, after_branch - 1);
        // `truncate` budgets display columns now, so this width is the truth.
        remaining = remaining.saturating_sub(UnicodeWidthStr::width(text.as_str()) + 1);
        line.push(' ');
        line.push_str(&text);
    }
    if show_branch {
        line.push_str("  ");
        line.push_str(branch_stat);
        remaining = remaining.saturating_sub(branch_width + 2);
    }
    for cell in tail {
        let cell_width = UnicodeWidthStr::width(cell.as_str());
        if remaining < cell_width + 2 {
            break;
        }
        line.push_str("  ");
        line.push_str(cell);
        remaining -= cell_width + 2;
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

    /// A row gives up its least useful field first: the activity goes before
    /// the branch, the branch before the brief, and the state never goes.
    #[test]
    fn a_row_gives_up_its_brief_before_its_facts() {
        let head = "▶◐ #2  ";
        let activity = ["write deep.txt 3s".to_string()];
        let wide = fit_row(head, "create a file", "mush/2 +8−0", &activity, 70);
        assert_eq!(
            wide,
            "▶◐ #2   create a file  mush/2 +8−0  write deep.txt 3s"
        );

        // Narrow: the activity goes, the branch and stat stay.
        let narrow = fit_row(head, "create a file", "mush/2 +8−0", &activity, 34);
        assert!(narrow.contains("mush/2 +8−0"), "{narrow}");
        assert!(!narrow.contains("write deep.txt"), "{narrow}");

        // Narrower: the brief yields too, the branch still stays.
        let tighter = fit_row(head, "create a file", "mush/2 +8−0", &activity, 26);
        assert!(tighter.contains("mush/2 +8−0"), "{tighter}");
        assert!(!tighter.contains("create"), "{tighter}");

        // Narrowest: the state alone, which is never dropped (the row is
        // trimmed, so the padded id loses its trailing spaces).
        assert_eq!(
            fit_row(head, "create a file", "mush/2 +8−0", &activity, 10),
            head.trim_end()
        );
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
