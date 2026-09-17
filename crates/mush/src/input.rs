//! The message box's text, a cursor in grapheme clusters, and the window that
//! keeps the cursor on screen.
//!
//! Cursor math is the one place where "one char = one column" is a lie: a
//! combining mark, a ZWJ emoji, or a CJK glyph each change how much room a
//! string takes. Edits land on grapheme boundaries (`unicode-segmentation`) and
//! painting measures display columns (`unicode-width`, through
//! `unicode-truncate`), so the cursor and the renderer cannot disagree.

use unicode_segmentation::UnicodeSegmentation;
use unicode_truncate::UnicodeTruncateStr;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Default, Clone)]
pub struct Input {
    text: String,
    /// Cursor position in grapheme clusters; 0 is before the first.
    cursor: usize,
}

impl Input {
    /// Take the text out, as a send does, leaving an empty box.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert(&mut self, text: &str) {
        let at = self.byte_at(self.cursor);
        self.text.insert_str(at, text);
        self.cursor += text.graphemes(true).count();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_at(self.cursor);
        let start = self.byte_at(self.cursor - 1);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn delete_forward(&mut self) {
        if self.cursor >= self.graphemes() {
            return;
        }
        let start = self.byte_at(self.cursor);
        let end = self.byte_at(self.cursor + 1);
        self.text.replace_range(start..end, "");
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.graphemes());
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.graphemes();
    }

    /// Display columns the whole text occupies.
    pub fn width(&self) -> usize {
        UnicodeWidthStr::width(self.text.as_str())
    }

    /// Display columns before the cursor.
    pub fn cursor_column(&self) -> usize {
        UnicodeWidthStr::width(&self.text[..self.byte_at(self.cursor)])
    }

    /// The visible text for a field `width` columns wide, and the cursor's
    /// column inside it. The text scrolls horizontally so the cursor is always
    /// visible; `…` marks whichever edge is elided. A window narrower than two
    /// columns has no room for text and a cursor, so it shows the markers.
    pub fn window(&self, width: usize) -> (String, usize) {
        let width = width.max(2);
        let total = self.width();
        let cursor_col = self.cursor_column();

        // Fixed point: the markers depend on `skip`, and `skip` depends on how
        // many columns the markers leave. Each pass either settles or moves
        // `skip` right, and a rightward move cannot repeat.
        let mut skip = 0usize;
        for _ in 0..8 {
            let base = width - usize::from(skip > 0);
            let trailing = total > skip + base;
            let avail = base - usize::from(trailing);
            if avail == 0 || cursor_col < skip + avail {
                break;
            }
            let next = cursor_col + 1 - avail;
            if next <= skip {
                break;
            }
            skip = next;
        }

        let leading = skip > 0;
        let base = width - usize::from(leading);
        let trailing = total > skip + base;
        let avail = base - usize::from(trailing);

        let mut out = String::new();
        if leading {
            out.push('…');
        }
        if avail > 0 {
            let (prefix, _) = self.text.unicode_truncate(skip);
            let (content, _) = self.text[prefix.len()..].unicode_truncate(avail);
            out.push_str(content);
        }
        if trailing {
            out.push('…');
        }
        let column = usize::from(leading) + cursor_col.saturating_sub(skip);
        (out, column.min(width - 1))
    }

    fn graphemes(&self) -> usize {
        self.text.graphemes(true).count()
    }

    fn byte_at(&self, grapheme: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .nth(grapheme)
            .map(|(index, _)| index)
            .unwrap_or(self.text.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(text: &str, cursor: usize) -> Input {
        let mut input = Input::default();
        input.insert(text);
        input.cursor = cursor;
        input
    }

    /// The whole text, however wide the window has to be.
    fn text_of(input: &Input) -> String {
        input.window(10_000).0
    }

    #[test]
    fn edits_land_on_grapheme_boundaries() {
        // A combining acute is one grapheme but two chars.
        let mut typed = input("ae\u{301}", 1);
        typed.insert("b");
        assert_eq!(text_of(&typed), "abe\u{301}");
        typed.backspace();
        assert_eq!(text_of(&typed), "ae\u{301}");
        typed.move_end();
        typed.backspace();
        assert_eq!(text_of(&typed), "a");
    }

    #[test]
    fn a_zwj_emoji_is_one_edit() {
        let family = "\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f466}";
        let mut typed = input(&format!("x{family}y"), 2);
        typed.backspace();
        assert_eq!(text_of(&typed), "xy");
        typed.delete_forward();
        assert_eq!(text_of(&typed), "x");
    }

    #[test]
    fn arrows_stop_at_the_edges_and_edit_where_they_land() {
        let mut typed = input("hello", 0);
        typed.move_left();
        typed.insert("A");
        assert_eq!(text_of(&typed), "Ahello");
        typed.move_end();
        typed.move_right();
        typed.insert("Z");
        assert_eq!(text_of(&typed), "AhelloZ");
        typed.move_home();
        typed.delete_forward();
        assert_eq!(text_of(&typed), "helloZ");
    }

    #[test]
    fn the_window_follows_the_cursor() {
        // Cursor at the end of a long line: the tail is visible.
        let typed = input("abcdefghij", 10);
        let (text, column) = typed.window(5);
        assert_eq!(text, "…hij");
        assert_eq!(column, 4);
        assert!(column < 5, "the cursor must fit inside the field");

        // Cursor at the start: the head is visible.
        let typed = input("abcdefghij", 0);
        let (text, column) = typed.window(5);
        assert_eq!(text, "abcd…");
        assert_eq!(column, 0);

        // Short text: no markers at all.
        let typed = input("hi", 2);
        assert_eq!(typed.window(5), ("hi".to_string(), 2));
    }

    #[test]
    fn a_wide_glyph_counts_as_two_columns() {
        let typed = input("日本語", 3);
        assert_eq!(typed.width(), 6);
        assert_eq!(typed.cursor_column(), 6);
        let (text, column) = typed.window(5);
        assert_eq!(text, "…本語");
        assert_eq!(column, 4);
    }

    #[test]
    fn take_empties_the_box() {
        let mut typed = input("bye", 3);
        assert_eq!(typed.take(), "bye");
        assert_eq!(text_of(&typed), "");
    }
}
