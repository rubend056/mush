//! Runtime configuration.
//!
//! Sourced from the environment at launch (with CLI overrides) and adjustable
//! at runtime from the TUI (`/provider`, `/url`, `/model`, `/key`). The chosen
//! endpoint, provider, and model persist in the session; the API key never
//! does — it lives in the home config or `MUSH_API_KEY`.
//!
//! [`resolve`] is the single place where the startup precedence is written
//! down; `main.rs` only parses argv and hands the values over.

use crate::session::Session;
use crate::userconfig::UserConfig;

/// Context window assumed when `MUSH_CONTEXT` is unset.
pub const DEFAULT_CONTEXT_TOKENS: usize = 8192;

/// Where mush talks to a model. Deliberately OpenAI-compatible so it works with
/// llama.cpp, Ollama, vLLM, LM Studio, DeepSeek, and hosted APIs alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    /// DeepSeek's hosted API at `https://api.deepseek.com`.
    DeepSeek,
    /// Any OpenAI-compatible endpoint (local servers, proxies, other hosts).
    Custom,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::DeepSeek, Provider::Custom];

    pub fn name(&self) -> &'static str {
        match self {
            Provider::DeepSeek => "deepseek",
            Provider::Custom => "custom",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "deepseek" => Some(Provider::DeepSeek),
            "custom" | "openai" | "openai-compatible" => Some(Provider::Custom),
            _ => None,
        }
    }

    /// Endpoint used when no URL is given.
    pub fn default_base_url(&self) -> &'static str {
        match self {
            Provider::DeepSeek => "https://api.deepseek.com",
            Provider::Custom => "http://rubendpc:8078",
        }
    }

    /// Whether the provider requires an API key for normal use.
    pub fn needs_api_key(&self) -> bool {
        matches!(self, Provider::DeepSeek)
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub provider: Provider,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    /// The endpoint's context window in tokens. The history trimmer keeps
    /// every request under it, reserving room for the tool schemas and the
    /// reply. Small local models are typically 8192.
    pub context_tokens: usize,
}

/// A set of user-supplied values: the command line, or the `MUSH_*`
/// environment. `None` means "not given", which is what lets a lower-priority
/// layer win.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    pub url: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub api_key: Option<String>,
}

impl Overrides {
    /// The environment layer: `MUSH_URL`, `MUSH_MODEL`, `MUSH_PROVIDER`,
    /// `MUSH_API_KEY`. Empty variables count as unset.
    pub fn from_env() -> Self {
        Self {
            url: env_nonempty("MUSH_URL"),
            model: env_nonempty("MUSH_MODEL"),
            provider: env_nonempty("MUSH_PROVIDER"),
            api_key: env_nonempty("MUSH_API_KEY"),
        }
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Endpoints are stored without a trailing slash so `chat_url` and
/// `models_url` always join cleanly.
fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Tokens every request reserves for the tool schemas. The root's nine
/// schemas measure ~3.1 KB (~1.0 K tokens at the 3 bytes/token heuristic),
/// so the reserve rounds up; `prompt` tests that they keep fitting.
pub const SCHEMA_TOKENS: usize = 1_100;

impl Config {
    /// Built-in defaults with the `MUSH_*` environment applied.
    pub fn from_env() -> Self {
        let env = Overrides::from_env();
        let provider = env
            .provider
            .as_deref()
            .and_then(Provider::parse)
            .unwrap_or(Provider::Custom);
        let base_url = env
            .url
            .as_deref()
            .map(normalize_url)
            .unwrap_or_else(|| provider.default_base_url().to_string());
        let context_tokens = std::env::var("MUSH_CONTEXT")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_CONTEXT_TOKENS);
        Self {
            provider,
            base_url,
            model: env.model.unwrap_or_default(),
            api_key: env.api_key,
            context_tokens,
        }
    }

    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            provider: Provider::Custom,
            base_url: normalize_url(&base_url.into()),
            model: model.into(),
            api_key,
            context_tokens: DEFAULT_CONTEXT_TOKENS,
        }
    }

    /// How much conversation history (in bytes) fits alongside the tool
    /// schemas and the reply inside `context_tokens`. Rough heuristic:
    /// ~3 bytes per token, `SCHEMA_TOKENS` of schemas, 2048 tokens of reply.
    pub fn history_budget(&self) -> usize {
        const REPLY_TOKENS: usize = 2048;
        const MARGIN_TOKENS: usize = 200;
        let tokens = self
            .context_tokens
            .saturating_sub(SCHEMA_TOKENS + REPLY_TOKENS + MARGIN_TOKENS);
        tokens * 3
    }

    pub fn chat_url(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url)
    }

    pub fn models_url(&self) -> String {
        format!("{}/v1/models", self.base_url)
    }

    /// The provider's known models, used when the endpoint cannot list them
    /// (offline, missing key, or a server without `/v1/models`).
    pub fn default_models(&self) -> Vec<String> {
        match self.provider {
            Provider::DeepSeek => vec!["deepseek-flash".to_string(), "deepseek-v4-pro".to_string()],
            Provider::Custom => Vec::new(),
        }
    }

    /// Provider-specific request knobs, applied by the agent loop.
    pub fn thinking_enabled(&self) -> bool {
        self.provider == Provider::DeepSeek
    }

    pub fn reasoning_effort(&self) -> Option<&'static str> {
        match self.provider {
            Provider::DeepSeek => Some("high"),
            Provider::Custom => None,
        }
    }

    /// Point at a different endpoint, normalizing the URL the same way every
    /// other entry point does.
    pub fn set_base_url(&mut self, url: &str) {
        self.base_url = normalize_url(url);
    }

    /// A short label for the status bar, e.g. `deepseek-flash @ deepseek.com`.
    pub fn label(&self) -> String {
        let model = if self.model.is_empty() {
            "no model".to_string()
        } else {
            self.model
                .rsplit('/')
                .next()
                .unwrap_or(&self.model)
                .to_string()
        };
        let endpoint = match self.provider {
            Provider::DeepSeek => "deepseek.com".to_string(),
            Provider::Custom => self.base_url.clone(),
        };
        format!("{model} @ {endpoint}")
    }
}

/// Startup resolution over four layers, highest priority first:
/// **CLI flags > `MUSH_*` environment > saved session > home config >
/// built-in defaults**. The API key comes from the environment or the home
/// config, never from the session — that file is workspace-local.
///
/// Returns an error only for an unknown provider name on the command line.
pub fn resolve(
    cli: &Overrides,
    home: &UserConfig,
    session: Option<&Session>,
) -> Result<Config, String> {
    resolve_with(
        Config::from_env(),
        cli,
        &Overrides::from_env(),
        home,
        session,
    )
}

/// The pure half of [`resolve`]: apply the layers to an already-built base
/// config. Kept separate so the precedence can be tested without a process
/// environment.
pub fn resolve_with(
    mut config: Config,
    cli: &Overrides,
    env: &Overrides,
    home: &UserConfig,
    session: Option<&Session>,
) -> Result<Config, String> {
    // 1. Command-line flags beat everything else.
    if let Some(url) = cli.url.as_deref() {
        config.set_base_url(url);
    }
    if let Some(provider) = cli.provider.as_deref() {
        config.provider = Provider::parse(provider)
            .ok_or_else(|| format!("unknown provider `{provider}` (try deepseek or custom)"))?;
    }
    if let Some(model) = cli.model.as_deref() {
        config.model = model.to_string();
    }

    // A URL, provider, or model the user stated explicitly, here or in the
    // environment, is never overridden by a stored one.
    let url_given = cli.url.is_some() || env.url.is_some();
    let provider_given = cli.provider.is_some() || env.provider.is_some();
    let model_given = cli.model.is_some() || env.model.is_some();

    // 2. Home config: machine-global defaults, and where the API key lives.
    if config.api_key.is_none() {
        config.api_key = home.api_key.clone();
    }
    if !provider_given {
        if let Some(provider) = Provider::parse(&home.provider) {
            config.provider = provider;
            // Switching to a hosted provider also switches its endpoint,
            // unless the home config names one.
            if !url_given && home.base_url.is_empty() {
                config.base_url = provider.default_base_url().to_string();
            }
        }
    }
    if !url_given && !home.base_url.is_empty() {
        config.set_base_url(&home.base_url);
    }
    if !model_given && config.model.is_empty() && !home.model.is_empty() {
        config.model = home.model.clone();
    }

    // 3. The workspace's saved session: the last runtime choice beats the
    //    machine-global defaults.
    if let Some(session) = session {
        if !url_given && !session.base_url.is_empty() {
            config.set_base_url(&session.base_url);
        }
        if !provider_given {
            if let Some(provider) = Provider::parse(&session.provider) {
                config.provider = provider;
            }
        }
        if !model_given && !session.model.is_empty() {
            config.model = session.model.clone();
        }
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(provider: &str, base_url: &str, model: &str) -> UserConfig {
        UserConfig {
            api_key: Some("sk-home".into()),
            provider: provider.into(),
            base_url: base_url.into(),
            model: model.into(),
        }
    }

    fn stored(provider: &str, base_url: &str, model: &str) -> Session {
        Session {
            root: String::new(),
            model: model.into(),
            provider: provider.into(),
            base_url: base_url.into(),
            updated: 0,
            messages: Vec::new(),
        }
    }

    #[test]
    fn cli_flags_beat_every_stored_layer() {
        let cli = Overrides {
            url: Some("http://cli:1/".into()),
            model: Some("cli-model".into()),
            provider: Some("deepseek".into()),
            api_key: None,
        };
        let env = Overrides {
            url: Some("http://env:2".into()),
            model: Some("env-model".into()),
            provider: Some("custom".into()),
            api_key: Some("sk-env".into()),
        };
        let session = stored("custom", "http://session:3", "session-model");
        let config = resolve_with(
            Config::new("http://base:0", "base-model", Some("sk-base".into())),
            &cli,
            &env,
            &home("custom", "http://home:4", "home-model"),
            Some(&session),
        )
        .unwrap();
        assert_eq!(config.base_url, "http://cli:1");
        assert_eq!(config.model, "cli-model");
        assert_eq!(config.provider, Provider::DeepSeek);
        // The key is never taken from the CLI; the environment's is already
        // in the base config, so the home key does not apply.
        assert_eq!(config.api_key.as_deref(), Some("sk-base"));
    }

    #[test]
    fn the_session_beats_the_home_config() {
        let config = resolve_with(
            Config::new("http://base:0", "", None),
            &Overrides::default(),
            &Overrides::default(),
            &home("custom", "http://home:4", "home-model"),
            Some(&stored("deepseek", "http://session:3", "session-model")),
        )
        .unwrap();
        assert_eq!(config.base_url, "http://session:3");
        assert_eq!(config.provider, Provider::DeepSeek);
        assert_eq!(config.model, "session-model");
        assert_eq!(config.api_key.as_deref(), Some("sk-home"));
    }

    #[test]
    fn home_provider_selects_its_endpoint_only_when_no_url_is_set() {
        // Nothing anywhere names a URL: the home provider's endpoint wins.
        let config = resolve_with(
            Config::new("http://base:0", "m", None),
            &Overrides::default(),
            &Overrides::default(),
            &home("deepseek", "", ""),
            None,
        )
        .unwrap();
        assert_eq!(config.base_url, "https://api.deepseek.com");

        // The base (environment) names one: it is respected.
        let config = resolve_with(
            Config::new("http://env:2", "m", None),
            &Overrides::default(),
            &Overrides {
                url: Some("http://env:2".into()),
                ..Overrides::default()
            },
            &home("deepseek", "", ""),
            None,
        )
        .unwrap();
        assert_eq!(config.base_url, "http://env:2");
    }

    #[test]
    fn an_unknown_provider_on_the_command_line_is_an_error() {
        let error = resolve_with(
            Config::new("http://base:0", "m", None),
            &Overrides {
                provider: Some("claude".into()),
                ..Overrides::default()
            },
            &Overrides::default(),
            &UserConfig::default(),
            None,
        )
        .unwrap_err();
        assert!(error.contains("claude"), "{error}");

        // An unparsable *stored* provider is simply ignored.
        let config = resolve_with(
            Config::new("http://base:0", "m", None),
            &Overrides::default(),
            &Overrides::default(),
            &home("claude", "", ""),
            Some(&stored("claude", "", "")),
        )
        .unwrap();
        assert_eq!(config.provider, Provider::Custom);
        assert_eq!(config.base_url, "http://base:0");
    }

    #[test]
    fn urls_are_normalized() {
        let config = Config::new("http://host:1///", "m", None);
        assert_eq!(config.base_url, "http://host:1");
        let mut config = config;
        config.set_base_url("  https://api.deepseek.com/  ");
        assert_eq!(config.base_url, "https://api.deepseek.com");
        assert_eq!(
            config.chat_url(),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn provider_parses_names_and_aliases() {
        assert_eq!(Provider::parse("deepseek"), Some(Provider::DeepSeek));
        assert_eq!(Provider::parse("DEEPSEEK "), Some(Provider::DeepSeek));
        assert_eq!(Provider::parse("custom"), Some(Provider::Custom));
        assert_eq!(Provider::parse("openai-compatible"), Some(Provider::Custom));
        assert_eq!(Provider::parse("claude"), None);
    }

    #[test]
    fn deepseek_has_preset_defaults() {
        let cfg = Config {
            provider: Provider::DeepSeek,
            base_url: Provider::DeepSeek.default_base_url().to_string(),
            model: String::new(),
            api_key: None,
            context_tokens: 8192,
        };
        assert_eq!(
            cfg.chat_url(),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            cfg.default_models(),
            vec!["deepseek-flash".to_string(), "deepseek-v4-pro".to_string()]
        );
        assert!(cfg.thinking_enabled());
        assert_eq!(cfg.reasoning_effort(), Some("high"));
    }

    #[test]
    fn custom_provider_stays_vanilla() {
        let cfg = Config::new("http://localhost:11434", "qwen2.5-coder", None);
        assert_eq!(cfg.provider, Provider::Custom);
        assert!(!cfg.thinking_enabled());
        assert_eq!(cfg.reasoning_effort(), None);
        assert!(cfg.default_models().is_empty());
        assert_eq!(cfg.label(), "qwen2.5-coder @ http://localhost:11434");
    }

    #[test]
    fn label_falls_back_when_no_model() {
        let cfg = Config::new("http://x:1", "", None);
        assert_eq!(cfg.label(), "no model @ http://x:1");
    }

    #[test]
    fn history_budget_fits_the_context_window() {
        // 8192 tokens: schema + reply reserve leaves ~14.5 KB of history.
        let small = Config::new("http://x:1", "m", None);
        assert_eq!(small.context_tokens, DEFAULT_CONTEXT_TOKENS);
        let budget = small.history_budget();
        assert!(
            (14_000..=15_000).contains(&budget),
            "unexpected budget {budget}"
        );

        // A big window leaves a much larger budget.
        let big = Config {
            context_tokens: 128_000,
            ..small.clone()
        };
        assert!(big.history_budget() > 300_000);

        // A tiny window never undershoots below the reserve.
        let tiny = Config {
            context_tokens: 1024,
            ..small
        };
        assert_eq!(tiny.history_budget(), 0);
    }
}
