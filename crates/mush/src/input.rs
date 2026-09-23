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

use mush_core::text::sanitize;

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

    /// Whether the box is empty. The question Backspace asks before it pops an
    /// attachment: on an empty box there is no text for the key to delete, so
    /// the newest picture is what it means.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Whether the cursor is at the very start of the box — index 0, not the
    /// start of the cursor's own line. The question Backspace asks before it
    /// pops an attachment when there *is* text: the pictures are painted above
    /// the words, so at index 0 the newest one is the thing immediately before
    /// the cursor, where a plain backspace would have deleted nothing.
    pub fn is_at_start(&self) -> bool {
        self.cursor == 0
    }

    /// The box's text, newlines included.
    #[cfg(test)]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Insert `text` at the cursor, as a paste or a typed character does.
    ///
    /// The cursor advances by the *result's* count, not the paste's: a
    /// skin-tone modifier or a regional indicator joins the cluster before it,
    /// so a cursor advanced by the paste's own graphemes lands one past the end
    /// of the box — `backspace` then computes both byte positions past the end
    /// and the first keystroke deletes nothing (finding D20).
    pub fn insert(&mut self, text: &str) {
        let at = self.byte_at(self.cursor);
        self.text.insert_str(at, text);
        self.cursor = (self.cursor + text.graphemes(true).count()).min(self.graphemes());
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

    /// Move the cursor one painted row up, keeping its display column.
    ///
    /// A painted row is one of the box's lines: a line wider than the box is
    /// windowed sideways with `…` ([`window_line`]) instead of wrapped, so the
    /// rows the human moves between are the hard lines. The column kept is the
    /// *display* column the cursor had — the same measurement
    /// [`Self::cursor_line`] and the painter use — so the landing spot is the
    /// row above at that column, clamped to the row's end when the row is
    /// shorter, exactly as a wrapped editor clamps. A column that falls inside
    /// a wide glyph lands *before* it, never in the glyph's second cell, and a
    /// column inside a grapheme cluster is that cluster's boundary. A press on
    /// the first row is a no-op: the box does not wrap, and there is no row
    /// above to move to.
    pub fn move_up(&mut self) {
        self.move_row(-1);
    }

    /// [`Self::move_up`], one row down; a press on the last row is a no-op.
    pub fn move_down(&mut self) {
        self.move_row(1);
    }

    /// The arithmetic both directions share: the row `step` away, at the
    /// cursor's own display column (clamped where it lands), or nowhere when
    /// there is no such row.
    fn move_row(&mut self, step: i64) {
        let (row, column) = self.cursor_line();
        let target = row as i64 + step;
        if !(0..self.line_count() as i64).contains(&target) {
            return;
        }
        let target = target as usize;
        self.cursor = self.line_start(target) + self.grapheme_at_column(target, column);
    }

    /// The grapheme in `line` a display `column` can start on: the walk adds
    /// each grapheme's painted width and stops before the one that would cross
    /// the column, so the answer is a grapheme boundary and a wide glyph's
    /// second cell is never a landing spot. A column past the line's end is the
    /// line's end.
    fn grapheme_at_column(&self, line: usize, column: usize) -> usize {
        let mut width = 0;
        let mut index = 0;
        for grapheme in self.line(line).graphemes(true) {
            let painted = UnicodeWidthStr::width(sanitize(grapheme).as_str());
            if width + painted > column {
                break;
            }
            width += painted;
            index += 1;
        }
        index
    }

    /// Home is the start of the cursor's own line, not of the box: with more
    /// than one line, jumping to the very beginning is not what the key means.
    pub fn move_home(&mut self) {
        self.cursor = self.line_start(self.cursor_line().0);
    }

    pub fn move_end(&mut self) {
        let line = self.cursor_line().0;
        self.cursor = self.line_start(line) + self.line_graphemes(line);
    }

    /// How many lines the box holds (always at least one).
    pub fn line_count(&self) -> usize {
        self.text.split('\n').count()
    }

    /// The line the cursor is on (0-based) and its column within that line.
    ///
    /// The column is measured over the line [`sanitize`]d — the same text
    /// [`Self::view`] paints — so the cursor and the renderer cannot disagree
    /// about where the cursor is, which is this module's one contract. The
    /// stored draft keeps the human's own bytes; the column a terminal is
    /// told is the painted line's.
    pub fn cursor_line(&self) -> (usize, usize) {
        let before = &self.text[..self.byte_at(self.cursor)];
        let line = before.matches('\n').count();
        let start = before.rfind('\n').map(|at| at + 1).unwrap_or(0);
        (
            line,
            UnicodeWidthStr::width(sanitize(&before[start..]).as_str()),
        )
    }

    /// The text of one line, without its newline.
    pub fn line(&self, index: usize) -> &str {
        self.text.split('\n').nth(index).unwrap_or("")
    }

    /// The lines to paint in a box `rows` tall and `width` columns wide, with
    /// the cursor's row and column inside them. The box scrolls vertically so
    /// the cursor's line is always one of them — a long message must not push
    /// the line being typed off the top.
    ///
    /// Every line is [`sanitize`]d here, at the one road a frame's copy of the
    /// draft takes: the box holds the human's own bytes — a send must send
    /// exactly what was typed, and the select-mode copy reads its source — but
    /// a terminal acts on the text it is shown, and a paste or an attach
    /// client can carry an escape into the draft. This is the box's half of the
    /// rule `App::set_status` keeps for the bar's word: the strings mush sends
    /// are untouched, the strings it paints are defanged.
    pub fn view(&self, rows: usize, width: usize) -> (Vec<String>, usize, usize) {
        let rows = rows.max(1);
        let (cursor_line, cursor_col) = self.cursor_line();
        // Keep the cursor's line in view, preferring to show earlier lines.
        let first = cursor_line.saturating_sub(rows.saturating_sub(1));
        let mut lines = Vec::with_capacity(rows);
        for index in first..(first + rows).min(self.line_count()) {
            let (text, _) = window_line(&sanitize(self.line(index)), 0, width);
            lines.push(text);
        }
        // The window is re-run around the cursor's own column so the cursor
        // stays visible on a line wider than the box. Its row is always one of
        // the painted ones: `first` is the cursor's line or above it, and there
        // is a row for every line from `first` up to the box's height.
        let cursor_row = cursor_line - first;
        let (windowed, column) = window_line(&sanitize(self.line(cursor_line)), cursor_col, width);
        lines[cursor_row] = windowed;
        (lines, cursor_row, column)
    }

    /// Grapheme index where one line starts.
    fn line_start(&self, line: usize) -> usize {
        self.text
            .split('\n')
            .take(line)
            .map(|part| part.graphemes(true).count() + 1)
            .sum()
    }

    fn line_graphemes(&self, line: usize) -> usize {
        self.line(line).graphemes(true).count()
    }

    /// Display columns before the cursor.
    #[cfg(test)]
    pub fn cursor_column(&self) -> usize {
        UnicodeWidthStr::width(&self.text[..self.byte_at(self.cursor)])
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

/// One line of `text`, windowed to `width` columns with the cursor at
/// `cursor_col` kept visible; `…` marks whichever edge is elided. A window
/// narrower than two columns has no room for text and a cursor, so it shows the
/// markers. Shared by the single-line view and each line of a multi-line one, so
/// the two cannot disagree about what "visible" means.
fn window_line(text: &str, cursor_col: usize, width: usize) -> (String, usize) {
    let width = width.max(2);
    let total = UnicodeWidthStr::width(text);

    // Fixed point: the markers depend on `skip`, and `skip` depends on how many
    // columns the markers leave. Each pass either breaks or moves `skip`
    // strictly right (that is the `next <= skip` guard below), and a pass that
    // moves is followed by one that breaks: a trailing mark only ever turns off
    // as `skip` grows, so the value it moved to is already the one the test
    // accepts. The one exception — a *first* pass that moves with no trailing
    // mark — is followed by a move and then a break. Three passes at most, so
    // the loop is its own bound: a `0..8` cap said nothing the arithmetic does
    // not.
    let mut skip = 0usize;
    loop {
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
        let (prefix, _) = text.unicode_truncate(skip);
        let (content, _) = text[prefix.len()..].unicode_truncate(avail);
        out.push_str(content);
    }
    if trailing {
        out.push('…');
    }
    let column = usize::from(leading) + cursor_col.saturating_sub(skip);
    (out, column.min(width - 1))
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
        input.view(1, 10_000).0.join("\n")
    }

    /// "At the start" is the box's first grapheme, not the cursor's own line:
    /// a multi-line draft with the cursor at the start of its second line must
    /// not read as the start of the box.
    #[test]
    fn the_cursor_knows_the_start_of_the_box_from_the_start_of_its_line() {
        // Cursor 4 is the start of the second line, with "one\n" before it.
        let mut typed = input("one\ntwo", 4);
        assert!(!typed.is_at_start(), "the start of a line is not the box's");
        typed.move_home();
        assert!(
            !typed.is_at_start(),
            "Home is the line's start, not the box's"
        );
        typed.move_end();
        assert!(!typed.is_at_start());
        assert!(input("one\ntwo", 0).is_at_start(), "index 0 is the start");
        assert!(Input::default().is_at_start(), "an empty box starts at 0");
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

    /// The one-line case of `view`, which is what the box paints through.
    #[test]
    fn the_window_follows_the_cursor() {
        // Cursor at the end of a long line: the tail is visible.
        let (lines, row, column) = input("abcdefghij", 10).view(1, 5);
        assert_eq!(lines, vec!["…hij"]);
        assert_eq!((row, column), (0, 4));
        assert!(column < 5, "the cursor must fit inside the field");

        // Cursor at the start: the head is visible.
        let (lines, _, column) = input("abcdefghij", 0).view(1, 5);
        assert_eq!(lines, vec!["abcd…"]);
        assert_eq!(column, 0);

        // Short text: no markers at all.
        let (lines, _, column) = input("hi", 2).view(1, 5);
        assert_eq!(lines, vec!["hi"]);
        assert_eq!(column, 2);
    }

    #[test]
    fn a_wide_glyph_counts_as_two_columns() {
        let typed = input("日本語", 3);
        assert_eq!(typed.cursor_column(), 6, "three wide glyphs, six columns");
        let (lines, _, column) = typed.view(1, 5);
        assert_eq!(lines, vec!["…本語"]);
        assert_eq!(column, 4);
    }

    /// A message box that can hold newlines must not lose the line being typed
    /// off the top of its own view, and Home/End must mean the current line.
    #[test]
    fn a_multiline_box_keeps_the_cursors_line_in_view() {
        // Cursor on the last line (grapheme 8: past "one\ntwo\n").
        let mut typed = input("one\ntwo\nthree", 8);
        typed.move_end();
        assert_eq!(typed.cursor_line(), (2, 5), "End is the end of *this* line");
        typed.move_home();
        assert_eq!(
            typed.cursor_line(),
            (2, 0),
            "Home is the start of *this* line"
        );
        assert_eq!(typed.line_count(), 3);
        assert_eq!(typed.line(1), "two");

        // Two rows of a three-line box: the cursor's line must be one of them.
        let (lines, row, column) = typed.view(2, 40);
        assert_eq!(lines, vec!["two".to_string(), "three".to_string()]);
        assert_eq!(row, 1);
        assert_eq!(column, 0);

        // The whole box fits: every line is shown, cursor on its own.
        let (lines, row, _) = typed.view(5, 40);
        assert_eq!(lines, vec!["one", "two", "three"]);
        assert_eq!(row, 2);
    }

    /// A line wider than the box still windows around the cursor, exactly as the
    /// single-line box did.
    #[test]
    fn a_long_line_in_a_multiline_box_still_scrolls_sideways() {
        // Cursor on the second line (grapheme 6: past "short\n").
        let mut typed = input("short\nabcdefghij", 6);
        typed.move_end();
        let (lines, row, column) = typed.view(2, 5);
        assert_eq!(row, 1);
        assert_eq!(lines[1], "…hij");
        assert!(column < 5, "the cursor must fit inside the field");
    }

    /// Vertical movement goes between the rows the box paints — the lines, a
    /// wide one windowed sideways rather than wrapped — and keeps the cursor's
    /// display column. A shorter row clamps it to its end; coming back, the
    /// walk starts from the column the cursor really has, which is what a
    /// wrapped editor does.
    #[test]
    fn vertical_movement_keeps_the_display_column_across_the_boxes_rows() {
        // A first row wider than the box, cursor at its end (23 columns).
        let mut typed = input("a long row that windows\nshort\nanother row", 23);
        assert_eq!(typed.cursor_line(), (0, 23));
        typed.move_down();
        assert_eq!(
            typed.cursor_line(),
            (1, 5),
            "a shorter row clamps the column to its end"
        );
        typed.move_down();
        assert_eq!(typed.cursor_line(), (2, 5), "the next row keeps it");
        typed.move_up();
        typed.move_up();
        assert_eq!(
            typed.cursor_line(),
            (0, 5),
            "the row above is the one landed on, at the column it had"
        );

        // The wide row's window follows the cursor's column there: the cursor
        // is on screen, at the column it kept.
        let (lines, row, column) = typed.view(1, 10);
        assert_eq!(row, 0);
        assert_eq!(lines, vec!["a long ro…".to_string()]);
        assert!(column < 10, "the cursor must fit inside the field");
    }

    /// The first and the last row are walls, not wraps: a press there leaves
    /// the cursor — and the text — exactly where it was. An empty box has one
    /// row and is both walls at once.
    #[test]
    fn vertical_movement_at_the_first_and_last_row_is_a_no_op() {
        let mut typed = input("one\ntwo", 0);
        typed.move_up();
        assert_eq!(
            typed.cursor_line(),
            (0, 0),
            "there is no row above the first"
        );
        typed.move_end();
        typed.move_up();
        assert_eq!(
            typed.cursor_line(),
            (0, 3),
            "End is the first row's end, and Up is still a wall"
        );
        typed.move_down();
        assert_eq!(typed.cursor_line(), (1, 3));
        typed.move_down();
        assert_eq!(
            typed.cursor_line(),
            (1, 3),
            "there is no row below the last"
        );
        assert_eq!(typed.text(), "one\ntwo", "no move edited the text");

        let mut empty = Input::default();
        empty.move_up();
        empty.move_down();
        assert_eq!(empty.cursor_line(), (0, 0));
        assert!(empty.is_empty(), "and the box is still empty");
    }

    /// A wide glyph's second cell is not a place a cursor can be: a display
    /// column inside one lands before the glyph, and a column inside a grapheme
    /// cluster is that cluster's boundary.
    #[test]
    fn vertical_movement_never_lands_inside_a_wide_glyph() {
        // Cursor after the first wide glyph (grapheme 1, column 2).
        let mut typed = input("日本語\nx", 1);
        assert_eq!(typed.cursor_line(), (0, 2));
        typed.move_down();
        assert_eq!(typed.cursor_line(), (1, 1));
        typed.move_up();
        assert_eq!(
            typed.cursor_line(),
            (0, 0),
            "column 1 is 日's second cell: the cursor stops before the glyph"
        );

        // A combining acute is one cluster: the cursor is before or after it,
        // never between the mark and the `e` it belongs to.
        let mut marked = input("ae\u{301}\nx", 1);
        marked.move_down();
        marked.move_up();
        assert_eq!(marked.cursor_line(), (0, 1));
        marked.move_right();
        assert_eq!(marked.cursor_line(), (0, 2), "past the whole cluster");
    }

    /// A paste whose last cluster merges with the one before it leaves the
    /// cursor at the end of the box, not one past it: the first Backspace
    /// deletes the merged cluster instead of nothing (finding D20).
    #[test]
    fn a_paste_that_merges_with_the_grapheme_before_it_leaves_the_cursor_in_the_box() {
        // A skin-tone modifier joins the `x` it follows into one cluster.
        let mut typed = input("x", 1);
        typed.insert("\u{1f3fd}");
        assert_eq!(text_of(&typed), "x\u{1f3fd}");
        assert_eq!(
            typed.cursor,
            text_of(&typed).graphemes(true).count(),
            "the cursor names a position inside the box"
        );
        typed.backspace();
        assert_eq!(
            text_of(&typed),
            "",
            "the first Backspace deletes the cluster the paste merged into"
        );
    }

    #[test]
    fn take_empties_the_box() {
        let mut typed = input("bye", 3);
        assert_eq!(typed.take(), "bye");
        assert_eq!(text_of(&typed), "");
    }
}
