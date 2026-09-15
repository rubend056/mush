//! Runtime configuration.
//!
//! Sourced from the environment at launch (with CLI overrides) and adjustable
//! at runtime from the TUI (`/provider`, `/url`, `/model`, `/key`). The chosen
//! endpoint, provider, and model persist in the session; the API key never
//! does — it lives in memory only (or in `MUSH_API_KEY`).

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

impl Config {
    pub fn from_env() -> Self {
        let provider = Provider::parse(&std::env::var("MUSH_PROVIDER").unwrap_or_default())
            .unwrap_or(Provider::Custom);
        let base_url = std::env::var("MUSH_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| provider.default_base_url().to_string());
        let model = std::env::var("MUSH_MODEL").unwrap_or_default();
        let api_key = std::env::var("MUSH_API_KEY").ok().filter(|s| !s.is_empty());
        let context_tokens = std::env::var("MUSH_CONTEXT")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(8192);
        Self {
            provider,
            base_url,
            model,
            api_key,
            context_tokens,
        }
    }

    pub fn new(base_url: impl Into<String>, model: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            provider: Provider::Custom,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            api_key,
            context_tokens: 8192,
        }
    }

    /// How much conversation history (in bytes) fits alongside the tool
    /// schemas and the reply inside `context_tokens`. Rough heuristic:
    /// ~3 bytes per token, ~800 tokens of schemas, 2048 tokens of reply.
    pub fn history_budget(&self) -> usize {
        const SCHEMA_TOKENS: usize = 800;
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

    /// A short label for the status bar, e.g. `deepseek-flash @ deepseek.com`.
    pub fn label(&self) -> String {
        let model = if self.model.is_empty() {
            "no model".to_string()
        } else {
            self.model.rsplit('/').next().unwrap_or(&self.model).to_string()
        };
        let endpoint = match self.provider {
            Provider::DeepSeek => "deepseek.com".to_string(),
            Provider::Custom => self.base_url.clone(),
        };
        format!("{model} @ {endpoint}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(cfg.chat_url(), "https://api.deepseek.com/v1/chat/completions");
        assert_eq!(cfg.default_models(), vec!["deepseek-flash".to_string(), "deepseek-v4-pro".to_string()]);
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
        // 8192 tokens: schema + reply reserve leaves ~15 KB of history.
        let small = Config::new("http://x:1", "m", None);
        assert_eq!(small.context_tokens, 8192);
        let budget = small.history_budget();
        assert!((15_000..=16_500).contains(&budget), "unexpected budget {budget}");

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