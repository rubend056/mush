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
//! **Textual, best-effort across many languages, and every answer says so out
//! loud.** This is not a parser and must never pretend to be one. It reads
//! *lines*: an item a macro generates is invisible (there is no expansion here
//! to see it in), a `fn` or a `def` spelled inside a string or inside a line of
//! prose that is not a comment can be a row, and a declaration split across
//! lines is only ever its first line. The header [`Outline::render`] opens with
//! carries that sentence on every answer, because a model that trusts an
//! outline further than the rule reaches is a model mush misled — the same
//! reason `usages` says "textual" and `read_file` names the lines a window
//! left.
//!
//! **One rule, and no path.** [`is_declaration`] takes the line and nothing
//! else: [`crate::usages`] asks the same predicate of every line it walks for a
//! symbol, and it has no file name to read a language off either, so a path
//! cannot be threaded to either caller. The keyword lists below are therefore a
//! *union* over the fifty-odd languages a source file is likely to be written
//! in, and the union is why conservatism outranks coverage: a word earns its
//! place only when a line *opening* with it is that language's way of declaring
//! something. A file of another language reads the rows anyway — that is what
//! "textual" costs, and the header says so before the rows do.
//!
//! The union has a second cost the header also covers: the rule reads a line's
//! shape and cannot see *nesting*, so a `val`, `var` or `const` inside a
//! function body is a row exactly like a property of a type — a language's own
//! scoping is nothing a line can tell it.
//!
//! **The rule, exactly.** A line is a declaration when, after its leading
//! whitespace, it opens with
//!
//! - a declaration keyword followed by a *name*: the union in `KEYWORDS` —
//!   Rust's `fn`, `struct`, `enum`, `trait`, `impl`, `const`, `static`, `type`,
//!   `mod`, `union` and `macro`; Go's `func`, `type` and `var`; Python's,
//!   Ruby's, Scala's and Groovy's `def`; JavaScript's, PHP's, Lua's and shell's
//!   `function`; `class`, `struct`, `interface` and `enum` the
//!   object-oriented world over; Swift's `extension`, `protocol` and
//!   `typealias`; Haskell's `data`, `newtype` and `instance`; Julia's
//!   `function`; Nim's `proc`; Perl's `sub`; Ada's and Fortran's `procedure`
//!   and `subroutine`; Protobuf's `message` and `service`; Solidity's
//!   `contract`; MATLAB's `classdef` — the constant is the list, and this is one
//!   word per family of it. The name is an identifier's first character (a
//!   letter or `_`), or `<` for `impl<T>` and `template <`, the two keywords a
//!   generic list may touch without a space; or
//! - the same behind **qualifiers**: `pub` (with its `(crate)`/`(in path)`
//!   group), `extern` with its optional ABI string, and the words that may
//!   stand before a declaration without changing what the line declares —
//!   `async`, `unsafe`, `export`, `declare`, `public`, `private`, `protected`,
//!   `internal`, `abstract`, `sealed`, `final`, `virtual`, `override`,
//!   `partial`, `readonly`, `synchronized`, `native`, `inline`, `local`,
//!   `mutable`, `extend`, `auto`, `default` — bounded by `MAX_QUALIFIERS`,
//!   because a line is not an invitation to loop. `const` is the delicate one:
//!   before `fn` (or a further qualifier of one) it qualifies the function, and
//!   anywhere else it *is* the declaration — that is what tells `const fn f`
//!   from `const N: usize` — and Scala's `case` is its twin, a qualifier before
//!   `class` or `object` and a match arm before anything else; or
//! - **SQL's two words**: `CREATE TABLE users (`, the verb in any case, with
//!   `OR REPLACE` and `IF NOT EXISTS` standing where qualifiers stand
//!   elsewhere, and the *object kind* — `TABLE`, `VIEW`, `INDEX`, `FUNCTION`,
//!   `SCHEMA` and their kin — doing what a keyword does in the other shapes: it
//!   says the statement makes a named thing, where `SELECT`, `INSERT` and
//!   `DROP` make nothing. `ALTER TABLE` is deliberately not read: it changes a
//!   table that already exists; or
//! - **R's assignment**: `mean_of <- function(x)`. An unqualified name opens no
//!   declaration here — that is the rule that keeps every data line and every
//!   call out — and this one shape is read because its right-hand side has to
//!   *be* the word `function` and its argument list: `x <- 1` is refused,
//!   `x <- f(y)` is refused, and so are Swift's and Rust's `let`, which open a
//!   binding and are English words besides; or
//! - a **sigil** touching the word: `@interface`, `@implementation` and
//!   `@protocol` (Objective-C, and Java's `@interface`), Sass's `@mixin` and
//!   `@function`, Elixir's `@type`, and `(defn` — Clojure's and its family's,
//!   where a line opening with `(` is usually a call, so only a def-family word
//!   after the paren is read. The sigil has to touch its word: `@ user` is
//!   prose and `( def` is not how anyone writes a form, and neither declares
//!   anything.
//!
//! HCL writes its block labels in quotes (`resource "aws_instance" "web" {`),
//! so for the six words that are HCL's — `resource`, `variable`, `module`,
//! `output`, `data`, `provider` — a `"` counts where every other keyword needs
//! a letter, and for the four that exist nowhere else it is the only name that
//! counts: `provider aws {` is not a declaration, and a line that merely opens
//! with a quoted string is one nowhere.
//!
//! A `!` or a `-` stuck to a keyword is part of its spelling (`macro_rules!`,
//! vim's `function!`, Clojure's `defn-`), and Go's method receiver is a group
//! between the keyword and the name (`func (r *T) Name()`), skipped to its
//! matching `)` like `pub(…)`'s path is. A byte-order mark at a line's opening
//! is stepped over for the walk's own reason: `Outline::of` is public, and
//! handing it a whole file's bytes should not cost that file its first
//! declaration.
//!
//! A line whose trimmed form is a comment is never a declaration: `//` (with
//! `///` and `//!`), `*`, `#`, `--`, `;`, `%` and `!` are refused before any
//! sigil or keyword is read, which is what keeps a doc comment from making a
//! phantom row — and what makes `#define ROWS 4`, `#[derive(Debug)]` and a
//! commented-out `// fn hash() {}` misses rather than rows. A block comment
//! whose body lines are not `*`-prefixed still shows through, which is the
//! textual rule's declared cost and not a hidden one. The keyword must be
//! followed by a *name*, never by `=`, `:`, `;`, `(`, `)` or `,` — the test the
//! rest of the rule leans on, and what keeps a YAML mapping, a TOML key, a JSON
//! pair and a line of prose out.
//!
//! **What a textual rule cannot see.** The rule reads a line's shape, so a
//! declaration whose shape it does not share is a miss — and a miss is the
//! answer this rule is allowed to give where a guess is not, so every one of
//! these is deliberate rather than unfinished:
//!
//! - a **method or function whose type is not a keyword**: `public int count()`
//!   in Java or C#, `int main(void)` in C, `String name()` in C#. The rule
//!   cannot know a *name* used as a type from a name used as a value, and the
//!   type words it could list — `long`, `short`, `double`, `float` — open lines
//!   of prose. The one return type it reads is `void`: a keyword in every
//!   language that spells a return type with one, and a word of nothing else;
//! - a **bare `name()`**: a shell function (`build() {`), a JavaScript class
//!   method, a C++ constructor. Ruby and Kotlin write a *call* with a trailing
//!   block exactly that way (`foo() { … }`), so this shape is refused for being
//!   ambiguous, which is a thing no other shape here is;
//! - a **line keyed by a name**: a Makefile target (`build:`) and an assembly
//!   label (`main:`) are the shape of a YAML key, and refusing that shape is
//!   what keeps a configuration file from answering with every line it has;
//! - **markup and stylesheets**: an HTML or XML element, a CSS rule
//!   (`body {`) and a Markdown heading (`# …`) declare nothing here — the
//!   heading is a comment line and the rest open with a character no word does;
//! - **a binding rather than a member**: a Nix binding (`foo = { … };`), a Rust
//!   or Swift `let`, a YAML, TOML or JSON key. The `=` rule and `let`'s absence
//!   are what make a data file answer zero rows — which is the right answer for
//!   a file that declares nothing;
//! - **Erlang, and the Lisp family's function form**: Erlang's own declarations
//!   open with a `-` (`-module(foo).`, `-type foo() :: …`) or with a bare name
//!   and `->`, and `(define (square x) …)` in Scheme puts the name where this
//!   rule reads a call's arguments.
//!
//! `impl` blocks are not collapsed and their items are not hidden: a collapsed
//! `impl Foo { … }` row would stand for lines it does not hold, i.e. exactly
//! the lie the invariant below forbids, and the methods inside are the anchors
//! a reader wants most. What keeps a block from drowning the outline is that a
//! row *is* the line — nothing is summarized, nothing is grouped — so the
//! header's count, the room the answer was built with ([`Outline::within`]) and
//! the result cap are the only bounds, and a file whose bulk is one `impl` is a
//! file whose shape is its methods.
//!
//! **The row list is bounded by the answer, not by the file.** A file can hold
//! a declaration on every one of a million lines — generated code, a
//! minified-ish header, a giant table — and building them all to paint one
//! screen spends the file's own size in memory on an answer a few hundred rows
//! long: the same trade [`crate::usages::rows_within`] refuses inside a file and
//! the whole-read cap refuses between files. [`Outline::within`] is the reader
//! an answer uses, and it is the counting design: the walk visits every line and
//! *counts* every declaration — materialising is what costs, and counting is one
//! `usize` on a scan that was going to visit the line anyway — while keeping at
//! most the rows the answer can show, plus the one past them that proves there
//! were more. What was not kept is still named: [`Outline::render`] says how
//! many of the counted rows it did not show, exactly as it does when the result
//! cap is what cut them, and the header's count is the file's own either way.
//!
//! **The invariant: a row may never lie about a line.** Whatever the rule
//! matched on the file's line, the row's own text still matches it — a cut row
//! is cut *after* the word that made it a declaration, never through it
//! ([`row_text`]), so re-running the rule over the row always finds the
//! declaration the row claims. The sweeps at the bottom of this file re-match
//! every row of every `.rs` file in this checkout against the rule it came
//! from, then every `def` and `class` of this checkout's own `scripts/*.py`,
//! then every sample of the language table under a forced cut; that property is
//! the one thing about a textual sketch that can be a hard guarantee.

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
const RULE_NOTE: &str = "textual, many languages, best-effort — not a compiler's answer";

/// The bytes [`Outline::render`] keeps free for the closing note that says how
/// many rows the answer did not show: two counts, spelled with at most twenty
/// digits each. The note itself is written once the loop knows `shown`. The
/// reserve holds for either cut — the result cap's and the room's — because the
/// sentence is the same one.
const CUT_RESERVE: usize = 160;

/// The fewest bytes a rendered row can take, counting the gutter, the line
/// number, the two spaces, the row's text and the newline (`Outline::render`'s
/// own format is `"  {line}  {text}\n"`).
///
/// The text under it is at least four bytes: the shortest word that opens a
/// declaration is two (`fn`), the name the rule insists on is one more, and a
/// word character beside the keyword would make the keyword part of a longer
/// word and no declaration at all ([`declaration_prefix`]). So ten is a floor
/// rather than an estimate of a row's length, and a floor is what
/// [`room_for_cap`] needs: a cap can never pay for more rows than it allows.
const MIN_ROW_BYTES: usize = 10;

/// The rows a result capped at `cap` bytes can show, at most — the room to hand
/// [`Outline::within`] when the cap is known, and the one place the answer's two
/// rulers meet (a cap counts bytes, a room counts rows, and the caller owns
/// both).
///
/// It rounds down, because a cap pays for a whole row or not at all, and it is
/// deliberately the *floor* of a row's length: a conversion that guessed high
/// would keep rows no answer could ever paint, which is the waste the room
/// exists to stop. A room past what the cap pays for costs one row of memory and
/// loses nothing; a room under it costs the rows the cap could have paid for,
/// and the answer says so either way.
pub const fn room_for_cap(cap: usize) -> usize {
    cap / MIN_ROW_BYTES
}

/// The room [`Outline::of`] and the workspace road keep when their caller does
/// not name one: what this crate's ceiling for a result the model reads
/// ([`crate::CMD_CAP`]) can pay for — an outline *is* such a result, and no
/// answer built in this crate is rendered past that ceiling.
///
/// It is *a cap and a high one* — the house rule's own shape — but the room is
/// still the answer's: a caller that knows the exact cap its turn has left
/// passes its own room to [`Outline::within`] and keeps proportionally less,
/// and a file with more rows than the room is not refused — it is counted, and
/// the count is what the header and the closing note say.
pub const DEFAULT_ROOM: usize = room_for_cap(crate::CMD_CAP);

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

/// The outline of one file: the rows the answer kept, how many rows the file
/// holds, the file's own line count, and whether its lines end with CRLF (the
/// fact a window read reports too, so one road's rows and the other's text tell
/// one story about the endings).
#[derive(Clone, Debug)]
pub struct Outline {
    path: String,
    lines: usize,
    crlf: bool,
    /// At most `room + 1` rows ([`Outline::within`]): the one past the room is
    /// the proof that the file held more, and [`Outline::render`] never shows
    /// it. Every row here is a row the answer may claim — the count above it is
    /// the file's, not this list's.
    definitions: Vec<Definition>,
    /// Every declaration the file spells, counted by the walk that visited every
    /// line. Equal to `definitions.len()` exactly when the list is the whole
    /// file's, so the two together are the answer's honesty.
    total: usize,
    /// The rows the answer may show — the room the caller named. It is kept
    /// because the proof row is not showable: `min(room, definitions.len())` is
    /// the most the cap can reach, and `render` says what it left of the rest.
    room: usize,
}

impl Outline {
    /// The outline of `text`, a file the caller has already read (the workspace
    /// road is [`Workspace::outline`](crate::workspace::Workspace::outline),
    /// which owns the read caps and the lossy decode). `path` is the name the
    /// model used, shown back in the header exactly as it was asked about.
    ///
    /// `room` is the rows the answer can show, and it is the caller's: how many
    /// rows fit is a fact about the answer, not about the file, so the door that
    /// knows the cap it will render at converts it ([`room_for_cap`]). The walk
    /// visits every line whatever the room is, so the file's count is exact and
    /// the room decides only what is *built*: a room past the file's own rows
    /// keeps them all, and a room under them keeps the first `room + 1` — the
    /// last of those the proof that there was more ([`Self::definitions`]) —
    /// while the answer names the rest rather than pretending to hold it.
    pub fn within(path: &str, text: &str, room: usize) -> Self {
        let found = scan(text, room);
        Self {
            path: path.trim().to_string(),
            lines: found.lines,
            crlf: text::is_crlf(text),
            definitions: found.rows,
            total: found.total,
            room,
        }
    }

    /// The outline of `text` for a caller with no cap of its own: the room this
    /// crate's ceiling for a result the model reads can pay for ([`DEFAULT_ROOM`]).
    /// A caller that *does* know its cap — the app renders a tool result at the
    /// `result_cap` its turn has left — passes [`room_for_cap`] of it to
    /// [`Self::within`] and keeps proportionally fewer rows for the same answer,
    /// which is the point of the room being the caller's.
    pub fn of(path: &str, text: &str) -> Self {
        Self::within(path, text, DEFAULT_ROOM)
    }

    /// Nothing to sketch: the answer is a sentence, not a header with no rows.
    /// The count is the file's own, so a room too small to keep a row still
    /// reads as "nothing declares anything here" only when that is true.
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// The rows this answer kept, in the file's own order — at most `room + 1`
    /// of them, the last being the proof row when the file held more than the
    /// room ([`Self::within`]), which [`Self::render`] never shows. The list is
    /// the whole file's exactly when it is as long as [`Self::total`]; a caller
    /// that needs every row asks for a room that pays for them rather than
    /// reading this list as the file.
    pub fn definitions(&self) -> &[Definition] {
        &self.definitions
    }

    /// How many declarations the walk found in the whole file: the header's
    /// count, exact even when [`Self::definitions`] kept only the room's rows,
    /// because counting is what the walk that visits every line does anyway.
    pub fn total(&self) -> usize {
        self.total
    }

    /// The line the header opens with: the file, its length, how many
    /// definitions the rule found, and the rule's own confession. The count is
    /// [`Self::total`] — the file's, not the kept list's — so a bounded row list
    /// makes the header no less exact.
    pub fn header(&self) -> String {
        format!(
            "{} — {}; {} ({RULE_NOTE})",
            self.path,
            count(self.lines, "line"),
            count(self.total, "definition"),
        )
    }

    /// The whole answer, within `cap` bytes: the header, `lead` under it when
    /// the caller has a sentence of its own (the unbounded-read fallback says
    /// there what the model is not being shown), a blank, then the rows — as
    /// many as the cap pays for and the room kept, which is every row the answer
    /// holds when the two are generous — and the closing notes.
    ///
    /// A row left out is always said, whichever of the two bounds left it: the
    /// note under the rows names the file's own count and the rows it showed,
    /// and an answer that shows every row it counted has nothing to say and says
    /// nothing (which is what makes a small file's answer the bytes it always
    /// was).
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
            // The cap cannot hold the header, a row and the room the cut note
            // needs, so the rows are not shown at all — and a header with a
            // count and nothing under it is the one answer that must never be
            // handed over as if it were the whole sketch. The note says what was
            // left, with the count of rows it showed (none); a cap too small
            // even for that is a cap too small for any answer, and the header is
            // the answer — cut, and marked as cut, by `truncate_for_model`.
            if out.ends_with('\n') {
                out.pop();
            }
            let note = self.cut_note(0);
            if out.len() + note.len() <= cap {
                out.push_str(&note);
            }
            return truncate_for_model(out, cap);
        }
        let mut shown = 0usize;
        // The rows the room allows: the one kept past it is the proof, never a
        // row of the answer.
        for definition in self.definitions.iter().take(self.room) {
            let row = format!("  {}  {}\n", definition.line, definition.text);
            if out.len() + row.len() > budget {
                break;
            }
            out.push_str(&row);
            shown += 1;
        }
        if out.ends_with('\n') {
            out.pop();
        }
        // Two different cuts leave a row out — the cap could not pay for it, or
        // the room never kept it — and both need the same sentence, because the
        // reader's question is only what it was not shown. `shown == self.total`
        // is the whole sketch and then there is nothing to say; anything less is
        // a count the answer owes the model.
        if shown < self.total {
            out.push_str(&self.cut_note(shown));
        }
        out.push_str(&crlf);
        truncate_for_model(out, cap)
    }

    /// The sentence under a bounded answer that says what it left: one home for
    /// the counts, so the row loop that stopped early, the room that kept fewer
    /// rows than the file holds, and the cap too small for a single row all say
    /// it the same way (the last with `shown` zero).
    ///
    /// It is the count of the *file* that the sentence names as the whole —
    /// [`Self::total`], which the walk verified line by line — and the count of
    /// what the reader can see beside it, so no reading of it can claim a row
    /// the answer did not keep. It is bounded by [`CUT_RESERVE`], which is why
    /// `render` can reserve its room before it starts: two counts, spelled with
    /// at most twenty digits each, and one fixed clause naming the road that
    /// still shows any row.
    fn cut_note(&self, shown: usize) -> String {
        format!(
            "\n[mush: only the first {shown} of {} definitions are shown — read_file \
             {{offset, limit}} shows a range around any row's line]",
            self.total
        )
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
///
/// The whole list, with no room to bound it: [`Outline::within`] is the reader
/// an answer uses, and this is that walk with room for all of it, for the
/// callers that want the rule's own answer rather than an answer's window (the
/// sweeps at the bottom of this file). A generated file holds a row on every
/// line it has, so a caller that builds this list has already decided to spend
/// the file's size in memory — which is exactly what the tool road must not do,
/// and does not: the shape is here for the rule's own reader, and the answer's
/// door is bounded.
pub fn definitions(text: &str) -> Vec<Definition> {
    scan(text, usize::MAX).rows
}

/// What one walk of a file's text found: the rows the answer keeps, how many
/// rows the file holds, and how many lines it has.
struct Scan {
    /// At most `room + 1` rows, in the file's own order. The one row past the
    /// room is the proof that the file held more than the room: the list is
    /// longer than the room exactly when it is not the whole file
    /// (`total > room`), which is the property [`crate::usages::rows_within`]
    /// carries into its caller as one more row than the room.
    rows: Vec<Definition>,
    /// Every declaration the file spells, counted while the walk visited every
    /// line. This is the number the header and the closing note name, so the
    /// count stays exact where a list cannot be.
    total: usize,
    /// The file's own line count, from the same walk ([`str::lines`], the
    /// reader both this and the header's count use).
    lines: usize,
}

/// The walk behind [`Outline::within`] and [`definitions`]: one pass over the
/// text that counts what the file holds, counts its lines, and keeps what the
/// answer can show.
///
/// Counting every row while building only the room's is the whole shape. The
/// scan cannot stop early the way `usages`' does: a row's text is a reader's
/// line, so the file's count is only exact if every line was read, and the count
/// is what the header claims. What the walk does not build is what it does not
/// keep, and the room bounds the built rows — the file's own size never enters
/// the answer's memory (finding IN9's trade, the same one `list_files` and the
/// search walk refuse between files).
///
/// `row_text` is the rule's own reader for a row's own text, and it allocates;
/// it is asked only for the rows the answer keeps, so the allocations the walk
/// makes are the room's rather than the file's while `is_declaration` — the
/// non-allocating half of the same rule — answers for every line.
fn scan(text: &str, room: usize) -> Scan {
    // One past the room: `room + 1` rows are the proof that the file held more
    // than the room, and nothing past them can enter the answer.
    let keep = room.saturating_add(1);
    let mut found = Scan {
        rows: Vec::new(),
        total: 0,
        lines: 0,
    };
    for (index, line) in text.lines().enumerate() {
        found.lines = index + 1;
        if !is_declaration(line) {
            continue;
        }
        found.total += 1;
        if found.rows.len() < keep {
            if let Some(text) = row_text(line) {
                found.rows.push(Definition {
                    line: index + 1,
                    text,
                });
            }
        }
    }
    found
}

/// Whether a tool result is one of this module's answers: a first line carrying
/// the header's own confession. The app's digest reads it to tell a `read_file`
/// that came back as an outline — the unbounded read's fallback — from one that
/// came back as text, and then reads it exactly as the `outline` tool's own
/// results are read, so the two readings cannot drift apart.
pub fn is_outline_answer(result: &str) -> bool {
    result
        .lines()
        .next()
        .is_some_and(|line| line.contains(RULE_NOTE))
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

/// The words that are a declaration on their own, in the language that spells
/// them that way, with `macro_rules!` handled beside this list (its `!` is part
/// of the spelling).
///
/// A union and not a language: [`is_declaration`] takes a line and no file
/// name, so nothing here knows what it is reading. A word earns its place by one
/// test — a line *opening* with it is that language's way of declaring
/// something — and the cost of a union is that the word also answers in a file
/// of another language; the module doc's "what it cannot see" is the other half
/// of that bargain.
const KEYWORDS: &[&str] = &[
    // Rust
    "fn",
    "struct",
    "enum",
    "trait",
    "impl",
    "const",
    "static",
    "type",
    "mod",
    "union",
    "macro",
    "macro_rules",
    // C, C++ and C#
    "class",
    "namespace",
    "template",
    "typedef",
    "void",
    "record",
    "delegate",
    // Java, C# and PHP
    "interface",
    "package",
    "trait",
    // Objective-C
    "implementation",
    "protocol",
    // Go
    "func",
    "var",
    // JavaScript, TypeScript, the JSX/TSX spellings of them, PHP, Lua and shell
    "function",
    // Python, Ruby, Scala and Groovy
    "def",
    "module",
    // Swift, Kotlin and Scala
    "extension",
    "typealias",
    "fun",
    "val",
    "object",
    // Dart and Sass
    "mixin",
    // Haskell and F#
    "data",
    "newtype",
    "instance",
    // Elixir
    "defp",
    "defmodule",
    "defmacro",
    "defprotocol",
    "defimpl",
    "defguard",
    // Perl
    "sub",
    // Nim, Tcl, Ada, Fortran and Pascal
    "proc",
    "procedure",
    "subroutine",
    // Solidity
    "contract",
    "library",
    "modifier",
    // MATLAB
    "classdef",
    // Protobuf and GraphQL
    "message",
    "service",
    "rpc",
    "oneof",
    "input",
    "scalar",
    "fragment",
];

/// The words that are a declaration only behind Lisp's `(`, where `(type x)` is
/// a *call* to `type` and only a def-family word can tell a definition from
/// one. Read in place of [`KEYWORDS`] when [`sigil`] reports the paren, and kept
/// apart for exactly that reason.
const LISP_KEYWORDS: &[&str] = &[
    "def",
    "defn",
    "define",
    "defmacro",
    "defmethod",
    "defmulti",
    "defonce",
    "defprotocol",
    "defrecord",
    "defstruct",
    "deftype",
    "defun",
    "defvar",
    "ns",
];

/// The keywords whose declaration names its thing in a *string*, as HCL writes
/// it: `resource "aws_instance" "web" {`. A `"` counts as a name after these
/// words and no others — and for the four that exist nowhere else it is the
/// only name that counts, so `provider "aws" {` is a row while `provider aws {`
/// is not: HCL puts a label in quotes, and a bare word there is not one.
const QUOTED_NAMES: &[&str] = &[
    "data", "module", "output", "provider", "resource", "variable",
];

/// The words that may stand before a declaration keyword without changing what
/// the line declares. `pub`, `extern` and the conditional table beside this one
/// need an arm of their own in [`declaration_prefix`]: the first two may carry a
/// group, and a conditional qualifier is a qualifier only sometimes.
const MODIFIERS: &[&str] = &[
    "abstract",
    "async",
    "auto",
    "declare",
    "default",
    "export",
    "extend",
    "final",
    "inline",
    "internal",
    "local",
    "mutable",
    "native",
    "override",
    "partial",
    "private",
    "protected",
    "public",
    "readonly",
    "sealed",
    "synchronized",
    "unsafe",
    "virtual",
];

/// The qualifiers that qualify only in front of certain words, and the words
/// they qualify. `const fn` is a qualified function while `const N: usize` is
/// the declaration itself, and Scala's `case class` is a qualified class while
/// Haskell's `case x of` is an expression that declares nothing.
const QUALIFIER_BEFORE: &[(&str, &[&str])] = &[
    ("const", &["fn", "unsafe", "async", "extern", "impl"]),
    ("case", &["class", "object"]),
];

/// SQL's object kinds: the word after `CREATE` that makes the statement a
/// declaration of a named thing, where `SELECT`, `INSERT` and `DROP` make
/// nothing this rule reads.
const SQL_KINDS: &[&str] = &[
    "database",
    "domain",
    "extension",
    "function",
    "index",
    "procedure",
    "role",
    "schema",
    "sequence",
    "table",
    "trigger",
    "type",
    "user",
    "view",
];

/// The words that may stand between SQL's `CREATE` and its object kind, and
/// after it: they say how the thing is made (`OR REPLACE`, `UNIQUE`, `IF NOT
/// EXISTS`), not what it is.
const SQL_NOISE: &[&str] = &[
    "exists",
    "global",
    "if",
    "local",
    "materialized",
    "not",
    "or",
    "replace",
    "temp",
    "temporary",
    "unique",
];

/// How many leading qualifiers the walk will strip before it gives up: enough
/// for `pub(in path) const unsafe extern "C" async fn` with room to spare, and
/// a bound, because a line is not an invitation to loop. SQL's head reads its
/// words under the same bound.
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
/// The shapes it reads are the module doc's list: a keyword and a name; the
/// same behind qualifiers; SQL's two words; R's `<- function`; a sigil's word.
/// `extern "C"`'s ABI string is skipped because `"C"` is not a word and the
/// next word is the keyword; `pub(in …)`'s group is skipped to its matching
/// `)`; `const` looks one word ahead because `const fn` and `const ITEM` are
/// the two shapes the same word opens. An unreadable shape (an unbalanced
/// group, an unterminated string) is simply not a declaration: the rule may
/// miss, but it may not guess.
fn declaration_prefix(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    // A byte-order mark is the file's, not the line's, and a caller that hands
    // this rule a whole file's bytes rather than the walk's decoded text would
    // otherwise read three invisible bytes as the line's opening word and lose
    // the file's first declaration to them.
    let trimmed = trimmed
        .strip_prefix('\u{FEFF}')
        .map_or(trimmed, str::trim_start);
    if is_comment(trimmed) {
        return None;
    }
    let base = line.len() - trimmed.len();
    let (mut rest, lisp) = sigil(trimmed);
    let head = rest;
    let head_at = base + (trimmed.len() - head.len());
    // SQL and R spell their declarations in shapes no keyword-led line has, so
    // they are read whole before the qualifier walk can break on their first
    // word.
    if let Some(at) = sql_head(rest) {
        return Some(head_at + at);
    }
    if let Some(at) = r_definition(rest) {
        return Some(head_at + at);
    }
    for _ in 0..MAX_QUALIFIERS {
        let (word, after) = leading_word(rest);
        let next = leading_word(rest[after..].trim_start()).0;
        match word {
            // `pub`, and the `(crate)` / `(super)` / `(in path)` group it may
            // carry. The group is skipped by its matching `)`, not by the next
            // space, because the path inside may hold spaces.
            "pub" => rest = skip_pub(rest)?,
            // `extern "C"` / `extern "system"`: the string is part of the
            // qualifier, and the keyword follows it.
            "extern" => rest = skip_extern(rest),
            // A qualifier that qualifies only in front of a set of words
            // ([`QUALIFIER_BEFORE`]): `const fn` strips the `const`, while
            // `const N: usize` leaves it to the shared tail below, so the
            // witness includes the name and a cut row still re-matches.
            _ if MODIFIERS.contains(&word)
                || QUALIFIER_BEFORE
                    .iter()
                    .any(|(qualifier, before)| *qualifier == word && before.contains(&next)) =>
            {
                rest = rest[after..].trim_start()
            }
            _ => break,
        }
    }
    let (word, after) = leading_word(rest);
    let start = head_at + (head.len() - rest.len());
    let mut tail = rest[after..].trim_start();
    // A `!` or a `-` stuck to the keyword is part of its spelling:
    // `macro_rules!`, vim's `function!`, Clojure's `defn-`.
    if tail.starts_with('!') || tail.starts_with('-') {
        tail = tail[1..].trim_start();
    }
    // Go's method receiver: `func (r *T) Name()`. The group is skipped the way
    // `pub(…)`'s is, so the name — and the witness — is the method's own.
    if word == "func" && tail.starts_with('(') {
        tail = skip_group(tail)?;
    }
    // The word has to be followed by a name, not by `=`, `:`, `;`, `(`, `)` or
    // `,`: this is what keeps `fnord()`, `types`, a TOML `type = "lib"` and a
    // YAML `type: string` out of an outline. The one name that is not a word is
    // HCL's quoted label ([`QUOTED_NAMES`]).
    let first = tail.chars().next()?;
    let named = first.is_alphabetic() || first == '_' || first == '<';
    let quoted = first == '"' && QUOTED_NAMES.contains(&word);
    let keywords = if lisp { LISP_KEYWORDS } else { KEYWORDS };
    let skipped = rest[after..].len() - tail.len();
    ((named && keywords.contains(&word)) || quoted)
        .then_some(start + after + skipped + first.len_utf8())
}

/// Whether the trimmed opening of a line is a comment marker rather than code.
///
/// [`declaration_prefix`] asks this first, before any sigil or keyword is read,
/// so "a comment is never a declaration" is a sentence of the rule. The name
/// requirement would refuse most of these lines on its own — no word opens with
/// `/`, `#` or `;` — and the markers are still worth their bytes: the reason a
/// commented-out declaration is not a row should be the rule's own words rather
/// than a side effect of the alphabet, and the shapes people ask about
/// (`#define ROWS 4`, `#[derive(Debug)]`, `#!/bin/sh`) are answered here.
///
/// The markers are the world's: `//` (with `///` and `//!`), `*` (the middle and
/// the end of a block comment), and the openings of Python's and shell's `#`,
/// SQL's and Haskell's `--`, Lisp's `;`, TeX's and MATLAB's `%` and Fortran's
/// `!`. A marker that is not here needs no entry: `<!--`, `/*`, `(*`, `{-` and
/// `'''` open no word either, and the name requirement refuses those lines one
/// step later with the same answer by another road.
fn is_comment(trimmed: &str) -> bool {
    const MARKERS: &[&str] = &["//", "*", "#", "--", ";", "%", "!"];
    MARKERS.iter().any(|marker| trimmed.starts_with(marker))
}

/// The line past the sigil it opens with, and whether that sigil was Lisp's
/// `(`.
///
/// Two sigils are read, each a language's way of opening a declaration rather
/// than punctuation: `@interface`, `@implementation` and `@protocol`
/// (Objective-C, and Java's `@interface`), Sass's `@mixin` and `@function`,
/// Elixir's `@type` — one `@`, several languages — and `(defn`, Clojure's and
/// its family's, where a line opening with `(` is usually a *call* and only a
/// def-family word after it can tell a definition from one. A sigil has to
/// touch its word: `@ user` is prose and `( def` is not how anyone writes a
/// form, and neither is a declaration.
fn sigil(trimmed: &str) -> (&str, bool) {
    let after = trimmed.get(1..).unwrap_or("");
    let word_follows = after.starts_with(|ch: char| ch.is_alphanumeric() || ch == '_');
    match trimmed.as_bytes().first() {
        Some(b'@') if word_follows => (after, false),
        Some(b'(') if word_follows => (after, true),
        _ => (trimmed, false),
    }
}

/// R's definition shape: `mean_of <- function(x)`, and nothing else.
///
/// A definition in R is an assignment, and an assignment on a line of its own
/// is what every other language here writes for a call's result or a data line
/// — so the shape is read only when the right-hand side *is* the word `function`
/// and its argument list opens right after it. `x <- 1` is refused, `x <- f(y)`
/// is refused, `x <- function(u) u + 1` is a declaration. The witness is just
/// past the `(`, so a cut row keeps the whole shape that made it one.
fn r_definition(rest: &str) -> Option<usize> {
    let (name, after) = leading_word(rest);
    let first = name.chars().next()?;
    if !(first.is_alphabetic() || first == '_') {
        return None;
    }
    let arrow = rest[after..].trim_start().strip_prefix("<-")?;
    let tail = arrow.trim_start();
    let (word, after) = leading_word(tail);
    if word != "function" {
        return None;
    }
    let list = tail[after..].trim_start();
    list.strip_prefix('(')?;
    Some(rest.len() - list.len() + 1)
}

/// SQL's declaration shape: `CREATE TABLE users (`, in any case.
///
/// SQL is the one language here whose declaration is two words before the name,
/// and the object kind is the word that decides: `CREATE TABLE`, `CREATE VIEW`,
/// `CREATE INDEX`, `CREATE FUNCTION` and their kin name the thing the statement
/// makes, which is what makes the line a declaration; `ALTER` is deliberately
/// not read, because it changes a table that already exists. The verb is
/// matched case-insensitively — SQL's own convention is to shout it — and the
/// witness lands past the name's first byte, so a cut row keeps the kind and
/// the name both.
fn sql_head(rest: &str) -> Option<usize> {
    let (verb, after) = leading_word(rest);
    if !verb.eq_ignore_ascii_case("create") {
        return None;
    }
    let mut at = after;
    let mut kind = false;
    for _ in 0..MAX_QUALIFIERS {
        let gap = rest[at..].len() - rest[at..].trim_start().len();
        at += gap;
        let (word, after) = leading_word(&rest[at..]);
        let in_list = |list: &[&str]| list.iter().any(|entry| word.eq_ignore_ascii_case(entry));
        if word.is_empty() {
            return None;
        }
        if in_list(SQL_NOISE) {
            at += after;
            continue;
        }
        if !kind {
            if !in_list(SQL_KINDS) {
                return None;
            }
            kind = true;
            at += after;
            continue;
        }
        let first = rest[at..].chars().next()?;
        return (first.is_alphabetic() || first == '_').then_some(at + first.len_utf8());
    }
    None
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
    skip_group(rest)
}

/// `rest` past the balanced group it opens with. `rest` has to start with `(`,
/// and the answer is the line past the matching `)`. `None` when the group
/// never closes: an unreadable line is not a declaration rather than a guess.
/// One home for both callers — `pub(…)`'s visibility path and Go's method
/// receiver — because the property that matters is the same for both: a group
/// is skipped by its nesting, not by the next space.
fn skip_group(rest: &str) -> Option<&str> {
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
            // A byte-order mark is the file's, not the line's: the rule steps
            // over one so a file handed to `Outline::of` whole still answers
            // with its first declaration.
            "\u{FEFF}fn marked() {}",
            "\u{FEFF}  pub fn indented() {}",
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
            // The name rule, over the words the union added: a keyword is only
            // a declaration when a *name* follows it, and never when what
            // follows is a value, a label or a call.
            "data: 3",
            "data = 3",
            "data(x)",
            "message: \"hi\"",
            "message = \"hi\"",
            "class: 3",
            "func(x)",
            "def(x)",
            "type(3)",
            "module.exports = {};",
            "package.json",
            "interface:",
            "record = 3",
            "void = 3",
            "function(x)",
            "SELECT * FROM users;",
            "const: 3",
            "static: 3",
            "local: 3",
        ] {
            assert!(!is_declaration(line), "{line:?} is not a declaration");
        }
    }

    /// One language's sample: the file as it would really look, the lines the
    /// rule must read as declarations, and the lines it must not.
    ///
    /// `text` is written at its own column — the leading `\` eats only the
    /// newline that opens the literal — because a sample whose indentation is
    /// escaped into continuation lines is a sample nobody can read, and these
    /// are meant to be read: an entry is the argument for why a word belongs in
    /// the union, made in the language's own spelling.
    struct Sample {
        /// The language, named the way the module doc names it.
        language: &'static str,
        /// Three to eight lines of the language, written the way its files are.
        text: &'static str,
        /// Every row `text` must answer, in the file's own order.
        rows: &'static [&'static str],
        /// Lines of `text` that must not be rows: a comment, a call, a string
        /// or a data line, as that language writes one. At least one per
        /// sample, because a sample of nothing but declarations tests half the
        /// rule.
        not_rows: &'static [&'static str],
        /// A sentence for an entry whose answer needs defending — chiefly a
        /// language the rule cannot see at all, which says here why it has no
        /// rows. Empty when the sample speaks for itself, and the test refuses
        /// an empty `rows` with an empty note: a language answered with nothing
        /// is a decision, not an oversight.
        note: &'static str,
    }

    /// The languages, one sample each: this table is the rule's coverage claim
    /// and its negative space at the same time.
    ///
    /// An entry is a language *answered*, and a language whose declaration the
    /// rule cannot see honestly has an empty `rows` and says so in its sample —
    /// a data file declares nothing, and no rows is the right answer, not a
    /// guess. A new language is one entry here, never one test.
    const SAMPLES: &[Sample] = &[
        Sample {
            language: "Rust",
            text: "\
//! The module's own doc — a comment, and never a row.
pub fn digest(name: &str) -> u64 {
    name.len() as u64
}

struct CallFacts;

impl CallFacts {
    fn hits(&self) -> usize { 0 }
}
",
            rows: &[
                "pub fn digest(name: &str) -> u64 {",
                "struct CallFacts;",
                "impl CallFacts {",
                "    fn hits(&self) -> usize { 0 }",
            ],
            not_rows: &[
                "//! The module's own doc — a comment, and never a row.",
                "    name.len() as u64",
            ],
            note: "",
        },
        Sample {
            language: "Python",
            text: "\
# A registry of handlers, keyed by name.
import json

class Registry:
    def register(self, name):
        handlers[name] = self.handle

    async def dispatch(self, event):
        return json.dumps(event)
",
            rows: &[
                "class Registry:",
                "    def register(self, name):",
                "    async def dispatch(self, event):",
            ],
            not_rows: &[
                "# A registry of handlers, keyed by name.",
                "import json",
                "        handlers[name] = self.handle",
            ],
            note: "",
        },
        Sample {
            language: "Go",
            text: "\
package server

// Server answers one request at a time.
type Server struct {
\taddr string
}

func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {
\tbody := s.decode(r)
\t_, _ = w.Write(body)
}
",
            rows: &[
                "package server",
                "type Server struct {",
                "func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {",
            ],
            not_rows: &[
                "// Server answers one request at a time.",
                "\tbody := s.decode(r)",
                "\t_, _ = w.Write(body)",
            ],
            note: "",
        },
        Sample {
            language: "JavaScript",
            text: "\
import { render } from \"./dom.js\";

export function mount(root, props) {
  root.append(render(props));
}

export default class App {
  constructor(props) {
    this.props = props;
  }
}

const once = () => root.textContent;
",
            rows: &[
                "export function mount(root, props) {",
                "export default class App {",
                "const once = () => root.textContent;",
            ],
            not_rows: &[
                "import { render } from \"./dom.js\";",
                "  constructor(props) {",
                "  root.append(render(props));",
            ],
            note: "",
        },
        Sample {
            language: "TypeScript",
            text: "\
interface User {
  id: string;
}

type Alias = User | null;

export abstract class Store {
  private ready = false;

  abstract load(id: string): Promise<User>;

  async fetch(id: string): Promise<User> {
    return this.load(id);
  }
}
",
            rows: &[
                "interface User {",
                "type Alias = User | null;",
                "export abstract class Store {",
            ],
            not_rows: &[
                "  async fetch(id: string): Promise<User> {",
                "  private ready = false;",
                "  abstract load(id: string): Promise<User>;",
            ],
            note: "A method whose return type is not `void` is a miss, and this\n                   sample says so twice: `Promise<User>` and `private ready` are\n                   no rows.",
        },
        Sample {
            language: "Java",
            text: "\
package com.example.app;

public class Main {
    public static void main(String[] args) {
        System.out.println(\"hello\");
    }

    @Override
    public void run() {
    }

    public int compute() { return 0; }
}
",
            rows: &[
                "package com.example.app;",
                "public class Main {",
                "    public static void main(String[] args) {",
                "    public void run() {",
            ],
            not_rows: &[
                "    @Override",
                "    public int compute() { return 0; }",
                "        System.out.println(\"hello\");",
            ],
            note: "",
        },
        Sample {
            language: "C",
            text: "\
#include <stdio.h>

struct point { int x; int y; };

typedef struct point point;

static void print_point(point p) {
    printf(\"%d\\n\", p.x);
}

int main(void) {
    print_point((point){1, 2});
}
",
            rows: &[
                "struct point { int x; int y; };",
                "typedef struct point point;",
                "static void print_point(point p) {",
            ],
            not_rows: &[
                "#include <stdio.h>",
                "int main(void) {",
                "    printf(\"%d\\n\", p.x);",
            ],
            note: "`int main(void)` is the miss the module doc names first: a\n                   return type that is not a keyword is invisible to a rule that\n                   cannot tell a type name from any other name.",
        },
        Sample {
            language: "C++",
            text: "\
#include <string>

namespace app {

template <typename T>
class Box {
 public:
  T value() const { return value_; }

 private:
  T value_;
};

}  // namespace app
",
            rows: &[
                "namespace app {",
                "template <typename T>",
                "class Box {",
            ],
            not_rows: &[
                "#include <string>",
                "  T value() const { return value_; }",
                "}  // namespace app",
            ],
            note: "",
        },
        Sample {
            language: "C#",
            text: "\
using System;

namespace App
{
    public sealed record User(int Id);

    internal static class Program
    {
        private static void Main(string[] args)
        {
            Console.WriteLine(\"hi\");
        }
    }
}
",
            rows: &[
                "namespace App",
                "    public sealed record User(int Id);",
                "    internal static class Program",
                "        private static void Main(string[] args)",
            ],
            not_rows: &[
                "using System;",
                "            Console.WriteLine(\"hi\");",
            ],
            note: "",
        },
        Sample {
            language: "Objective-C",
            text: "\
#import <UIKit/UIKit.h>

@interface MyView : UIView

@property (nonatomic) NSString *title;

@end

@implementation MyView

- (void)drawRect:(CGRect)rect {
    [super drawRect:rect];
}

@end
",
            rows: &["@interface MyView : UIView", "@implementation MyView"],
            not_rows: &[
                "#import <UIKit/UIKit.h>",
                "@property (nonatomic) NSString *title;",
                "- (void)drawRect:(CGRect)rect {",
                "    [super drawRect:rect];",
            ],
            note: "",
        },
        Sample {
            language: "Ruby",
            text: "\
# A tiny service object.
require \"json\"

module Handlers
  class Greeting
    def call(name)
      \"hello #{name}\"
    end
  end
end
",
            rows: &["module Handlers", "  class Greeting", "    def call(name)"],
            not_rows: &[
                "# A tiny service object.",
                "require \"json\"",
                "      \"hello #{name}\"",
            ],
            note: "",
        },
        Sample {
            language: "SQL",
            text: "\
-- The users of the app.
CREATE TABLE users (
    id serial primary key,
    email text not null
);

CREATE INDEX idx_users_email ON users (email);

CREATE OR REPLACE FUNCTION bump(counter int) RETURNS int AS $$
    SELECT counter + 1;
$$ LANGUAGE sql;

SELECT count(*) FROM users;
ALTER TABLE users ADD COLUMN active boolean;
",
            rows: &[
                "CREATE TABLE users (",
                "CREATE INDEX idx_users_email ON users (email);",
                "CREATE OR REPLACE FUNCTION bump(counter int) RETURNS int AS $$",
            ],
            not_rows: &[
                "-- The users of the app.",
                "SELECT count(*) FROM users;",
                "ALTER TABLE users ADD COLUMN active boolean;",
            ],
            note: "`ALTER TABLE` is refused on purpose: it changes a table that\n                   already exists, which is not a declaration.",
        },
        Sample {
            language: "R",
            text: "\
# The mean of a numeric vector.
mean_of <- function(x, na.rm = TRUE) {
  sum(x, na.rm = na.rm) / length(x)
}

ratio <- mean_of(values) / 2

cat(\"done\")
",
            rows: &["mean_of <- function(x, na.rm = TRUE) {"],
            not_rows: &[
                "# The mean of a numeric vector.",
                "ratio <- mean_of(values) / 2",
                "cat(\"done\")",
            ],
            note: "The one assignment shape the rule reads: the right-hand side has\n                   to *be* the word `function`, so `ratio <- mean_of(…)` is not a\n                   row.",
        },
        Sample {
            language: "Clojure",
            text: "\
(ns app.core)

(def version \"1.0.0\")

(defn handle [req]
  {:status 200})

(defmacro when-ok [x]
  (list 'when x))
",
            rows: &[
                "(ns app.core)",
                "(def version \"1.0.0\")",
                "(defn handle [req]",
                "(defmacro when-ok [x]",
            ],
            not_rows: &["  {:status 200})", "  (list 'when x))"],
            note: "",
        },
        Sample {
            language: "Haskell",
            text: "\
-- The shapes a drawing has.
module Shape where

data Shape
  = Circle Double
  | Square Double

newtype Name = Name String

instance Show Shape where
  show (Circle r) = \"circle\"
",
            rows: &[
                "module Shape where",
                "data Shape",
                "newtype Name = Name String",
                "instance Show Shape where",
            ],
            not_rows: &[
                "-- The shapes a drawing has.",
                "  = Circle Double",
                "  show (Circle r) = \"circle\"",
            ],
            note: "",
        },
        Sample {
            language: "HCL",
            text: "\
terraform {
  required_version = \">= 1.5\"
}

provider \"aws\" {
  region = var.region
}

resource \"aws_instance\" \"web\" {
  ami           = \"ami-123\"
  instance_type = \"t3.micro\"
}

variable \"region\" {
  default = \"eu-west-1\"
}
",
            rows: &[
                "provider \"aws\" {",
                "resource \"aws_instance\" \"web\" {",
                "variable \"region\" {",
            ],
            not_rows: &[
                "terraform {",
                "  region = var.region",
                "  ami           = \"ami-123\"",
            ],
            note: "",
        },
        Sample {
            language: "HTML",
            text: "\
<!DOCTYPE html>
<html lang=\"en\">
  <head>
    <title>mush</title>
  </head>
  <body>
    <!-- A comment, and an element that declares nothing. -->
    <div class=\"row\">text</div>
  </body>
</html>
",
            rows: &[],
            not_rows: &[
                "<!DOCTYPE html>",
                "  <head>",
                "    <!-- A comment, and an element that declares nothing. -->",
                "    <div class=\"row\">text</div>",
            ],
            note: "Markup declares nothing by this rule: a tag has no keyword, \
                   and every one opens with a character no word does.",
        },
        Sample {
            language: "JSON",
            text: "\
{
  \"name\": \"mush\",
  \"deps\": {
    \"serde\": \"1\"
  },
  \"features\": [\"outline\", \"usages\"]
}
",
            rows: &[],
            not_rows: &[
                "  \"name\": \"mush\",",
                "  \"deps\": {",
                "  \"features\": [\"outline\", \"usages\"]",
            ],
            note: "A data file's whole content is keys and values, and the name \
                   rule refuses every one of them: no rows is the answer this file \
                   is supposed to get, not a failure to find any.",
        },
        Sample {
            language: "YAML",
            text: "\
# The workspace's own config.
name: mush
root: /home/you/p/mush
tools:
  - outline
  - usages
",
            rows: &[],
            not_rows: &[
                "# The workspace's own config.",
                "name: mush",
                "tools:",
                "  - outline",
            ],
            note: "A YAML key and a Makefile target are the same shape, and \
                   refusing that shape is what keeps every line of this file out.",
        },
        Sample {
            language: "TOML",
            text: "\
[package]
name = \"mush\"
type = \"lib\"

[dependencies]
serde = { version = \"1\", features = [\"derive\"] }
",
            rows: &[],
            not_rows: &[
                "[package]",
                "name = \"mush\"",
                "type = \"lib\"",
                "serde = { version = \"1\", features = [\"derive\"] }",
            ],
            note: "Tables and key-value pairs: the keyword has to be followed by a \
                   name, and a TOML line's name is followed by `=`.",
        },
        Sample {
            language: "Markdown",
            text: "\
# mush

A TUI that runs agents. It reads lines, so this file
answers no rows.

## Tools

- `outline` sketches a file
- `usages` names a symbol
",
            rows: &[],
            not_rows: &[
                "# mush",
                "## Tools",
                "- `outline` sketches a file",
                "answers no rows.",
            ],
            note: "Headings are comment lines and prose is prose; a fenced code \
                   line that *opens* like a declaration is a row, which the \
                   workspace road's own test pins.",
        },
        Sample {
            language: "JSX",
            text: "\
function Item({ id }) {
  return <li key={id}>{id}</li>;
}

export default function Toggle({ label }) {
  const [on, setOn] = useState(false);
  return <button aria-pressed={on}>{label}</button>;
}
",
            rows: &[
                "function Item({ id }) {",
                "export default function Toggle({ label }) {",
            ],
            not_rows: &[
                "  return <li key={id}>{id}</li>;",
                "  const [on, setOn] = useState(false);",
                "  return <button aria-pressed={on}>{label}</button>;",
            ],
            note: "A destructuring binding (`const [on, setOn] = …`) is a miss: \
                   the name rule wants a letter or `_` after the keyword and `[` \
                   is neither, so the component lines are the rows.",
        },
        Sample {
            language: "TSX",
            text: "\
interface Props {
  label: string;
}

export function Toggle({ label }: Props) {
  const [on, setOn] = useState(false);
  return <button aria-pressed={on}>{label}</button>;
}
",
            rows: &[
                "interface Props {",
                "export function Toggle({ label }: Props) {",
            ],
            not_rows: &[
                "  label: string;",
                "  const [on, setOn] = useState(false);",
                "  return <button aria-pressed={on}>{label}</button>;",
            ],
            note: "",
        },
        Sample {
            language: "PHP",
            text: "\
<?php
namespace App\\Http;

final class Router {
    public function handle(Request $request): Response {
        return new Response('ok');
    }
}
",
            rows: &[
                "namespace App\\Http;",
                "final class Router {",
                "    public function handle(Request $request): Response {",
            ],
            not_rows: &["<?php", "        return new Response('ok');"],
            note: "",
        },
        Sample {
            language: "Swift",
            text: "\
protocol Drawable {
    func draw() -> String
}

extension String: Drawable {
    func draw() -> String { self }
}

let greeting = \"hello\"
",
            rows: &[
                "protocol Drawable {",
                "    func draw() -> String",
                "extension String: Drawable {",
                "    func draw() -> String { self }",
            ],
            not_rows: &["let greeting = \"hello\""],
            note: "A `func` line is a row even when its return type is not `void`: \
                   `func` opens the line, unlike Java's `public int compute()`, \
                   whose first word is the type.",
        },
        Sample {
            language: "Kotlin",
            text: "\
fun main(args: Array<String>) {
    println(args.size)
}

data class User(val name: String)
",
            rows: &[
                "fun main(args: Array<String>) {",
                "data class User(val name: String)",
            ],
            not_rows: &["    println(args.size)"],
            note: "",
        },
        Sample {
            language: "Scala",
            text: "\
package com.example.app
object Main {
  def main(args: Array[String]): Unit = {
    println(args.length)
  }
}

case class User(name: String)
",
            rows: &[
                "package com.example.app",
                "object Main {",
                "  def main(args: Array[String]): Unit = {",
                "case class User(name: String)",
            ],
            not_rows: &["    println(args.length)"],
            note: "",
        },
        Sample {
            language: "Dart",
            text: "\
import 'dart:math';
mixin HasArea {
  double area();
}

class Circle with HasArea {
  double area() => pi;
}
",
            rows: &["mixin HasArea {", "class Circle with HasArea {"],
            not_rows: &[
                "import 'dart:math';",
                "  double area();",
                "  double area() => pi;",
            ],
            note: "A method whose return type is not `void` (`double area()`) is \
                   the miss the module doc names; the `mixin` and `class` lines \
                   are the rows.",
        },
        Sample {
            language: "Julia",
            text: "\
module Geometry
struct Circle
    radius::Float64
end

function area(c::Circle)
    pi * c.radius^2
end
",
            rows: &[
                "module Geometry",
                "struct Circle",
                "function area(c::Circle)",
            ],
            not_rows: &["    radius::Float64", "    pi * c.radius^2"],
            note: "",
        },
        Sample {
            language: "Lua",
            text: "\
-- A tiny module.
local M = {}

function M.new(name)
  return setmetatable({}, { __index = M })
end

local function greet(name) return \"hello \" .. name end
",
            rows: &[
                "function M.new(name)",
                "local function greet(name) return \"hello \" .. name end",
            ],
            not_rows: &[
                "-- A tiny module.",
                "local M = {}",
                "  return setmetatable({}, { __index = M })",
            ],
            note: "",
        },
        Sample {
            language: "Perl",
            text: "\
package Greeting;
use strict;

sub new {
    my ($class, %args) = @_;
    return bless { %args }, $class;
}
",
            rows: &["package Greeting;", "sub new {"],
            not_rows: &[
                "use strict;",
                "    my ($class, %args) = @_;",
                "    return bless { %args }, $class;",
            ],
            note: "",
        },
        Sample {
            language: "Elixir",
            text: "\
defmodule Router do
  @type conn :: map()
  def handle(conn) do
    parse(conn)
  end

  defp parse(conn), do: conn
end
",
            rows: &[
                "defmodule Router do",
                "  @type conn :: map()",
                "  def handle(conn) do",
                "  defp parse(conn), do: conn",
            ],
            not_rows: &["    parse(conn)", "  end"],
            note: "",
        },
        Sample {
            language: "Erlang",
            text: "\
-module(router).
-export([parse/1]).

-spec parse(binary()) -> map().
parse(X) ->
    #{path => X}.
",
            rows: &[],
            not_rows: &[
                "-module(router).",
                "-spec parse(binary()) -> map().",
                "parse(X) ->",
                "    #{path => X}.",
            ],
            note: "Erlang's own declarations open with a `-` (`-module`, \
                   `-type`) or with a bare name and `->` (`parse(X) ->`), so \
                   this sample answers no rows; that is the module doc's named \
                   miss, not a hole.",
        },
        Sample {
            language: "F#",
            text: "\
namespace App
module Shapes =
    type Shape =
        | Circle of float

    let area shape =
        match shape with
        | Circle r -> 3.14 * r * r
",
            rows: &["namespace App", "module Shapes =", "    type Shape ="],
            not_rows: &[
                "        | Circle of float",
                "    let area shape =",
                "        match shape with",
            ],
            note: "",
        },
        Sample {
            language: "OCaml",
            text: "\
(* A tiny shape module. *)
type shape =
  | Circle of float
  | Square of float

module Shapes = struct
  let count = 2
end
",
            rows: &["type shape =", "module Shapes = struct"],
            not_rows: &[
                "(* A tiny shape module. *)",
                "  | Circle of float",
                "  let count = 2",
            ],
            note: "",
        },
        Sample {
            language: "Groovy",
            text: "\
package app
class Greeter {
    String who = 'world'

    def greet() {
        return \"hello ${who}\"
    }
}
",
            rows: &["package app", "class Greeter {", "    def greet() {"],
            not_rows: &[
                "    String who = 'world'",
                "        return \"hello ${who}\"",
            ],
            note: "",
        },
        Sample {
            language: "Bash/sh",
            text: "\
#!/usr/bin/env bash
build() {
  make release
}

function rotate {
  find /var/backups -mtime +7 -delete
}
",
            rows: &["function rotate {"],
            not_rows: &[
                "#!/usr/bin/env bash",
                "build() {",
                "  make release",
                "  find /var/backups -mtime +7 -delete",
            ],
            note: "",
        },
        Sample {
            language: "PowerShell",
            text: "\
#Requires -Version 5.1

function Get-Report {
    Get-Content $Path | Measure-Object -Line
}
class Report {
    [string]$Name
}
",
            rows: &["function Get-Report {", "class Report {"],
            not_rows: &[
                "#Requires -Version 5.1",
                "    Get-Content $Path | Measure-Object -Line",
                "    [string]$Name",
            ],
            note: "",
        },
        Sample {
            language: "CSS/SCSS",
            text: "\
:root { --gap: 8px; }

@mixin flex($dir: row) {
  display: flex;
  flex-direction: $dir;
}

@function double($n) { @return $n * 2; }
",
            rows: &[
                "@mixin flex($dir: row) {",
                "@function double($n) { @return $n * 2; }",
            ],
            not_rows: &[
                ":root { --gap: 8px; }",
                "  display: flex;",
                "  flex-direction: $dir;",
            ],
            note: "",
        },
        Sample {
            language: "Vue",
            text: "\
<template><p>{{ message }}</p></template>
<script>
export default { name: \"Greeting\" };
</script>
<script setup>
const message = \"hello\";
function greet(n) { return n; }
</script>
",
            rows: &[
                "const message = \"hello\";",
                "function greet(n) { return n; }",
            ],
            not_rows: &[
                "<template><p>{{ message }}</p></template>",
                "export default { name: \"Greeting\" };",
            ],
            note: "",
        },
        Sample {
            language: "Svelte",
            text: "\
<script>
  export let count = 0;

  function bump() {
    count += 1;
  }
</script>
<button on:click={bump}>{count}</button>
",
            rows: &["  function bump() {"],
            not_rows: &[
                "  export let count = 0;",
                "    count += 1;",
                "<button on:click={bump}>{count}</button>",
            ],
            note: "",
        },
        Sample {
            language: "XML",
            text: "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<project xmlns=\"http://maven.apache.org/POM/4.0.0\">
  <!-- The build declares nothing this rule reads. -->
  <artifactId>mush-core</artifactId>
  <version>0.1.0</version>
</project>
",
            rows: &[],
            not_rows: &[
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
                "  <!-- The build declares nothing this rule reads. -->",
                "  <artifactId>mush-core</artifactId>",
            ],
            note: "XML declares nothing by this rule: an element, an attribute \
                   and a comment all open with characters no declaration word \
                   does, so the empty answer is the honest one.",
        },
        Sample {
            language: "Zig",
            text: "\
const std = @import(\"std\");

pub fn parse(allocator: Allocator, src: []const u8) !Ast {
    return try std.zig.parse(allocator, src);
}

pub const Token = enum { ident, number, eof };
",
            rows: &[
                "const std = @import(\"std\");",
                "pub fn parse(allocator: Allocator, src: []const u8) !Ast {",
                "pub const Token = enum { ident, number, eof };",
            ],
            not_rows: &["    return try std.zig.parse(allocator, src);"],
            note: "",
        },
        Sample {
            language: "Nim",
            text: "\
# A tiny geometry module.
import std/math

type Point = object
  x, y: float

proc length(p: Point): float =
  sqrt(p.x * p.x + p.y * p.y)
",
            rows: &["type Point = object", "proc length(p: Point): float ="],
            not_rows: &[
                "# A tiny geometry module.",
                "import std/math",
                "  x, y: float",
            ],
            note: "",
        },
        Sample {
            language: "Crystal",
            text: "\
require \"json\"

module Greeter
  def greet(name : String) : String
    \"hello #{name}\"
  end
end
",
            rows: &["module Greeter", "  def greet(name : String) : String"],
            not_rows: &["require \"json\"", "    \"hello #{name}\""],
            note: "",
        },
        Sample {
            language: "Solidity",
            text: "\
pragma solidity ^0.8.20;

contract Token {
    uint256 public totalSupply;

    function transfer(address to, uint256 amount) public {
        totalSupply -= amount;
    }
}
",
            rows: &[
                "contract Token {",
                "    function transfer(address to, uint256 amount) public {",
            ],
            not_rows: &[
                "pragma solidity ^0.8.20;",
                "    uint256 public totalSupply;",
                "        totalSupply -= amount;",
            ],
            note: "A state variable (`uint256 public totalSupply;`) is a miss: \
                   the line opens with a type name, and the rule cannot tell one \
                   from any other word. `contract` and `function` open the rows.",
        },
        Sample {
            language: "Ada",
            text: "\
-- A stack of integers.
package body Stacks is

   procedure Push (X : Integer) is
   begin
      null;
   end Push;
end Stacks;
",
            rows: &[
                "package body Stacks is",
                "   procedure Push (X : Integer) is",
            ],
            not_rows: &[
                "-- A stack of integers.",
                "   begin",
                "      null;",
                "   end Push;",
            ],
            note: "",
        },
        Sample {
            language: "Fortran",
            text: "\
module geometry
  type :: point
    real :: x, y
  end type point
contains
  subroutine fill_array(n, a)
  end subroutine fill_array
end module geometry
",
            rows: &["module geometry", "  subroutine fill_array(n, a)"],
            not_rows: &[
                "  type :: point",
                "    real :: x, y",
                "  end subroutine fill_array",
            ],
            note: "Fortran's `type :: point` is a miss: the rule wants a name \
                   after the keyword and the line spells `::`. The `module` and \
                   `subroutine` lines are the rows.",
        },
        Sample {
            language: "MATLAB",
            text: "\
% Compute a weighted mean.
function m = mean_of(v, w)
    assert(numel(v) == numel(w));
    m = sum(v .* w) / sum(w);
end
",
            rows: &["function m = mean_of(v, w)"],
            not_rows: &[
                "% Compute a weighted mean.",
                "    assert(numel(v) == numel(w));",
                "    m = sum(v .* w) / sum(w);",
            ],
            note: "",
        },
        Sample {
            language: "Assembly",
            text: "\
default rel

section .text
global _start

_start:
    mov rax, 60
    syscall
",
            rows: &[],
            not_rows: &[
                "default rel",
                "section .text",
                "global _start",
                "_start:",
                "    mov rax, 60",
            ],
            note: "An assembly label (`_start:`) is a name and a colon — the \
                   shape of a YAML key — and every other line is a directive or \
                   an instruction, so no line of this file opens with a \
                   declaration word.",
        },
        Sample {
            language: "Makefile",
            text: "\
CC = gcc
CFLAGS = -O2 -Wall

build: main.o util.o
\t$(CC) $(CFLAGS) -o build main.o util.o

.PHONY: build
",
            rows: &[],
            not_rows: &[
                "CC = gcc",
                "build: main.o util.o",
                "\t$(CC) $(CFLAGS) -o build main.o util.o",
                ".PHONY: build",
            ],
            note: "A Makefile target (`build:`) is the same name-and-colon shape \
                   as a YAML key, and a variable assignment is a binding: no line \
                   of this file opens with a declaration word, and no rows is the \
                   answer.",
        },
        Sample {
            language: "Dockerfile",
            text: "\
FROM rust:1.80 AS build
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=build /app/target/release/mush /usr/local/bin/mush
ENTRYPOINT [\"mush\"]
",
            rows: &[],
            not_rows: &[
                "FROM rust:1.80 AS build",
                "RUN cargo build --release",
                "ENTRYPOINT [\"mush\"]",
            ],
            note: "Dockerfile's instructions (`FROM`, `RUN`, `COPY`, \
                   `ENTRYPOINT`) are its own words, none of them a declaration \
                   keyword, so the rule reads no line of this file.",
        },
        Sample {
            language: "Protobuf",
            text: "\
syntax = \"proto3\";
message User {
  string name = 1;
}

service Users {
  rpc GetUser (UserRequest) returns (User);
}
",
            rows: &[
                "message User {",
                "service Users {",
                "  rpc GetUser (UserRequest) returns (User);",
            ],
            not_rows: &["syntax = \"proto3\";", "  string name = 1;"],
            note: "",
        },
        Sample {
            language: "GraphQL",
            text: "\
type Query {
  user(id: ID!): User
}

union SearchResult = User | Post

fragment UserFields on User {
  id
}
",
            rows: &[
                "type Query {",
                "union SearchResult = User | Post",
                "fragment UserFields on User {",
            ],
            not_rows: &["  user(id: ID!): User", "  id"],
            note: "",
        },
        Sample {
            language: "Nix",
            text: "\
{
  description = \"mush, packaged\";

  outputs = { self, nixpkgs }: {
    packages.default = nixpkgs.legacyPackages.callPackage ./default.nix { };
  };
}
",
            rows: &[],
            not_rows: &[
                "  description = \"mush, packaged\";",
                "  outputs = { self, nixpkgs }: {",
                "    packages.default = nixpkgs.legacyPackages.callPackage ./default.nix { };",
            ],
            note: "A Nix file is bindings (`foo = { … };`) and function arguments \
                   (`{ pkgs, … }:`), the shapes the name rule refuses by name, so \
                   no rows is the answer this file is supposed to get.",
        },
        Sample {
            language: "Awk",
            text: "\
BEGIN { FS = \",\" }

{ total += $2 }

END { print total }

function max(a, b) {
    return a > b ? a : b
}
",
            rows: &["function max(a, b) {"],
            not_rows: &[
                "BEGIN { FS = \",\" }",
                "{ total += $2 }",
                "END { print total }",
                "    return a > b ? a : b",
            ],
            note: "",
        },
        Sample {
            language: "Tcl",
            text: "\
# A tiny Tcl module.
namespace eval ::shapes {
    proc area {w h} {
        expr {$w * $h}
    }
}

set area [area 3 4]
",
            rows: &["namespace eval ::shapes {", "    proc area {w h} {"],
            not_rows: &[
                "# A tiny Tcl module.",
                "        expr {$w * $h}",
                "set area [area 3 4]",
            ],
            note: "",
        },
        Sample {
            language: "Vim script",
            text: "\
\" A tiny plugin.
let g:loaded_tiny = 1

function! s:helper(name) abort
  return \"hello \" . a:name
endfunction

command! Tiny call s:helper(\"world\")
",
            rows: &["function! s:helper(name) abort"],
            not_rows: &[
                "\" A tiny plugin.",
                "let g:loaded_tiny = 1",
                "  return \"hello \" . a:name",
                "command! Tiny call s:helper(\"world\")",
            ],
            note: "",
        },
    ];

    /// The table, through the one rule: for every language, the rows are exactly
    /// the lines its entry names — no more, so a rule that read a comment, a
    /// call, a string or a data line as a declaration fails here, and no fewer,
    /// so a rule that went blind fails here too.
    ///
    /// The count is asserted because the coverage is the claim: fifty-odd
    /// languages is the floor the module doc names, and a table that loses an
    /// entry to a careless edit should fail here rather than go quiet. The
    /// names are checked for repeats because two entries for one language would
    /// be one entry and a hole.
    #[test]
    fn every_language_of_the_union_is_outlined_as_its_own_sample_says() {
        assert!(
            SAMPLES.len() >= 50,
            "the table answers {} languages",
            SAMPLES.len()
        );
        let mut names: Vec<&str> = SAMPLES.iter().map(|sample| sample.language).collect();
        names.sort_unstable();
        let listed = names.len();
        names.dedup();
        assert_eq!(names.len(), listed, "a language is listed twice");

        for sample in SAMPLES {
            let found = definitions(sample.text);
            let rows: Vec<&str> = found.iter().map(|row| row.text.as_str()).collect();
            assert_eq!(
                rows.as_slice(),
                sample.rows,
                "{}: the rows are not the lines the sample names",
                sample.language
            );
            let lines: Vec<&str> = sample.text.lines().collect();
            for definition in &found {
                assert_eq!(
                    lines[definition.line - 1],
                    definition.text,
                    "{}: a row is not its own line",
                    sample.language
                );
                assert!(
                    is_declaration(&definition.text),
                    "{}: a row that does not re-match the rule",
                    sample.language
                );
            }
            assert!(
                !sample.not_rows.is_empty(),
                "{}: a sample with nothing that must not be a row tests half the \
                 rule",
                sample.language
            );
            for line in sample.not_rows {
                assert!(
                    lines.contains(line),
                    "{}: {line:?} is not in the sample, so refusing it proves nothing",
                    sample.language
                );
                assert!(
                    !is_declaration(line),
                    "{}: {line:?} must not be a row",
                    sample.language
                );
            }
            assert!(
                !sample.rows.is_empty() || !sample.note.is_empty(),
                "{}: a language answered with no rows at all says why",
                sample.language
            );
        }
    }

    /// A file of another language that this repository really holds: the
    /// `scripts/*.py` a person wrote, not a sample written to pass. The table
    /// cannot prove this — its samples are written by the same hand as the rule
    /// — so the invariant is checked here against text nobody wrote for it: the
    /// rows ascend, each row is its file's line (cut where it was cut), each row
    /// re-matches the rule, and every line that opens Python's `def`, `class`
    /// or `async def` really is a row.
    #[test]
    fn a_python_file_of_this_checkout_is_outlined_by_the_same_rule() {
        let root = repo_root();
        let mut files = Vec::new();
        files_with_extension(&root.join("scripts"), "py", &mut files);
        files.sort();
        assert!(files.len() >= 5, "the sweep found {} files", files.len());

        let mut rows = 0usize;
        let mut declarations = 0usize;
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
            // The shape a Python file writes its own members with: every one of
            // them is a row, or the union has a hole where a language says it
            // does not.
            for line in &lines {
                let trimmed = line.trim_start();
                if ["def ", "class ", "async def "]
                    .iter()
                    .any(|opening| trimmed.starts_with(opening))
                {
                    declarations += 1;
                    assert!(
                        is_declaration(line),
                        "{}: {line:?} is a Python declaration and not a row",
                        path.display()
                    );
                }
            }
        }
        assert!(rows > 50, "the Python sweep found only {rows} rows");
        assert!(
            declarations > 50,
            "the Python sweep found only {declarations} defs and classes"
        );
    }

    /// A file of no language at all: a note in prose, on disk, and this
    /// checkout's own `Cargo.toml` — whose every line is a key and a value —
    /// read from disk like any other file. The answer is no rows, and the
    /// sentence that still names `read_file`: a file with nothing to declare is
    /// a normal file, not a failure to read one.
    #[test]
    fn a_file_of_no_language_answers_no_rows() {
        let dir = std::env::temp_dir().join(format!("mush-outline-notes-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let note = dir.join("notes.txt");
        fs::write(
            &note,
            "# Notes\n\nThe rule reads lines, one rule for every language.\n\n- a bullet\n- another\n",
        )
        .unwrap();
        for path in [&note, &repo_root().join("Cargo.toml")] {
            let text = fs::read_to_string(path).unwrap();
            let outline = Outline::of(&path.display().to_string(), &text);
            assert!(
                outline.is_empty(),
                "{}: {} rows in a file that declares nothing: {:?}",
                path.display(),
                outline.definitions().len(),
                outline.definitions()
            );
            let answer = outline.render(4_000, "");
            assert!(answer.contains("no definitions"), "{}", answer);
            assert!(answer.contains("read_file"), "{}", answer);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// The checkout this crate is a workspace member of: the tests that walk
    /// real files all need the same two steps, so they share one.
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("the crate sits under <repo>/crates/mush-core")
            .to_path_buf()
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

    /// The same invariant over the whole table at once: whatever a sample's row
    /// is, padding it past [`ROW_WIDTH`] forces the cut, and the cut row still
    /// re-matches the rule. It is the *offset* under test — a row cut before the
    /// name's first byte would stop being a declaration — and the padding makes
    /// every row of every language prove that it is not.
    #[test]
    fn every_cut_row_of_every_language_still_re_matches_the_rule() {
        for sample in SAMPLES {
            for row in sample.rows {
                let padded = format!("{row}{}", " x".repeat(ROW_WIDTH));
                let cut = row_text(&padded).expect("a padded row is still a declaration");
                assert!(
                    cut.ends_with('…'),
                    "{}: {row:?} was not cut",
                    sample.language
                );
                assert!(
                    padded.starts_with(cut.trim_end_matches('…')),
                    "{}: the row is not that line, cut: {cut:?}",
                    sample.language
                );
                assert!(
                    is_declaration(&cut),
                    "{}: a cut row may never stop being the declaration it claims: \
                     {cut:?}",
                    sample.language
                );
            }
        }
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
            "NOTES.md — 3 lines; no definitions (textual, many languages, best-effort — not a \
             compiler's answer); read_file shows the text"
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
            "src/big.rs — 120 lines; 120 definitions (textual, many languages, best-effort — \
             not a compiler's answer)"
        );
        let one = Outline::of("one.rs", "fn only() {}\n");
        assert_eq!(
            one.header(),
            "one.rs — 1 line; 1 definition (textual, many languages, best-effort — not a \
             compiler's answer)"
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

    /// The two edges under a cap too small for even one row. A header plus the
    /// room the cut note needs still says what it left — with the count of rows
    /// it showed, which is zero — and a cap too small even for that is the
    /// header alone, cut and marked as cut rather than handed over as the whole
    /// sketch. The app's caps are far above both; this is a public function's
    /// contract, pinned because any caller can ask for any cap at all.
    #[test]
    fn a_cap_too_small_for_a_row_still_says_what_it_left() {
        let outline = Outline::of("two.rs", "fn a() {}\nfn b() {}\n");
        let header = outline.header();
        // Inside the window: too small for the header, a row and the note's
        // reserve, roomy enough for the header and the note itself.
        let narrow = outline.render(header.len() + CUT_RESERVE - 20, "");
        assert!(narrow.starts_with(&header), "{narrow:?}");
        assert!(
            narrow.contains("[mush: only the first 0 of 2 definitions are shown"),
            "a header with no rows under it says why: {narrow:?}"
        );
        assert!(
            !narrow.contains("  fn a"),
            "the rows really were left: {narrow:?}"
        );

        // Too small for the note as well: the header is the answer, and the cut
        // is the one `truncate_for_model` marks.
        let tiny = outline.render(header.len() - 20, "");
        assert!(tiny.starts_with("two.rs — 2 lines"), "{tiny:?}");
        assert!(tiny.contains("truncated at"), "the cut is marked: {tiny:?}");
    }

    /// A million declarations in one file — the generated file, the
    /// minified-ish header, the giant table — and the answer it gets. The walk
    /// counts every line's declaration, so the header's count is the file's own
    /// and not an estimate; the rows it keeps are the room's and the proof row,
    /// so the file's size never reaches the answer's memory; and the painted
    /// answer says what it left, which is the only thing that makes a bounded
    /// row list honest.
    ///
    /// The string is built by repeating one line, so what this test spends is
    /// the walk — a million lines under the rule — and not the fixture.
    #[test]
    fn a_million_declarations_are_counted_and_not_kept() {
        let text = "fn a() {}\n".repeat(1_000_000);
        let started = Instant::now();
        let outline = Outline::within("huge.rs", &text, DEFAULT_ROOM);
        let walked = started.elapsed();

        // The count is the file's own, verified line by line rather than
        // extrapolated from what was built.
        assert_eq!(outline.total(), 1_000_000);
        assert_eq!(
            outline.definitions().len(),
            DEFAULT_ROOM + 1,
            "the room's rows and the one that proves there were more"
        );
        assert!(outline
            .header()
            .starts_with("huge.rs — 1,000,000 lines; 1,000,000 definitions"));

        // The painted answer: the cap's rows, all of them kept ones, and the
        // sentence that names the file the answer is not.
        let answer = outline.render(crate::CMD_CAP, "");
        let shown = answer.lines().filter(|line| line.starts_with("  ")).count();
        assert!(shown > 0 && shown <= DEFAULT_ROOM, "{shown} rows shown");
        assert!(answer.len() <= crate::CMD_CAP);
        assert!(
            answer.contains(&format!(
                "only the first {shown} of 1000000 definitions are shown"
            )),
            "a bounded answer says what it left: {answer}"
        );
        assert!(answer.contains("  1  fn a() {}"), "{answer}");
        assert!(answer.ends_with("any row's line]"), "{answer}");

        // The memory the rows hold: each row's own text plus the row itself,
        // which is the shape of the cost the room exists to bound. The file is
        // ten megabytes and the rows are tens of kilobytes — two orders of
        // magnitude — and the bound is asserted so a walk that ever goes back
        // to building every row is caught here rather than in a report.
        let held: usize = outline
            .definitions()
            .iter()
            .map(|row| row.text.len() + std::mem::size_of::<Definition>())
            .sum();
        assert!(
            text.len() > held * 100,
            "the rows hold {held} bytes of a {} byte file",
            text.len()
        );
        // The walk's own cost is one pass: a rule that goes quadratic shows up
        // here, and the number is worth seeing in a `--nocapture` run.
        eprintln!("million-line outline: {walked:?}, {held} bytes of rows kept");
        assert!(walked < Duration::from_secs(10), "the walk took {walked:?}");
    }

    /// The answer a small file always gave, byte for byte: the header, the
    /// blank line, the rows, and nothing else — no room, no note, nothing
    /// between the caller and the sketch it asked for. A bound has to be
    /// invisible until it bites, and this is the test that says it is.
    #[test]
    fn a_small_file_answers_the_bytes_it_always_did() {
        let text = "// a heading\nfn first() {\n}\n\npub struct Second;\n";
        let outline = Outline::of("src/small.rs", text);
        assert_eq!(
            outline.render(4_000, ""),
            "src/small.rs — 5 lines; 2 definitions (textual, many languages, best-effort — not a \
             compiler's answer)\n\n  2  fn first() {\n  5  pub struct Second;"
        );
        assert_eq!(
            outline.definitions().len(),
            outline.total(),
            "nothing was left out of a file the room covers"
        );
    }

    /// The row past the room is the proof, and only the proof. A file holding
    /// exactly the room is whole and the list says so (`len == total`); a file
    /// holding more keeps one more than the room, and the list being longer than
    /// the room *is* the evidence that the file was; and the answer shows the
    /// room and never the proof row, whatever cap it is given.
    #[test]
    fn the_row_past_the_room_is_the_proof_the_file_held_more() {
        let exactly: String = (1..=10).map(|n| format!("fn item_{n}() {{}}\n")).collect();
        let more: String = (1..=1_000)
            .map(|n| format!("fn item_{n}() {{}}\n"))
            .collect();

        let whole = Outline::within("ten.rs", &exactly, 10);
        assert_eq!(whole.definitions().len(), 10, "the file, and no proof row");
        assert_eq!(whole.definitions().len(), whole.total());

        let bounded = Outline::within("many.rs", &more, 10);
        assert_eq!(bounded.definitions().len(), 11, "the room, and the proof");
        assert_eq!(bounded.total(), 1_000);
        assert!(
            bounded.definitions().len() > 10,
            "a list longer than the room is the proof the room was not the file"
        );

        // A cap that would pay for every row kept still shows the room: the
        // proof row is not a row of any answer.
        let answer = bounded.render(1_000_000, "");
        let shown = answer.lines().filter(|line| line.starts_with("  ")).count();
        assert_eq!(shown, 10, "{answer}");
        assert!(answer.contains("  10  fn item_10() {}"), "{answer}");
        assert!(
            !answer.contains("fn item_11()"),
            "the proof is not shown: {answer}"
        );
        assert!(
            answer.contains("only the first 10 of 1000 definitions are shown"),
            "{answer}"
        );

        // Zero room still proves: one row is built, none is shown, and the
        // sentence carries the count of both sides.
        let none = Outline::within("none.rs", &more, 0);
        assert_eq!(none.definitions().len(), 1);
        assert_eq!(none.total(), 1_000);
        let answer = none.render(1_000_000, "");
        assert!(
            answer.contains("only the first 0 of 1000 definitions are shown"),
            "{answer}"
        );
        assert!(!answer.contains("  fn item_1"), "{answer}");

        // A room past the file is the whole file, and no note is owed.
        let roomy = Outline::within("roomy.rs", &more, usize::MAX);
        assert_eq!(roomy.definitions().len(), 1_000);
        assert_eq!(roomy.total(), 1_000);
        let answer = roomy.render(1_000_000, "");
        assert!(!answer.contains("[mush: only the first"), "{answer}");
    }

    /// The property a bounded row list must keep, over a table of shapes rather
    /// than one file: for every room and every cap, every row an answer paints
    /// is a row it kept, that row is its file's own line and re-matches the rule
    /// it came from, rows ascend, the header's count is the file's count, and an
    /// answer with no closing note is the whole sketch — no row left out and
    /// none claimed.
    #[test]
    fn a_bounded_answer_claims_no_row_it_did_not_keep() {
        let texts = [
            String::from("fn one() {}\nlet a = 1;\nfn two() {}\n"),
            String::new(),
            String::from("no declarations here\njust prose\n"),
            (1..=50)
                .map(|n| format!("fn item_{n}() {{ let body = \"{}\"; }}\n", "x".repeat(120)))
                .collect(),
            (1..=500).map(|n| format!("fn item_{n}() {{}}\n")).collect(),
        ];
        let rooms = [0usize, 1, 3, 40, DEFAULT_ROOM, usize::MAX];
        let caps = [700usize, 2_000, 4_000, 20_000];
        for text in &texts {
            let lines: Vec<&str> = text.lines().collect();
            let counted = definitions(text).len();
            for room in rooms {
                let outline = Outline::within("shape.rs", text, room);
                assert_eq!(outline.total(), counted, "the count is the file's own");
                assert_eq!(
                    outline.definitions().len(),
                    counted.min(room.saturating_add(1)),
                    "the kept rows are the room's and the proof"
                );
                assert!(outline.header().contains(&count(counted, "definition")));
                for cap in caps {
                    let answer = outline.render(cap, "");
                    let mut shown = 0usize;
                    let mut last = 0usize;
                    for line in answer.lines() {
                        let Some(rest) = line.strip_prefix("  ") else {
                            continue;
                        };
                        let (number, text) = rest.split_once("  ").expect("a row holds two spaces");
                        let number: usize = number.parse().expect("a row names a line");
                        shown += 1;
                        assert!(
                            number > last && number <= lines.len(),
                            "{number} after {last} in a file of {} lines",
                            lines.len()
                        );
                        last = number;
                        let body = text.strip_suffix('…').unwrap_or(text);
                        assert!(
                            lines[number - 1].starts_with(body),
                            "{number}: the row is not that line, cut: {text:?}"
                        );
                        assert!(
                            is_declaration(text),
                            "{number}: a row may never lie — {text:?} does not re-match the rule"
                        );
                    }
                    assert!(
                        shown <= outline.definitions().len(),
                        "an answer claims no row it did not keep: {shown} of {}",
                        outline.definitions().len()
                    );
                    assert!(shown <= room, "the room is what an answer may show");
                    assert!(
                        answer.len() <= cap + CUT_RESERVE,
                        "{} bytes for a cap of {cap}",
                        answer.len()
                    );
                    if answer.contains("[mush: only the first") {
                        assert!(
                            shown < counted,
                            "a cut answer is not the whole file's rows: {answer}"
                        );
                    } else {
                        assert_eq!(
                            shown, counted,
                            "an answer with no note is the whole sketch: {answer}"
                        );
                        assert_eq!(outline.definitions().len(), counted);
                    }
                }
            }
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
        let root = repo_root();
        let mut files = Vec::new();
        files_with_extension(&root.join("crates"), "rs", &mut files);
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
        eprintln!(
            "outline sweep: {} files, {rows} rows, {elapsed:?}",
            files.len()
        );
        assert!(rows > 500, "the sweep found only {rows} rows");
        assert!(
            elapsed < Duration::from_secs(2),
            "the sweep of {} files and {rows} rows took {elapsed:?}",
            files.len()
        );
    }

    /// Every file under `dir` whose extension is `extension`, depth-first, in
    /// name order — a handful of lines rather than a walker dependency, and the
    /// order is not the property's business (the sort in the caller is).
    fn files_with_extension(dir: &Path, extension: &str, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files_with_extension(&path, extension, out);
            } else if path.extension().is_some_and(|found| found == extension) {
                out.push(path);
            }
        }
    }
}
