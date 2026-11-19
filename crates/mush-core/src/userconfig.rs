//! User-level configuration stored in the home directory.
//!
//! Unlike the per-workspace session (`.mush/session.json`), this file is
//! machine-global: it is where the API key lives (the session deliberately
//! never stores secrets) and it holds the defaults a human would otherwise type
//! on every command line. Resolution order on startup is
//! `CLI flags > environment > saved session > this file > built-in defaults`.
//!
//! The file is **TOML**, and it is meant to be hand-edited: every field is
//! optional, the file mush writes carries a short `#` preamble naming the
//! precedence, one `#` line above each key saying what that key does, and an
//! unknown key is ignored (and kept) rather than fatal, so a newer mush can add
//! one without breaking an older one. The long explanations live in
//! `docs/mush.md` and the manual, not in the file (finding C12).
//! `mush --print-config` prints what the layers above resolved to.
//!
//! [`toml_edit`] and not a plain TOML serializer, because a save is a *merge*
//! over the human's own file: what a save does not state — their `#` comments,
//! their formatting, a key or a table a newer mush invented — has to survive it,
//! which is what the format-preserving parser is for (finding C12). The
//! extension names the format: a `MUSH_CONFIG` path ending in `.json` is the
//! file mush wrote before this change, read as JSON and migrated to TOML by the
//! first startup (`UserConfig::load_at`).
//!
//! Path: `$MUSH_CONFIG`, else the platform config directory
//! (`$XDG_CONFIG_HOME/mush/config.toml`, usually `~/.config/mush/config.toml`
//! on Unix, Application Support on macOS, `%APPDATA%` on Windows). When
//! neither names one — `HOME` unset with no `MUSH_CONFIG`, and no home the
//! passwd database can name — there is no home config at all: mush refuses it
//! rather than fall back to a relative path in the directory it was launched
//! from (finding IN14).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use toml_edit::{DocumentMut, Item, Table};

use crate::workspace::{atomic_write, Fresh};

/// The key mush's old JSON header travelled under: the eleven paragraphs that
/// documented the file. It is not a setting, nothing ever read it back, and a
/// save deletes it — the file documents itself with real `#` comments now
/// (finding C12).
const COMMENT_KEY: &str = "_comment";

/// What every file mush writes says about itself, above the first key and
/// separated from it by the blank line that closes the block.
///
/// Four lines, because everything else the old header said is now one `#` line
/// above the key it is about, and the reasons a field exists are the manual's.
/// What is left here is what a hand-editor needs before reading any key: the
/// file's format and that every field is optional, the precedence, the two
/// commands that name another file or show the resolution, and the one promise
/// that makes editing safe (mush keeps what it does not know).
const PREAMBLE: &str = "\
# mush home config — hand-editable TOML, and every field is optional.
# Resolution: CLI flags > MUSH_* environment > this workspace's session > this file > built-in defaults.
# `MUSH_CONFIG` names another file, and `mush --print-config` shows what these resolved to.
# Unknown keys are ignored and kept, and so are the comments you write yourself; the reasons a field exists are in docs/mush.md and the manual, not here.
";

/// The keys mush documents, in the order a file lists them: the values first
/// and the two tables last, since a TOML table is written as its own section
/// and a section ends the values above it.
const FIELDS: &[&str] = &[
    "api_key",
    "provider",
    "base_url",
    "model",
    "context",
    "temperature",
    "max_completion_tokens",
    "reasoning_effort",
    "thinking",
    "api_keys",
    "providers",
];

/// The one `#` line above a key, or `None` for a key mush knows nothing about.
///
/// One line each, and spelled from [`crate::provider`] wherever a name or a
/// number could drift from what a request actually sends: a hand-editable file
/// is the one place a human reads a default. The provider line keeps the
/// meaning the long header carried — the annotations are not exhaustive, a
/// model they do not name is unannotated rather than assumed blind, and no key
/// lives in that table.
fn field_help(field: &str) -> Option<String> {
    use crate::provider::{
        context_default_hint, effort_default_hint, names_hint, thinking_default_hint, Provider,
        DEFAULT_PROVIDER,
    };
    Some(match field {
        "api_key" => "api_key: the flat key an older file carried — a legacy fallback for a host \
                      `api_keys` does not name. A save moves it under the host this file's \
                      `base_url` names, and mush never writes a null one."
            .to_string(),
        "api_keys" => "api_keys: one key per host — the endpoint's `host[:port]`, as `base_url` \
                       names it. `/key` writes the endpoint in force, switching endpoints keeps \
                       every other host's entry, and a host with no entry falls back to `api_key` \
                       above."
            .to_string(),
        "providers" => format!(
            "providers: rows of your own — a base_url, the model and knobs selecting it applies, \
             and a `models` map annotating the ids that endpoint serves (vision, context). The \
             annotations are not exhaustive: a model they do not name is unannotated, not assumed \
             blind — its pictures are sent, and a `vision: false` here restores the refusal. \
             Select a row with /provider, --provider, MUSH_PROVIDER or a workspace's session. No \
             keys in this table: a provider's key is the `api_keys` entry for its host. The \
             built-ins ({}) are always available, and a row may not take one of their names over.",
            names_hint()
        ),
        "provider" => format!(
            "provider: {}; `{}` defaults to the local endpoint {}.",
            names_hint(),
            DEFAULT_PROVIDER.name(),
            Provider::Custom.default_base_url()
        ),
        "base_url" => "base_url: an OpenAI-compatible endpoint, without a trailing slash; a \
                       control character in it is refused."
            .to_string(),
        "model" => "model: the model id to start with, when nothing above names one.".to_string(),
        "context" => format!(
            "context: a context window in tokens; the built-in default is {}. Stating it here \
             beats what the endpoint advertises, as --context does.",
            context_default_hint()
        ),
        "temperature" => "temperature: 0.0-2.0, sent with every request; 1.0 is the model's own \
                          choice, and the default."
            .to_string(),
        "max_completion_tokens" => format!(
            "max_completion_tokens: true sends the reply cap — {}, at most — as \
             max_completion_tokens; OpenAI's reasoning models reject max_tokens.",
            crate::config::REPLY_SHARE_WORDS
        ),
        "reasoning_effort" => format!(
            "reasoning_effort: \"low\", \"high\" or \"max\" — exactly what the DeepSeek OpenAI \
             format documents. The provider's own default is {}.",
            effort_default_hint()
        ),
        "thinking" => format!(
            "thinking: true asks for the provider's thinking mode (the request carries \
             {{\"type\":\"enabled\"}}); false sends no thinking field and leaves the model's own \
             default. The provider's own default is {}.",
            thinking_default_hint()
        ),
        _ => return None,
    })
}

/// One named provider in [`UserConfig::providers`]: an OpenAI-compatible
/// endpoint, the knobs selecting it applies, and an annotation set of the model
/// ids it serves.
///
/// Every field is optional, because a row is a *statement* about a host and a
/// human writes only what they know: an empty row is a name that keeps the
/// endpoint already set and says nothing about it.
#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// The endpoint this row owns, without a trailing slash; a control
    /// character in it is refused where every other endpoint is. Stating one
    /// makes selecting the row *switch* to it; leaving it empty keeps the
    /// endpoint in force, the way the `custom` row does.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// An **annotation set keyed by model id**, not a claim to be exhaustive.
    ///
    /// The endpoint may serve models this map does not name, and they are still
    /// pickable: a model nothing here names is simply *unannotated*, so it falls
    /// through to `unknown` — never to "blind" by the field's absence. What a
    /// row states here is the only way mush learns a capability of a model it
    /// has never heard of (the home box serving `lfm2-vl` is the case that
    /// matters), and it is stated, so it is trusted where a guess would not be.
    ///
    /// Ids compare without their namespace (`vendor/model`), exactly as
    /// [`crate::provider::ProviderRow::documented_context`] and
    /// [`crate::provider::ProviderRow::vision`] compare them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, ModelConfig>,
    /// The model selecting this row applies when nothing above states one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Ask for the host's thinking mode. `false` (and the field's absence) sends
    /// no `thinking` field at all; `true` sends `{"type":"enabled"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    /// The `reasoning_effort` a request to this host asks for when the human
    /// states none: "low", "high" or "max", checked here rather than where it
    /// is applied so a typo in a row nobody selected is still reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Sampling temperature selecting this row applies; clamped where every
    /// temperature is, so a typo is a value mush can still send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// The window this row's models are budgeted against when neither the model
    /// annotation nor the endpoint states one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<usize>,
}

/// What one provider row states about one model id.
///
/// Both fields are `Option` on purpose: absent is "this row states nothing",
/// which is a different fact from `vision: false` — a human's statement that
/// the model is blind. The first leaves the model *unknown* (its parts travel,
/// and an endpoint that cannot see says so in its own words), the second keeps
/// today's refusal and drop.
#[derive(Default, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Whether this model accepts image content parts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    /// The window this model has, in tokens — the same fact
    /// [`crate::provider::known_context`] states for a documented model, stated
    /// here by the human because only they know what their server was started
    /// with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<usize>,
}

/// The machine-global defaults the precedence chain consults below the session.
///
/// Every field is optional: an absent field is simply not stated, and a file
/// with only the four connection fields still loads and still means what it
/// always meant. There are deliberately no knobs beyond the ones mush reads
/// (`Config`): no `param_style`, because the only parameter-name switch mush has
/// is the reply cap, which `max_completion_tokens` states directly, and no
/// `history_budget_multiplier`, because the history budget is a conservative
/// share of the window and the window is the knob that states it. A field
/// nothing reads would be a promise, not a setting.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct UserConfig {
    /// The legacy flat key: what a file written before [`Self::api_keys`]
    /// carries, and the fallback [`crate::config::resolve`] reads for a host
    /// the map does not name. Stored in plain text on your own machine; never
    /// written to the workspace.
    ///
    /// A stated key has a host to live under, so `/key` writes the map; the
    /// flat field is written only by a save whose value states a key for a file
    /// that names no endpoint, and by nothing else. A save that has no flat key
    /// to write removes the field rather than writing a null: TOML has none at
    /// all, and an absent key reads back as `None`. A key that reached the run
    /// through `MUSH_API_KEY` was deliberately kept out of files, and no save
    /// triggered by an unrelated command copies it in, flat or keyed
    /// (finding C11).
    ///
    /// A legacy value is migrated under its own host — the one the file's
    /// `base_url` names — by the next [`KeyWrite::Stated`] save, and left
    /// exactly as it is by [`KeyWrite::Keep`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// The keys this machine holds, one per host: the host [`host_of`] names
    /// for an endpoint, i.e. the authority (`host[:port]`) exactly as
    /// `checked_url`/`normalize_url` leave it, with no scheme, path or query.
    ///
    /// A key belongs to the host it was minted for (findings C6, D6), and the
    /// flat field above cannot say which host that was — so moving the endpoint
    /// to another host dropped the key. Keyed by host, it is kept *for its own
    /// host* and adopted again when the endpoint comes back to it. `/key`
    /// writes the entry for the endpoint in force; a save never deletes another
    /// host's entry. An empty map is not written at all: a host with no entry
    /// has no key, and that absence is the statement.
    ///
    /// [`host_of`]: crate::config::host_of
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub api_keys: BTreeMap<String, String>,
    /// The provider rows this machine names, each an endpoint under a name the
    /// human chose: `/provider home`, `--provider work`, and a row of their own
    /// in the picker.
    ///
    /// These are ordinary rows of the table [`crate::provider::Catalog`]
    /// resolves at startup — the built-ins (`deepseek`, `custom`) are always
    /// there, and a row here adds a name beside them. A row may not shadow a
    /// built-in name, which is a startup failure naming this file and the name:
    /// two rows called `deepseek` would make the endpoint a request reaches
    /// depend on which arm of the merge ran first.
    ///
    /// **A row is a preset, not a place.** Selecting it applies the fields it
    /// states — the endpoint, the model, the window, the temperature and the
    /// two request knobs — exactly as the built-in rows above do from their
    /// constants; everything a layer above this file states (a flag, the
    /// environment, a workspace's session) still outranks it.
    ///
    /// **No keys here.** [`Self::api_keys`] is the only place a key is stored: a
    /// provider's key is the entry for its host, which is why that map is keyed
    /// by host and not by name. Two names for one host share its key, and a row
    /// that moves to another host keeps the old one untouched.
    ///
    /// An empty map is not written at all; a save that states no rows keeps the
    /// ones the file already had, and one that states rows replaces the whole
    /// map with them (the rows are this value's statement, like [`Self::api_keys`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Provider name as given by [`Provider::name()`](crate::provider::Provider::name)
    /// or by a row of [`Self::providers`], i.e. a name `--provider` accepts (see
    /// [`provider::PROVIDERS`](crate::provider::PROVIDERS)).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Context window in tokens. A window stated here is a statement, not a
    /// guess: it beats what the endpoint advertises, exactly like `--context`,
    /// and only a window this workspace remembers (the session) outranks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<usize>,
    /// Sampling temperature sent with every request. Clamped by
    /// `Config::temperature`, so a typo is a value mush can still send rather
    /// than a request an endpoint rejects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Send the reply cap as `max_completion_tokens` instead of `max_tokens`,
    /// which OpenAI's reasoning models require.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<bool>,
    /// Reasoning effort sent as `reasoning_effort`: "low", "high" or "max",
    /// exactly the values the DeepSeek OpenAI format documents. Stated here it
    /// is honoured wherever the endpoint is pointed; unstated, the provider's
    /// own documented default applies (`provider::PROVIDERS`), and a row that
    /// documents none is the only way the field is left off. A value mush does
    /// not know is reported at startup rather than sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Ask for the provider's thinking mode. `true` sends the `thinking` field
    /// a provider documents (`{"type":"enabled"}`); `false` sends no
    /// `thinking` field at all and leaves the model's own default. Unstated,
    /// the provider's own default applies (see `provider::PROVIDERS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
}

/// The decision [`config_path`] makes, pure so a test can pin both answers
/// without touching the process's environment: an explicit override is taken
/// as given, and otherwise the platform config directory is the only road.
fn config_path_from(
    override_path: Option<OsString>,
    config_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = override_path {
        return Some(toml_path(PathBuf::from(path)));
    }
    config_dir.map(|dir| dir.join("mush/config.toml"))
}

/// The file mush writes, out of the name the human gave it: a `.json` name is
/// the *legacy* file, and the TOML is that same file with the extension the
/// format takes now.
///
/// This is what keeps `MUSH_CONFIG=…/my.json` naming that machine's config
/// after the migration, instead of naming the file the migration removed: the
/// sibling `<stem>.toml` is where the values are written and read from then on,
/// and the one line the migration says names both paths (finding C12).
fn toml_path(path: PathBuf) -> PathBuf {
    if is_legacy_json(&path) {
        path.with_extension("toml")
    } else {
        path
    }
}

/// The extension names the format: `.json` is the file mush wrote before TOML —
/// read as JSON, and the file a migration converts — and anything else is TOML.
fn is_legacy_json(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
}

/// Where the user config lives: `MUSH_CONFIG` names it outright, and otherwise
/// it is the platform config directory (`$XDG_CONFIG_HOME/mush/config.toml` on
/// Unix, the Application Support directory on macOS, `%APPDATA%` on Windows).
/// `None` is a machine with no home config at all — see the reason below.
///
/// The file is the *machine-global* one the API key is written to in plain
/// text, so a home is what it is named after and the only road to it that is
/// not the workspace is built from one. `MUSH_CONFIG` is an explicit road the
/// human chose, relative or not, and it is taken as given — as the TOML file
/// beside a `.json` name, for the reason `toml_path` gives. Without it, an
/// unset `HOME` and no config directory (on Unix: no `$XDG_CONFIG_HOME` and no
/// home the passwd database can name) has no such place: the old fallback was
/// the *relative* `.mush-user-config.json`, which is whatever directory mush
/// was launched from — usually a git repository, one `git add -A` from a
/// committed credential — and a second workspace then reads a different config
/// (finding IN14).
///
/// Refusing is truer than inventing a path: an absolute one under the cwd is
/// still the workspace's own, and one anywhere else is a place the human never
/// chose and the next run cannot promise to find again. So there is no home
/// config, and every reader and writer says so by name — `HOME is not set` —
/// instead of silently reading or writing the cwd ([`UserConfig::load`],
/// [`config_path_label`]).
pub fn config_path() -> Option<PathBuf> {
    config_path_from(
        std::env::var_os("MUSH_CONFIG").filter(|path| !path.is_empty()),
        dirs::config_dir(),
    )
}

/// The home config's path as a message names it, or the refusal that stands in
/// it: one spelling for the refusals `config::resolve` writes, so a machine
/// with no home reads the same sentence wherever a path would have been
/// (finding IN14).
pub fn config_path_label() -> String {
    match config_path() {
        Some(path) => path.display().to_string(),
        None => "no home config: HOME is not set and MUSH_CONFIG names no file".to_string(),
    }
}

/// A home config file as it was read: the values, and what to say when the file
/// was there and could not be used.
///
/// "Missing" and "there and unusable" are different facts about a layer, and
/// flattening them into the same silence is what let an unreadable file be
/// replaced by mush's four fields and its header on the first save — with the
/// human's key inside it (finding C3). The complaint carries the path and the
/// reason; the sentence around it is the caller's, because `main` is where the
/// human reads it.
pub struct Loaded {
    /// The values. [`UserConfig::default`] when the file is absent, or when it
    /// is there and could not be used.
    pub config: UserConfig,
    /// `Some` when the file was there and could not be read or parsed: why, and
    /// where. `None` when it was read, or when there was no file at all — the
    /// two silences are the same only because there is nothing to say in
    /// either.
    pub complaint: Option<String>,
    /// `Some` when this read converted a legacy JSON file to TOML: one line
    /// naming where the file is now and that the old one is gone.
    ///
    /// It is not a complaint — nothing failed — but it is not silence either:
    /// the human's file moved, and the file a save writes from now on is the
    /// new one. It travels the road the resolution notices take, because a
    /// migration behind a path the human named must be said out loud
    /// (finding C12).
    pub migrated: Option<String>,
}

/// The road a home-config save takes the key fields by — the legacy flat
/// `api_key` and the host → key `api_keys` map together.
///
/// The key is the one thing whose statement a save may have to *withhold*, so
/// the road is an argument rather than a convention. It can come from a layer
/// the human deliberately kept out of files (`MUSH_API_KEY`, the README's own
/// road for a key you do not want on disk), and a save triggered by an
/// unrelated command — `/url`, `/model`, `/provider`, whose acks say nothing
/// about a file — must not be the road that copies it in (finding C11). Every
/// other field of the value is stated either way, and the ordinary merge still
/// fills in whatever this value leaves unstated.
///
/// Neither road ever writes a null flat key: TOML has none, and a flat key is
/// written only when there is a string to write and removed otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyWrite {
    /// State the key this value holds: `Some` writes it under the host of this
    /// value's `base_url` (flat, when that endpoint is empty), while `None`
    /// says this endpoint has none — by there being no entry, never by deleting
    /// another host's (findings C6, D6).
    ///
    /// A legacy flat key in the file is migrated on the way: its host is the
    /// one the file's own `base_url` names, and while the map has no entry for
    /// it the key moves there instead of being dropped. No endpoint, or an
    /// entry already there, means no migration — and the flat key is left
    /// exactly as it was, because a key is never deleted on a guess.
    Stated,
    /// Leave the file's own key structure exactly as it is — the map and the
    /// flat field, present or absent — whatever this value holds: no migration,
    /// no insert, no delete. The road for a save whose key is not the human's to
    /// write: it is not a statement about the endpoint, it is silence about the
    /// key (finding C11).
    Keep,
}

impl UserConfig {
    pub fn load() -> Loaded {
        Self::load_at(config_path())
    }

    /// The same read for an already-resolved path, and for a machine that has
    /// none: `HOME` unset with no `MUSH_CONFIG` is not "a file that is not
    /// there" — there is no home config at all, and the complaint says so by
    /// name instead of letting the defaults arrive in silence (finding IN14).
    ///
    /// This is also the one road that migrates: the TOML file mush would use is
    /// missing and the legacy `config.json` beside it is there. A migration is
    /// a *startup* fact — it happens once, before anything reads a value, and
    /// the next start (or the same one, again) finds the TOML file and converts
    /// nothing (finding C12).
    fn load_at(path: Option<PathBuf>) -> Loaded {
        let Some(path) = path else {
            return Loaded {
                config: UserConfig::default(),
                complaint: Some(
                    "no home config to read: HOME is not set and MUSH_CONFIG names no file \
                     — set HOME or MUSH_CONFIG; using defaults"
                        .to_string(),
                ),
                migrated: None,
            };
        };
        let legacy = path.with_extension("json");
        let mut migrated = None;
        // A `.json` path mush was handed is read as JSON by [`Self::load_from`]
        // already; what is left is the TOML path whose legacy sibling needs
        // converting, and that is the one case migration runs in.
        if !is_legacy_json(&path) && !path.exists() && legacy.is_file() {
            match migrate(&legacy, &path) {
                Ok(line) => migrated = Some(line),
                // The JSON is still the human's file, so it is still what this
                // run reads — and if it cannot be read either, that is the
                // complaint its own read gives, not a second sentence about a
                // migration nobody asked for.
                Err(why) => {
                    let fallback = Self::load_from(&legacy);
                    return Loaded {
                        config: fallback.config,
                        complaint: Some(fallback.complaint.unwrap_or(why)),
                        migrated: None,
                    };
                }
            }
        }
        let mut loaded = Self::load_from(&path);
        loaded.migrated = migrated;
        loaded
    }

    /// Read the file, or the defaults.
    ///
    /// A missing file is silence: a machine that never wrote one has nothing to
    /// complain about. A file that is *there* and cannot be used — unreadable,
    /// not the format its name states, or one field of the wrong type — is the
    /// defaults *with a complaint*: a hard failure would take the whole TUI down
    /// over one wrong character, and silence would let the human's key and
    /// settings vanish into the built-ins without a word (finding C3). An
    /// unknown *key* is ignored: forward compatibility costs nothing a human
    /// editing this by hand would miss.
    ///
    /// Two values in a file that *did* parse are reported by name instead, where
    /// they are read: the home config's `provider` and its `reasoning_effort`
    /// ([`crate::config::resolve`]), because a name mush does not know must
    /// never fall through to a default host (findings A17, C2).
    pub fn load_from(path: &Path) -> Loaded {
        let complaint = |why: String| Loaded {
            config: UserConfig::default(),
            complaint: Some(why),
            migrated: None,
        };
        let bytes = match fs::read(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Loaded {
                    config: UserConfig::default(),
                    complaint: None,
                    migrated: None,
                }
            }
            Err(error) => {
                return complaint(format!(
                    "could not read {} — {error}; using defaults",
                    path.display()
                ))
            }
            Ok(bytes) => bytes,
        };
        match parse(&bytes, path) {
            Ok(config) => Loaded {
                config,
                complaint: None,
                migrated: None,
            },
            Err(why) => complaint(format!(
                "could not read {} — {why}; using defaults",
                path.display()
            )),
        }
    }

    /// Write the file: this value's fields, over whatever is already there,
    /// with the two key fields taken by the road `key` names.
    ///
    /// A field this value leaves *unstated* — `None`, or an empty string for a
    /// connection field — keeps the value the file already had, and so does a
    /// key this build does not know. The TUI saves only the four fields it owns
    /// (a `/key` writes the connection and nothing else), and a hand-edited
    /// `temperature`, a comment, or a setting only a newer mush understands,
    /// must survive that. Fields that are stated overwrite.
    ///
    /// The file is TOML edited in place ([`toml_edit`]): the human's own `#`
    /// comments and formatting for everything this save does not state are
    /// preserved, and so are keys and tables mush does not know. What mush does
    /// write is the *header* — the `PREAMBLE` at the top, which a save
    /// rewrites, and the one `#` line above each key it documents, which is
    /// mush's line even when the human replaced it with their own
    /// (finding C12).
    ///
    /// `api_key` and `api_keys` are the fields with a road of their own
    /// ([`KeyWrite`]), because the key is the one thing a save may have to
    /// withhold. [`KeyWrite::Stated`] makes the key an ordinary stated field:
    /// `Some(l)` writes `l` under the host of this value's `base_url`, or flat
    /// when that endpoint is empty, while `None` means this endpoint has no key
    /// — stated by there being no entry for it, never by deleting another
    /// host's, because the key lives beside the endpoint it was minted for and
    /// the save that drops it is the save that just moved the endpoint
    /// (findings C6, D6); merging the old host's key forward would hand it to
    /// the new one on the next start. A flat `api_key` the file still carries is
    /// a legacy key with no host of its own, and the file's `base_url` is the
    /// only hint there is: a `Stated` save migrates it under that host while the
    /// map has no entry for it, and leaves it alone when there is no host to key
    /// it on or the map already speaks for that host. [`KeyWrite::Keep`] leaves
    /// the file's own map and flat key exactly where they are, and migrates
    /// nothing: it is what lets a command whose ack says nothing about a file
    /// avoid putting a key in one (finding C11).
    ///
    /// A flat key is never written as anything but a string: one is inserted
    /// only when there is a key to insert, and the field is otherwise removed.
    /// The flat field is a legacy read-only fallback, not something every save
    /// restates.
    ///
    /// A file that is there and is not a table mush can merge into — not TOML,
    /// not UTF-8, or unreadable — is moved beside itself as `<name>.bak` (then
    /// `.bak.2`, …) before anything is written: this save states four fields and
    /// rewrites the header, so without that it would be the thing that destroys
    /// the file the human's key is in (finding C3). A backup that fails refuses
    /// the save rather than writing over the only copy.
    pub fn save_to(&self, path: &Path, key: KeyWrite) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // What the merge can read. A table is the shape a file mush wrote (and
        // the human edited); anything else is not content the merge can
        // preserve, so it is the backup's business instead. The comment block
        // at the top is mush's preamble and is rewritten below, so it is not
        // carried into the document this save edits.
        let existing = fs::read(path)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| strip_preamble(&text).parse::<DocumentMut>().ok());
        // `missing` is the one case with a right answer of its own: there is
        // nothing to keep, so nothing is kept. Everything else that does not
        // merge gets the backup first.
        let missing = matches!(fs::metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound);
        if existing.is_none() && !missing {
            keep_unparsable(path)?;
        }
        let mut doc = existing.unwrap_or_default();
        // The JSON era's header key: mush wrote it, nothing read it, and the
        // file documents itself with comments now (finding C12).
        doc.as_table_mut().remove(COMMENT_KEY);
        // The key, by the road the caller named. Both key fields travel
        // together because both are the key, and a `Keep` save must not let a
        // value's own map stand over the file's.
        self.write_key(&mut doc, key);
        // Then this value's other fields: a stated one is written with its
        // line, and one the file holds but this value does not state keeps the
        // value it had — and gets mush's line, because that is the header.
        let stated = self.stated();
        for field in FIELDS {
            match stated.get(field) {
                Some(item) => set_field(&mut doc, field, item.clone()),
                None if doc.get(field).is_some() => set_comment(&mut doc, field),
                None => {}
            }
        }
        let text = format!("{PREAMBLE}\n{doc}");
        // A new file is the human's alone ([`Fresh::Private`]): it carries the
        // API key, and a `022` umask must not make it group-readable.
        atomic_write(path, text.as_bytes(), Fresh::Private)
    }

    /// This value's fields as the TOML a save states: only the ones it has a
    /// value for, so an unstated field cannot overwrite the file's, and no
    /// absent field becomes a null (TOML has none).
    ///
    /// Built by hand rather than through `serde` because the *shape* is the
    /// file's: rows and annotation sets are real tables a human edits line by
    /// line, and an `f32` temperature is written as `0.3`, not as the widened
    /// `0.30000001192092896` the `f64` TOML stores would print.
    fn stated(&self) -> Table {
        let mut stated = Table::new();
        if !self.provider.is_empty() {
            stated.insert("provider", text(&self.provider));
        }
        if !self.base_url.is_empty() {
            stated.insert("base_url", text(&self.base_url));
        }
        if !self.model.is_empty() {
            stated.insert("model", text(&self.model));
        }
        if let Some(context) = self.context {
            stated.insert("context", count(context));
        }
        if let Some(temperature) = self.temperature {
            stated.insert("temperature", decimal(temperature));
        }
        if let Some(flag) = self.max_completion_tokens {
            stated.insert("max_completion_tokens", toml_edit::value(flag));
        }
        if let Some(effort) = self.reasoning_effort.as_deref() {
            stated.insert("reasoning_effort", text(effort));
        }
        if let Some(flag) = self.thinking {
            stated.insert("thinking", toml_edit::value(flag));
        }
        if !self.providers.is_empty() {
            stated.insert("providers", Item::Table(self.providers_table()));
        }
        stated
    }

    /// The `providers` map as TOML tables: one section per row
    /// (`[providers.home]`), the annotation set nested the same way, so a row a
    /// human is meant to edit has a line per fact rather than one long line.
    fn providers_table(&self) -> Table {
        let mut rows = Table::new();
        for (name, row) in &self.providers {
            let mut stated = Table::new();
            if !row.base_url.is_empty() {
                stated.insert("base_url", text(&row.base_url));
            }
            if !row.model.is_empty() {
                stated.insert("model", text(&row.model));
            }
            if let Some(flag) = row.thinking {
                stated.insert("thinking", toml_edit::value(flag));
            }
            if let Some(effort) = row.reasoning_effort.as_deref() {
                stated.insert("reasoning_effort", text(effort));
            }
            if let Some(temperature) = row.temperature {
                stated.insert("temperature", decimal(temperature));
            }
            if let Some(context) = row.context {
                stated.insert("context", count(context));
            }
            if !row.models.is_empty() {
                let mut models = Table::new();
                for (id, facts) in &row.models {
                    let mut stated = Table::new();
                    if let Some(vision) = facts.vision {
                        stated.insert("vision", toml_edit::value(vision));
                    }
                    if let Some(context) = facts.context {
                        stated.insert("context", count(context));
                    }
                    models.insert(id, Item::Table(stated));
                }
                stated.insert("models", Item::Table(models));
            }
            rows.insert(name, Item::Table(stated));
        }
        rows
    }

    /// The key fields, by the road [`KeyWrite`] names. Both are taken here and
    /// not by the ordinary field loop, because the map is the caller's
    /// statement and the flat field is a legacy fallback whose only writer is
    /// the migration below (findings C6, D6, C11).
    fn write_key(&self, doc: &mut DocumentMut, key: KeyWrite) {
        if key == KeyWrite::Keep {
            // Silence about the key: the map and the flat field stay exactly as
            // the file had them, present or absent.
            return;
        }
        let file_flat = doc.get("api_key").and_then(Item::as_str).map(str::to_owned);
        let file_host = doc
            .get("base_url")
            .and_then(Item::as_str)
            .map(crate::config::host_of)
            .unwrap_or_default();
        let mut map = api_keys_table(doc);
        let mut flat = file_flat.clone();
        // A flat key the file still carries is legacy: the host it belongs to
        // is the one the file's own `base_url` names, and that is the only hint
        // there is — so move it under that host before this save states
        // anything. No endpoint, or an entry the map already has, means no
        // migration, and then the flat key is left exactly as it was: a key is
        // never deleted on a guess.
        if let Some(legacy) = file_flat.as_deref() {
            let taken = map
                .iter()
                .any(|(stored, _)| stored.eq_ignore_ascii_case(file_host));
            if !file_host.is_empty() && !taken {
                map.insert(file_host, text(legacy));
                flat = None;
            }
        }
        // `None` says this host has none, by there being no entry: never by
        // deleting another host's, whose key is still the key for that host.
        if let Some(written) = self.api_key.as_deref() {
            let host = crate::config::host_of(&self.base_url);
            if host.is_empty() {
                // No endpoint, no host to key on: the map has nowhere to put
                // the key, so it is stated flat as the old file did — the only
                // shape that can carry it, and one no host can claim.
                flat = Some(written.to_owned());
            } else {
                map.insert(host, text(written));
            }
        }
        if map.is_empty() {
            doc.as_table_mut().remove("api_keys");
        } else {
            doc.insert("api_keys", Item::Table(map));
        }
        match flat {
            Some(flat) => {
                doc.insert("api_key", text(&flat));
            }
            None => {
                doc.as_table_mut().remove("api_key");
            }
        }
    }
}

/// Read one home config file in the format its name states: the JSON mush wrote
/// before TOML, or TOML.
fn parse(bytes: &[u8], path: &Path) -> Result<UserConfig, String> {
    if is_legacy_json(path) {
        return serde_json::from_slice(bytes).map_err(one_line);
    }
    let text = std::str::from_utf8(bytes).map_err(one_line)?;
    let document = text.parse::<DocumentMut>().map_err(one_line)?;
    toml_edit::de::from_document(document).map_err(one_line)
}

/// A parser's complaint as one line: a status line is one line, and a TOML
/// error is a multi-line caret diagram that would otherwise land in a chat's
/// footer.
fn one_line(error: impl std::fmt::Display) -> String {
    error
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Convert a legacy `config.json` into the TOML file this machine uses now, and
/// say so.
///
/// This is a *format* change and not a statement about the key, so no
/// [`KeyWrite`] road runs here: the values cross verbatim — a flat `api_key`
/// stays flat, the `api_keys` map stays a map — and a key the old file held is
/// never re-homed, moved or dropped on the way.
///
/// The old file is removed only after the file just written has been read back
/// and agrees, because a key is never deleted on a hope: a write that fails, a
/// read-back that disagrees, a removal that fails, or a JSON file that cannot be
/// read at all leaves that file exactly where it was, leaves no half-migrated
/// TOML behind, and says why (finding C12).
fn migrate(legacy: &Path, target: &Path) -> Result<String, String> {
    let bytes = fs::read(legacy).map_err(|error| unreadable(legacy, &error.to_string()))?;
    // The values the TOML is compared against are what mush reads out of the
    // old file: a file mush cannot make a config of is not one to convert.
    let expected = parse(&bytes, legacy).map_err(|why| unreadable(legacy, &why))?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|error| unreadable(legacy, &error.to_string()))?;
    let mut doc = DocumentMut::new();
    if let Some(fields) = value.as_object() {
        // The fields a file lists, in the order a save writes them, then
        // whatever a newer mush put in the old file. Iterating the JSON map
        // alone would list them alphabetically, which is not the order the
        // human will see from the next save on.
        let known = FIELDS
            .iter()
            .filter_map(|field| fields.get(*field).map(|value| (*field, value)));
        let unknown = fields
            .iter()
            .filter(|(field, _)| !FIELDS.contains(&field.as_str()))
            .map(|(field, value)| (field.as_str(), value));
        for (field, value) in known.chain(unknown) {
            // The header key was mush's own and nothing read it; TOML has no
            // null, and a null is a field no reader stated.
            if field == COMMENT_KEY || value.is_null() {
                continue;
            }
            if let Some(item) = item_of(value) {
                set_field(&mut doc, field, item);
            }
        }
    }
    let text = format!("{PREAMBLE}\n{doc}");
    if let Err(error) = atomic_write(target, text.as_bytes(), Fresh::Private) {
        return Err(format!(
            "could not write {} — {error}; {} is still where it was",
            target.display(),
            legacy.display()
        ));
    }
    // Read the file that was just written: the values have to be the ones the
    // old file stated, or the migration is the thing that lost them.
    let written = UserConfig::load_from(target);
    let agrees = written.complaint.is_none()
        && serde_json::to_value(&written.config).ok() == serde_json::to_value(&expected).ok();
    if !agrees {
        let _ = fs::remove_file(target);
        return Err(format!(
            "could not migrate {} to {} — it did not read back what was written; {} is still \
             where it was, and is what this run reads",
            legacy.display(),
            target.display(),
            legacy.display()
        ));
    }
    if let Err(error) = fs::remove_file(legacy) {
        // Two files holding the key is worse than one migration retried: the
        // TOML goes, so the state is exactly what it was before this run.
        let _ = fs::remove_file(target);
        return Err(format!(
            "could not remove {} after migrating it — {error}; it is still where it was",
            legacy.display()
        ));
    }
    Ok(format!(
        "home config migrated to {} — the old {} is gone",
        target.display(),
        legacy.display()
    ))
}

/// The one sentence a file mush could not read at all is said in.
fn unreadable(legacy: &Path, why: &str) -> String {
    format!(
        "could not read {} — {why}; using defaults",
        legacy.display()
    )
}

/// A JSON scalar as the TOML value it is: a migration writes the old file's
/// values verbatim, so nothing the human stated changes shape on the way.
fn scalar_of(value: &Value) -> Option<toml_edit::Value> {
    Some(match value {
        Value::Null => return None,
        Value::Bool(flag) => toml_edit::Value::from(*flag),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => toml_edit::Value::from(integer),
            None => toml_edit::Value::from(number.as_f64()?),
        },
        Value::String(text) => toml_edit::Value::from(text.as_str()),
        Value::Array(_) | Value::Object(_) => return None,
    })
}

/// A JSON value as the TOML item a migration writes. `None` for the shapes TOML
/// cannot hold — a null, or a table inside an array — which no home config field
/// is; a key that cannot cross is left to the read-back check to report.
fn item_of(value: &Value) -> Option<Item> {
    Some(match value {
        Value::Object(fields) => {
            let mut table = Table::new();
            for (field, value) in fields {
                table.insert(field, item_of(value)?);
            }
            Item::Table(table)
        }
        Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(scalar_of(item)?);
            }
            Item::Value(toml_edit::Value::Array(array))
        }
        scalar => Item::Value(scalar_of(scalar)?),
    })
}

/// A string field as a TOML string.
fn text(value: &str) -> Item {
    toml_edit::value(value)
}

/// A token count as a TOML integer.
fn count(value: usize) -> Item {
    toml_edit::value(value as i64)
}

/// A temperature as the human would write it: `0.3`, not the widened
/// `0.30000001192092896` a `f32` becomes once it is the `f64` TOML stores — and
/// always a float, since an integer `0` would not read back into an `f32` field.
fn decimal(value: f32) -> Item {
    match format!("{value:?}").parse::<toml_edit::Value>() {
        Ok(float) if float.is_float() => Item::Value(float),
        _ => Item::Value(toml_edit::Value::from(f64::from(value))),
    }
}

/// A map that holds only maps is a grouping, not a section of its own: marking
/// it implicit writes `[providers.home.models.lfm2-vl]` rather than an empty
/// `[providers.home.models]` header above it. `top` exempts the field's own
/// table — its header is where the `#` line above the field goes.
fn implicits(item: &mut Item, top: bool) {
    let Item::Table(table) = item else {
        return;
    };
    let mut only_tables = !top && !table.is_empty();
    for (_, inner) in table.iter_mut() {
        implicits(inner, false);
        only_tables &= inner.is_table();
    }
    table.set_implicit(only_tables);
}

/// The file's `api_keys` table, cloned so the entries this save does not speak
/// for survive it — every other host's key, verbatim. An inline table
/// (`api_keys = { … }`) is read the same way.
fn api_keys_table(doc: &DocumentMut) -> Table {
    match doc.get("api_keys") {
        Some(Item::Table(table)) => table.clone(),
        Some(Item::Value(toml_edit::Value::InlineTable(inline))) => inline.clone().into_table(),
        _ => Table::new(),
    }
}

/// Drop the comment block at the top of the file: mush's [`PREAMBLE`] lives
/// there, and a save writes a fresh one.
///
/// The block ends at the first blank line. That is what keeps a human's comment
/// above a key — which sits below that blank line — from being mistaken for the
/// header and rewritten; and it is why the header is separated from the keys at
/// all (finding C12).
fn strip_preamble(text: &str) -> &str {
    let mut end = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return match end {
                0 => text,
                _ => &text[(end + line.len()).min(text.len())..],
            };
        }
        if !trimmed.starts_with('#') {
            // A key before any blank line is a file with no header of ours.
            return text;
        }
        end += line.len();
    }
    text
}

/// The lines a human stacked above a key, minus the last one — which is the
/// line mush keeps current.
fn leading_lines(doc: &DocumentMut, field: &str) -> String {
    let prefix = doc
        .key(field)
        .and_then(|key| key.leaf_decor().prefix())
        .and_then(|prefix| prefix.as_str())
        .unwrap_or("");
    split_last_comment(prefix).0.to_string()
}

/// A key's leading prefix, split at its last comment line: that line is mush's
/// (one `#` line per key, the header this file has instead of `_comment`), and
/// everything above it is the human's own.
fn split_last_comment(prefix: &str) -> (&str, Option<&str>) {
    let mut at = 0;
    let mut last = None;
    for line in prefix.split_inclusive('\n') {
        if line.trim_start().starts_with('#') {
            last = Some((at, at + line.len()));
        }
        at += line.len();
    }
    match last {
        Some((start, stop)) => (&prefix[..start], Some(&prefix[start..stop])),
        None => (prefix, None),
    }
}

/// Mush's line above a section, keeping the blank line that separates it from
/// the values above and whatever the human stacked above mush's own line — the
/// same rule a value key's line follows, on the decor a section header reads.
fn section_comment(table: &mut Table, help: &str) {
    let prefix = table
        .decor()
        .prefix()
        .and_then(|prefix| prefix.as_str())
        .unwrap_or("")
        .to_string();
    let above = match split_last_comment(&prefix).0 {
        "" => "\n",
        above => above,
    }
    .to_string();
    table.decor_mut().set_prefix(format!("{above}# {help}\n"));
}

/// Write one field over whatever the file held, with mush's line above it.
///
/// The human's own lines above a key are kept — only the last one, the one
/// directly above the key, is mush's to replace — because that is the promise
/// the preamble makes about their comments.
fn set_field(doc: &mut DocumentMut, field: &str, mut item: Item) {
    implicits(&mut item, true);
    let above = leading_lines(doc, field);
    let help = field_help(field);
    doc.insert(field, item);
    if let Some(Item::Table(table)) = doc.get_mut(field) {
        // A table is written as its own section, and the line above a section
        // belongs to the table's decor — not to the key's, which is where a
        // value's line lives.
        if let Some(help) = help {
            section_comment(table, &help);
        }
        return;
    }
    if let Some(help) = help {
        if let Some(mut key) = doc.key_mut(field) {
            key.leaf_decor_mut()
                .set_prefix(format!("{above}# {help}\n"));
        }
    }
}

/// Put mush's line above a key the file holds and this save does not state: the
/// header is mush's and is kept current, while the field's own value — and the
/// human's lines above mush's — stay exactly as they were.
fn set_comment(doc: &mut DocumentMut, field: &str) {
    let Some(help) = field_help(field) else {
        return;
    };
    let above = leading_lines(doc, field);
    if let Some(Item::Table(table)) = doc.get_mut(field) {
        section_comment(table, &help);
        return;
    }
    if let Some(mut key) = doc.key_mut(field) {
        key.leaf_decor_mut()
            .set_prefix(format!("{above}# {help}\n"));
    }
}

/// Move a home config mush cannot use beside itself, before a save that would
/// replace it.
///
/// Why this road keeps a copy is its own: the file holds the human's key and
/// their settings, and the first `/key`, `/url` or picker would replace what
/// mush could not read. The name is the one numbering rule both store files
/// take ([`crate::workspace::backup_name`]): the next free `.bak` name, with
/// one bound for both roads, because a copy already beside the file is one the
/// human already needed and this must not be the second accident. A failure is
/// returned, and the caller refuses the write: a save that cannot keep what it
/// is about to replace must not replace it.
fn keep_unparsable(path: &Path) -> std::io::Result<PathBuf> {
    let to = crate::workspace::backup_name(path).map_err(std::io::Error::other)?;
    fs::rename(path, &to)?;
    Ok(to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::{Held, Scratch};

    fn temp_path(name: &str) -> Held<PathBuf> {
        // A root of its own, and the guard comes back with the path: a
        // leftover from an interrupted run must not merge into this one, and
        // this run's file must go when the test does.
        let dir = Scratch::new(&format!("userconfig-{name}"));
        let path = dir.path().join("config.toml");
        dir.hold(path)
    }

    /// The header the written file carries — every `#` line above the first
    /// blank line — so a test can assert on what a human opening the file would
    /// read first (finding C12).
    fn header_of(path: &Path) -> String {
        let written = fs::read_to_string(path).unwrap();
        let header: Vec<&str> = written
            .lines()
            .take_while(|line| !line.trim().is_empty())
            .collect();
        assert!(
            header.iter().all(|line| line.starts_with('#')),
            "the header is a block of comments: {written}"
        );
        header.join("\n")
    }

    /// Every `#` line of the written file, header and per-key lines together.
    fn comments_of(path: &Path) -> String {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|line| line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A machine with no home has no home config path — and never the cwd's
    /// own (finding IN14).
    ///
    /// The relative fallback this replaced (`.mush-user-config.json`) was
    /// reachable exactly when `$MUSH_CONFIG` was unset and the platform config
    /// directory was `None` — on Unix, no `$XDG_CONFIG_HOME` and no home the
    /// passwd database can name — and it put the API key in whatever directory
    /// mush was launched from. The decision takes its two inputs as parameters
    /// so both answers can be pinned without mutating the process's
    /// environment.
    #[test]
    fn a_machine_with_no_home_has_no_home_config_path() {
        assert_eq!(
            config_path_from(None, None),
            None,
            "no override and no config directory is no path, never a relative one"
        );
        assert_eq!(
            config_path_from(None, Some(PathBuf::from("/home/someone/.config"))),
            Some(PathBuf::from("/home/someone/.config/mush/config.toml")),
            "the platform config directory is the one road"
        );
        assert_eq!(
            config_path_from(Some(OsString::from("beside-me.toml")), None),
            Some(PathBuf::from("beside-me.toml")),
            "MUSH_CONFIG is the human's own road, taken as given"
        );
        assert_eq!(
            config_path_from(Some(OsString::from("beside-me.json")), None),
            Some(PathBuf::from("beside-me.toml")),
            "a `.json` name is the legacy file: the TOML beside it is what mush writes"
        );
    }

    /// The read on that machine is not "a file that was not there": the
    /// complaint names the missing home, so the defaults never arrive in
    /// silence and a human can fix the environment (finding IN14).
    #[test]
    fn a_machine_with_no_home_config_says_home_is_not_set() {
        let loaded = UserConfig::load_at(None);
        assert_eq!(loaded.config.api_key, None, "the defaults are the values");
        let complaint = loaded.complaint.expect("there is something to say");
        assert!(
            complaint.contains("HOME is not set"),
            "the sentence names the environment to fix: {complaint}"
        );
        assert!(
            !complaint.contains(".mush-user-config.json"),
            "and never a path in the cwd: {complaint}"
        );
    }

    /// The file mush writes reads back as the values it was given, and carries
    /// the header that explains what a hand-edit may set.
    #[test]
    fn roundtrips_via_explicit_path() {
        let path = temp_path("roundtrip");
        let user = UserConfig {
            api_key: Some("sk-test-1234".into()),
            provider: "deepseek".into(),
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-flash".into(),
            ..UserConfig::default()
        };
        user.save_to(&path, KeyWrite::Stated).unwrap();
        let loaded = UserConfig::load_from(&path);
        assert!(loaded.complaint.is_none(), "the file read");
        let loaded = loaded.config;
        assert_eq!(
            loaded.api_key, None,
            "the key lands under its host, not flat"
        );
        assert_eq!(
            loaded.api_keys.get("api.deepseek.com").map(String::as_str),
            Some("sk-test-1234")
        );
        assert_eq!(loaded.provider, "deepseek");
        assert_eq!(loaded.model, "deepseek-flash");

        // The file is TOML with real comments now: the `_comment` key is gone,
        // the preamble is a four-line block at the top, and each key the save
        // states carries its own one-line explanation (finding C12).
        let written = fs::read_to_string(&path).unwrap();
        assert!(
            !written.contains(COMMENT_KEY),
            "the old `_comment` header key is gone: {written}"
        );
        let header = header_of(&path);
        assert!(
            header.contains("CLI flags > MUSH_* environment"),
            "the precedence is in the file, not only in the docs: {header}"
        );
        assert!(
            header.contains("MUSH_CONFIG") && header.contains("--print-config"),
            "the preamble names the two commands a hand-editor needs: {header}"
        );
        let comments = comments_of(&path);
        // Every field this save states is documented, on its own line — and the
        // ones it does not state have no key to be documented above.
        for field in ["provider", "base_url", "model", "api_keys"] {
            assert!(
                comments.contains(&format!("# {field}: ")),
                "`{field}` is documented above its key: {comments}"
            );
        }
        // The provider line is spelled from the table, so it offers every name
        // a `/provider` could select — never a stale one.
        for spec in crate::provider::PROVIDERS {
            assert!(
                comments.contains(spec.name),
                "`{}` is offered in the file's own header: {comments}",
                spec.name
            );
        }
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A home config that is *there* and cannot be used is the defaults with a
    /// complaint — never silence — and the save that follows keeps the human's
    /// bytes beside the file it is about to replace (finding C3): the key and
    /// the settings in an unreadable file used to be dropped without a word,
    /// and the first `/key`, `/url` or picker replaced the file with mush's own
    /// fields.
    #[test]
    fn an_unreadable_home_config_is_said_and_kept() {
        let path = temp_path("unreadable");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = "{ \"api_key\": \"sk-secret-0123456789\", ";
        fs::write(&path, original).unwrap();

        let loaded = UserConfig::load_from(&path);
        let complaint = loaded
            .complaint
            .clone()
            .expect("a file that is there and cannot be read is not silence");
        assert!(
            complaint.contains(&path.display().to_string()),
            "{complaint}"
        );
        assert!(complaint.contains("using defaults"), "{complaint}");
        assert!(
            loaded.config.api_key.is_none(),
            "nothing is taken from a file mush could not read"
        );

        // The save states the connection and cannot merge into this file, so
        // the original goes to `<name>.bak` first: a save must not be the thing
        // that loses the human's key.
        let saved = UserConfig {
            provider: "deepseek".into(),
            base_url: "https://api.deepseek.com".into(),
            ..UserConfig::default()
        };
        saved.save_to(&path, KeyWrite::Stated).unwrap();
        let backup = PathBuf::from(format!("{}.bak", path.display()));
        assert_eq!(
            fs::read_to_string(&backup).unwrap(),
            original,
            "the human's bytes are beside the file, not gone"
        );
        let reloaded = UserConfig::load_from(&path);
        assert!(reloaded.complaint.is_none(), "what mush wrote reads back");
        assert_eq!(reloaded.config.provider, "deepseek");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// The home config's copy is numbered by the one rule both store files take
    /// ([`crate::workspace::backup_name`]): a second unparsable file goes to
    /// `.bak.2`, never over the first copy — even though the two roads keep
    /// their copies for different reasons, this one for the key and the
    /// settings the file holds.
    #[test]
    fn a_second_unparsable_config_does_not_overwrite_the_first() {
        let path = temp_path("second-backup");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let saved = UserConfig {
            provider: "deepseek".into(),
            ..UserConfig::default()
        };

        fs::write(&path, "{ \"api_key\": \"sk-first\", ").unwrap();
        saved.save_to(&path, KeyWrite::Stated).unwrap();
        fs::write(&path, "{ \"api_key\": \"sk-second\", ").unwrap();
        saved.save_to(&path, KeyWrite::Stated).unwrap();

        assert_eq!(
            fs::read_to_string(format!("{}.bak", path.display())).unwrap(),
            "{ \"api_key\": \"sk-first\", "
        );
        assert_eq!(
            fs::read_to_string(format!("{}.bak.2", path.display())).unwrap(),
            "{ \"api_key\": \"sk-second\", "
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_file_is_defaults() {
        let loaded = UserConfig::load_from(Path::new("/nonexistent/mush/config.toml"));
        assert!(
            loaded.complaint.is_none(),
            "a machine that never wrote one has nothing to complain about"
        );
        let user = loaded.config;
        assert!(user.api_key.is_none());
        assert!(user.provider.is_empty());
    }

    /// The four-field file that mush wrote before this file grew keeps loading
    /// and keeps meaning exactly what it meant; the new fields are simply not
    /// stated.
    #[test]
    fn the_old_four_field_file_still_loads() {
        let path = temp_path("old");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"api_key = "sk-old"
provider = "deepseek"
base_url = "https://api.deepseek.com"
model = "deepseek-flash"
"#,
        )
        .unwrap();

        let user = UserConfig::load_from(&path).config;
        assert_eq!(user.api_key.as_deref(), Some("sk-old"));
        assert_eq!(user.provider, "deepseek");
        assert_eq!(user.base_url, "https://api.deepseek.com");
        assert_eq!(user.model, "deepseek-flash");
        assert_eq!(user.context, None);
        assert_eq!(user.temperature, None);
        assert_eq!(user.max_completion_tokens, None);
        // The thinking knobs are unstated too, which is what leaves the
        // provider's own defaults in charge.
        assert_eq!(user.reasoning_effort, None);
        assert_eq!(user.thinking, None);
        // A file written before `providers` existed has no rows, and that is
        // absence rather than a failure: the built-ins are the whole table.
        assert!(
            user.providers.is_empty(),
            "no rows is every file mush wrote before the map existed"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// The row shape a test writes: a home box with one annotated model, the
    /// case the whole map exists for.
    fn home_row() -> ProviderConfig {
        ProviderConfig {
            base_url: "http://rubendpc:8078".into(),
            models: BTreeMap::from([(
                "lfm2-vl".to_string(),
                ModelConfig {
                    vision: Some(true),
                    context: Some(32_768),
                },
            )]),
            model: "lfm2-vl".into(),
            thinking: Some(false),
            reasoning_effort: Some("high".into()),
            temperature: Some(0.2),
            context: Some(32_768),
        }
    }

    /// A named provider's row is a field like any other: what a human writes is
    /// what comes back, the annotation set included — a save that lost the
    /// annotations would turn every stated capability into `unknown` on the
    /// next start.
    #[test]
    fn a_named_provider_row_round_trips_through_the_file() {
        let path = temp_path("providers-round-trip");
        let user = UserConfig {
            providers: BTreeMap::from([("home".to_string(), home_row())]),
            ..UserConfig::default()
        };
        user.save_to(&path, KeyWrite::Keep).unwrap();

        let loaded = UserConfig::load_from(&path);
        assert!(loaded.complaint.is_none(), "{:?}", loaded.complaint);
        assert_eq!(loaded.config.providers, user.providers);
        // The file explains itself: the header names the map and says the one
        // thing a human editing it must not get wrong (no keys, and an
        // unannotated model is not assumed blind).
        let comments = comments_of(&path);
        assert!(comments.contains("# providers: "), "{comments}");
        assert!(comments.contains("not assumed blind"), "{comments}");
        assert!(
            comments.contains("api_keys` entry for its host"),
            "{comments}"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// The whole `providers` object is one statement, like the key map and
    /// unlike the scalar fields: a save that states rows replaces the file's
    /// map, and a save that states none — every runtime command, whose acks say
    /// nothing about a provider — keeps it. Per-entry merging is precisely what
    /// would let a command that never saw the file delete a row.
    #[test]
    fn a_save_states_the_whole_providers_map_and_keeps_it_when_the_value_states_none() {
        let path = temp_path("providers-merge");
        let written = UserConfig {
            providers: BTreeMap::from([("home".to_string(), home_row())]),
            ..UserConfig::default()
        };
        written.save_to(&path, KeyWrite::Keep).unwrap();

        // A `/url`-shaped save: it states the connection and nothing about rows.
        UserConfig {
            base_url: "http://box:9".into(),
            ..UserConfig::default()
        }
        .save_to(&path, KeyWrite::Keep)
        .unwrap();
        let loaded = UserConfig::load_from(&path).config;
        assert_eq!(
            loaded.providers, written.providers,
            "a save that states no rows keeps the file's"
        );

        // A save that states rows: the value's map is the whole statement.
        let replaced = UserConfig {
            providers: BTreeMap::from([(
                "work".to_string(),
                ProviderConfig {
                    base_url: "https://api.example.com/v1".into(),
                    ..ProviderConfig::default()
                },
            )]),
            ..UserConfig::default()
        };
        replaced.save_to(&path, KeyWrite::Keep).unwrap();
        let loaded = UserConfig::load_from(&path).config;
        assert_eq!(
            loaded.providers, replaced.providers,
            "the value's map replaces the file's, annotations and all"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A row that states nothing is a name and nothing else — an empty object
    /// loads, so a human can add one and fill it in later — and an annotation
    /// that states only one fact states only that one.
    #[test]
    fn an_empty_row_and_a_one_fact_annotation_load() {
        let path = temp_path("providers-partial");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"[providers.bare]

[providers.home.models.lfm2-vl]
vision = true
"#,
        )
        .unwrap();

        let user = UserConfig::load_from(&path);
        assert!(user.complaint.is_none(), "{:?}", user.complaint);
        let home = user.config.providers.get("home").expect("the row loaded");
        let facts = home.models.get("lfm2-vl").expect("the annotation loaded");
        assert_eq!(facts.vision, Some(true));
        assert_eq!(
            facts.context, None,
            "a fact the human did not state is not a number mush invented"
        );
        assert_eq!(home.base_url, "", "and the row states no endpoint");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// Every field loads, a key a newer mush invented is ignored rather than
    /// fatal, and saving keeps it: nobody's settings are destroyed by a
    /// downgrade.
    #[test]
    fn every_field_loads_and_unknown_keys_are_kept() {
        let path = temp_path("every-field");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"_comment = ["written by a newer mush"]
api_key = "sk-all"
provider = "custom"
base_url = "http://host:1"
model = "m"
context = 64000
temperature = 0.3
max_completion_tokens = true
reasoning_effort = "low"
thinking = false
future_knob = { a = 1 }

[api_keys]
"old:9" = "sk-map"

[providers.home]
base_url = "http://rubendpc:8078"

[providers.home.models.lfm2-vl]
vision = true
"#,
        )
        .unwrap();

        let user = UserConfig::load_from(&path).config;
        assert_eq!(user.api_key.as_deref(), Some("sk-all"));
        assert_eq!(
            user.api_keys.get("old:9").map(String::as_str),
            Some("sk-map")
        );
        assert_eq!(user.provider, "custom");
        assert_eq!(user.base_url, "http://host:1");
        assert_eq!(user.model, "m");
        assert_eq!(user.context, Some(64_000));
        assert_eq!(user.temperature, Some(0.3));
        assert_eq!(user.max_completion_tokens, Some(true));
        assert_eq!(user.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(user.thinking, Some(false));
        assert_eq!(
            user.providers
                .get("home")
                .and_then(|row| row.models.get("lfm2-vl"))
                .and_then(|facts| facts.vision),
            Some(true),
            "a row and its annotations are fields of the value like any other"
        );
        let saved = UserConfig {
            api_key: Some("sk-new".into()),
            provider: "custom".into(),
            base_url: "http://host:1".into(),
            model: "m".into(),
            ..UserConfig::default()
        };
        saved.save_to(&path, KeyWrite::Stated).unwrap();
        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(
            reloaded.api_keys.get("host:1").map(String::as_str),
            Some("sk-new"),
            "the stated key lands under the endpoint's host"
        );
        assert_eq!(reloaded.api_key, None, "and not flat");
        assert_eq!(reloaded.context, Some(64_000));
        assert_eq!(reloaded.temperature, Some(0.3));
        assert_eq!(reloaded.max_completion_tokens, Some(true));
        assert_eq!(reloaded.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(reloaded.thinking, Some(false));
        assert_eq!(
            reloaded.providers, user.providers,
            "the rows and their annotations survive a save that states none"
        );
        assert_eq!(
            reloaded.api_keys.get("old:9").map(String::as_str),
            Some("sk-map"),
            "a host this save did not speak for keeps its entry"
        );
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("future_knob"), "{written}");
        assert!(header_of(&path).contains("CLI flags > MUSH_* environment"));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A stated key is written under the host of the endpoint it was given —
    /// the authority, path and scheme stripped — and the legacy flat field is
    /// cleared: the key now lives beside its true host, and leaving it flat
    /// would let a later host adopt it (findings C6, D6).
    #[test]
    fn a_stated_key_lands_under_the_host_of_its_endpoint() {
        let path = temp_path("stated-host");
        let user = UserConfig {
            api_key: Some("sk-hosted".into()),
            base_url: "https://api.deepseek.com/v1/".into(),
            ..UserConfig::default()
        };
        user.save_to(&path, KeyWrite::Stated).unwrap();
        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(
            reloaded
                .api_keys
                .get("api.deepseek.com")
                .map(String::as_str),
            Some("sk-hosted")
        );
        assert_eq!(
            reloaded.api_key, None,
            "the flat field is cleared once the key is keyed"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A save that states a key for one host keeps every other host's entry:
    /// switching the endpoint no longer forgets the key of the host left behind.
    #[test]
    fn a_second_hosts_key_keeps_the_first_hosts_entry() {
        let path = temp_path("two-hosts");
        UserConfig {
            api_key: Some("sk-a".into()),
            base_url: "http://a:1".into(),
            ..UserConfig::default()
        }
        .save_to(&path, KeyWrite::Stated)
        .unwrap();
        UserConfig {
            api_key: Some("sk-b".into()),
            base_url: "http://b:2".into(),
            ..UserConfig::default()
        }
        .save_to(&path, KeyWrite::Stated)
        .unwrap();

        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(
            reloaded.api_keys.get("a:1").map(String::as_str),
            Some("sk-a")
        );
        assert_eq!(
            reloaded.api_keys.get("b:2").map(String::as_str),
            Some("sk-b")
        );
        assert_eq!(reloaded.api_key, None);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// The road a save takes the key by is the caller's: [`KeyWrite::Keep`]
    /// writes the file's own map and flat key back — present or absent — while
    /// every other field the value states still lands. It is the road a save an
    /// unrelated command triggers takes, so a key that only the environment
    /// holds never becomes a file's key (finding C11); [`KeyWrite::Stated`] is
    /// the road `/key` and a host change take.
    #[test]
    fn a_keep_save_leaves_the_files_own_key_where_it_was() {
        let path = temp_path("keep-key");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let with_key = UserConfig {
            api_key: Some("sk-file-0123456789".into()),
            provider: "custom".into(),
            base_url: "http://old:1".into(),
            ..UserConfig::default()
        };
        with_key.save_to(&path, KeyWrite::Stated).unwrap();

        // The save a `/model` makes, with a key that came from the environment:
        // the file's own key stays exactly where it was, the model lands.
        let picked = UserConfig {
            api_key: Some("sk-env-0123456789".into()),
            provider: "custom".into(),
            base_url: "http://old:1".into(),
            model: "another".into(),
            ..UserConfig::default()
        };
        picked.save_to(&path, KeyWrite::Keep).unwrap();
        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(
            reloaded.api_keys.get("old:1").map(String::as_str),
            Some("sk-file-0123456789"),
            "the file's own key is not replaced by the value's"
        );
        assert_eq!(reloaded.api_key, None, "and its flat field is untouched");
        assert_eq!(reloaded.model, "another", "the other fields still land");

        // A file that held no key keeps holding none: `Keep` is silence about
        // the key, not the value's key written under another name.
        let bare = temp_path("keep-key-bare");
        fs::create_dir_all(bare.parent().unwrap()).unwrap();
        picked.save_to(&bare, KeyWrite::Keep).unwrap();
        let reloaded = UserConfig::load_from(&bare).config;
        assert_eq!(reloaded.api_key, None, "no file key, no key written");
        assert!(reloaded.api_keys.is_empty(), "and no entry invented");
        assert_eq!(reloaded.model, "another");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let _ = fs::remove_dir_all(bare.parent().unwrap());
    }

    /// `Keep` leaves the *whole* key structure the file had: a file that holds
    /// both a map entry and the older flat key keeps both, byte for byte, even
    /// when the value being saved holds a different key entirely.
    #[test]
    fn a_keep_save_leaves_the_whole_key_structure_untouched() {
        let path = temp_path("keep-both");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"api_key = "sk-flat"
base_url = "http://a:1"
model = "old"

[api_keys]
"a:1" = "sk-a"
"#,
        )
        .unwrap();

        UserConfig {
            api_key: Some("sk-other".into()),
            base_url: "http://b:2".into(),
            model: "new".into(),
            // A value that carries its own map must not stand over the file's.
            api_keys: [("b:2".to_string(), "sk-other".to_string())]
                .into_iter()
                .collect(),
            ..UserConfig::default()
        }
        .save_to(&path, KeyWrite::Keep)
        .unwrap();

        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(reloaded.api_key.as_deref(), Some("sk-flat"));
        assert_eq!(
            reloaded.api_keys.get("a:1").map(String::as_str),
            Some("sk-a")
        );
        assert_eq!(
            reloaded.api_keys.len(),
            1,
            "the value's own map is not written: {reloaded:?}"
        );
        assert_eq!(reloaded.model, "new", "the fields the save owns still land");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A save that states `None` says this host has no key — and says it by
    /// there being no entry, never by deleting another host's. The save that
    /// does this is the one that just moved the endpoint and dropped the key
    /// (findings C6, D6); the host it left keeps its own. Every other unstated
    /// field keeps its old value.
    #[test]
    fn a_stated_none_deletes_no_other_hosts_entry() {
        let path = temp_path("no-key");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let with_key = UserConfig {
            api_key: Some("sk-old-0123456789".into()),
            provider: "custom".into(),
            base_url: "http://old:1".into(),
            context: Some(32_000),
            ..UserConfig::default()
        };
        with_key.save_to(&path, KeyWrite::Stated).unwrap();

        // The save after a host change: the endpoint moves, no key comes with
        // it — but the old host's entry stays, and the window is untouched.
        let moved = UserConfig {
            provider: "deepseek".into(),
            base_url: "https://api.deepseek.com".into(),
            ..UserConfig::default()
        };
        moved.save_to(&path, KeyWrite::Stated).unwrap();
        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(
            reloaded.api_keys.get("old:1").map(String::as_str),
            Some("sk-old-0123456789"),
            "the host just left keeps its own key"
        );
        assert_eq!(
            reloaded.api_keys.get("api.deepseek.com"),
            None,
            "and the new host has no entry"
        );
        assert_eq!(reloaded.api_key, None, "the flat field stays empty");
        assert_eq!(reloaded.base_url, "https://api.deepseek.com");
        assert_eq!(reloaded.context, Some(32_000), "the merge still merges");

        // A `None` for the *same* host it already has does not delete its own
        // entry either: silence about the key is the absence of one, not a cut.
        moved.save_to(&path, KeyWrite::Stated).unwrap();
        let again = UserConfig::load_from(&path).config;
        assert_eq!(again.api_keys.len(), 1, "the entry survived both saves");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// The four-field file has no map: its key is flat, and it stays flat until
    /// the next `Stated` save — one that states a key for the current host moves
    /// it under that host and clears the flat field, and one that states none
    /// migrates it under the host the file's own `base_url` names. This is the
    /// road that turns an old file into a map without losing the key.
    #[test]
    fn the_old_flat_file_migrates_under_its_host_on_the_next_stated_save() {
        let path = temp_path("migrate");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"api_key = "sk-old"
provider = "deepseek"
base_url = "https://api.deepseek.com"
model = "deepseek-flash"
"#,
        )
        .unwrap();

        let loaded = UserConfig::load_from(&path).config;
        assert!(loaded.api_keys.is_empty(), "an old file has no map");
        assert_eq!(loaded.api_key.as_deref(), Some("sk-old"));

        UserConfig {
            api_key: loaded.api_key.clone(),
            provider: loaded.provider.clone(),
            base_url: loaded.base_url.clone(),
            model: loaded.model.clone(),
            ..UserConfig::default()
        }
        .save_to(&path, KeyWrite::Stated)
        .unwrap();

        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(
            reloaded
                .api_keys
                .get("api.deepseek.com")
                .map(String::as_str),
            Some("sk-old"),
            "the flat key moved under its own host"
        );
        assert_eq!(reloaded.api_key, None, "and no flat key was left behind");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A stated key with no endpoint to key on is written flat, as the old file
    /// did: the map has no host to put it under, and the flat field is the only
    /// shape that carries it. The map's other hosts are still preserved.
    #[test]
    fn a_stated_key_with_no_endpoint_is_written_flat() {
        let path = temp_path("no-endpoint");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[api_keys]\n\"a:1\" = \"sk-a\"\n").unwrap();

        UserConfig {
            api_key: Some("sk-homeless".into()),
            ..UserConfig::default()
        }
        .save_to(&path, KeyWrite::Stated)
        .unwrap();

        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(reloaded.api_key.as_deref(), Some("sk-homeless"));
        assert_eq!(
            reloaded.api_keys.get("a:1").map(String::as_str),
            Some("sk-a")
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A save never writes `"api_key": null`: a file that never had a flat key
    /// gets no `api_key` field at all, whichever road the save takes. An absent
    /// field reads back as `None`, and the null every `/url`, `/model` or
    /// `/provider` used to leave behind was exactly the noise the legacy field
    /// was supposed to shed.
    #[test]
    fn a_save_never_writes_a_null_flat_key() {
        let keep = temp_path("no-null-keep");
        UserConfig {
            base_url: "http://a:1".into(),
            model: "m".into(),
            ..UserConfig::default()
        }
        .save_to(&keep, KeyWrite::Keep)
        .unwrap();
        let written = fs::read_to_string(&keep).unwrap();
        assert!(
            !written.contains("api_key = "),
            "a Keep save writes no flat field: {written}"
        );
        assert_eq!(UserConfig::load_from(&keep).config.api_key, None);

        let stated = temp_path("no-null-stated");
        UserConfig {
            base_url: "http://a:1".into(),
            model: "m".into(),
            ..UserConfig::default()
        }
        .save_to(&stated, KeyWrite::Stated)
        .unwrap();
        let written = fs::read_to_string(&stated).unwrap();
        assert!(
            !written.contains("api_key = "),
            "a Stated+None save writes no flat field: {written}"
        );
        assert_eq!(UserConfig::load_from(&stated).config.api_key, None);
        let _ = fs::remove_dir_all(keep.parent().unwrap());
        let _ = fs::remove_dir_all(stated.parent().unwrap());
    }

    /// `Keep` is silence about the key: a legacy flat key is neither migrated nor
    /// rewritten, and no null stands in its place. The map it sits beside is
    /// restored untouched too.
    #[test]
    fn a_keep_save_does_not_migrate_a_legacy_flat_key() {
        let path = temp_path("keep-legacy");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"api_key = "sk-legacy"
base_url = "http://a:1"

[api_keys]
"b:2" = "sk-b"
"#,
        )
        .unwrap();

        UserConfig {
            base_url: "http://a:1".into(),
            model: "m".into(),
            ..UserConfig::default()
        }
        .save_to(&path, KeyWrite::Keep)
        .unwrap();

        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(
            reloaded.api_key.as_deref(),
            Some("sk-legacy"),
            "the flat key stays flat: a Keep save is not a statement about it"
        );
        assert_eq!(
            reloaded.api_keys.len(),
            1,
            "no entry was minted for its host"
        );
        assert_eq!(
            reloaded.api_keys.get("b:2").map(String::as_str),
            Some("sk-b")
        );
        assert_eq!(reloaded.model, "m", "the fields the save owns still land");
        let written = fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("api_key = \"sk-legacy\""),
            "the legacy string is still what the field holds: {written}"
        );
        assert!(
            !written.contains("api_key = null"),
            "and no null stands in for it: {written}"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A save that states no key still rescues a legacy flat one: the file's own
    /// `base_url` is the only host it can belong to, so the key moves there and
    /// the flat field goes — a save must not drop a key it did not state. A
    /// second such save neither duplicates the entry nor overwrites it.
    #[test]
    fn a_stated_none_save_migrates_a_legacy_flat_key_under_the_files_own_host() {
        let path = temp_path("migrate-none");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"api_key = "sk-legacy"
provider = "custom"
base_url = "http://a:1/v1"
"#,
        )
        .unwrap();

        // The save a host change leaves behind: the file's own endpoint, and no
        // key stated for it.
        let stated_none = || UserConfig {
            provider: "custom".into(),
            base_url: "http://a:1/v1".into(),
            ..UserConfig::default()
        };
        stated_none().save_to(&path, KeyWrite::Stated).unwrap();

        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(
            reloaded.api_keys.get("a:1").map(String::as_str),
            Some("sk-legacy"),
            "the flat key moved under the host the file's own base_url names"
        );
        assert_eq!(reloaded.api_key, None, "and no flat key was left behind");
        let written = fs::read_to_string(&path).unwrap();
        assert!(
            !written.contains("api_key = "),
            "the flat field is gone, not null: {written}"
        );

        stated_none().save_to(&path, KeyWrite::Stated).unwrap();
        let again = UserConfig::load_from(&path).config;
        assert_eq!(again.api_keys.len(), 1, "the entry was not duplicated");
        assert_eq!(
            again.api_keys.get("a:1").map(String::as_str),
            Some("sk-legacy"),
            "and the second save did not overwrite it"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// No endpoint in the file is no host to migrate a legacy flat key to: the
    /// key is left exactly as it was rather than dropped or keyed on a guess.
    #[test]
    fn a_stated_none_save_with_no_endpoint_leaves_a_flat_key_alone() {
        let path = temp_path("no-host-migrate");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "api_key = \"sk-legacy\"\n").unwrap();

        UserConfig {
            model: "m".into(),
            ..UserConfig::default()
        }
        .save_to(&path, KeyWrite::Stated)
        .unwrap();

        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(
            reloaded.api_key.as_deref(),
            Some("sk-legacy"),
            "a key is never deleted on a guess"
        );
        assert!(reloaded.api_keys.is_empty(), "and no host was invented");
        assert_eq!(reloaded.model, "m");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A host the map already speaks for is not migrated onto: the flat key is
    /// left exactly as it is rather than replacing, or being replaced by, the
    /// entry that host already has.
    #[test]
    fn a_stated_none_save_does_not_migrate_over_an_existing_entry() {
        let path = temp_path("no-overwrite-migrate");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"api_key = "sk-flat"
base_url = "http://a:1"

[api_keys]
"a:1" = "sk-map"
"#,
        )
        .unwrap();

        UserConfig {
            base_url: "http://a:1".into(),
            ..UserConfig::default()
        }
        .save_to(&path, KeyWrite::Stated)
        .unwrap();

        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(reloaded.api_key.as_deref(), Some("sk-flat"));
        assert_eq!(
            reloaded.api_keys.get("a:1").map(String::as_str),
            Some("sk-map"),
            "the entry that host already has is not overwritten"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A stated key for another host does not delete a flat key the migration
    /// could not place: with no endpoint in the file there is no host to move it
    /// under, so it stays the fallback for a host the map does not name — no
    /// writer removes a key it did not state.
    #[test]
    fn a_stated_key_for_another_host_keeps_an_unplaceable_flat_key() {
        let path = temp_path("stated-other-host-keeps-flat");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "api_key = \"sk-legacy\"\n").unwrap();

        UserConfig {
            api_key: Some("sk-new".into()),
            base_url: "http://b:2".into(),
            ..UserConfig::default()
        }
        .save_to(&path, KeyWrite::Stated)
        .unwrap();

        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(
            reloaded.api_keys.get("b:2").map(String::as_str),
            Some("sk-new"),
            "the stated key lands under its own host"
        );
        assert_eq!(
            reloaded.api_key.as_deref(),
            Some("sk-legacy"),
            "and the flat key the migration could not place is still there"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// The file mush wrote before this change is JSON, and the first startup
    /// with TOML in its place converts it: the values cross verbatim — the flat
    /// `api_key` stays flat, a provider row keeps its annotations — the JSON is
    /// removed once the TOML has been read back and agrees, and a second load
    /// converts nothing (finding C12).
    #[test]
    fn a_legacy_config_json_migrates_on_the_first_load() {
        let dir = Scratch::new("userconfig-migrate-json");
        let json = dir.path().join("config.json");
        let toml = dir.path().join("config.toml");
        fs::write(
            &json,
            r#"{
  "api_key": "sk-flat-0123456789",
  "provider": "custom",
  "base_url": "http://rubendpc:8078",
  "model": "lfm2-vl",
  "temperature": 0.2,
  "providers": {
    "home": {
      "base_url": "http://rubendpc:8078",
      "models": { "lfm2-vl": { "vision": true, "context": 32768 } }
    }
  }
}"#,
        )
        .unwrap();

        let first = UserConfig::load_at(Some(toml.clone()));
        assert!(first.complaint.is_none(), "{:?}", first.complaint);
        let line = first.migrated.expect("the migration is said, not silent");
        assert!(line.contains(&toml.display().to_string()), "{line}");
        assert!(line.contains(&json.display().to_string()), "{line}");
        assert!(line.contains("gone"), "{line}");
        assert_eq!(
            first.config.api_key.as_deref(),
            Some("sk-flat-0123456789"),
            "the format changed, not the key: it is not moved under a host"
        );
        assert_eq!(first.config.temperature, Some(0.2));
        let facts = first
            .config
            .providers
            .get("home")
            .and_then(|row| row.models.get("lfm2-vl"))
            .expect("the row and its annotation crossed");
        assert_eq!(facts.vision, Some(true));
        assert_eq!(facts.context, Some(32_768));

        // The TOML is the file now: the old header key is gone, the preamble is
        // mush's, and the file is private because it holds a key.
        let written = fs::read_to_string(&toml).unwrap();
        assert!(!written.contains(COMMENT_KEY), "{written}");
        assert!(header_of(&toml).contains("MUSH_CONFIG"), "{written}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&toml).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the file holds a key: nobody else may read it");
        }
        assert!(
            !json.exists(),
            "the old file is gone, which is what the line says"
        );

        // A second load reads the TOML and converts nothing.
        let second = UserConfig::load_at(Some(toml.clone()));
        assert!(second.migrated.is_none(), "the migration runs once");
        assert!(second.complaint.is_none(), "{:?}", second.complaint);
        assert_eq!(second.config.api_key.as_deref(), Some("sk-flat-0123456789"));
    }

    /// A migration that cannot write leaves the JSON exactly where it was, says
    /// why, and the run still reads the values out of it: a format change that
    /// fails must not cost the human their settings, and must not leave a
    /// half-migrated TOML behind either (finding C12).
    #[cfg(unix)]
    #[test]
    fn a_migration_whose_write_fails_leaves_the_json_in_place() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Scratch::new("userconfig-migrate-fail");
        let json = dir.path().join("config.json");
        let toml = dir.path().join("config.toml");
        let original = r#"{"api_key": "sk-flat-0123456789"}"#;
        fs::write(&json, original).unwrap();
        // Nowhere to write the TOML: the directory refuses the temporary file
        // every atomic write starts with.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let loaded = UserConfig::load_at(Some(toml.clone()));
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();

        let complaint = loaded.complaint.expect("a failed migration is said");
        assert!(
            complaint.contains(&toml.display().to_string()),
            "{complaint}"
        );
        assert!(
            complaint.contains(&json.display().to_string()),
            "{complaint}"
        );
        assert!(
            !toml.exists(),
            "a migration that failed leaves no file of its own behind"
        );
        assert_eq!(
            fs::read_to_string(&json).unwrap(),
            original,
            "the old file is exactly where and what it was"
        );
        assert_eq!(
            loaded.config.api_key.as_deref(),
            Some("sk-flat-0123456789"),
            "and the run reads it, because it is still the human's config"
        );
        assert!(loaded.migrated.is_none(), "nothing was migrated");
    }

    /// What the human wrote is theirs: a `#` comment of their own and a table
    /// mush knows nothing about survive a save, and so does the value of a field
    /// the save does not state. The file is edited in place, not regenerated
    /// (finding C12).
    #[test]
    fn a_humans_comment_and_an_unknown_table_survive_a_save() {
        let path = temp_path("human-comments");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"model = "m"

# my own note, and it is about me
# temperature: I typed this line myself
temperature = 0.4

# a table a newer mush will read
[future_knob]
a = 1
"#,
        )
        .unwrap();

        UserConfig {
            model: "new".into(),
            context: Some(64_000),
            ..UserConfig::default()
        }
        .save_to(&path, KeyWrite::Keep)
        .unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("# my own note, and it is about me"),
            "the human's line above mush's stays: {written}"
        );
        assert!(
            written.contains("# a table a newer mush will read"),
            "and so does the one above their own table: {written}"
        );
        assert!(
            written.contains("[future_knob]") && written.contains("a = 1"),
            "an unknown table is kept, not rewritten: {written}"
        );
        // The line directly above a key mush documents is mush's, and it is
        // kept current — that is the header.
        assert!(
            !written.contains("# temperature: I typed this line myself"),
            "{written}"
        );
        assert!(written.contains("# temperature: 0.0-2.0"), "{written}");
        let reloaded = UserConfig::load_from(&path);
        assert!(reloaded.complaint.is_none(), "{:?}", reloaded.complaint);
        assert_eq!(
            reloaded.config.temperature,
            Some(0.4),
            "a field the save does not state keeps the file's value"
        );
        assert_eq!(reloaded.config.model, "new", "and the stated one lands");
        assert_eq!(reloaded.config.context, Some(64_000));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A host with a port is a legal TOML key only when it is quoted:
    /// `rubendpc:8078` is not a bare key, and a map written unquoted would not
    /// read back at all (finding C12).
    #[test]
    fn a_host_with_a_port_round_trips_as_a_quoted_key() {
        let path = temp_path("ported-host");
        let user = UserConfig {
            api_key: Some("sk-ported".into()),
            base_url: "http://rubendpc:8078".into(),
            ..UserConfig::default()
        };
        user.save_to(&path, KeyWrite::Stated).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("\"rubendpc:8078\" = \"sk-ported\""),
            "the key is quoted: {written}"
        );
        let reloaded = UserConfig::load_from(&path);
        assert!(reloaded.complaint.is_none(), "{:?}", reloaded.complaint);
        assert_eq!(
            reloaded
                .config
                .api_keys
                .get("rubendpc:8078")
                .map(String::as_str),
            Some("sk-ported")
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// The header is short: four lines at the top, one line per key the file
    /// carries, and the whole of it fits on a screen. The eleven paragraphs it
    /// replaced were the manual in a file a human opens to change one number;
    /// what a hand-editor needs in the file is the precedence, the two commands
    /// that name another file or show the resolution, where the reasons went,
    /// and the one rule `providers` must not get wrong (finding C12).
    #[test]
    fn the_header_is_short_and_names_the_commands() {
        let path = temp_path("short-header");
        UserConfig {
            provider: "custom".into(),
            base_url: "http://a:1".into(),
            api_key: Some("sk-short".into()),
            context: Some(32_768),
            temperature: Some(0.2),
            max_completion_tokens: Some(true),
            reasoning_effort: Some("low".into()),
            thinking: Some(false),
            providers: BTreeMap::from([("home".to_string(), home_row())]),
            ..UserConfig::default()
        }
        .save_to(&path, KeyWrite::Stated)
        .unwrap();

        let written = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = written
            .lines()
            .filter(|line| line.trim_start().starts_with('#'))
            .collect();
        // The preamble and one line per documented key: fourteen of fifteen
        // possible today, and the bound says a field may not grow a paragraph.
        assert!(
            lines.len() <= 16,
            "the header is not a document — {} comment lines: {written}",
            lines.len()
        );
        let size: usize = lines.iter().map(|line| line.len()).sum();
        assert!(
            size < 2_600,
            "the header fits on a screen — {size} bytes of comment: {written}"
        );
        let header = lines.join("\n");
        assert!(header.contains("MUSH_CONFIG"), "{header}");
        assert!(header.contains("--print-config"), "{header}");
        assert!(
            header.contains("docs/mush.md"),
            "the file says where the reasons are: {header}"
        );
        assert!(
            header.contains("No keys in this table"),
            "the one rule a `providers` editor must not get wrong: {header}"
        );
        // Both the names and the windows are spelled from the provider table,
        // so the file cannot offer a name or a number a request would not use.
        for spec in crate::provider::PROVIDERS {
            assert!(
                header.contains(spec.name),
                "`{}` is offered in the file's own header: {header}",
                spec.name
            );
            assert!(
                header.contains(&spec.fallback_context_tokens.to_string()),
                "`{}`'s default window is in the header: {header}",
                spec.name
            );
        }
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
