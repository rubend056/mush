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
use crate::{CMD_CAP, LIST_LIMIT, READ_CAP};

/// Context window assumed when nothing better is known: `MUSH_CONTEXT`, an
/// endpoint's own metadata, or the provider's per-model table all beat it.
pub const DEFAULT_CONTEXT_TOKENS: usize = 8192;

/// The largest window that can be stored. A window is untrusted input — a
/// `MUSH_CONTEXT`, or an endpoint's advertised metadata — and one past this is
/// a lie that would only make the derived budget meaningless.
pub const MAX_CONTEXT_TOKENS: usize = 10_000_000;

/// The smallest window that can be stored. Below this the request reserve
/// (schemas, reply, margin) would swallow the whole window, and every request
/// would exceed it however much history was trimmed.
const MIN_CONTEXT_TOKENS: usize = 1_024;

/// Keep a window inside the range mush can work with, whatever its source.
fn clamp_context(tokens: usize) -> usize {
    tokens.clamp(MIN_CONTEXT_TOKENS, MAX_CONTEXT_TOKENS)
}

/// A model's known context window, from the provider's documentation. Used when
/// the endpoint does not advertise one (`api.deepseek.com` answers with ids
/// only). Keep these honest: the value is shown wherever the model is chosen.
const KNOWN_CONTEXT: &[(&str, usize)] =
    &[("deepseek-flash", 500_000), ("deepseek-v4-pro", 500_000)];

/// The window a model id is documented to have, if we know it.
pub fn known_context(model: &str) -> Option<usize> {
    let model = model.rsplit('/').next().unwrap_or(model);
    KNOWN_CONTEXT
        .iter()
        .find(|(known, _)| *known == model)
        .map(|(_, tokens)| *tokens)
}

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
            // Deliberately no `openai`/`openai-compatible` aliases: they would
            // land on `Custom`, whose default endpoint is a LAN host, and the
            // request (and the API key) would go there.
            "custom" => Some(Provider::Custom),
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
    /// reply.
    pub context_tokens: usize,
    /// True when the human stated the window (flag, `MUSH_CONTEXT`, `/context`,
    /// or a stored explicit choice). Only then does it beat what the endpoint
    /// advertises: discovery is for guessing, not for overruling.
    pub context_explicit: bool,
    /// Sampling temperature sent with every request. Coding wants the model's
    /// own best judgement, not mush's idea of a cautious one, so the default is
    /// 1.0 — the value every OpenAI-compatible endpoint documents as "use the
    /// model's default" — and a human who wants a cooler model sets it.
    pub temperature: f32,
    /// Send the reply cap as `max_completion_tokens` instead of `max_tokens`.
    /// OpenAI's reasoning models reject the old name, everything else only
    /// documents it, so this is opt-in rather than guessed.
    pub max_completion_tokens: bool,
}

/// The temperature every request carries unless the human says otherwise.
pub const DEFAULT_TEMPERATURE: f32 = 1.0;

/// A set of user-supplied values: the command line, or the `MUSH_*`
/// environment. `None` means "not given", which is what lets a lower-priority
/// layer win.
///
/// No `Eq`: a temperature is a float, and `1.0 == 1.0` is not the question any
/// caller asks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overrides {
    pub url: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub context: Option<usize>,
    /// Sampling temperature (`--temperature`). A float stated by a human, so it
    /// is stored as one rather than as the text they typed; no endpoint ever
    /// sees it outside the range `Config::temperature` clamps to.
    pub temperature: Option<f32>,
    /// Whether the reply cap travels as `max_completion_tokens`
    /// (`--max-completion-tokens`). `None` is "not stated", which is what keeps
    /// a deliberate `false` distinguishable from silence.
    pub max_completion_tokens: Option<bool>,
}

impl Overrides {
    /// The environment layer: `MUSH_URL`, `MUSH_MODEL`, `MUSH_PROVIDER`,
    /// `MUSH_API_KEY`, `MUSH_CONTEXT`. Empty variables count as unset. The
    /// temperature and the reply cap's name have no environment spelling: they
    /// are stated on a command line or in the home config. A malformed
    /// `MUSH_CONTEXT` is ignored here; [`Self::from_env_checked`] is the form
    /// startup uses, so it is reported instead.
    pub fn from_env() -> Self {
        Self {
            url: env_nonempty("MUSH_URL"),
            model: env_nonempty("MUSH_MODEL"),
            provider: env_nonempty("MUSH_PROVIDER"),
            api_key: env_nonempty("MUSH_API_KEY"),
            context: env_nonempty("MUSH_CONTEXT").and_then(|value| parse_context_env(&value).ok()),
            // No environment spelling for these two: see the doc comment.
            temperature: None,
            max_completion_tokens: None,
        }
    }

    /// [`Self::from_env`] with a malformed `MUSH_CONTEXT` reported instead of
    /// silently dropped, so a typo costs a message at startup rather than a
    /// window nobody asked for.
    pub fn from_env_checked() -> Result<Self, String> {
        let mut overrides = Self::from_env();
        if let Some(name) = overrides.provider.as_deref() {
            parse_provider_env(name)?;
        }
        overrides.context = env_nonempty("MUSH_CONTEXT")
            .map(|value| parse_context_env(&value))
            .transpose()?;
        Ok(overrides)
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Parse `MUSH_CONTEXT`. A value that is present but wrong is an error rather
/// than a silent fallback: the variable is how a human states the window, and
/// running with a different one than asked for is the harder bug to notice.
pub fn parse_context_env(value: &str) -> Result<usize, String> {
    match value.trim().parse::<usize>() {
        Ok(tokens) if tokens > 0 => Ok(tokens),
        _ => Err(format!("MUSH_CONTEXT needs a token count, got `{value}`")),
    }
}

/// Validate `MUSH_PROVIDER` the way the command line is validated. Ignoring a
/// typo would leave the provider at its default — `Custom`, whose default
/// endpoint is a LAN host — so a key meant for somewhere else would be sent
/// there (finding A17).
pub fn parse_provider_env(value: &str) -> Result<Provider, String> {
    Provider::parse(value).ok_or_else(|| {
        format!("MUSH_PROVIDER: unknown provider `{value}` (try deepseek or custom)")
    })
}

/// Endpoints are stored without a trailing slash so `chat_url` and
/// `models_url` always join cleanly.
fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Tokens every request reserves for the tool schemas. The root's nine
/// schemas measure ~3.4 KB (~1.1 K tokens at the 3 bytes/token heuristic),
/// so the reserve rounds up; `prompt` tests that they keep fitting.
///
/// The schemas are context paid on *every* request, so this is a real cost and
/// the descriptions are kept to the rules a model must read to obey them (the
/// one-non-isolated-sibling limit, that stopping a child is not finishing it,
/// and that a command already runs in the workspace root). They grew from
/// ~3.1 KB when those rules were made explicit, to ~3.6 KB when the `cd` rule
/// joined them; the `schemas_fit_the_budget_reserve` test is what makes that a
/// decision rather than a silent drift.
pub const SCHEMA_TOKENS: usize = 1_220;

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
        let context = env.context.filter(|n| *n > 0).map(clamp_context);
        Self {
            provider,
            base_url,
            model: env.model.unwrap_or_default(),
            api_key: env.api_key,
            context_tokens: context.unwrap_or(DEFAULT_CONTEXT_TOKENS),
            context_explicit: context.is_some(),
            temperature: DEFAULT_TEMPERATURE,
            max_completion_tokens: false,
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
            context_explicit: false,
            temperature: DEFAULT_TEMPERATURE,
            max_completion_tokens: false,
        }
    }

    /// What every request samples at: the configured temperature, clamped to
    /// the range endpoints accept, so a typo in a config file is a value mush
    /// can still send rather than a request an endpoint rejects.
    pub fn temperature(&self) -> f32 {
        self.temperature.clamp(0.0, 2.0)
    }

    /// Whether the reply cap travels as `max_completion_tokens`. OpenAI's
    /// reasoning models reject `max_tokens`; every other endpoint documents it,
    /// so the choice is the human's (or a provider default), never a guess.
    pub fn uses_max_completion_tokens(&self) -> bool {
        self.max_completion_tokens
    }

    /// Set the window from the human (`/context`, a stored choice). An explicit
    /// window beats anything an endpoint says.
    pub fn set_context(&mut self, tokens: usize) {
        self.context_tokens = clamp_context(tokens);
        self.context_explicit = true;
    }

    /// Caps that derive from the window, so a small model is not handed a tool
    /// result larger than its whole transcript: one read may take a quarter of
    /// the budget, one command an eighth, one listing a sixty-fourth. They are
    /// ceilings and floors at once — the floor is also capped by the budget, so
    /// a small window never gets a tool result it cannot hold.
    pub fn read_cap(&self) -> usize {
        READ_CAP
            .min(self.history_budget() / 4)
            .max(512)
            .min(self.history_budget())
    }

    pub fn cmd_cap(&self) -> usize {
        CMD_CAP
            .min(self.history_budget() / 8)
            .max(512)
            .min(self.history_budget())
    }

    pub fn list_limit(&self) -> usize {
        LIST_LIMIT
            .min(self.history_budget() / 64)
            .max(50)
            .min(self.history_budget())
    }

    /// How much conversation history (in bytes) fits alongside the tool
    /// schemas and the reply inside `context_tokens`. Rough heuristic: ~3
    /// bytes per token, `SCHEMA_TOKENS` of schemas, 2048 tokens of reply. The
    /// reserve is itself capped at half the window: a small window shrinks it
    /// (half the window is always history) instead of leaving no budget at all.
    pub fn history_budget(&self) -> usize {
        const REPLY_TOKENS: usize = 2048;
        const MARGIN_TOKENS: usize = 200;
        let reserve = (SCHEMA_TOKENS + REPLY_TOKENS + MARGIN_TOKENS).min(self.context_tokens / 2);
        self.context_tokens
            .saturating_sub(reserve)
            .saturating_mul(3)
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

    /// The window to assume for the current model when the endpoint advertises
    /// nothing: the documented one, else the provider's general default.
    pub fn fallback_context(&self) -> usize {
        known_context(&self.model).unwrap_or(match self.provider {
            Provider::DeepSeek => 128_000,
            Provider::Custom => DEFAULT_CONTEXT_TOKENS,
        })
    }

    /// Adopt a window learned from the endpoint or the model table, unless the
    /// human stated one explicitly. The value is endpoint metadata, i.e.
    /// untrusted: a window below the floor is raised to it (the server is
    /// saying the window is small, not that it is one token), and one past the
    /// ceiling is lowered.
    pub fn adopt_context(&mut self, tokens: usize) -> bool {
        if self.context_explicit || tokens == 0 {
            return false;
        }
        let tokens = clamp_context(tokens);
        if tokens == self.context_tokens {
            return false;
        }
        self.context_tokens = tokens;
        true
    }

    /// Point at a different model: the window is re-derived from the new model's
    /// documented size unless the human stated one.
    pub fn set_model(&mut self, model: &str) {
        self.model = model.to_string();
        self.rederive_context();
    }

    /// Re-derive the window from the model/provider table. A window the human
    /// stated explicitly is never touched; a window merely learned from the
    /// previous endpoint is replaced.
    pub fn rederive_context(&mut self) {
        if !self.context_explicit {
            self.context_tokens = self.fallback_context();
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
/// Returns an error for an unknown provider name on the command line, and for a
/// `MUSH_CONTEXT` that is not a token count.
pub fn resolve(
    cli: &Overrides,
    home: &UserConfig,
    session: Option<&Session>,
) -> Result<Config, String> {
    let env = Overrides::from_env_checked()?;
    resolve_with(Config::from_env(), cli, &env, home, session)
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
        // Naming a provider on the command line selects its own endpoint — the
        // flag must reach `api.deepseek.com`, not whatever the environment's
        // provider defaulted to — unless a URL was named too.
        if cli.url.is_none() && env.url.is_none() {
            config.base_url = config.provider.default_base_url().to_string();
        }
    }
    if let Some(model) = cli.model.as_deref() {
        config.model = model.to_string();
    }
    if let Some(context) = cli.context.filter(|n| *n > 0) {
        config.set_context(context);
    }
    if let Some(temperature) = cli.temperature.or(env.temperature) {
        config.temperature = temperature;
    }
    if let Some(max_completion_tokens) = cli.max_completion_tokens.or(env.max_completion_tokens) {
        config.max_completion_tokens = max_completion_tokens;
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
        // A window the human chose for this workspace, remembered. It is an
        // explicit statement, so it outranks anything discovered later.
        if !config.context_explicit {
            if let Some(tokens) = session.context.filter(|n| *n > 0) {
                config.set_context(tokens);
            }
        }
    }

    // 4. An endpoint named on the command line or in the environment is a
    //    *custom* endpoint: a stored provider must not leak its DeepSeek-only
    //    knobs (`reasoning_effort`, `thinking`) to a URL it does not own. A
    //    provider named alongside the URL keeps its knobs.
    if (cli.url.is_some() || env.url.is_some()) && !provider_given {
        config.provider = Provider::Custom;
    }

    // 5. Nothing was stated: assume the model's documented window, else the
    //    provider's default. An endpoint that advertises one (llama.cpp's
    //    `meta.n_ctx`, vLLM's `max_model_len`) overrides this at discovery.
    if !config.context_explicit {
        config.context_tokens = config.fallback_context();
    }

    Ok(config)
}

/// The number in a "context length" complaint, when a server names one. Hosted
/// APIs are the only place mush cannot discover the window, and their error is
/// the one source that is always current.
///
/// The number only counts when the text around it is about context: a generic
/// `the maximum is 10` (a 429 body, say) must not be read as a 10-token window.
pub fn parse_context_hint(message: &str) -> Option<usize> {
    let lower = message.to_ascii_lowercase();
    for marker in [
        "maximum context length is ",
        "context length is ",
        "maximum context window of ",
        "context window of ",
        "max_model_len is ",
        // llama.cpp: `context size (2048 tokens)`, and Anthropic/Gemini-style
        // `205404 tokens > 200000 maximum` (the number after `>` is the limit).
        "context size (",
        "tokens > ",
    ] {
        let Some(at) = lower.find(marker) else {
            continue;
        };
        let tail = &message[at + marker.len()..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(tokens) = digits.parse::<usize>() {
            // Only a plausible window qualifies: anything smaller or larger is
            // some other number that happened to follow the same words.
            if (MIN_CONTEXT_TOKENS..=MAX_CONTEXT_TOKENS).contains(&tokens) {
                return Some(tokens);
            }
        }
    }
    None
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
            context: None,
            updated: 0,
            messages: Vec::new(),
            agents: Vec::new(),
        }
    }

    #[test]
    fn cli_flags_beat_every_stored_layer() {
        let cli = Overrides {
            url: Some("http://cli:1/".into()),
            model: Some("cli-model".into()),
            provider: Some("deepseek".into()),
            api_key: None,
            context: None,
            ..Overrides::default()
        };
        let env = Overrides {
            url: Some("http://env:2".into()),
            model: Some("env-model".into()),
            provider: Some("custom".into()),
            api_key: Some("sk-env".into()),
            context: None,
            ..Overrides::default()
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

    /// The temperature and the reply cap's name are stated, not guessed: the
    /// command line is the top layer, the environment the next one down, and a
    /// statement below them is filled in only when nothing above said anything.
    #[test]
    fn the_command_line_states_the_temperature_and_the_cap_name() {
        let config = resolve_with(
            Config::new("http://base:0", "m", None),
            &Overrides {
                temperature: Some(0.2),
                max_completion_tokens: Some(true),
                ..Overrides::default()
            },
            &Overrides::default(),
            &UserConfig::default(),
            None,
        )
        .unwrap();
        assert_eq!(config.temperature(), 0.2);
        assert!(config.uses_max_completion_tokens());

        // The environment is below the flag: it fills what the flag left alone.
        let from_env = resolve_with(
            Config::new("http://base:0", "m", None),
            &Overrides::default(),
            &Overrides {
                temperature: Some(0.5),
                max_completion_tokens: Some(true),
                ..Overrides::default()
            },
            &UserConfig::default(),
            None,
        )
        .unwrap();
        assert_eq!(from_env.temperature(), 0.5);
        assert!(from_env.uses_max_completion_tokens());

        // Nobody stated anything: the documented defaults stay.
        let untouched = resolve_with(
            Config::new("http://base:0", "m", None),
            &Overrides::default(),
            &Overrides::default(),
            &UserConfig::default(),
            None,
        )
        .unwrap();
        assert_eq!(untouched.temperature(), DEFAULT_TEMPERATURE);
        assert!(!untouched.uses_max_completion_tokens());
    }

    #[test]
    fn provider_parses_names_and_aliases() {
        assert_eq!(Provider::parse("deepseek"), Some(Provider::DeepSeek));
        assert_eq!(Provider::parse("DEEPSEEK "), Some(Provider::DeepSeek));
        assert_eq!(Provider::parse("custom"), Some(Provider::Custom));
        // No OpenAI aliases: `Custom` defaults to a LAN host, so mapping them
        // there would send someone else's key to it.
        assert_eq!(Provider::parse("openai"), None);
        assert_eq!(Provider::parse("openai-compatible"), None);
        assert_eq!(Provider::parse("claude"), None);
    }

    /// Every request samples at 1.0 unless the human says otherwise — the value
    /// endpoints document as "the model's default" — and a value a config file
    /// got wrong is clamped rather than sent.
    #[test]
    fn the_temperature_defaults_to_one_and_is_clamped() {
        let cfg = Config::new("http://x:1", "m", None);
        assert_eq!(cfg.temperature(), 1.0);
        assert_eq!(DEFAULT_TEMPERATURE, 1.0);

        let cold = Config {
            temperature: 0.0,
            ..cfg.clone()
        };
        assert_eq!(cold.temperature(), 0.0, "0 is a value, not an absence");

        let wild = Config {
            temperature: 9.5,
            ..cfg.clone()
        };
        assert_eq!(wild.temperature(), 2.0, "clamped, so it can still be sent");
        let negative = Config {
            temperature: -3.0,
            ..cfg
        };
        assert_eq!(negative.temperature(), 0.0);
    }

    /// The reply cap travels under one name or the other, never both: OpenAI's
    /// reasoning models reject `max_tokens`, and everything else only knows it.
    #[test]
    fn the_reply_cap_is_sent_under_exactly_one_name() {
        let cfg = Config::new("http://x:1", "m", None);
        assert!(!cfg.uses_max_completion_tokens(), "the documented default");

        let mut cfg = cfg;
        cfg.max_completion_tokens = true;
        assert!(cfg.uses_max_completion_tokens());
    }

    #[test]
    fn deepseek_has_preset_defaults() {
        let cfg = Config {
            provider: Provider::DeepSeek,
            base_url: Provider::DeepSeek.default_base_url().to_string(),
            model: String::new(),
            api_key: None,
            context_tokens: 8192,
            context_explicit: false,
            temperature: DEFAULT_TEMPERATURE,
            max_completion_tokens: false,
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
        // 8192 tokens: the full reserve (1220 schemas + 2048 reply + 200
        // margin) leaves 14_172 bytes of history. The schema reserve has grown
        // twice, each time with the test and the comment moved together: 1100
        // when the delegation contract became explicit, 1220 when the `cd` rule
        // joined it. See SCHEMA_TOKENS.
        let small = Config::new("http://x:1", "m", None);
        assert_eq!(small.context_tokens, DEFAULT_CONTEXT_TOKENS);
        assert_eq!(small.history_budget(), 14_172);

        // A big window leaves a much larger budget.
        let big = Config {
            context_tokens: 128_000,
            temperature: DEFAULT_TEMPERATURE,
            max_completion_tokens: false,
            ..small.clone()
        };
        assert!(big.history_budget() > 300_000);

        // A tiny window shrinks the reserve to half the window instead of
        // ignoring it: history still gets 1536 bytes, and no cap — which has
        // a floor of its own — is larger than the budget that holds it.
        let tiny = Config {
            context_tokens: 1024,
            ..small
        };
        assert_eq!(tiny.history_budget(), 1_536);
        assert!(tiny.read_cap() <= tiny.history_budget());
        assert!(tiny.cmd_cap() <= tiny.history_budget());
        assert!(tiny.list_limit() <= tiny.history_budget());
    }

    /// The window comes from the model when nobody said otherwise, and the
    /// caps follow it: one tool result must never be larger than the transcript
    /// that has to hold it.
    #[test]
    fn the_window_and_the_caps_scale_together() {
        let mut cfg = Config::new("http://x:1", "deepseek-v4-pro", None);
        assert!(!cfg.context_explicit);
        // `Config::new` does not resolve; the fallback is what the resolver uses.
        assert_eq!(cfg.fallback_context(), 500_000);
        cfg.context_tokens = cfg.fallback_context();
        assert_eq!(cfg.read_cap(), READ_CAP, "a huge window keeps the ceiling");
        assert_eq!(cfg.cmd_cap(), CMD_CAP);
        assert_eq!(cfg.list_limit(), LIST_LIMIT);

        // An 8k local window: a single read may take a quarter of the budget.
        let small = Config::new("http://x:1", "m", None);
        assert_eq!(small.read_cap(), small.history_budget() / 4);
        assert!(
            small.read_cap() < 4_000,
            "a read must fit in an 8k transcript: {}",
            small.read_cap()
        );
        assert!(small.cmd_cap() < small.read_cap());
        assert!(small.list_limit() < small.cmd_cap());

        // Under a window smaller than the caps' own floors, the budget wins:
        // a 1k window is never handed a 512-byte read it cannot hold.
        let tiny = Config {
            context_tokens: 1_024,
            ..Config::new("http://x:1", "m", None)
        };
        assert_eq!(tiny.history_budget(), 1_536);
        assert_eq!(tiny.read_cap(), 512);
        assert!(tiny.read_cap() <= tiny.history_budget());
        assert!(tiny.cmd_cap() <= tiny.history_budget());
        assert!(tiny.list_limit() <= tiny.history_budget());
        assert!(tiny.list_limit() >= 50, "the floor still applies within it");

        // An explicit window is never overruled by discovery.
        let mut cfg = Config::new("http://x:1", "m", None);
        cfg.set_context(64_000);
        assert!(!cfg.adopt_context(8_192), "the human's number stays");
        assert_eq!(cfg.context_tokens, 64_000);
        let mut cfg = Config::new("http://x:1", "m", None);
        assert!(cfg.adopt_context(32_768), "discovery fills in a guess");
        assert_eq!(cfg.context_tokens, 32_768);
    }

    #[test]
    fn a_context_hint_is_read_from_the_server_complaint() {
        assert_eq!(
            parse_context_hint("This model's maximum context length is 131072 tokens"),
            Some(131_072)
        );
        assert_eq!(
            parse_context_hint("The input exceeds the context length is 32768 tokens"),
            Some(32_768)
        );
        assert_eq!(
            parse_context_hint("This endpoint's maximum context window of 8192 tokens is smaller"),
            Some(8_192)
        );
        assert_eq!(
            parse_context_hint("max_model_len is 4096 and the request needs 5000"),
            Some(4_096)
        );
        // llama.cpp names the window in parentheses.
        assert_eq!(
            parse_context_hint(
                "the request exceeds the available context size (2048 tokens), try increasing it"
            ),
            Some(2_048)
        );
        // Anthropic/Gemini-style: the number after `>` is the limit, not the
        // size of the request that just blew past it.
        assert_eq!(
            parse_context_hint("prompt is too long: 205404 tokens > 200000 maximum"),
            Some(200_000)
        );
        // Generic text is not a window: this once collapsed an 8k window to 10
        // tokens, throwing the run's history away.
        assert_eq!(
            parse_context_hint(
                "Rate limit reached for requests: the maximum is 10 requests per minute."
            ),
            None
        );
        assert_eq!(parse_context_hint("429 rate limited"), None);
        assert_eq!(parse_context_hint(""), None);
        // Neither a number below any real window nor one above the ceiling.
        assert_eq!(
            parse_context_hint("maximum context length is 512 tokens"),
            None
        );
        assert_eq!(
            parse_context_hint("maximum context length is 999999999999 tokens"),
            None
        );
    }

    /// A window from anywhere is clamped into the range mush can work with, so
    /// a bogus number can never overflow the budget it derives.
    #[test]
    fn a_stated_window_is_clamped() {
        let mut cfg = Config::new("http://x:1", "m", None);
        cfg.set_context(usize::MAX);
        assert!(cfg.context_explicit);
        assert_eq!(cfg.context_tokens, MAX_CONTEXT_TOKENS);
        assert_eq!(
            cfg.history_budget(),
            (MAX_CONTEXT_TOKENS - SCHEMA_TOKENS - 2048 - 200) * 3
        );

        cfg.set_context(1);
        assert_eq!(cfg.context_tokens, 1_024);
        assert!(cfg.history_budget() > 0);
    }

    /// The window an endpoint advertises is untrusted too: a 1-token window
    /// would otherwise leave the caps above a zero budget.
    #[test]
    fn an_adopted_window_is_clamped_like_a_stated_one() {
        let mut cfg = Config::new("http://x:1", "m", None);
        assert!(cfg.adopt_context(1), "raised to the floor, not ignored");
        assert_eq!(cfg.context_tokens, 1_024);
        assert!(cfg.history_budget() > 0);
        assert!(!cfg.adopt_context(0), "zero is still no answer");
        assert!(!cfg.adopt_context(1), "nothing changed");

        let mut cfg = Config::new("http://x:1", "m", None);
        assert!(cfg.adopt_context(usize::MAX));
        assert_eq!(cfg.context_tokens, MAX_CONTEXT_TOKENS);

        // An explicit window is never touched, whatever the endpoint claims.
        let mut cfg = Config::new("http://x:1", "m", None);
        cfg.set_context(8_192);
        assert!(!cfg.adopt_context(1));
        assert!(!cfg.adopt_context(usize::MAX));
        assert_eq!(cfg.context_tokens, 8_192);
    }

    /// The window follows the model at runtime, unless the human stated one.
    #[test]
    fn a_model_change_rederives_the_window() {
        let mut cfg = Config::new("http://x:1", "", None);
        assert_eq!(cfg.context_tokens, DEFAULT_CONTEXT_TOKENS);
        cfg.set_model("deepseek-v4-pro");
        assert_eq!(cfg.model, "deepseek-v4-pro");
        assert_eq!(cfg.context_tokens, 500_000, "the documented window");

        // A stated window survives every later model/provider change.
        cfg.set_context(8_192);
        cfg.set_model("deepseek-flash");
        assert_eq!(cfg.context_tokens, 8_192);

        // A window merely learned from the previous endpoint does not: a 4k
        // local server's answer must not survive `/provider deepseek`.
        let mut cfg = Config::new("http://x:1", "m", None);
        assert!(cfg.adopt_context(4_096));
        cfg.provider = Provider::DeepSeek;
        cfg.rederive_context();
        assert_eq!(cfg.context_tokens, 128_000, "the new provider's default");
    }

    /// A provider named on the command line reaches its own endpoint; a URL
    /// named there makes the endpoint custom, so a stored provider's
    /// DeepSeek-only knobs never leak to it.
    #[test]
    fn a_named_provider_selects_its_endpoint_and_a_named_url_is_custom() {
        let base = || Config::new("http://rubendpc:8078", "m", None);

        // `--provider deepseek` with no URL anywhere: the flag wins the URL
        // too, instead of keeping the Custom default from the environment.
        let config = resolve_with(
            base(),
            &Overrides {
                provider: Some("deepseek".into()),
                ..Overrides::default()
            },
            &Overrides::default(),
            &UserConfig::default(),
            None,
        )
        .unwrap();
        assert_eq!(config.provider, Provider::DeepSeek);
        assert_eq!(config.base_url, "https://api.deepseek.com");

        // `--url localhost:11434` with a stored DeepSeek provider: the stored
        // provider does not own that URL, so its knobs do not apply.
        let config = resolve_with(
            base(),
            &Overrides {
                url: Some("http://localhost:11434".into()),
                ..Overrides::default()
            },
            &Overrides::default(),
            &home("deepseek", "", ""),
            Some(&stored("deepseek", "", "")),
        )
        .unwrap();
        assert_eq!(config.base_url, "http://localhost:11434");
        assert_eq!(config.provider, Provider::Custom);
        assert!(!config.thinking_enabled());
        assert_eq!(config.reasoning_effort(), None);

        // A provider named alongside the URL keeps its knobs.
        let config = resolve_with(
            base(),
            &Overrides {
                url: Some("http://localhost:11434".into()),
                provider: Some("deepseek".into()),
                ..Overrides::default()
            },
            &Overrides::default(),
            &UserConfig::default(),
            None,
        )
        .unwrap();
        assert_eq!(config.base_url, "http://localhost:11434");
        assert_eq!(config.provider, Provider::DeepSeek);
        assert_eq!(config.reasoning_effort(), Some("high"));
    }

    /// `MUSH_CONTEXT` is stated by a human: a typo is reported by name rather
    /// than silently running a different window.
    #[test]
    fn a_bad_context_environment_value_is_reported() {
        for value in ["abc", "-1", "0", "8k"] {
            assert_eq!(
                parse_context_env(value).unwrap_err(),
                format!("MUSH_CONTEXT needs a token count, got `{value}`")
            );
        }
        assert!(parse_context_env("").is_err(), "empty is not a count");
        assert_eq!(parse_context_env(" 8192 "), Ok(8_192));
    }

    /// A misspelled `MUSH_PROVIDER` must be reported, not quietly left at the
    /// default: `Custom`'s default endpoint is a LAN host, so a key meant for
    /// a hosted API would be sent there (finding A17).
    #[test]
    fn a_bad_provider_environment_value_is_reported() {
        assert_eq!(parse_provider_env("deepseek"), Ok(Provider::DeepSeek));
        assert_eq!(parse_provider_env(" custom "), Ok(Provider::Custom));
        for value in ["openai", "openai-compatible", "claude"] {
            let error = parse_provider_env(value).unwrap_err();
            assert!(error.contains(value), "{error}");
        }
    }
}
