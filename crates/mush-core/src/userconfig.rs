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
//! on Unix, Application Support on macOS, `%APPDATA%` on Windows).

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
        "api_key: the provider's secret; also read from MUSH_API_KEY. Never written into a workspace.".to_string(),
        format!(
            "provider: {}; `{}` defaults to the local endpoint {}.",
            names_hint(),
            DEFAULT_PROVIDER.name(),
            Provider::Custom.default_base_url()
        ),
        "base_url: an OpenAI-compatible endpoint, without a trailing slash.".to_string(),
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

/// Where the user config lives. `MUSH_CONFIG` overrides the path for tests and
/// unusual setups; otherwise the platform's config directory is used
/// (`$XDG_CONFIG_HOME/mush/config.json` on Unix, the Application Support
/// directory on macOS, `%APPDATA%` on Windows).
pub fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("MUSH_CONFIG") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    if let Some(dir) = dirs::config_dir() {
        return dir.join("mush/config.json");
    }
    PathBuf::from(".mush-user-config.json")
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

impl UserConfig {
    pub fn load() -> Loaded {
        Self::load_from(&config_path())
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

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&config_path())
    }

    /// Write the file: this value's fields, over whatever is already there.
    ///
    /// A field this value leaves *unstated* — `None`, or an empty string for a
    /// connection field — keeps the value the file already had, and so does a
    /// key this build does not know. The TUI saves only the four fields it owns
    /// (a `/key` writes the connection and nothing else), and a hand-edited
    /// `temperature`, or a setting only a newer mush understands, must survive
    /// that. Fields that are stated overwrite.
    ///
    /// `api_key` is the one field that is *always* stated: `None` there means
    /// this endpoint has no key, not "leave the file's alone". The key lives
    /// beside the endpoint it was minted for, and the save that drops it is the
    /// save that just moved the endpoint (findings C6, D6) — merging the old
    /// host's key forward would hand it to the new one on the next start.
    ///
    /// A file that is there and is not an object mush can merge into — not
    /// JSON, JSON that is not an object, or unreadable — is moved beside itself
    /// as `<name>.bak` (then `.bak.2`, …) before anything is written: this save
    /// states four fields and rewrites the header, so without that it would be
    /// the thing that destroys the file the human's key is in (finding C3). A
    /// backup that fails refuses the save rather than writing over the only
    /// copy.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
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
            for (key, value) in before {
                let unstated = fields.get(key).map_or(true, |current| {
                    current.is_null() || current.as_str() == Some("")
                });
                // `api_key` is the exception, and the reason is above: the key
                // is stated, and `None` is a statement.
                if unstated && key != COMMENT_KEY && key != "api_key" {
                    fields.insert(key.clone(), value.clone());
                }
            }
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
/// The name is the session store's ([`crate::session::keep_unreadable`]), and
/// so is the rule that an existing backup is never overwritten: the next free
/// name (`.bak`, `.bak.2`, …) is taken instead, because a copy already beside
/// the file is one the human already needed and this must not be the second
/// accident. A failure is returned, and the caller refuses the write: a save
/// that cannot keep what it is about to replace must not replace it.
fn keep_unparsable(path: &Path) -> std::io::Result<PathBuf> {
    /// How many names a hand-broken file may burn before the problem is not
    /// the name.
    const TRIES: u32 = 100;
    let base = PathBuf::from(format!("{}.bak", path.display()));
    for step in 1..=TRIES {
        let to = if step == 1 {
            base.clone()
        } else {
            PathBuf::from(format!("{}.{step}", base.display()))
        };
        if to.exists() {
            continue;
        }
        fs::rename(path, &to)?;
        return Ok(to);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("every backup name beside {} is taken", path.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mush-userconfig-{name}-{}", std::process::id()));
        // A leftover from an interrupted run must not merge into this one.
        let _ = fs::remove_dir_all(&dir);
        dir.join("config.json")
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
        user.save_to(&path).unwrap();
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
        saved.save_to(&path).unwrap();
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
        saved.save_to(&path).unwrap();
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
        with_key.save_to(&path).unwrap();
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
        moved.save_to(&path).unwrap();
        let reloaded = UserConfig::load_from(&path).config;
        assert_eq!(reloaded.api_key, None, "the old host's key is not re-homed");
        assert_eq!(reloaded.base_url, "https://api.deepseek.com");
        assert_eq!(reloaded.context, Some(32_000), "the merge still merges");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
