//! The `usages` tool's text: who mentions a symbol, word by word, file by file.
//!
//! "Who uses this?" has two answers in a coding tool, and the honest one here is
//! the small one. A **compiler's** answer resolves the name: which `held` is the
//! `held` in scope, which mentions are calls and which are a local that happens
//! to share the spelling, and what a macro expanded to on a line no file holds.
//! mush has no name resolution, no scope and no call graph, and will not grow
//! one inside a text tool. So the answer this module builds is **textual**: a
//! line is a row when the symbol stands in it as a *word*, and every answer says
//! so on its own first line (`agent.rs`'s `usages_tool` carries that sentence,
//! beside the result it describes). The tool is deliberately not named
//! `references`: that word promises the compiler's answer, and a model that
//! reads the promise writes a summary the rule cannot support — "no references"
//! as if scope had been consulted.
//!
//! **The word boundary, exactly.** A word character is an identifier's:
//! alphanumerics and `_` — the same predicate [`crate::outline`]'s
//! `leading_word` reads a name with, Unicode letters included, because Rust
//! identifiers are Unicode and an ASCII-only boundary would silently split a
//! real one. The symbol is a hit when `symbol` occurs in the line and the
//! character before its first byte and the character after its last are not
//! word characters (or do not exist). So `held` in `beheld` is not a row,
//! `held_x` is not either, while `self.held`, `Type::held` and `&held` are:
//! `.`, `:` and `&` are not word characters.
//!
//! **The alphabet is Rust's, and one reading of it is worth naming.** `$` and
//! `#` are not word characters either, so `usages("foo")` reports `$foo` (a
//! JavaScript identifier of its own) and `usages("include")` reports
//! `#include` (a C directive): the character before the name is simply not a
//! word character. The other reading is worse — making `$` a word character
//! would make the Rust macro metavariable `$name` unmentionable, a miss for a
//! line that plainly spells the name — and this rule is written for Rust. The
//! tool cannot tell one language's identifier alphabet from another's, and
//! does not pretend to.
//!
//! A needle that is one non-word character is walked like any other word: `-`
//! is a hit in `a - b` and not in `a-b`, because a hit needs a non-word
//! character on *both* sides. A needle of whitespace is no special case either
//! — it is not the *empty* needle, which the door refuses — so `usages(" ")`
//! walks, and a space is a hit in `a = = b` and not in `a = b`.
//!
//! That is the whole rule, including its most arguable reading: **`.` and `::`
//! do not split a needle**. Asking for `method` hits `self.method()` and
//! `Type::method` — the tool cannot tell a field from a local, and a rule that
//! guessed "a mention after a dot is a receiver" would be the first sentence of
//! the scope analysis this tool exists without. Asking for `Type::method` is a
//! longer needle, matched where that whole phrase stands at boundaries: it
//! narrows the answer rather than resolving it. The invariant the tests pin is
//! the one the rule can keep — **every row's line really holds the symbol at a
//! boundary** — and the sweep at the bottom of this file checks it against every
//! `.rs` file in this checkout, with a second, character-walking implementation
//! of the rule as the witness.
//!
//! **The declaration-looking row.** "Who uses this?" is half of the question;
//! the other half is "where is it defined?", and a file whose row is the
//! declaration itself is where the answer begins. Whether a line looks like a
//! declaration is not a second rule: it is [`crate::outline::is_declaration`]'s
//! one rule, the same predicate the `outline` tool counts rows with, so the two
//! tools cannot disagree about what a declaration is. It is *looking*, not
//! defining: a macro-generated item has no line here, an `impl Foo` line counts
//! as declaration-looking without being the definition of `Foo`, and the
//! answer's header says "textual … not a compiler's answer" for exactly this.
//! The definition-looking rows are listed first within their file (the file's
//! own order among them), then every other mention in the file's order.
//!
//! **The rows.** A row is the file's 1-based line number and that line, cut to
//! [`crate::outline::ROW_WIDTH`] with `…` where it was cut, and never cut
//! through the word that makes a declaration-looking line one
//! ([`crate::outline::row_text`], reused for that property). Rows are the
//! reader's lines ([`str::lines`]) and not [`crate::text::file_lines`]'s: a row
//! is a *location*, and the number is the anchor — `read_file {offset}` shows
//! the bytes, `\r` of a CRLF ending and all — where a search's match line is
//! shown to be copied (`search` takes the bytes for that reason). A line longer
//! than the width is cut for the same reason an outline row is: one minified
//! file must not spend the answer. The cut is a *prefix*, so a mention past the
//! width is a row whose text does not reach its own word: the row's claim is its
//! line number, and `read_file {offset}` shows the line. The list itself is
//! bounded the same way, and by the answer rather than by the file:
//! [`rows_within`] is the walk's reader, and it builds one row past the room the
//! answer has left rather than every row the file holds — a generated file can
//! hold a row on each of a million lines, and the answer it can spend is ten.
//!
//! A leading BOM is not a line's text either: U+FEFF at the very start of a
//! file is the UTF-8 signature, not the first line's first character
//! ([`crate::text::strip_bom`]), so a Windows editor's `\u{feff}fn f() {}` is
//! read as the declaration it spells instead of as a first word that is
//! neither `fn` nor a name.

use crate::outline::{is_declaration, row_text as outline_row_text, ROW_WIDTH};
use crate::text;

/// One line that mentions the symbol: the file's 1-based line number, the
/// row's own text (the line, cut to [`ROW_WIDTH`]), and whether the outline
/// rule reads the line as a declaration.
///
/// The text is the line itself — leading whitespace and all — cut with `…`
/// where it was cut, so the number and the text together are the anchor a
/// `read_file {offset}` is built from, exactly as an outline row is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Usage {
    pub line: usize,
    pub text: String,
    /// The line is declaration-looking by [`crate::outline::is_declaration`] —
    /// textually, not semantically. [`FileUsages::rows`] lists these first.
    pub definition: bool,
}

/// One file's rows: the file's name as the model named it, and every line that
/// mentions the symbol, the declaration-looking ones first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileUsages {
    pub file: String,
    pub rows: Vec<Usage>,
}

impl FileUsages {
    /// The line numbers of the declaration-looking rows, ascending — the rows
    /// [`Self::rows`] leads with, so the answer can name the definition site
    /// in the group header without a second walk.
    ///
    /// It reads the rows' own order rather than a second list of line numbers:
    /// two lists that can disagree is the drift this avoids, and the order is
    /// the invariant the tests pin ([`rows`] builds it).
    pub fn declarations(&self) -> Vec<usize> {
        self.rows
            .iter()
            .take_while(|row| row.definition)
            .map(|row| row.line)
            .collect()
    }
}

/// What a `usages` walk found: one group per file that mentions the symbol, and
/// what the walk could not read.
///
/// The three counters are [`crate::workspace::Matches`]'s own, for the same
/// reason [`crate::workspace::Workspace::search`] carries them: a model reads a
/// miss as "the symbol is not there", so a walk that skipped a file, or could
/// not name one, counts it instead of answering a false negative. `scanned` is
/// the other half — the files whose bytes *were* read — so a miss can say what
/// it did look at.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usages {
    pub groups: Vec<FileUsages>,
    /// Files whose bytes were read and searched — hits or no hits.
    pub scanned: usize,
    /// The row cap cut the walk: at least one further row exists beyond the
    /// rows here (the walk stops at the first row it cannot keep).
    pub more: bool,
    /// Files the walk never opened: binary (a NUL byte), or past
    /// [`crate::workspace::SEARCH_FILE_CAP`].
    pub skipped: usize,
    /// Files whose name cannot travel on the model's road
    /// (`Workspace::name_for_model`): a row under such a name would be a dead
    /// end, so the file is counted and not searched.
    pub unnamed: usize,
}

impl Usages {
    /// Every row of every group — the answer's own count, so the header and the
    /// groups cannot disagree about how many hits were shown.
    pub fn hits(&self) -> usize {
        self.groups.iter().map(|group| group.rows.len()).sum()
    }
}

/// Whether `line` mentions `symbol` at a word boundary: the whole rule, in one
/// predicate (see the module doc for what a boundary is and why).
///
/// The scan is from the left and continues past a boundary-less occurrence: a
/// line may hold `beheld held`, where the first occurrence is not a hit and the
/// second is — stopping at the first would be a false miss. The step is one
/// *character*, never one byte: a symbol's first byte may open a multi-byte
/// character, and a step into its middle would not be a string boundary.
///
/// A needle holding a line break is not a case this predicate can decide: a
/// line never holds one, so every call answers `false`, and the tool refuses
/// such a needle at its own door ([`crate::workspace::Workspace::usages`])
/// rather than walking the tree for a guaranteed miss. A carriage return is
/// different — a lone one *is* a line's text — and is walked like any other
/// character.
pub fn is_usage(line: &str, symbol: &str) -> bool {
    // `find("")` answers `Some(0)` forever — an empty needle is not a rule
    // this can walk. The tool layer refuses it before it gets here
    // (`Workspace::usages`), and the predicate must still not loop on it.
    if symbol.is_empty() {
        return false;
    }
    let mut at = 0usize;
    while let Some(found) = line[at..].find(symbol) {
        let start = at + found;
        let end = start + symbol.len();
        let before = line[..start].chars().next_back();
        let after = line[end..].chars().next();
        let open = before.map_or(true, |ch| !word_char(ch));
        let close = after.map_or(true, |ch| !word_char(ch));
        if open && close {
            return true;
        }
        let step = line[start..].chars().next().map_or(1, char::len_utf8);
        at = start + step;
    }
    false
}

/// Whether `ch` can stand inside an identifier: the boundary's own alphabet.
/// The one home of the predicate ([`is_usage`]'s two checks), and the same rule
/// `outline::leading_word` reads a name's characters with.
fn word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Every row of one file's `text` that mentions `symbol` — the file's own
/// order, except that the declaration-looking rows come first (and keep their
/// order among themselves).
///
/// The whole list, with no room to bound it: [`rows_within`] is the reader the
/// walk uses, and this is that reader with `usize::MAX` room, for the callers
/// that want the rule's own answer rather than an answer's window (the sweeps
/// at the bottom of this file).
///
/// A file with no rows answers an empty `Vec`; "no rows here" is a fact the
/// caller pools with every other file's, not an error.
pub fn rows(symbol: &str, text: &str) -> Vec<Usage> {
    rows_within(symbol, text, usize::MAX)
}

/// The same rows, cut to `room` with the proof that there were more: at most
/// `room + 1` of them, and exactly `room + 1` when the file held a further
/// row. The caller's cut is then the walk's — `Workspace::usages` stops at the
/// row it cannot keep, and one row past the room is what proves there was one.
///
/// The bound is the point, and it is the answer's rather than the file's. A
/// minified or generated file can hold a row on every one of a million lines,
/// and building them all to show ten spends the file's own size in memory to
/// produce an answer ten rows long — the same trade `list_files` refuses
/// (finding IN9) and the walk's cap refuses between files. The scan still
/// visits every line, because a declaration on the *last* line of a file leads
/// its group and stopping early would lose the row the group exists for; what
/// is bounded is what is built, and the rows built are exactly the first
/// `room + 1` of the full list, definitions first and each half in line order.
pub fn rows_within(symbol: &str, text: &str, room: usize) -> Vec<Usage> {
    // One past the room: `room + 1` rows are the proof of "there is more",
    // and nothing beyond them can enter the answer the caller builds.
    let keep = room.saturating_add(1);
    let mut definitions: Vec<Usage> = Vec::new();
    let mut mentions: Vec<Usage> = Vec::new();
    for (index, line) in text::strip_bom(text).lines().enumerate() {
        // `keep` declarations have filled the window on their own, and
        // declarations lead it: nothing later can enter, so the scan is done.
        if definitions.len() == keep {
            break;
        }
        if !is_usage(line, symbol) {
            continue;
        }
        if is_declaration(line) {
            definitions.push(Usage {
                line: index + 1,
                text: row_text(line),
                definition: true,
            });
            // A declaration takes the window's front, so the row it pushed
            // out is the last mention — and the window is what this has to
            // carry, not the file.
            if definitions.len() + mentions.len() > keep {
                mentions.pop();
            }
        } else if definitions.len() + mentions.len() < keep {
            // A mention is kept only while it could still stand inside the
            // window; a later declaration pushes the window back, and the
            // mentions kept so far are exactly the ones it can reach.
            mentions.push(Usage {
                line: index + 1,
                text: row_text(line),
                definition: false,
            });
        }
    }
    definitions.append(&mut mentions);
    definitions
}

/// The row's own text for a line: the line itself, cut to [`ROW_WIDTH`] with
/// `…` where it was cut.
///
/// A declaration-looking line is cut by the outline's own rule
/// ([`crate::outline::row_text`]), which never cuts through the word that made
/// the line a declaration — an over-long `pub(in path)` row may overrun the
/// width for that, and the outline's own reasoning is the reason. Every other
/// line is cut on a character boundary at the width, with the cut said.
fn row_text(line: &str) -> String {
    if let Some(cut) = outline_row_text(line) {
        return cut;
    }
    if line.len() <= ROW_WIDTH {
        return line.to_string();
    }
    let cut = text::boundary_at_or_before(line, ROW_WIDTH);
    format!("{}…", &line[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// The rule, line by line: the boundary cases matter as much as the hits —
    /// a rule that matched every substring would answer "who uses this?" with
    /// every line holding the letters, which is what `search` already does.
    #[test]
    fn the_rule_reads_a_word_and_refuses_a_substring() {
        for line in [
            "held",
            "let held = 1;",
            "self.held()",
            "Type::held(&x)",
            "&held,",
            "(held)",
            "\"held\"",
            "a = held + 1;",
            "_x + held",
            "fn held(x: usize) -> usize { x }",
        ] {
            assert!(is_usage(line, "held"), "{line:?} mentions `held` as a word");
        }
        for line in [
            "",
            "beheld",
            "beheld_it",
            "aheld",
            "held_x",
            "held2",
            "heldx",
            "preheldpost",
            "_held",
            "fn held_it() {}",
            "// nothing to see",
        ] {
            assert!(
                !is_usage(line, "held"),
                "{line:?} does not hold `held` as a word"
            );
        }
        // One occurrence without a boundary does not hide a later one with it.
        assert!(is_usage("beheld, then held plainly", "held"));
        // The classic substring pair, on its own word: `hold` inside
        // `threshold` is the case the tool must not answer.
        assert!(!is_usage("let threshold = 3;", "hold"));
        assert!(is_usage("let hold = 3; // threshold too", "hold"));
        // An empty needle is not a rule that can walk; the door refuses it
        // (`Workspace::usages`), and the predicate answers `false`.
        assert!(!is_usage("anything", ""));
    }

    /// A `.` or a `::` is not a word character, so `method` is a hit on
    /// `self.method()` and on `Type::method` — the tool cannot tell a field
    /// from a local and says so by matching both. A *qualified* needle is a
    /// longer phrase, matched whole where it stands at boundaries: it narrows
    /// the answer instead of resolving it.
    #[test]
    fn a_namespaced_mention_is_a_word_hit_and_a_qualified_needle_is_a_phrase() {
        for line in ["self.method()", "Type::method(&x)", "let method = 1;"] {
            assert!(is_usage(line, "method"), "{line:?}");
        }
        for line in ["self.methodology()", "Type::method2()", "method_x"] {
            assert!(!is_usage(line, "method"), "{line:?}");
        }
        assert!(is_usage("let m = Type::method(x);", "Type::method"));
        assert!(is_usage("x.self.field = 1;", "self.field"));
        // The phrase must start and end at boundaries of its own: a longer
        // path in front is a different spelling, and a longer identifier
        // behind is a different name.
        assert!(!is_usage("let m = MyType::method(x);", "Type::method"));
        assert!(!is_usage("let m = Type::methodology(x);", "Type::method"));
        assert!(!is_usage("x.a_self.field = 1;", "self.field"));
    }

    /// The alphabet is Rust's, and one reading of it is worth naming: `$` and
    /// `#` are not word characters, so a JavaScript `$foo` is reported for
    /// `foo` — a false positive in that language — while a Rust `$name` is
    /// reported for `name`, which is the reading that keeps a macro
    /// metavariable findable. The tool cannot tell one language's identifier
    /// alphabet from another's, and the rule is written for the language the
    /// outline rule reads names in.
    #[test]
    fn a_dollar_prefix_is_a_boundary_because_a_metavariables_name_is_the_one_to_keep() {
        // JavaScript: `$foo` is an identifier of its own, and this rule cannot
        // see that. The row it reports is still honest — the line really does
        // hold `foo` at a boundary — and the module doc says the rule cannot
        // resolve the difference.
        assert!(is_usage("let $foo = 1;", "foo"));
        assert!(is_usage("$('x')", "$"));
        assert!(is_usage("$foo", "$foo"));
        // Rust: `$name` in a macro body spells the metavariable `name`, and a
        // rule that counted `$` as a word character would answer a miss for a
        // line that plainly holds it.
        assert!(is_usage("($name:expr) => { $name }", "name"));
        // `#` is a boundary for the same reason: a C directive's name is the
        // name, and `#` is not an identifier character in this rule's Rust.
        assert!(is_usage("#include <stdio.h>", "include"));
    }

    /// A needle of one non-word character is a word like any other: a hit
    /// needs non-word characters on *both* sides, so `-` stands alone in
    /// `a - b` and is part of `a-b`. Whitespace is no special case either —
    /// it is not the *empty* needle, which the door refuses — so `usages(" ")`
    /// walks, and a space is a hit only where neither neighbour is a word.
    #[test]
    fn a_one_character_needle_needs_boundaries_on_both_sides() {
        assert!(is_usage("a - b", "-"));
        assert!(!is_usage("a-b", "-"));
        assert!(is_usage("a . b", "."));
        assert!(!is_usage("a.b", "."));
        assert!(is_usage("x = $ y", "$"));
        assert!(!is_usage("x=$y", "$"));
        assert!(is_usage("a = = b", " "));
        assert!(!is_usage("a = b", " "));
        assert!(is_usage("\t\t", "\t"));
        // A letter outside ASCII is a word character like any other: `é`
        // needs boundaries exactly as `e` does.
        assert!(is_usage(" é ", "é"));
        assert!(!is_usage("café", "é"));
        // `_` is an identifier's own character, so it is not a boundary
        // either.
        assert!(is_usage("a _ b", "_"));
        assert!(!is_usage("a_b", "_"));
    }

    /// A needle that holds a line break can never be a row: a row is one
    /// line's text and no line holds a line break, so the rule answers
    /// `false` on every line. `Workspace::usages` refuses such a needle at its
    /// door rather than walking the tree to prove a miss it already knows. A
    /// carriage return is the opposite case — a lone one *is* a line's text —
    /// and is walked like any other character.
    #[test]
    fn a_needle_with_a_line_break_can_never_be_a_row() {
        assert!(!is_usage("held", "held\nheld"));
        assert!(rows("held\nheld", "held\nheld\n").is_empty());
        // The lone carriage return of a line that ends without a line feed is
        // a character of that line; the `\r` of a CRLF ending is not.
        assert!(is_usage("held\r", "held\r"));
        assert!(rows("held\r", "held\r\n").is_empty());
        assert_eq!(rows("held\r", "held\r").len(), 1);
    }

    /// A row is the *reader's* line, and the reader's line holds neither the
    /// BOM that signs a file nor the `\r` of a CRLF ending: the signature is
    /// not the first line's first character, and an ending is not a line's
    /// text. Both are still the file's bytes, and `search` and `read_file` are
    /// the roads that hand those over.
    #[test]
    fn a_leading_bom_and_a_crlf_ending_are_not_a_rows_text() {
        let found = rows("held", "\u{feff}fn held() {}\r\nlet a = held;\r\n");
        assert_eq!(
            found
                .iter()
                .map(|row| (row.line, row.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "fn held() {}"), (2, "let a = held;")],
            "the declaration first, and neither row holds the signature or the `\\r`"
        );
        assert!(
            found[0].definition,
            "`fn` after the signature is a declaration"
        );

        // Only the *leading* U+FEFF is a signature: one later in the text is a
        // zero-width no-break space, a character like any other, and the row is
        // that line — the signature's own question is `text::strip_bom`'s, and
        // the row keeps the character it found.
        let later = rows("held", "let a = held;\n\u{feff}fn held() {}\n");
        let second = later
            .iter()
            .find(|row| row.line == 2)
            .expect("line 2 is a row");
        assert_eq!(
            second.text, "\u{feff}fn held() {}",
            "a U+FEFF mid-file is text, not a signature"
        );

        // A lone carriage return is the line's text (`str::lines` drops only
        // the one that ends a CRLF), and a file whose last line has no line
        // feed still has that line.
        let lone = rows("held", "held\r\nheld\r");
        assert_eq!(
            lone.iter().map(|row| row.line).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(lone[1].text, "held\r");
    }

    /// A row is the line's own characters minus the ending: an escape
    /// sequence, a tab, a zero-width joiner and a combining mark all come back
    /// as the file holds them — the pane sanitizes its own copy — and an
    /// over-long line is cut on a character boundary with the cut said, so one
    /// minified line spends one row and not its length.
    #[test]
    fn a_row_keeps_the_lines_own_characters_and_cuts_by_the_width() {
        let control = "let held = \"\x1b[31m\";\t// held";
        assert_eq!(
            rows("held", &format!("{control}\n"))[0].text,
            control,
            "no sanitizing on a model road"
        );

        let zwj = "let held = \"\u{200d}\";";
        assert_eq!(rows("held", &format!("{zwj}\n"))[0].text, zwj);

        let marks = format!("held{}", "\u{0301}".repeat(1_000));
        let cut = rows("held", &format!("{marks}\n"));
        assert_eq!(cut.len(), 1);
        assert!(cut[0].text.ends_with('…'), "the cut is said");
        assert!(cut[0].text.len() <= ROW_WIDTH + '…'.len_utf8());
        assert!(marks.starts_with(cut[0].text.trim_end_matches('…')));

        let giant = format!("let held = \"{}\";", "x".repeat(200_000));
        let found = rows("held", &format!("{giant}\n"));
        assert_eq!(found.len(), 1, "a line is one row however long it is");
        assert!(found[0].text.ends_with('…'));
        assert!(found[0].text.len() <= ROW_WIDTH + '…'.len_utf8());
    }

    /// The bounded reader: `rows_within` builds one row past its room when the
    /// file held more and every row when it held less — the first `room + 1`
    /// rows of the full list, definitions first — so the walk can cut a
    /// million-line file to an answer ten rows long without building the
    /// million.
    #[test]
    fn a_bounded_row_list_carries_the_room_and_the_proof_of_one_more_row() {
        let text: String = (1..=1_000)
            .map(|n| format!("let a = held; // {n}\n"))
            .collect();
        let whole = rows("held", &text);
        assert_eq!(
            whole.len(),
            1_000,
            "the rule's own answer is the whole file"
        );

        assert_eq!(rows_within("held", &text, 10), whole[..11].to_vec());
        assert_eq!(rows_within("held", &text, 1).len(), 2);
        assert_eq!(
            rows_within("held", &text, 0).len(),
            1,
            "zero room proves one row"
        );
        // The room's own edge: a file holding exactly the room has no proof to
        // give, and one holding a single further row does.
        let three = "held\nheld\nheld\n";
        assert_eq!(rows_within("held", three, 3).len(), 3);
        assert_eq!(rows_within("held", three, 2).len(), 3);
        assert!(rows_within("held", "nothing here\n", 10).is_empty());
        assert_eq!(
            rows_within("held", &text, usize::MAX).len(),
            1_000,
            "no room to bound is the whole list"
        );

        // A declaration that arrives late still leads the window — the scan is
        // the file's, not the room's — and it takes a slot an earlier mention
        // would have held.
        let late = format!("{}fn held() {{}}\n", "a = held;\n".repeat(50));
        let window = rows_within("held", &late, 3);
        assert_eq!(
            window
                .iter()
                .map(|row| (row.line, row.definition))
                .collect::<Vec<_>>(),
            vec![(51, true), (1, false), (2, false), (3, false)],
            "the late declaration leads, and the window is the answer's size"
        );
    }

    /// The rows of one file: the declaration-looking line first (line order
    /// within the two halves), every row is its file's line, and the line
    /// number is the line's own 1-based number.
    #[test]
    fn a_files_rows_lead_with_the_definition_looking_line() {
        let text = "// held mentions\n\
                    let a = held;\n\
                    fn held(x: usize) -> usize { x }\n\
                    // fn held() {}\n\
                    let b = held + 1;\n\
                    pub struct Held;\n";
        let rows = rows("held", text);
        assert_eq!(
            rows.iter().map(|row| row.line).collect::<Vec<_>>(),
            vec![3, 1, 2, 4, 5],
            "the declaration first, then the mentions in the file's own order"
        );
        assert!(rows[0].definition, "line 3 is the declaration-looking row");
        assert!(!rows[1].definition && !rows[2].definition);
        assert_eq!(rows[0].text, "fn held(x: usize) -> usize { x }");
        // A comment line holds the word, so it is a row; the commented-out
        // `fn held() {}` is *not* a declaration — the outline rule refuses
        // comment lines, and this tool does not invent a second rule.
        assert_eq!(rows[1].text, "// held mentions");
        assert_eq!(rows[2].text, "let a = held;");
        assert_eq!(rows[3].text, "// fn held() {}");
        assert_eq!(rows[4].text, "let b = held + 1;");
        // The file's `pub struct Held` is a different word: case is part of
        // the spelling, and a hit is exact.
        assert!(
            rows.iter().all(|row| row.line != 6),
            "`Held` is not `held`: {rows:?}"
        );
    }

    /// More than one declaration-looking line in a file: every one of them
    /// leads, in the file's own line order, and `declarations()` is the list
    /// the group header's plural clause is built from — the two rows and the
    /// clause cannot disagree because there is one list.
    #[test]
    fn every_declaration_looking_row_leads_in_line_order() {
        let text = "let a = held;\nfn held() {}\nlet b = held;\npub struct held;\n";
        let found = rows("held", text);
        assert_eq!(
            found.iter().map(|row| row.line).collect::<Vec<_>>(),
            vec![2, 4, 1, 3],
            "both declarations lead, then the mentions"
        );
        let group = FileUsages {
            file: "f.rs".to_string(),
            rows: found,
        };
        assert_eq!(group.declarations(), vec![2, 4]);
    }

    /// A long line is cut at the outline's width with the cut said, and a
    /// declaration-looking one is cut by the outline's rule — never through the
    /// word that made it one. Nothing else about the line changes.
    #[test]
    fn a_row_is_the_line_cut_and_never_the_rule() {
        let body = format!("let held = \"{}\";", "x".repeat(2 * ROW_WIDTH));
        let found = rows("held", &body);
        assert_eq!(found.len(), 1);
        assert!(found[0].text.ends_with('…'), "{:?}", found[0].text);
        assert!(found[0].text.len() <= ROW_WIDTH + '…'.len_utf8());
        assert!(body.starts_with(found[0].text.trim_end_matches('…')));
        assert!(!found[0].definition);

        // The over-long qualifier: the outline's cut keeps the declaration
        // word whole (its own row invariant), and the row is still this line.
        let long = format!(
            "pub(in {}) fn held() {{}}",
            "crate::some::deeply::nested::module::path::that::keeps::going::and::going::further::\
             still::going"
        );
        let second = rows("held", &long);
        assert_eq!(second.len(), 1);
        assert!(second[0].definition);
        assert!(second[0].text.ends_with('…'), "{:?}", second[0].text);
        assert!(long.starts_with(second[0].text.trim_end_matches('…')));
        assert!(is_declaration(&second[0].text));
    }

    /// The property the brief calls the hard invariant, over this checkout:
    /// every `.rs` file under `crates/` is walked for a handful of symbols this
    /// repository really spells, and every row is checked against the file's
    /// own line — the line the row names holds the symbol at a boundary, the
    /// row's text is that line (cut), the declaration flag is the outline
    /// rule's answer, and the rows of each half ascend.
    ///
    /// The sweep keeps the reading and no wall-clock bound: the checkout grows,
    /// so a stopwatch on it records the box and not the code, and the complexity
    /// claim is [`a_usage_walk_scales_with_its_text_and_not_its_square`]'s — the
    /// rule behind both is the doc comment of `outline.rs`'s
    /// `a_million_declarations_are_counted_and_not_kept`.
    #[test]
    fn every_row_in_this_checkout_names_a_line_that_holds_the_word() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("the crate sits under <repo>/crates/mush-core");
        let mut files = Vec::new();
        rust_files(&root.join("crates"), &mut files);
        files.sort();
        assert!(files.len() > 30, "the sweep found {} files", files.len());

        let started = Instant::now();
        let mut counted = 0usize;
        for symbol in ["Workspace", "usages", "is_declaration", "held", "fn"] {
            for path in &files {
                let text =
                    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                let found = rows(symbol, &text);
                let lines: Vec<&str> = text.lines().collect();
                let mut last_definition = 0usize;
                let mut last_mention = 0usize;
                let mut seen_mention = false;
                for row in &found {
                    counted += 1;
                    let line = lines[row.line - 1];
                    assert!(
                        is_usage(line, symbol),
                        "{}:{} does not hold `{symbol}` at a boundary: {line:?}",
                        path.display(),
                        row.line
                    );
                    assert_eq!(
                        row.definition,
                        is_declaration(line),
                        "{}:{}: the declaration flag is the outline rule's",
                        path.display(),
                        row.line
                    );
                    let body = row.text.strip_suffix('…').unwrap_or(&row.text);
                    assert!(
                        line.starts_with(body),
                        "{}:{}: the row is not that line, cut: {:?}",
                        path.display(),
                        row.line,
                        row.text
                    );
                    if row.definition {
                        assert!(
                            !seen_mention,
                            "{}:{}: a declaration row after a plain mention — the two halves \
                             are not the order this promises",
                            path.display(),
                            row.line
                        );
                        assert!(
                            row.line > last_definition,
                            "{}:{}: declaration rows must ascend",
                            path.display(),
                            row.line
                        );
                        last_definition = row.line;
                    } else {
                        seen_mention = true;
                        assert!(
                            row.line > last_mention,
                            "{}:{}: mention rows must ascend",
                            path.display(),
                            row.line
                        );
                        last_mention = row.line;
                    }
                }
            }
        }
        let elapsed = started.elapsed();
        // The number, not only the bound: a sweep whose cost moves is worth
        // seeing in a `--nocapture` run, the same reading the outline sweep
        // gets.
        eprintln!(
            "usages sweep: {} files, {counted} rows over 5 symbols, {elapsed:?}",
            files.len()
        );
        assert!(counted > 500, "the sweep found only {counted} rows");
    }

    /// The sweep above under shape 1 of the rule in `outline.rs`'s
    /// `a_million_declarations_are_counted_and_not_kept`, which is why that
    /// sweep keeps no wall-clock bound. Generated text at 1,000 and 4,000
    /// lines with the five symbols sprinkled through it as declarations and as
    /// plain mentions, and the sweep's own checks over every row `rows`
    /// answers; nine readings per size, interleaved small, large, small, large,
    /// …, fastest kept. Both sizes are small on purpose: this guard runs
    /// beside the rest of the suite, and a descheduled reading is the longer
    /// text's tail — at 5,000 and 20,000 lines with five readings the suite's
    /// own load once read 8.28×, because no clean two-hundred-millisecond
    /// window was left to find. Linear is four times the lines (4×), quadratic
    /// is its square (16×), and six is the factor a deschedule cannot
    /// manufacture.
    #[test]
    fn a_usage_walk_scales_with_its_text_and_not_its_square() {
        let symbols = ["Workspace", "usages", "is_declaration", "held", "fn"];
        let small = usage_corpus(1_000);
        let large = usage_corpus(4_000);

        let mut fastest = [Duration::MAX; 2];
        let mut counted = [0usize; 2];
        for _ in 0..9 {
            for (at, text) in [(0usize, &small), (1usize, &large)] {
                let started = Instant::now();
                let walked = usage_walk(&symbols, text);
                let elapsed = started.elapsed();
                counted[at] = walked;
                fastest[at] = fastest[at].min(elapsed);
            }
        }
        // The cheap property at every timed repeat: the walk really walked,
        // and four times the lines is four times the rows, so the ratio cannot
        // come from measuring nothing.
        assert!(counted[0] > 0, "the walk found no rows to check");
        assert_eq!(
            counted[1],
            counted[0] * 4,
            "four times the lines is four times the rows"
        );
        eprintln!(
            "usages scale: 1,000 lines in {:?}, 4,000 lines in {:?} ({:.2}×), {} rows",
            fastest[0],
            fastest[1],
            fastest[1].as_secs_f64() / fastest[0].as_secs_f64(),
            counted[1]
        );
        assert!(
            fastest[1] <= fastest[0] * 6,
            "four times the lines cost {:?} against {:?} — linear is 4× and quadratic is 16×",
            fastest[1],
            fastest[0]
        );
    }

    /// The text the scaling guard walks: `lines` lines, the five symbols
    /// sprinkled through it as declarations and as plain mentions — the
    /// pattern's length divides both sizes, so the four-times text holds
    /// exactly four times the rows — plus lines holding none of them.
    fn usage_corpus(lines: usize) -> String {
        let mut text = String::new();
        for n in 0..lines {
            match n % 8 {
                0 => text.push_str("fn held() {}\n"),
                1 => text.push_str("let usages = Workspace::default();\n"),
                2 => text.push_str("if is_declaration(line) { held(); }\n"),
                3 => text.push_str("let held = usages;\n"),
                4 => text.push_str("// held and is_declaration in a comment\n"),
                5 => text.push_str("struct Workspace;\n"),
                6 => text.push_str("fn usages_row() {}\n"),
                _ => text.push('\n'),
            }
        }
        text
    }

    /// One reading of the scaling guard's walk, the checkout sweep's own shape:
    /// [`rows`] for each symbol over `text`, then the sweep's per-row checks —
    /// the named line holds the word at a boundary, the declaration flag is the
    /// outline rule's, the row's text is that line cut, and each half's rows
    /// ascend — with the row count answered so a guard can assert the walk
    /// really walked.
    fn usage_walk(symbols: &[&str], text: &str) -> usize {
        let lines: Vec<&str> = text.lines().collect();
        let mut counted = 0usize;
        for symbol in symbols {
            let mut last_definition = 0usize;
            let mut last_mention = 0usize;
            let mut seen_mention = false;
            for row in rows(symbol, text) {
                counted += 1;
                let line = lines[row.line - 1];
                assert!(
                    is_usage(line, symbol),
                    "{} does not hold `{symbol}` at a boundary: {line:?}",
                    row.line
                );
                assert_eq!(
                    row.definition,
                    is_declaration(line),
                    "{}: the declaration flag is the outline rule's",
                    row.line
                );
                let body = row.text.strip_suffix('…').unwrap_or(&row.text);
                assert!(
                    line.starts_with(body),
                    "{}: the row is not that line, cut: {:?}",
                    row.line,
                    row.text
                );
                if row.definition {
                    assert!(
                        !seen_mention,
                        "{}: a declaration row after a plain mention",
                        row.line
                    );
                    assert!(
                        row.line > last_definition,
                        "{}: declaration rows must ascend",
                        row.line
                    );
                    last_definition = row.line;
                } else {
                    seen_mention = true;
                    assert!(
                        row.line > last_mention,
                        "{}: mention rows must ascend",
                        row.line
                    );
                    last_mention = row.line;
                }
            }
        }
        counted
    }

    /// The substring half of the invariant, as a second implementation: every
    /// line of every `.rs` file in this checkout is checked with a
    /// character-walking rule written the slow way, and the two must agree on
    /// every line. This is what makes "a substring-only hit is not a row" a
    /// property of the repo rather than an example: a line holding only
    /// `beheld` or `held_x` answers `false` under both rules, and one holding
    /// `self.held` answers `true` under both.
    #[test]
    fn the_rule_agrees_with_a_character_walk_on_every_line_of_this_checkout() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("the crate sits under <repo>/crates/mush-core");
        let mut files = Vec::new();
        rust_files(&root.join("crates"), &mut files);
        files.sort();
        assert!(files.len() > 30, "the sweep found {} files", files.len());

        let mut lines = 0usize;
        // The last two are the needles no word rule can be assumed to handle:
        // a single non-word character and a character no identifier opens
        // with. Whatever the two implementations answer there, they must
        // answer the same thing on every line of this checkout.
        for symbol in ["held", "usages", "Workspace", "search", "fn", "-", "$"] {
            for path in &files {
                let text =
                    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                for line in text.lines() {
                    lines += 1;
                    assert_eq!(
                        is_usage(line, symbol),
                        by_walk(line, symbol),
                        "{}: the two rules disagree about `{symbol}`: {line:?}",
                        path.display()
                    );
                }
            }
        }
        assert!(lines > 10_000, "the sweep read only {lines} lines");
    }

    /// The rule again, written the slow way — per *character*, with the whole
    /// needle matched at each position — so the two implementations do not
    /// share the bug a copy would. Test-only: the fast one is [`is_usage`].
    fn by_walk(line: &str, symbol: &str) -> bool {
        if symbol.is_empty() {
            return false;
        }
        let chars: Vec<char> = line.chars().collect();
        let needle: Vec<char> = symbol.chars().collect();
        (0..chars.len()).any(|at| {
            chars[at..].starts_with(&needle)
                && (at == 0 || !word_char(chars[at - 1]))
                && chars
                    .get(at + needle.len())
                    .map_or(true, |after| !word_char(*after))
        })
    }

    /// Every `.rs` file under `dir`, depth-first, in name order — the outline
    /// sweep's own walker, a handful of lines rather than a dependency.
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|e| e.path())
            .collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
}
