//! Width and truncation arithmetic — one home for what a "column" means.
//!
//! Wrapping, truncation, row budgets and secret masks all count *display
//! columns*, so they live together and cannot drift (finding B9). Nothing here
//! touches a terminal: this is string arithmetic over `unicode-width`.

use unicode_truncate::UnicodeTruncateStr;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

    let head_width = UnicodeWidthStr::width(head);
    if width <= head_width + 2 {
        return head.to_string();
    }
    let budget = width - head_width - 1;
    let branch_width = UnicodeWidthStr::width(branch_stat);
    let show_branch = branch_width > 0 && branch_width + 2 <= budget.saturating_sub(4);
    let after_branch = budget.saturating_sub(if show_branch { branch_width + 2 } else { 0 });

    // The tail, reserved first and each cell whole.
    let mut cells = Vec::new();
    let mut remaining = after_branch;
    for cell in tail {
        let cell_width = UnicodeWidthStr::width(cell.as_str());
        if remaining < cell_width + 2 {
            break;
        }
        cells.push(cell);
        remaining -= cell_width + 2;
    }

    let mut line = head.to_string();
    if !brief.is_empty() {
        // Whole, if it fits — a short title costs nothing — and otherwise only
        // when the columns left are enough to say something: a brief cut to
        // `cre…` is not a brief, and the row spends those columns on nothing
        // instead.
        let room = remaining.saturating_sub(1);
        if UnicodeWidthStr::width(brief) <= room || room >= MIN_FIELD {
            line.push(' ');
            line.push_str(&truncate(brief, room));
        }
    }
    if show_branch {
        line.push_str("  ");
        line.push_str(branch_stat);
    }
    for cell in cells {
        line.push_str("  ");
        line.push_str(cell);
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

    #[test]
    fn wraps_on_word_boundaries() {
        let lines = wrap_text("the quick brown fox jumps", 10);
        assert!(lines.iter().all(|line| line.chars().count() <= 10));
        assert_eq!(lines.concat().replace(' ', ""), "thequickbrownfoxjumps");
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
