//! The per-workspace hue: which colour a window's chrome wears, and where the
//! decision comes from.
//!
//! Two mush windows on two workspaces were indistinguishable — same borders,
//! same focus badge, same selected row. Every workspace now hashes its
//! absolute path to one of thirty named hues, and the chrome that used to be
//! `Color::Cyan` is painted in it by one rule: the hue *points* — at whose
//! window this is, or at where the keyboard is — and never *reports* what
//! happened. That rule yields every site, and this is the whole list: the
//! focused pane's border, the bar's badge, the message prompt, the selected
//! row of the list the keyboard is in (the agent tree's, a band while it has
//! the keyboard and the hue as ink while the chat does; the picker's), the
//! picker's frame, the transcript's select-mode cursor band and its selection,
//! and an activity line. The rest of the palette is *content* and stays fixed —
//! a failure is red in every workspace, the floor notice is yellow, dimmed text
//! is gray — because what happened reads the same wherever it is read; only
//! *whose window* this is wears the hue.
//!
//! The path is canonicalized before hashing, so two spellings of one directory
//! are one window in one colour, and hashed as given when it does not resolve,
//! because `--print-config` describes a workspace that is allowed not to exist
//! yet. The hash is FNV-1a 64, written out here rather than taken from
//! `DefaultHasher`: the standard hasher's output is deliberately not stable
//! across Rust releases, and a workspace whose colour changed when mush was
//! rebuilt would be the very confusion the hue exists to prevent.
//!
//! `MUSH_THEME` overrules all of it: a hue by name, `256` to demand the
//! indexed form even on a truecolor terminal, `off` for the fixed palette,
//! `auto` for the unset spelling. A name mush does not know is a startup error
//! naming it and listing the ones that work — the `MUSH_*` house rule
//! (`Overrides::from_env_checked`), because a typo must cost a message, not a
//! window in a colour nobody asked for.

use std::path::Path;

use ratatui::style::Color;

/// One of the thirty hues: the word a human types in `MUSH_THEME`, and the RGB
/// a truecolor terminal paints it with.
#[derive(Debug)]
pub(crate) struct Hue {
    pub(crate) name: &'static str,
    pub(crate) rgb: (u8, u8, u8),
}

impl Hue {
    /// One table row, spelled so the palette below reads as one hue per line:
    /// a `const fn` is the only form a `static` initializer accepts that
    /// rustfmt will not wrap into four lines per entry.
    const fn new(name: &'static str, rgb: (u8, u8, u8)) -> Self {
        Self { name, rgb }
    }
}

/// The thirty hues, in the order `hash % 30` indexes them.
///
/// Every one is in the L* 65–84 band, so it works as a background under the
/// `Color::Black` mush paints on the bar's badge and the selected rows, and no
/// two are closer than ΔE2000 ≈ 11.8 to each other: the whole point of thirty
/// is that a collision is rare *and* that two hues are told apart at a glance.
/// A test holds both ends of that promise (see `every_hue_fits_the_black_text_
/// band` and `no_two_hues_are_close_enough_to_confuse`).
pub(crate) static HUES: &[Hue] = &[
    Hue::new("blush", (0xFF, 0xAA, 0xAA)),
    Hue::new("coral", (0xF5, 0x80, 0x70)),
    Hue::new("peach", (0xFF, 0xBF, 0xA3)),
    Hue::new("apricot", (0xE5, 0x8A, 0x56)),
    Hue::new("amber", (0xFE, 0xB2, 0x69)),
    Hue::new("fawn", (0xC2, 0x99, 0x6A)),
    Hue::new("sand", (0xEA, 0xCB, 0x93)),
    Hue::new("mustard", (0xC4, 0xA8, 0x48)),
    Hue::new("butter", (0xD8, 0xD3, 0x6D)),
    Hue::new("olive", (0x9A, 0xA8, 0x45)),
    Hue::new("sage", (0xB1, 0xC6, 0x8C)),
    Hue::new("fern", (0x6C, 0xB1, 0x5E)),
    Hue::new("mint", (0x89, 0xE3, 0x95)),
    Hue::new("jade", (0x77, 0xBE, 0x9A)),
    Hue::new("emerald", (0x3B, 0xE7, 0xC0)),
    Hue::new("teal", (0x00, 0xB6, 0xAA)),
    Hue::new("turquoise", (0x77, 0xE0, 0xDD)),
    Hue::new("lagoon", (0x3D, 0xB0, 0xC2)),
    Hue::new("aqua", (0x03, 0xDC, 0xFF)),
    Hue::new("azure", (0x00, 0xAC, 0xE9)),
    Hue::new("sky", (0x8A, 0xCF, 0xFF)),
    Hue::new("cornflower", (0x63, 0xA1, 0xFD)),
    Hue::new("periwinkle", (0xA8, 0xBD, 0xFC)),
    Hue::new("iris", (0xA4, 0x94, 0xF1)),
    Hue::new("lilac", (0xD8, 0xC6, 0xFF)),
    Hue::new("orchid", (0xB9, 0x93, 0xC7)),
    Hue::new("fuchsia", (0xF5, 0xA7, 0xFC)),
    Hue::new("flamingo", (0xE8, 0x7E, 0xBD)),
    Hue::new("pink", (0xFF, 0xB8, 0xD9)),
    Hue::new("cherry", (0xF7, 0x78, 0x91)),
];

/// The environment the theme decision reads, each variable read exactly once
/// at the edge. The decision itself ([`Theme::resolve`]) is a pure function of
/// this value and a path, so a test hands it values instead of mutating the
/// process environment, and a run reads the environment once rather than
/// scattering `std::env::var` through the logic.
#[derive(Debug)]
pub(crate) struct EnvText {
    /// `MUSH_THEME`: a hue's name, `auto`, `256` or `off`. Unset, empty and
    /// `auto` are the same instruction — the workspace path chooses.
    pub(crate) theme: Option<String>,
    /// `COLORTERM`, where a terminal announces truecolor.
    pub(crate) colorterm: Option<String>,
    /// `TERM`, where the terminals that set no `COLORTERM` name the mode.
    pub(crate) term: Option<String>,
}

impl EnvText {
    /// Read the three variables from the process environment, once, at the
    /// start of a run.
    pub(crate) fn read() -> Self {
        Self {
            theme: env_nonempty("MUSH_THEME"),
            colorterm: env_nonempty("COLORTERM"),
            term: env_nonempty("TERM"),
        }
    }

    /// Whether the terminal says it paints 24-bit colour: `COLORTERM` equal to
    /// `truecolor` or `24bit` (the standard announcement, and what tmux and
    /// screen pass through from what is beneath them), or a `TERM` that names
    /// the mode itself — `xterm-direct`, `*-24bit`, `*-truecolor`. Both are
    /// compared case-insensitively, because terminals are not consistent about
    /// the spelling.
    fn truecolor(&self) -> bool {
        let equals = |value: &Option<String>, want: &str| {
            value
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case(want))
        };
        if equals(&self.colorterm, "truecolor") || equals(&self.colorterm, "24bit") {
            return true;
        }
        self.term.as_deref().is_some_and(|term| {
            let term = term.to_ascii_lowercase();
            ["truecolor", "24bit", "direct"]
                .iter()
                .any(|mode| term.contains(mode))
        })
    }
}

/// The environment variable this module reads, empty counting as unset like
/// every other `MUSH_*` reader.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// A workspace's painting identity: the hue, and the form it is painted in.
///
/// [`Theme::default`] is the *fixed* palette — exactly the look mush had
/// before hues existed — which is what keeps a caller that does not care about
/// hues, and every test that asserts painted text, painting the colours it
/// always painted.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    /// The one colour the chrome sites paint. The variant *is* the form, so
    /// nothing can describe the theme as truecolor while holding an index.
    accent: Color,
    /// The hue behind the accent, or `None` for the fixed palette.
    hue: Option<&'static Hue>,
    /// What chose the accent; the accent alone cannot say whether the human
    /// named the hue or the workspace path hashed it.
    origin: Origin,
}

/// Where a [`Theme`]'s accent came from.
#[derive(Clone, Copy, Debug)]
enum Origin {
    /// No hue: the fixed palette of [`Theme::default`], before any environment.
    Fixed,
    /// `MUSH_THEME=off`, the same palette stated.
    Off,
    /// The hue named by `MUSH_THEME=<name>`.
    Named,
    /// The hue hashed from the workspace path, the form being the terminal's
    /// answer.
    Workspace,
    /// The hue hashed from the workspace path, with `MUSH_THEME=256` demanding
    /// the indexed form.
    Workspace256,
}

/// How a hue is painted: the terminal's own bytes, or the nearest entry of its
/// 256-colour palette.
#[derive(Clone, Copy)]
enum Form {
    Rgb,
    Indexed,
}

/// The form a terminal's answer gives: its own bytes when it announced
/// truecolor, the nearest index otherwise.
fn form(truecolor: bool) -> Form {
    if truecolor {
        Form::Rgb
    } else {
        Form::Indexed
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: Color::Cyan,
            hue: None,
            origin: Origin::Fixed,
        }
    }
}

impl Theme {
    /// The colour the chrome sites paint: today's `Color::Cyan`, or the hue.
    pub(crate) fn accent(&self) -> Color {
        self.accent
    }

    /// The hue behind the accent, or `None` for the fixed palette.
    pub(crate) fn hue(&self) -> Option<&'static Hue> {
        self.hue
    }

    /// The one decision: `MUSH_THEME` chooses the hue and the terminal's
    /// capability the form, both read from `env`; with no `MUSH_THEME`, the
    /// workspace path chooses the hue.
    ///
    /// A value that is neither a hue's name nor `auto`/`256`/`off` is an error
    /// naming it and the names that work.
    pub(crate) fn resolve(env: &EnvText, root: &Path) -> Result<Self, String> {
        // Empty and surrounding whitespace count as silence, like the other
        // `MUSH_*` readers spend theirs.
        let stated = env
            .theme
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        Ok(match stated {
            // `off` is the fixed palette, and the spelling is kept: a human
            // debugging a window's colours has to know the environment
            // silenced the hue.
            Some("off") => Self {
                accent: Color::Cyan,
                hue: None,
                origin: Origin::Off,
            },
            // The workspace chooses the hue, and the human the form.
            Some("256") => Self::hued(hue_of(root), Form::Indexed, Origin::Workspace256),
            Some("auto") | None => {
                Self::hued(hue_of(root), form(env.truecolor()), Origin::Workspace)
            }
            Some(name) => {
                let hue = hue_by_name(name).ok_or_else(|| unknown_theme(name))?;
                Self::hued(hue, form(env.truecolor()), Origin::Named)
            }
        })
    }

    /// A theme whose accent is `hue` in `form`.
    fn hued(hue: &'static Hue, form: Form, origin: Origin) -> Self {
        let accent = match form {
            Form::Rgb => Color::Rgb(hue.rgb.0, hue.rgb.1, hue.rgb.2),
            Form::Indexed => Color::Indexed(nearest_256(hue.rgb)),
        };
        Self {
            accent,
            hue: Some(hue),
            origin,
        }
    }

    /// The `--print-config` value for this theme: the hue, the form it will be
    /// painted in, and where the hue came from. Every sentence is true for the
    /// terminal it was resolved on — an indexed theme never claims truecolor —
    /// because the dump exists to say what a window would look like.
    pub(crate) fn describe(&self) -> String {
        let Some(hue) = self.hue() else {
            return match self.origin {
                Origin::Off => "off (MUSH_THEME)".to_string(),
                _ => "none (fixed colours)".to_string(),
            };
        };
        let form = match self.accent {
            Color::Rgb(..) => "truecolor",
            _ => "indexed",
        };
        match self.origin {
            Origin::Named => format!("{} ({form}, MUSH_THEME)", hue.name),
            Origin::Workspace => format!("{} ({form}, from the workspace path)", hue.name),
            Origin::Workspace256 => format!(
                "{} ({form}, from the workspace path; MUSH_THEME=256)",
                hue.name
            ),
            // Both fixed origins returned above, with no hue to name.
            Origin::Fixed | Origin::Off => unreachable!("a hue-less theme has no hue"),
        }
    }
}

/// The hue a workspace path hashes to: canonicalized when it resolves, as
/// given when it does not.
fn hue_of(root: &Path) -> &'static Hue {
    let path = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    &HUES[(fnv1a_64(path.as_os_str().as_encoded_bytes()) % HUES.len() as u64) as usize]
}

/// FNV-1a 64: the offset basis, then each byte XORed in and the product taken
/// modulo 2^64 — the standard's arithmetic, `wrapping_mul` being that modulo.
///
/// Hand-written rather than `DefaultHasher` because the hue has to survive a
/// mush rebuild: the standard hasher is explicitly documented as unstable
/// across Rust releases, and a workspace whose colour changed on upgrade would
/// defeat the hue.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// The hue `MUSH_THEME=<name>` names, or `None`.
///
/// Case-sensitive: the names are the table's own lowercase words, and a second
/// spelling kept in step by hand is a second thing to drift.
fn hue_by_name(name: &str) -> Option<&'static Hue> {
    HUES.iter().find(|hue| hue.name == name)
}

/// The error an unknown `MUSH_THEME` value gets: the value, and every spelling
/// that would have worked. The list is built from the table, so a hue added or
/// renamed cannot be left out of the message.
fn unknown_theme(value: &str) -> String {
    let names: Vec<&str> = HUES.iter().map(|hue| hue.name).collect();
    format!(
        "MUSH_THEME: unknown theme `{value}` (try {} or auto, 256, off)",
        names.join(", ")
    )
}

/// The first xterm-256 entry the search visits: everything below is the
/// terminal's own palette, whose values are not mush's to know.
const FIRST_SEARCHED: u8 = 16;

/// The xterm-256 entry nearest `rgb`, by squared RGB distance.
///
/// The first sixteen entries are the terminal's own palette — their values are
/// terminal-dependent and they are meant to be chosen by name — so the search
/// runs over the entries from 16 up: the 6×6×6 colour cube, then the 24-step
/// gray ramp. Computed rather than tabled because the table would be 240 rows
/// of arithmetic the two formulas in [`xterm_rgb`] already say.
fn nearest_256(rgb: (u8, u8, u8)) -> u8 {
    let mut best = 0u8;
    let mut best_distance = u32::MAX;
    for offset in 0u8..240 {
        let distance = squared_distance(rgb, xterm_rgb(offset));
        if distance < best_distance {
            best_distance = distance;
            best = offset;
        }
    }
    // The search counts offsets from entry 16, so the answer sits that far
    // above the winning offset.
    FIRST_SEARCHED + best
}

/// The sum of the squared per-channel differences: cheap, and monotone in the
/// Euclidean distance, which is all a nearest entry needs.
fn squared_distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let channel = |x: u8, y: u8| {
        let delta = i32::from(x) - i32::from(y);
        (delta * delta) as u32
    };
    channel(a.0, b.0) + channel(a.1, b.1) + channel(a.2, b.2)
}

/// The RGB of the xterm-256 entry `offset` places after the start of the
/// searchable palette: the 6×6×6 cube for the first 216, then the 24-step gray
/// ramp.
///
/// The cube's six levels are `0, 95, 135, 175, 215, 255` — the first step is
/// bigger than the rest, which is not arithmetic but the standard — and the
/// ramp runs 8..=238 in steps of ten.
fn xterm_rgb(offset: u8) -> (u8, u8, u8) {
    if offset < 216 {
        let n = u16::from(offset);
        let level = |v: u16| if v == 0 { 0 } else { (55 + 40 * v) as u8 };
        (level(n / 36), level((n / 6) % 6), level(n % 6))
    } else {
        let gray = 8 + 10 * (offset - 216);
        (gray, gray, gray)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use mush_core::scratch::Scratch;

    /// One `EnvText` built by hand, the way the edge would have read it.
    fn text(theme: Option<&str>, colorterm: Option<&str>, term: Option<&str>) -> EnvText {
        EnvText {
            theme: theme.map(str::to_string),
            colorterm: colorterm.map(str::to_string),
            term: term.map(str::to_string),
        }
    }

    /// The environment of a plain truecolor terminal, `MUSH_THEME` unset.
    fn truecolor() -> EnvText {
        text(None, Some("truecolor"), None)
    }

    /// A path that does not resolve, so `hue_of` hashes it as given: the
    /// tests below are about the hash, not about the filesystem.
    fn nowhere() -> PathBuf {
        PathBuf::from("/nonexistent/mush/theme-test")
    }

    /// The hue of the one path every `MUSH_THEME`-driven test resolves for.
    fn nowhere_hue() -> &'static Hue {
        hue_of(&nowhere())
    }

    /// One hue's lightness, from its sRGB bytes, the test's own arithmetic so
    /// the band is checked against the standard and not against the palette:
    /// linearise each channel, weight it into Y, then the CIE L* of that.
    fn lightness(rgb: (u8, u8, u8)) -> f64 {
        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        let y = 0.2126 * channel(rgb.0) + 0.7152 * channel(rgb.1) + 0.0722 * channel(rgb.2);
        if y > 216.0 / 24389.0 {
            116.0 * y.cbrt() - 16.0
        } else {
            24389.0 / 27.0 * y
        }
    }

    /// The same path twice is the same hue, and the two spellings a human
    /// actually types of one directory are one hue too: the hash is of the
    /// canonical path, so `mkdir work && cd work/` cannot change the colour.
    #[test]
    fn two_spellings_of_one_directory_are_one_hue() {
        let dir = Scratch::new("theme");
        let hue = hue_of(&dir);
        assert!(std::ptr::eq(hue, hue_of(&dir)));
        assert!(std::ptr::eq(hue, hue_of(&dir.join("."))));
        // A path that does not canonicalize is hashed as given, and is still
        // stable: `--print-config` describes workspaces that may not exist yet.
        let missing = nowhere();
        assert!(std::ptr::eq(hue_of(&missing), hue_of(&missing)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Three hundred paths reach every one of the thirty hues, so the hash
    /// reads its input and spreads it: one bucket, or a handful, would mean
    /// the hue was a constant dressed up as a hash.
    #[test]
    fn the_hash_spreads_paths_over_the_hues() {
        let mut buckets = BTreeSet::new();
        for i in 0..300 {
            let path = PathBuf::from(format!("/nonexistent/mush/workspace-{i}"));
            buckets.insert(hue_of(&path).name);
        }
        assert_eq!(
            buckets.len(),
            HUES.len(),
            "300 paths reached {} of {} hues: {buckets:?}",
            buckets.len(),
            HUES.len()
        );
    }

    /// The names are what a human types in `MUSH_THEME=<name>`: lowercase
    /// ASCII, one word, no collisions with each other or with the three
    /// reserved spellings.
    #[test]
    fn every_hue_name_is_typeable() {
        let mut names = BTreeSet::new();
        for hue in HUES {
            assert!(
                !hue.name.is_empty()
                    && hue
                        .name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                "`{}` is not a typeable name",
                hue.name
            );
            assert!(
                names.insert(hue.name),
                "`{}` is twice in the table",
                hue.name
            );
            assert!(hue_by_name(hue.name).is_some(), "{}", hue.name);
        }
        for reserved in ["auto", "256", "off"] {
            assert!(
                hue_by_name(reserved).is_none(),
                "`{reserved}` is reserved and cannot name a hue"
            );
        }
    }

    /// Every hue works as a background under the `Color::Black` mush paints on
    /// it, which is the band the palette was chosen in — roughly L* 60–85, the
    /// pastel middle where black text reads and the hue still looks like a
    /// colour.
    #[test]
    fn every_hue_fits_the_black_text_band() {
        for hue in HUES {
            let lightness = lightness(hue.rgb);
            assert!(
                (60.0..=85.0).contains(&lightness),
                "{} is L* {lightness:.1}",
                hue.name
            );
        }
    }

    /// No two hues are the same colour, and none is a near-duplicate either: a
    /// squared RGB distance of at least 20 is a crude floor under the
    /// perceptual separation the palette was designed to (ΔE2000 ≥ 11.8), and
    /// it fails loudly if an edit collapses two entries together.
    #[test]
    fn no_two_hues_are_close_enough_to_confuse() {
        let mut colours = BTreeSet::new();
        for hue in HUES {
            assert!(colours.insert(hue.rgb), "{} repeats a colour", hue.name);
        }
        for (index, one) in HUES.iter().enumerate() {
            for other in &HUES[index + 1..] {
                let distance = squared_distance(one.rgb, other.rgb);
                assert!(
                    distance >= 20 * 20,
                    "{} and {} are {} apart",
                    one.name,
                    other.name,
                    f64::from(distance).sqrt()
                );
            }
        }
    }

    /// `Theme::default` is the fixed palette: the exact look of every frame
    /// mush painted before hues existed, for the tests and callers that only
    /// care about text.
    #[test]
    fn the_default_theme_is_the_fixed_palette() {
        let theme = Theme::default();
        assert_eq!(theme.accent(), Color::Cyan);
        assert!(theme.hue().is_none());
        assert_eq!(theme.describe(), "none (fixed colours)");
    }

    /// The first rung of the capability ladder: a terminal that says
    /// truecolor gets the hue's own bytes, whichever way it said it.
    #[test]
    fn a_truecolor_terminal_gets_the_hues_own_bytes() {
        let hue = nowhere_hue();
        for env in [
            text(None, Some("truecolor"), None),
            text(None, Some("24bit"), None),
            text(None, Some("TRUECOLOR"), None),
            text(None, Some("TRUECOLOR"), Some("xterm-256color")),
            text(None, None, Some("xterm-direct")),
            text(None, None, Some("screen-24bit")),
            text(None, None, Some("xterm-truecolor")),
        ] {
            let theme = Theme::resolve(&env, &nowhere()).unwrap();
            assert_eq!(
                theme.accent(),
                Color::Rgb(hue.rgb.0, hue.rgb.1, hue.rgb.2),
                "{env:?}"
            );
            assert_eq!(theme.hue().map(|hue| hue.name), Some(hue.name), "{env:?}");
        }
    }

    /// The second rung: without a truecolor announcement the hue is painted as
    /// the nearest of the terminal's own 256, and a `COLORTERM` that merely
    /// exists — `xterm-256color` in it — is not an announcement.
    #[test]
    fn a_256_colour_terminal_gets_the_nearest_entry() {
        let hue = nowhere_hue();
        for env in [
            text(None, None, None),
            text(None, Some("xterm-256color"), Some("xterm-256color")),
            text(None, None, Some("linux")),
            text(None, Some(""), Some("dumb")),
        ] {
            let theme = Theme::resolve(&env, &nowhere()).unwrap();
            assert_eq!(
                theme.accent(),
                Color::Indexed(nearest_256(hue.rgb)),
                "{env:?}"
            );
        }
        // Pinned, by hand: teal is `(0, 182, 170)`. Entry 37's offset is 21,
        // which is the cube entry `(0, 175, 175)` — 21/36 = 0 red, (21/6) % 6
        // = 3 and 21 % 6 = 3, both level 55 + 40·3 = 175 — and its squared
        // distance is 0² + 7² + 5² = 74, closer than any neighbour (entry 38
        // is (0, 175, 215), already 0² + 7² + 45²) or any gray.
        assert_eq!(nearest_256((0x00, 0xB6, 0xAA)), 37);
    }

    /// The index is the nearest one, checked against the terminal's palette
    /// rebuilt in the test's own terms — six cube levels of 0, 95, 135, 175,
    /// 215, 255 and a ramp of 8..=238 by tens — so the module's arithmetic is
    /// not compared with itself.
    #[test]
    fn the_index_is_the_nearest_entry_of_the_terminals_palette() {
        let levels = [0u8, 95, 135, 175, 215, 255];
        let mut entries: Vec<((u8, u8, u8), u8)> = Vec::new();
        for offset in 0u16..216 {
            entries.push((
                (
                    levels[(offset / 36) as usize],
                    levels[((offset / 6) % 6) as usize],
                    levels[(offset % 6) as usize],
                ),
                FIRST_SEARCHED + offset as u8,
            ));
        }
        for step in 0..24u8 {
            let gray = 8 + 10 * step;
            entries.push(((gray, gray, gray), FIRST_SEARCHED + 216 + step));
        }
        let nearest = |rgb: (u8, u8, u8)| {
            entries
                .iter()
                .min_by_key(|(entry, _)| squared_distance(rgb, *entry))
                .map(|(_, index)| *index)
                .unwrap()
        };
        for hue in HUES {
            assert_eq!(
                nearest_256(hue.rgb),
                nearest(hue.rgb),
                "{} is not indexed to its nearest entry",
                hue.name
            );
        }
    }

    /// `MUSH_THEME=256` demands the indexed form on a truecolor terminal: the
    /// human asked for the fallback, and the terminal's answer does not
    /// overrule them.
    #[test]
    fn mush_theme_256_forces_the_indexed_form() {
        let env = text(Some("256"), Some("truecolor"), None);
        let theme = Theme::resolve(&env, &nowhere()).unwrap();
        assert_eq!(
            theme.accent(),
            Color::Indexed(nearest_256(nowhere_hue().rgb))
        );
        assert_eq!(theme.hue().map(|hue| hue.name), Some(nowhere_hue().name));
        assert_eq!(
            theme.describe(),
            format!(
                "{} (indexed, from the workspace path; MUSH_THEME=256)",
                nowhere_hue().name
            )
        );
    }

    /// `MUSH_THEME=off` is today's fixed palette, and says so: a human
    /// debugging a window's colours has to know the environment silenced the
    /// hue rather than that mush forgot one.
    #[test]
    fn mush_theme_off_is_the_fixed_palette() {
        let env = text(Some("off"), Some("truecolor"), None);
        let theme = Theme::resolve(&env, &nowhere()).unwrap();
        assert_eq!(theme.accent(), Color::Cyan);
        assert!(theme.hue().is_none());
        assert_eq!(theme.describe(), "off (MUSH_THEME)");
    }

    /// A named hue is that hue, whatever the path is, and it follows the same
    /// capability ladder as the hashed one.
    #[test]
    fn a_named_hue_is_that_hue() {
        let teal = hue_by_name("teal").unwrap();
        let rgb = Color::Rgb(teal.rgb.0, teal.rgb.1, teal.rgb.2);
        let on_truecolor =
            Theme::resolve(&text(Some("teal"), Some("truecolor"), None), &nowhere()).unwrap();
        assert_eq!(on_truecolor.accent(), rgb);
        assert_eq!(on_truecolor.describe(), "teal (truecolor, MUSH_THEME)");

        let indexed = Theme::resolve(&text(Some("teal"), None, Some("linux")), &nowhere()).unwrap();
        assert_eq!(indexed.accent(), Color::Indexed(nearest_256(teal.rgb)));
        assert_eq!(indexed.describe(), "teal (indexed, MUSH_THEME)");

        // `auto` is the explicit spelling of unset, not a hue.
        let unset = Theme::resolve(&truecolor(), &nowhere()).unwrap();
        let spelled =
            Theme::resolve(&text(Some("auto"), Some("truecolor"), None), &nowhere()).unwrap();
        assert_eq!(spelled.accent(), unset.accent());
        assert_eq!(spelled.describe(), unset.describe());
    }

    /// A name mush does not know is a message naming the value and every
    /// spelling that works, straight from the table — so a hue added or
    /// renamed cannot be missing from the list.
    #[test]
    fn an_unknown_name_is_an_error_naming_it() {
        let env = text(Some("tale"), Some("truecolor"), None);
        let error = Theme::resolve(&env, &nowhere()).unwrap_err();
        assert!(error.contains("MUSH_THEME"), "{error}");
        assert!(error.contains("tale"), "{error}");
        for hue in HUES {
            assert!(
                error.contains(hue.name),
                "`{}` is missing: {error}",
                hue.name
            );
        }
        for reserved in ["auto", "256", "off"] {
            assert!(error.contains(reserved), "`{reserved}` is missing: {error}");
        }
    }

    /// A `MUSH_THEME` mush cannot use never panics, and empty or blank counts
    /// as unset: the failure modes are a message or the auto hue, never a
    /// crash on startup.
    #[test]
    fn a_bad_mush_theme_never_panics() {
        let unset = Theme::resolve(&truecolor(), &nowhere()).unwrap().describe();
        for value in [
            "",
            " ",
            "\t",
            " auto",
            "auto ",
            "off ",
            " OFF",
            "0",
            "30",
            "-amber",
            "amber ",
            "two words",
            "amber;rm -rf",
            "☃",
            &"z".repeat(4096),
        ] {
            let env = text(Some(value), Some("truecolor"), None);
            let theme = Theme::resolve(&env, &nowhere());
            if value.trim().is_empty() {
                assert_eq!(theme.unwrap().describe(), unset, "{value:?}");
            }
        }
    }

    /// The `--print-config` sentence for every way a theme can be chosen: it
    /// names the hue, the form, and the source, and never claims a truecolor
    /// form on a terminal that did not announce one.
    #[test]
    fn the_describe_line_is_true_for_every_way_a_theme_is_chosen() {
        let name = nowhere_hue().name;
        let resolve = |theme: Option<&str>, colorterm: Option<&str>, term: Option<&str>| {
            Theme::resolve(&text(theme, colorterm, term), &nowhere())
                .unwrap()
                .describe()
        };
        let auto = resolve(None, Some("truecolor"), None);
        assert_eq!(auto, format!("{name} (truecolor, from the workspace path)"));
        assert_eq!(
            resolve(None, None, None),
            format!("{name} (indexed, from the workspace path)")
        );
        assert_eq!(
            resolve(Some("256"), Some("truecolor"), None),
            format!("{name} (indexed, from the workspace path; MUSH_THEME=256)")
        );
        assert_eq!(
            resolve(Some("teal"), Some("truecolor"), None),
            "teal (truecolor, MUSH_THEME)"
        );
        assert_eq!(
            resolve(Some("teal"), None, None),
            "teal (indexed, MUSH_THEME)"
        );
        assert_eq!(resolve(Some("off"), None, None), "off (MUSH_THEME)");
        assert_eq!(Theme::default().describe(), "none (fixed colours)");
        // The form word is the accent's own variant, so the sentence cannot
        // disagree with what a frame will paint.
        assert_eq!(
            Theme::resolve(&text(None, Some("truecolor"), None), &nowhere())
                .unwrap()
                .accent(),
            Color::Rgb(
                nowhere_hue().rgb.0,
                nowhere_hue().rgb.1,
                nowhere_hue().rgb.2
            )
        );
    }
}
