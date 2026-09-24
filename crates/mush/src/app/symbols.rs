//! The one glyph table: the mark each tool's call wears, in two rungs.
//!
//! Every call's header row opens with a *mark* — one glyph and the space after
//! it — and the glyph says which tool was called before the name is read. The
//! mark is what the layout measures: the ask's own column starts after it, and
//! the details and the payload under the header stand at its width
//! ([`crate::app::call_grid`]).
//!
//! **Two rungs, one table.** A terminal that cannot show the symbols gets the
//! ascii rung instead: one letter or punctuation mark per tool, all of them
//! seven-bit, so a font or a locale that mangles `⧗` still shows *something*
//! for the wait. Which rung a pane starts on is the locale's answer
//! ([`Symbols::from_env`], read in exactly one place) and the human's to
//! overrule for the session with `/glyphs ascii` / `/glyphs symbols` — the
//! switch is a *view*, like `Ctrl-T` and `Ctrl-O`: not stored, not said into the
//! conversation, and the settings road the fold numbers are waiting for is its
//! future home. `/glyphs` alone opens [`Symbols::preview`]: the table itself, one
//! row per tool, so the human can check what their font does with it.
//!
//! **Every glyph is one column.** That is what lets a result message — which
//! the transcript does not let know which call produced it — stand its whole
//! block at the same two-column gutter ([`Symbols::GUTTER_MARK`], the pipe
//! every payload row wears), so a call still reads as one block. The tests
//! below are the pin: a glyph two columns wide would be caught there rather
//! than as a ragged gutter.
//!
//! **The table has no wildcard arm.** A new [`ToolName`] cannot compile until
//! it has been given a mark in both rungs, the same house rule the digest
//! follows; a name no tool answers to (the model can invent one) wears the
//! generic mark — today's `⚙`, and `#` on the ascii rung.

use mush_core::message::{FunctionCall, ToolCall};
use mush_core::tools::ToolName;

use crate::agent::{CallFacts, CallOutcome, Measure, Tone};
use crate::app::call_grid;

/// Which mark table a pane paints through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rung {
    /// The symbols: one glyph per tool, from the blocks this font has already
    /// been seen to render.
    Symbols,
    /// The ascii rung: one seven-bit mark per tool, for a terminal or a locale
    /// the symbols cannot travel through.
    Ascii,
}

impl Rung {
    /// The word the switch and its acknowledgement spell this rung with.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Rung::Symbols => "symbols",
            Rung::Ascii => "ascii",
        }
    }
}

/// The glyph table at one rung: the marks a pane paints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Symbols {
    rung: Rung,
}

/// The columns of the glyph-and-name field in [`Symbols::preview`], so the
/// example rows line up under each other: `spawn_agent` with its glyph and
/// space is the longest at thirteen.
const PREVIEW_HEAD: usize = 13;

/// The two columns between that field and the example row it leads.
const PREVIEW_GAP: usize = 2;

impl Symbols {
    /// The symbols rung: the one the locale usually answers with, and the stand-in
    /// every test that does not care about the rung paints through.
    ///
    /// A constant rather than a `symbols()` constructor: the type and the rung
    /// word are the same word, and a function named after its own type reads as a
    /// namespace (`clippy::self_named_constructors`).
    pub(crate) const SYMBOLS: Symbols = Symbols {
        rung: Rung::Symbols,
    };

    /// The ascii rung.
    pub(crate) const ASCII: Symbols = Symbols { rung: Rung::Ascii };

    /// The rung `word` names, for `/glyphs ascii` and `/glyphs symbols`: read
    /// letter-blind like the other words mush takes from a command line.
    pub(crate) fn parse(word: &str) -> Option<Self> {
        match word.to_ascii_lowercase().as_str() {
            "symbols" => Some(Self::SYMBOLS),
            "ascii" => Some(Self::ASCII),
            _ => None,
        }
    }

    /// The rung the pane opens on, read from the locale: the first non-empty of
    /// `LC_ALL`, `LC_CTYPE`, `LANG`, and the symbols rung for a value that names
    /// UTF-8. This is the **one place** the environment is read; the decision
    /// itself is [`Self::of_value`], a pure function of the value, so every
    /// locale a test can think of is a case there rather than an environment
    /// this file has to be run under.
    pub(crate) fn from_env() -> Self {
        let value = ["LC_ALL", "LC_CTYPE", "LANG"]
            .into_iter()
            .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()));
        Self::of_value(value.as_deref())
    }

    /// The rung a locale value asks for: `utf-8`/`utf8` anywhere in it
    /// (case-blind) is the symbols rung, and anything else — `C`, `POSIX`, a
    /// latin-1 locale, and a shell that exported no locale at all — is the
    /// ascii one, because a terminal whose encoding cannot be shown to carry
    /// UTF-8 is the terminal the symbols rung exists for.
    pub(crate) fn of_value(locale: Option<&str>) -> Self {
        let utf8 = locale.is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("utf-8") || value.contains("utf8")
        });
        if utf8 {
            Self::SYMBOLS
        } else {
            Self::ASCII
        }
    }

    /// This rung.
    pub(crate) fn rung(self) -> Rung {
        self.rung
    }

    /// The mark a call of `name` wears: the glyph and the space after it.
    ///
    /// One row of the table, both rungs, and no wildcard arm: a new tool has to
    /// be given a glyph for a font and a mark for a terminal without one before
    /// the crate builds again. A name no tool answers to wears the generic mark
    /// — the `⚙` the digest wore before the table, and `#` where even that is
    /// not safe.
    pub(crate) fn mark(self, name: &str) -> &'static str {
        let Some(tool) = ToolName::parse(name) else {
            return match self.rung {
                Rung::Symbols => "⚙ ",
                Rung::Ascii => "# ",
            };
        };
        let (symbols, ascii) = match tool {
            // A page with text lines: `▣`, the picture's mark, already renders
            // on the human's font.
            ToolName::ReadFile => ("▤ ", "R "),
            // Misc Symbols, the block `⚙` comes from: the trigram for earth is
            // three stacked lines, which is what an outline is. The ascii rung
            // takes the letter its own name starts with.
            ToolName::Outline => ("☷ ", "O "),
            // The pencil, from the Dingbats block `✓` and `✉` come from. The
            // Latin-1 `±` is `edit_file`'s: it is in every font, and it reads as
            // "a change" — the two pencils are near-identical at a glance and
            // must not be told apart by shape.
            ToolName::WriteFile => ("✎ ", "W "),
            ToolName::EditFile => ("± ", "E "),
            // The Misc-Symbols block `⚙` comes from.
            ToolName::ListFiles => ("☰ ", "L "),
            // Misc Technical: the one to eyeball, which is what `/glyphs` is for.
            ToolName::Search => ("⌕ ", "? "),
            // The ordinary TUI prompt glyph.
            ToolName::RunCommand => ("❯ ", "$ "),
            // Arrows: a child hangs off this.
            ToolName::SpawnAgent => ("↳ ", "+ "),
            // Geometric Shapes, and already mush's own running mark. The same
            // block `read_file`'s `▤` comes from.
            ToolName::Status => ("◐ ", "S "),
            // Arrows: steering, both ways.
            ToolName::Control => ("⇄ ", "! "),
            // Math Operators, and already mush's own waiting mark. The ascii
            // rung's `.` is its one-byte spelling — the human's list ended in
            // `…`, which no ascii rung can carry.
            ToolName::Wait => ("⧗ ", ". "),
        };
        match self.rung {
            Rung::Symbols => symbols,
            Rung::Ascii => ascii,
        }
    }

    /// The glyph alone: the mark without the space after it, for a surface that
    /// has its own spacing.
    pub(crate) fn glyph(self, name: &str) -> &'static str {
        self.mark(name).trim_end()
    }

    /// The gutter every row of a result's block stands at, in every rung: two
    /// columns, the mark's own width, with the pipe the pane uses for what a
    /// call said back.
    ///
    /// A `tool` message carries the call's id and not the tool's name, so a
    /// result cannot ask the table which glyph asked for it; the gutter is the
    /// one shape every mark shares (two columns, pinned by the test below). It
    /// is **not** a blank any more: a payload's rows wear this on
    /// every row — the first row's mark *is* it, and [`crate::app::chat`]'s
    /// fold hangs the wrapped rows and the `…` on the same pipe — so a dump is
    /// bound to the call that made it and can never be read as prose.
    pub(crate) const GUTTER_MARK: &'static str = "│ ";

    /// The pipe [`Self::GUTTER_MARK`] is made of, on its own: one character,
    /// read by a surface that pads a row by the pipe alone
    /// ([`crate::app::call_grid`]'s detail rows). Spelled here, beside the
    /// gutter it is the first column of, and pinned by the test below.
    pub(crate) const GUTTER_PIPE: char = '│';

    /// The columns [`Self::GUTTER_MARK`] and every mark take: one glyph and one
    /// space. See the module doc for why a constant is honest here.
    ///
    /// The tests' pin, not a production number: a row measures the mark it was
    /// handed rather than trusting a constant ([`crate::app::call_grid`]), so
    /// nothing outside a test has a reason to read this.
    #[cfg(test)]
    pub(crate) const GUTTER: usize = 2;

    /// The `/glyphs` preview: one *block* per tool — the glyph, the tool's own
    /// name and an example of the row it wears — so a glance at the human's
    /// terminal tells them what their font does with the table.
    ///
    /// `width` is the columns the reading surface gives one row — the popup's
    /// own measure ([`crate::app::screen::picker_text_width`]), not a width this
    /// file invents: the example is a row the *pane* would paint, so it is
    /// painted for the room it is shown in and a popup narrower than the picture
    /// shows a narrower pane rather than a clipped line.
    ///
    /// And it is a real call's rows: [`call_grid::header`] paints them through
    /// this rung's mark, so what the preview shows is the pane's own arithmetic
    /// and not a second picture of it. Where the grid stacks — the ask's column
    /// cannot hold the outcome beside it — the outcome's own row is shown too,
    /// under the head, with the head field left blank: the block reads as one
    /// call whatever the width, and a dropped row would make the narrow preview
    /// say the outcome does not exist.
    pub(crate) fn preview(self, width: usize) -> String {
        let row_width = width.saturating_sub(PREVIEW_HEAD + PREVIEW_GAP);
        let mut out = String::new();
        for tool in ToolName::ALL {
            let (ask, outcome, measure) = sample(tool);
            let call = ToolCall {
                id: "preview".to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: tool.as_str().to_string(),
                    arguments: "{}".to_string(),
                },
            };
            let facts = CallFacts {
                ask: ask.to_string(),
                cwd: None,
                outcome: outcome.map(|text| CallOutcome {
                    text: text.to_string(),
                    tone: Tone::Ok,
                }),
                measure: measure.map(|(count, size)| Measure {
                    count: count.map(str::to_string),
                    size: size.map(str::to_string),
                }),
                details: Vec::new(),
            };
            let head = format!("{} {}", self.glyph(tool.as_str()), tool.as_str());
            // One row per tool: the preview's blocks are told by their heads and
            // its rows line up under one field, which is what the font check is
            // for. An ask the row cannot hold is cut the way the compact log
            // cuts one — the unfold's extra rows belong to the transcript, not
            // to the popup.
            for (at, row) in call_grid::header(
                &call,
                &facts,
                row_width,
                self.mark(tool.as_str()),
                call_grid::AskRows::One,
            )
            .iter()
            .enumerate()
            {
                let row: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
                if at == 0 {
                    out.push_str(&format!("{head:<PREVIEW_HEAD$}  {row}\n"));
                } else {
                    // The stacked row: a field of its own width, left blank.
                    out.push_str(&format!("{:PREVIEW_HEAD$}  {row}\n", ""));
                }
            }
        }
        out.trim_end().to_string()
    }
}

/// One tool's `/glyphs` sample: the ask the example row wears, the verdict at
/// its arrow, and the measure at its right edge — the last two each optional,
/// and the measure itself a count and a size, each optional.
type Sample = (
    &'static str,
    Option<&'static str>,
    Option<(Option<&'static str>, Option<&'static str>)>,
);

/// The `/glyphs` sample for one tool: the ask the example row wears, the
/// verdict at its arrow and the measure at its right edge. The tools that steer
/// a run take no arguments, and their rows are the ones that say so; the tools
/// that produce a payload have no verdict and their example is the measure.
///
/// The measures are the pane's own arithmetic — `123L 4.1KB`, `37 defs` — and
/// the two numbers behind them (a count and a size) are a fixture's, not a
/// claim about the sample file.
fn sample(tool: ToolName) -> Sample {
    match tool {
        ToolName::ReadFile => (
            "crates/mush-core/src/text.rs 1408→1530",
            None,
            Some((Some("123L"), Some("4.1KB"))),
        ),
        ToolName::Outline => (
            "crates/mush-core/src/outline.rs",
            None,
            Some((Some("37 defs"), None)),
        ),
        ToolName::WriteFile => ("src/lex.rs", Some("41L → 3L"), None),
        ToolName::EditFile => ("src/lex.rs", Some("3 hunks"), None),
        ToolName::ListFiles => ("src", None, Some((Some("12 files"), Some("812B")))),
        ToolName::Search => (
            "\"markdown_rows\" in crates",
            None,
            Some((Some("7 hits"), Some("2.1KB"))),
        ),
        ToolName::RunCommand => (
            "cargo test -p mush",
            Some("5s"),
            Some((Some("41L"), Some("1.2KB"))),
        ),
        ToolName::SpawnAgent => ("table layout fixes", Some("#188 on mush/188"), None),
        ToolName::Status => ("", Some("3 agents · 1 job"), None),
        ToolName::Control => ("#4 message \"one more line\"", Some("#4 messaged"), None),
        ToolName::Wait => ("#2", Some("#2 done"), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    /// The table covers every tool, in both rungs: a `ToolName` without a mark
    /// cannot compile (the `match` has no wildcard), and this walks
    /// [`ToolName::ALL`] so a tool whose row is written but whose name is wrong
    /// is a failure rather than a call with the generic mark.
    #[test]
    fn every_tool_has_a_mark_in_both_rungs() {
        for rung in [Symbols::SYMBOLS, Symbols::ASCII] {
            for tool in ToolName::ALL {
                let mark = rung.mark(tool.as_str());
                assert!(!mark.trim_end().is_empty(), "{tool} at {:?}", rung.rung());
                assert_ne!(
                    mark.trim_end(),
                    rung.mark("no_such_tool").trim_end(),
                    "{tool} wears the generic mark at {:?}",
                    rung.rung()
                );
            }
            // The name the model can invent wears the generic mark, and the two
            // rungs spell it differently: `⚙` is a symbol, `#` is ascii.
            assert_eq!(
                rung.mark("teleport").trim_end(),
                match rung.rung() {
                    Rung::Symbols => "⚙",
                    Rung::Ascii => "#",
                }
            );
        }
    }

    /// Every mark is one glyph and one space, and every glyph is **one** pane
    /// column: the mark width is what the grid measures the ask against, and
    /// the block's gutter is the same width in a message that cannot know which
    /// call it belongs to.
    #[test]
    fn every_mark_is_the_gutter() {
        assert_eq!(
            UnicodeWidthStr::width(Symbols::GUTTER_MARK),
            Symbols::GUTTER
        );
        assert!(
            Symbols::GUTTER_MARK.starts_with(Symbols::GUTTER_PIPE),
            "the pipe is the gutter's first column: {:?}",
            Symbols::GUTTER_MARK
        );
        for rung in [Symbols::SYMBOLS, Symbols::ASCII] {
            for tool in ToolName::ALL {
                let mark = rung.mark(tool.as_str());
                assert!(
                    mark.ends_with(' '),
                    "{tool}'s mark carries the one space: {mark:?}"
                );
                assert_eq!(
                    UnicodeWidthStr::width(mark),
                    Symbols::GUTTER,
                    "{tool} at {:?}: {mark:?}",
                    rung.rung()
                );
                assert_eq!(
                    UnicodeWidthStr::width(rung.glyph(tool.as_str())),
                    Symbols::GUTTER - 1,
                    "{tool}'s glyph is one column"
                );
            }
        }
    }

    /// The ascii rung is seven-bit throughout — the one property the rung
    /// exists for — while the symbols rung may hold the glyphs the font has
    /// proven.
    #[test]
    fn the_ascii_rung_is_ascii() {
        for tool in ToolName::ALL {
            let glyph = Symbols::ASCII.glyph(tool.as_str());
            assert!(
                glyph.is_ascii(),
                "{tool}'s ascii glyph is not ascii: {glyph:?}"
            );
            assert_eq!(glyph.chars().count(), 1, "{tool}: {glyph:?}");
        }
    }

    /// The rung is the locale's answer: a UTF-8 value (however it is spelled
    /// and cased) is the symbols rung, and everything else — `C`, `POSIX`, a
    /// latin-1 locale, and a shell that exported nothing at all — is the ascii
    /// one.
    #[test]
    fn the_locale_chooses_the_rung() {
        let rung = |value: Option<&str>| Symbols::of_value(value).rung();
        assert_eq!(rung(Some("en_GB.UTF-8")), Rung::Symbols);
        assert_eq!(rung(Some("en_US.utf8")), Rung::Symbols);
        assert_eq!(rung(Some("UTF-8")), Rung::Symbols);
        assert_eq!(rung(Some("c.utf-8")), Rung::Symbols);
        assert_eq!(rung(Some("C")), Rung::Ascii);
        assert_eq!(rung(Some("POSIX")), Rung::Ascii);
        assert_eq!(rung(Some("de_DE.ISO-8859-1")), Rung::Ascii);
        assert_eq!(rung(None), Rung::Ascii);
        // The switch reads a word the way the rest of the command line does.
        assert_eq!(Symbols::parse("ASCII"), Some(Symbols::ASCII));
        assert_eq!(Symbols::parse("symbols"), Some(Symbols::SYMBOLS));
        assert_eq!(Symbols::parse("emoji"), None);
    }

    /// The preview is the human's font check: one *block* per tool, each naming
    /// its glyph, its tool and an example of the painted rows — and the example
    /// is the grid's own rows, arrow and all, not a second drawing of them. It
    /// is painted for the columns the popup gives a row, so no line is wider
    /// than the surface that shows it.
    #[test]
    fn the_preview_shows_every_tool_and_its_row() {
        // The columns a popup on a 133-column terminal gives one row. The narrow
        // case below is the 34 the 40-column floor gives.
        const WIDTH: usize = 73;
        for rung in [Symbols::SYMBOLS, Symbols::ASCII] {
            let preview = rung.preview(WIDTH);
            let lines: Vec<&str> = preview.lines().collect();
            for line in &lines {
                assert!(
                    UnicodeWidthStr::width(*line) <= WIDTH,
                    "a row wider than the surface: {line:?}"
                );
            }

            // One block per tool: a head row, and — where the grid stacks — the
            // outcome's own row under it, the head field blank. A block is told
            // by its head: the rows that do not open with the blank field.
            let mut blocks: Vec<Vec<&str>> = Vec::new();
            for line in &lines {
                if line.starts_with(' ') {
                    blocks
                        .last_mut()
                        .expect("a block for the stacked row to continue")
                        .push(line);
                } else {
                    blocks.push(vec![line]);
                }
            }
            assert_eq!(blocks.len(), ToolName::ALL.len(), "one block per tool");
            let mut heads: Vec<usize> = Vec::new();
            for (block, tool) in blocks.iter().zip(ToolName::ALL) {
                let name = tool.as_str();
                let head = block[0];
                assert!(
                    head.starts_with(rung.glyph(name)),
                    "{name}'s block opens with its glyph: {head:?}"
                );
                assert!(
                    head.contains(&format!("{}  ", name)),
                    "{name}'s block names the tool: {head:?}"
                );
                assert!(
                    head[Symbols::GUTTER + 1..].contains(rung.mark(name)),
                    "{name}'s block holds the painted row: {head:?}"
                );
                let whole = block.join("\n");
                assert!(
                    whole.contains('→'),
                    "{name}'s example carries its outcome, on one row or the two: {whole:?}"
                );
                // The outcome's own row stands under the head field, at the same
                // column the example rows all start at.
                if let Some(rest) = block.get(1) {
                    assert_eq!(
                        UnicodeWidthStr::width(&head[..head.find("  ").unwrap_or(head.len())]),
                        PREVIEW_HEAD,
                        "the field's own width: {head:?}"
                    );
                    assert!(
                        rest.starts_with(&" ".repeat(PREVIEW_HEAD + PREVIEW_GAP)),
                        "the stacked row stands under the example's column: {rest:?}"
                    );
                }
                heads.push(UnicodeWidthStr::width(
                    &head[..head
                        .rfind(rung.mark(name))
                        .expect("the painted row's own mark")],
                ));
            }
            // The example rows line up under each other: the painted row opens
            // at the same column in every block — the head field's own width plus
            // the two-space gap — and that column is the field's, not each
            // glyph's. Found rather than assumed: `rfind` takes the row's own
            // mark, the head's being the earlier one.
            assert!(
                heads.iter().all(|at| *at == PREVIEW_HEAD + PREVIEW_GAP),
                "one field width, the gap included: {heads:?}"
            );

            // The narrow surface — the popup at the 40-column floor: the field
            // and the gap still stand, the grid cannot hold the ask and the
            // outcome on one row, and the block says so instead of dropping the
            // outcome: every tool here has its second row.
            let narrow = rung.preview(34);
            let mut rows = 0;
            for line in narrow.lines() {
                rows += 1;
                assert!(
                    UnicodeWidthStr::width(line) <= 34,
                    "a narrow row is still a row that fits: {line:?}"
                );
            }
            assert_eq!(
                rows,
                ToolName::ALL.len() * 2,
                "at 34 columns every block is the ask and the outcome's own row:\n{narrow}"
            );
            assert!(
                narrow
                    .lines()
                    .any(|line| line.trim_start().starts_with("→")),
                "and the outcome's row is there, arrow and all:\n{narrow}"
            );
        }
    }
}
