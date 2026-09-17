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
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.split('\n') {
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
