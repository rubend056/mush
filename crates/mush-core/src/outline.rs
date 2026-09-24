//! The `outline` tool's text: what one file declares, one row per line.
//!
//! A model that wants a file's shape should not have to read the file to find
//! it. `outline(path)` answers with one row per declaration — the line number,
//! then that line, cut to a width — so the shape costs a screen instead of a
//! window, and every row doubles as the anchor a `read_file {offset}` or an
//! `edit_file {old_string}` is built from. The same rendering is what an
//! unbounded read of a file past the result cap answers with
//! ([`Workspace::outline`](crate::workspace::Workspace::outline), and
//! `read_tool` in the app), because a capped head of 40 lines tells a model
//! nothing about the 900 it did not see.
//!
//! **Textual, Rust-first, best-effort — and every answer says so out loud.**
//! This is not a parser and must never pretend to be one. It reads *lines*: an
//! item a macro generates is invisible (there is no expansion here to see it
//! in), a `fn` spelled inside a string or a comment body that is not itself a
//! comment line can be a row, and a declaration split across lines is only ever
//! its first line. The header [`Outline::render`] opens with carries that
//! sentence on every answer, because a model that trusts an outline further
//! than the rule reaches is a model mush misled — the same reason `search`
//! says "no regex" and `read_file` names the lines a window left.
//!
//! **The rule, exactly.** A line is a declaration when, after its leading
//! whitespace, it opens with
//!
//! - a declaration keyword — `fn`, `struct`, `enum`, `trait`, `impl`, `const`,
//!   `static`, `type`, `mod`, `union`, `macro`, and `macro_rules!` — followed
//!   by a *name*: an identifier's first character (a letter or `_`), or `<`
//!   for `impl<T>`, the one keyword a generic list may touch without a space;
//!   or
//! - any run of qualifiers first: `pub` (with its `(crate)`/`(in path)` group),
//!   `async`, `unsafe`, `default`, `auto`, and `extern` with its optional ABI
//!   string. `const` is the delicate one: before `fn` (or a further qualifier
//!   of one) it qualifies the function, and anywhere else it *is* the
//!   declaration — that is what tells `const fn f` from `const N: usize`.
//!
//! A line whose trimmed form is a comment — `//`, `///`, `//!`, `*` — is never
//! a declaration, which is what keeps a doc comment from making a phantom row;
//! a block comment whose body lines are not `*`-prefixed still shows through,
//! which is the textual rule's declared cost and not a hidden one.
//!
//! `impl` blocks are not collapsed and their items are not hidden: a collapsed
//! `impl Foo { … }` row would stand for lines it does not hold, i.e. exactly
//! the lie the invariant below forbids, and the methods inside are the anchors
//! a reader wants most. What keeps a block from drowning the outline is that a
//! row *is* the line — nothing is summarized, nothing is grouped — so the
//! header's count and the result cap are the only bounds, and a file whose
//! bulk is one `impl` is a file whose shape is its methods.
//!
//! **The invariant: a row may never lie about a line.** Whatever the rule
//! matched on the file's line, the row's own text still matches it — a cut row
//! is cut *after* the word that made it a declaration, never through it
//! ([`row_text`]), so re-running the rule over the row always finds the
//! declaration the row claims. The sweep at the bottom of this file re-matches
//! every row of every `.rs` file in this checkout against the rule it came
//! from; that property is the one thing about a textual sketch that can be a
//! hard guarantee.

use crate::text;
use crate::workspace::{truncate_for_model, CRLF_NOTE};

/// The bytes a row's text is cut to before `…` marks the cut.
///
/// Sized to what a model needs from one line rather than to a pane: a full
/// `pub fn digest(name: ToolName, args: &Value, result: Option<&str>, root:
/// &Path) -> CallFacts {` fits, and the rows of a long file stay scannable.
/// Bytes, not columns, like every other model road's cut
/// ([`crate::workspace::truncate_for_model`], `match_line` of `search`): a
/// result is data, and a column is a pane's unit.
pub const ROW_WIDTH: usize = 100;

/// The sentence every header carries: the tool's own confession, in the same
/// breath as the counts, so no reader can miss what kind of answer this is.
const RULE_NOTE: &str = "textual, Rust-first — not a compiler's answer";

/// The bytes [`Outline::render`] keeps free for the closing note that says how
/// many rows the result cap cut: two counts, spelled with at most twenty digits
/// each. The note itself is written once the loop knows `shown`.
const CUT_RESERVE: usize = 160;

/// One declaration: the file's 1-based line number and the row's own text.
///
/// The text is the line itself — leading whitespace and all — cut to
/// [`ROW_WIDTH`] with `…` where it was cut, and never cut through the word that
/// made it a declaration (see [`row_text`]). The number is the line's own, so
/// the pair is the anchor: `read_file {offset: line}` reads from it, and the
/// text (when it was not cut) matches the line `edit_file` would replace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    pub line: usize,
    pub text: String,
}

/// The outline of one file: the rows the rule found, the file's own line count,
/// and whether its lines end with CRLF (the fact a window read reports too, so
/// one road's rows and the other's text tell one story about the endings).
#[derive(Clone, Debug)]
pub struct Outline {
    path: String,
    lines: usize,
    crlf: bool,
    definitions: Vec<Definition>,
}

impl Outline {
    /// The outline of `text`, a file the caller has already read (the workspace
    /// road is [`Workspace::outline`](crate::workspace::Workspace::outline),
    /// which owns the read caps and the lossy decode). `path` is the name the
    /// model used, shown back in the header exactly as it was asked about.
    pub fn of(path: &str, text: &str) -> Self {
        Self {
            path: path.trim().to_string(),
            lines: text.lines().count(),
            crlf: text::is_crlf(text),
            definitions: definitions(text),
        }
    }

    /// Nothing to sketch: the answer is a sentence, not a header with no rows.
    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }

    /// The rows, in the file's own order.
    pub fn definitions(&self) -> &[Definition] {
        &self.definitions
    }

    /// The line the header opens with: the file, its length, how many
    /// definitions the rule found, and the rule's own confession.
    pub fn header(&self) -> String {
        format!(
            "{} — {}; {} ({RULE_NOTE})",
            self.path,
            count(self.lines, "line"),
            count(self.definitions.len(), "definition"),
        )
    }

    /// The whole answer, within `cap` bytes: the header, `lead` under it when
    /// the caller has a sentence of its own (the unbounded-read fallback says
    /// there what the model is not being shown), a blank, then the rows — as
    /// many as the cap pays for — and the closing notes.
    ///
    /// The notes are why the row loop reserves room before it starts: a result
    /// whose last line is a half-written row and whose "the rest was cut" note
    /// fell off the end says nothing about what it left. The CRLF note is
    /// `CRLF_NOTE`, the same sentence [`crate::workspace::Workspace::read_window`]
    /// appends, from one home, because a row copied out of a CRLF file into an
    /// `edit_file` crosses the same line-ending rule a window copied out of one
    /// does.
    pub fn render(&self, cap: usize, lead: &str) -> String {
        if self.definitions.is_empty() {
            return truncate_for_model(self.empty_answer(), cap);
        }
        let mut out = self.header();
        if !lead.is_empty() {
            out.push('\n');
            out.push_str(lead);
        }
        out.push_str("\n\n");
        let crlf = if self.crlf {
            format!("\n{CRLF_NOTE}")
        } else {
            String::new()
        };
        let budget = cap.saturating_sub(crlf.len() + CUT_RESERVE);
        if out.len() > budget {
            // A cap too small for even the header is a cap too small for any
            // answer; the header is the answer, and the cut says so.
            return truncate_for_model(out, cap);
        }
        let mut shown = 0usize;
        let mut cut = false;
        for definition in &self.definitions {
            let row = format!("  {}  {}\n", definition.line, definition.text);
            if out.len() + row.len() > budget {
                cut = true;
                break;
            }
            out.push_str(&row);
            shown += 1;
        }
        if out.ends_with('\n') {
            out.pop();
        }
        if cut {
            out.push_str(&format!(
                "\n[mush: only the first {shown} of {} definitions are shown — read_file \
                 {{offset, limit}} shows a range around any row's line]",
                self.definitions.len()
            ));
        }
        out.push_str(&crlf);
        truncate_for_model(out, cap)
    }

    /// The answer for a file the rule found nothing in.
    ///
    /// "No definitions" is a normal fact, not a failure — a markdown file, a
    /// config, a Rust file of pure plumbing all answer it — so it is an `Ok`
    /// sentence that names the road that still shows the text, never a refusal
    /// a model reads as "the file is unreadable". An empty file gets the same
    /// sentence a window read gives it (`{path} is empty`) with the fact that
    /// there was nothing to sketch, rather than a header claiming `0 lines; 0
    /// definitions`.
    fn empty_answer(&self) -> String {
        if self.lines == 0 {
            format!(
                "{} is empty — there are no definitions to outline",
                self.path
            )
        } else {
            format!(
                "{} — {}; no definitions ({RULE_NOTE}); read_file shows the text",
                self.path,
                count(self.lines, "line")
            )
        }
    }
}

/// Every declaration row of `text`, ascending by line, one per line, no
/// duplicates (the walk is the file's own order and the rule is one answer per
/// line, so neither can happen — the sweep test asserts both anyway, because
/// a later rule that looks back or forward could break them silently).
pub fn definitions(text: &str) -> Vec<Definition> {
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            row_text(line).map(|text| Definition {
                line: index + 1,
                text,
            })
        })
        .collect()
}

/// Whether a line is a declaration by this module's textual rule — the whole
/// rule in one predicate, with the reasoning in `declaration_prefix`.
pub fn is_declaration(line: &str) -> bool {
    declaration_prefix(line).is_some()
}

/// The row's own text for a line, or `None` when the line is not a declaration.
///
/// The line comes back whole unless it is longer than [`ROW_WIDTH`], and a cut
/// line ends in `…`. The cut may overrun the width on one shape of line: when
/// the word that makes the line a declaration sits behind more than
/// [`ROW_WIDTH`] bytes of qualifiers (`pub(in a::very::long::module::path)`),
/// the cut is taken after that word rather than through it. A row cut through
/// its own keyword would re-match the rule as *not* a declaration — the row
/// would lie about the line it names — and the invariant that it cannot is
/// worth a rare over-long row.
pub fn row_text(line: &str) -> Option<String> {
    let decisive = declaration_prefix(line)?;
    let width = ROW_WIDTH.max(decisive);
    if line.len() <= width {
        return Some(line.to_string());
    }
    let at = text::boundary_at_or_before(line, width);
    Some(format!("{}…", &line[..at]))
}

/// The words that are a declaration on their own, with `macro_rules!` handled
/// beside this list (its `!` is part of the spelling).
const KEYWORDS: &[&str] = &[
    "fn", "struct", "enum", "trait", "impl", "const", "static", "type", "mod", "union", "macro",
];

/// The words that may stand before a declaration keyword without changing what
/// the line declares. `pub`, `extern` and `const` need arm of their own in
/// [`declaration_prefix`]: the first two may carry a group, the third may be
/// the declaration itself.
const MODIFIERS: &[&str] = &["async", "unsafe", "default", "auto"];

/// How many leading qualifiers the walk will strip before it gives up: enough
/// for `pub(in path) const unsafe extern "C" async fn` with room to spare, and
/// a bound, because a line is not an invitation to loop.
const MAX_QUALIFIERS: usize = 8;

/// The byte offset just past the word that makes `line` a declaration — and
/// past the first byte of the name that word declares — or `None` when nothing
/// does. It is deliberately *one past the name's first byte* and not merely
/// one past the keyword: [`row_text`] cuts a row no earlier than this offset,
/// so the cut can never leave a row that no longer re-matches the rule (a
/// trailing `fn…` has no name, and the rule would refuse it).
///
/// This is the one rule, and everything else here is its reader: `is_declaration`
/// asks it yes/no, `row_text` uses its offset as the earliest honest cut, and
/// the header's count is how many lines answered `Some`. It is deliberately a
/// scan from the left and not a search: a `fn` in the middle of a line is
/// somebody's call, a closure's signature or a trait bound, never the line's
/// own declaration, so a declaration is only ever a line whose *opening* is
/// one.
///
/// `extern "C"`'s ABI string is skipped because `"C"` is not a word and the
/// next word is the keyword; `pub(in …)`'s group is skipped to its matching
/// `)`; `const` looks one word ahead because `const fn` and `const ITEM` are
/// the two shapes the same word opens. An unreadable shape (an unbalanced
/// group, an unterminated string) is simply not a declaration: the rule may
/// miss, but it may not guess.
fn declaration_prefix(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    // A comment is never a declaration. `//`, `///` and `//!` are one check;
    // `*` is the middle of a block comment and the end of one. The whole point
    // is doc comments: they are most of a Rust file's lines and every one of
    // them may open with a word that looks like a declaration.
    if trimmed.starts_with("//") || trimmed.starts_with('*') {
        return None;
    }
    let base = line.len() - trimmed.len();
    let mut rest = trimmed;
    for _ in 0..MAX_QUALIFIERS {
        let (word, after) = leading_word(rest);
        match word {
            // `pub`, and the `(crate)` / `(super)` / `(in path)` group it may
            // carry. The group is skipped by its matching `)`, not by the next
            // space, because the path inside may hold spaces.
            "pub" => rest = skip_pub(rest)?,
            // `extern "C"` / `extern "system"`: the string is part of the
            // qualifier, and the keyword follows it.
            "extern" => rest = skip_extern(rest),
            "const" => {
                let next = leading_word(rest[after..].trim_start()).0;
                if matches!(next, "fn" | "unsafe" | "async" | "extern" | "impl") {
                    rest = rest[after..].trim_start();
                } else {
                    // `const` is not qualifying a function: it is the
                    // declaration itself (`const N: usize = 1;`). Leave it to
                    // the shared tail below, so the witness includes the
                    // name and a cut row still re-matches.
                    break;
                }
            }
            _ if MODIFIERS.contains(&word) => rest = rest[after..].trim_start(),
            _ => break,
        }
    }
    let (word, after) = leading_word(rest);
    let start = base + (trimmed.len() - rest.len());
    let tail = rest[after..].trim_start();
    let skipped = rest[after..].len() - tail.len();
    if word == "macro_rules" && rest[after..].starts_with('!') {
        return Some(start + after + 1);
    }
    // The word has to be followed by a name, not by `=`, `:` or `;`: this is
    // what keeps `fnord()`, `types`, a TOML `type = "lib"` and a YAML
    // `type: string` out of an outline.
    let first = tail.chars().next()?;
    let named = first.is_alphabetic() || first == '_' || first == '<';
    (KEYWORDS.contains(&word) && named).then_some(start + after + skipped + first.len_utf8())
}

/// The identifier at the front of `text`, and where it ends. Non-ASCII letters
/// are identifier bytes to Rust, so they are identifier bytes here too: the
/// rule's keywords are ASCII, and a Unicode name beside one changes nothing.
fn leading_word(text: &str) -> (&str, usize) {
    let end = text
        .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .unwrap_or(text.len());
    (&text[..end], end)
}

/// `rest` past its `pub` and past the visibility group it may carry. `None`
/// when the group never closes — an unreadable line is not a declaration
/// rather than a guess.
fn skip_pub(rest: &str) -> Option<&str> {
    let rest = rest["pub".len()..].trim_start();
    if !rest.starts_with('(') {
        return Some(rest);
    }
    let mut depth = 0usize;
    for (at, ch) in rest.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest[at + 1..].trim_start());
                }
            }
            _ => {}
        }
    }
    None
}

/// `rest` past its `extern` and past the ABI string when one is there.
fn skip_extern(rest: &str) -> &str {
    let rest = rest["extern".len()..].trim_start();
    let Some(body) = rest.strip_prefix('"') else {
        return rest;
    };
    match body.find('"') {
        Some(end) => body[end + 1..].trim_start(),
        // An unterminated ABI string: the line is not readable as a
        // declaration, and what follows a broken string is not a keyword this
        // rule should read.
        None => "",
    }
}

/// `4,120` — a count with thousands commas, the header's own arithmetic.
/// Hand-rolled because the dependency budget has no locale crate and one
/// separator needs no locale.
fn commas(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, ch) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `1 line` / `4,120 lines`: one count, spelled so a single one never reads
/// `1 lines` — the kind of small wrongness a model copies into its summary.
fn count(n: usize, noun: &str) -> String {
    let plural = if n == 1 { "" } else { "s" };
    format!("{} {noun}{plural}", commas(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// The rule, line by line: what it reads as a declaration and what it
    /// refuses. The refusals matter as much as the rows — a rule that matched
    /// every line with the letters `fn` in it would drown an outline in calls,
    /// closures and prose.
    #[test]
    fn the_rule_reads_declarations_and_nothing_else() {
        for line in [
            "fn main() {",
            "pub fn digest(name: ToolName) -> CallFacts {",
            "pub(crate) fn hidden() {}",
            "pub(in crate::app) fn scoped() {}",
            "async fn fetch() {}",
            "pub async unsafe fn both() {}",
            "const fn small() -> usize {",
            "unsafe extern \"C\" fn ffi() {",
            "pub extern \"C\" fn abi() {}",
            "const N: usize = 4;",
            "    const LIMIT: usize = 10;",
            "static ALIVE: bool = true;",
            "static mut COUNTER: u32 = 0;",
            "type Alias = Vec<u8>;",
            "pub type Pair<T> = (T, T);",
            "mod tests {",
            "struct CallFacts {",
            "enum Tone {",
            "union Bits {",
            "trait Model {",
            "impl CallFacts {",
            "impl<T> From<T> for Wrapper<T> {",
            "unsafe impl Send for Thing {}",
            "macro_rules! shout {",
            "macro loud {",
            "    fn nested() {}",
        ] {
            assert!(is_declaration(line), "{line:?} is a declaration");
        }
        for line in [
            "",
            "    ",
            "// fn commented() {}",
            "/// fn documented() {}",
            "//! fn inner_doc() {}",
            " * fn block_comment() {}",
            "*/",
            "/* fn one_line_block() {} */",
            "#[derive(Debug)]",
            "fnord()",
            "types",
            "let fn_pointer = 1;",
            "return Thing::default();",
            "type: string",
            "type = \"lib\"",
            "fn = 3",
            "mod = 3",
            "//! A doc comment that says `pub fn` in prose",
            "let s = \"fn not_a_line() {}\";",
            "impl_thing()",
            "constituent",
        ] {
            assert!(!is_declaration(line), "{line:?} is not a declaration");
        }
    }

    /// The invariant, on the shapes that could break it: a cut row is still a
    /// declaration, and it is still a *prefix* of the file's line rather than a
    /// rearranged one. The over-long qualifier is the case that forces the cut
    /// past [`ROW_WIDTH`] — a row cut through `fn` would re-match as nothing.
    #[test]
    fn a_row_is_the_line_cut_after_the_word_that_made_it_one() {
        let long = format!(
            "pub(in {}) fn hidden() {{}}",
            "crate::some::deeply::nested::module::path::that::keeps::going::and::going::further::\
             still::going"
        );
        assert!(is_declaration(&long));
        let row = row_text(&long).unwrap();
        assert!(row.ends_with('…'), "a cut row says it was cut: {row:?}");
        assert!(
            row.len() >= ROW_WIDTH,
            "the cut went past the width rather than through the keyword: {row:?}"
        );
        assert!(
            long.starts_with(row.trim_end_matches('…')),
            "the row is that line, cut: {row:?}"
        );
        assert!(
            is_declaration(&row),
            "a cut row may never stop being the declaration it claims: {row:?}"
        );

        // The ordinary cut: a long body, cut at the width, still a declaration.
        let body = "pub fn digest(name: ToolName, args: &Value, result: Option<&str>, root: &Path, extra: &mut Vec<Definition>) -> CallFacts {";
        let row = row_text(body).unwrap();
        assert!(row.ends_with('…'));
        assert!(row.len() <= ROW_WIDTH + '…'.len_utf8());
        assert!(is_declaration(&row), "{row:?}");

        // A short line comes back exactly, and a comment comes back `None`.
        assert_eq!(row_text("fn main() {").unwrap(), "fn main() {");
        assert_eq!(row_text("    /// fn doc() {}"), None);
    }

    /// The rows are the file's own order: one per line, ascending, no
    /// duplicates, and the line numbers are the lines the rows describe.
    #[test]
    fn the_rows_are_the_lines_they_name() {
        let text = "// a heading\n\nfn first() {\n    // fn hidden() {}\n    let x = 1;\n}\n\npub struct Second;\n";
        let found = definitions(text);
        assert_eq!(
            found,
            vec![
                Definition {
                    line: 3,
                    text: "fn first() {".to_string()
                },
                Definition {
                    line: 8,
                    text: "pub struct Second;".to_string()
                },
            ]
        );
        let lines: Vec<&str> = text.lines().collect();
        for definition in &found {
            assert!(lines[definition.line - 1].starts_with(&definition.text));
        }
    }

    /// The empty answers: an empty file, and a file the rule found nothing in.
    /// Both are `Ok` sentences that still name a road — a model that reads "no
    /// definitions" as "nothing to see" has been told about `read_file`.
    #[test]
    fn nothing_to_outline_still_answers() {
        let empty = Outline::of("empty.rs", "");
        assert!(empty.is_empty());
        assert_eq!(
            empty.render(4_000, ""),
            "empty.rs is empty — there are no definitions to outline"
        );

        let prose = Outline::of("NOTES.md", "# Notes\n\nThe rule is textual.\n");
        assert!(prose.is_empty());
        assert_eq!(
            prose.render(4_000, ""),
            "NOTES.md — 3 lines; no definitions (textual, Rust-first — not a compiler's \
             answer); read_file shows the text"
        );
    }

    /// The header is the fixed sample: the commas, the singular spelled, the
    /// confession present. A model reads this line to decide whether to trust
    /// the rows under it, so its shape is a contract.
    #[test]
    fn the_header_names_the_file_the_count_and_the_rule() {
        let text: String = (1..=120).map(|n| format!("fn item_{n}() {{}}\n")).collect();
        let outline = Outline::of("src/big.rs", &text);
        assert_eq!(outline.definitions().len(), 120);
        assert_eq!(
            outline.header(),
            "src/big.rs — 120 lines; 120 definitions (textual, Rust-first — not a compiler's \
             answer)"
        );
        let one = Outline::of("one.rs", "fn only() {}\n");
        assert_eq!(
            one.header(),
            "one.rs — 1 line; 1 definition (textual, Rust-first — not a compiler's answer)"
        );
        // The thousands comma is the header's own claim, so it is pinned here:
        // a 4,120-line file must not read `4120 lines`.
        let many: String = (1..=4_120).map(|_| "\n").collect();
        let outline = Outline::of("huge.rs", &many);
        assert!(
            outline
                .header()
                .starts_with("huge.rs — 4,120 lines; 0 definitions"),
            "{}",
            outline.header()
        );
    }

    /// `render` keeps its road: the lead sentence survives under the header, the
    /// rows come after a blank, and a cap that cannot hold every row says how
    /// many it did hold rather than stopping mid-row.
    #[test]
    fn a_capped_outline_says_what_it_left() {
        let text: String = (1..=200)
            .map(|n| format!("fn item_{n}() {{ let body = \"{}\"; }}\n", "x".repeat(80)))
            .collect();
        let outline = Outline::of("src/big.rs", &text);
        assert!(!outline.is_empty());
        let whole = outline.render(100_000, "");
        assert!(whole.starts_with("src/big.rs — 200 lines; 200 definitions"));
        assert!(whole.contains("\n  3  fn item_3()"));

        let lead = "[mush: the file's text was not shown — read a range with offset/limit]";
        let capped = outline.render(2_000, lead);
        assert!(
            capped.contains(lead),
            "the road survives the cap: {capped:?}"
        );
        assert!(
            capped.contains("[mush: only the first "),
            "the cut is named: {capped:?}"
        );
        assert!(!capped.contains("fn item_199"), "the rows really were cut");
        assert!(
            capped.len() <= 2_000,
            "the answer fits the cap it was given: {}",
            capped.len()
        );
        // Nothing in it is a half-row: every row line ends whole.
        for line in capped.lines().filter(|line| line.starts_with("  ")) {
            assert!(
                line.starts_with("  ") && !line.ends_with("fn item_"),
                "{line:?}"
            );
        }
    }

    /// The property the brief calls the hard invariant, over this checkout:
    /// every `.rs` file under `crates/` is outlined, and every row of every
    /// outline is ascending, is that file's line, and re-matches the rule it
    /// came from. Bounded, too: the sweep is a walk and a line scan, and the
    /// bound is asserted so a rule that ever becomes quadratic is caught here
    /// rather than as a slow tool.
    #[test]
    fn every_row_in_this_checkout_is_ascending_and_never_lies() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("the crate sits under <repo>/crates/mush-core");
        let mut files = Vec::new();
        rust_files(&root.join("crates"), &mut files);
        files.sort();
        assert!(files.len() > 30, "the sweep found {} files", files.len());

        let started = Instant::now();
        let mut rows = 0usize;
        for path in &files {
            let text =
                fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let outline = Outline::of(&path.display().to_string(), &text);
            let lines: Vec<&str> = text.lines().collect();
            let mut last = 0usize;
            for definition in outline.definitions() {
                rows += 1;
                assert!(
                    definition.line > last,
                    "{}: line {} after {} — rows must ascend",
                    path.display(),
                    definition.line,
                    last
                );
                last = definition.line;
                let line = lines[definition.line - 1];
                assert!(
                    is_declaration(line),
                    "{}:{} is not a declaration: {line:?}",
                    path.display(),
                    definition.line
                );
                let body = definition
                    .text
                    .strip_suffix('…')
                    .unwrap_or(&definition.text);
                assert!(
                    line.starts_with(body),
                    "{}:{}: the row is not that line, cut: {:?}",
                    path.display(),
                    definition.line,
                    definition.text
                );
                assert!(
                    is_declaration(&definition.text),
                    "{}:{}: a row may never lie — {:?} does not re-match the rule",
                    path.display(),
                    definition.line,
                    definition.text
                );
            }
        }
        let elapsed = started.elapsed();
        // The number, not only the bound: a sweep whose cost moves is worth
        // seeing in a `--nocapture` run, the same reading `prompt`'s schema
        // size gets.
        eprintln!("outline sweep: {} files, {rows} rows, {elapsed:?}", files.len());
        assert!(rows > 500, "the sweep found only {rows} rows");
        assert!(
            elapsed < Duration::from_secs(2),
            "the sweep of {} files and {rows} rows took {elapsed:?}",
            files.len()
        );
    }

    /// Every `.rs` file under `dir`, depth-first, in name order — a handful of
    /// lines rather than a walker dependency, and the order is not the
    /// property's business (the sort in the caller is).
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                out.push(path);
            }
        }
    }
}
