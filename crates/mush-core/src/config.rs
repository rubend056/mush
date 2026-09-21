//! Runtime configuration.
//!
//! Sourced from the environment at launch (with CLI overrides) and adjustable
//! at runtime from the TUI (`/provider`, `/url`, `/model`, `/key`). The chosen
//! endpoint, provider, and model persist in the session; the API key never
//! does — it lives in the home config or `MUSH_API_KEY`.
//!
//! [`resolve`] is the single place where the startup precedence is written
//! down; `main.rs` only parses argv and hands the values over.

use crate::provider;
use crate::session::Session;
use crate::userconfig::UserConfig;
use crate::CMD_CAP;

/// Re-exported so a caller reads the whole provider vocabulary from one crate
/// path. Every vendor fact behind it lives in [`crate::provider`], the only
/// module that names one.
pub use crate::provider::{known_context, ModelSpec, Provider, ProviderSpec, PROVIDERS};

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

/// The most one reply may be asked for, in tokens. The cap has to cover a
/// thinking model's reasoning too: when it is spent before the visible answer,
/// the reply arrives cut off (`finish_reason: length`). At mush's old ceiling
/// of 20_480 a real run ended with `reply cut off at 20480 tokens` for ordinary
/// work, so the ceiling is now the number the human stated. It is a number mush
/// has been told is safe rather than a guess: the endpoint itself answered a
/// larger request with `Invalid max_tokens value, the valid range of
/// max_tokens is [1, 393216]`, and 120_000 sits well inside that. Asking past a
/// vendor's limit is a 400 — worse than a short reply — so the ceiling stays
/// below what an endpoint documents it accepts, and `Config::reply_cap` is the
/// only reader.
pub const MAX_REPLY_TOKENS: u32 = 120_000;

/// The ceiling has to stay under what an endpoint accepts: a request past a
/// vendor's documented `max_tokens` range is a 400, which is worse than the
/// short reply this number exists to stop. The number below is the one the
/// endpoint itself named in that complaint; it is checked here, at compile
/// time, so a later edit cannot raise the ceiling past it by accident.
const _: () = assert!(MAX_REPLY_TOKENS < 393_216);

/// The share of the window one reply may use, as a divisor: `window / this`.
/// `Config::reply_cap` asks for this share, and `Config::request_reserve`
/// reserves exactly what `reply_cap` may ask for, so the cap a request carries
/// and the budget that has to hold it cannot disagree about what one reply
/// costs.
///
/// An eighth rather than a quarter, re-tuned on the human's numbers: the
/// reply cap is a ceiling a run rarely reaches, while the history the reserve
/// was holding back is what a long conversation actually needs. The share is
/// one number, read by the cap and by the reserve, so the two cannot disagree
/// about what a reply costs (see `docs/findings.md` §8.30).
const REPLY_SHARE_DIVISOR: usize = 8;

/// [`REPLY_SHARE_DIVISOR`] in words, for the two strings a *human* reads (the
/// `--help` line and the home config's own field help): one phrase, in the one
/// file that owns the number, interpolated rather than retyped (finding T2 §19
/// is the same class — a fact spelled wherever it is needed).
pub const REPLY_SHARE_WORDS: &str = "an eighth of the window";

/// The bytes-per-token the budget heuristic uses. The conversion is a *guess*
/// both ways — `Config::history_budget` turns a window's tokens into the bytes
/// `Message::weight` counts, and the UI's context meter turns those bytes back
/// into tokens — so it lives once, here, where the guess is documented. The
/// endpoint's own `usage` is the only place a real count comes from.
pub const BYTES_PER_TOKEN: usize = 3;

/// Keep a window inside the range mush can work with, whatever its source.
fn clamp_context(tokens: usize) -> usize {
    tokens.clamp(MIN_CONTEXT_TOKENS, MAX_CONTEXT_TOKENS)
}

/// The reasoning effort a request asks for: exactly the values the DeepSeek
/// OpenAI format documents, and nothing wider. There is no spelling meaning
/// "send nothing": stating an effort is what puts the field on the request,
/// and the only way a request carries none is an endpoint whose provider row
/// documents no default (the `custom` row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningEffort {
    Low,
    High,
    Max,
}

impl ReasoningEffort {
    /// The value as the endpoint spells it.
    pub fn as_str(&self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "max",
        }
    }

    /// Parse what a human stated. Every accepted string is an effort the
    /// request will carry; a spelling mush does not know is named back rather
    /// than mapped onto one, so a typo can never become a different ask.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(ReasoningEffort::Low),
            "high" => Ok(ReasoningEffort::High),
            "max" => Ok(ReasoningEffort::Max),
            _ => Err(format!(
                "unknown reasoning effort `{value}` (try low, high or max)"
            )),
        }
    }
}

/// What a request says about the provider's thinking mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinkingMode {
    /// Send no `thinking` field at all: the model's own default stands. The
    /// honest "off" — a provider that documents the field documents only how
    /// to enable it, so stating off never invents a shape an endpoint may
    /// reject.
    Off,
    /// Ask for the thinking mode: `{"type":"enabled"}`, the one shape a
    /// provider that documents this field documents.
    On,
}

impl ThinkingMode {
    /// Parse what a human stated (`--thinking on|off`).
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "on" => Ok(ThinkingMode::On),
            "off" => Ok(ThinkingMode::Off),
            _ => Err(format!("unknown thinking mode `{value}` (try on or off)")),
        }
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
    /// The reasoning effort every request asks for. `None` is "not stated":
    /// the provider's own documented default applies, from its row in
    /// [`crate::provider::PROVIDERS`]. A stated effort is always sent, wherever
    /// the human pointed mush, which is why the vocabulary has no "send
    /// nothing" spelling: an endpoint gets no `reasoning_effort` field only
    /// because its row documents none.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// The provider's thinking mode. `None` is "not stated": the provider's own
    /// documented default applies, from its row in
    /// [`crate::provider::PROVIDERS`], and an endpoint whose provider documents
    /// none gets no `thinking` field at all. Stated, the human's choice is sent
    /// wherever they pointed mush.
    pub thinking: Option<ThinkingMode>,
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
    /// Reasoning effort (`--reasoning-effort` or `MUSH_REASONING_EFFORT`).
    /// `None` is "not stated"; every `Some` is an effort a request sends.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Provider thinking mode (`--thinking` or `MUSH_THINKING`). A stated `off`
    /// is a decision, not silence.
    pub thinking: Option<ThinkingMode>,
}

impl Overrides {
    /// The environment layer, read leniently: `MUSH_URL`, `MUSH_MODEL`,
    /// `MUSH_PROVIDER`, `MUSH_API_KEY`, `MUSH_CONTEXT`,
    /// `MUSH_REASONING_EFFORT`, `MUSH_THINKING`. Empty variables count as
    /// unset. A malformed value is dropped here rather than reported, because
    /// this reader is for the callers with no human to answer —
    /// [`Config::from_env`], and the live-endpoint test — while
    /// [`Self::from_env_checked`] is the form startup uses, so a typo costs a
    /// message instead of a window or a knob nobody asked for. The temperature
    /// and the reply cap's name have no environment spelling: they are stated
    /// on a command line or in the home config.
    pub fn from_env() -> Self {
        let text = EnvText::read();
        Self {
            url: text.url,
            model: text.model,
            provider: text.provider,
            api_key: text.api_key,
            context: text
                .context
                .and_then(|value| parse_context_env(&value).ok()),
            // No environment spelling for these two: see the doc comment.
            temperature: None,
            max_completion_tokens: None,
            reasoning_effort: text
                .reasoning_effort
                .and_then(|value| ReasoningEffort::parse(&value).ok()),
            thinking: text
                .thinking
                .and_then(|value| ThinkingMode::parse(&value).ok()),
        }
    }

    /// The environment layer, read for startup: the same seven variables as
    /// [`Self::from_env`] and the same one read of them, but a value that does
    /// not parse is an error naming its variable rather than a silent drop.
    /// The provider is validated first, then the context, the effort and the
    /// thinking mode, so the first typo in that order is the one reported.
    pub fn from_env_checked() -> Result<Self, String> {
        let text = EnvText::read();
        // The provider first: a key meant for somewhere else must not be sent
        // to the default endpoint (finding A17).
        let provider = match text.provider {
            Some(name) => {
                parse_provider_env(&name)?;
                Some(name)
            }
            None => None,
        };
        Ok(Self {
            url: text.url,
            model: text.model,
            provider,
            api_key: text.api_key,
            context: text
                .context
                .map(|value| parse_context_env(&value))
                .transpose()?,
            temperature: None,
            max_completion_tokens: None,
            reasoning_effort: text
                .reasoning_effort
                .map(|value| parse_effort_env(&value))
                .transpose()?,
            thinking: text
                .thinking
                .map(|value| parse_thinking_env(&value))
                .transpose()?,
        })
    }
}

/// The environment as it was read: every `MUSH_*` variable this module knows,
/// each spelled and read exactly once. The two readers above parse these
/// fields with their own policy, so the list of names lives here, in one
/// place, and neither reader can drift into a variable the other does not see.
struct EnvText {
    url: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    api_key: Option<String>,
    context: Option<String>,
    reasoning_effort: Option<String>,
    thinking: Option<String>,
}

impl EnvText {
    fn read() -> Self {
        Self {
            url: env_nonempty("MUSH_URL"),
            model: env_nonempty("MUSH_MODEL"),
            provider: env_nonempty("MUSH_PROVIDER"),
            api_key: env_nonempty("MUSH_API_KEY"),
            context: env_nonempty("MUSH_CONTEXT"),
            reasoning_effort: env_nonempty("MUSH_REASONING_EFFORT"),
            thinking: env_nonempty("MUSH_THINKING"),
        }
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
        format!(
            "MUSH_PROVIDER: unknown provider `{value}` (try {})",
            provider::names_hint()
        )
    })
}

/// Validate `MUSH_REASONING_EFFORT` the way the command line is validated. A
/// typo is reported by name rather than ignored, for the same reason
/// `MUSH_CONTEXT`'s is: a value the human stated must never travel to an
/// endpoint as some other value, nor be silently dropped in favour of a
/// provider default they were trying to overrule.
pub fn parse_effort_env(value: &str) -> Result<ReasoningEffort, String> {
    ReasoningEffort::parse(value).map_err(|error| format!("MUSH_REASONING_EFFORT: {error}"))
}

/// Validate `MUSH_THINKING`, for the same reason: `MUSH_THINKING=of` must say
/// so, not quietly leave the thinking mode on.
pub fn parse_thinking_env(value: &str) -> Result<ThinkingMode, String> {
    ThinkingMode::parse(value).map_err(|error| format!("MUSH_THINKING: {error}"))
}

/// Endpoints are stored without a trailing slash so `chat_url` and
/// `models_url` always join cleanly.
fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Tokens every request reserves for the tool schemas. Six schemas measure
/// ~3.7 KB (~1.25 K tokens at the 3 bytes/token heuristic), so the reserve
/// rounds up; `prompt` tests that they keep fitting.
///
/// The schemas are context paid on *every* request, so this is a real cost.
/// Ownership keeps it down: the prompts carry how to work (the rules, the
/// delegation policy, what the machine is like), and a schema carries only its
/// own call — arguments, defaults, and what comes back. The cut to six tools
/// (the shell reads and writes better than a bespoke tool) took the payload
/// from ~5.1 KB to ~3.5 KB; `edit_file`'s nested `edits` and `control`'s two
/// verbs are most of what is left, and `wait`'s contract grew when it took on
/// the machine lock (the refusal's road back) and named its own cap. The
/// `schemas_fit_the_budget_reserve` test is what makes growth a decision
/// rather than a silent drift.
pub const SCHEMA_TOKENS: usize = 1_300;

impl Config {
    /// Built-in defaults with the `MUSH_*` environment applied.
    ///
    /// Leniently: a malformed value is dropped, because this constructor has
    /// no human to report to. [`resolve`] is the startup path — it reads the
    /// same environment once, through [`Overrides::from_env_checked`], and
    /// reports the typo the two readers would otherwise disagree about.
    pub fn from_env() -> Self {
        Self::from_env_layer(&Overrides::from_env())
    }

    /// Built-in defaults with one environment layer applied — the base
    /// [`resolve`] starts from. Every variable the environment can state is
    /// taken from the layer, so the lenient and the checked read cannot be
    /// applied to different ends; `resolve` hands this the one checked read it
    /// also passes to [`resolve_with`], so no value is read or parsed twice.
    ///
    /// The two request knobs with no `MUSH_*` spelling — the temperature and
    /// the reply cap's name — keep their defaults: the command line and the
    /// home config are their only sources (see [`Overrides`]).
    fn from_env_layer(env: &Overrides) -> Self {
        let provider = env
            .provider
            .as_deref()
            .and_then(Provider::parse)
            .unwrap_or(provider::DEFAULT_PROVIDER);
        let base_url = env
            .url
            .as_deref()
            .map(normalize_url)
            .unwrap_or_else(|| provider.default_base_url().to_string());
        let context = env.context.filter(|n| *n > 0).map(clamp_context);
        Self {
            provider,
            base_url,
            model: env.model.clone().unwrap_or_default(),
            api_key: env.api_key.clone(),
            context_tokens: context.unwrap_or(DEFAULT_CONTEXT_TOKENS),
            context_explicit: context.is_some(),
            temperature: DEFAULT_TEMPERATURE,
            max_completion_tokens: false,
            // A stated effort or thinking mode is sent wherever the human
            // pointed mush; unstated leaves the provider's own documented
            // default to apply.
            reasoning_effort: env.reasoning_effort,
            thinking: env.thinking,
        }
    }

    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            provider: provider::DEFAULT_PROVIDER,
            base_url: normalize_url(&base_url.into()),
            model: model.into(),
            api_key,
            context_tokens: DEFAULT_CONTEXT_TOKENS,
            context_explicit: false,
            temperature: DEFAULT_TEMPERATURE,
            max_completion_tokens: false,
            // Unstated: see the field docs for what the provider's own default
            // then is.
            reasoning_effort: None,
            thinking: None,
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

    /// The one cap on the text a tool result may carry. The command's result is
    /// now the only road by which big text reaches the model — the deleted file
    /// tools' caps are gone with them — so it scales with the window like a read
    /// did: a quarter of [`Self::history_budget`], floored at 512 bytes so a
    /// tiny window still gets an answer, and capped by [`CMD_CAP`] so a huge one
    /// does not hand the model a transcript's worth in a single turn. A result
    /// that hits the cap says so (see `truncate_for_model`), so a model never
    /// mistakes a cut result for a complete one.
    pub fn cmd_cap(&self) -> usize {
        CMD_CAP
            .min(self.history_budget() / 4)
            .max(512)
            .min(self.history_budget())
    }

    /// How many bytes of conversation history fit alongside the tool schemas
    /// and the reply inside `context_tokens`, at the ~3 bytes per token the
    /// heuristic uses. The reserve is itself capped at half the window: a small
    /// window shrinks it (half the window is always history) instead of leaving
    /// no budget at all.
    ///
    /// So `history + schemas + reply + margin == window`, and a request that
    /// spends its whole reply cap still fits the window it is sent to. That is
    /// what keeps a long conversation from being cut off as a context-length
    /// complaint: the trimmer and the cap are two halves of one budget.
    pub fn history_budget(&self) -> usize {
        self.context_tokens
            .saturating_sub(self.request_reserve())
            .saturating_mul(BYTES_PER_TOKEN)
    }

    /// The tokens every request pays besides history: the tool schemas, the
    /// reply [`Config::reply_cap`] may ask for, and a margin. It can never be
    /// more than half the window, so a window too small for its own schemas
    /// still has a budget rather than none (the shape of finding A4).
    ///
    /// The reply is read from `reply_cap` itself rather than from the share it
    /// is derived from: the cap has a floor and a ceiling of its own, and a
    /// reserve that counted the raw share would leave the floor uncovered on a
    /// small window and over-reserve on a huge one.
    ///
    /// The margin is the room a turn *adds* between two requests: a tool
    /// result lands in the next prompt, and a request that spent its whole
    /// reply cap and then a tool result is the request that overflows. It was
    /// 200 tokens when the tools were few and their results were not.
    fn request_reserve(&self) -> usize {
        const MARGIN_TOKENS: usize = 5_000;
        (SCHEMA_TOKENS + self.reply_cap() as usize + MARGIN_TOKENS).min(self.context_tokens / 2)
    }

    /// The tokens one reply may be asked for: [`REPLY_SHARE_WORDS`], floored
    /// at 1_024 so a request always asks for *some* reply, and capped by
    /// [`MAX_REPLY_TOKENS`]. Asking for more than the window can hold is how a
    /// reply arrives cut off, and asking past what the endpoint accepts is a
    /// rejected request, so both ends of this number are stated rather than
    /// discovered.
    pub fn reply_cap(&self) -> u32 {
        let share = (self.context_tokens / REPLY_SHARE_DIVISOR).max(1_024);
        MAX_REPLY_TOKENS.min(share as u32)
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
        self.provider
            .spec()
            .models
            .iter()
            .map(|model| model.id.to_string())
            .collect()
    }

    /// The window to assume for the current model when the endpoint advertises
    /// nothing: the documented one, else the provider's general default.
    pub fn fallback_context(&self) -> usize {
        known_context(&self.model).unwrap_or(self.provider.spec().fallback_context_tokens)
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

    /// Whether a request asks for the provider's thinking mode. A mode the
    /// human stated is honoured wherever they pointed mush — a local endpoint
    /// running a thinking model is exactly why the knob exists; unstated, only
    /// the provider that documents the field gets it.
    pub fn thinking_enabled(&self) -> bool {
        match self.thinking {
            Some(mode) => mode == ThinkingMode::On,
            None => self.provider.spec().thinking_by_default,
        }
    }

    /// Whether the human stated the thinking mode (flag, `MUSH_THINKING`, or
    /// the home config) rather than leaving the provider's default. Only
    /// `--print-config` needs the difference: it is what lets the line say
    /// whose value a request is about to carry.
    pub fn thinking_stated(&self) -> bool {
        self.thinking.is_some()
    }

    /// What a request sends as `reasoning_effort`, `None` for no such field at
    /// all. A stated value reaches any endpoint; unstated, the provider's own
    /// documented default applies, and the `custom` row documents none — which
    /// is now the only reason an endpoint is never given the field.
    pub fn reasoning_effort(&self) -> Option<&'static str> {
        match self.reasoning_effort {
            Some(effort) => Some(effort.as_str()),
            None => self.provider.spec().reasoning_effort_by_default,
        }
    }

    /// Whether the human stated the effort rather than the provider's default
    /// being sent.
    pub fn reasoning_effort_stated(&self) -> bool {
        self.reasoning_effort.is_some()
    }

    /// Point at a different endpoint, normalizing the URL the same way every
    /// other entry point does.
    pub fn set_base_url(&mut self, url: &str) {
        self.base_url = normalize_url(url);
    }

    /// Whether the endpoint a request will carry is the provider's own — the
    /// question anything that wants to name the vendor rather than the URL has
    /// to ask first.
    ///
    /// [`Self::base_url`] is the whole of where a request goes ([`Self::chat_url`]
    /// appends to it), so it is what decides. Not the provider: its knobs
    /// outlive a URL that points elsewhere (`--provider deepseek --url
    /// https://my-proxy`, or a `/url` typed at a running mush). And not
    /// [`crate::provider::ProviderSpec::switches_endpoint`], which says what
    /// *selecting* the provider does once, not where requests are being sent
    /// now. Both sides are compared as they are stored: `base_url` is normalized
    /// by every writer ([`Self::set_base_url`], `resolve`), and a table row's
    /// `default_base_url` carries no trailing slash.
    fn on_the_providers_own_endpoint(&self) -> bool {
        self.provider.spec().default_base_url == self.base_url
    }

    /// A short label for the status bar: the model, then the endpoint — the
    /// provider's own name where the provider owns the endpoint (see
    /// [`crate::provider::ProviderSpec::display_endpoint`]), the configured URL
    /// otherwise.
    ///
    /// "Owns" is [`Self::on_the_providers_own_endpoint`], not "has a
    /// `display_endpoint`": asked of the provider alone, the bar painted the
    /// provider's own short name over a request whose URL was a proxy — a
    /// vendor's name on somebody else's host, while the `/url` ack printed the
    /// proxy (finding A2). A URL that is not the provider's own is named as it
    /// is, so a custom endpoint never borrows a vendor's name either.
    ///
    /// The model's own spelling is defanged as it is labelled, because it is a
    /// name an *endpoint* chose (`/v1/models`) that only ever exists to be
    /// painted: `ESC ]0;PWNED BEL` in a model id renamed the window through the
    /// facts line. The id that is sent back in a request is [`Config::model`],
    /// which this does not touch.
    pub fn label(&self) -> String {
        let model = if self.model.is_empty() {
            "no model".to_string()
        } else {
            crate::text::sanitize(self.model.rsplit('/').next().unwrap_or(&self.model))
        };
        let endpoint = match self.provider.spec().display_endpoint {
            Some(endpoint) if self.on_the_providers_own_endpoint() => endpoint.to_string(),
            _ => self.base_url.clone(),
        };
        format!("{model} @ {endpoint}")
    }
}

/// Startup resolution over four layers, highest priority first:
/// **CLI flags > `MUSH_*` environment > saved session > home config >
/// built-in defaults**. The API key comes from the environment or the home
/// config, never from the session — that file is workspace-local.
///
/// The environment is read here, exactly once, with one policy
/// ([`Overrides::from_env_checked`]): that single read is both the base config
/// [`resolve_with`] starts from and the environment layer it ranks against the
/// flags, so no variable is read twice or parsed under two policies.
///
/// Returns an error for an unknown provider name on the command line, and for a
/// `MUSH_CONTEXT`, `MUSH_REASONING_EFFORT`, `MUSH_THINKING` or `MUSH_PROVIDER`
/// that does not parse.
pub fn resolve(
    cli: &Overrides,
    home: &UserConfig,
    session: Option<&Session>,
) -> Result<Config, String> {
    let env = Overrides::from_env_checked()?;
    resolve_with(Config::from_env_layer(&env), cli, &env, home, session)
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
        config.provider = Provider::parse(provider).ok_or_else(|| {
            format!(
                "unknown provider `{provider}` (try {})",
                provider::names_hint()
            )
        })?;
        // Naming a provider on the command line selects its own endpoint — the
        // flag must reach the host the provider names, not whatever the
        // environment's provider defaulted to — unless a URL was named too.
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
    if let Some(temperature) = cli.temperature {
        config.temperature = temperature;
    }
    if let Some(max_completion_tokens) = cli.max_completion_tokens {
        config.max_completion_tokens = max_completion_tokens;
    }
    if let Some(effort) = cli.reasoning_effort.or(env.reasoning_effort) {
        config.reasoning_effort = Some(effort);
    }
    if let Some(mode) = cli.thinking.or(env.thinking) {
        config.thinking = Some(mode);
    }

    // A URL, provider, or model the user stated explicitly, here or in the
    // environment, is never overridden by a stored one.
    let url_given = cli.url.is_some() || env.url.is_some();
    let provider_given = cli.provider.is_some() || env.provider.is_some();
    let model_given = cli.model.is_some() || env.model.is_some();
    // The temperature and the reply cap's name have no `MUSH_*` spelling, so
    // no environment layer can state them (`Overrides::from_env` leaves both
    // unset): the flag is the only layer above the home config.
    let temperature_given = cli.temperature.is_some();
    let cap_given = cli.max_completion_tokens.is_some();
    let effort_given = cli.reasoning_effort.is_some() || env.reasoning_effort.is_some();
    let thinking_given = cli.thinking.is_some() || env.thinking.is_some();

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
    if !temperature_given {
        if let Some(temperature) = home.temperature {
            config.temperature = temperature;
        }
    }
    if !cap_given {
        if let Some(max_completion_tokens) = home.max_completion_tokens {
            config.max_completion_tokens = max_completion_tokens;
        }
    }
    if !effort_given {
        if let Some(value) = home.reasoning_effort.as_deref() {
            // A value the file got wrong is reported rather than dropped: an
            // effort mush does not know must never reach an endpoint, and a
            // silent fallback would send an effort nobody asked for. This is
            // the same rule the flags and `MUSH_*` follow.
            config.reasoning_effort = Some(
                ReasoningEffort::parse(value).map_err(|error| format!("home config: {error}"))?,
            );
        }
    }
    if !thinking_given {
        if let Some(on) = home.thinking {
            config.thinking = Some(if on {
                ThinkingMode::On
            } else {
                ThinkingMode::Off
            });
        }
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

    // The home config's window. A window there is a statement too, so it beats
    // what an endpoint advertises; but a window this workspace remembers is the
    // more specific statement, which is why this waits for the session above.
    // The layers still read CLI > env > session > home.
    if !config.context_explicit {
        if let Some(tokens) = home.context.filter(|n| *n > 0) {
            config.set_context(tokens);
        }
    }

    // 4. An endpoint named on the command line or in the environment is a
    //    *custom* endpoint: a stored provider must not leak its hosted-provider
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
            ..UserConfig::default()
        }
    }

    fn stored(provider: &str, base_url: &str, model: &str) -> Session {
        Session {
            model: model.into(),
            provider: provider.into(),
            base_url: base_url.into(),
            context: None,
            messages: Vec::new(),
            agents: Vec::new(),
            notices: Vec::new(),
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

    /// The home config fills what every layer above it leaves unstated: the
    /// request knobs nothing else can state, and a window that a human means
    /// (so it beats discovery — but not a window this workspace remembers).
    #[test]
    fn the_home_config_fills_what_the_layers_above_leave_unstated() {
        let home = UserConfig {
            context: Some(32_000),
            temperature: Some(0.2),
            max_completion_tokens: Some(true),
            ..UserConfig::default()
        };
        let mut config = resolve_with(
            Config::new("http://base:0", "m", None),
            &Overrides::default(),
            &Overrides::default(),
            &home,
            None,
        )
        .unwrap();
        assert_eq!(config.temperature(), 0.2);
        assert!(config.uses_max_completion_tokens());
        assert_eq!(config.context_tokens, 32_000);
        assert!(config.context_explicit, "a stated window is not a guess");
        assert!(
            !config.adopt_context(4_096),
            "so an endpoint cannot overrule it"
        );

        // A flag above the file wins, and an unstated field is the built-in
        // default rather than an empty file's.
        let config = resolve_with(
            Config::new("http://base:0", "m", None),
            &Overrides {
                context: Some(8_000),
                temperature: Some(0.9),
                max_completion_tokens: Some(false),
                ..Overrides::default()
            },
            &Overrides::default(),
            &home,
            None,
        )
        .unwrap();
        assert_eq!(config.temperature(), 0.9);
        assert!(!config.uses_max_completion_tokens());
        assert_eq!(config.context_tokens, 8_000);

        let empty = resolve_with(
            Config::new("http://base:0", "m", None),
            &Overrides::default(),
            &Overrides::default(),
            &UserConfig::default(),
            None,
        )
        .unwrap();
        assert_eq!(empty.temperature(), DEFAULT_TEMPERATURE);
        assert!(!empty.uses_max_completion_tokens());

        // A window this workspace remembers is more specific than the
        // machine-global one, so the session still wins.
        let mut session = stored("custom", "http://session:3", "session-model");
        session.context = Some(16_000);
        let config = resolve_with(
            Config::new("http://base:0", "", None),
            &Overrides::default(),
            &Overrides::default(),
            &home,
            Some(&session),
        )
        .unwrap();
        assert_eq!(config.context_tokens, 16_000);
        assert_eq!(config.temperature(), 0.2, "the other knobs still come home");
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

    /// The base `resolve` starts from is the environment layer applied to
    /// built-in defaults — the whole layer, so nothing has to be re-read to
    /// complete it — except the two knobs the environment cannot state
    /// (`temperature`, the reply cap's name), which keep their defaults
    /// however a hand-built layer carries them.
    #[test]
    fn the_environment_base_is_one_layer_over_the_defaults() {
        let env = Overrides {
            url: Some("http://env:2/".into()),
            model: Some("env-model".into()),
            provider: Some("deepseek".into()),
            api_key: Some("sk-env".into()),
            context: Some(9_000),
            temperature: Some(0.5),
            max_completion_tokens: Some(true),
            reasoning_effort: Some(ReasoningEffort::Max),
            thinking: Some(ThinkingMode::On),
        };
        let base = Config::from_env_layer(&env);
        assert_eq!(base.base_url, "http://env:2");
        assert_eq!(base.model, "env-model");
        assert_eq!(base.provider, Provider::DeepSeek);
        assert_eq!(base.api_key.as_deref(), Some("sk-env"));
        assert_eq!(base.context_tokens, 9_000);
        assert!(base.context_explicit);
        assert_eq!(base.reasoning_effort, Some(ReasoningEffort::Max));
        assert_eq!(base.thinking, Some(ThinkingMode::On));
        assert_eq!(base.temperature(), DEFAULT_TEMPERATURE);
        assert!(!base.uses_max_completion_tokens());
    }

    /// The temperature and the reply cap's name are stated, not guessed: the
    /// command line states them, the home config fills what it left alone, and
    /// the environment cannot state them at all — no `MUSH_*` spelling exists,
    /// so `Overrides::from_env` leaves both unset.
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

        // An environment layer that carries one of the two is not read: there
        // is no `MUSH_*` that could have built it, so its presence is not a
        // statement the layering can honour, and the silence above the home
        // config stays silence.
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
        assert_eq!(from_env.temperature(), DEFAULT_TEMPERATURE);
        assert!(!from_env.uses_max_completion_tokens());

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

    /// Nothing stated means today's request, bit for bit: DeepSeek asks for its
    /// thinking mode and `high`, and every other endpoint gets neither field.
    /// The knobs are only the human's when the human states one.
    #[test]
    fn nothing_stated_keeps_the_preset_knobs() {
        let deepseek = resolve_with(
            Config::new("http://base:0", "m", None),
            &Overrides {
                provider: Some("deepseek".into()),
                ..Overrides::default()
            },
            &Overrides::default(),
            &UserConfig::default(),
            None,
        )
        .unwrap();
        assert_eq!(deepseek.provider, Provider::DeepSeek);
        assert!(deepseek.thinking_enabled());
        assert_eq!(deepseek.reasoning_effort(), Some("high"));
        assert!(!deepseek.thinking_stated());
        assert!(!deepseek.reasoning_effort_stated());

        let custom = resolve_with(
            Config::new("http://base:0", "m", None),
            &Overrides::default(),
            &Overrides::default(),
            &UserConfig::default(),
            None,
        )
        .unwrap();
        assert_eq!(custom.provider, Provider::Custom);
        assert!(!custom.thinking_enabled());
        assert_eq!(custom.reasoning_effort(), None);
        assert!(!custom.thinking_stated());
        assert!(!custom.reasoning_effort_stated());
    }

    /// The thinking knobs rank like every other setting: the flag over the
    /// environment over the home config, each layer filling only what the one
    /// above it left unstated.
    #[test]
    fn the_effort_and_thinking_layers_rank_like_every_other() {
        let base = || Config::new("http://base:0", "m", None);
        let home = UserConfig {
            reasoning_effort: Some("low".into()),
            thinking: Some(true),
            ..UserConfig::default()
        };
        let env = Overrides {
            reasoning_effort: Some(ReasoningEffort::Max),
            thinking: Some(ThinkingMode::Off),
            ..Overrides::default()
        };
        let cli = Overrides {
            reasoning_effort: Some(ReasoningEffort::High),
            thinking: Some(ThinkingMode::On),
            ..Overrides::default()
        };

        // Command line: beats both.
        let config = resolve_with(base(), &cli, &env, &home, None).unwrap();
        assert_eq!(config.reasoning_effort(), Some("high"));
        assert!(config.thinking_enabled(), "the flag said on, the env off");

        // Environment: fills what the flag left alone.
        let config = resolve_with(base(), &Overrides::default(), &env, &home, None).unwrap();
        assert_eq!(config.reasoning_effort(), Some("max"));
        assert!(!config.thinking_enabled(), "the env said off, the file on");
        assert!(config.reasoning_effort_stated());

        // Home config: the last statement before the built-in default.
        let config = resolve_with(
            base(),
            &Overrides::default(),
            &Overrides::default(),
            &home,
            None,
        )
        .unwrap();
        assert_eq!(config.reasoning_effort(), Some("low"));
        assert!(config.thinking_enabled());
        assert!(config.thinking_stated());

        // The home config's `false` is a statement too: on DeepSeek it
        // overrules the preset, which is exactly why it is worth stating — and
        // a stated effort replaces the preset the same way, since a stated
        // value is what a request carries.
        let config = resolve_with(
            base(),
            &Overrides {
                provider: Some("deepseek".into()),
                ..Overrides::default()
            },
            &Overrides::default(),
            &UserConfig {
                reasoning_effort: Some("max".into()),
                thinking: Some(false),
                ..UserConfig::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(config.provider, Provider::DeepSeek);
        assert_eq!(
            config.reasoning_effort(),
            Some("max"),
            "the stated effort, not the preset"
        );
        assert!(!config.thinking_enabled(), "stated off sends no field");
        assert!(config.reasoning_effort_stated());
        assert!(config.thinking_stated());
    }

    /// A value the human states is honoured wherever they point mush — a local
    /// endpoint running a thinking model is the reason the knob exists — while
    /// the *provider's* default still never follows a URL it does not own (the
    /// test below).
    #[test]
    fn a_stated_knob_is_honoured_on_a_custom_endpoint() {
        let config = resolve_with(
            Config::new("http://localhost:11434", "qwen", None),
            &Overrides {
                url: Some("http://localhost:11434".into()),
                ..Overrides::default()
            },
            &Overrides::default(),
            &UserConfig {
                provider: "deepseek".into(),
                reasoning_effort: Some("low".into()),
                thinking: Some(true),
                ..UserConfig::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(config.provider, Provider::Custom, "the URL is the human's");
        assert_eq!(config.reasoning_effort(), Some("low"));
        assert!(config.thinking_enabled());

        // And the reverse: a stated `off` suppresses the DeepSeek thinking
        // preset rather than being mistaken for silence, while the effort left
        // unstated still follows that provider's row.
        let config = resolve_with(
            Config::new("http://base:0", "m", None),
            &Overrides {
                provider: Some("deepseek".into()),
                thinking: Some(ThinkingMode::Off),
                ..Overrides::default()
            },
            &Overrides::default(),
            &UserConfig::default(),
            None,
        )
        .unwrap();
        assert_eq!(config.provider, Provider::DeepSeek);
        assert_eq!(config.reasoning_effort(), Some("high"));
        assert!(!config.thinking_enabled());
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
            reasoning_effort: None,
            thinking: None,
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

    /// A provider's display endpoint is a shortening of the URL it owns, so it
    /// may stand for the request only while the request goes there: asked of the
    /// provider alone, the bar read `deepseek-flash @ deepseek.com` over a
    /// request whose URL was a proxy, while the `/url` ack printed the proxy.
    /// Both reachable paths leave exactly that state — `--provider deepseek
    /// --url https://my-proxy`, which only a missing `--provider` would have
    /// turned into `Custom`, and a `/provider deepseek` followed by a `/url` at
    /// a running mush (finding A2).
    #[test]
    fn a_label_names_the_providers_endpoint_only_where_a_request_goes() {
        let mut cfg = Config {
            provider: Provider::DeepSeek,
            ..Config::new(
                Provider::DeepSeek.default_base_url(),
                "deepseek-flash",
                None,
            )
        };
        assert_eq!(
            cfg.label(),
            "deepseek-flash @ deepseek.com",
            "the provider's own endpoint, as its row spells it"
        );

        cfg.set_base_url("https://my-proxy");
        assert_eq!(
            cfg.label(),
            "deepseek-flash @ https://my-proxy",
            "the endpoint in use, not the one the provider would have used"
        );
    }

    /// And the other direction: a URL the provider does not own is named as a
    /// URL, even when it is on the provider's own host — the label claims
    /// nothing about who answers that the request does not carry, so a custom
    /// endpoint can never start wearing a vendor's name (finding A2).
    #[test]
    fn a_label_spells_out_an_endpoint_the_provider_does_not_own() {
        // The human typed the vendor's host but kept the `custom` row: the
        // endpoint is theirs, and it is a fact worth seeing.
        let cfg = Config::new(
            Provider::DeepSeek.default_base_url(),
            "deepseek-flash",
            None,
        );
        assert_eq!(cfg.provider, Provider::Custom);
        assert_eq!(cfg.label(), "deepseek-flash @ https://api.deepseek.com");

        // A path under the provider's host is not the provider's endpoint: a
        // request goes to `<base_url>/v1/chat/completions`, so a base URL with
        // a path of its own is a different endpoint, and is shown as itself.
        let cfg = Config {
            provider: Provider::DeepSeek,
            ..Config::new("https://api.deepseek.com/v1", "deepseek-flash", None)
        };
        assert_eq!(cfg.label(), "deepseek-flash @ https://api.deepseek.com/v1");
    }

    /// The model half of the label is a *name an endpoint chose*, painted in
    /// the facts line and in the picker: an `ESC ]0;PWNED BEL` in it renamed the
    /// window and a CSI wiped the frame. The label is defanged; the id a request
    /// carries is not touched.
    #[test]
    fn a_label_defangs_the_model_an_endpoint_named() {
        let hostile = "boom\rREST \x1b]0;PWNED\x07\x1b[2J\x1b[HMock";
        let cfg = Config::new("http://x:1", hostile, None);
        let label = cfg.label();
        assert!(!label.contains('\x1b'), "{label:?}");
        assert!(!label.contains('\r'), "{label:?}");
        assert!(
            label.starts_with("boom␍REST Mock @ http://x:1"),
            "{label:?}"
        );
        assert_eq!(cfg.model, hostile, "the id a request sends is untouched");
    }

    #[test]
    fn history_budget_fits_the_context_window() {
        // 8192 tokens: the reserve is capped at half the window (the margin
        // below is 5_000, which alone is more than half of an 8k window), so
        // history gets the other half — 12_288 bytes. The schema line of the
        // reserve has moved with every contract change, test and comment
        // together: 1100 (delegation), 1220 (`cd`), 1700 (the machine's three
        // tools), 1750 (what the waits hand over, H15), 1700 when the dedup
        // pass fit the same rules in fewer bytes, 1200 after the cut to six
        // tools took the shell's work off the schema list, and 1300 when `wait`
        // took on the machine lock and named its cap. See SCHEMA_TOKENS.
        let small = Config::new("http://x:1", "m", None);
        assert_eq!(small.context_tokens, DEFAULT_CONTEXT_TOKENS);
        assert_eq!(small.history_budget(), 12_288);

        // A big window leaves a much larger budget, and the reply's share of it
        // grows with the window: 128k reserves 16k for one reply.
        let big = Config {
            context_tokens: 128_000,
            temperature: DEFAULT_TEMPERATURE,
            max_completion_tokens: false,
            ..small.clone()
        };
        assert_eq!(big.history_budget(), 317_100);

        // A tiny window shrinks the reserve to half the window instead of
        // ignoring it: history still gets 1536 bytes, and the cap — which has
        // a floor of its own — is not larger than the budget that holds it.
        let tiny = Config {
            context_tokens: 1024,
            ..small
        };
        assert_eq!(tiny.history_budget(), 1_536);
        assert!(tiny.cmd_cap() <= tiny.history_budget());
    }

    /// The reserve keeps the request inside the window: history, schemas, the
    /// reply's share and the margin add up to exactly `context_tokens` at every
    /// size, so a run that spends its whole reply cap is not the run that
    /// overflows the window. It can never swallow the window either, whatever
    /// the schemas grow to (finding A4's shape).
    #[test]
    fn the_reserve_keeps_a_full_reply_inside_the_window() {
        for window in [8_192, 120_000, 128_000, 500_000, MAX_CONTEXT_TOKENS] {
            let mut cfg = Config::new("http://x:1", "m", None);
            cfg.set_context(window);
            assert_eq!(
                cfg.history_budget() / 3 + cfg.request_reserve(),
                cfg.context_tokens,
                "history + reserve is not the window at {window}"
            );
            assert!(
                cfg.request_reserve() <= cfg.context_tokens / 2,
                "the reserve swallowed the window at {window}"
            );
        }

        // The floor a window can be clamped to is still bigger than the reserve
        // asked of it: the ledger stays readable at the smallest size mush
        // stores.
        let mut tiny = Config::new("http://x:1", "m", None);
        tiny.set_context(1);
        assert_eq!(tiny.context_tokens, MIN_CONTEXT_TOKENS);
        assert!(tiny.request_reserve() < tiny.context_tokens);
    }

    /// One reply may never be asked for more than a share of the window: an
    /// endpoint cannot deliver what it does not have, and a thinking model
    /// spends the cap before it reaches the answer. The ceiling is the number
    /// the human stated, and it stays under the range the endpoint itself
    /// documents it accepts — a cap past that is a rejected request, which is
    /// worse than a short reply.
    #[test]
    fn a_reply_never_asks_for_more_than_the_window_has() {
        let window = |tokens: usize| {
            let mut cfg = Config::new("http://127.0.0.1:1", "m", None);
            cfg.set_context(tokens);
            cfg
        };
        assert_eq!(window(8_192).reply_cap(), 1_024, "the floor binds at 8k");
        assert_eq!(
            window(120_000).reply_cap(),
            (120_000 / REPLY_SHARE_DIVISOR) as u32,
            "in the middle of the range the cap *is* the share"
        );
        assert_eq!(
            window(2_000_000).reply_cap(),
            MAX_REPLY_TOKENS,
            "a window big enough to ask for more than the endpoint accepts keeps the ceiling"
        );
        assert_eq!(MAX_REPLY_TOKENS, 120_000, "the human's number");
        // Never zero, however tiny the window: a request for no reply is not a
        // request.
        assert_eq!(window(512).reply_cap(), 1_024);
    }

    /// The window precedence the new default must not disturb: a window a human
    /// stated beats the built-in one, a window learned from the endpoint beats
    /// both, and a window nobody stated is never remembered as if somebody had.
    /// `context_explicit` is what the session stores by, so it is what this
    /// pins.
    #[test]
    fn the_window_precedence_outlives_the_new_default() {
        let cli = Overrides {
            provider: Some("deepseek".into()),
            ..Overrides::default()
        };
        let resolve = |cli: &Overrides, home: &UserConfig| {
            resolve_with(
                Config::new("http://base:0", "", None),
                cli,
                &Overrides::default(),
                home,
                None,
            )
            .unwrap()
        };

        // Nothing stated: the provider's own default, and a guess, not a
        // statement — which is what keeps it out of the session and the file.
        let mut config = resolve(&cli, &UserConfig::default());
        assert_eq!(config.context_tokens, 120_000, "the shipped default");
        assert!(!config.context_explicit);

        // The endpoint's own number overrides the default.
        assert!(config.adopt_context(500_000), "learned, and adopted");
        assert_eq!(config.context_tokens, 500_000);
        assert!(!config.context_explicit, "still a guess, still not stored");

        // ...and a window the human states overrides that, for good.
        config.set_context(64_000);
        assert!(config.context_explicit);
        assert!(!config.adopt_context(500_000), "the human's number stays");
        assert_eq!(config.context_tokens, 64_000);

        // The home config's window is a statement too — the layer under the
        // session, and over the built-in default.
        let config = resolve(
            &cli,
            &UserConfig {
                context: Some(200_000),
                ..UserConfig::default()
            },
        );
        assert_eq!(config.context_tokens, 200_000);
        assert!(config.context_explicit);
    }

    /// The window comes from the model when nobody said otherwise, and the
    /// command result cap follows it: the one road a big text result travels
    /// must never be larger than the transcript that has to hold it.
    #[test]
    fn the_window_and_the_caps_scale_together() {
        let mut cfg = Config::new("http://x:1", "deepseek-v4-pro", None);
        assert!(!cfg.context_explicit);
        // `Config::new` does not resolve; the fallback is what the resolver uses.
        assert_eq!(cfg.fallback_context(), 500_000);
        cfg.context_tokens = cfg.fallback_context();
        assert_eq!(cfg.cmd_cap(), CMD_CAP, "a huge window keeps the ceiling");

        // An 8k local window: a single command result may take a quarter of the
        // budget, well under the ceiling.
        let small = Config::new("http://x:1", "m", None);
        assert_eq!(small.cmd_cap(), small.history_budget() / 4);
        assert!(
            small.cmd_cap() < CMD_CAP,
            "an 8k transcript cannot hold the ceiling: {}",
            small.cmd_cap()
        );

        // Under a window smaller than the cap's own floor, the budget wins:
        // a 1k window is never handed a 512-byte result it cannot hold.
        let tiny = Config {
            context_tokens: 1_024,
            ..Config::new("http://x:1", "m", None)
        };
        assert_eq!(tiny.history_budget(), 1_536);
        assert_eq!(tiny.cmd_cap(), 512);
        assert!(tiny.cmd_cap() <= tiny.history_budget());

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
        // The ledger still adds up at the ceiling: the reserve is what the
        // window does not get to spend on history.
        assert_eq!(
            cfg.history_budget() / 3 + cfg.request_reserve(),
            MAX_CONTEXT_TOKENS
        );
        assert!(cfg.history_budget() > 20_000_000);

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
        assert_eq!(cfg.context_tokens, 120_000, "the new provider's default");
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

    /// The thinking knobs' environment spellings report a typo by name, the way
    /// `MUSH_CONTEXT` does: a value mush cannot use must never travel to an
    /// endpoint, and dropping it would send the provider's default instead of
    /// the setting the human stated.
    #[test]
    fn a_bad_effort_or_thinking_environment_value_is_reported() {
        assert_eq!(parse_effort_env("high"), Ok(ReasoningEffort::High));
        assert_eq!(parse_effort_env(" MAX "), Ok(ReasoningEffort::Max));
        let error = parse_effort_env("very").unwrap_err();
        assert!(
            error.contains("MUSH_REASONING_EFFORT") && error.contains("very"),
            "{error}"
        );

        assert_eq!(parse_thinking_env("on"), Ok(ThinkingMode::On));
        assert_eq!(parse_thinking_env("OFF"), Ok(ThinkingMode::Off));
        let error = parse_thinking_env("of").unwrap_err();
        assert!(
            error.contains("MUSH_THINKING") && error.contains("of"),
            "{error}"
        );
    }

    /// The vocabulary is exactly the three values DeepSeek documents, so every
    /// spelling from a wider one — including the deleted `none`/`off` and the
    /// `medium`, `minimal` and `xhigh` other vendors use — is refused by name
    /// rather than folded onto an effort the human did not ask for.
    #[test]
    fn an_effort_outside_the_vocabulary_is_refused_by_name() {
        for value in ["none", "off", "medium", "minimal", "xhigh"] {
            let error = ReasoningEffort::parse(value).unwrap_err();
            assert!(error.contains(value), "{error}");
            assert!(error.contains("low, high or max"), "{error}");
        }
    }

    /// The same rule one layer down: a hand-edited effort the human got wrong
    /// is named by startup instead of being sent, and instead of being ignored
    /// in favour of a default they were trying to overrule.
    #[test]
    fn a_bad_effort_in_the_home_config_is_reported() {
        let error = resolve_with(
            Config::new("http://x:1", "m", None),
            &Overrides::default(),
            &Overrides::default(),
            &UserConfig {
                reasoning_effort: Some("very".into()),
                ..UserConfig::default()
            },
            None,
        )
        .unwrap_err();
        assert!(error.contains("very"), "{error}");
        assert!(error.contains("home config"), "{error}");
    }
}
