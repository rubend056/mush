//! User-level configuration stored in the home directory.
//!
//! Unlike the per-workspace session (`.mush/session.json`), this file is
//! machine-global: it is where the API key lives (the session deliberately
//! never stores secrets) and it holds the defaults a human would otherwise type
//! on every command line. Resolution order on startup is
//! `CLI flags > environment > saved session > this file > built-in defaults`.
//!
//! The file is meant to be hand-edited, so it is written to be readable without
//! this module: every field is optional, the file mush writes carries a
//! `_comment` header naming the precedence and each field, and an unknown key is
//! ignored (and kept) rather than fatal, so a newer mush can add one without
//! breaking an older one. `mush --print-config` prints what the layers above
//! resolved to.
//!
//! Path: `$MUSH_CONFIG`, else the platform config directory
//! (`$XDG_CONFIG_HOME/mush/config.json`, usually `~/.config/mush/config.json`
//! on Unix, Application Support on macOS, `%APPDATA%` on Windows). When
//! neither names one — `HOME` unset with no `MUSH_CONFIG`, and no home the
//! passwd database can name — there is no home config at all: mush refuses it
//! rather than fall back to a relative path in the directory it was launched
//! from (finding IN14).

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::workspace::{atomic_write, Fresh};

/// The key the self-documenting header travels under. A plain JSON key, not a
/// JSONC dialect: `_` sorts it to the top, every parser survives it, and it is
/// never read back — the fields below say what is editable.
const COMMENT_KEY: &str = "_comment";

/// What every file mush writes says about itself: the precedence, then the
/// fields, then the command that shows what they resolved to.
///
/// The provider names and the endpoint `custom` falls back to are spelled from
/// [`crate::provider::PROVIDERS`], so this header cannot offer a choice the
/// `/provider` picker would not.
fn comment() -> Vec<String> {
    use crate::provider::{names_hint, Provider, DEFAULT_PROVIDER};
    vec![
        "mush home config — hand-editable, and every field is optional.".to_string(),
        "Resolution: CLI flags > MUSH_* environment > this workspace's session > this file > built-in defaults.".to_string(),
        "api_key: the provider's secret; also read from MUSH_API_KEY, and a \
         control character in it is refused. Never written into a workspace."
            .to_string(),
        format!(
            "provider: {}; `{}` defaults to the local endpoint {}.",
            names_hint(),
            DEFAULT_PROVIDER.name(),
            Provider::Custom.default_base_url()
        ),
        "base_url: an OpenAI-compatible endpoint, without a trailing slash; a \
         control character in it is refused."
            .to_string(),
        "model: the model id to start with, when nothing above names one.".to_string(),
        format!(
            "context: a context window in tokens; the built-in default is {}. Stating it here beats what the endpoint advertises, as --context does.",
            crate::provider::context_default_hint()
        ),
        "temperature: 0.0-2.0, sent with every request; 1.0 is the model's own choice, and the default.".to_string(),
        format!(
            "max_completion_tokens: true sends the reply cap — {}, at most — as max_completion_tokens; OpenAI's reasoning models reject max_tokens.",
            crate::config::REPLY_SHARE_WORDS
        ),
        format!(
            "reasoning_effort: \"low\", \"high\" or \"max\" — exactly what the DeepSeek OpenAI format documents. A value here reaches any endpoint; the provider's own default is {}.",
            crate::provider::effort_default_hint()
        ),
        format!(
            "thinking: true asks for the provider's thinking mode (the request carries {{\"type\":\"enabled\"}}); false sends no thinking field at all and leaves the model's own default. The provider's own default is {}.",
            crate::provider::thinking_default_hint()
        ),
        "Keys mush does not know are ignored, and kept when mush rewrites this file. `mush --print-config` shows what these resolved to.".to_string(),
    ]
}

/// The machine-global defaults the precedence chain consults below the session.
///
/// Every field is optional: an absent or `null` field is simply not stated, and
/// a file with only the four connection fields still loads and still means what
/// it always meant. There are deliberately no knobs beyond the ones mush reads
/// (`Config`): no `param_style`, because the only parameter-name switch mush has
/// is the reply cap, which `max_completion_tokens` states directly, and no
/// `history_budget_multiplier`, because the history budget is a conservative
/// share of the window and the window is the knob that states it. A field
/// nothing reads would be a promise, not a setting.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct UserConfig {
    /// API key for the provider. Stored in plain text on your own machine;
    /// never written to the workspace.
    ///
    /// It is written by the roads that state one *for the file*: `/key`, and
    /// a save that keeps the file's own key where it is ([`KeyWrite`]). A key
    /// that reached the run through `MUSH_API_KEY` was deliberately kept out
    /// of files, and no save triggered by an unrelated command copies it in
    /// (finding C11).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Provider name as given by [`Provider::name()`](crate::provider::Provider::name),
    /// i.e. a name `--provider` accepts (see [`provider::PROVIDERS`](crate::provider::PROVIDERS)).
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    /// Context window in tokens. A window stated here is a statement, not a
    /// guess: it beats what the endpoint advertises, exactly like `--context`,
    /// and only a window this workspace remembers (the session) outranks it.
    #[serde(default)]
    pub context: Option<usize>,
    /// Sampling temperature sent with every request. Clamped by
    /// `Config::temperature`, so a typo is a value mush can still send rather
    /// than a request an endpoint rejects.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Send the reply cap as `max_completion_tokens` instead of `max_tokens`,
    /// which OpenAI's reasoning models require.
    #[serde(default)]
    pub max_completion_tokens: Option<bool>,
    /// Reasoning effort sent as `reasoning_effort`: "low", "high" or "max",
    /// exactly the values the DeepSeek OpenAI format documents. Stated here it
    /// is honoured wherever the endpoint is pointed; unstated, the provider's
    /// own documented default applies (`provider::PROVIDERS`), and a row that
    /// documents none is the only way the field is left off. A value mush does
    /// not know is reported at startup rather than sent.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Ask for the provider's thinking mode. `true` sends the `thinking` field
    /// a provider documents (`{"type":"enabled"}`); `false` sends no
    /// `thinking` field at all and leaves the model's own default. Unstated,
    /// the provider's own default applies (see `provider::PROVIDERS`).
    #[serde(default)]
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
        return Some(PathBuf::from(path));
    }
    config_dir.map(|dir| dir.join("mush/config.json"))
}

/// Where the user config lives: `MUSH_CONFIG` names it outright, and otherwise
/// it is the platform config directory (`$XDG_CONFIG_HOME/mush/config.json` on
/// Unix, the Application Support directory on macOS, `%APPDATA%` on Windows).
/// `None` is a machine with no home config at all — see the reason below.
///
/// The file is the *machine-global* one the API key is written to in plain
/// text, so a home is what it is named after and the only road to it that is
/// not the workspace is built from one. `MUSH_CONFIG` is an explicit road the
/// human chose, relative or not, and it is taken as given. Without it, an
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
}

/// The road a home-config save takes the `api_key` field by.
///
/// `api_key` is the one field whose statement a save may have to *withhold*,
/// so the road is an argument rather than a convention. The key can come from
/// a layer the human deliberately kept out of files (`MUSH_API_KEY`, the
/// README's own road for a key you do not want on disk), and a save triggered
/// by an unrelated command — `/url`, `/model`, `/provider`, whose acks say
/// nothing about a file — must not be the road that copies it in (finding
/// C11). Every other field of the value is stated either way, and the ordinary
/// merge still fills in whatever this value leaves unstated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyWrite {
    /// State the key this value holds: `Some` writes it, `None` says this
    /// endpoint has none. `None` is a statement, not silence — the save that
    /// follows a host change writes it so the old host's key is not merged
    /// forward to the new one (findings C6, D6).
    Stated,
    /// Leave the file's own `api_key` exactly as it is, present or absent,
    /// whatever this value holds. The road for a save whose key is not the
    /// human's to write: it is not a statement about the endpoint, it is
    /// silence about the key.
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
    fn load_at(path: Option<PathBuf>) -> Loaded {
        let Some(path) = path else {
            return Loaded {
                config: UserConfig::default(),
                complaint: Some(
                    "no home config to read: HOME is not set and MUSH_CONFIG names no file \
                     — set HOME or MUSH_CONFIG; using defaults"
                        .to_string(),
                ),
            };
        };
        Self::load_from(&path)
    }

    /// Read the file, or the defaults.
    ///
    /// A missing file is silence: a machine that never wrote one has nothing to
    /// complain about. A file that is *there* and cannot be used — unreadable,
    /// not JSON, or one field of the wrong type — is the defaults *with a
    /// complaint*: a hard failure would take the whole TUI down over one wrong
    /// character, and silence would let the human's key and settings vanish into
    /// the built-ins without a word (finding C3). An unknown *key* is ignored:
    /// forward compatibility costs nothing a human editing this by hand would
    /// miss.
    ///
    /// Two values in a file that *did* parse are reported by name instead, where
    /// they are read: the home config's `provider` and its `reasoning_effort`
    /// ([`crate::config::resolve`]), because a name mush does not know must
    /// never fall through to a default host (findings A17, C2).
    pub fn load_from(path: &Path) -> Loaded {
        let complaint = |why: String| Loaded {
            config: UserConfig::default(),
            complaint: Some(why),
        };
        let bytes = match fs::read(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Loaded {
                    config: UserConfig::default(),
                    complaint: None,
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
        match serde_json::from_slice(&bytes) {
            Ok(config) => Loaded {
                config,
                complaint: None,
            },
            Err(error) => complaint(format!(
                "could not read {} — {error}; using defaults",
                path.display()
            )),
        }
    }

    /// Write the file: this value's fields, over whatever is already there,
    /// with `api_key` taken by the road `key` names.
    ///
    /// A field this value leaves *unstated* — `None`, or an empty string for a
    /// connection field — keeps the value the file already had, and so does a
    /// key this build does not know. The TUI saves only the four fields it owns
    /// (a `/key` writes the connection and nothing else), and a hand-edited
    /// `temperature`, or a setting only a newer mush understands, must survive
    /// that. Fields that are stated overwrite.
    ///
    /// `api_key` is the one field with a road of its own ([`KeyWrite`]).
    /// [`KeyWrite::Stated`] makes it an ordinary stated field: `None` there
    /// means this endpoint has no key, not "leave the file's alone", because
    /// the key lives beside the endpoint it was minted for and the save that
    /// drops it is the save that just moved the endpoint (findings C6, D6) —
    /// merging the old host's key forward would hand it to the new one on the
    /// next start. [`KeyWrite::Keep`] writes the file's own key back, present
    /// or absent, which is what lets a command whose ack says nothing about a
    /// file avoid putting a key in one (finding C11).
    ///
    /// A file that is there and is not an object mush can merge into — not
    /// JSON, JSON that is not an object, or unreadable — is moved beside itself
    /// as `<name>.bak` (then `.bak.2`, …) before anything is written: this save
    /// states four fields and rewrites the header, so without that it would be
    /// the thing that destroys the file the human's key is in (finding C3). A
    /// backup that fails refuses the save rather than writing over the only
    /// copy.
    pub fn save_to(&self, path: &Path, key: KeyWrite) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut merged = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        // What the merge can read. An object is the shape a file mush wrote
        // (and the human edited); anything else is not content the merge can
        // preserve, so it is the backup's business instead.
        let existing = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .filter(Value::is_object);
        // `missing` is the one case with a right answer of its own: there is
        // nothing to keep, so nothing is kept. Everything else that does not
        // merge gets the backup first.
        let missing = matches!(fs::metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound);
        if existing.is_none() && !missing {
            keep_unparsable(path)?;
        }
        if let (Some(fields), Some(Value::Object(before))) = (merged.as_object_mut(), &existing) {
            for (field, value) in before {
                // `api_key` has its road of its own, taken just below: the
                // merge loop must not carry a `None` over the file's key, and
                // must not let a `Keep` save's own key stand either.
                if field == "api_key" {
                    continue;
                }
                let unstated = fields.get(field).map_or(true, |current| {
                    current.is_null() || current.as_str() == Some("")
                });
                if unstated && field != COMMENT_KEY {
                    fields.insert(field.clone(), value.clone());
                }
            }
        }
        // The key, by the road the caller named: this value's own (`Stated`,
        // `None` included) or the file's, present or absent (`Keep`).
        if let Some(fields) = merged.as_object_mut() {
            let written = match key {
                KeyWrite::Stated => self.api_key.clone(),
                KeyWrite::Keep => existing
                    .as_ref()
                    .and_then(|file| file.get("api_key"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            };
            fields.insert(
                "api_key".to_string(),
                written.map(Value::String).unwrap_or(Value::Null),
            );
        }
        // The header is mush's and is always rewritten: the file explains
        // itself, whatever the human does to it.
        if let Some(fields) = merged.as_object_mut() {
            fields.insert(COMMENT_KEY.to_string(), json!(comment()));
        }
        let json = serde_json::to_vec_pretty(&merged).unwrap_or_else(|_| b"{}".to_vec());
        // A new file is the human's alone ([`Fresh::Private`]): it carries the
        // API key, and a `022` umask must not make it group-readable.
        atomic_write(path, &json, Fresh::Private)
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
        let path = dir.path().join("config.json");
        dir.hold(path)
    }

    /// The header the written file carries, joined into one string, so a test
    /// can assert on what a human opening the file would read.
    fn header_of(path: &Path) -> String {
        let written = fs::read_to_string(path).unwrap();
        let value: Value = serde_json::from_str(&written).unwrap();
        value[COMMENT_KEY]
            .as_array()
            .unwrap_or_else(|| panic!("no `{COMMENT_KEY}` header in {written}"))
            .iter()
            .map(|line| line.as_str().unwrap_or_default())
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
            Some(PathBuf::from("/home/someone/.config/mush/config.json")),
            "the platform config directory is the one road"
        );
        assert_eq!(
            config_path_from(Some(OsString::from("beside-me.json")), None),
            Some(PathBuf::from("beside-me.json")),
            "MUSH_CONFIG is the human's own road, taken as given"
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
        assert_eq!(loaded.api_key.as_deref(), Some("sk-test-1234"));
        assert_eq!(loaded.provider, "deepseek");
        assert_eq!(loaded.model, "deepseek-flash");

        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains(COMMENT_KEY), "{written}");
        let header = header_of(&path);
        assert!(
            header.contains("CLI flags > MUSH_* environment"),
            "the precedence is in the file, not only in the docs: {header}"
        );
        // Every field a hand-edit may set, named in the header itself.
        for field in [
            "api_key",
            "provider",
            "base_url",
            "model",
            "context",
            "temperature",
            "max_completion_tokens",
            "reasoning_effort",
            "thinking",
        ] {
            assert!(header.contains(field), "`{field}` is documented: {header}");
        }
        // The provider line is spelled from the table, so it offers every name
        // a `/provider` could select — never a stale one.
        for spec in crate::provider::PROVIDERS {
            assert!(
                header.contains(spec.name),
                "`{}` is offered in the file's own header: {header}",
                spec.name
            );
            // The window a provider defaults to is in the file too: a human
            // hand-editing `context` reads the number a request would use
            // where nothing else stated one.
            assert!(
                header.contains(&spec.fallback_context_tokens.to_string()),
                "`{}`'s default window is in the header: {header}",
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
        let loaded = UserConfig::load_from(Path::new("/nonexistent/mush/config.json"));
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
            r#"{
  "api_key": "sk-old",
  "provider": "deepseek",
  "base_url": "https://api.deepseek.com",
  "model": "deepseek-flash"
}"#,
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
            r#"{
  "_comment": ["written by a newer mush"],
  "api_key": "sk-all",
  "provider": "custom",
  "base_url": "http://host:1",
  "model": "m",
  "context": 64000,
  "temperature": 0.3,
  "max_completion_tokens": true,
  "reasoning_effort": "low",
  "thinking": false,
  "future_knob": {"a": 1}
}"#,
        )
        .unwrap();

        let user = UserConfig::load_from(&path).config;
        assert_eq!(user.api_key.as_deref(), Some("sk-all"));
        assert_eq!(user.provider, "custom");
        assert_eq!(user.base_url, "http://host:1");
        assert_eq!(user.model, "m");
        assert_eq!(user.context, Some(64_000));
        assert_eq!(user.temperature, Some(0.3));
        assert_eq!(user.max_completion_tokens, Some(true));
        assert_eq!(user.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(user.thinking, Some(false));

        // The TUI's save states the connection and nothing else; the knobs and
        // the unknown key are left exactly as the human wrote them.
        let saved = UserConfig {
            api_key: Some("sk-new".into()),
            provider: "custom".into(),
            base_url: "http://host:1".into(),
            model: "m".into(),
            ..UserConfig::default()
        };
        saved.save_to(&path, KeyWrite::Stated).unwrap();
        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(reloaded.api_key.as_deref(), Some("sk-new"));
        assert_eq!(reloaded.context, Some(64_000));
        assert_eq!(reloaded.temperature, Some(0.3));
        assert_eq!(reloaded.max_completion_tokens, Some(true));
        assert_eq!(reloaded.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(reloaded.thinking, Some(false));
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("future_knob"), "{written}");
        assert!(header_of(&path).contains("CLI flags > MUSH_* environment"));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// The road a save takes the key by is the caller's: [`KeyWrite::Keep`]
    /// writes the file's own key back — present or absent — while every other
    /// field the value states still lands. It is the road a save an unrelated
    /// command triggers takes, so a key that only the environment holds never
    /// becomes a file's key (finding C11); [`KeyWrite::Stated`] is the road
    /// `/key` and a host change take.
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
        // the file's key stays exactly where it was, the model lands.
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
            reloaded.api_key.as_deref(),
            Some("sk-file-0123456789"),
            "the file's own key is not replaced by the value's"
        );
        assert_eq!(reloaded.model, "another", "the other fields still land");

        // A file that held no key keeps holding none: `Keep` is silence about
        // the key, not the value's key written under another name.
        let bare = temp_path("keep-key-bare");
        fs::create_dir_all(bare.parent().unwrap()).unwrap();
        picked.save_to(&bare, KeyWrite::Keep).unwrap();
        let reloaded = UserConfig::load_from(&bare).config;
        assert_eq!(reloaded.api_key, None, "no file key, no key written");
        assert_eq!(reloaded.model, "another");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let _ = fs::remove_dir_all(bare.parent().unwrap());
    }

    /// The key lives beside the endpoint it was minted for: a save that states
    /// `None` erases the key the file held rather than merging it forward,
    /// because the save that does this is the one that just moved the endpoint
    /// and dropped the key (findings C6, D6). Every other unstated field keeps
    /// its old value.
    #[test]
    fn a_save_states_the_key_even_when_it_has_none() {
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
        assert_eq!(
            UserConfig::load_from(&path).config.api_key.as_deref(),
            Some("sk-old-0123456789")
        );

        // The save after a host change: the endpoint moves, and the key is not
        // there to move with it — but a field this save does not own (the
        // window) is untouched.
        let moved = UserConfig {
            provider: "deepseek".into(),
            base_url: "https://api.deepseek.com".into(),
            ..UserConfig::default()
        };
        moved.save_to(&path, KeyWrite::Stated).unwrap();
        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(reloaded.api_key, None, "the old host's key is not re-homed");
        assert_eq!(reloaded.base_url, "https://api.deepseek.com");
        assert_eq!(reloaded.context, Some(32_000), "the merge still merges");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
